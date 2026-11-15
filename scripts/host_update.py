#!/usr/bin/env python3
"""Owner-local updates for an existing, fixed Constellation container service.

No listener, agent command or container-side host control is installed. The
connector's in-app policy remains disabled. Work uses its existing plan journal,
admission inode and update controller; this module supplies only host admission
and the known container service boundary.
"""
import argparse
import fcntl
import hashlib
import importlib.util
import io
import json
import os
import re
import sys
import tarfile
import time
import uuid
from pathlib import Path

spec = importlib.util.spec_from_file_location('slack_update', Path(__file__).with_name('update.py'))
u = importlib.util.module_from_spec(spec)
spec.loader.exec_module(u)

# Exact installation recipe scripts from amplifier-constellation-instance.
LAUNCH_SHA = '9beb79215935b0a746a0989c353e1da9ebfb7b654873d6de4034466663dfb4b8'
SERVICE_SHA = '868b7976638f28b68045191f2195cb3b7456c1161c7b1e8a62ba73cf2f553906'
ACTIVE = u.ACTIVE | {'recovery_required'}


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def container_configuration(value):
    # Docker returns Mounts in varying order. Preserve every entry/field, while
    # treating that one collection as unordered; command and environment lists
    # in Config/HostConfig retain their original ordering.
    mounts = value['Mounts']
    destinations = [m['Destination'] for m in mounts]
    if len(destinations) != len(set(destinations)):
        raise RuntimeError('ambiguous_container_mounts')
    return fingerprint({'Config': value['Config'], 'HostConfig': value['HostConfig'],
                        'Mounts': sorted(mounts, key=lambda m: json.dumps(m, sort_keys=True))})


def validate_update_mounts(home, mounts):
    for path, writable in [(home, True), (home/'bin', False), (home/'updates', False)]:
        matches = [m for m in mounts if m['Destination'] == str(path)]
        if len(matches) != 1 or matches[0]['Source'] != str(path) or matches[0]['RW'] != writable:
            raise RuntimeError('container_update_mount_changed')

    for mount in mounts:
        destination = Path(mount['Destination'])
        source = Path(mount['Source']).resolve()
        for protected in (home/'bin', home/'updates'):
            if destination != protected and destination.is_relative_to(protected):
                raise RuntimeError('nested_update_mount')
            # The one exact home mount is intentionally writable beneath the
            # verified RO overlays. Any other writable alias exposes those files.
            ordinary_home = source == home and destination == home
            if mount['RW'] and not ordinary_home and (source.is_relative_to(protected) or protected.is_relative_to(source)):
                raise RuntimeError('writable_update_alias')


def owned(path):
    path = Path(path)
    stat = path.lstat()
    if path.is_symlink() or stat.st_uid != os.getuid() or stat.st_mode & 0o022:
        raise RuntimeError('host_control_ownership_changed')
    return path


def manager(unit):
    raw = u.run('/usr/bin/systemctl', '--user', 'show', unit,
                '-p', 'ActiveState', '-p', 'SubState', '-p', 'MainPID',
                '-p', 'ControlPID', '-p', 'ControlGroup', '-p', 'Job', '-p', 'NeedDaemonReload').decode()
    return dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)


def manager_idle(state):
    if state.get('NeedDaemonReload') != 'no':
        raise RuntimeError('service_reload_pending')
    # systemctl renders the empty D-Bus job tuple as a present `Job=` on
    # some supported versions. Missing data is still unknown, never idle.
    job = state.get('Job')
    if job is None or (job.strip() and job.split()[0] != '0'):
        raise RuntimeError('manager_job_pending')
    return (state['ActiveState'] in ('inactive', 'failed')
            and not int(state.get('MainPID') or 0)
            and not int(state.get('ControlPID') or 0))


def process(pid):
    if int(pid) <= 0:
        return None
    root = Path('/proc') / str(int(pid))
    try:
        stat = (root/'stat').read_text().rpartition(') ')[2].split()
        if stat[0] == 'Z':
            return None
        status = dict(line.split(':', 1) for line in (root/'status').read_text().splitlines() if ':' in line)
        return {'pid': int(pid), 'start_ticks': int(stat[19]),
                'uid': int(status['Uid'].split()[0]),
                'ppid': int(stat[1]), 'pgrp': int(stat[2]), 'session': int(stat[3]),
                'nspid': [int(v) for v in status.get('NSpid', str(pid)).split()],
                'cgroup': (root/'cgroup').read_text(),
                'argv': [v.decode() for v in (root/'cmdline').read_bytes().split(b'\0') if v]}
    except FileNotFoundError:
        return None


