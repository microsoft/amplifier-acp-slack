#!/usr/bin/env python3
"""Actual disposable connector stop/swap/restart/rollback; no Slack or model activity."""
import argparse,hashlib,importlib.util,json,os,shutil,subprocess,tempfile,time,traceback,uuid
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
for name,path in [('launcher',ROOT/'tests/native_launch.py'),('worker',ROOT/'scripts/native_update.py')]:
 spec=importlib.util.spec_from_file_location(name,path);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);globals()[name]=module

def run(package,result):
 checks=[]
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
   import sqlite3
   with sqlite3.connect(home/'state.sqlite3') as db:db.execute("INSERT INTO records(collection,id,value,updated_at) VALUES('native-fixture','preserved','{\"value\":1}',1)")
   expected_value=[1]
   def input_arrived_after_backup():
    expected_value[0]+=1
    with sqlite3.connect(home/'state.sqlite3') as db:db.execute("UPDATE records SET value=? WHERE collection='native-fixture' AND id='preserved'",(json.dumps({'value':expected_value[0]},separators=(',',':')),))
   original_binary=home/'bin'/binary.name
   for fail in [None,"health","start"]:
    identifier=str(uuid.uuid4());folder=home/'native-updates'/identifier;folder.mkdir(parents=True)
    shutil.copytree(bundle,folder/'candidate');entry=binary.relative_to(bundle).as_posix()
    job={'schema':1,'id':identifier,'state':'ready','action':'apply','entry':entry,'binary_sha256':worker.digest(binary),'installed_sha256':worker.digest(original_binary),'config_sha256':worker.digest(home/'adapter.json'),'version':subprocess.check_output([str(binary),'--version'],text=True).strip().split()[-1],'release_url':'https://example.invalid/disposable-fixture'}
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
     deadline=time.monotonic()+240
     while time.monotonic()<deadline:
      out=worker.read(folder/'plan.json')
      if out['state'] in {'succeeded','rolled_back','failed','deferred','recovery_required'}:break
      time.sleep(.2)
     else:raise AssertionError('The installed native worker did not settle')
     assert worker.read(home/'native-updates'/('request-'+request_id+'.json'))['id']==identifier
     assert (folder/'worker.py').read_bytes()==(ROOT/'scripts/native_update.py').read_bytes(),'Package worker differs from the fault-injection fixture'
    else:
     current.execute();out=worker.read(folder/'plan.json')
    assert out['state']==('rolled_back' if fail else 'succeeded'),{'plan':out,'rollback_errors':rollback_errors}
    checks.append('actual failed start restores prior binary/runtime' if fail=='start' else 'actual failed verification restores prior binary/runtime' if fail else 'actual API dispatch and packaged private worker perform idle stop, swap and verified restart')
    assert (home/'adapter.json').read_bytes()==config_bytes
    assert worker.digest(home/'secrets/local-password')==secret_before and worker.digest(home/'secrets/control-token')==private_before
    assert current.api('status')['id']==identity_before
    with sqlite3.connect(home/'state.sqlite3') as db:assert db.execute("SELECT value FROM records WHERE collection='native-fixture' AND id='preserved'").fetchone()==(json.dumps({'value':expected_value[0]},separators=(',',':')),)
    assert not (home/'updates/drain.json').exists()
   checks.append('identity/config/credentials/DB evidence unchanged; no database rewind')
   # A stop failure with the actual server still holding its lock cannot replace anything.
   identifier=str(uuid.uuid4());folder=home/'native-updates'/identifier;folder.mkdir();shutil.copytree(bundle,folder/'candidate');job.update(id=identifier,state='ready',installed_sha256=worker.digest(original_binary),config_sha256=worker.digest(home/'adapter.json'));worker.write(folder/'plan.json',job);worker.write(home/'native-updates/active.json',{'id':identifier});current=worker.Worker(home,identifier)
   before=worker.digest(original_binary);current.stop=lambda:(_ for _ in ()).throw(RuntimeError('injected unknown stop'))
   current.execute();assert worker.read(folder/'plan.json')['state']=='recovery_required';assert worker.digest(original_binary)==before;assert current.api('status')['id']==identity_before;checks.append('unknown stop never replaces a running service or rewinds state')
  finally:
   if current:
    current=worker.Worker(home,current.id);current.save('rolling_back')
    try:current.stop()
    except Exception:pass
   if proc.poll() is None:proc.terminate()
   proc.wait(timeout=20);log.close()
 receipt={'passed':True,'checks':checks,'package_sha256':hashlib.sha256(package.read_bytes()).hexdigest(),'limits':['Release download represented by local exact bundle; GitHub digest selection separately tested','Connector disabled throughout; no Slack, model, session or startup registration']}
 if result:result.write_text(json.dumps(receipt,indent=2)+'\n')
 print(json.dumps(receipt,indent=2))
if __name__=='__main__':
 p=argparse.ArgumentParser();p.add_argument('--package',type=Path,required=True);p.add_argument('--result',type=Path);a=p.parse_args();run(a.package.resolve(),a.result)
