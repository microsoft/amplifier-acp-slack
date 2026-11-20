"""Native worker boundaries using disposable files and a loopback HTTP impostor."""
import hashlib,importlib.util,io,json,os,tarfile,threading,unittest,uuid,zipfile
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch
from http.server import BaseHTTPRequestHandler,HTTPServer
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('native_update',ROOT/'scripts/native_update.py');u=importlib.util.module_from_spec(spec);spec.loader.exec_module(u)
class NativeUpdateTests(unittest.TestCase):
 def setUp(self):
  self.temp=TemporaryDirectory();self.addCleanup(self.temp.cleanup);self.home=Path(self.temp.name);self.id=str(uuid.uuid4());self.agent=str(uuid.uuid4())
  self.root=self.home/'native-updates';self.folder=self.root/self.id;self.folder.mkdir(parents=True);(self.home/'bin').mkdir();(self.home/'secrets').mkdir();(self.home/'runtime').mkdir()
  self.binary=self.home/'bin'/('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter');self.binary.write_bytes(b'original')
  u.write(self.home/'adapter.json',{'id':self.agent,'enabled':False,'server':{'auth_mode':'local_password','web_transport':'local_http','public_url':'http://127.0.0.1:1','port':1}})
  u.write(self.home/'native-installation.json',{'id':self.agent,'update_owner':'installation'});u.write(self.root/'active.json',{'id':self.id});u.write(self.folder/'plan.json',{'id':self.id,'state':'ready','action':'apply'})
 def worker(self):return u.Worker(self.home,self.id)
 def test_atomic_write_and_existing_owner_restriction(self):
  self.assertEqual(self.worker().config['id'],self.agent);u.write(self.home/'saved-connections.json',{'url':'https://owner.example'})
  with self.assertRaisesRegex(RuntimeError,'externally_managed'):self.worker()
 def test_malformed_journal_is_not_treated_as_absent(self):
  p=self.folder/'malformed.json';p.write_bytes(b'{"state":')
  with self.assertRaises(json.JSONDecodeError):u.read(p)
 def test_archive_refuses_escape_and_symlink(self):
  for name,mode in [('../escape',0o100644),('link',0o120777)]:
   file=self.folder/'test.zip'
   with zipfile.ZipFile(file,'w') as z:
    info=zipfile.ZipInfo(name);info.external_attr=mode<<16;z.writestr(info,b'bad')
   with self.assertRaises(RuntimeError):u.payload(file,self.folder/'candidate')
  self.assertFalse((self.root/'escape').exists())
 def test_runtime_inventory_is_exact_before_use(self):
  root=self.folder/'resources';(root/'runtime/python').mkdir(parents=True);(root/'runtime/python/python').write_bytes(b'python')
  manifest={'component':'acp-slack-adapter','target':u.native_target(),'python_files':{'python':u.digest(root/'runtime/python/python')}};u.write(root/'native-bundle.json',manifest)
  self.assertEqual(u.validate_runtime(root),manifest);(root/'runtime/python/python').write_bytes(b'changed')
  with self.assertRaisesRegex(RuntimeError,'private_runtime_mismatch'):u.validate_runtime(root)
 def test_busy_work_is_deferred_before_any_stop_or_swap(self):
  root=self.folder/'candidate';(root/'runtime/python').mkdir(parents=True);(root/'runtime/python/python').write_bytes(b'python');(root/'candidate-bin').write_bytes(b'new')
  u.write(root/'native-bundle.json',{'component':'acp-slack-adapter','target':u.native_target(),'python_files':{'python':u.digest(root/'runtime/python/python')}})
  job=u.read(self.folder/'plan.json');job.update(entry='candidate-bin',binary_sha256=u.digest(root/'candidate-bin'),installed_sha256=u.digest(self.binary),config_sha256=u.digest(self.home/'adapter.json'));u.write(self.folder/'plan.json',job)
  worker=self.worker()
  with patch.object(u.sys,'platform','linux'),patch.object(worker,'api',return_value={'idle':False}),patch.object(worker,'stop') as stop:
   worker.apply();stop.assert_not_called()
  self.assertEqual(u.read(worker.file)['state'],'deferred');self.assertEqual(self.binary.read_bytes(),b'original')
 def test_explicit_rollback_defers_before_stopping_busy_work(self):
  worker=self.worker();worker.job.update(action='rollback',state='queued')
  with patch.object(worker,'api',return_value={'idle':False}),patch.object(worker,'stop') as stop:
   worker.apply();stop.assert_not_called()
  self.assertEqual(worker.job['state'],'deferred')
 def test_rollback_reconciles_the_known_candidate_before_its_api_is_ready(self):
  from types import SimpleNamespace
  worker=self.worker();backup=self.folder/'backup';backup.mkdir();(backup/'binary').write_bytes(b'previous');worker.job['previous_sha256']=u.digest(backup/'binary')
  calls=[]
  def stop_before_api_ready():
   calls.append(True)
   if len(calls)<3:raise ConnectionRefusedError()
  # The service lock stays free until this known process has actually started.
  worker.started_process=SimpleNamespace(poll=lambda:0 if len(calls)>=3 else None)
  with patch.object(worker,'stop',side_effect=stop_before_api_ready),patch.object(worker,'replace'),patch.object(worker,'start'),patch.object(worker,'health'):
   worker.rollback('fixture_failed_health');self.assertEqual(len(calls),3)
  self.assertEqual(worker.job['state'],'rolled_back')
 def test_identical_release_has_no_cutover(self):
  worker=self.worker();worker.job.update(up_to_date=True,binary_sha256=u.digest(self.binary))
  with patch.object(worker,'stop') as stop:
   worker.apply();stop.assert_not_called()
  self.assertEqual(worker.job['state'],'succeeded');self.assertFalse(worker.job['changed'])
 def test_replacement_launch_is_durable_before_process_dispatch(self):
  from types import SimpleNamespace
  worker=self.worker();worker.save('verifying',binary_sha256=u.digest(self.binary))
  launches=[]
  def spawn(command,**kwargs):
   journal=u.read(worker.file);launch=journal.get('launch')
   self.assertIsInstance(launch,dict,'A replacement launch must be recorded before Popen')
   self.assertEqual(launch['mode'],'candidate');self.assertEqual(launch['binary_sha256'],u.digest(self.binary))
   self.assertEqual(kwargs['env']['ACP_SLACK_UPDATE_PLAN'],self.id)
   self.assertEqual(kwargs['env']['ACP_SLACK_UPDATE_LAUNCH'],launch['nonce']);launches.append(launch['nonce'])
   return SimpleNamespace(pid=123)
  with patch.object(u.subprocess,'Popen',side_effect=spawn):worker.start();worker.start()
  self.assertNotEqual(launches[0],launches[1],'Each dispatch invalidates prior launch identity')
 def managed_worker(self):
  marker=u.read(self.home/'native-installation.json');marker['manager_owner']=u.MANAGER_OWNER;u.write(self.home/'native-installation.json',marker)
  worker=self.worker();worker.save('verifying',binary_sha256=u.digest(self.binary));return worker
 def test_manager_path_spelling_accepts_only_windows_canonical_prefix_equivalence(self):
  with patch.object(u.os,'name','nt'):
   self.assertTrue(u.native_path_matches('\\\\?\\C:\\Owner\\Slack','C:\\Owner\\Slack'))
   self.assertTrue(u.native_path_matches('\\\\?\\UNC\\server\\owner\\Slack','\\\\server\\owner\\Slack'))
   self.assertTrue(u.native_path_matches('C:\\OWNER\\Slack','c:/owner/slack'))
   for value in [None,'C:\\Owner\\Other','C:\\Owner\\x\\..\\Slack','Owner\\Slack','\\\\.\\C:\\Owner\\Slack']:
    with self.subTest(value=value):self.assertFalse(u.native_path_matches(value,'C:\\Owner\\Slack'))
  with patch.object(u.os,'name','posix'):
   self.assertFalse(u.native_path_matches('/home/Owner','/home/owner'))
 def managed_observation(self,worker,request):
  return {'schema':u.MANAGER_SCHEMA,'id':self.agent,'home':str(worker.home),'executable':str(worker.binary),
          'state':'ready','request_current':True,'profile_matches':True,'launch_observed':True,'autostart':False,
          'request':{'request_id':request,'phase':'ready'},'pid':123,'process_identity':{'pid':123,'start_ticks':'456','boot_id':str(uuid.uuid4())},
          'service':{'manager':'fixture','name':'acp-slack-adapter-'+self.agent}}
 def test_managed_launch_records_same_intent_before_one_fixed_manager_dispatch(self):
  from types import SimpleNamespace
  worker=self.managed_worker();calls=[]
  def run(command,**options):
   launch=u.read(worker.file)['launch'];request=launch['manager_request_id']
   self.assertEqual(launch['manager_dispatch'],'intent')
   self.assertEqual(command,[str(worker.binary),'--home',str(worker.home),'native-start','--request-id',request,'--update-plan',self.id,'--update-launch',launch['nonce']])
   self.assertLessEqual(options['timeout'],60);self.assertGreater(options['timeout'],0)
   calls.append(command);return SimpleNamespace(returncode=0,stdout=json.dumps(self.managed_observation(worker,request)))
  with patch.object(u.subprocess,'run',side_effect=run),patch.object(u.subprocess,'Popen') as direct:
   worker.start();direct.assert_not_called()
  self.assertEqual(len(calls),1);self.assertEqual(worker.job['launch']['manager_dispatch'],'confirmed')
  self.assertEqual(worker.started_pid,123);self.assertEqual(worker.started_generation,worker.job['launch']['process_identity'])
  self.assertFalse(hasattr(worker,'started_process'))
 def test_managed_lost_start_response_keeps_same_request_and_has_no_child_fallback(self):
  worker=self.managed_worker()
  with patch.object(u.subprocess,'run',side_effect=u.subprocess.TimeoutExpired('native-start',60)) as dispatch,patch.object(u.subprocess,'Popen') as direct:
   with self.assertRaises(u.ManagedUnknown):worker.start()
   self.assertEqual(dispatch.call_count,1);direct.assert_not_called()
  launch=u.read(worker.file)['launch'];self.assertEqual(launch['manager_dispatch'],'unknown')
  self.assertEqual(str(uuid.UUID(launch['manager_request_id'])),launch['manager_request_id'])
  self.assertEqual(str(uuid.UUID(launch['nonce'])),launch['nonce'])
  with patch.object(u.subprocess,'run') as repeated,self.assertRaises(u.ManagedUnknown):worker.start()
  repeated.assert_not_called();self.assertEqual(u.read(worker.file)['launch'],launch)
 def test_lost_manager_reply_journal_failure_does_not_mask_unknown_or_start_rollback(self):
  for action in ('start','stop'):
   with self.subTest(action=action):
    worker=self.managed_worker()
    if action=='stop':self.seed_managed_launch(worker)
    original=worker.save
    def save(state=None,**values):
     value=values.get('launch') if action=='start' else values.get('manager_stop')
     if isinstance(value,dict) and (value.get('manager_dispatch')=='unknown' or value.get('dispatch')=='unknown'):raise OSError('journal unavailable')
     original(state,**values)
    with patch.object(worker,'save',side_effect=save),patch.object(u.subprocess,'run',side_effect=u.subprocess.TimeoutExpired('manager',60)),self.assertRaises(u.ManagedUnknown):getattr(worker,action)()
    saved=u.read(worker.file)['launch' if action=='start' else 'manager_stop']
    self.assertEqual(saved['manager_dispatch' if action=='start' else 'dispatch'],'intent')
 def test_confirmed_manager_reply_journal_failure_remains_unknown(self):
  from types import SimpleNamespace
  for action in ('start','stop'):
   with self.subTest(action=action):
    worker=self.managed_worker()
    if action=='stop':self.seed_managed_launch(worker)
    original=worker.save
    def save(state=None,**values):
     value=values.get('launch') if action=='start' else values.get('manager_stop')
     if isinstance(value,dict) and (value.get('manager_dispatch')=='confirmed' or value.get('dispatch')=='confirmed'):raise OSError('journal unavailable')
     original(state,**values)
    def run(command,**options):
     value=self.managed_observation(worker,worker.job['launch']['manager_request_id']) if action=='start' else self.managed_stop_reply(worker,worker.job['manager_stop'])
     return SimpleNamespace(returncode=0,stdout=json.dumps(value))
    with patch.object(worker,'save',side_effect=save),patch.object(u.subprocess,'run',side_effect=run),self.assertRaises(u.ManagedUnknown):getattr(worker,action)()
 def test_managed_ready_requires_current_full_generation_and_profile(self):
  worker=self.managed_worker();request=str(uuid.uuid4());value=self.managed_observation(worker,request)
  self.assertEqual(worker.managed_ready(value,request),value['process_identity'])
  for key,changed in [('schema','old'),('id',str(uuid.uuid4())),('home','other'),('request_current',False),('profile_matches',False),('launch_observed',False),('autostart',True),('pid',124),('request',None),('service',{})]:
   with self.subTest(key=key),self.assertRaises(u.ManagedUnknown):worker.managed_ready({**value,key:changed},request)
  with self.assertRaises(u.ManagedUnknown):worker.managed_ready(value,str(uuid.uuid4()))
  with self.assertRaises(u.ManagedUnknown):worker.managed_ready(value,request,{**value['process_identity'],'start_ticks':'789'})
 def test_managed_start_and_health_share_one_original_60_second_window(self):
  from types import SimpleNamespace
  worker=self.managed_worker();replies=[];timeouts=[];now=[100]
  def run(command,**options):
   timeouts.append(options['timeout'])
   if command[3]=='native-start':replies.append(self.managed_observation(worker,worker.job['launch']['manager_request_id']));now[0]=125
   return SimpleNamespace(returncode=0,stdout=json.dumps(replies[0]))
  with patch.object(u.time,'monotonic',side_effect=lambda:now[0]),patch.object(u.subprocess,'run',side_effect=run),patch.object(u.subprocess,'check_output',return_value='acp-slack-adapter 99.0.0\n'),patch.object(worker,'api',return_value={'id':self.agent,'version':'99.0.0','addresses':{'pid':123}}):
   worker.start();self.assertEqual(worker.managed_deadline,160);worker.health(u.digest(self.binary))
  self.assertEqual(timeouts,[60,35])
 def test_managed_unknown_execute_never_autorolls_back_or_replays(self):
  worker=self.managed_worker();worker.gate();backup=self.folder/'backup';backup.mkdir();(backup/'binary').write_bytes(b'previous')
  with patch.object(worker,'apply',side_effect=u.ManagedUnknown('lost reply')),patch.object(worker,'rollback') as rollback:
   worker.execute();rollback.assert_not_called()
  self.assertEqual(worker.job['state'],'recovery_required');self.assertTrue((self.home/'updates/drain.json').exists())
 def test_managed_rollback_does_not_treat_a_free_lock_as_stop_proof(self):
  worker=self.managed_worker()
  with patch.object(worker,'stop',side_effect=u.ManagedUnknown('unknown stop')),patch.object(u,'locked') as lock,patch.object(worker,'replace') as replace:
   with self.assertRaises(u.ManagedUnknown):worker.rollback('fixture')
   lock.assert_not_called();replace.assert_not_called()
 def test_unknown_or_changed_manager_ownership_is_not_legacy_fallback(self):
  marker=u.read(self.home/'native-installation.json');marker['manager_owner']='another-owner';u.write(self.home/'native-installation.json',marker)
  with self.assertRaisesRegex(RuntimeError,'manager_owner_unknown'):self.worker()
  worker=self.managed_worker();marker=u.read(self.home/'native-installation.json');marker.pop('manager_owner');u.write(self.home/'native-installation.json',marker)
  with self.assertRaisesRegex(RuntimeError,'manager_owner_changed'):worker.guard_ownership()
 def seed_managed_launch(self,worker,*,with_witness=True):
  start=str(uuid.uuid4());process={'pid':123,'start_ticks':'456','boot_id':str(uuid.uuid4())}
  service={'manager':'fixture','name':'acp-slack-adapter-'+self.agent}
  binding={'id':self.agent,'home':str(worker.home),'service':service,'profile':{'executable':str(worker.binary),
           'cwd':str(worker.home),'args':['--home',str(worker.home),'native-serve'],'autostart':False},'binary_sha256':u.digest(worker.binary),'update':None}
  manager=worker.home/'native-manager';u.write(manager/'active.json',{'request_id':start})
  u.write(manager/'requests'/(start+'.json'),{'request_id':start,'phase':'ready','binding':binding})
  if with_witness:u.write(manager/'requests'/(start+'.launched.json'),{'request_id':start,'binding':binding,'process':process})
  worker.save('stopping',config_sha256=u.digest(self.home/'adapter.json'),installed_sha256=u.digest(worker.binary))
  return {'start_request_id':start,'process_identity':process if with_witness else None,'service':service}
 def managed_stop_reply(self,worker,intent):
  return {'schema':u.MANAGER_SCHEMA,'id':self.agent,'home':str(worker.home),'executable':str(worker.binary),
          'state':'inactive','control_current':True,'autostart':False,'service':intent['service'],
          'control':{'request_id':intent['request_id'],'action':'stop','phase':'inactive','start_request_id':intent['start_request_id'],
                     'dispatch_outcome':'accepted','observation':'exact_profile_inactive_and_service_lock_available',
                     'process_identity':intent['process_identity'],'process_absent':True,'update_plan':self.id}}
 def test_managed_stop_is_one_journal_bound_dispatch_without_health_dependency(self):
  from types import SimpleNamespace
  worker=self.managed_worker();captured=self.seed_managed_launch(worker);calls=[]
  def run(command,**options):
   intent=u.read(worker.file)['manager_stop'];self.assertEqual(intent['dispatch'],'intent')
   self.assertEqual(intent['config_sha256'],u.digest(self.home/'adapter.json'))
   self.assertEqual({key:intent[key] for key in captured},captured)
   self.assertEqual(command,[str(worker.binary),'--home',str(worker.home),'native-stop','--request-id',intent['request_id'],'--update-plan',self.id])
   self.assertLessEqual(options['timeout'],60);calls.append(command)
   return SimpleNamespace(returncode=0,stdout=json.dumps(self.managed_stop_reply(worker,intent)))
  with patch.object(u.subprocess,'run',side_effect=run),patch.object(worker,'api') as health,patch.object(u,'locked') as lock:
   worker.stop();health.assert_not_called();lock.assert_not_called()
  self.assertEqual(len(calls),1);self.assertEqual(worker.job['manager_stop']['dispatch'],'confirmed')
 def test_managed_restore_captures_current_settings_without_rewriting_them(self):
  from types import SimpleNamespace
  worker=self.managed_worker();self.seed_managed_launch(worker);check_digest=worker.job['config_sha256']
  config=u.read(self.home/'adapter.json');config['owner_setting']='changed after CHECK';u.write(self.home/'adapter.json',config)
  current=(self.home/'adapter.json').read_bytes();worker.save('rolling_back')
  def run(command,**options):
   intent=u.read(worker.file)['manager_stop']
   self.assertEqual(intent['config_sha256'],hashlib.sha256(current).hexdigest())
   self.assertNotEqual(intent['config_sha256'],check_digest)
   self.assertEqual(worker.job['config_sha256'],check_digest)
   return SimpleNamespace(returncode=0,stdout=json.dumps(self.managed_stop_reply(worker,intent)))
  with patch.object(u.subprocess,'run',side_effect=run):worker.stop()
  self.assertEqual((self.home/'adapter.json').read_bytes(),current)
 def test_managed_stop_unknown_preserves_intent_without_repeat_or_free_lock_proof(self):
  worker=self.managed_worker();self.seed_managed_launch(worker)
  with patch.object(u.subprocess,'run',side_effect=u.subprocess.TimeoutExpired('native-stop',60)) as dispatch,patch.object(u,'locked') as lock:
   with self.assertRaises(u.ManagedUnknown):worker.stop()
   saved=u.read(worker.file)['manager_stop'];self.assertEqual(saved['dispatch'],'unknown')
   with self.assertRaises(u.ManagedUnknown):worker.stop()
   self.assertEqual(dispatch.call_count,1);lock.assert_not_called();self.assertEqual(u.read(worker.file)['manager_stop'],saved)
 def test_managed_stop_rejects_reused_pid_missing_exit_or_wrong_control(self):
  worker=self.managed_worker();captured=self.seed_managed_launch(worker);intent={'request_id':str(uuid.uuid4()),'dispatch':'intent',**captured}
  value=self.managed_stop_reply(worker,intent);worker.managed_stopped(value,intent)
  for key,changed in [('request_id',str(uuid.uuid4())),('start_request_id',str(uuid.uuid4())),('process_absent',False),('process_identity',{**captured['process_identity'],'start_ticks':'789'}),('process_identity',None),('action','retire'),('update_plan',str(uuid.uuid4()))]:
   with self.subTest(key=key),self.assertRaises(u.ManagedUnknown):worker.managed_stopped({**value,'control':{**value['control'],key:changed}},intent)
  with self.assertRaises(u.ManagedUnknown):worker.managed_stopped({**value,'control_current':False},intent)
 def test_never_launched_stop_accepts_null_only_with_exact_recorded_absence(self):
  worker=self.managed_worker();captured=self.seed_managed_launch(worker,with_witness=False)
  self.assertEqual(worker.managed_current_launch(),captured)
  intent={'request_id':str(uuid.uuid4()),'dispatch':'intent',**captured};value=self.managed_stop_reply(worker,intent)
  worker.managed_stopped(value,intent)
  del value['control']['process_identity']
  with self.assertRaises(u.ManagedUnknown):worker.managed_stopped(value,intent)
 def test_confirmed_same_launch_stop_uses_readonly_observation_not_another_dispatch(self):
  from types import SimpleNamespace
  worker=self.managed_worker();captured=self.seed_managed_launch(worker)
  prior={'request_id':str(uuid.uuid4()),'dispatch':'confirmed',**captured};worker.save(manager_stop=prior)
  value=self.managed_stop_reply(worker,prior);value.update(request_current=True,profile_matches=True,request={'request_id':captured['start_request_id'],'phase':'ready'})
  with patch.object(u.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps(value))) as command:
   worker.stop()
  self.assertEqual(command.call_args.args[0],[str(worker.binary),'--home',str(worker.home),'native-status'])
  self.assertEqual(worker.job['manager_stop'],prior)
 def test_managed_stop_refuses_changed_launch_witness_before_dispatch(self):
  worker=self.managed_worker();captured=self.seed_managed_launch(worker)
  path=worker.home/'native-manager/requests'/(captured['start_request_id']+'.launched.json');value=u.read(path)
  value['binding']['profile']['args']=['foreign'];u.write(path,value)
  with patch.object(u.subprocess,'run') as dispatch,self.assertRaises(u.ManagedUnknown):worker.stop()
  dispatch.assert_not_called();self.assertNotIn('manager_stop',worker.job)
 def prepare_worker_dispatch(self,phase='accepted'):
  worker=self.managed_worker();request=str(uuid.uuid4());worker.save('queued',request_id=request)
  u.write(self.folder/('worker-dispatch-'+request+'.json'),{'schema':1,'plan_id':self.id,'request_id':request,'phase':phase})
  return worker,request
 def test_managed_worker_enters_once_and_records_only_same_request_terminal_state(self):
  worker,request=self.prepare_worker_dispatch();calls=[]
  def execute(current):
   entered=u.read(self.folder/('worker-entered-'+request+'.json'))
   self.assertEqual(entered,{'schema':1,'plan_id':self.id,'request_id':request,'pid':os.getpid()})
   calls.append(True);current.save('succeeded')
  with patch.object(u.Worker,'execute',execute):u.run_managed_worker(self.home,self.id,request)
  finished=u.read(self.folder/('worker-finished-'+request+'.json'))
  self.assertEqual(finished['state'],'succeeded');self.assertEqual(finished['request_id'],request)
  worker.save('queued',request_id=request)
  with patch.object(u.Worker,'execute') as repeated:u.run_managed_worker(self.home,self.id,request)
  repeated.assert_not_called();self.assertEqual(len(calls),1)
  self.assertEqual(u.read(self.folder/('worker-finished-'+request+'.json')),finished)
 def test_managed_worker_refuses_stale_request_or_unstarted_dispatch_before_entry(self):
  worker,request=self.prepare_worker_dispatch()
  cases=[('job','request_id',str(uuid.uuid4())),('dispatch','phase','registering'),('dispatch','plan_id',str(uuid.uuid4())),('active','id',str(uuid.uuid4()))]
  paths={'job':worker.file,'dispatch':self.folder/('worker-dispatch-'+request+'.json'),'active':self.root/'active.json'}
  for owner,key,value in cases:
   original=u.read(paths[owner]);u.write(paths[owner],{**original,key:value})
   with self.subTest(owner=owner,key=key),patch.object(u.Worker,'execute') as execute,self.assertRaises(RuntimeError):u.run_managed_worker(self.home,self.id,request)
   execute.assert_not_called();self.assertFalse((self.folder/('worker-entered-'+request+'.json')).exists())
   u.write(paths[owner],original)
 def test_managed_worker_unknown_exception_or_changed_request_never_gets_finished_receipt(self):
  for outcome in ('unknown','exception','changed'):
   worker,request=self.prepare_worker_dispatch()
   def execute(current):
    if outcome=='exception':raise OSError('journal unavailable')
    current.save('recovery_required' if outcome=='unknown' else 'succeeded')
    if outcome=='changed':u.write(current.file,{**current.job,'request_id':str(uuid.uuid4())})
   with patch.object(u.Worker,'execute',execute):
    if outcome=='unknown':u.run_managed_worker(self.home,self.id,request)
    else:
     with self.assertRaises((OSError,RuntimeError)):u.run_managed_worker(self.home,self.id,request)
   self.assertTrue((self.folder/('worker-entered-'+request+'.json')).exists())
   self.assertFalse((self.folder/('worker-finished-'+request+'.json')).exists())
 def test_replace_keeps_other_installation_state_and_recovers_missing_runtime_dir(self):
  (self.home/'secrets/token').write_bytes(b'private');(self.home/'runtime/old').write_bytes(b'old');candidate=self.folder/'binary';candidate.write_bytes(b'new');runtime=self.folder/'new-runtime';runtime.mkdir();(runtime/'new').write_bytes(b'new-runtime')
  self.worker().replace(candidate,runtime);self.assertEqual(self.binary.read_bytes(),b'new');self.assertEqual((self.home/'runtime/new').read_bytes(),b'new-runtime');self.assertEqual((self.home/'secrets/token').read_bytes(),b'private')
  import shutil
  shutil.rmtree(self.home/'runtime');self.worker().replace(candidate,runtime);self.assertTrue((self.home/'runtime/new').exists())
 def release_fixture(self):
  version='99.0.0';target=u.native_target();filename='acp-slack-adapter-'+version+'-'+target+('.zip' if os.name=='nt' or u.sys.platform=='darwin' else '.tar.gz')
  entry='Slack Connector.app/Contents/MacOS/acp-slack-adapter' if u.sys.platform=='darwin' else 'acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter'
  base='Slack Connector.app/Contents/Resources/' if u.sys.platform=='darwin' else ''
  manifest={'component':'acp-slack-adapter','target':target,'python_files':{'private':hashlib.sha256(b'python').hexdigest()}}
  files={entry:b'candidate',base+'native-bundle.json':json.dumps(manifest).encode(),base+'runtime/python/private':b'python'};stream=io.BytesIO()
  if filename.endswith('.zip'):
   with zipfile.ZipFile(stream,'w') as archive:
    for name,data in files.items():archive.writestr(name,data)
  else:
   with tarfile.open(fileobj=stream,mode='w:gz') as archive:
    for name,data in files.items():info=tarfile.TarInfo(name);info.size=len(data);info.mode=0o755;archive.addfile(info,io.BytesIO(data))
  archive=stream.getvalue();metadata=json.dumps({'version':version,'target':target,'entry':entry,'sha256':hashlib.sha256(archive).hexdigest(),'binary_sha256':hashlib.sha256(b'candidate').hexdigest()}).encode()
  prefix='https://github.com/'+u.REPO+'/releases/download/v'+version+'/'
  blobs={prefix+filename:archive,prefix+filename+'.json':metadata}
  release={'tag_name':'v'+version,'draft':False,'prerelease':False,'id':17,'html_url':prefix,'assets':[{'name':url.removeprefix(prefix),'browser_download_url':url,'size':len(data),'digest':'sha256:'+hashlib.sha256(data).hexdigest()} for url,data in blobs.items()]}
  blobs['https://api.github.com/repos/'+u.REPO+'/releases/latest']=json.dumps(release).encode()
  return blobs,release
 def test_published_release_binds_fixed_origin_asset_digest_and_version(self):
  from types import SimpleNamespace
  blobs,release=self.release_fixture();worker=self.worker()
  with patch.object(u,'fetch',side_effect=lambda url,limit:blobs[url]),patch.object(u.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='acp-slack-adapter 99.0.0')):
   worker.check()
  self.assertEqual(worker.job['state'],'ready');self.assertEqual(worker.job['release_id'],17);self.assertEqual(worker.job['binary_sha256'],hashlib.sha256(b'candidate').hexdigest())
 @unittest.skipUnless(os.name=='nt','Windows version-probe creation flags')
 def test_both_windows_version_probes_are_hidden_without_widening_budgets(self):
  from types import SimpleNamespace
  blobs,_=self.release_fixture();worker=self.worker()
  with patch.object(u,'fetch',side_effect=lambda url,limit:blobs[url]),patch.object(u.subprocess,'run',return_value=SimpleNamespace(returncode=0,stdout='acp-slack-adapter 99.0.0')) as candidate:
   worker.check()
  self.assertEqual(candidate.call_args.kwargs['creationflags'],u.subprocess.CREATE_NO_WINDOW)
  self.assertEqual(candidate.call_args.kwargs['timeout'],15)
  worker=self.managed_worker();request=str(uuid.uuid4());value=self.managed_observation(worker,request)
  worker.managed_deadline=u.time.monotonic()+60;worker.started_pid=value['pid'];worker.started_generation=value['process_identity']
  worker.job['launch']={'manager_request_id':request,'service':value['service']}
  with patch.object(u.subprocess,'check_output',return_value='acp-slack-adapter 99.0.0') as installed,patch.object(worker,'manager_call',return_value=value),patch.object(worker,'api',return_value={'id':self.agent,'version':'99.0.0','addresses':{'pid':value['pid']}}):
   worker.health(u.digest(self.binary))
  self.assertEqual(installed.call_args.kwargs['creationflags'],u.subprocess.CREATE_NO_WINDOW)
  self.assertGreater(installed.call_args.kwargs['timeout'],0);self.assertLessEqual(installed.call_args.kwargs['timeout'],15)
 def test_changed_public_asset_digest_is_rejected_before_extraction(self):
  blobs,release=self.release_fixture();release['assets'][0]['digest']='sha256:'+'0'*64;blobs['https://api.github.com/repos/'+u.REPO+'/releases/latest']=json.dumps(release).encode()
  with patch.object(u,'fetch',side_effect=lambda url,limit:blobs[url]),self.assertRaisesRegex(RuntimeError,'release_asset_digest_mismatch'):self.worker().check()
  self.assertFalse((self.folder/'candidate').exists())
 def test_proof_connection_close_never_reconnects_with_credential(self):
  secret='z'*64;(self.home/'secrets/control-token').write_text(secret);seen=[];agent=self.agent
  class Handler(BaseHTTPRequestHandler):
   def do_GET(self):
    seen.append(dict(self.headers));nonce=self.path.split('nonce=')[-1];origin=f'http://127.0.0.1:{self.server.server_port}'
    proof=u.hmac.new(secret.encode(),f'acp-slack-adapter.local-proof/1\n{agent}\n{origin}\n{nonce}'.encode(),hashlib.sha256).hexdigest();body=json.dumps({'schema':1,'id':agent,'origin':origin,'nonce':nonce,'proof':proof}).encode();self.send_response(200);self.send_header('Connection','close');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
   def log_message(self,*args):pass
  server=HTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
  try:
   config=u.read(self.home/'adapter.json');config['server'].update(port=server.server_port,public_url=f'http://127.0.0.1:{server.server_port}');u.write(self.home/'adapter.json',config)
   with self.assertRaisesRegex(RuntimeError,'local_proof_connection_closed'):self.worker().api('status')
   self.assertEqual(len(seen),1);self.assertFalse(any(k.lower()=='authorization' for k in seen[0]))
  finally:server.shutdown();server.server_close();thread.join()
