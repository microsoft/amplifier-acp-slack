#!/usr/bin/env python3
"""Actual disposable connector stop/swap/restart/rollback; no Slack or model activity."""
import argparse,contextlib,hashlib,importlib.util,json,os,shutil,sqlite3,subprocess,sys,tempfile,time,traceback,unittest,uuid
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
@contextlib.contextmanager
def database(path):
 connection=sqlite3.connect(path)
 try:
  with connection:yield connection
 finally:connection.close()
for name,path in [('launcher',ROOT/'tests/native_launch.py'),('worker',ROOT/'scripts/native_update.py')]:
 spec=importlib.util.spec_from_file_location(name,path);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);globals()[name]=module

def unix_generation(pid):
 if sys.platform=='linux':
  try:raw=Path('/proc')/str(pid)/'stat';text=raw.read_text()
  except FileNotFoundError:return None
  parts=text.rsplit(')',1)[1].split()
  if parts[0] in {'Z','X','x'}:return None
  return {'pid':pid,'start_ticks':parts[19],'boot_id':Path('/proc/sys/kernel/random/boot_id').read_text().strip()}
 if sys.platform=='darwin':
  import ctypes as c
  class Info(c.Structure):
   _fields_=[(name,c.c_uint32) for name in ('flags','status','xstatus','pid','ppid','uid','gid','ruid','rgid','svuid','svgid','rfu')]+[('comm',c.c_char*16),('name',c.c_char*32)]+[(name,c.c_uint32) for name in ('nfiles','pgid','pjobc','e_tdev','e_tpgid')]+[('nice',c.c_int32),('seconds',c.c_uint64),('microseconds',c.c_uint64)]
  lib=c.CDLL('/usr/lib/libproc.dylib',use_errno=True);lib.proc_pidinfo.argtypes=[c.c_int,c.c_int,c.c_uint64,c.c_void_p,c.c_int];lib.proc_pidinfo.restype=c.c_int
  info=Info();n=lib.proc_pidinfo(pid,3,0,c.byref(info),c.sizeof(info))
  if n==0 and c.get_errno()==3:return None
  assert n==c.sizeof(info) and info.pid==pid,'Process generation is unavailable'
  if info.status==5:return None
  return {'pid':pid,'start_seconds':str(info.seconds),'start_microseconds':str(info.microseconds)}
 raise AssertionError('Unsupported process observer')

def unix_executable(pid):
 if sys.platform=='linux':return Path('/proc')/str(pid)/'exe'
 if sys.platform=='darwin':
  import ctypes as c
  lib=c.CDLL('/usr/lib/libproc.dylib',use_errno=True);lib.proc_pidpath.argtypes=[c.c_int,c.c_void_p,c.c_uint32];lib.proc_pidpath.restype=c.c_int
  name=c.create_string_buffer(4096);size=lib.proc_pidpath(pid,name,len(name));assert size>0,'Worker executable unavailable'
  return Path(os.fsdecode(name.value))
 raise AssertionError('Unsupported process observer')

def receipt_observation(path,diagnostics):
 """Create-only entry files can be observed before their first flush completes."""
 try:path.stat()
 except FileNotFoundError:return None
 try:
  value=worker.read(path)
  assert isinstance(value,dict),'Completed receipt is not an object'
  return value
 except json.JSONDecodeError as error:
  tail=error.doc[error.pos:].strip()
  partial=(error.pos>=len(error.doc.rstrip()) or error.msg.startswith('Unterminated string')
           or tail in {'t','tr','tru','f','fa','fal','fals','n','nu','nul'})
  if not partial:raise
  if len(diagnostics)<16:diagnostics.append({'file':path.name,'kind':'incomplete_json','bytes':len(error.doc.encode('utf-8')),'offset':error.pos})
  return None

def validate_worker_receipt(value,plan,request,expected_state=None):
 keys={'schema','plan_id','request_id','pid'}|({'state'} if expected_state is not None else set())
 assert isinstance(value,dict) and set(value)==keys,'Completed receipt shape changed'
 assert type(value['schema']) is int and value['schema']==1
 assert value['plan_id']==plan and value['request_id']==request
 assert type(value['pid']) is int and value['pid']>0
 if expected_state is not None:assert value['state']==expected_state

