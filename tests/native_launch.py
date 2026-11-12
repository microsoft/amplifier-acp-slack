#!/usr/bin/env python3
"""Exercise an extracted native download using only its bundled runtime (no Slack/model calls)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import urllib.error
import xml.etree.ElementTree as ET
import zipfile
import uuid

def run_startup_query(powershell,query,env):
    print('startup-inspection: before-powershell',file=sys.stderr,flush=True)
    try:
        return subprocess.run([str(powershell),'-NoProfile','-NonInteractive','-Command',query],env=env,check=True,capture_output=True,timeout=30).stdout
    except (subprocess.TimeoutExpired,subprocess.CalledProcessError) as error:
        # Fixed fixture diagnostics only, with no configuration or secret bytes.
        for stream in (error.stdout,error.stderr):
            if stream:print(stream.decode('utf-8',errors='replace') if isinstance(stream,bytes) else stream,file=sys.stderr,flush=True)
        raise

def startup_registration(home,label,env):
    """Read this fresh fixture's actual registration, refusing different owners."""
    assert label=='acp-slack-adapter-'+str(uuid.UUID(label.removeprefix('acp-slack-adapter-')))
    if os.name=='nt':
        query=r"""$ErrorActionPreference='Stop'
function Mark([string]$phase) { [Console]::Error.WriteLine('startup-inspection: ' + $phase); [Console]::Error.Flush() }
Mark 'script-entered'
function OwnerSid([string] $name) {
 if ([string]::IsNullOrWhiteSpace($name)) { throw 'Task owner missing' }
 if ($name.StartsWith('S-1-')) { return $name }
 return ([Security.Principal.NTAccount]::new($name)).Translate([Security.Principal.SecurityIdentifier]).Value
}
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
Mark 'before-service'
$s=New-Object -ComObject Schedule.Service
Mark 'before-connect'
$s.Connect()
Mark 'before-folder'
$folder=$s.GetFolder('\')
Mark 'before-query'
try { $t=$folder.GetTask($env:SLACK_FIXTURE_TASK) }
catch {
 Mark 'query-exception'
 $e=$_.Exception
 $seen=New-Object 'System.Collections.Generic.List[object]'
 for ($i=0; $i -lt 12; $i++) {
  foreach ($old in $seen) { if ([Object]::ReferenceEquals($old,$e)) { throw 'Startup inspection exception cycle' } }
  $seen.Add($e)
  Mark ('exception-' + $i + '-hresult-' + $e.HResult)
  if ($null -eq $e.InnerException) { break }
  if ($i -eq 11) { throw 'Startup inspection exception depth exceeded' }
  $e=$e.InnerException
 }
 if ($e.HResult -eq -2147024894) { Mark 'confirmed-absent'; @{present=$false}|ConvertTo-Json -Compress; Mark 'complete'; exit 0 }
 throw
}
Mark 'query-complete'
$d=$t.Definition
$owners=@(); foreach ($trigger in $d.Triggers) { if ($trigger.Type -ne 9) { throw 'Unexpected trigger' }; $owners += OwnerSid $trigger.UserId }
Mark 'before-result'
@{present=$true;xml=[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t.Xml));sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;principal_sid=(OwnerSid $d.Principal.UserId);trigger_sids=@($owners)}|ConvertTo-Json -Compress
Mark 'complete'
"""
        powershell=Path(os.environ['SystemRoot'])/'System32/WindowsPowerShell/v1.0/powershell.exe'
        value=json.loads(run_startup_query(powershell,query,{**env,'SLACK_FIXTURE_TASK':label}).decode('utf-8'))
        if value=={'present':False}:return None
        assert value['present'] is True and value['principal_sid']==value['sid'] and value['trigger_sids']==[value['sid']]
        observed=__import__('base64').b64decode(value['xml']).decode('utf-8')
        task=ET.fromstring(observed);ns={'t':'http://schemas.microsoft.com/windows/2004/02/mit/task'}
        principals=task.findall('./t:Principals/t:Principal',ns);assert len(principals)==1
        assert principals[0].findtext('t:LogonType',namespaces=ns)=='InteractiveToken'
        assert principals[0].findtext('t:RunLevel',namespaces=ns) in (None,'LeastPrivilege')
        actions=task.findall('./t:Actions/*',ns);assert len(actions)==1 and actions[0].tag=='{'+ns['t']+'}Exec'
        commands=actions[0].findall('t:Command',ns);arguments=actions[0].findall('t:Arguments',ns)
        assert len(commands)==len(arguments)==1
        assert os.path.samefile(commands[0].text,home/'bin/acp-slack-adapter.exe')
        invocation=arguments[0].text
        assert invocation.startswith('--home "') and invocation.endswith('" serve')
        assert os.path.samefile(invocation[len('--home "'):-len('" serve')],home)
        return observed
    path=Path.home()/'Library/LaunchAgents'/(label+'.plist')
    try:entry=path.lstat()
    except FileNotFoundError:return None
    assert stat.S_ISREG(entry.st_mode) and entry.st_uid==os.getuid()
    observed=path.read_bytes()
    value=plistlib.loads(observed)
    assert value['Label']==label
    args=value['ProgramArguments'];assert len(args)==4 and args[1]=='--home' and args[3]=='serve'
    assert os.path.samefile(args[0],home/'bin/acp-slack-adapter') and os.path.samefile(args[2],home)
    assert value['RunAtLoad'] is True
    return observed

