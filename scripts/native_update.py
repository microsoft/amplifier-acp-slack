"""Fixed native Slack release worker. Private stdin-free invocation: home + plan UUID.
No arbitrary commands, URLs, host operations, model calls, or state rollback.
"""
import contextlib, hashlib, hmac, http.client, io, json, os, platform, shutil, sqlite3, subprocess, sys, tarfile, time, urllib.request, uuid, zipfile
from pathlib import Path, PurePosixPath
REPO='microsoft/amplifier-acp-slack'
CRITICAL={'stopping','swapping','verifying','rolling_back','recovery_required'}
MAX_ARCHIVE=384*1024*1024

def read(path):return json.loads(path.read_text())
def digest(path):
    with path.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def sync_file(path):
    with path.open('r+b') as stream:os.fsync(stream.fileno())
def durable_replace(source,target):
    if os.name=='nt':
        import ctypes
        from ctypes import wintypes
        move=ctypes.WinDLL('kernel32',use_last_error=True).MoveFileExW
        move.argtypes=[wintypes.LPCWSTR,wintypes.LPCWSTR,wintypes.DWORD];move.restype=wintypes.BOOL
        if not move(str(source),str(target),0x1|0x8):raise ctypes.WinError(ctypes.get_last_error())
    else:
        os.replace(source,target)
        for parent in {source.parent,target.parent}:
            fd=os.open(parent,os.O_RDONLY)
            try:os.fsync(fd)
            finally:os.close(fd)
def sync_tree(root):
    for path in root.rglob('*'):
        if path.is_file():sync_file(path)
    if os.name!='nt':
        for path in [*sorted((p for p in root.rglob('*') if p.is_dir()),reverse=True),root]:
            fd=os.open(path,os.O_RDONLY)
            try:os.fsync(fd)
            finally:os.close(fd)
def write(path,value):
    path.parent.mkdir(mode=0o700,parents=True,exist_ok=True);temp=path.parent/('.write-'+uuid.uuid4().hex)
    with open(temp,'x',opener=lambda p,f:os.open(p,f,0o600)) as f:json.dump(value,f,indent=2);f.flush();os.fsync(f.fileno())
    durable_replace(temp,path)
@contextlib.contextmanager
def locked(path,blocking=True):
    path.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
    with path.open('a+b') as f:
        if os.name=='nt':
            import msvcrt
            f.seek(0,2)
            if f.tell()==0:f.write(b' ');f.flush()
            f.seek(0);msvcrt.locking(f.fileno(),msvcrt.LK_LOCK if blocking else msvcrt.LK_NBLCK,1)
        else:
            import fcntl
            fcntl.flock(f,fcntl.LOCK_EX|(0 if blocking else fcntl.LOCK_NB))
        yield

def fetch(url,limit):
    request=urllib.request.Request(url,headers={'User-Agent':'Constellation-Slack-Updater','Accept':'application/vnd.github+json'})
    with urllib.request.urlopen(request,timeout=90) as response:data=response.read(limit+1)
    if len(data)>limit:raise RuntimeError('release_download_limit')
    return data

def native_target():
    family={'Linux':'unknown-linux-gnu','Darwin':'apple-darwin','Windows':'pc-windows-msvc'}[platform.system()]
    arch={'x86_64':'x86_64','AMD64':'x86_64','arm64':'aarch64','ARM64':'aarch64','aarch64':'aarch64'}[platform.machine()]
    return arch+'-'+family

def payload(archive,destination):
    total=0; names=set()
    def output(name,data,mode):
        nonlocal total
        path=PurePosixPath(name)
        if not path.parts or path.is_absolute() or '..' in path.parts or '\\' in name or ':' in name or name in names:raise RuntimeError('invalid_release_layout')
        names.add(name);total+=len(data)
        if total>1024*1024*1024:raise RuntimeError('release_expansion_limit')
        target=destination/name;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(data);target.chmod(mode&0o777)
    if archive.name.endswith('.zip'):
        with zipfile.ZipFile(archive) as source:
            for item in source.infolist():
                if item.is_dir():continue
                mode=item.external_attr>>16
                if mode&0o170000 not in (0,0o100000) or item.file_size>256*1024*1024:raise RuntimeError('invalid_release_layout')
                output(item.filename,source.read(item),mode or 0o644)
    else:
        with tarfile.open(archive) as source:
            for item in source:
                if item.isdir():continue
                if not item.isfile() or item.size>256*1024*1024:raise RuntimeError('invalid_release_layout')
                output(item.name,source.extractfile(item).read(),item.mode)

