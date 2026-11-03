#!/usr/bin/env python3
"""Installation-local updater. Its only source and service targets are fixed below.

Invoked by the authenticated connector controller in a separate systemd unit.
Never accepts a command, URL, binary path, service name, or credential from ACP.
"""
import argparse, fcntl, hashlib, json, os, platform, shutil
import sqlite3, ssl, subprocess, time, urllib.request, uuid, zipfile
from pathlib import Path

REPO = 'microsoft/amplifier-acp-slack'
WORKFLOW = 'check.yml'
ACTIVE = {'queued', 'planning', 'draining', 'stopping', 'swapping', 'verifying', 'rolling_back'}
MAX_ARCHIVE = 128 * 1024 * 1024

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1024 * 1024), b''): h.update(b)
    return h.hexdigest()

def read(path): return json.loads(path.read_text())
def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = path.with_suffix('.next')
    with temporary.open('w') as f:
        json.dump(value, f, indent=2); f.write('\n'); f.flush(); os.fsync(f.fileno())
    temporary.chmod(0o600); temporary.replace(path)
    fd = os.open(path.parent, os.O_RDONLY)
    try: os.fsync(fd)
    finally: os.close(fd)

def run(*args, timeout=120):
    # Captured stderr may include local paths/account state. Never expose it in receipts.
    result = subprocess.run(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout, check=False)
    if result.returncode: raise RuntimeError('required_command_failed')
    return result.stdout

def gh(path):
    return json.loads(run('/usr/bin/gh', 'api', '--hostname', 'github.com', path))

def quoted(value):
    return '"' + str(value).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%').replace('$', '$$') + '"'

def service(home, config):
    if platform.system() != 'Linux': raise RuntimeError('linux_systemd_required')
    if os.geteuid() == 0: raise RuntimeError('installation_owner_required')
    unit = 'acp-slack-adapter-' + str(uuid.UUID(config['id'])) + '.service'
    fragment = Path.home() / '.config/systemd/user' / unit
    # Enrollment recognizes the installer's exact service, not arbitrary user units.
    actual = run('/usr/bin/systemctl', '--user', 'show', unit, '-p', 'FragmentPath', '--value').decode().strip()
    if actual != str(fragment) or fragment.is_symlink(): raise RuntimeError('unrecognized_service')
    if run('/usr/bin/systemctl', '--user', 'show', unit, '-p', 'DropInPaths', '--value').strip():
        raise RuntimeError('service_override_requires_operator_review')
    expected = 'ExecStart=' + quoted(home/'bin/acp-slack-adapter') + ' serve --home ' + quoted(home)
    allowed=[expected]
    # Earlier installers emitted the same fixed argv without quotes for simple
    # paths. Accept only that exact representation, with no systemd expansions.
    if all(ch.isascii() and (ch.isalnum() or ch in '/._-') for ch in str(home)):
        allowed.append('ExecStart='+str(home/'bin/acp-slack-adapter')+' serve --home '+str(home))
    entries = [line for line in fragment.read_text().splitlines() if line.startswith('ExecStart=')]
    if len(entries)!=1 or entries[0] not in allowed: raise RuntimeError('unrecognized_service_command')
    return unit

def policy(home, config, agent=False):
    p = read(home/'updates/policy.json')
    if p.get('schema') != 1 or p.get('adapter_id') != config['id'] or not p.get('enabled'):
        raise RuntimeError('updates_disabled')
    if agent and (not p.get('agent_enabled') or p.get('owner_slack_user_id') != config['context']['owner_slack_user_id']):
        raise RuntimeError('agent_updates_disabled')
    return p

def current_socket_healthy(home, status, started_at, revision):
    runtime = status.get('runtime', {})
    updated = runtime.get('updated_at')
    if (runtime.get('state') != 'running' or runtime.get('config_revision') != revision
            or not isinstance(updated, int) or updated < started_at):
        return False
    # The socket records hello/disconnect in the audit store. runtime.json's
    # connections array is a placeholder and does not describe socket health.
    # Match the management connection check: the latest audit must belong to
    # this process/configuration, not a previous successful connection.
    with sqlite3.connect('file:'+str(home/'state.sqlite3')+'?mode=ro', uri=True, timeout=5) as db:
        observed = db.execute("SELECT time,data FROM audit WHERE kind='connection' ORDER BY seq DESC LIMIT 1").fetchone()
    if not observed or observed[0] < updated:
        return False
    return json.loads(observed[1]).get('state') == 'connected'