def validate_cleanup_reply(value,plan,request,observation_only):
 # The authenticated API returns a public-safe proof projection, not private
 # native paths, raw receipts, or an invented full-generation observation.
 assert value=={'schema':'com.microsoft.amplifier.slack-native-update-cleanup/1',
  'plan_id':plan,'request_id':request,'cleanup':'complete',
  'observation_only':observation_only,
  'proof_basis':'matching_worker_receipts_exact_profile_inactive_empty_and_lifetime_lock_free',
  'full_generation_observed':False},'Worker cleanup is not conclusively proved'
 assert type(value['observation_only']) is bool and value['full_generation_observed'] is False

def update_inventory(home):
 root=home/'native-updates'
 return {p.relative_to(root).as_posix():('directory' if p.is_dir() else worker.digest(p))
         for p in sorted(root.rglob('*'))}

def exact_path_identity(first,second):
 a,b=Path(first).resolve(strict=True),Path(second).resolve(strict=True)
 def key(path):
  value=str(path)
  if os.name=='nt':
   if value.startswith('\\\\?\\UNC\\'):value='\\\\'+value[8:]
   elif value.startswith('\\\\?\\'):value=value[4:]
   value=os.path.normcase(value)
  return value
 return key(a)==key(b) and os.path.samefile(a,b)