def validate_runtime(resources):
    manifest=read(resources/'native-bundle.json')
    if manifest.get('component')!='acp-slack-adapter' or manifest.get('target')!=native_target():raise RuntimeError('wrong_release_platform')
    files=manifest.get('python_files',{})
    if not 1<=len(files)<=20000:raise RuntimeError('invalid_runtime_inventory')
    for name,expected in files.items():
        p=PurePosixPath(name)
        if p.is_absolute() or '..' in p.parts or '\\' in name or ':' in name:raise RuntimeError('invalid_runtime_inventory')
        item=resources/'runtime/python'/name
        if not item.is_file() or item.is_symlink() or digest(item)!=expected:raise RuntimeError('private_runtime_mismatch')
    return manifest

class Worker:
    def __init__(self,home,identifier):
        self.home=home.resolve();self.id=str(uuid.UUID(identifier));self.root=self.home/'native-updates';self.folder=self.root/self.id;self.file=self.folder/'plan.json';self.job=read(self.file);self.config=read(self.home/'adapter.json');self.binary=self.home/'bin'/('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter')
        self.guard_ownership()
        if self.job['id']!=self.id or read(self.root/'active.json')['id']!=self.id:raise RuntimeError('invalid_plan')
    def guard_ownership(self):
        cfg=read(self.home/'adapter.json');marker=read(self.home/'native-installation.json')
        if cfg['server'].get('auth_mode')!='local_password' or cfg['server'].get('web_transport')!='local_http' or marker.get('id')!=cfg['id'] or marker.get('update_owner')!='installation':raise RuntimeError('native_owner_required')
        if (self.home/'saved-connections.json').exists() or (self.home/'updates/registered-service.json').exists():raise RuntimeError('externally_managed_update')
    def save(self,state=None,**values):
        if state:self.job['state']=state
        self.job.update(values,updated_at=int(time.time()*1000));write(self.file,self.job)
    def api(self,path,body=None):
        config=read(self.home/'adapter.json');origin=config['server']['public_url'];port=config['server']['port']
        if origin!='http://127.0.0.1:'+str(port):raise RuntimeError('invalid_local_origin')
        token=(self.home/'secrets/control-token').read_text().strip();nonce=os.urandom(32).hex();connection=http.client.HTTPConnection('127.0.0.1',port,timeout=6)
        try:
            connection.request('GET','/api/v1/local-proof?nonce='+nonce);response=connection.getresponse();proof=json.loads(response.read(4097))
            expected=hmac.new(token.encode(),f"acp-slack-adapter.local-proof/1\n{config['id']}\n{origin}\n{nonce}".encode(),hashlib.sha256).hexdigest()
            if response.status!=200 or proof.get('schema')!=1 or proof.get('id')!=config['id'] or proof.get('origin')!=origin or proof.get('nonce')!=nonce or not hmac.compare_digest(proof.get('proof',''),expected):raise RuntimeError('local_listener_mismatch')
            # http.client would reconnect automatically after Connection: close; forbid that.
            if connection.sock is None or response.will_close:raise RuntimeError('local_proof_connection_closed')
            connection.request('POST' if body is not None else 'GET','/api/v1/'+path,body=None if body is None else json.dumps(body),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
            response=connection.getresponse();value=json.loads(response.read(2*1024*1024+1))
            if response.status!=200:raise RuntimeError('local_operation_refused')
            return value
        finally:connection.close()
    def gate(self):write(self.home/'updates/drain.json',{'plan_id':self.id,'expires_at':int(time.time()*1000)+120000})
    def check(self):
        self.save('checking')
        release=json.loads(fetch('https://api.github.com/repos/'+REPO+'/releases/latest',2*1024*1024))
        if release.get('draft') or release.get('prerelease') or not release.get('tag_name','').startswith('v'):raise RuntimeError('published_release_unavailable')
        version=release['tag_name'][1:]
        if len(version)>64 or any(c not in '0123456789.' for c in version):raise RuntimeError('invalid_release_version')
        filename='acp-slack-adapter-'+version+'-'+native_target()+('.zip' if os.name=='nt' or sys.platform=='darwin' else '.tar.gz')
        assets={a['name']:a for a in release['assets']}
        def asset(name,maximum):
            record=assets.get(name,{})
            if not record.get('browser_download_url','').startswith('https://github.com/'+REPO+'/releases/download/'+release['tag_name']+'/') or record.get('size',maximum+1)>maximum:raise RuntimeError('release_asset_missing')
            data=fetch(record['browser_download_url'],maximum)
            if record.get('digest')!='sha256:'+hashlib.sha256(data).hexdigest():raise RuntimeError('release_asset_digest_mismatch')
            return data
        metadata=json.loads(asset(filename+'.json',128*1024));archive=self.folder/filename;archive.write_bytes(asset(filename,MAX_ARCHIVE))
        if metadata.get('target')!=native_target() or metadata.get('version')!=version or metadata.get('sha256')!=digest(archive):raise RuntimeError('release_metadata_mismatch')
        candidate=self.folder/'candidate'
        if candidate.exists():raise RuntimeError('candidate_already_exists')
        candidate.mkdir();payload(archive,candidate)
        entry=PurePosixPath(metadata['entry'])
        if entry.is_absolute() or '..' in entry.parts:raise RuntimeError('invalid_release_entry')
        binary=candidate/str(entry)
        if not binary.is_file() or digest(binary)!=metadata['binary_sha256']:raise RuntimeError('release_binary_mismatch')
        resources=binary.parent.parent/'Resources' if sys.platform=='darwin' else binary.parent
        validate_runtime(resources)
        result=subprocess.run([str(binary),'--version'],capture_output=True,text=True,timeout=15)
        if result.returncode or result.stdout.strip()!='acp-slack-adapter '+version:raise RuntimeError('candidate_version_mismatch')
        self.save('ready',version=version,tag=release['tag_name'],release_id=release['id'],release_url=release['html_url'],archive_sha256=digest(archive),entry=metadata['entry'],binary_sha256=metadata['binary_sha256'],installed_sha256=digest(self.binary),config_sha256=digest(self.home/'adapter.json'),up_to_date=digest(self.binary)==metadata['binary_sha256'])
    def stop(self):
        handle=None
        if os.name=='nt':
            # Hold the actual authenticated service process before requesting exit.
            # Its lock can release before Windows unloads its executable/DLLs.
            import ctypes as c
            from ctypes import wintypes as w
            pid=self.api('status').get('addresses',{}).get('pid')
            if not isinstance(pid,int) or not 0<pid<2**32:raise RuntimeError('service_process_unknown')
            kernel=c.WinDLL('kernel32',use_last_error=True)
            kernel.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];kernel.OpenProcess.restype=w.HANDLE
            kernel.WaitForSingleObject.argtypes=[w.HANDLE,w.DWORD];kernel.WaitForSingleObject.restype=w.DWORD
            kernel.CloseHandle.argtypes=[w.HANDLE]
            handle=kernel.OpenProcess(0x100000,False,pid)
            if not handle:raise RuntimeError('service_process_unknown')
        try:
            self.api('desktop/update-stop',{'plan_id':self.id})
            if handle and kernel.WaitForSingleObject(handle,20000)!=0:raise RuntimeError('service_did_not_exit')
            deadline=time.monotonic()+20
            while time.monotonic()<deadline:
                try:
                    with locked(self.home/'service.lock',False):return
                except OSError:time.sleep(.1)
            raise RuntimeError('service_did_not_stop')
        finally:
            if handle:kernel.CloseHandle(handle)
    def start(self):
        mode={'verifying':'candidate','rolling_back':'rollback'}.get(self.job['state'])
        if mode is None:raise RuntimeError('replacement_launch_phase_invalid')
        nonce=str(uuid.uuid4());expected=self.job['binary_sha256' if mode=='candidate' else 'previous_sha256']
        if digest(self.binary)!=expected:raise RuntimeError('replacement_launch_bytes_changed')
        self.save(launch={'nonce':nonce,'mode':mode,'binary_sha256':expected})
        env={**os.environ,'ACP_SLACK_UPDATE_PLAN':self.id,'ACP_SLACK_UPDATE_LAUNCH':nonce}
        kwargs={'stdin':subprocess.DEVNULL,'stdout':subprocess.DEVNULL,'stderr':subprocess.DEVNULL,'env':env}
        if os.name=='nt':kwargs['creationflags']=subprocess.CREATE_NO_WINDOW|subprocess.CREATE_NEW_PROCESS_GROUP
        else:kwargs['start_new_session']=True
        self.started_process=subprocess.Popen([str(self.binary),'--home',str(self.home),'serve'],**kwargs)
        self.started_pid=self.started_process.pid
    def health(self,expected):
        expected_version=subprocess.check_output([str(self.binary),'--version'],text=True,timeout=15).strip().split()[-1]
        deadline=time.monotonic()+60
        while time.monotonic()<deadline:
            self.gate()
            try:
                result=self.api('status');runtime=result.get('runtime',{});socket=result.get('state',{}).get('slack_connection',{})
                if digest(self.binary)==expected and result['id']==self.config['id'] and result.get('version')==expected_version and result.get('addresses',{}).get('pid')==self.started_pid and (not self.config['enabled'] or runtime.get('state')=='running' and (socket if isinstance(socket,str) else socket.get('state'))=='connected'):return
            except Exception:pass
            time.sleep(.3)
        raise RuntimeError('candidate_health_failed')
    def protected(self):
        paths=[self.home/'adapter.json',self.home/'native-installation.json']
        paths+=list((self.home/'secrets').rglob('*'))
        return {str(p.relative_to(self.home)):digest(p) for p in paths if p.is_file() and p.name!='owner.lock'}
    def replace(self,binary,runtime):
        next_binary=self.binary.with_suffix('.native-next');shutil.copyfile(binary,next_binary);next_binary.chmod(0o755);sync_file(next_binary);durable_replace(next_binary,self.binary)
        runtime_next=self.home/'runtime.native-next'
        if runtime_next.exists():shutil.rmtree(runtime_next)
        shutil.copytree(runtime,runtime_next);sync_tree(runtime_next)
        runtime_old=self.home/'runtime.native-old'
        if runtime_old.exists():shutil.rmtree(runtime_old)
        if (self.home/'runtime').exists():durable_replace(self.home/'runtime',runtime_old)
        durable_replace(runtime_next,self.home/'runtime')
    def rollback(self,reason):
        # Supersede a candidate whose Popen may have completed before its worker
        # died. The service validates this journal under service.lock before state.
        self.save('rolling_back',reason=reason,launch=None);self.gate()
        try:self.stop()
        except (OSError,RuntimeError):
            # Our candidate may own the lock before its API is listening. Reconcile
            # that same known child briefly; never terminate or replace an unknown process.
            child=getattr(self,'started_process',None)
            deadline=time.monotonic()+20
            while child is not None and child.poll() is None and time.monotonic()<deadline:
                try:self.stop()
                except (OSError,RuntimeError):pass
                if child.poll() is None:time.sleep(.1)
            if child is not None and child.poll() is None:
                raise RuntimeError('candidate_stop_unresolved')
            # A free lock before the known child starts is not proof it stopped.
            with locked(self.home/'service.lock',False):pass
        backup=self.folder/'backup'
        if digest(backup/'binary')!=self.job['previous_sha256']:raise RuntimeError('rollback_digest_mismatch')
        with locked(self.home/'service.lock',False):
            self.replace(backup/'binary',backup/'runtime')
        self.start();self.health(self.job['previous_sha256'])
        if (backup/'release.json').exists():write(self.home/'release.json',read(backup/'release.json'))
        self.save('rolled_back',reason=reason)
    def apply(self):
        self.guard_ownership()
        backup=self.folder/'backup'
        if self.job.get('action')=='rollback':
            if self.job.get('critical_recovery'):
                self.rollback('interrupted_update_recovered');return
            self.save('draining');self.gate()
            if not self.api('desktop/update-readiness')['idle']:
                self.save('deferred',reason='work_or_delivery_not_settled');return
            with locked(self.home/'updates/admission.lock',False):
                self.guard_ownership()
                if not self.api('desktop/update-readiness')['idle']:
                    self.save('deferred',reason='work_or_delivery_not_settled');return
                self.rollback('owner_requested')
            return
        if self.job.get('up_to_date') and digest(self.binary)==self.job.get('binary_sha256'):
            self.save('succeeded',changed=False,completed_at=int(time.time()*1000));return
        if self.job['state'] in CRITICAL:self.rollback('interrupted_update_recovered');return
        if digest(self.binary)!=self.job['installed_sha256'] or digest(self.home/'adapter.json')!=self.job['config_sha256']:raise RuntimeError('installation_changed')
        candidate=self.folder/'candidate'/self.job['entry'];resources=candidate.parent.parent/'Resources' if sys.platform=='darwin' else candidate.parent
        if digest(candidate)!=self.job['binary_sha256']:raise RuntimeError('candidate_changed')
        manifest=validate_runtime(resources)
        self.save('draining');self.gate()
        if not self.api('desktop/update-readiness')['idle']:self.save('deferred',reason='work_or_delivery_not_settled');return
        with locked(self.home/'updates/admission.lock',False):
            self.guard_ownership()
            if not self.api('desktop/update-readiness')['idle']:self.save('deferred',reason='work_or_delivery_not_settled');return
            backup.mkdir(mode=0o700,exist_ok=True);shutil.copyfile(self.binary,backup/'binary');(backup/'binary').chmod(0o755);shutil.copytree(self.home/'runtime',backup/'runtime',dirs_exist_ok=True)
            if (self.home/'release.json').exists():shutil.copyfile(self.home/'release.json',backup/'release.json')
            sync_tree(backup)
            self.save(previous_sha256=digest(backup/'binary'))
            self.save('stopping');self.stop()
            # Keep this guard through every filesystem replacement. Reopening the
            # app cannot start an old process between stop and atomic swap.
            with locked(self.home/'service.lock',False):
                self.guard_ownership()
                if digest(self.home/'adapter.json')!=self.job['config_sha256']:raise RuntimeError('configuration_changed')
                with contextlib.closing(sqlite3.connect(self.home/'state.sqlite3')) as db,contextlib.closing(sqlite3.connect(backup/'state.sqlite3')) as out:db.backup(out)
                protected=self.protected();write(backup/'protected.json',protected)
                staged_runtime=self.folder/'runtime-candidate';shutil.copytree(resources/'runtime',staged_runtime,dirs_exist_ok=True);write(staged_runtime/'native-bundle.json',manifest)
                self.save('swapping');self.replace(candidate,staged_runtime);self.save('verifying')
            self.start();self.health(self.job['binary_sha256'])
            if self.protected()!=protected:raise RuntimeError('protected_state_changed')
            release=read(backup/'release.json') if (backup/'release.json').exists() else {};release.update(version=self.job['version'],sha256=self.job['binary_sha256'],release_url=self.job['release_url'],updated_at=int(time.time()*1000));write(self.home/'release.json',release)
            self.save('succeeded',completed_at=int(time.time()*1000))
    def execute(self):
        try:
            if self.job['action']=='check':self.check()
            else:self.apply()
        except Exception:
            if self.job.get('state') in CRITICAL and (self.folder/'backup/binary').exists():
                try:self.rollback('candidate_failed_verification')
                except Exception:self.save('recovery_required',reason='automatic_rollback_incomplete')
            else:self.save('failed',reason='update_could_not_complete')
        finally:
            gate=self.home/'updates/drain.json'
            if self.job.get('state') not in CRITICAL and gate.exists() and read(gate).get('plan_id')==self.id:gate.unlink()

def main():
    os.umask(0o077);home=Path(sys.argv[1]);identifier=str(uuid.UUID(sys.argv[2]));root=home/'native-updates'
    with locked(root/'worker.lock',True):
        # A delayed child from an acknowledged interrupted request must not run it again.
        if read(root/identifier/'plan.json').get('state') not in {'queued','checking','draining',*CRITICAL}:return
        try:Worker(home,identifier).execute()
        except Exception:
            file=root/identifier/'plan.json';job=read(file);job.update(state='recovery_required' if job.get('state') in CRITICAL else 'failed',reason='installation_update_unavailable');write(file,job)
if __name__=='__main__':main()