def cleanup_fixture_startup(home,label,env):
    # Re-read ownership immediately before fallback deletion; never trust an old
    # snapshot or treat a failed inspection as an absent task.
    if startup_registration(home,label,env) is None:return
    if os.name=='nt':
        subprocess.run([str(Path(os.environ['SystemRoot'])/'System32/schtasks.exe'),'/Delete','/TN',label,'/F'],capture_output=True,check=True,timeout=30)
    else:
        (Path.home()/'Library/LaunchAgents'/(label+'.plist')).unlink()
    assert startup_registration(home,label,env) is None,'Fixture startup registration remains'

def verify_startup_removal(remove,read,cleanup):
    try:
        remove()
        assert read() is None,'Startup API reported removal but the registration remains'
    except Exception as error:
        try:cleanup()
        except Exception as cleanup_error:error.add_note('Fixture cleanup also failed: '+repr(cleanup_error))
        raise

def superseded_launch(binary,home,env):
    """A process carrying the prior launch nonce reaches main after recovery."""
    root=home/'native-updates';assert not root.exists()
    plan=str(uuid.uuid4());old_nonce=str(uuid.uuid4());folder=root/plan;folder.mkdir(parents=True)
    executable=home/'bin'/binary.name;sha=hashlib.sha256(executable.read_bytes()).hexdigest()
    record={'schema':1,'id':plan,'state':'rolling_back','action':'rollback','previous_sha256':sha,'launch':{'nonce':str(uuid.uuid4()),'mode':'rollback','binary_sha256':sha}}
    marker=root/'active.json';marker.write_text(json.dumps({'id':plan}),encoding='utf-8');journal=folder/'plan.json';journal.write_text(json.dumps(record),encoding='utf-8')
    before={p:p.read_bytes()for p in [journal,marker,home/'adapter.json',home/'state.sqlite3']}
    try:
        result=subprocess.run([str(executable),'--home',str(home),'serve'],env={**env,'ACP_SLACK_UPDATE_PLAN':plan,'ACP_SLACK_UPDATE_LAUNCH':old_nonce},capture_output=True,text=True,encoding='utf-8',timeout=10)
        assert result.returncode!=0 and 'Update launch was superseded' in result.stderr,(result.returncode,result.stderr)
        assert all(p.read_bytes()==data for p,data in before.items()),'Rejected old launch changed saved state'
        runtime=home/'runtime.json';previous_runtime=runtime.read_bytes()if runtime.exists()else None
        recovery=subprocess.Popen([str(executable),'--home',str(home),'serve'],env=env,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        try:
            base=json.loads((home/'endpoints.json').read_text(encoding='utf-8'))['base_url']
            for _ in range(100):
                try:
                    if request(base,'/api/v1/auth/session')[0]==200:break
                except (OSError,ValueError):pass
                assert recovery.poll()is None,'Management recovery process exited';time.sleep(.1)
            else:raise AssertionError('Management recovery did not become available')
            headers={'Authorization':'Bearer '+(home/'secrets/control-token').read_text(encoding='utf-8').strip()}
            code,_,status=request(base,'/api/v1/status',headers=headers)
            assert code==200 and status['runtime']['state']=='stopped'and 'recovery' in status['runtime']['reason']
            code,_,update=request(base,'/api/v1/desktop/updates',headers=headers)
            assert code==200 and update['current']['id']==plan
            journal.write_bytes(b'{damaged');code,_,update=request(base,'/api/v1/desktop/updates',headers=headers)
            assert code==200 and update['journal_error']and update['current']is None
            assert journal.read_bytes()==b'{damaged'
            assert (runtime.read_bytes()if runtime.exists()else None)==previous_runtime,'Recovery management started the Slack runtime'
        finally:recovery.terminate();recovery.wait(timeout=20)
    finally:shutil.rmtree(root)

def extract(package,folder):
    if package.name.endswith('.zip'):
        with zipfile.ZipFile(package) as source:
            for item in source.infolist():
                name=Path(item.filename)
                if name.is_absolute() or '..' in name.parts:raise ValueError('Unsafe member')
                dest=folder/name
                if item.is_dir():dest.mkdir(parents=True,exist_ok=True);continue
                dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(source.read(item));dest.chmod((item.external_attr>>16)&0o777 or 0o644)
    else:
        with tarfile.open(package) as source:source.extractall(folder,filter='data')
def request(base,path,body=None,headers=None):
    data=None if body is None else json.dumps(body).encode()
    r=urllib.request.Request(base+path,data=data,headers={'Content-Type':'application/json',**(headers or {})})
    try:response=urllib.request.build_opener(urllib.request.ProxyHandler({})).open(r,timeout=20)
    except urllib.error.HTTPError as e:response=e
    with response:return response.status,{k.lower():v for k,v in response.headers.items()},json.load(response)
def run(package,result,browser=False,verify_setup_tool=False,verify_startup=False):
    checks=[]
    with tempfile.TemporaryDirectory(prefix='Slack native café 子 ') as temporary:
        root=Path(temporary);bundle=root/'download';bundle.mkdir();extract(package,bundle)
        binary=next(p for p in bundle.rglob('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter') if p.is_file())
        binary.chmod(0o755);home=root/'private connection';env={k:v for k,v in os.environ.items() if not k.upper().startswith(('SLACK_','OPENAI_','ANTHROPIC_','AZURE_','GOOGLE_','AMPLIFIER_'))};env['PATH']=str(root/'no-tools');env['PYTHONDONTWRITEBYTECODE']='1';env['USER']='Native café 子';env['USERNAME']='Native café 子'
        if verify_startup:env['PATH']=str(Path(os.environ['SystemRoot'])/'System32') if os.name=='nt' else '/usr/bin:/bin'
        def start():
            log=open(root/'server.log','ab');p=subprocess.Popen([str(binary),'--home',str(home),'--no-browser'],env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log)
            for _ in range(400):
                if p.poll() is not None:raise AssertionError('Native app exited during startup: '+(root/'server.log').read_text(encoding='utf-8',errors='replace')[-1000:])
                try:
                    endpoint=json.loads((home/'endpoints.json').read_text(encoding='utf-8'));base=endpoint['base_url'];status,_,_=request(base,'/api/v1/auth/session')
                    if status==200:return p,base,log
                except (OSError,ValueError):pass
                time.sleep(.1)
            p.terminate();p.wait();log.close();raise AssertionError('Native app did not become ready')
        p,base,log=start()
        try:
            config=json.loads((home/'adapter.json').read_text(encoding='utf-8'));assert config['server']['auth_mode']=='local_password' and config['server']['web_transport']=='local_http'
            assert not config['enabled'] and not config['setup_complete'] and config['acp']['command']=='';checks.append('fresh native no-agent installation')
            status,_,session=request(base,'/api/v1/auth/session');assert status==200 and session['first_account_needed']
            status,_,_=request(base,'/api/v1/config');assert status==401
            status,_,_=request(base,'/api/v1/auth/session',headers={'Host':'evil.test','Origin':'http://evil.test'});assert status==403;checks.append('unauthenticated and cross-origin refused')
            cap=json.loads((home/'secrets/first-account').read_text(encoding='utf-8'))['token'];password='Disposable native password café 子'
            if browser:
                fixture=root/'browser-input.json';fixture.write_text(json.dumps({'base':base,'cap':cap,'password':password}))
                subprocess.run([shutil.which('node'),str(Path(__file__).with_name('native-browser.mjs')),str(fixture)],check=True,timeout=90)
                status,headers,session=request(base,'/api/v1/auth/login',{'username':config['server']['owner'],'password':password})
                checks.append('actual browser first-account and sign-in flow')
            else:status,headers,session=request(base,'/api/v1/auth/first-account',{'setup_token':cap,'password':password,'start_at_signin':False})
            assert status==200 and session['authenticated']
            encoded=(home/'secrets/local-password').read_bytes();assert encoded.startswith(b'$argon2id$') and password.encode() not in encoded;checks.append('create-only protected local account')
            config_bytes=(home/'adapter.json').read_bytes();password_hash=hashlib.sha256(encoded).hexdigest()
            reopened=subprocess.run([str(binary),'--home',str(home),'--no-browser'],env=env,capture_output=True,text=True,encoding='utf-8',timeout=20);assert reopened.returncode==0,reopened.stderr;assert (home/'adapter.json').read_bytes()==config_bytes;checks.append('second open verifies existing identity without replacement')
            status_run=subprocess.run([str(binary),'--home',str(home),'api','GET','status'],env=env,capture_output=True,text=True,encoding='utf-8',timeout=20);assert status_run.returncode==0,status_run.stderr;assert json.loads(status_run.stdout)['id']==config['id'];checks.append('same-connection listener proof before CLI authorization')
            private_python=home/'runtime/python'/('python.exe' if os.name=='nt' else 'bin/python3')
            py=subprocess.run([str(private_python),'-I','-B','-X','utf8','-c','import ssl,sqlite3,json; print(json.dumps({"isolated":__import__("sys").flags.isolated,"ssl":bool(ssl.OPENSSL_VERSION)}))'],env=env,capture_output=True,text=True,encoding='utf-8',timeout=20);assert py.returncode==0,py.stderr;assert json.loads(py.stdout)=={'isolated':1,'ssl':True};checks.append('private Python starts without system tools')
            cookie=headers['set-cookie'].split(';',1)[0];status,_,_=request(base,'/api/v1/setup/slack',headers={'Cookie':cookie});assert status==200;checks.append('guided Slack setup status uses bundled runtime without network or credentials')
            if verify_startup:
                if os.name!='nt' and __import__('sys').platform!='darwin':raise AssertionError('Startup fixture is limited to native Windows/Mac CI')
                startup_headers={'Cookie':cookie,'X-CSRF-Token':session['csrf']};label='acp-slack-adapter-'+config['id']
                assert startup_registration(home,label,env) is None,'Fresh fixture must not touch an existing startup entry'
                startup_error=None
                def startup_request(enabled):
                    r=urllib.request.Request(base+'/api/v1/desktop/startup',data=json.dumps({'enabled':enabled}).encode(),method='PUT',headers={'Content-Type':'application/json',**startup_headers})
                    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(r,timeout=30) as response:assert json.load(response)['configured'] is enabled
                try:
                    startup_request(True)
                    observed=startup_registration(home,label,env);assert observed is not None
                    startup_request(True)
                    assert startup_registration(home,label,env)==observed,'Idempotent enable changed the native registration'
                    checks.append('actual owner-only startup registration, native readback and idempotent enable preserve installation')
                except BaseException as error:
                    startup_error=error
                    raise
                finally:
                    try:
                        verify_startup_removal(lambda:startup_request(False),lambda:startup_registration(home,label,env),lambda:cleanup_fixture_startup(home,label,env))
                    except Exception as removal_error:
                        if startup_error is None:raise
                        startup_error.add_note('Startup removal also failed: '+repr(removal_error))
                        for note in getattr(removal_error,'__notes__',[]):startup_error.add_note(note)
                assert (home/'adapter.json').read_bytes()==config_bytes and hashlib.sha256((home/'secrets/local-password').read_bytes()).hexdigest()==password_hash
                checks.append('independent native absence confirms startup removal preserves installation and account')

            if verify_setup_tool:
                code="import importlib.util,pathlib,subprocess,sys; p=pathlib.Path(sys.argv[1]); spec=importlib.util.spec_from_file_location('setup',p/'secrets/slack-setup-helper.py'); m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); b=m.ensure_cli(p/'secrets/slack-setup'); r=subprocess.run([str(b),'version','--skip-update'],capture_output=True,text=True,encoding='utf-8',timeout=30); assert r.returncode==0 and '4.12.0' in r.stdout, (r.returncode,r.stdout); print('Official Slack CLI 4.12.0 starts')"
                vendor_env={**env,'SLACK_SKIP_UPDATE':'true','NO_COLOR':'1'}
                verified=subprocess.run([str(private_python),'-I','-B','-X','utf8','-c',code,str(home)],env=vendor_env,capture_output=True,text=True,encoding='utf-8',timeout=150)
                assert verified.returncode==0,verified.stderr
                checks.append('actual checksum-pinned official Slack CLI starts on this platform')

        finally:p.terminate();p.wait(timeout=20);log.close()
        superseded_launch(binary,home,env);checks.extend(['actual superseded replacement refuses startup before state, listener or Slack runtime','management recovery remains available without Slack runtime; damaged journal is reported and preserved'])
        p,base,log=start()
        try:
            assert (home/'adapter.json').read_bytes()==config_bytes
            status,_,session=request(base,'/api/v1/auth/login',{'username':config['server']['owner'],'password':password});assert status==200 and session['authenticated']
            assert hashlib.sha256((home/'secrets/local-password').read_bytes()).hexdigest()==password_hash;checks.append('restart retains account, identity, configuration and disabled state')
        finally:p.terminate();p.wait(timeout=20);log.close()
        # Seed a fictional, already configured paused connection while stopped.
        # This replaces the legacy Linux-only system-Python fixture on native OSes.
        existing=json.loads(config_bytes);existing['setup_complete']=True
        existing['slack'].update(team_id='TNATIVEFIXTURE',app_id='ANATIVEFIXTURE',loopback_test=True,api_base='http://127.0.0.1:1/')
        (home/'adapter.json').write_text(json.dumps(existing),encoding='utf-8')
        credentials=home/'secrets/slack.json'
        with open(credentials,'x',opener=lambda p,f:os.open(p,f,0o600)) as stream:json.dump({'app_token':'test-app','bot_token':'test-bot'},stream)
        paused_config=(home/'adapter.json').read_bytes();paused_credentials=credentials.read_bytes()
        p,base,log=start()
        try:
            status,headers,session=request(base,'/api/v1/auth/login',{'username':existing['server']['owner'],'password':password});assert status==200
            cookie=headers['set-cookie'].split(';',1)[0]
            status,_,connected=request(base,'/api/v1/setup/slack',{'action':'connect'},{'Cookie':cookie,'X-CSRF-Token':session['csrf']})
            assert status==200 and connected['connection_reused'] and connected['connection_ready'] and not connected['connection_running'] and connected['next_action']=='done',connected
            assert (home/'adapter.json').read_bytes()==paused_config and credentials.read_bytes()==paused_credentials
            assert not (home/'secrets/slack-setup/project').exists()
            checks.append('native private helper reuses an existing paused Slack connection without app creation, engine launch or configuration changes')
        finally:p.terminate();p.wait(timeout=20);log.close()
        if not verify_startup:assert not (home/'startup.json').exists();checks.append('startup false does not register an OS service')
    receipt={'passed':True,'checks':checks,'package':str(package),'package_sha256':hashlib.sha256(package.read_bytes()).hexdigest(),'limitations':['No real Slack authorization or model/provider call','No publisher signing/notarization claim','Current-user startup registration exercised only when verify_startup is enabled; automatic sign-in launch is not simulated']}
    if result:result.write_text(json.dumps(receipt,indent=2)+'\n')
    print(json.dumps(receipt,indent=2))
if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--package',required=True,type=Path);p.add_argument('--result',type=Path);p.add_argument('--browser',action='store_true');p.add_argument('--verify-setup-tool',action='store_true');p.add_argument('--verify-startup',action='store_true');a=p.parse_args();run(a.package.resolve(),a.result,a.browser,a.verify_setup_tool,a.verify_startup)
