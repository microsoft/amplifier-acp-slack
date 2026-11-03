"""Disposable connector update/restart tests; no real systemd, GitHub or Slack effects."""
import getpass, importlib.util, io, json, os, shutil, signal, sqlite3, ssl, subprocess, zipfile, hashlib
import sys, tempfile, threading, time, unittest, uuid
from pathlib import Path
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]
BINARY=ROOT/'target/debug/acp-slack-adapter'
spec=importlib.util.spec_from_file_location('connector_update',ROOT/'scripts/update.py')
u=importlib.util.module_from_spec(spec);spec.loader.exec_module(u)

def alive(pid):
    try:
        return Path('/proc/'+str(pid)+'/stat').read_text().split()[2]!='Z'
    except FileNotFoundError:return False

def until(check, timeout=15):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        if check():return
        time.sleep(.05)
    raise AssertionError('Timed out waiting for fixture condition')

def fake_worker(home, identifier, mode='normal'):
    actual_run=u.run
    def run(*args,**kwargs):
        if args[0]=='/usr/bin/systemctl':return (home/'fixture.pid').read_bytes()
        return actual_run(*args,**kwargs)
    class Controller(u.Controller):
        def control(self,action):
            pid=int((home/'fixture.pid').read_text())
            if action=='stop':
                if alive(pid):os.kill(pid,signal.SIGTERM)
                until(lambda:not alive(pid))
                if mode=='crash_after_stop':
                    (home/'fixture-stopped').touch();time.sleep(120)
            else:
                log=(home/'fixture.log').open('ab')
                child=subprocess.Popen([str(self.binary),'serve','--home',str(home)],stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
                (home/'fixture.pid').write_text(str(child.pid));log.close()
        def health(self,expected):
            if mode=='all_health_failure' or (mode=='health_failure' and expected==self.job['binary_sha256']):raise RuntimeError('health_check_failed')
            super().health(expected)
    with patch.object(u,'service',return_value='fixture-only'),patch.object(u,'run',side_effect=run):
        with (home/'updates/worker.lock').open('a+b') as lock:
            u.fcntl.flock(lock,u.fcntl.LOCK_EX)
            Controller(home,identifier).execute()

class Updates(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='connector-update-');self.home=Path(self.temp.name);self.children=[]
        subprocess.run([str(BINARY),'init','--home',str(self.home),'--owner',getpass.getuser()],check=True,stdout=subprocess.DEVNULL)
        cfg=u.read(self.home/'adapter.json');cfg['context']['owner_slack_user_id']='UOWNER';u.write(self.home/'adapter.json',cfg)
        self.start();until(lambda:(self.home/'endpoints.json').exists())
        self.root=self.home/'updates';self.root.mkdir(exist_ok=True)
        u.write(self.root/'policy.json',{'schema':1,'adapter_id':cfg['id'],'revision':1,'enabled':True,'agent_enabled':True,'owner_slack_user_id':'UOWNER'})
        self.id=str(uuid.uuid4());self.folder=self.root/'plans'/self.id;self.folder.mkdir(parents=True)
        self.candidate=self.folder/'candidate';shutil.copyfile(BINARY,self.candidate)
        with self.candidate.open('ab') as f:f.write(b'\nfixture-update\n')
        self.candidate.chmod(0o755)
        self.original=u.digest(self.home/'bin/acp-slack-adapter')
        self.job={'id':self.id,'state':'queued','action':'apply','policy_revision':1,'authorized_owner':'UOWNER','actor_kind':'agent','binary_sha256':u.digest(self.candidate),'installed_sha256':self.original,'source_commit':'a'*40,'repository':u.REPO}
        u.write(self.folder/'plan.json',self.job);u.write(self.root/'active.json',{'id':self.id})
        self.secret=self.home/'secrets/fixture-secret';self.secret.write_text('private-not-exported');self.secret.chmod(0o600)
        self.original_config=(self.home/'adapter.json').read_bytes()
        with sqlite3.connect(self.home/'state.sqlite3') as db:db.execute("INSERT INTO session_bindings VALUES('saved','endpoint','session-preserved',1,'DTEST','',0)")
    def tearDown(self):
        for child in self.children:
            if child.poll() is None:child.kill()
            child.wait()
            if child.stdout:child.stdout.close()
            if child.stderr:child.stderr.close()
        if (self.home/'fixture.pid').exists():
            pid=int((self.home/'fixture.pid').read_text())
            if alive(pid):os.kill(pid,signal.SIGTERM);until(lambda:not alive(pid))
        self.temp.cleanup()
    def start(self):
        log=(self.home/'fixture.log').open('ab');child=subprocess.Popen([str(self.home/'bin/acp-slack-adapter'),'serve','--home',str(self.home)],stdout=log,stderr=log,start_new_session=True);log.close();self.children.append(child);(self.home/'fixture.pid').write_text(str(child.pid))
    def worker(self,mode='normal'):
        child=subprocess.Popen([sys.executable,str(Path(__file__).resolve()),'--worker',str(self.home),self.id,mode],stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True);self.children.append(child);return child
    def done(self,child):
        stdout,stderr=child.communicate(timeout=20);self.assertEqual(child.returncode,0,stderr.decode());return u.read(self.folder/'plan.json')
    def assert_preserved(self):
        self.assertEqual((self.home/'adapter.json').read_bytes(),self.original_config)
        self.assertEqual(self.secret.read_text(),'private-not-exported')
        self.assertTrue((self.folder/'backup/state.sqlite3').exists())
        with sqlite3.connect(self.home/'state.sqlite3') as db:self.assertEqual(db.execute("SELECT session_id FROM session_bindings WHERE context='saved'").fetchone()[0],'session-preserved')
    def test_enabled_connector_health_uses_current_socket_audit(self):
        cfg=u.read(self.home/'adapter.json');cfg['enabled']=True;u.write(self.home/'adapter.json',cfg)
        started=u.read(self.home/'endpoints.json')['started_at'];updated=started+1
        runtime={'state':'running','config_revision':cfg['revision'],'updated_at':updated,'connections':[]}
        u.write(self.home/'runtime.json',runtime)
        def socket(state,at):
            with sqlite3.connect(self.home/'state.sqlite3') as db:
                db.execute("INSERT INTO audit(time,kind,data) VALUES(?,'connection',?)",(at,json.dumps({'state':state})))
        status={'runtime':runtime,'connections':[]}
        self.assertFalse(u.current_socket_healthy(self.home,status,started,cfg['revision']))
        socket('connected',started-1)
        self.assertFalse(u.current_socket_healthy(self.home,status,started,cfg['revision']))
        socket('connected',updated)
        self.assertTrue(u.current_socket_healthy(self.home,status,started,cfg['revision']))
        # Exercise the actual HTTPS status response and Controller.health,
        # including its deliberately empty connections placeholder.
        with patch.object(u,'service',return_value='fixture-only'),patch.object(u,'run',return_value=(self.home/'fixture.pid').read_bytes()):
            u.Controller(self.home,self.id).health(self.original)
        for state in ['connecting','reconnecting','disconnected']:
            socket(state,updated+1)
            self.assertFalse(u.current_socket_healthy(self.home,status,started,cfg['revision']))
        socket('connected',updated+2)
        self.assertFalse(u.current_socket_healthy(self.home,status,updated+1,cfg['revision']))
        self.assertFalse(u.current_socket_healthy(self.home,status,started,cfg['revision']+1))
        runtime['state']='failed'
        self.assertFalse(u.current_socket_healthy(self.home,status,started,cfg['revision']))
    def test_fixed_source_and_verified_artifact_provenance(self):
        archive=io.BytesIO()
        executable=b"#!/bin/sh\nprintf 'acp-slack-adapter fixture\\n'\n"
        with zipfile.ZipFile(archive,'w') as zipped:zipped.writestr('acp-slack-adapter',executable)
        data=archive.getvalue();head='a'*40;expected='sha256:'+hashlib.sha256(data).hexdigest()
        arch={'aarch64':'ARM64','x86_64':'X64'}[u.platform.machine()]
        def gh(path):
            self.assertTrue(path.startswith('repos/microsoft/amplifier-acp-slack/'))
            if path.endswith('commits/main'):return {'sha':head}
            if '/workflows/check.yml/runs?' in path:return {'workflow_runs':[{'id':123,'head_sha':head,'event':'push','head_branch':'main','conclusion':'success','repository':{'full_name':u.REPO},'html_url':'https://github.com/'+u.REPO+'/actions/runs/123'}]}
            if path.endswith('/runs/123/artifacts?per_page=100'):return {'artifacts':[{'id':456,'name':'acp-slack-adapter-Linux-'+arch+'-'+head,'expired':False,'digest':expected,'size_in_bytes':len(data)}]}
            self.fail('Unexpected GitHub endpoint')
        class Download:
            def __init__(inner,args,**kwargs):
                self.assertEqual(args[-1],'repos/'+u.REPO+'/actions/artifacts/456/zip');kwargs['stdout'].write(data);kwargs['stdout'].flush();inner.returncode=0
            def poll(inner):return 0
            def wait(inner):return 0
        actual_popen=subprocess.Popen
        def popen(args,**kwargs):return Download(args,**kwargs) if args[0]=='/usr/bin/gh' else actual_popen(args,**kwargs)
        self.job.update(action='plan');u.write(self.folder/'plan.json',self.job)
        with patch.object(u,'service',return_value='fixture-only'),patch.object(u,'gh',side_effect=gh),patch.object(u.subprocess,'Popen',side_effect=popen):
            controller=u.Controller(self.home,self.id);controller.execute();job=u.read(self.folder/'plan.json');self.assertEqual(job['state'],'ready',job);self.assertEqual(job['source_commit'],head);self.assertEqual(job['artifact_digest'],expected);self.assertEqual(job['binary_sha256'],hashlib.sha256(executable).hexdigest())
            expected='sha256:'+'0'*64;controller=u.Controller(self.home,self.id);controller.execute();job=u.read(self.folder/'plan.json');self.assertEqual(job['reason'],'artifact_digest_mismatch')

    def test_success_and_manual_rollback_restart_real_connector(self):
        before=int((self.home/'fixture.pid').read_text());job=self.done(self.worker());self.assertEqual(job['state'],'succeeded',job)
        self.assertNotEqual(before,int((self.home/'fixture.pid').read_text()));self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.job['binary_sha256']);self.assertEqual(u.read(self.home/'release.json')['commit'],'a'*40);self.assert_preserved()
        job.update(state='queued',action='rollback');u.write(self.folder/'plan.json',job)
        job=self.done(self.worker());self.assertEqual(job['state'],'rolled_back',job);self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.original);self.assertNotIn('commit',u.read(self.home/'release.json'));self.assert_preserved()
    def test_failed_health_restores_known_good_binary(self):
        job=self.done(self.worker('health_failure'));self.assertEqual(job['state'],'rolled_back',job);self.assertEqual(job['reason'],'candidate_failed_health');self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.original);self.assert_preserved()
    def test_failed_rollback_stays_paused_until_explicit_recovery(self):
        job=self.done(self.worker('all_health_failure'));self.assertEqual(job['state'],'recovery_required',job);self.assertTrue((self.root/'drain.json').exists())
        job.update(state='rolling_back',action='rollback');u.write(self.folder/'plan.json',job)
        job=self.done(self.worker());self.assertEqual(job['state'],'rolled_back',job);self.assertFalse((self.root/'drain.json').exists());self.assert_preserved()
    def test_helper_loss_after_stop_recovers_without_replaying_work(self):
        child=self.worker('crash_after_stop');until(lambda:(self.home/'fixture-stopped').exists());child.kill();child.wait()
        self.assertEqual(u.read(self.folder/'plan.json')['state'],'stopping')
        # Policy revocation cannot prevent restoring the already-stopped service.
        p=u.read(self.root/'policy.json');p.update(enabled=False,revision=2);u.write(self.root/'policy.json',p)
        job=self.done(self.worker());self.assertEqual(job['state'],'rolled_back',job);self.assertEqual(job['reason'],'interrupted_update_recovered');self.assert_preserved()
    def test_waits_for_turn_and_reply_drain_before_restart(self):
        with sqlite3.connect(self.home/'state.sqlite3') as db:
            db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES('busy','context','{}','running',0,0)")
            db.execute("INSERT INTO outbox(id,job_id,context,channel,thread,text,status,created,available) VALUES('reply','busy','context','DTEST','','reply','ready',0,0)")
        before=int((self.home/'fixture.pid').read_text());child=self.worker();until(lambda:u.read(self.folder/'plan.json')['state']=='draining');time.sleep(.3);self.assertEqual(int((self.home/'fixture.pid').read_text()),before)
        with sqlite3.connect(self.home/'state.sqlite3') as db:db.execute("UPDATE jobs SET status='speak' WHERE id='busy'")
        time.sleep(1.2);self.assertEqual(int((self.home/'fixture.pid').read_text()),before)
        with sqlite3.connect(self.home/'state.sqlite3') as db:db.execute("UPDATE outbox SET status='sent' WHERE id='reply'")
        job=self.done(child);self.assertEqual(job['state'],'succeeded',job)
        with sqlite3.connect(self.home/'state.sqlite3') as db:self.assertEqual(db.execute("SELECT status FROM outbox WHERE id='reply'").fetchone()[0],'sent')
    def test_corrupt_candidate_and_policy_change_do_not_restart(self):
        before=int((self.home/'fixture.pid').read_text());self.candidate.write_bytes(b'invalid');job=self.done(self.worker());self.assertEqual(job['state'],'failed');self.assertEqual(job['reason'],'candidate_digest_mismatch');self.assertEqual(int((self.home/'fixture.pid').read_text()),before)
        job.update(state='queued');u.write(self.folder/'plan.json',job);p=u.read(self.root/'policy.json');p['revision']=2;u.write(self.root/'policy.json',p);job=self.done(self.worker());self.assertEqual(job['reason'],'update_policy_changed');self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.original)
    def test_uncertain_delivery_defers_without_blocking_further_agent_work(self):
        with sqlite3.connect(self.home/'state.sqlite3') as db:db.execute("INSERT INTO outbox(id,job_id,context,channel,thread,text,status,created,available) VALUES('uncertain','missing','context','DTEST','','reply','uncertain',0,0)")
        job=self.done(self.worker());self.assertEqual(job['state'],'deferred');self.assertEqual(job['reason'],'uncertain_delivery_requires_reconciliation');self.assertFalse((self.root/'drain.json').exists());self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.original)
    def test_revocation_while_draining_releases_gate(self):
        with sqlite3.connect(self.home/'state.sqlite3') as db:db.execute("INSERT INTO jobs(id,context,trigger,status,created,updated) VALUES('busy','context','{}','running',0,0)")
        child=self.worker();until(lambda:u.read(self.folder/'plan.json')['state']=='draining');time.sleep(.3)
        p=u.read(self.root/'policy.json');p['enabled']=False;u.write(self.root/'policy.json',p);job=self.done(child);self.assertEqual(job['state'],'failed');self.assertEqual(job['reason'],'updates_disabled');self.assertFalse((self.root/'drain.json').exists());self.assertEqual(u.digest(self.home/'bin/acp-slack-adapter'),self.original)

