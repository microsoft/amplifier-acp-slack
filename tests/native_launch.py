#!/usr/bin/env python3
"""Exercise an extracted native download using only its bundled runtime (no Slack/model calls)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time
import urllib.request
import urllib.error
import zipfile

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
                if p.poll() is not None:raise AssertionError('Native app exited during startup: '+(root/'server.log').read_text(errors='replace')[-1000:])
                try:
                    endpoint=json.loads((home/'endpoints.json').read_text());base=endpoint['base_url'];status,_,_=request(base,'/api/v1/auth/session')
                    if status==200:return p,base,log
                except (OSError,ValueError):pass
                time.sleep(.1)
            p.terminate();p.wait();log.close();raise AssertionError('Native app did not become ready')
        p,base,log=start()
        try:
            config=json.loads((home/'adapter.json').read_text());assert config['server']['auth_mode']=='local_password' and config['server']['web_transport']=='local_http'
            assert not config['enabled'] and not config['setup_complete'] and config['acp']['command']=='';checks.append('fresh native no-agent installation')
            status,_,session=request(base,'/api/v1/auth/session');assert status==200 and session['first_account_needed']
            status,_,_=request(base,'/api/v1/config');assert status==401
            status,_,_=request(base,'/api/v1/auth/session',headers={'Host':'evil.test','Origin':'http://evil.test'});assert status==403;checks.append('unauthenticated and cross-origin refused')
            cap=json.loads((home/'secrets/first-account').read_text())['token'];password='Disposable native password café 子'
            if browser:
                fixture=root/'browser-input.json';fixture.write_text(json.dumps({'base':base,'cap':cap,'password':password}))
                subprocess.run([shutil.which('node'),str(Path(__file__).with_name('native-browser.mjs')),str(fixture)],check=True,timeout=90)
                status,headers,session=request(base,'/api/v1/auth/login',{'username':config['server']['owner'],'password':password})
                checks.append('actual browser first-account and sign-in flow')
            else:status,headers,session=request(base,'/api/v1/auth/first-account',{'setup_token':cap,'password':password,'start_at_signin':False})
            assert status==200 and session['authenticated']
            encoded=(home/'secrets/local-password').read_bytes();assert encoded.startswith(b'$argon2id$') and password.encode() not in encoded;checks.append('create-only protected local account')
            config_bytes=(home/'adapter.json').read_bytes();password_hash=hashlib.sha256(encoded).hexdigest()
            reopened=subprocess.run([str(binary),'--home',str(home),'--no-browser'],env=env,capture_output=True,text=True,timeout=20);assert reopened.returncode==0,reopened.stderr;assert (home/'adapter.json').read_bytes()==config_bytes;checks.append('second open verifies existing identity without replacement')
            status_run=subprocess.run([str(binary),'--home',str(home),'api','GET','status'],env=env,capture_output=True,text=True,timeout=20);assert status_run.returncode==0,status_run.stderr;assert json.loads(status_run.stdout)['id']==config['id'];checks.append('same-connection listener proof before CLI authorization')
            private_python=home/'runtime/python'/('python.exe' if os.name=='nt' else 'bin/python3')
            py=subprocess.run([str(private_python),'-I','-B','-X','utf8','-c','import ssl,sqlite3,json; print(json.dumps({"isolated":__import__("sys").flags.isolated,"ssl":bool(ssl.OPENSSL_VERSION)}))'],env=env,capture_output=True,text=True,timeout=20);assert py.returncode==0,py.stderr;assert json.loads(py.stdout)=={'isolated':1,'ssl':True};checks.append('private Python starts without system tools')
            cookie=headers['set-cookie'].split(';',1)[0];status,_,_=request(base,'/api/v1/setup/slack',headers={'Cookie':cookie});assert status==200;checks.append('guided Slack setup status uses bundled runtime without network or credentials')
            if verify_startup:
                if os.name!='nt' and __import__('sys').platform!='darwin':raise AssertionError('Startup fixture is limited to native Windows/Mac CI')
                startup_headers={'Cookie':cookie,'X-CSRF-Token':session['csrf']};label='acp-slack-adapter-'+config['id']
                try:
                    # API uses PUT; the fixture preserves the exact caller method.
                    r=urllib.request.Request(base+'/api/v1/desktop/startup',data=json.dumps({'enabled':True}).encode(),method='PUT',headers={'Content-Type':'application/json',**startup_headers})
                    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(r,timeout=30) as response:assert json.load(response)['configured']
                    if os.name=='nt':
                        observed=subprocess.check_output([str(Path(os.environ['SystemRoot'])/'System32/schtasks.exe'),'/Query','/TN',label,'/XML'],text=True)
                        assert 'acp-slack-adapter.exe' in observed and str(home) in observed
                    else:
                        observed=(Path.home()/'Library/LaunchAgents'/(label+'.plist')).read_text();assert str(home) in observed and 'acp-slack-adapter' in observed
                    checks.append('actual current-user startup registration points to the preserved installation')
                finally:
                    r=urllib.request.Request(base+'/api/v1/desktop/startup',data=json.dumps({'enabled':False}).encode(),method='PUT',headers={'Content-Type':'application/json',**startup_headers})
                    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(r,timeout=30) as response:assert not json.load(response)['configured']
                checks.append('startup removal preserves installation and account')

            if verify_setup_tool:
                code="import importlib.util,pathlib,subprocess,sys; p=pathlib.Path(sys.argv[1]); spec=importlib.util.spec_from_file_location('setup',p/'secrets/slack-setup-helper.py'); m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); b=m.ensure_cli(p/'secrets/slack-setup'); r=subprocess.run([str(b),'version','--skip-update'],capture_output=True,text=True,timeout=30); assert r.returncode==0 and '4.12.0' in r.stdout, (r.returncode,r.stdout); print('Official Slack CLI 4.12.0 starts')"
                vendor_env={**env,'SLACK_SKIP_UPDATE':'true','NO_COLOR':'1'}
                verified=subprocess.run([str(private_python),'-I','-B','-X','utf8','-c',code,str(home)],env=vendor_env,capture_output=True,text=True,timeout=150)
                assert verified.returncode==0,verified.stderr
                checks.append('actual checksum-pinned official Slack CLI starts on this platform')

        finally:p.terminate();p.wait(timeout=20);log.close()
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