class Controller:
    def __init__(self, home, identifier):
        self.home = home.resolve(); self.identifier = str(uuid.UUID(identifier))
        self.root = self.home/'updates'; self.folder = self.root/'plans'/self.identifier
        self.file = self.folder/'plan.json'; self.job = read(self.file)
        if self.job['id'] != self.identifier: raise RuntimeError('invalid_plan')
        self.config = read(self.home/'adapter.json')
        self.unit = service(self.home, self.config)
        write(self.root/'registered-service.json',{'adapter_id':self.config['id'],'service':self.unit})
        self.binary = self.home/'bin/acp-slack-adapter'
    def save(self, state=None, **values):
        if state: self.job['state'] = state
        self.job.update(values, updated_at=int(time.time()*1000)); write(self.file, self.job)
    def require_policy(self):
        value=policy(self.home, read(self.home/'adapter.json'), self.job.get('actor_kind') == 'agent')
        if value['revision']!=self.job['policy_revision']:raise RuntimeError('update_policy_changed')
        if self.job.get('actor_kind')=='agent' and value['owner_slack_user_id']!=self.job['authorized_owner']:raise RuntimeError('update_owner_changed')
        return value
    def plan(self):
        self.require_policy(); self.save('planning')
        head = gh('repos/'+REPO+'/commits/main')['sha']
        if len(head) != 40 or any(c not in '0123456789abcdef' for c in head): raise RuntimeError('invalid_source_revision')
        runs = gh('repos/'+REPO+'/actions/workflows/'+WORKFLOW+'/runs?branch=main&event=push&status=success&per_page=30')['workflow_runs']
        build = next((r for r in runs if r['head_sha']==head and r['event']=='push' and r['head_branch']=='main' and r['conclusion']=='success' and r['repository']['full_name']==REPO), None)
        if not build: raise RuntimeError('current_main_has_no_successful_build')
        arch = {'aarch64':'ARM64', 'x86_64':'X64'}.get(platform.machine())
        if not arch: raise RuntimeError('unsupported_architecture')
        name = 'acp-slack-adapter-Linux-'+arch+'-'+head
        artifacts = gh('repos/'+REPO+'/actions/runs/'+str(build['id'])+'/artifacts?per_page=100')['artifacts']
        artifact = next((a for a in artifacts if a['name']==name and not a['expired']), None)
        if not artifact or artifact.get('size_in_bytes',MAX_ARCHIVE+1)>MAX_ARCHIVE: raise RuntimeError('verified_artifact_unavailable')
        expected = artifact.get('digest', '')
        if not expected.startswith('sha256:') or len(expected)!=71: raise RuntimeError('artifact_digest_unavailable')
        archive = self.folder/'artifact.zip'
        # Artifact ID is obtained from GitHub, never from the caller. gh follows
        # GitHub's signed artifact redirect; credentials are not placed in argv.
        with archive.open('wb') as output:
            proc = subprocess.Popen(['/usr/bin/gh','api','--hostname','github.com','repos/'+REPO+'/actions/artifacts/'+str(int(artifact['id']))+'/zip'],stdin=subprocess.DEVNULL,stdout=output,stderr=subprocess.DEVNULL)
            try:
                deadline=time.monotonic()+120
                while proc.poll() is None:
                    if archive.stat().st_size>MAX_ARCHIVE or time.monotonic()>deadline:
                        proc.kill(); raise RuntimeError('artifact_download_limit')
                    time.sleep(.1)
                if proc.returncode: raise RuntimeError('github_artifact_access_unavailable')
            finally:
                if proc.poll() is None: proc.kill()
                proc.wait()
        if 'sha256:'+digest(archive)!=expected: raise RuntimeError('artifact_digest_mismatch')
        candidate=self.folder/'candidate'
        with zipfile.ZipFile(archive) as zipped:
            entries=zipped.infolist()
            if len(entries)!=1 or entries[0].filename!='acp-slack-adapter' or entries[0].file_size>MAX_ARCHIVE or entries[0].is_dir(): raise RuntimeError('invalid_artifact_layout')
            with zipped.open(entries[0]) as source,candidate.open('wb') as target: shutil.copyfileobj(source,target)
        candidate.chmod(0o700)
        version=run(str(candidate),'--version',timeout=15).decode().strip()
        if not version.startswith('acp-slack-adapter '): raise RuntimeError('invalid_candidate')
        self.require_policy()
        self.save('ready',repository=REPO,source_commit=head,workflow=WORKFLOW,run_id=build['id'],run_url=build['html_url'],artifact_id=artifact['id'],artifact_digest=expected,binary_sha256=digest(candidate),installed_sha256=digest(self.binary),version=version,up_to_date=digest(candidate)==digest(self.binary))
    def gate(self, critical=False):
        write(self.root/'drain.json',{'plan_id':self.identifier,'expires_at':int(time.time()*1000)+(120000 if critical else 15000)})
    def pending_counts(self):
        with sqlite3.connect('file:'+str(self.home/'state.sqlite3')+'?mode=ro',uri=True,timeout=5) as db:
            values=db.execute("SELECT (SELECT COUNT(*) FROM jobs WHERE status='running'),(SELECT COUNT(*) FROM outbox WHERE status IN ('ready','sending')),(SELECT COUNT(*) FROM outbox WHERE status='uncertain')").fetchone()
            return dict(zip(('running_turns','pending_replies','uncertain_deliveries'),values))
    def pending(self):return sum(self.pending_counts().values())
    def control(self, action): run('/usr/bin/systemctl','--user',action,self.unit,timeout=45)
    def health(self, expected):
        deadline=time.monotonic()+45
        while time.monotonic()<deadline:
            self.gate()
            try:
                if digest(self.binary)!=expected: raise RuntimeError('binary_changed')
                endpoints=read(self.home/'endpoints.json');ctx=ssl.create_default_context(cafile=str(self.home/'tls/root-ca.crt'))
                # Fixed installation's authenticated loopback endpoint, no caller URL.
                port=read(self.home/'adapter.json')['server']['port'];base='https://127.0.0.1:'+str(port)
                if ':' in read(self.home/'adapter.json')['server']['bind']: base='https://[::1]:'+str(port)
                token=(self.home/'secrets/control-token').read_text().strip()
                request=urllib.request.Request(base+'/api/v1/status',headers={'Authorization':'Bearer '+token})
                with urllib.request.urlopen(request,context=ctx,timeout=3) as response: status=json.load(response)
                if status['id']!=self.config['id'] or endpoints['pid']!=int(run('/usr/bin/systemctl','--user','show',self.unit,'-p','MainPID','--value')): raise RuntimeError('wrong_service')
                cfg=read(self.home/'adapter.json')
                if cfg['enabled'] and not current_socket_healthy(self.home,status,endpoints['started_at'],cfg['revision']): raise RuntimeError('connection_not_ready')
                return
            except Exception: time.sleep(.5)
        raise RuntimeError('health_check_failed')
    def replace(self, source):
        temporary=self.binary.with_suffix('.update-next');shutil.copyfile(source,temporary);temporary.chmod(0o755)
        with temporary.open('rb') as f: os.fsync(f.fileno())
        temporary.replace(self.binary)
        fd=os.open(self.binary.parent,os.O_RDONLY)
        try:os.fsync(fd)
        finally:os.close(fd)
    def snapshot(self):
        backup=self.folder/'backup';backup.mkdir(mode=0o700,exist_ok=True)
        shutil.copyfile(self.binary,backup/'binary');(backup/'binary').chmod(0o700)
        for name in ['adapter.json','release.json']:
            if (self.home/name).exists():shutil.copyfile(self.home/name,backup/name)
        # Backups are for operator recovery, never automatically restored over
        # newly received messages or credentials. Normal rollback changes binary only.
        with sqlite3.connect(self.home/'state.sqlite3') as source,sqlite3.connect(backup/'state.sqlite3') as target:source.backup(target)
        protected={}
        for base in [self.home/'secrets',self.home/'tls']:
            for file in base.rglob('*'):
                if file.is_file():protected[str(file.relative_to(self.home))]=digest(file)
        write(backup/'protected-hashes.json',protected)
        return backup
    def restore_metadata(self, backup):
        if (backup/'release.json').exists():write(self.home/'release.json',read(backup/'release.json'))
        else:(self.home/'release.json').unlink(missing_ok=True)
    def record_installation(self, backup):
        metadata=read(backup/'release.json') if (backup/'release.json').exists() else {}
        metadata.update(repository=REPO,commit=self.job['source_commit'],sha256=self.job['binary_sha256'],updated_at=int(time.time()*1000),update_plan=self.identifier,build={key:self.job.get(key) for key in ['workflow','run_id','run_url','artifact_id','artifact_digest']})
        if self.job.get('version'):metadata['version']=self.job['version'].split()[-1]
        write(self.home/'release.json',metadata)
    def finish_rollback(self, backup, reason):
        self.save('rolling_back',reason=reason);self.gate(critical=True);self.control('stop')
        if digest(backup/'binary')!=self.job['previous_sha256']:raise RuntimeError('rollback_digest_mismatch')
        self.replace(backup/'binary');self.restore_metadata(backup);self.control('start');self.health(self.job['previous_sha256'])
        self.save('rolled_back',reason=reason)
    def apply(self):
        candidate=self.folder/'candidate';backup=self.folder/'backup'
        # A helper killed after stopping/swapping resumes by restoring the last
        # known-good binary. No guessed continuation or second model turn.
        if self.job['state'] in {'stopping','swapping','verifying','rolling_back'}:
            self.finish_rollback(backup,'interrupted_update_recovered');return
        self.require_policy()
        rollback=self.job.get('action')=='rollback'
        if rollback:
            if digest(self.binary)!=self.job['binary_sha256']:raise RuntimeError('installed_binary_changed')
            target=backup/'binary';expected=self.job['previous_sha256']
            if digest(target)!=expected:raise RuntimeError('rollback_digest_mismatch')
        else:
            if digest(candidate)!=self.job['binary_sha256']:raise RuntimeError('candidate_digest_mismatch')
            if digest(self.binary)!=self.job['installed_sha256']:raise RuntimeError('installed_binary_changed')
            target=candidate;expected=self.job['binary_sha256']
        self.save('draining');deadline=time.monotonic()+1800
        config_hash=digest(self.home/'adapter.json')
        while True:
            self.require_policy()
            if time.monotonic()>deadline:raise RuntimeError('work_did_not_drain')
            waiting=self.pending_counts()
            if self.job.get('waiting_for')!=waiting:self.save(waiting_for=waiting)
            if waiting['uncertain_deliveries']:raise RuntimeError('uncertain_delivery_requires_reconciliation')
            self.gate()
            if not sum(waiting.values()):break
            time.sleep(1)
        # Hold the same cross-process lock as job claiming across the final
        # check and stop so a worker cannot start between idle and cutover.
        with (self.root/'admission.lock').open('a+b') as admission:
            fcntl.flock(admission,fcntl.LOCK_EX)
            if self.pending():raise RuntimeError('new_work_arrived')
            if digest(self.home/'adapter.json')!=config_hash:raise RuntimeError('configuration_changed')
            if digest(self.binary)!=(self.job['binary_sha256'] if rollback else self.job['installed_sha256']):raise RuntimeError('installed_binary_changed')
            if digest(target)!=expected:raise RuntimeError('candidate_digest_mismatch')
            self.require_policy()
            if not rollback:
                backup=self.snapshot();self.save(previous_sha256=digest(backup/'binary'))
            self.save('stopping');self.gate(critical=True)
            try:self.control('stop')
            except Exception:
                self.control('start');raise
        try:
            self.gate(critical=True);self.save('swapping');self.replace(target);self.save('verifying');self.control('start');self.health(expected)
            protected=read(backup/'protected-hashes.json')
            if any(digest(self.home/path)!=value for path,value in protected.items()):raise RuntimeError('protected_configuration_changed')
            if digest(self.home/'adapter.json')!=config_hash:raise RuntimeError('configuration_changed_during_update')
            if rollback:self.restore_metadata(backup)
            else:self.record_installation(backup)
            self.save('rolled_back' if rollback else 'succeeded',completed_at=int(time.time()*1000))
        except Exception:
            if rollback:
                self.control('stop');self.replace(candidate);self.control('start');self.health(self.job['binary_sha256'])
                self.save('succeeded',reason='rollback_failed_current_restored');return
            self.finish_rollback(backup,'candidate_failed_health')
    def execute(self):
        try:
            if self.job['action']=='plan':self.plan()
            else:self.apply()
        except Exception as error:
            # Only fixed machine-readable causes enter the UI, not stderr,
            # tokens, transcript contents, or upstream response bodies.
            reason=str(error)
            if not reason or len(reason)>80 or any(c not in 'abcdefghijklmnopqrstuvwxyz_0123456789' for c in reason):reason='update_operation_failed'
            critical=self.job['state'] in {'stopping','swapping','verifying','rolling_back'}
            deferred=reason in {'work_did_not_drain','uncertain_delivery_requires_reconciliation'}
            self.save('recovery_required' if critical else ('deferred' if deferred else 'failed'),reason=reason)
        finally:
            gate=self.root/'drain.json'
            if self.job['state']!='recovery_required' and gate.exists() and read(gate).get('plan_id')==self.identifier:gate.unlink()

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--home',type=Path,required=True);parser.add_argument('--id',required=True);args=parser.parse_args()
    os.umask(0o077);root=args.home.resolve()/'updates';root.mkdir(mode=0o700,exist_ok=True)
    with (root/'worker.lock').open('a+b') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        file=root/'plans'/str(uuid.UUID(args.id))/'plan.json'
        if read(file)['state'] not in ACTIVE:return
        if read(root/'active.json')['id']!=str(uuid.UUID(args.id)):return
        try:Controller(args.home,args.id).execute()
        except Exception:
            file=root/'plans'/str(uuid.UUID(args.id))/'plan.json';job=read(file);critical=job['state'] in {'stopping','swapping','verifying','rolling_back'};job.update(state='recovery_required' if critical else 'failed',reason='installation_update_unavailable');write(file,job)
if __name__=='__main__':main()