class ServiceRecognition(unittest.TestCase):
    def test_only_fixed_installer_service_is_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);home=root/'installation';home.mkdir();config={'id':str(uuid.uuid4())};unit='acp-slack-adapter-'+config['id']+'.service';fragment=root/'.config/systemd/user'/unit;fragment.parent.mkdir(parents=True)
            def run(*args,**kwargs):
                if 'FragmentPath' in args:return str(fragment).encode()
                if 'DropInPaths' in args:return b''
                raise AssertionError('Unexpected service command')
            with patch.object(u.Path,'home',return_value=root),patch.object(u,'run',side_effect=run):
                for command in ['ExecStart='+u.quoted(home/'bin/acp-slack-adapter')+' serve --home '+u.quoted(home),'ExecStart='+str(home/'bin/acp-slack-adapter')+' serve --home '+str(home)]:
                    fragment.write_text(command+'\n');self.assertEqual(u.service(home,config),unit)
                for command in ['ExecStart=/bin/sh -c anything','ExecStart='+str(home/'bin/acp-slack-adapter')+' serve --home /other','ExecStart='+str(home/'bin/acp-slack-adapter')+' serve --home '+str(home)+' --extra']:
                    fragment.write_text(command+'\n')
                    with self.assertRaisesRegex(RuntimeError,'unrecognized_service_command'):u.service(home,config)

if __name__=='__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--worker':fake_worker(Path(sys.argv[2]),sys.argv[3],sys.argv[4])
    else:unittest.main(verbosity=2)