@unittest.skipUnless(os.name=='nt','Actual Windows file-sharing semantics')
class WindowsJournalTests(unittest.TestCase):
 def setUp(self):
  import ctypes as c
  from ctypes import wintypes as w
  self.c=c;self.w=w;self.temp=TemporaryDirectory(prefix='Slack journal café 子 ');self.addCleanup(self.temp.cleanup);self.root=Path(self.temp.name)
  self.kernel=c.WinDLL('kernel32',use_last_error=True)
  self.kernel.CreateFileW.argtypes=[w.LPCWSTR,w.DWORD,w.DWORD,w.LPVOID,w.DWORD,w.DWORD,w.HANDLE];self.kernel.CreateFileW.restype=w.HANDLE
  self.kernel.ReadFile.argtypes=[w.HANDLE,w.LPVOID,w.DWORD,c.POINTER(w.DWORD),w.LPVOID];self.kernel.ReadFile.restype=w.BOOL
  self.kernel.CloseHandle.argtypes=[w.HANDLE];self.kernel.CloseHandle.restype=w.BOOL
 def open_reader(self,path,sharing):
  h=self.kernel.CreateFileW(str(path),0x80000000,sharing,None,3,0x80,None)
  if h==self.c.c_void_p(-1).value:raise self.c.WinError(self.c.get_last_error())
  return h
 def snapshot(self,handle):
  buffer=self.c.create_string_buffer(4096);count=self.w.DWORD()
  self.assertTrue(self.kernel.ReadFile(handle,buffer,len(buffer),self.c.byref(count),None))
  return json.loads(buffer.raw[:count.value])
 def test_published_journal_preserves_already_open_reader_snapshot(self):
  p=self.root/'plan.json';u.write(p,{'generation':0});handle=self.open_reader(p,7)
  try:
   u.write(p,{'generation':1});u.write(p,{'generation':2})
   self.assertEqual(u.read(p),{'generation':2});self.assertEqual(self.snapshot(handle),{'generation':0})
  finally:self.kernel.CloseHandle(handle)
 def test_reader_denying_delete_is_refused_without_losing_original(self):
  p=self.root/'plan.json';u.write(p,{'generation':0});before=p.read_bytes();handle=self.open_reader(p,3)
  try:
   with self.assertRaises(OSError) as raised:u.write(p,{'generation':1})
   self.assertEqual(raised.exception.winerror,32);self.assertEqual(p.read_bytes(),before)
  finally:self.kernel.CloseHandle(handle)
  u.write(p,{'generation':2});self.assertEqual(u.read(p),{'generation':2})
 def test_concurrent_worker_reads_never_lose_or_partially_read_journal(self):
  p=self.root/'plan.json';u.write(p,{'generation':0,'padding':'x'*16384});stop=threading.Event();started=threading.Event();errors=[];reads=[]
  def reader():
   while not stop.is_set():
    try:
     value=u.read(p)
     if not isinstance(value.get('generation'),int) or value.get('padding')!='x'*16384:raise AssertionError('Partial journal')
     reads.append(value['generation']);started.set()
    except BaseException as error:errors.append(error);started.set();return
  threads=[threading.Thread(target=reader) for _ in range(2)]
  for thread in threads:thread.start()
  try:
   self.assertTrue(started.wait(5))
   for generation in range(1,101):u.write(p,{'generation':generation,'padding':'x'*16384})
  finally:
   stop.set()
   for thread in threads:thread.join(5);self.assertFalse(thread.is_alive())
  self.assertFalse(errors,repr(errors));self.assertTrue(reads);self.assertEqual(u.read(p)['generation'],100)
 def test_long_unicode_path_create_and_replace(self):
  folder=self.root
  while len(str(folder))<340:folder=folder/('café 子 '+('x'*25))
  folder.mkdir(parents=True);p=folder/'plan-😀.json'
  u.write(p,{'generation':0});u.write(p,{'generation':1});self.assertEqual(u.read(p),{'generation':1})
 def test_utf16_name_alignment_never_changes_destination_name(self):
  for length in range(1,9):
   p=self.root/(('x'*length)+'.json');u.write(p,{'length':length});u.write(p,{'length':length+1})
   self.assertEqual(u.read(p),{'length':length+1})
  self.assertEqual(sorted(p.name for p in self.root.iterdir()),sorted(('x'*n)+'.json' for n in range(1,9)))
 def test_readonly_destination_is_not_overridden(self):
  import stat
  p=self.root/'plan.json';u.write(p,{'generation':0});p.chmod(stat.S_IREAD)
  try:
   with self.assertRaises(OSError):u.write(p,{'generation':1})
   self.assertEqual(u.read(p),{'generation':0})
  finally:p.chmod(stat.S_IREAD|stat.S_IWRITE)
 def test_directory_is_not_replaced_by_json(self):
  p=self.root/'plan.json';p.mkdir();(p/'kept').write_bytes(b'evidence')
  with self.assertRaises(OSError):u.write(p,{'generation':1})
  self.assertEqual((p/'kept').read_bytes(),b'evidence')
 def test_failed_post_rename_flush_reports_uncertainty_without_undoing_publication(self):
  import contextlib
  from types import SimpleNamespace
  p=self.root/'plan.json';u.write(p,{'generation':0});original=u._windows_handle
  def refused_flush(handle):self.c.set_last_error(5);return False
  @contextlib.contextmanager
  def fail_after_actual_rename(path,access):
   with original(path,access) as (kernel,handle):
    yield SimpleNamespace(SetFileInformationByHandle=kernel.SetFileInformationByHandle,FlushFileBuffers=refused_flush),handle
  with patch.object(u,'_windows_handle',side_effect=fail_after_actual_rename),self.assertRaises(OSError) as raised:u.write(p,{'generation':1})
  self.assertEqual(raised.exception.winerror,5)
  # Publication occurred, but the caller received an error, never false success.
  self.assertEqual(u.read(p),{'generation':1})
if __name__=='__main__':unittest.main()
