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
  worker=self.worker();backup=self.folder/'backup';backup.mkdir();(backup/'binary').write_bytes(b'previous');worker.job['previous_sha256']=u.digest(backup/'binary');worker.started_process=SimpleNamespace(poll=lambda:None)
  with patch.object(worker,'stop',side_effect=[ConnectionRefusedError(),None]) as stop,patch.object(worker,'replace'),patch.object(worker,'start'),patch.object(worker,'health'):
   worker.rollback('fixture_failed_health');self.assertEqual(stop.call_count,2)
  self.assertEqual(worker.job['state'],'rolled_back')
 def test_identical_release_has_no_cutover(self):
  worker=self.worker();worker.job.update(up_to_date=True,binary_sha256=u.digest(self.binary))
  with patch.object(worker,'stop') as stop:
   worker.apply();stop.assert_not_called()
  self.assertEqual(worker.job['state'],'succeeded');self.assertFalse(worker.job['changed'])
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
if __name__=='__main__':unittest.main()