class ContainerService:
    def __init__(self, home, manifest_path, expected=None):
        if sys.platform != 'linux' or os.getuid() == 0:
            raise RuntimeError('linux_installation_owner_required')
        self.home = Path(home).resolve()
        self.manifest_path = owned(Path(manifest_path).resolve())
        self.config = u.read(self.home/'adapter.json')
        self.unit = 'acp-slack-adapter-' + str(uuid.UUID(self.config['id'])) + '.service'
        self.manifest = u.read(self.manifest_path)
        self.state = Path(self.manifest['state']).resolve()
        self.container = self.manifest['container']
        if not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', self.container):
            raise RuntimeError('invalid_container_identity')
        if self.manifest_path != self.state/'manifest.json':
            raise RuntimeError('unrecognized_container_manifest')
        self.argv = [str(self.home/'bin/acp-slack-adapter'), 'serve', '--home', str(self.home)]
        command = self.manifest['services'].get(self.unit, {})
        if command.get('argv') != self.argv or command.get('cwd') != str(self.home):
            raise RuntimeError('unrecognized_container_command')
        if {'home': str(self.home), 'kind': 'acp-slack-adapter'} not in self.manifest['homes']:
            raise RuntimeError('container_home_mismatch')
        self.expected = expected
        snapshot = self.snapshot()
        if expected is not None and snapshot != expected:
            raise RuntimeError('host_service_changed')
        self.expected = snapshot

    def show(self, key):
        return u.run('/usr/bin/systemctl', '--user', 'show', self.unit, '-p', key, '--value').decode().strip()

    def inspect(self):
        value = json.loads(u.run('/usr/bin/docker', 'inspect', self.container))[0]
        if value['Name'] != '/' + self.container or not value['State']['Running']:
            raise RuntimeError('container_unavailable')
        if value['HostConfig'].get('PidMode') == 'host' or value['HostConfig'].get('Privileged') or any(cap in ('SYS_ADMIN', 'CAP_SYS_ADMIN') for cap in (value['HostConfig'].get('CapAdd') or [])):
            raise RuntimeError('private_pid_namespace_required')
        return value

    def snapshot(self):
        units = Path.home()/'.config/systemd/user'
        fragment = owned(units/self.unit)
        drop = owned(units/(self.unit+'.d')/'50-container.conf')
        if self.show('FragmentPath') != str(fragment) or self.show('DropInPaths') != str(drop):
            raise RuntimeError('unrecognized_service_overrides')
        expected = 'ExecStart=' + u.quoted(self.home/'bin/acp-slack-adapter') + ' serve --home ' + u.quoted(self.home)
        allowed = [expected]
        if re.fullmatch(r'[a-zA-Z0-9/._-]+', str(self.home)):
            allowed.append('ExecStart='+' '.join(self.argv))
        if [line for line in fragment.read_text().splitlines() if line.startswith('ExecStart=')] not in [[v] for v in allowed]:
            raise RuntimeError('unrecognized_service_command')
        if any(c.isspace() or c in '%\\"' for c in str(self.state)):
            raise RuntimeError('unrecognized_container_state_path')
        expected_drop = (f'[Unit]\nRequires={self.container}.service\nAfter={self.container}.service\n'
                         f'PartOf={self.container}.service\n[Service]\nExecStart=\n'
                         f'ExecStart=/usr/bin/python3 {self.state}/launch-service.py {self.state}/services.json {self.unit}\nTimeoutStopSec=40\n')
        if drop.read_text() != expected_drop:
            raise RuntimeError('unrecognized_container_dropin')
        if self.show('NeedDaemonReload') != 'no':
            raise RuntimeError('service_reload_pending')
        effective = self.show('ExecStart')
        argv = f'/usr/bin/python3 {self.state}/launch-service.py {self.state}/services.json {self.unit}'
        if f'argv[]={argv} ;' not in effective or effective.count('argv[]=') != 1:
            raise RuntimeError('effective_service_command_changed')
        launch = owned(self.state/'launch-service.py')
        services = owned(self.state/'services.json')
        if u.digest(launch) != LAUNCH_SHA or u.read(services) != {'container': self.container, 'services': self.manifest['services']}:
            raise RuntimeError('container_launcher_changed')
        container = self.inspect()
        mounts = container['Mounts']
        validate_update_mounts(self.home, mounts)
        if any(m['Destination'] in ('/var/run/docker.sock', '/run/docker.sock', '/run/user') for m in mounts):
            raise RuntimeError('container_host_control_exposed')
        # Read one fixed image file, without executing a command inside the container.
        archive = u.run('/usr/bin/docker', 'cp', self.container+':/opt/constellation/service.py', '-')
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            entries = tar.getmembers()
            if len(entries) != 1 or not entries[0].isfile() or entries[0].size > 16384:
                raise RuntimeError('unrecognized_container_supervisor')
            if hashlib.sha256(tar.extractfile(entries[0]).read()).hexdigest() != SERVICE_SHA:
                raise RuntimeError('container_supervisor_changed')
        init = process(container['State']['Pid'])
        if not init:
            raise RuntimeError('container_generation_unavailable')
        policy = owned(self.home/'updates/policy.json')
        admission = owned(self.home/'updates/admission.lock').stat()
        return {'container_id': container['Id'], 'image_id': container['Image'],
                'init': {'pid': init['pid'], 'start_ticks': init['start_ticks'], 'cgroup': init['cgroup']},
                'container_configuration': container_configuration(container),
                'files': {str(p): u.digest(p) for p in (self.manifest_path, fragment, drop, launch, services)},
                'admission': [admission.st_dev, admission.st_ino],
                'adapter_id': self.config['id'],
                'owner': self.config['context']['owner_slack_user_id']}

    def verify(self):
        if self.snapshot() != self.expected:
            raise RuntimeError('host_service_changed')

    def require_policy(self, job):
        self.verify()
        policy = u.read(self.home/'updates/policy.json')
        config = u.read(self.home/'adapter.json')
        if (policy.get('schema') != 1 or policy.get('adapter_id') != config['id']
                or policy.get('enabled') is not False or policy.get('agent_enabled') is not False
                or policy.get('owner_slack_user_id') != config['context']['owner_slack_user_id']
                or u.digest(self.home/'updates/policy.json') != job.get('host_policy_sha256')
                or job.get('actor_kind') != 'host_operator'
                or job.get('policy_revision') != policy['revision']
                or job.get('authorized_owner') != policy['owner_slack_user_id']):
            raise RuntimeError('host_update_authority_changed')
        return policy

    def processes(self):
        raw = u.run('/usr/bin/docker', 'top', self.container, '-eo', 'pid').decode().splitlines()[1:]
        return [item for line in raw if (item := process(int(line.strip())))]

    def applications(self):
        found = []
        for item in self.processes():
            if item['argv'] == self.argv:
                if item['uid'] != os.getuid() or item['cgroup'] != self.expected['init']['cgroup']:
                    raise RuntimeError('application_ownership_changed')
                found.append(item)
        return found

    def descendants(self, app):
        all_processes = self.processes()
        ids = {app['pid']}
        while True:
            added = {p['pid'] for p in all_processes if p['ppid'] in ids}
            if added <= ids:
                break
            ids |= added
        # Child argv may contain private backend arguments; retain only the
        # process generation needed to prove exit.
        return [{'pid': p['pid'], 'start_ticks': p['start_ticks']}
                for p in all_processes if p['pid'] in ids and p['pid'] != app['pid']]

    def running(self):
        self.verify()
        state = manager(self.unit)
        manager_idle(state)
        wrapper = process(int(state.get('MainPID') or 0))
        expected = ['/usr/bin/python3', str(self.state/'launch-service.py'), str(self.state/'services.json'), self.unit]
        if state['ActiveState'] != 'active' or not wrapper or wrapper['uid'] != os.getuid() or wrapper['argv'] != expected:
            raise RuntimeError('service_not_running')
        if state.get('ControlGroup') and state['ControlGroup'] not in wrapper['cgroup']:
            raise RuntimeError('service_process_mismatch')
        apps = self.applications()
        if len(apps) != 1:
            raise RuntimeError('application_process_ambiguous')
        return {'manager': wrapper, 'application': apps[0]}

    def verify_running(self, expected_binary, endpoints, job):
        current = self.running()
        app = current['application']
        if endpoints['pid'] != app['nspid'][-1] or u.digest(Path('/proc')/str(app['pid'])/'exe') != expected_binary:
            raise RuntimeError('application_binary_or_namespace_mismatch')
        previous = job.get('host_stopped_processes')
        if previous and (current['manager'] == previous['manager'] or app == previous['application']):
            raise RuntimeError('old_application_process')
        return current

    def gone(self, records, children=()):
        for record in [*(records or {}).values(), *children]:
            now = process(record['pid'])
            if now and now['start_ticks'] == record['start_ticks']:
                return False
        app = (records or {}).get('application', {})
        if app.get('session') and any(p.get('session') == app['session'] for p in self.processes()):
            return False
        return not self.applications()

    def assert_stopped(self, job):
        self.verify()
        if not manager_idle(manager(self.unit)) or not self.gone(job.get('host_stopped_processes', job.get('host_live_processes', job.get('host_origin_processes'))), job.get('host_stopped_children', [])):
            raise RuntimeError('service_not_stopped')

    def control(self, action, controller):
        self.verify()
        state = manager(self.unit)
        idle = manager_idle(state)
        known = controller.job.get('host_live_processes', controller.job.get('host_origin_processes'))
        if action == 'stop':
            # A prior stop may have completed before its reply or worker was lost.
            # Observe it; never send Stop to a newly appeared process generation.
            if idle and self.gone(known, controller.job.get('host_stopped_children', [])):
                return
            current = self.running()
            if known and current != known:
                intent = controller.job.get('host_manager_action', {})
                # An interrupted Start can have created the expected replacement.
                # Bind the actual namespace identity and installed executable before
                # authorizing rollback; arbitrary successor processes are refused.
                endpoints = u.read(self.home/'endpoints.json')
                if intent.get('action') != 'start' or endpoints['started_at'] < intent['at']:
                    raise RuntimeError('application_generation_changed')
                self.verify_running(u.digest(self.home/'bin/acp-slack-adapter'), endpoints, controller.job)
            controller.save(host_stopped_processes=current, host_stopped_children=self.descendants(current['application']))
        else:
            if self.applications() or not idle:
                raise RuntimeError('application_still_running')
            current = controller.job.get('host_stopped_processes')
            if not self.gone(current, controller.job.get('host_stopped_children', [])):
                raise RuntimeError('old_application_process')
        # Record before dispatch. An ambiguous result is reconciled by observation,
        # never by issuing the manager command a second time.
        controller.save(host_manager_action={'action': action, 'at': int(time.time()*1000), 'outcome': 'unknown'})
        error = None
        try:
            u.run('/usr/bin/systemctl', '--user', action, self.unit, timeout=45)
        except Exception as exc:
            error = exc
        deadline = time.monotonic()+45
        while time.monotonic() < deadline:
            self.verify()
            state = manager(self.unit)
            try:
                idle = manager_idle(state)
            except RuntimeError as problem:
                if str(problem) == 'manager_job_pending':
                    time.sleep(.1)
                    continue
                raise
            if action == 'stop':
                if self.gone(current, controller.job.get('host_stopped_children', [])) and idle:
                    controller.save(host_manager_action={'action': action, 'outcome': 'confirmed'})
                    return
            elif state['ActiveState'] == 'active':
                controller.save(host_live_processes=self.running(), host_manager_action={'action': action, 'outcome': 'confirmed'})
                return
            time.sleep(.1)
        raise RuntimeError('service_action_outcome_unknown') from error