class ActualWorker:
 """Hold the OS process identity of this plan's actual packaged interpreter."""
 def __init__(self,folder,identifier,managed=False):
  self.folder=folder.resolve();self.identifier=identifier;self.handle=None;self.pid=None;self.settled=False
  self.managed=managed;self.generation=None
  self.executable=self.folder/'worker-runtime/python'/('pythonw.exe' if os.name=='nt' and managed else 'python.exe' if os.name=='nt' else 'bin/python3')
 def capture(self,pid=None):
  if os.name!='nt':
   if self.managed:
    assert type(pid) is int and pid>0;self.pid=pid;self.generation=unix_generation(pid)
    assert self.generation,'Managed worker departed before its generation was observed'
    assert exact_path_identity(unix_executable(pid),self.executable),'One-shot worker executable differs'
    assert unix_generation(pid)==self.generation,'Worker generation changed during executable observation'
   return
  import ctypes as c
  from ctypes import wintypes as w
  self.kernel=c.WinDLL('kernel32',use_last_error=True)
  self.kernel.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];self.kernel.OpenProcess.restype=w.HANDLE
  self.kernel.QueryFullProcessImageNameW.argtypes=[w.HANDLE,w.DWORD,w.LPWSTR,c.POINTER(w.DWORD)]
  self.kernel.WaitForSingleObject.argtypes=[w.HANDLE,w.DWORD];self.kernel.CloseHandle.argtypes=[w.HANDLE]
  # Inspect executable identities directly; console/CIM text may normalize the
  # runner's short Unicode temp path differently from the process image path.
  psapi=c.WinDLL('psapi',use_last_error=True)
  psapi.EnumProcesses.argtypes=[c.POINTER(w.DWORD),w.DWORD,c.POINTER(w.DWORD)]
  ids=(w.DWORD*8192)();used=w.DWORD()
  assert psapi.EnumProcesses(ids,c.sizeof(ids),c.byref(used))
  assert used.value<c.sizeof(ids),'Process enumeration exceeded fixture bound'
  selected=pid
  for candidate in ids[:used.value//c.sizeof(w.DWORD)]:
   if selected is not None and candidate!=selected:continue
   handle=self.kernel.OpenProcess(0x100000|0x1000,False,candidate)
   if not handle:continue
   buf=c.create_unicode_buffer(32768);size=w.DWORD(32768)
   matches=False
   try:
    if self.kernel.QueryFullProcessImageNameW(handle,0,buf,c.byref(size)):
     try:matches=exact_path_identity(buf.value,self.executable)
     except OSError:
      if selected is not None:raise
      matches=False
    if matches:
     assert self.handle is None,'Multiple processes use this exact plan interpreter'
     self.handle=handle;self.pid=candidate
     self.kernel.GetProcessTimes.argtypes=[w.HANDLE,c.POINTER(w.FILETIME),c.POINTER(w.FILETIME),c.POINTER(w.FILETIME),c.POINTER(w.FILETIME)];self.kernel.GetProcessTimes.restype=w.BOOL
     birth,exit,kernel,user=w.FILETIME(),w.FILETIME(),w.FILETIME(),w.FILETIME()
     assert self.kernel.GetProcessTimes(handle,c.byref(birth),c.byref(exit),c.byref(kernel),c.byref(user))
     self.generation={'pid':candidate,'creation_time_100ns':str((birth.dwHighDateTime<<32)|birth.dwLowDateTime)}
   finally:
    if not matches:self.kernel.CloseHandle(handle)
  assert self.handle,'Exact per-plan interpreter was not observed'
 def wait(self,deadline):
  if self.settled:return
  if self.handle:
   assert self.kernel.WaitForSingleObject(self.handle,max(0,int((deadline-time.monotonic())*1000)))==0,'Known update worker did not exit'
   self.kernel.CloseHandle(self.handle);self.handle=None;self.settled=True
  elif self.managed:
   assert self.generation is not None
   while time.monotonic()<deadline:
    if unix_generation(self.pid)!=self.generation:self.settled=True;return
    time.sleep(.1)
   raise AssertionError('Same managed worker generation remains alive')
  else:
   # The ordinary API reports this same worker lock. Unix replacement allows
   # concurrent reads; acquire it only to establish that its operation returned.
   while time.monotonic()<deadline:
    try:
     with worker.locked(self.folder.parent/'worker.lock',False):self.settled=True;return
    except (BlockingIOError,PermissionError):time.sleep(.1)
   raise AssertionError('Update worker still owns its operation')

def managed_roundtrip(package,result):
 """Actual API -> independent one-shot OS worker -> same-profile replacement.

 Local candidate bytes stand in for a selected release. GitHub selection remains
 separately tested. No synthetic manager, process/exit observation, or cleanup.
 """
 root=Path(tempfile.mkdtemp(prefix='Slack managed update café 子 ')).resolve();home=root/'installation';bundle=root/'download';bundle.mkdir()
 target=result.with_name(result.stem+'-managed.json') if result else root/'result.json'
 evidence={'schema':1,'passed':False,'home':str(home),'package_sha256':worker.digest(package),'events':[],'external_authorization':False,'model_calls':0,'action_retry':False}
 env=dict(os.environ)
 env['PATH']=str(Path(os.environ['SystemRoot'])/'System32') if os.name=='nt' else '/usr/bin:/bin:/usr/sbin:/sbin'
 for key in list(env):
  if key in {'PYTHONHOME','PYTHONPATH','NODE_OPTIONS'} or key.endswith(('_API_KEY','_TOKEN')):env.pop(key,None)
 def save():target.write_text(json.dumps(evidence,indent=2)+'\n',encoding='utf-8')
 def call(exe,action,*args,capture_timeout=65):
  row={'action':action,'args':list(args),'started_at':time.time()};evidence['events'].append(row);save()
  command=[str(exe),'--home',str(home),action,*args]
  proc=subprocess.Popen(command,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,creationflags=subprocess.CREATE_NO_WINDOW if os.name=='nt' else 0)
  row['caller_pid']=proc.pid
  try:out,err=proc.communicate(timeout=capture_timeout)
  except subprocess.TimeoutExpired:
   row.update(outcome='unknown',caller_exit=proc.poll());save();raise
  row.update(caller_exit=proc.returncode,elapsed=time.time()-row['started_at']);save()
  assert proc.returncode==0 and len(out)<=1024*1024 and len(err)<=8192,(action,proc.returncode,err[:2048])
  return json.loads(out)
 def ready(value):
  assert value['schema']=='com.microsoft.amplifier.slack-native-status/1' and value['state']=='ready'
  assert value['id']==identity and exact_path_identity(value['home'],home) and exact_path_identity(value['executable'],installed)
  assert value['request_current'] is True and value['profile_matches'] is True and value['launch_observed'] is True
  assert value['autostart'] is False and value['pid']==value['process_identity']['pid']
  assert value['service']==service and len(value['process_identity'])>=3
  return value['process_identity']
 def protected():
  return {str(p.relative_to(home)):worker.digest(p) for p in [home/'adapter.json',home/'native-installation.json',*sorted((home/'secrets').rglob('*'))] if p.is_file()}
 def transition(action,expected_state,before):
  request=str(uuid.uuid4());current=worker.Worker(home,plan_id);row={'phase':action,'request_id':request,'plan_id':plan_id};evidence['events'].append(row);save()
  try:current.api('desktop/updates',{'action':action,'request_id':request,'plan_id':plan_id});row['http_reply']='received'
  except (TimeoutError,ConnectionError):row['http_reply']='unknown'
  save();deadline=time.monotonic()+240;actual=ActualWorker(folder,plan_id,managed=True)
  entered_file=folder/('worker-entered-'+request+'.json');finished_file=folder/('worker-finished-'+request+'.json')
  row['receipt_observations']=[]
  while time.monotonic()<deadline:
   entered=receipt_observation(entered_file,row['receipt_observations'])
   if entered is not None:
    validate_worker_receipt(entered,plan_id,request)
    actual.capture(entered['pid']);row['worker_generation']=actual.generation;save();break
   time.sleep(.05)
  else:raise AssertionError('One-shot worker entry was not observed')
  assert (folder/('worker-'+request+'.py')).read_bytes()==(ROOT/'scripts/native_update.py').read_bytes()
  while time.monotonic()<deadline:
   finished=receipt_observation(finished_file,row['receipt_observations'])
   if finished is not None:
    validate_worker_receipt(finished,plan_id,request,expected_state)
    assert finished=={**entered,'state':expected_state};break
   observed=worker.read(folder/'plan.json')
   if observed.get('state') in {'failed','recovery_required','deferred'}:raise AssertionError(('Managed update did not settle',observed.get('state'),observed.get('reason')))
   time.sleep(.1)
  else:raise AssertionError('One-shot worker terminal receipt was not observed')
  actual.wait(deadline);assert actual.settled
  settled=worker.read(folder/'plan.json');assert settled['request_id']==request and settled['state']==expected_state
  control=settled['manager_stop']['control'];assert control['process_identity']==before and control['process_absent'] is True
  observed=call(installed,'native-status');after=ready(observed)
  assert after!=before and observed['request']['request_id']==settled['launch']['manager_request_id']
  assert after==settled['launch']['process_identity'];assert after['pid']!=actual.pid
  row.update(finished=finished,worker_exit_observed=True,old_generation=before,replacement_generation=after);save()
  def cleanup_api(action):
   body=root/('cleanup-'+action+'-'+request+'.json')
   worker.write(body,{'action':action,'plan_id':plan_id,'request_id':request})
   # The unchanged local API transport owns 90 seconds; the fixed native
   # cleanup still owns 60. This fixture only allows reply serialization.
   return call(installed,'api','POST','desktop/updates','--body-file',str(body),capture_timeout=95)
  retired=cleanup_api('retire_worker');validate_cleanup_reply(retired,plan_id,request,False)
  row['worker_retirement']=retired;save()
  preserved=update_inventory(home)
  inspected=cleanup_api('inspect_worker_retirement');validate_cleanup_reply(inspected,plan_id,request,True)
  assert update_inventory(home)==preserved,'Read-only cleanup inspection changed update records'
  duplicate=cleanup_api('retire_worker');validate_cleanup_reply(duplicate,plan_id,request,False)
  assert duplicate==retired and update_inventory(home)==preserved,'Same-request cleanup created another operation'
  row.update(worker_retirement_inspection=inspected,worker_retirement_duplicate=duplicate,
             update_records_unchanged_after_inspection_and_duplicate=True);save()
  time.sleep(.25)
  assert ready(call(installed,'native-status'))==after,'Replacement did not outlive worker exit/retirement'
  assert not (home/'updates/drain.json').exists()
  return after
 try:
  launcher.extract(package,bundle);binary=next(p for p in bundle.rglob('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter') if p.is_file())
  setup=call(binary,'native-setup');assert setup['created'] is True and setup['serving'] is False
  identity=setup['id'];installed=Path(setup['executable']);first=str(uuid.uuid4())
  initial=call(installed,'native-start','--request-id',first);service=initial['service'];generation=ready(initial)
  base=worker.read(home/'endpoints.json')['base_url'];cap=worker.read(home/'secrets/first-account')['token']
  assert launcher.request(base,'/api/v1/auth/first-account',{'setup_token':cap,'password':'Disposable managed update '+uuid.uuid4().hex,'start_at_signin':False})[0]==200
  baseline=protected();evidence['protected_before']=baseline;evidence['service']=service
  with database(home/'state.sqlite3') as db:db.execute("INSERT INTO records(collection,id,value,updated_at) VALUES('native-fixture','preserved','{\"value\":1}',1)")
  plan_id=str(uuid.uuid4());folder=home/'native-updates'/plan_id;folder.mkdir(parents=True)
  shutil.copytree(bundle,folder/'candidate');entry=binary.relative_to(bundle).as_posix()
  version=subprocess.check_output([str(binary),'--version'],text=True,encoding='utf-8',timeout=15,creationflags=subprocess.CREATE_NO_WINDOW if os.name=='nt' else 0).strip().split()[-1]
  job={'schema':1,'id':plan_id,'state':'ready','action':'check','entry':entry,'binary_sha256':worker.digest(binary),'installed_sha256':worker.digest(installed),'config_sha256':worker.digest(home/'adapter.json'),'version':version,'release_url':'https://example.invalid/disposable-fixture'}
  worker.write(folder/'plan.json',job);worker.write(home/'native-updates/active.json',{'id':plan_id})
  applied=transition('apply','succeeded',generation);assert protected()==baseline
  # Settings and work arriving after Apply must survive an explicit Restore.
  config=call(installed,'api','GET','config');config['label']='Changed after applied fixture'
  body=root/'config-request.json';worker.write(body,{'expected_revision':config['revision'],'config':config})
  call(installed,'api','PUT','config','--body-file',str(body));expected=protected()
  with database(home/'state.sqlite3') as db:db.execute("UPDATE records SET value='{\"value\":2}' WHERE collection='native-fixture' AND id='preserved'")
  restored=transition('rollback','rolled_back',applied);assert protected()==expected
  with database(home/'state.sqlite3') as db:assert db.execute("SELECT value FROM records WHERE collection='native-fixture' AND id='preserved'").fetchone()==('{"value":2}',)
  current=call(installed,'native-status');assert ready(current)==restored
  for action,state in [('stop','inactive'),('retire','retired')]:
   request=str(uuid.uuid4());value=call(installed,'native-'+action,'--request-id',request)
   assert value['state']==state and value['control_current'] is True
   control=value['control'];assert control['request_id']==request and control['action']==action and control['phase']==state
   assert control['start_request_id']==current['request']['request_id'] and control['process_identity']==restored and control['process_absent'] is True
  assert protected()==expected
  evidence.update(passed=True,protected_after=expected,cleanup='fixed app and both worker profiles checked retired; exact fixture root removal pending')
  save();shutil.rmtree(root);evidence['cleanup']='all exact owned profiles retired; disposable root removed';save()
 except BaseException as error:
  evidence.update(error_type=type(error).__name__,cleanup='not attempted after failure/unknown',retained_home=home.exists());save();raise
 return evidence

def run(package,result):
 checks=[];observations=[];actual_worker=None;worker_deadline=None
 with tempfile.TemporaryDirectory(prefix='Slack update café 子 ') as temp:
  root=Path(temp);bundle=root/'download';bundle.mkdir();launcher.extract(package,bundle)
  binary=next(p for p in bundle.rglob('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter') if p.is_file());home=root/'installation';log=open(root/'server.log','ab')
  proc=subprocess.Popen([str(binary),'--home',str(home),'--no-browser'],stdin=subprocess.DEVNULL,stdout=log,stderr=log)
  current=None
  try:
   for _ in range(400):
    try:
     base=json.loads((home/'endpoints.json').read_text())['base_url']
     if launcher.request(base,'/api/v1/auth/session')[0]==200:break
    except (OSError,ValueError):pass
    if proc.poll() is not None:raise AssertionError('Fixture connector exited')
    time.sleep(.1)
   else:raise AssertionError('Fixture connector did not start')
   config_bytes=(home/'adapter.json').read_bytes();config=json.loads(config_bytes);cap=worker.read(home/'secrets/first-account')['token']
   assert launcher.request(base,'/api/v1/auth/first-account',{'setup_token':cap,'password':'Disposable update password','start_at_signin':False})[0]==200
   secret_before=worker.digest(home/'secrets/local-password');identity_before=config['id'];private_before=worker.digest(home/'secrets/control-token')
   # A new queued source may arrive during installation. It must never be rewound.
   with database(home/'state.sqlite3') as db:db.execute("INSERT INTO records(collection,id,value,updated_at) VALUES('native-fixture','preserved','{\"value\":1}',1)")
   expected_value=[1]
   def input_arrived_after_backup():
    expected_value[0]+=1
    with database(home/'state.sqlite3') as db:db.execute("UPDATE records SET value=? WHERE collection='native-fixture' AND id='preserved'",(json.dumps({'value':expected_value[0]},separators=(',',':')),))
   original_binary=home/'bin'/binary.name
   for fail in [None,"health","start"]:
    identifier=str(uuid.uuid4());folder=home/'native-updates'/identifier;folder.mkdir(parents=True)
    shutil.copytree(bundle,folder/'candidate');entry=binary.relative_to(bundle).as_posix()
    job={'schema':1,'id':identifier,'state':'ready','action':'apply','entry':entry,'binary_sha256':worker.digest(binary),'installed_sha256':worker.digest(original_binary),'config_sha256':worker.digest(home/'adapter.json'),'version':subprocess.check_output([str(binary),'--version'],text=True,encoding='utf-8').strip().split()[-1],'release_url':'https://example.invalid/disposable-fixture'}
    worker.write(folder/'plan.json',job);worker.write(home/'native-updates/active.json',{'id':identifier})
    current=worker.Worker(home,identifier)
    real_health=current.health;health_calls=[]
    if fail=="health":
     def fail_first(expected):
      health_calls.append(expected)
      if len(health_calls)==1:
       input_arrived_after_backup();raise RuntimeError('injected failed health after actual swap')
      return real_health(expected)
     current.health=fail_first
    if fail=="start":
     real_start=current.start;start_calls=[]
     def fail_start():
      start_calls.append(True)
      if len(start_calls)==1:
       input_arrived_after_backup();raise OSError('injected candidate start failure')
      return real_start()
     current.start=fail_start
    real_rollback=current.rollback;rollback_errors=[]
    def traced_rollback(reason):
     try:return real_rollback(reason)
     except Exception:
      rollback_errors.append(traceback.format_exc());raise
    current.rollback=traced_rollback
    if fail is None:
     # Use the packaged API dispatcher and its private interpreter for the real
     # no-toolchain worker path. Later cases inject faults at exact worker seams.
     request_id=str(uuid.uuid4())
     try:current.api('desktop/updates',{'action':'apply','request_id':request_id,'plan_id':identifier})
     except (TimeoutError,ConnectionError):pass  # Reconcile the same durable request; never redispatch.
     worker_deadline=time.monotonic()+240
     actual_worker=ActualWorker(folder,identifier)
     while time.monotonic()<worker_deadline:
      try:
       observed=current.api('desktop/updates');out=observed.get('current')
       if out is not None:
        assert out['id']==identifier
        sample={'state':out['state'],'worker_active':out.get('worker_active')}
        if not observations or observations[-1]!=sample:observations.append(sample)
        if os.name=='nt' and out.get('worker_active') is True and actual_worker.pid is None:actual_worker.capture()
        if out['state'] in {'succeeded','rolled_back','failed','deferred','recovery_required'} and out.get('worker_active') is False:break
      except (OSError,ConnectionError):pass  # Saved server is stopped/restarted by this exact update.
      time.sleep(.2)
     else:raise AssertionError('The installed native worker did not settle')
     if os.name=='nt':assert actual_worker.pid is not None,'Did not bind the actual private update worker'
     actual_worker.wait(worker_deadline)
     print('Observed real update API phases: '+json.dumps(observations))
     if result:result.with_suffix('.phases.json').write_text(json.dumps(observations,indent=2)+'\n')
     assert worker.read(home/'native-updates'/('request-'+request_id+'.json'))['id']==identifier
     assert (folder/'worker.py').read_bytes()==(ROOT/'scripts/native_update.py').read_bytes(),'Package worker differs from the fault-injection fixture'
    else:
     current.execute();out=worker.read(folder/'plan.json')
    assert out['state']==('rolled_back' if fail else 'succeeded'),{'plan':out,'rollback_errors':rollback_errors}
    checks.append('actual failed start restores prior binary/runtime' if fail=='start' else 'actual failed verification restores prior binary/runtime' if fail else 'actual API dispatch and packaged private worker perform idle stop, swap and verified restart')
    assert (home/'adapter.json').read_bytes()==config_bytes
    assert worker.digest(home/'secrets/local-password')==secret_before and worker.digest(home/'secrets/control-token')==private_before
    assert current.api('status')['id']==identity_before
    with database(home/'state.sqlite3') as db:assert db.execute("SELECT value FROM records WHERE collection='native-fixture' AND id='preserved'").fetchone()==(json.dumps({'value':expected_value[0]},separators=(',',':')),)
    assert not (home/'updates/drain.json').exists()
   checks.append('identity/config/credentials/DB evidence unchanged; no database rewind')
   # A stop failure with the actual server still holding its lock cannot replace anything.
   identifier=str(uuid.uuid4());folder=home/'native-updates'/identifier;folder.mkdir();shutil.copytree(bundle,folder/'candidate');job.update(id=identifier,state='ready',installed_sha256=worker.digest(original_binary),config_sha256=worker.digest(home/'adapter.json'));worker.write(folder/'plan.json',job);worker.write(home/'native-updates/active.json',{'id':identifier});current=worker.Worker(home,identifier)
   before=worker.digest(original_binary);current.stop=lambda:(_ for _ in ()).throw(RuntimeError('injected unknown stop'))
   current.execute();assert worker.read(folder/'plan.json')['state']=='recovery_required';assert worker.digest(original_binary)==before;assert current.api('status')['id']==identity_before;checks.append('unknown stop never replaces a running service or rewinds state')
  finally:
   if actual_worker is not None:actual_worker.wait(worker_deadline)
   if current:
    current=worker.Worker(home,current.id);current.save('rolling_back')
    try:current.stop()
    except Exception:pass
   if proc.poll() is None:proc.terminate()
   proc.wait(timeout=20);log.close()
 receipt={'passed':True,'checks':checks,'package_sha256':hashlib.sha256(package.read_bytes()).hexdigest(),'limits':['Release download represented by local exact bundle; GitHub digest selection separately tested','Connector disabled throughout; no Slack, model, session or startup registration']}
 if result:result.write_text(json.dumps(receipt,indent=2)+'\n')
 receipt['managed_update']=managed_roundtrip(package,result) if os.name=='nt' or sys.platform=='darwin' else {
  'status':'not_run','reason':'Actual user-manager setup is qualified on Windows/macOS only; Linux retains the legacy process fixture without provisioning a user systemd session.'}
 if result:result.write_text(json.dumps(receipt,indent=2)+'\n')
 print(json.dumps(receipt,indent=2))
if __name__=='__main__':
 p=argparse.ArgumentParser();p.add_argument('--package',type=Path);p.add_argument('--result',type=Path);p.add_argument('--self-test',action='store_true');a=p.parse_args()
 if a.self_test:
  from unittest.mock import patch
  class ObserverTests(unittest.TestCase):
   def test_exact_paths_accept_resolved_alias_but_not_copy_or_different_hardlink(self):
    with tempfile.TemporaryDirectory() as tmp:
     root=Path(tmp);file=root/'worker';file.write_bytes(b'one');copy=root/'copy';copy.write_bytes(b'one')
     self.assertTrue(exact_path_identity(file,file));self.assertFalse(exact_path_identity(file,copy))
     other=root/'hardlink';os.link(file,other);self.assertFalse(exact_path_identity(file,other))
     if os.name=='nt':self.assertTrue(exact_path_identity(str(file),'\\\\?\\'+str(file.resolve())))
     else:
      alias=root/'alias';alias.symlink_to(file);self.assertTrue(exact_path_identity(file,alias))
   def test_cleanup_reply_requires_same_identity_exact_safe_projection_and_complete_proof(self):
    plan=str(uuid.uuid4());request=str(uuid.uuid4())
    value={'schema':'com.microsoft.amplifier.slack-native-update-cleanup/1','plan_id':plan,'request_id':request,
     'cleanup':'complete','observation_only':False,'proof_basis':'matching_worker_receipts_exact_profile_inactive_empty_and_lifetime_lock_free','full_generation_observed':False}
    validate_cleanup_reply(value,plan,request,False)
    validate_cleanup_reply({**value,'observation_only':True},plan,request,True)
    for key,wrong in [('plan_id',str(uuid.uuid4())),('request_id',str(uuid.uuid4())),('cleanup','unknown'),('proof_basis',None),('full_generation_observed',True),('observation_only',0)]:
     with self.subTest(key=key),self.assertRaises(AssertionError):validate_cleanup_reply({**value,key:wrong},plan,request,False)
    with self.assertRaises(AssertionError):validate_cleanup_reply({**value,'home':'private'},plan,request,False)
   def test_cleanup_inventory_detects_new_dispatch_or_modified_saved_request(self):
    with tempfile.TemporaryDirectory() as tmp:
     home=Path(tmp);folder=home/'native-updates';folder.mkdir();saved=folder/'request.json';saved.write_text('original')
     original=update_inventory(home);self.assertEqual(update_inventory(home),original)
     saved.write_text('changed');self.assertNotEqual(update_inventory(home),original)
     saved.write_text('original');(folder/'another-worker').mkdir();self.assertNotEqual(update_inventory(home),original)
   def test_partial_create_only_receipt_is_pending_but_malformed_complete_is_fatal(self):
    with tempfile.TemporaryDirectory() as tmp:
     file=Path(tmp)/'entry.json';observed=[]
     self.assertIsNone(receipt_observation(file,observed))
     for value in ['', '{', '{"schema":', '{"request_id":"abc']:
      file.write_text(value);self.assertIsNone(receipt_observation(file,observed))
     file.write_text('{"schema":1}');self.assertEqual(receipt_observation(file,observed),{'schema':1})
     file.write_text('{"schema":bad}')
     with self.assertRaises(json.JSONDecodeError):receipt_observation(file,observed)
     for valid in ('null','[]','1','true'):
      file.write_text(valid)
      with self.assertRaises(AssertionError):receipt_observation(file,observed)
     with patch.object(worker,'read',side_effect=PermissionError('refused')):
      with self.assertRaises(PermissionError):receipt_observation(file,observed)
     self.assertEqual(len(observed),4)
   def test_complete_wrong_receipt_fields_are_fatal_not_pending(self):
    plan=str(uuid.uuid4());request=str(uuid.uuid4());value={'schema':1,'plan_id':plan,'request_id':request,'pid':123}
    validate_worker_receipt(value,plan,request)
    for key,wrong in [('schema',True),('schema',2),('plan_id',str(uuid.uuid4())),('request_id',str(uuid.uuid4())),('pid',True),('pid',0)]:
     with self.subTest(key=key,value=wrong),self.assertRaises(AssertionError):validate_worker_receipt({**value,key:wrong},plan,request)
    with self.assertRaises(AssertionError):validate_worker_receipt({**value,'state':'recovery_required'},plan,request,'succeeded')
   def test_managed_worker_selects_separate_fixed_windowless_interpreter(self):
    with tempfile.TemporaryDirectory() as tmp:
     actual=ActualWorker(Path(tmp),str(uuid.uuid4()),managed=True)
     self.assertEqual(actual.executable.name,'pythonw.exe' if os.name=='nt' else 'python3')
   @unittest.skipIf(os.name=='nt','Unix generation observer')
   def test_same_worker_generation_is_not_settled_by_free_lock(self):
    with tempfile.TemporaryDirectory() as tmp:
     actual=ActualWorker(Path(tmp),str(uuid.uuid4()),managed=True);actual.pid=123;actual.generation={'pid':123,'birth':'old'}
     with patch(__name__+'.unix_generation',return_value=actual.generation),patch.object(time,'sleep'),patch.object(time,'monotonic',side_effect=[0,2]),patch.object(worker,'locked') as lock:
      with self.assertRaises(AssertionError):actual.wait(1)
     lock.assert_not_called();self.assertFalse(actual.settled)
   @unittest.skipIf(os.name=='nt','Unix generation observer')
   def test_absent_or_reused_pid_settles_only_old_exact_generation(self):
    with tempfile.TemporaryDirectory() as tmp:
     for observed in [None,{'pid':123,'birth':'new'}]:
      actual=ActualWorker(Path(tmp),str(uuid.uuid4()),managed=True);actual.pid=123;actual.generation={'pid':123,'birth':'old'}
      with patch(__name__+'.unix_generation',return_value=observed):actual.wait(time.monotonic()+1)
      self.assertTrue(actual.settled)
  result=unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(ObserverTests));raise SystemExit(0 if result.wasSuccessful() else 1)
 if not a.package:p.error('--package is required for actual disposable qualification')
 run(a.package.resolve(),a.result)
