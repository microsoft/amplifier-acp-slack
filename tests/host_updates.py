"""Host update admission/recovery controls. No Docker, systemd or network effects."""
import importlib.util
import json
import os
import tempfile
import sqlite3
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('host_update', ROOT/'scripts/host_update.py')
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)
u = h.u


class Binding:
    def __init__(self, home, manifest, expected=None):
        self.home, self.unit, self.expected = home, 'fictional.service', {'fixed': True}
        if expected is not None and expected != self.expected:
            raise RuntimeError('host_service_changed')

    def require_policy(self, job):
        p = u.read(self.home/'updates/policy.json')
        if p['enabled'] or p['agent_enabled'] or job['policy_revision'] != p['revision'] or job['host_policy_sha256'] != u.digest(self.home/'updates/policy.json'):
            raise RuntimeError('host_update_authority_changed')
        return p

    def verify_running(self, expected, endpoints, job):
        return {'manager': {'pid': 30, 'start_ticks': 1}, 'application': {'pid': 40, 'start_ticks': 2}}


class HostUpdates(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)/'home'
        self.root = self.home/'updates'
        self.root.mkdir(parents=True)
        (self.root/'admission.lock').touch(mode=0o600)
        (self.home/'bin').mkdir()
        with sqlite3.connect(self.home/'state.sqlite3') as db:
            db.execute('CREATE TABLE jobs (status TEXT)')
            db.execute('CREATE TABLE outbox (status TEXT)')
        (self.home/'bin/acp-slack-adapter').write_bytes(b'original')
        u.write(self.home/'endpoints.json', {'pid': 14})
        self.policy = {'schema': 1, 'adapter_id': str(uuid.uuid4()), 'revision': 2,
                       'enabled': False, 'agent_enabled': False, 'owner_slack_user_id': 'Ufixture'}
        u.write(self.root/'policy.json', self.policy)
        self.manifest = Path(self.temp.name)/'manifest.json'
        self.manifest.write_text('{}')
        self.identifier = str(uuid.uuid4())
        self.binding = patch.object(h, 'ContainerService', Binding)
        self.binding.start()

    def tearDown(self):
        self.binding.stop()
        self.temp.cleanup()

    def queue(self, action, **kwargs):
        return h.queue_action(self.home, self.manifest, action, self.identifier, **kwargs)

    def save(self, value):
        u.write(self.root/'plans'/self.identifier/'plan.json', value)

    def test_plan_reuses_exact_journal_and_disabled_policy(self):
        before = (self.root/'policy.json').read_bytes()
        with patch.object(h, 'dispatch') as dispatch:
            first = self.queue('plan')
            same = self.queue('plan')
        self.assertEqual(first, same)
        self.assertEqual(dispatch.call_count, 1)
        self.assertEqual(u.read(self.root/'active.json'), {'id': self.identifier})
        self.assertEqual((self.root/'policy.json').read_bytes(), before)
        self.assertEqual(first['actor_kind'], 'host_operator')
        self.assertEqual(first['host_dispatch']['outcome'], 'unknown')

    def test_lost_dispatch_reply_is_not_blindly_reissued(self):
        with patch.object(h, 'dispatch', side_effect=TimeoutError) as dispatch:
            first = self.queue('plan')
            self.assertEqual(self.queue('plan'), first)
        self.assertEqual(dispatch.call_count, 1)
        self.assertEqual(first['state'], 'queued')
        with patch.object(h, 'manager', return_value={'ActiveState': 'active', 'MainPID': '12', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}), patch.object(h, 'dispatch') as dispatch:
            self.queue('recover')
            dispatch.assert_not_called()

    def test_explicit_recovery_records_intent_before_dispatch(self):
        self.queue('plan', dispatch=False)
        def dispatch(*args):
            self.assertEqual(self.queue('status')['host_recovery_attempts'][0]['outcome'], 'unknown')
            raise TimeoutError()
        with patch.object(h, 'manager', return_value={'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}), patch.object(h, 'dispatch', side_effect=dispatch) as call:
            value = self.queue('recover')
        self.assertEqual(call.call_count, 1)
        self.assertEqual(len(value['host_recovery_attempts']), 1)
        self.assertEqual(value['state'], 'queued')

    def test_revoked_or_changed_policy_refuses_apply(self):
        job = self.queue('plan', dispatch=False)
        job['state'] = 'ready'
        self.save(job)
        for field, value in [('revision', 3), ('enabled', True), ('agent_enabled', True), ('owner_slack_user_id', 'Uother')]:
            u.write(self.root/'policy.json', {**self.policy, field: value})
            with self.assertRaisesRegex(RuntimeError, 'authority_changed'):
                self.queue('apply', dispatch=False)
            self.assertEqual(self.queue('status')['state'], 'ready')
        u.write(self.root/'policy.json', self.policy)

    def test_second_plan_cannot_supersede_unsettled_or_recovery_required(self):
        original = self.queue('plan', dispatch=False)
        for state in ['queued', 'verifying', 'recovery_required']:
            self.save({**original, 'state': state})
            with self.assertRaisesRegex(RuntimeError, 'another_update_unsettled'):
                h.queue_action(self.home, self.manifest, 'plan', str(uuid.uuid4()), dispatch=False)

    def test_source_and_manifest_drift_refused_even_on_retry(self):
        job = self.queue('plan', dispatch=False)
        job['host_source']['update.py'] = 'changed'
        self.save(job)
        with self.assertRaisesRegex(RuntimeError, 'source_changed'):
            self.queue('plan')
        job['host_source'] = h.code_hashes()
        job['host_manifest'] = '/different/manifest.json'
        self.save(job)
        with self.assertRaisesRegex(RuntimeError, 'manifest_changed'):
            self.queue('plan')

    def test_apply_terminal_retry_does_not_apply_again(self):
        job = self.queue('plan', dispatch=False)
        job['state'] = 'ready'
        self.save(job)
        with patch.object(h, 'dispatch') as dispatch:
            applied = self.queue('apply')
            applied['state'] = 'succeeded'
            self.save(applied)
            result = self.queue('apply')
        self.assertEqual(result['state'], 'succeeded')
        self.assertEqual(dispatch.call_count, 1)

    def test_deferred_plan_can_continue_explicitly_after_conditions_settle(self):
        job = self.queue('plan', dispatch=False)
        job.update(state='deferred', action='apply', host_accepted_actions=['plan', 'apply'],
                   reason='uncertain_delivery_requires_reconciliation')
        self.save(job)
        idle = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}
        with patch.object(h, 'manager', return_value=idle), patch.object(h, 'dispatch') as dispatch:
            result = self.queue('apply')
        self.assertEqual(result['state'], 'queued')
        self.assertEqual(dispatch.call_count, 1)
        self.assertEqual(result['host_accepted_actions'], ['plan', 'apply', 'apply'])

    def test_uncertain_delivery_still_defers_explicit_continuation(self):
        job = self.queue('plan', dispatch=False)
        job.update(state='deferred', action='apply', host_accepted_actions=['plan', 'apply'])
        self.save(job)
        with sqlite3.connect(self.home/'state.sqlite3') as db:
            db.execute("INSERT INTO outbox VALUES ('uncertain')")
        idle = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}
        with patch.object(h, 'manager', return_value=idle), patch.object(h, 'dispatch') as dispatch:
            result = self.queue('apply')
        self.assertEqual(result['state'], 'deferred')
        dispatch.assert_not_called()

    def test_pending_worker_manager_job_refuses_recovery_dispatch(self):
        self.queue('plan', dispatch=False)
        pending = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '8 /job/8', 'NeedDaemonReload': 'no'}
        with patch.object(h, 'manager', return_value=pending), patch.object(h, 'dispatch') as dispatch:
            with self.assertRaisesRegex(RuntimeError, 'manager_job_pending'):
                self.queue('recover')
        dispatch.assert_not_called()

    def test_container_drift_during_worker_preserves_recovery_required(self):
        job = self.queue('plan', dispatch=False)
        for phase, expected in [('queued', 'failed'), ('swapping', 'recovery_required')]:
            self.save({**job, 'state': phase})
            with patch.object(h, 'ContainerService', side_effect=RuntimeError('host_service_changed')):
                result = h.run_worker(self.home, self.manifest, self.identifier)
            self.assertEqual(result['state'], expected)
            self.assertEqual(result['reason'], 'host_service_changed')