def unit_name(identifier, action):
    return 'acp-slack-host-update-' + identifier + '-' + action + '.service'


def dispatch(home, manifest_path, identifier, action):
    # Fixed host-side code only. The source files are hash-bound in each plan.
    command = ['/usr/bin/systemd-run', '--user', '--collect', '--quiet', '--service-type=exec',
               '--property=UMask=0077', '--property=Restart=on-failure', '--property=RestartSec=5',
               '--property=RuntimeMaxSec=3600', '--unit='+unit_name(identifier, action),
               '/usr/bin/python3', '-I', '-B', str(Path(__file__).resolve()), '--home', str(home),
               '--container-manifest', str(manifest_path), '--plan-id', identifier, '--action', 'worker']
    u.run(*command, timeout=30)


def code_hashes():
    return {p.name: u.digest(p) for p in (Path(__file__), Path(__file__).with_name('update.py'))}


def queue_action(home, manifest_path, action, identifier, *, dispatch=True):
    home, manifest_path = Path(home).resolve(), Path(manifest_path).resolve()
    identifier = str(uuid.UUID(identifier))
    root = home/'updates'
    # These controls must already exist and be mounted read-only in the container.
    owned(root/'admission.lock')
    file = root/'plans'/identifier/'plan.json'
    if action == 'status':
        return u.read(file)
    with (root/'queue.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        existing = u.read(file) if file.exists() else None
        if existing and existing.get('host_source') != code_hashes():
            raise RuntimeError('host_worker_source_changed')
        if existing and existing.get('host_manifest') != str(manifest_path):
            raise RuntimeError('host_manifest_changed')
        # A retry observes the same accepted request, including terminal failure.
        continuing = existing and action == 'apply' and existing.get('state') == 'deferred'
        if existing and action in existing.get('host_accepted_actions', []) and not continuing:
            return existing
        if continuing:
            if not manager_idle(manager(unit_name(identifier, existing['action']))):
                return existing
            if sum(u.pending_counts(home).values()):
                return existing
        active = u.read(root/'active.json') if (root/'active.json').exists() else None
        if active:
            prior = u.read(root/'plans'/str(uuid.UUID(active['id']))/'plan.json')
            if prior['id'] != identifier and prior['state'] in ACTIVE:
                raise RuntimeError('another_update_unsettled')
        binding = ContainerService(home, manifest_path, existing.get('host_service') if existing else None)
        policy = u.read(root/'policy.json')
        if action == 'plan':
            if existing:
                raise RuntimeError('plan_already_exists')
            job = {'id': identifier, 'created_at': int(time.time()*1000), 'host_manifest': str(manifest_path),
                   'host_service': binding.expected, 'host_source': code_hashes(), 'host_accepted_actions': [],
                   'policy_revision': policy['revision'], 'authorized_owner': policy['owner_slack_user_id'],
                   'host_policy_sha256': u.digest(root/'policy.json'),
                   'actor_kind': 'host_operator'}
            binding.require_policy(job)
            job['host_origin_processes'] = binding.verify_running(u.digest(home/'bin/acp-slack-adapter'), u.read(home/'endpoints.json'), job)
        elif action in ('apply', 'rollback'):
            job = existing
            if not job or job['state'] not in ({'ready', 'deferred'} if action == 'apply' else {'succeeded', 'recovery_required'}):
                raise RuntimeError('plan_not_ready')
            binding.require_policy(job)
        elif action == 'recover':
            job = existing
            if not job or job['state'] not in u.ACTIVE:
                raise RuntimeError('no_interrupted_worker')
            state = manager(unit_name(identifier, job['action']))
            if not manager_idle(state):
                return job
            job.setdefault('host_recovery_attempts', []).append({'at': int(time.time()*1000), 'outcome': 'unknown'})
            u.write(file, job)
            if dispatch:
                try:
                    globals()['dispatch'](home, manifest_path, identifier, job['action'])
                except Exception:
                    pass
            return u.read(file)
        else:
            raise RuntimeError('unsupported_host_action')
        job['host_accepted_actions'].append(action)
        job.update(action=action, state='rolling_back' if job.get('state') == 'recovery_required' else 'queued', updated_at=int(time.time()*1000),
                   host_dispatch={'unit': unit_name(identifier, action), 'outcome': 'unknown'})
        u.write(file, job)
        u.write(root/'active.json', {'id': identifier})
        if dispatch:
            try:
                globals()['dispatch'](home, manifest_path, identifier, action)
            except Exception:
                # No guess about whether systemd accepted it; status/recover reconcile.
                return u.read(file)
        return u.read(file)


def run_worker(home, manifest_path, identifier):
    home = Path(home).resolve()
    root = home/'updates'
    identifier = str(uuid.UUID(identifier))
    with (root/'worker.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        file = root/'plans'/identifier/'plan.json'
        job = u.read(file)
        if job['state'] not in u.ACTIVE or u.read(root/'active.json')['id'] != identifier:
            return job
        if job.get('host_source') != code_hashes() or job.get('host_manifest') != str(Path(manifest_path).resolve()):
            raise RuntimeError('host_worker_source_changed')
        try:
            binding = ContainerService(home, manifest_path, job['host_service'])
            controller = u.Controller(home, identifier, binding)
            controller.execute()
        except Exception as error:
            reason = str(error)
            if not re.fullmatch('[a-z_0-9]{1,80}', reason):
                reason = 'host_update_unavailable'
            critical = job['state'] in {'stopping', 'swapping', 'verifying', 'rolling_back'}
            job.update(state='recovery_required' if critical else 'failed', reason=reason)
            u.write(file, job)
        return u.read(file)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--home', required=True, type=Path)
    parser.add_argument('--container-manifest', required=True, type=Path)
    parser.add_argument('--plan-id', required=True)
    parser.add_argument('--action', required=True, choices=['plan', 'apply', 'rollback', 'status', 'recover', 'worker'])
    args = parser.parse_args()
    os.umask(0o077)
    if args.action == 'worker':
        value = run_worker(args.home, args.container_manifest, args.plan_id)
    else:
        value = queue_action(args.home, args.container_manifest, args.action, args.plan_id)
    print(json.dumps(value, sort_keys=True))


if __name__ == '__main__':
    main()
