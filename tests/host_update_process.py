"""Disposable Linux Docker + user-systemd qualification for host-owned updates.

Uses real old/new release binaries, the exact Instance container launcher and
supervisor, actual systemd/Docker process control, loopback Slack, and an ordinary
deterministic ACP program. Planning uses a fixture host subprocess to replace
GitHub acquisition with the supplied binary. Apply uses the packaged command and
its real transient systemd worker. No live installation,
Slack credentials, model, or provider is used. Requires aiohttp and Docker.

Run with --old-binary PATH --new-binary PATH --instance-source PATH --image TAG.
The supplied image must contain Python, libpam, and the exact Instance service.py
at /opt/constellation/service.py. --build-image builds a disposable Ubuntu image.
"""
import argparse
import asyncio
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import ssl
import subprocess
import sys
import tempfile
import time
import uuid

from aiohttp import ClientSession, web
from process_integration import SlackFixture, private_json, protocol, rows, until

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(*args, check=True):
    result = subprocess.run([str(v) for v in args], capture_output=True, text=True)
    if check and result.returncode:
        raise AssertionError((args, result.returncode, result.stdout, result.stderr))
    return result.stdout.strip()


def host_module(path=None):
    spec = importlib.util.spec_from_file_location('host_update', path or ROOT/'scripts/host_update.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # Diagnostic only: retain the real refusal and show its exact fixture-owned
    # binding difference. No check is suppressed or weakened.
    verify = module.ContainerService.verify
    def observed_verify(binding):
        try:
            return verify(binding)
        except RuntimeError:
            current = binding.snapshot()
            changed = {key: {'expected': binding.expected.get(key), 'observed': value}
                       for key, value in current.items() if binding.expected.get(key) != value}
            print('FIXTURE_BINDING_DIFFERENCE '+json.dumps(changed, sort_keys=True), file=sys.stderr, flush=True)
            raise
    module.ContainerService.verify = observed_verify
    return module


def fixture_worker(args):
    """Replace download selection only; execute the production worker unchanged."""
    host = host_module(args.worker_script)

    def offline_plan(controller):
        controller.require_policy()
        controller.save('planning')
        candidate = controller.folder/'candidate'
        shutil.copyfile(args.new_binary, candidate)
        candidate.chmod(0o700)
        controller.save('ready', repository=host.u.REPO,
                        source_commit=args.source_commit, workflow='fixture-offline-acquisition',
                        binary_sha256=digest(candidate), installed_sha256=digest(controller.binary),
                        version=command(candidate, '--version'),
                        up_to_date=digest(candidate) == digest(controller.binary),
                        fixture_acquisition=True)

    host.u.Controller.plan = offline_plan
    result = host.run_worker(args.worker_home, args.worker_manifest, args.worker_id)
    print(json.dumps(result, sort_keys=True))


class FaultSlack(SlackFixture):
    def __init__(self):
        super().__init__()
        self.fault_home = None
        self.candidate_hash = None
        self.fail_candidate = False
        self.fault_observed = False

    async def api(self, request):
        if (self.fail_candidate and request.match_info['method'] == 'apps.connections.open'
                and digest(self.fault_home/'bin/acp-slack-adapter') == self.candidate_hash):
            assert request.headers.get('Authorization', '').startswith('Bearer test-')
            if not self.fault_observed:
                # A new retained row appears after the backup. Rollback must not
                # restore the database snapshot over this independently received row.
                with sqlite3.connect(self.fault_home/'state.sqlite3') as db:
                    db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES(?,?,?,?,?,?)",
                               ('during-failed-start', 'fixture-retained', '{}', 'unknown', 2, 2))
                self.fault_observed = True
            return web.json_response({'ok': False, 'error': 'fixture_connection_unavailable'}, status=503)
        return await super().api(request)


