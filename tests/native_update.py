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