class ContainerFingerprint(unittest.TestCase):
    def test_nested_overrides_cannot_hide_host_journal_or_binary(self):
        home = Path('/fixture')
        mounts = [{'Source': str(p), 'Destination': str(p), 'RW': rw}
                  for p, rw in [(home, True), (home/'bin', False), (home/'updates', False)]]
        h.validate_update_mounts(home, mounts)
        for destination in ['/fixture/updates/plans', '/fixture/updates/admission.lock', '/fixture/bin/acp-slack-adapter']:
            for writable in [True, False]:
                with self.assertRaisesRegex(RuntimeError, 'nested_update_mount'):
                    h.validate_update_mounts(home, mounts + [{'Source': '/different', 'Destination': destination, 'RW': writable}])

    def test_writable_source_aliases_cannot_bypass_readonly_overlays(self):
        home = Path('/fixture')
        mounts = [{'Source': str(p), 'Destination': str(p), 'RW': rw}
                  for p, rw in [(home, True), (home/'bin', False), (home/'updates', False)]]
        for source in ['/fixture', '/fixture/updates', '/fixture/updates/admission.lock', '/fixture/bin', '/']:
            with self.assertRaisesRegex(RuntimeError, 'writable_update_alias'):
                h.validate_update_mounts(home, mounts + [{'Source': source, 'Destination': '/alias', 'RW': True}])
        h.validate_update_mounts(home, mounts + [{'Source': '/fixture/workspace', 'Destination': '/workspace', 'RW': True}])

    def test_only_mount_collection_order_is_ignored(self):
        one = {'Source': '/a', 'Destination': '/a', 'RW': False, 'Propagation': 'rprivate'}
        two = {'Source': '/b', 'Destination': '/b', 'RW': True, 'Propagation': 'rprivate'}
        original = {'Config': {'Cmd': ['a', 'b']}, 'HostConfig': {}, 'Mounts': [one, two]}
        expected = h.container_configuration(original)
        self.assertEqual(h.container_configuration({**original, 'Mounts': [two, one]}), expected)
        for field, value in [('RW', True), ('Source', '/other'), ('Propagation', 'rshared')]:
            self.assertNotEqual(h.container_configuration({**original, 'Mounts': [{**one, field: value}, two]}), expected)
        self.assertNotEqual(h.container_configuration({**original, 'Config': {'Cmd': ['b', 'a']}}), expected)
        with self.assertRaisesRegex(RuntimeError, 'ambiguous_container_mounts'):
            h.container_configuration({**original, 'Mounts': [one, one]})


