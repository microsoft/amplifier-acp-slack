"""Disposable typed-Add ownership/lost-ack tests; no systemd or Slack calls."""
import importlib.util,json,tempfile,unittest,uuid,hashlib,copy,os,subprocess,time,getpass
from pathlib import Path
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('connection_setup',Path(__file__).resolve().parents[1]/'ops/connections/setup.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

class Setup(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
  for name in ['binary','installer']: (self.root/name).write_text('fixture')
  (self.root/'agent').mkdir();m.write(self.root/'agent/config.json',{'config':{'server':{'public_url':'https://agent.example'}}})
  self.credential=self.root/'slot-acp.json';m.write(self.credential,{'token':'fixture-private-scoped-acp','scopes':['acp','tools:use']})
  self.path=self.root/'owner.json';m.write(self.path,{'schema':1,'state':str(self.root/'owner'),'data_root':str(self.root/'homes'),'agent_home':str(self.root/'agent'),'binary':str(self.root/'binary'),'installer':str(self.root/'installer'),'slots':[{'id':'second','label':'Second workspace','bind':'127.0.0.1','port':18480,'public_url':'https://slack.example','acp_credential':{'file':str(self.credential),'sha256':hashlib.sha256(self.credential.read_bytes()).hexdigest()}}]})
  self.c=m.load(self.path);self.req={'schema':1,'service_id':'slack','revision':self.c['revision'],'action':'add','request_id':str(uuid.uuid4()),'values':{'label':'Team $(no-shell)','slot':'second'}};self.calls=[]
  exists=Path.exists;self.pam=patch.object(Path,'exists',lambda p:True if str(p)=='/etc/pam.d/acp-slack-adapter' else exists(p));self.pam.start()
 def tearDown(self):self.pam.stop();self.temp.cleanup()
 def inspect(self,c,home):
  self.assertEqual(m.read(home/'connection-gate.json')['request_id'],self.req['request_id'])
  return {'id':'slack:fixture','service':'slack','label':self.req['values']['label'],'revision':'r1','context_provider_ids':['slack:fixture'],'commands':{a:['/fixed/adapter','connection',a] for a in ['connect','disconnect','status']}}
 def invoke(self,args,**kwargs):
  self.calls.append(args);self.assertNotIn('shell',kwargs)
 def test_add_fixed_args_saved_paused_and_idempotent(self):
  r=m.operation(self.c,self.req,self.invoke,self.inspect)
  self.assertEqual(r['state'],'saved');self.assertEqual(len(self.calls),1)
  self.assertIn('--label=Team $(no-shell)',self.calls[0])
  self.assertEqual(m.read(self.root/'homes/second/saved-connections.json')['url'],'https://agent.example/#connections')
  self.assertEqual(m.operation(self.c,self.req,self.invoke,self.inspect),r);self.assertEqual(len(self.calls),1)
 def test_lost_installer_reply_observes_owned_home_without_reinvocation(self):
  def lost(args,**kwargs):self.invoke(args,**kwargs);raise TimeoutError('lost after effect')
  self.assertFalse(m.operation(self.c,self.req,lost,self.inspect)['settled'])
  self.req['action']='status';self.assertEqual(m.operation(self.c,self.req,self.invoke,self.inspect)['state'],'saved');self.assertEqual(len(self.calls),1)
 def test_changed_request_rejected_existing_home_preserved(self):
  m.operation(self.c,self.req,self.invoke,self.inspect);self.req['values']['label']='changed'
  with self.assertRaisesRegex(ValueError,'reused'):m.operation(self.c,self.req,self.invoke,self.inspect)
  self.req['request_id']=str(uuid.uuid4());r=m.operation(self.c,self.req,self.invoke,self.inspect)
  self.assertEqual(r['state'],'failed');self.assertEqual(len(self.calls),1)
 def test_unknown_status_allocates_nothing_and_unapproved_slot_rejected(self):
  self.req['action']='status';self.assertFalse(m.operation(self.c,self.req,self.invoke,self.inspect)['settled']);self.assertFalse((self.root/'homes').exists())
  self.req['values']['slot']='../../escape'
  with self.assertRaises(ValueError):m.operation(self.c,self.req,self.invoke,self.inspect)
  self.assertEqual(self.calls,[])
 def test_enrollment_has_only_typed_fields_and_fixed_owner_commands(self):
  e=m.enrollment(self.c,self.path);self.assertEqual([f['name'] for f in e['add']['fields']],['label','slot'])
  for action,argv in e['add_commands'].items():self.assertEqual(argv[-1],action);self.assertIn(str(self.path),argv)
  broken=dict(self.c,state=str(self.root/'homes/owner'));m.write(self.path,broken)
  with self.assertRaisesRegex(ValueError,'outside'):m.load(self.path)
 def test_missing_owner_or_changed_credential_refused(self):
  slot=copy.deepcopy(self.c['slots'][0]);slot.pop('acp_credential')
  with self.assertRaisesRegex(ValueError,'credential_required'):m.credential(slot)
  m.write(self.credential,{'token':'fixture-private-scoped-acp','scopes':['acp','context:owner']})
  slot=copy.deepcopy(self.c['slots'][0]);slot['acp_credential']['sha256']=hashlib.sha256(self.credential.read_bytes()).hexdigest()
  with self.assertRaisesRegex(ValueError,'scoped_acp'):m.credential(slot)
  with self.assertRaisesRegex(ValueError,'changed'):m.credential(self.c['slots'][0])
 def test_scoped_configuration_and_lost_reply_do_not_inherit_owner(self):
  home=self.root/'homes/second';(home/'secrets').mkdir(parents=True)
  config={'revision':1,'enabled':False,'context':{'enabled':False},'acp':{'command':str(self.root/'agent/bin/agent-instance'),'args':['acp','--home',str(self.root/'agent')]}}
  document=config;calls=[]
  def api(h,path,body=None,method=None):
   if body:
    calls.append(body);document.update(body['config']);document['revision']=2;raise TimeoutError('committed, reply lost')
   return copy.deepcopy(document)
  with self.assertRaises(TimeoutError):m.configure_scoped(self.c,home,api)
  m.configure_scoped(self.c,home,api);self.assertEqual(len(calls),1)
  self.assertEqual(document['acp']['args'][-2:],['--token-file',str(home/'secrets/agent-acp-token')]);self.assertFalse(document['context']['enabled'])
  self.assertEqual((home/'secrets/agent-acp-token').stat().st_mode&0o777,0o600)
  document['acp']['args']=['arbitrary-changed']
  with self.assertRaisesRegex(ValueError,'endpoint_changed'):m.configure_scoped(self.c,home,api)
 def test_actual_tls_scoped_config_stays_paused_and_is_idempotent(self):
  binary=Path(os.environ.get('ACP_SLACK_TEST_BINARY',Path(__file__).resolve().parents[1]/'target/debug/acp-slack-adapter'))
  self.assertTrue(binary.is_file(),'Build the adapter before process validation')
  home=self.root/'homes/second'
  subprocess.run([str(binary),'--home',str(home),'init','--owner',getpass.getuser()],check=True,capture_output=True)
  config=m.read(home/'adapter.json');config['acp'].update(command=str(self.root/'agent/bin/agent-instance'),args=['acp','--home',str(self.root/'agent')],cwd=str(self.root/'agent'));m.write(home/'adapter.json',config)
  m.write(home/'connection-gate.json',{'request_id':self.req['request_id']})
  process=subprocess.Popen([str(binary),'--home',str(home),'serve'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  try:
   for _ in range(100):
    if (home/'endpoints.json').exists():break
    time.sleep(.05)
   m.configure_scoped(self.c,home);first=m.local_api(home,'config')
   m.configure_scoped(self.c,home);self.assertEqual(m.local_api(home,'config'),first)
   self.assertFalse(first['enabled']);self.assertFalse(first['context']['enabled']);self.assertTrue((home/'connection-gate.json').exists())
   self.assertEqual(first['acp']['args'][-2:],['--token-file',str(home/'secrets/agent-acp-token')])
  finally:
   process.terminate()
   try:process.wait(timeout=10)
   except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
if __name__=='__main__':unittest.main()