async def run(args):
    assert sys.platform == 'linux' and os.getuid() != 0
    assert args.old_binary.is_file() and args.new_binary.is_file()
    host = host_module()
    old_hash, new_hash = digest(args.old_binary), digest(args.new_binary)
    assert old_hash != new_hash, 'This qualification needs distinct actual release binaries'
    source = args.instance_source/'ops/container'
    assert digest(source/'launch-service.py') == host.LAUNCH_SHA
    assert digest(source/'service.py') == host.SERVICE_SHA
    fixture = FaultSlack()
    app = web.Application()
    app.router.add_post('/api/{method}', fixture.api)
    app.router.add_get('/api/{method}', fixture.api)
    app.router.add_get('/socket/{name}', fixture.socket)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, '127.0.0.1', 0)
    await site.start()
    fixture.port = site._server.sockets[0].getsockname()[1]
    checks, units, containers, workers, extracted, worker_units = [], [], [], [], [], []
    unit_definitions = {}
    worker_script = None
    unit_root = Path.home()/'.config/systemd/user'
    unit_root.mkdir(parents=True, exist_ok=True)
    result = {'scope': 'disposable_real_docker_systemd_loopback_slack_plain_acp',
              'artifact_acquisition': 'explicit_offline_fixture', 'actual_model_coverage': False,
              'worker_launch': 'plan: fixture host subprocess; apply: actual packaged command and transient systemd worker',
              'production_changes': False, 'old_binary_sha256': old_hash,
              'new_binary_sha256': new_hash, 'checks': checks}

    def passed(name):
        checks.append(name)
        print('PASS '+name, flush=True)

    async def shell(*values):
        return await asyncio.to_thread(command, *values)

    async def worker(home, manifest, identifier):
        # A real host process, separate from the connector's container. The
        # production queue and worker journal are reused; only the acquisition
        # function in this test process is substituted above.
        proc = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).resolve()),
            '--worker-home', str(home), '--worker-manifest', str(manifest), '--worker-id', identifier,
            '--worker-script', str(worker_script),
            '--new-binary', str(args.new_binary), '--source-commit', args.source_commit,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        workers.append(proc)
        output, error = await asyncio.wait_for(proc.communicate(), 180)
        assert proc.returncode == 0, (output.decode(), error.decode())
        if error:
            print(error.decode(), file=sys.stderr, flush=True)
        return json.loads(output)

    async def operation(home, manifest, action, identifier):
        if action == 'plan':
            await asyncio.to_thread(host.queue_action, home, manifest, action, identifier, dispatch=False)
            value = await worker(home, manifest, identifier)
        else:
            worker_unit = host.unit_name(identifier, action)
            worker_units.append(worker_unit)
            await shell(args.new_binary, '--home', home, 'host-update', action,
                        '--container-manifest', manifest, '--plan-id', identifier)
            file = home/'updates/plans'/identifier/'plan.json'
            await until(lambda: json.loads(file.read_text())['state'] not in host.u.ACTIVE,
                        'actual packaged host worker reaches terminal result', timeout=180)
            value = json.loads(file.read_text())
        result.setdefault('operation_results', []).append(value)
        return value

    async def api(home, path, body):
        endpoint = json.loads((home/'endpoints.json').read_text())['base_url']
        trust = ssl.create_default_context(cafile=str(home/'tls/root-ca.crt'))
        token = (home/'secrets/control-token').read_text().strip()
        async with ClientSession() as client:
            async with client.post(endpoint+'/api/v1/'+path, json=body, ssl=trust,
                                   headers={'Authorization': 'Bearer '+token}) as response:
                value = await response.json()
                assert response.status < 300, value
                return value

    async def settled(home):
        await until(lambda: not rows(home, "SELECT id FROM jobs WHERE status='running'")
                    and not rows(home, "SELECT id FROM outbox WHERE status IN ('ready','sending')"),
                    'real connector work settled')

    with tempfile.TemporaryDirectory(prefix='slack-host-update-') as temporary:
        base = Path(temporary)
        try:
            if args.build_image:
                image_source = base/'image'
                image_source.mkdir()
                shutil.copy2(source/'service.py', image_source/'service.py')
                (image_source/'Dockerfile').write_text(
                    'FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y --no-install-recommends '
                    'python3 libpam0g ca-certificates && rm -rf /var/lib/apt/lists/*\n'
                    f'RUN getent passwd {os.getuid()} || useradd --uid {os.getuid()} fixtureowner\n'
                    'COPY service.py /opt/constellation/service.py\nCMD ["/usr/bin/sleep", "infinity"]\n')
                await shell('/usr/bin/docker', 'build', '-t', args.image, image_source)
            image_owner = await shell('/usr/bin/docker', 'run', '--rm', '--network', 'none',
                '--user', f'{os.getuid()}:{os.getgid()}', args.image, '/usr/bin/id', '-un')

            async def installation(name):
                home, state = base/name, base/(name+'-container')
                for directory in (home, home/'bin', home/'secrets', home/'endpoint', home/'updates', state):
                    directory.mkdir(mode=0o700)
                shutil.copy2(args.old_binary, home/'bin/acp-slack-adapter')
                (home/'bin/acp-slack-adapter').chmod(0o700)
                shutil.copy2(ROOT/'tests/protocol_acp_fixture.py', home/'endpoint/agent.py')
                config = {'schema': 1, 'id': str(uuid.uuid4()), 'label': name, 'revision': 1,
                    'enabled': True, 'setup_complete': True, 'participation': 'mentions',
                    'acp': {'command': '/usr/bin/python3', 'args': [str(home/'endpoint/agent.py'),
                        str(home/'endpoint'), name], 'cwd': str(home/'endpoint'), 'timeout_seconds': 60, 'env': {}},
                    'slack': {'team_id': 'Ttest', 'app_id': 'A'+name, 'bot_user_id': 'U'+name,
                        'bot_id': 'B'+name, 'channels': ['C'+name, 'D'+name], 'allow_dms': True,
                        'loopback_test': True, 'api_base': f'http://127.0.0.1:{fixture.port}/api/'},
                    'context': {'owner_slack_user_id': 'UHUMAN'},
                    'attention': {'parallel_contexts': 2, 'settle_ms': 30, 'minimum_reply_interval_seconds': 0,
                        'max_replies_per_context_per_hour': 100, 'max_replies_per_agent_per_hour': 100,
                        'max_decisions_per_agent_per_hour': 200, 'max_bot_replies_without_human': 4},
                    'server': {'bind': '127.0.0.1', 'port': 0, 'owner': image_owner,
                        'pam_service': 'acp-slack-adapter', 'public_url': None, 'embed_origins': []}}
                private_json(home/'adapter.json', config)
                private_json(home/'secrets/slack.json', {'app_token': 'test-'+name+'-app', 'bot_token': 'test-'+name})
                private_json(home/'updates/policy.json', {'schema': 1, 'adapter_id': config['id'], 'revision': 1,
                    'enabled': False, 'agent_enabled': False, 'owner_slack_user_id': 'UHUMAN'})
                (home/'updates/admission.lock').touch(mode=0o600)
                unit = 'acp-slack-adapter-'+config['id']+'.service'
                extracted.append(Path.home()/'.local/share/acp-slack-host-updates'/config['id'])
                container = 'slack-update-fixture-'+uuid.uuid4().hex[:16]
                container_unit = container+'.service'
                services = {unit: {'argv': [str(home/'bin/acp-slack-adapter'), 'serve', '--home', str(home)],
                                   'cwd': str(home)}}
                shutil.copy2(source/'launch-service.py', state/'launch-service.py')
                private_json(state/'services.json', {'container': container, 'services': services})
                private_json(state/'manifest.json', {'container': container, 'state': str(state),
                    'image': args.image, 'homes': [{'kind': 'acp-slack-adapter', 'home': str(home)}],
                    'services': services})
                await shell('/usr/bin/docker', 'create', '--name', container, '--network', 'host',
                    '--user', f'{os.getuid()}:{os.getgid()}', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                    '--mount', f'type=bind,source={home},target={home}',
                    '--mount', f'type=bind,source={home}/bin,target={home}/bin,readonly',
                    '--mount', f'type=bind,source={home}/updates,target={home}/updates,readonly', args.image)
                containers.append(container)
                (unit_root/container_unit).write_text('[Unit]\nDescription=Disposable Slack test container\n[Service]\n'
                    f'ExecStart=/usr/bin/docker start --attach {container}\nExecStop=/usr/bin/docker stop --time 10 {container}\n')
                (unit_root/unit).write_text('[Unit]\nDescription=Disposable Slack adapter\n[Service]\n'
                    f'ExecStart={home}/bin/acp-slack-adapter serve --home {home}\n')
                drop = unit_root/(unit+'.d')
                drop.mkdir(mode=0o700)
                (drop/'50-container.conf').write_text(f'[Unit]\nRequires={container_unit}\nAfter={container_unit}\n'
                    f'PartOf={container_unit}\n[Service]\nExecStart=\nExecStart=/usr/bin/python3 {state}/launch-service.py '
                    f'{state}/services.json {unit}\nTimeoutStopSec=40\n')
                for own_unit in (container_unit, unit):
                    files = [unit_root/own_unit]
                    if own_unit == unit:
                        files.append(drop/'50-container.conf')
                    unit_definitions[own_unit] = {str(p): digest(p) for p in files}
                units.extend([container_unit, unit])
                await shell('/usr/bin/systemctl', '--user', 'daemon-reload')
                await shell('/usr/bin/systemctl', '--user', 'start', unit)
                await until(lambda: name in fixture.sockets, name+' real container Socket Mode')
                return home, state/'manifest.json', unit

            home, manifest, unit = await installation('alpha')
            # Exercise the packaged front door. An unknown status ID is expected
            # to refuse; extraction is not permission to create or apply a plan.
            unused = str(uuid.uuid4())
            extraction = await asyncio.to_thread(subprocess.run,
                [str(args.new_binary), '--home', str(home), 'host-update', 'status',
                 '--container-manifest', str(manifest), '--plan-id', unused], capture_output=True, text=True)
            assert extraction.returncode != 0
            identity = json.loads((home/'adapter.json').read_text())['id']
            control = Path.home()/'.local/share/acp-slack-host-updates'/identity
            folders = list(control.glob('worker-*'))
            assert len(folders) == 1
            worker_script = folders[0]/'host_update.py'
            assert worker_script.read_bytes() == (ROOT/'scripts/host_update.py').read_bytes()
            assert (folders[0]/'update.py').read_bytes() == (ROOT/'scripts/update.py').read_bytes()
            assert not (home/'updates/plans'/unused/'plan.json').exists()
            host = host_module(worker_script)
            passed('Packaged owner CLI extracts exact immutable host modules outside container mounts')
            await fixture.send('Dalpha', 'ask alpha: before-update')
            await until(lambda: any('before-update' in p['markdown_text'] for p in fixture.posts), 'original response')
            await settled(home)
            binding = rows(home, 'SELECT context,endpoint,session_id,resumable,channel,thread FROM session_bindings')
            assert len(binding) == 1
            with sqlite3.connect(home/'state.sqlite3') as db:
                db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES(?,?,?,?,?,?)",
                           ('prior-unknown', 'fixture-retained', '{}', 'unknown', 1, 1))
            protected = {str(p.relative_to(home)): digest(p) for p in [home/'adapter.json', home/'updates/policy.json',
                home/'secrets/slack.json', home/'secrets/control-token', home/'tls/root-ca.crt', home/'tls/root-ca.key'] if p.exists()}
            retained = {}
            for table in ('messages', 'jobs', 'outbox', 'outbox_parts'):
                columns = [entry[1] for entry in rows(home, 'PRAGMA table_info('+table+')')]
                retained[table] = (columns, rows(home, 'SELECT '+','.join(columns)+' FROM '+table))
            def retained_unchanged():
                for table, (columns, original) in retained.items():
                    current = rows(home, 'SELECT '+','.join(columns)+' FROM '+table)
                    assert all(row in current for row in original), table
            result['original_record_counts'] = {table: len(values) for table, (_, values) in retained.items()}
            initial_service = await asyncio.to_thread(host.ContainerService, home, manifest)
            initial_snapshot = initial_service.expected
            initial_prompts = len(protocol(home, 'prompt'))
            passed('Actual old connector established a retained session through loopback Slack and ordinary ACP')

            fixture.fault_home, fixture.candidate_hash, fixture.fail_candidate = home, new_hash, True
            rollback_id = str(uuid.uuid4())
            plan = await operation(home, manifest, 'plan', rollback_id)
            assert plan['state'] == 'ready', plan
            failed = await operation(home, manifest, 'apply', rollback_id)
            assert failed['state'] == 'rolled_back', failed
            assert failed['reason'] == 'candidate_failed_health' and fixture.fault_observed
            assert digest(home/'bin/acp-slack-adapter') == old_hash
            assert rows(home, "SELECT id,status FROM jobs WHERE id IN ('prior-unknown','during-failed-start') ORDER BY id") == [
                ('during-failed-start', 'unknown'), ('prior-unknown', 'unknown')]
            assert rows(home, 'SELECT context,endpoint,session_id,resumable,channel,thread FROM session_bindings') == binding
            assert len(protocol(home, 'prompt')) == initial_prompts
            retained_unchanged()
            passed('Real failed readiness rolls back only the executable and preserves a newer unknown row and original session')

            fixture.fail_candidate = False
            success_id = str(uuid.uuid4())
            assert (await operation(home, manifest, 'plan', success_id))['state'] == 'ready'
            success = await operation(home, manifest, 'apply', success_id)
            assert success['state'] == 'succeeded', success
            assert digest(home/'bin/acp-slack-adapter') == new_hash
            assert (await asyncio.to_thread(host.ContainerService, home, manifest)).expected == initial_snapshot
            assert all(digest(home/p) == value for p, value in protected.items())
            assert rows(home, 'SELECT context,endpoint,session_id,resumable,channel,thread FROM session_bindings') == binding
            assert len(protocol(home, 'prompt')) == initial_prompts
            retained_unchanged()
            passed('The same supported path updates successfully without changing container, wrapper, backend, protected state, or replaying prompts')
            sessions_before_check = len(protocol(home, 'session_new'))
            checked = await api(home, 'check', {'target': 'all'})
            assert checked['ok'] and checked['slack']['ok'] and checked['acp']['ok'], checked
            assert len(protocol(home, 'prompt')) == initial_prompts
            # Existing check/all intentionally creates a diagnostic ACP session.
            # It must not replace the original conversation's retained binding.
            sessions_after_check = len(protocol(home, 'session_new'))
            assert sessions_after_check == sessions_before_check+1
            assert rows(home, 'SELECT context,endpoint,session_id,resumable,channel,thread FROM session_bindings') == binding
            result['diagnostic_check'] = {'new_diagnostic_sessions': 1, 'prompts': 0, 'original_binding_preserved': True}
            await fixture.send('Dalpha', 'ask alpha: after-update')
            await until(lambda: any('after-update' in p['markdown_text'] for p in fixture.posts), 'fresh response after update')
            await settled(home)
            assert len(protocol(home, 'session_new')) == sessions_after_check
            assert protocol(home, 'session_load')[-1]['session_id'] == binding[0][2]
            assert len(protocol(home, 'prompt')) == initial_prompts+1
            assert rows(home, 'SELECT context,endpoint,session_id,resumable,channel,thread FROM session_bindings') == binding
            passed('Fresh qualification and a new input recover the same original session; no old turn is replayed')
            retry = json.loads(await shell(args.new_binary, '--home', home, 'host-update', 'apply',
                '--container-manifest', manifest, '--plan-id', success_id))
            assert retry == success
            packaged_status = json.loads(await shell(args.new_binary, '--home', home, 'host-update', 'status',
                '--container-manifest', manifest, '--plan-id', success_id))
            assert packaged_status == success
            subsequent_id = str(uuid.uuid4())
            subsequent = await operation(home, manifest, 'plan', subsequent_id)
            assert subsequent['state'] == 'ready' and subsequent['up_to_date']
            passed('Lost action acknowledgement reads the retained result and a subsequent plan uses the same host path')

            other, other_manifest, _ = await installation('beta')
            with sqlite3.connect(other/'state.sqlite3') as db:
                db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES(?,?,?,?,?,?)",
                           ('uncertain-job', 'fixture', '{}', 'unknown', 1, 1))
                db.execute("INSERT INTO outbox(id,job_id,context,channel,thread,text,status,created,available) VALUES(?,?,?,?,?,?,?,?,?)",
                           ('uncertain-outbox', 'uncertain-job', 'fixture', 'Dbeta', '', 'not safe to resend', 'uncertain', 1, 1))
            before = (await asyncio.to_thread(host.ContainerService, other, other_manifest)).running()
            uncertain_id = str(uuid.uuid4())
            assert (await operation(other, other_manifest, 'plan', uncertain_id))['state'] == 'ready'
            deferred = await operation(other, other_manifest, 'apply', uncertain_id)
            assert deferred['state'] == 'deferred' and deferred['reason'] == 'uncertain_delivery_requires_reconciliation', deferred
            assert (await asyncio.to_thread(host.ContainerService, other, other_manifest)).running() == before
            assert digest(other/'bin/acp-slack-adapter') == old_hash
            assert rows(other, "SELECT status FROM outbox WHERE id='uncertain-outbox'") == [('uncertain',)]
            assert not protocol(other, 'prompt')
            passed('An uncertain outbox remains unknown and prevents a stop, update, resend, or prompt')
            result.update(status='passed', source_hashes=host.code_hashes(),
                          plans=[failed, success, subsequent, deferred])
        except Exception as error:
            result.update(status='failed', error=repr(error))
            if args.receipt:
                args.receipt.parent.mkdir(parents=True, exist_ok=True)
                for unit in units+worker_units:
                    journal = await asyncio.to_thread(command, '/usr/bin/journalctl', '--user', '-u', unit,
                                                      '--no-pager', '-n', '100', check=False)
                    (args.receipt.parent/(unit+'.log')).write_text(journal)
            raise
        finally:
            cleanup_errors, final_units = [], {}
            for proc in workers:
                if proc.returncode is None:
                    proc.kill()
                    await proc.wait()
            for unit in worker_units+list(reversed(units)):
                definitions = unit_definitions.get(unit, {})
                unchanged = all(Path(p).is_file() and digest(p) == expected for p, expected in definitions.items())
                if not unchanged:
                    cleanup_errors.append('unit_definition_changed:'+unit)
                    continue
                await asyncio.to_thread(command, '/usr/bin/systemctl', '--user', 'stop', unit, check=False)
                raw = await asyncio.to_thread(command, '/usr/bin/systemctl', '--user', 'show', unit,
                    '-p', 'LoadState', '-p', 'ActiveState', '-p', 'SubState', '-p', 'MainPID', '-p', 'ControlPID', '-p', 'Job', check=False)
                state = dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)
                final_units[unit] = state
                quiet = (state.get('ActiveState') in ('inactive', 'failed')
                    and state.get('MainPID', '0') == '0' and state.get('ControlPID', '0') == '0'
                    and 'Job' in state and (not state['Job'] or state['Job'].split()[0] == '0'))
                if not quiet:
                    cleanup_errors.append('unit_not_quiescent:'+unit)
                await asyncio.to_thread(command, '/usr/bin/systemctl', '--user', 'reset-failed', unit, check=False)
                if unit in unit_definitions:
                    (unit_root/unit).unlink(missing_ok=True)
                    shutil.rmtree(unit_root/(unit+'.d'), ignore_errors=True)
            for container in containers:
                await asyncio.to_thread(command, '/usr/bin/docker', 'rm', '-f', container, check=False)
            for control in extracted:
                shutil.rmtree(control, ignore_errors=True)
            await asyncio.to_thread(command, '/usr/bin/systemctl', '--user', 'daemon-reload', check=False)
            await runner.cleanup()
            remaining = []
            for container in containers:
                inspected = await asyncio.to_thread(subprocess.run,
                    ['/usr/bin/docker', 'inspect', container], capture_output=True, text=True)
                if inspected.returncode == 0:
                    remaining.append(container)
            result['cleanup'] = {'containers_removed': not remaining, 'remaining': remaining,
                'owned_units_removed': all(not (unit_root/u).exists() and not (unit_root/(u+'.d')).exists() for u in units),
                'extracted_modules_removed': all(not path.exists() for path in extracted),
                'unit_definitions_sha256': unit_definitions, 'final_unit_states': final_units,
                'errors': cleanup_errors}
            cleanup_ok = (not remaining and not cleanup_errors and result['cleanup']['owned_units_removed']
                          and result['cleanup']['extracted_modules_removed'])
            if not cleanup_ok:
                result.update(status='failed', cleanup_failure=True)
            if args.receipt:
                args.receipt.parent.mkdir(parents=True, exist_ok=True)
                args.receipt.write_text(json.dumps(result, indent=2)+'\n')
            assert cleanup_ok, result['cleanup']
    print(json.dumps({'status': 'passed', 'checks': len(checks)}, sort_keys=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-binary', type=Path)
    parser.add_argument('--new-binary', required=True, type=Path)
    parser.add_argument('--instance-source', type=Path)
    parser.add_argument('--image', default='slack-host-update-fixture:local')
    parser.add_argument('--build-image', action='store_true')
    parser.add_argument('--source-commit', default='6a2aaa27462125ac676806ea7b65001985c42ba1')
    parser.add_argument('--receipt', type=Path)
    parser.add_argument('--worker-home', type=Path)
    parser.add_argument('--worker-manifest', type=Path)
    parser.add_argument('--worker-id')
    parser.add_argument('--worker-script', type=Path)
    args = parser.parse_args()
    args.new_binary = args.new_binary.resolve()
    if args.worker_home:
        fixture_worker(args)
    else:
        assert args.old_binary and args.instance_source
        args.old_binary, args.instance_source = args.old_binary.resolve(), args.instance_source.resolve()
        asyncio.run(run(args))


if __name__ == '__main__':
    main()