class ProcessBoundary(unittest.TestCase):
    def binding(self):
        b = object.__new__(h.ContainerService)
        b.unit = 'fictional.service'
        b.verify = lambda: None
        b.applications = lambda: []
        return b

    def test_namespace_pid_is_bound_to_actual_executable(self):
        b = self.binding()
        app = {'pid': 88, 'start_ticks': 3, 'nspid': [88, 14]}
        b.running = lambda: {'application': app, 'manager': {'pid': 9}}
        with patch.object(u, 'digest', return_value='expected'):
            b.verify_running('expected', {'pid': 14}, {})
            with self.assertRaisesRegex(RuntimeError, 'namespace_mismatch'):
                b.verify_running('expected', {'pid': 88}, {})
            with self.assertRaisesRegex(RuntimeError, 'namespace_mismatch'):
                b.verify_running('different', {'pid': 14}, {})

    def test_live_generation_and_pid_reuse_are_distinguished(self):
        b = self.binding()
        known = {'application': {'pid': 88, 'start_ticks': 3}}
        with patch.object(h, 'process', return_value={'pid': 88, 'start_ticks': 3}):
            self.assertFalse(b.gone(known))
        with patch.object(h, 'process', return_value={'pid': 88, 'start_ticks': 4}):
            self.assertTrue(b.gone(known))

    def test_completed_stop_observed_without_second_command(self):
        b = self.binding()
        class Controller:
            job = {'host_live_processes': {'application': {'pid': 88, 'start_ticks': 3}}}
        with patch.object(h, 'manager', return_value={'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}), patch.object(h, 'process', return_value=None), patch.object(u, 'run') as run:
            b.control('stop', Controller())
            run.assert_not_called()

    def test_explicit_empty_manager_job_is_distinct_from_missing_data(self):
        idle = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'NeedDaemonReload': 'no'}
        self.assertTrue(h.manager_idle({**idle, 'Job': ''}))
        with self.assertRaisesRegex(RuntimeError, 'manager_job_pending'):
            h.manager_idle(idle)
        with self.assertRaisesRegex(RuntimeError, 'manager_job_pending'):
            h.manager_idle({**idle, 'Job': '42 /job/42'})

    def test_pending_manager_job_prevents_start(self):
        b = self.binding()
        b.running = lambda: {'manager': {'pid': 20}, 'application': {'pid': 30}}
        class Controller:
            job = {}
            def save(self, **values): self.job.update(values)
        pending = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '7 /org/freedesktop/systemd1/job/7', 'NeedDaemonReload': 'no'}
        active = {**pending, 'ActiveState': 'active', 'Job': '0'}
        with patch.object(h, 'manager', side_effect=[pending, active]), patch.object(u, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'manager_job_pending'):
                b.control('start', Controller())
            run.assert_not_called()

    def test_stale_loaded_unit_prevents_start(self):
        b = self.binding()
        b.running = lambda: {'manager': {'pid': 20}, 'application': {'pid': 30}}
        class Controller:
            job = {}
            def save(self, **values): self.job.update(values)
        stale = {'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'yes'}
        active = {**stale, 'ActiveState': 'active'}
        with patch.object(h, 'manager', side_effect=[stale, active]), patch.object(u, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'service_reload_pending'):
                b.control('start', Controller())
            run.assert_not_called()

    def test_retained_child_prevents_replacement_after_parent_exit(self):
        b = self.binding()
        old = {'application': {'pid': 88, 'start_ticks': 3}}
        child = {'pid': 89, 'start_ticks': 4}
        with patch.object(h, 'process', side_effect=lambda pid: child if pid == 89 else None):
            self.assertFalse(b.gone(old, [child]))
        with patch.object(h, 'process', return_value=None):
            self.assertTrue(b.gone(old, [child]))

    def test_unknown_old_process_prevents_start(self):
        b = self.binding()
        b.applications = lambda: [{'pid': 88}]
        with patch.object(h, 'manager', return_value={'ActiveState': 'inactive', 'MainPID': '0', 'ControlPID': '0', 'Job': '0', 'NeedDaemonReload': 'no'}), patch.object(u, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'still_running'):
                b.control('start', type('Controller', (), {'job': {}})())
            run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
