#!/usr/bin/env python3
"""Exercise an extracted native download using only its bundled runtime (no Slack/model calls)."""
import argparse
import base64
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
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

def exact_path_identity(first,second):
    """Same resolved path AND file; spelling normalization is Windows-only."""
    a,b=Path(first).resolve(strict=True),Path(second).resolve(strict=True)
    def key(path):
        value=str(path)
        if os.name=='nt':
            if value.startswith('\\\\?\\UNC\\'):value='\\\\'+value[8:]
            elif value.startswith('\\\\?\\'):value=value[4:]
            value=os.path.normcase(value)
        return value
    return key(a)==key(b) and os.path.samefile(a,b)

def run_startup_query(powershell,query,env):
    print('startup-inspection: before-powershell',file=sys.stderr,flush=True)
    try:
        return subprocess.run([str(powershell),'-NoProfile','-NonInteractive','-Command',query],env=env,check=True,capture_output=True,timeout=30).stdout
    except (subprocess.TimeoutExpired,subprocess.CalledProcessError) as error:
        # Fixed fixture diagnostics only, with no configuration or secret bytes.
        for stream in (error.stdout,error.stderr):
            if stream:print(stream.decode('utf-8',errors='replace') if isinstance(stream,bytes) else stream,file=sys.stderr,flush=True)
        raise

def parse_startup_observation(text):
    """Fixed .NET frame; do not depend on PowerShell module/JSON discovery."""
    fields=text.splitlines()
    if fields==['slack-startup-v1','absent']:return {'present':False}
    if len(fields)!=6 or fields[:2]!=['slack-startup-v1','present']:
        raise ValueError('Invalid startup observation framing')
    xml=base64.b64decode(fields[2],validate=True)
    if not xml or base64.b64encode(xml).decode('ascii')!=fields[2]:
        raise ValueError('Invalid startup observation XML encoding')
    xml.decode('utf-8')
    for sid in fields[3:]:
        if not re.fullmatch(r'S-1-[0-9]+(?:-[0-9]+)+',sid):
            raise ValueError('Invalid startup observation SID')
    return {'present':True,'xml':fields[2],'sid':fields[3],'principal_sid':fields[4],'trigger_sids':[fields[5]]}

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
Mark 'before-service-type'
$serviceType=[Type]::GetTypeFromProgID('Schedule.Service',$true)
Mark 'before-service-create'
$s=[Activator]::CreateInstance($serviceType)
Mark 'before-connect'
$s.Connect()
Mark 'before-folder'
$folder=$s.GetFolder('\')
Mark 'before-query'
try { $t=$folder.GetTask($env:SLACK_FIXTURE_TASK) }
catch {
 Mark 'query-exception'
 $e=$_.Exception
 $seen=[Collections.Generic.List[object]]::new()
 for ($i=0; $i -lt 12; $i++) {
  foreach ($old in $seen) { if ([Object]::ReferenceEquals($old,$e)) { throw 'Startup inspection exception cycle' } }
  $seen.Add($e)
  Mark ('exception-' + $i + '-hresult-' + $e.HResult)
  if ($null -eq $e.InnerException) { break }
  if ($i -eq 11) { throw 'Startup inspection exception depth exceeded' }
  $e=$e.InnerException
 }
 if ($e.HResult -eq -2147024894) { Mark 'confirmed-absent'; [Console]::Out.WriteLine("slack-startup-v1`nabsent"); [Console]::Out.Flush(); Mark 'complete'; exit 0 }
 throw
}
Mark 'query-complete'
$d=$t.Definition
$owners=@(); foreach ($trigger in $d.Triggers) { if ($trigger.Type -ne 9) { throw 'Unexpected trigger' }; $owners += OwnerSid $trigger.UserId }
if ($owners.Count -ne 1) { throw 'Expected one owned logon trigger' }
Mark 'before-result'
$fields=[string[]]@('slack-startup-v1','present',[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t.Xml)),[Security.Principal.WindowsIdentity]::GetCurrent().User.Value,(OwnerSid $d.Principal.UserId),$owners[0])
[Console]::Out.WriteLine([String]::Join("`n",$fields))
[Console]::Out.Flush()
Mark 'complete'
"""
        powershell=Path(os.environ['SystemRoot'])/'System32/WindowsPowerShell/v1.0/powershell.exe'
        value=parse_startup_observation(run_startup_query(powershell,query,{**env,'SLACK_FIXTURE_TASK':label}).decode('utf-8'))
        if value=={'present':False}:return None
        assert value['present'] is True and value['principal_sid']==value['sid'] and value['trigger_sids']==[value['sid']]
        observed=base64.b64decode(value['xml'],validate=True).decode('utf-8')
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

def fresh_setup_status(home,base,headers):
    """Status must not launch or materialize the setup helper."""
    script=home/'secrets/slack-setup-helper.py';folder=home/'secrets/slack-setup'
    assert not os.path.lexists(script) and not os.path.lexists(folder),'Expected untouched fresh setup'
    status,_,value=request(base,'/api/v1/setup/slack',headers=headers)
    assert status==200 and value['authorized'] is False and value['authorization_pending'] is False
    assert value['setup_complete'] is False and value['connection_running'] is False
    assert not os.path.lexists(script) and not os.path.lexists(folder),'Read-only setup GET materialized a helper or directory'
    return value

def prepare_fresh_setup_tool(home,base,headers):
    """One explicit local discard on this empty fixture materializes its helper.

    There is no approval, ticket, project or app to discard. The production
    action starts no Slack authorization, CLI download or external request.
    An uncertain reply is fatal; no fallback helper copy or action replay.
    """
    script=home/'secrets/slack-setup-helper.py';folder=home/'secrets/slack-setup'
    assert not os.path.lexists(script) and not os.path.lexists(folder),'Setup tool preparation requires untouched fixture state'
    config_path=home/'adapter.json';before=config_path.read_bytes();config=json.loads(before)
    assert config['enabled'] is False and config['setup_complete'] is False and not config['slack']['app_id']
    protected={p:p.read_bytes() for p in (home/'secrets').rglob('*') if p.is_file()}
    status,_,value=request(base,'/api/v1/setup/slack',{'action':'discard'},headers=headers)
    assert status==200 and value['authorized'] is False and value['authorization']=='missing'
    assert value['setup_complete'] is False and value['connection_running'] is False
    assert config_path.read_bytes()==before and all(p.read_bytes()==saved for p,saved in protected.items()),'Local fixture preparation changed existing configuration/account state'
    assert script.is_file() and not script.is_symlink()
    assert script.read_bytes()==(Path(__file__).resolve().parents[1]/'scripts/slack_setup.py').read_bytes(),'Materialized helper differs from exact compiled source'
    assert folder.is_dir() and not folder.is_symlink()
    assert not any(os.path.lexists(folder/name) for name in ('auth','pending-auth','pending.json','project','issued.json','bin','cli.json')),'Fresh local discard created authorization or external-tool state'
    # The real saved reader must accept the exact directories made by the
    # explicit helper. A successful helper reply alone does not prove this.
    materialized={p:p.read_bytes() for p in (home/'secrets').rglob('*') if p.is_file()}
    status,_,saved=request(base,'/api/v1/setup/slack',headers=headers)
    assert status==200 and saved['authorized'] is False and saved['authorization']=='missing' and saved['authorization_pending'] is False,'Post-discard saved setup read failed'
    assert saved['setup_complete'] is False and saved['connection_running'] is False
    assert config_path.read_bytes()==before and materialized=={p:p.read_bytes() for p in (home/'secrets').rglob('*') if p.is_file()},'Read-only post-discard status changed retained setup/account state'

def finite_setup(binary,root,env):
    """Actual create-only entry point; no server, browser, Slack or model."""
    home=root/'finite  setup café 子'
    command=[str(binary),'--home',str(home),'native-setup']
    created=subprocess.run(command,env=env,capture_output=True,text=True,encoding='utf-8',timeout=60)
    assert created.returncode==0,created.stderr
    receipt=json.loads(created.stdout)
    config=json.loads((home/'adapter.json').read_text(encoding='utf-8'))
    assert receipt['schema']=='com.microsoft.amplifier.slack-native-setup/1'
    assert receipt['created'] is True and receipt['serving'] is False
    assert receipt['id']==config['id'] and exact_path_identity(receipt['home'],home)
    assert Path(receipt['executable']).is_file()
    assert receipt['management']=={'kind':'installation-local-cli','api_version':'acp-slack-adapter.management/1','relative_api_paths':True,'requires_running_connector':True}
    assert not config['enabled'] and not config['setup_complete'] and not config['acp']['command']
    assert not (home/'endpoints.json').exists() and not (home/'startup.json').exists()
    assert not (home/'secrets/local-password').exists()
    for secret in ['first-account','control-token']:
        raw=(home/'secrets'/secret).read_text(encoding='utf-8')
        value=json.loads(raw)['token'] if secret=='first-account' else raw.strip()
        assert value and value not in created.stdout and value not in created.stderr
    def snapshot():
        return {str(p.relative_to(home)):hashlib.sha256(p.read_bytes()).hexdigest() for p in home.rglob('*') if p.is_file()}
    before=snapshot()
    refused=subprocess.run(command,env=env,capture_output=True,text=True,encoding='utf-8',timeout=20)
    assert refused.returncode!=0 and snapshot()==before
    occupied=root/'existing unrelated folder';occupied.mkdir();(occupied/'owner-data').write_bytes(b'preserve me')
    refused=subprocess.run([str(binary),'--home',str(occupied),'native-setup'],env=env,capture_output=True,text=True,encoding='utf-8',timeout=20)
    assert refused.returncode!=0 and list(occupied.iterdir())==[occupied/'owner-data']
    assert (occupied/'owner-data').read_bytes()==b'preserve me'
    return ['finite native setup exits without starting service or exposing credentials','repeated native setup preserves exact existing installation','native setup refuses unrelated existing folder']

@contextmanager
def retained_fixture_root():
    """A failed/unknown native child must not race automatic temp removal."""
    root=Path(tempfile.mkdtemp(prefix='Slack native café 子 ')).resolve(strict=True)
    try:
        yield root
    except BaseException:
        print('Native fixture retained after failure: '+str(root),file=sys.stderr,flush=True)
        raise
    else:
        shutil.rmtree(root)

def verify_packaged_hooks(root,helper,result,env):
    """Actual native shell only; this is not an actual CLI deploy/auth chain."""
    home=root/'finite  setup café 子'
    python=home/'runtime/python'/('python.exe' if os.name=='nt' else 'bin/python3')
    saved=root/'native-hook-result.json'
    command=[str(python),'-I','-B','-X','utf8',str(Path(__file__).with_name('slack_hook_process.py')),
             '--home',str(home),'--helper',str(helper),'--helper-sha256',hashlib.sha256(helper.read_bytes()).hexdigest(),
             '--result',str(saved)]
    checkpoint={'passed':False,'phase':'packaged_native_hook_contract','retained_fixture_root':str(root),
                'limits':['Python substitutes for the Go parent; not an actual Slack CLI hook/auth/deploy chain',
                          'Windows visible-console sampling is bounded, and new windows are not attributed to the child tree']}
    def save():
        if saved.exists():
            try:
                if saved.stat().st_size>1024*1024:raise ValueError('Hook receipt bound')
                checkpoint['native_hook_contract']=json.loads(saved.read_text(encoding='utf-8'))
            except (OSError,ValueError):checkpoint['native_hook_readback']='unavailable_or_incomplete'
        if result:result.write_text(json.dumps(checkpoint,indent=2)+'\n',encoding='utf-8')
    save()
    try:
        # Match production's immediate hidden CLI boundary. The probe launches
        # PowerShell with upstream default flags, without hiding its descendants.
        with (root/'native-hook-stdout').open('xb') as out,(root/'native-hook-stderr').open('xb') as err:
            child=subprocess.Popen(command,env=env,stdin=subprocess.DEVNULL,stdout=out,stderr=err,
                                   creationflags=subprocess.CREATE_NO_WINDOW if os.name=='nt' else 0)
            checkpoint['probe_pid']=child.pid;save()
            try:child.wait(timeout=90)
            except subprocess.TimeoutExpired:
                checkpoint['outcome']='unknown_timeout_no_retry_no_kill';raise
        def bounded_output(path):
            with path.open('rb') as stream:raw=stream.read(1024*1024+1)
            assert len(raw)<=1024*1024,'Native hook output exceeds bound'
            return raw
        stdout,stderr=(bounded_output(root/name) for name in ('native-hook-stdout','native-hook-stderr'))
        checkpoint.update(probe_returncode=child.returncode,stdout_sha256=hashlib.sha256(stdout).hexdigest(),
                          stderr_sha256=hashlib.sha256(stderr).hexdigest())
        save()
        assert child.returncode==0 and checkpoint.get('native_hook_contract',{}).get('passed') is True,'Native hook contract failed; exact checkpoint retained'
        return checkpoint['native_hook_contract']
    finally:
        if sys.exc_info()[0] is None:save()
        else:
            try:save()
            except BaseException:pass  # Preserve the original failure/unknown.

def manager_record_projection(value):
    """Content-free identity/phase evidence; never copy arbitrary record fields."""
    def digest(item):return hashlib.sha256(json.dumps(item,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    def canonical_id(value):
        try:return isinstance(value,str) and str(uuid.UUID(value))==value
        except ValueError:return False
    result={}
    for key in ('id','request_id','start_request_id','update_plan'):
        if canonical_id(value.get(key)):result[key]=value[key]
    for key in ('schema','process_absent'):
        if type(value.get(key)) in (int,bool):result[key]=value[key]
    enums={'phase':{'registering','registered','registration_refused','start_intent','accepted','refused','unknown','ready','observing_existing','intent','inactive','retired'},
           'action':{'stop','retire'},'dispatch_outcome':{'unconfirmed','accepted','refused','unknown'}}
    for key,allowed in enums.items():
        if isinstance(value.get(key),str):result[key]=value[key] if value[key] in allowed else 'unrecognized'
    for key in ('binding','profile','service','before','outcome','observation'):
        if key in value:result[key+'_sha256']=digest(value[key])
    binding=value.get('binding')
    if isinstance(binding,dict):
        result['binding_id']=binding['id'] if canonical_id(binding.get('id')) else None
        sha=binding.get('binary_sha256')
        if isinstance(sha,str) and re.fullmatch('[0-9a-f]{64}',sha):result['binary_sha256']=sha
    for key in ('process','process_identity'):
        process=value.get(key)
        if process is None and key in value:result[key]=None
        if not isinstance(process,dict):continue
        observed={}
        for field in ('pid','session_id'):
            if type(process.get(field)) is int and 0<=process[field]<=2**32-1:observed[field]=process[field]
        for field in ('start_ticks','creation_time_100ns','start_seconds','start_microseconds','boot_seconds','boot_microseconds'):
            raw=process.get(field)
            if isinstance(raw,str) and re.fullmatch('[0-9]{1,24}',raw):observed[field]=raw
        if process.get('platform') in ('windows','macos'):observed['platform']=process['platform']
        if canonical_id(process.get('boot_id')):observed['boot_id']=process['boot_id']
        result[key]=observed
    return result

def managed_failure_records(home,expected_home,events):
    """One read-only snapshot of this fixture's issued fixed manager records.

    This is not a fresh process/manager query or a settlement observation. Reads
    are not retried. Missing, unsafe, changing and unreadable records stay distinct.
    """
    result={'schema':1,'read_only':True,'scope':'saved records only; not atomic or current process liveness','observation_monotonic':time.monotonic(),'records':{}}
    def identity(info):return (info.st_dev,info.st_ino)
    def ordinary(info,folder=False):
        return (stat.S_ISDIR(info.st_mode) if folder else stat.S_ISREG(info.st_mode)) and not getattr(info,'st_file_attributes',0)&getattr(stat,'FILE_ATTRIBUTE_REPARSE_POINT',0x400)
    if expected_home is None:
        result['unavailable']='fresh_home_identity_not_recorded';return result
    try:
        current=home.lstat()
        if not ordinary(current,True) or identity(current)!=expected_home:
            result['unavailable']='fresh_home_identity_changed';return result
    except OSError as error:
        result['unavailable']='home_read_error';result['errno']=error.errno;return result
    paths=['native-manager/active.json','native-manager/enrollment.json','native-manager/control.json']
    # These IDs come only from commands already issued by the fixture. Neither
    # pointers nor directory enumeration can expand the collection scope.
    for event in events[:16]:
        action,args=event.get('action'),event.get('args',[])
        if action not in ('native-start','native-stop','native-retire') or len(args)!=2 or args[0]!='--request-id':continue
        try:
            if str(uuid.UUID(args[1]))!=args[1]:continue
        except (ValueError,TypeError,AttributeError):continue
        if action=='native-start':paths.extend([f'native-manager/requests/{args[1]}.json',f'native-manager/requests/{args[1]}.launched.json'])
        else:paths.append(f'native-manager/controls/{args[1]}.json')
    result['truncated']=len(events)>16 or len(paths)>16
    for relative in dict.fromkeys(paths[:16]):
        row={};result['records'][relative]=row
        try:
            if identity(home.lstat())!=expected_home:raise ValueError('home changed')
            path=home
            for component in Path(relative).parts[:-1]:
                path=path/component
                if not ordinary(path.lstat(),True):raise ValueError('unsafe directory')
            path=home/relative;before=path.lstat()
            if not ordinary(before) or before.st_nlink!=1:raise ValueError('unsafe file')
            if before.st_size>65536:
                row.update(state='oversize',size=before.st_size);continue
            flags=os.O_RDONLY|getattr(os,'O_BINARY',0)|getattr(os,'O_NOFOLLOW',0)
            with os.fdopen(os.open(path,flags),'rb') as stream:
                opened=os.fstat(stream.fileno())
                if not ordinary(opened) or identity(opened)!=identity(before):raise ValueError('opened identity changed')
                raw=stream.read(65537);after=os.fstat(stream.fileno())
            row['size']=len(raw)
            if len(raw)>65536:row['state']='oversize';continue
            row['sha256']=hashlib.sha256(raw).hexdigest()
            if identity(path.lstat())!=identity(before) or identity(home.lstat())!=expected_home or (after.st_size,after.st_mtime_ns)!=(before.st_size,before.st_mtime_ns):
                row['state']='changed_during_read';continue
            try:value=json.loads(raw)
            except (ValueError,UnicodeError):row['state']='malformed';continue
            if not isinstance(value,dict):row['state']='invalid_record';continue
            row.update(state='observed',projection=manager_record_projection(value))
        except FileNotFoundError:row['state']='absent_at_read'
        except OSError as error:row.update(state='read_error',error_type=type(error).__name__,errno=error.errno)
        except ValueError:row['state']='unsafe_or_changed'
    return result

def retain_managed_failure(receipt,home,expected_home,events,save):
    # Diagnostics and receipt persistence must never replace the original
    # action exception, including interruption while collecting evidence.
    try:receipt['manager_records']=managed_failure_records(home,expected_home,events)
    except BaseException as error:receipt['manager_records']={'unavailable':'diagnostic_error','error_type':type(error).__name__}
    try:save()
    except BaseException as error:receipt['diagnostic_save_error']=type(error).__name__

def managed_lifecycle(binary,env,result=None):
    """Actual fixed OS profile, separate from the legacy startup-toggle proof.

    Each native action keeps its 60-second owner bound; 65 seconds is only the
    CLI launch/serialization/reap envelope. There is no action retry. An unknown
    result retains this fresh home and any registration for inspection.
    """
    root=Path(tempfile.mkdtemp(prefix='Slack managed café 子 ')).resolve()
    home=root/'installation';events=[];home_identity=None
    receipt={'schema':1,'scope':'actual fresh fixed-manager lifecycle','home':str(home),'passed':False,'events':events,'external_authorization':False,'model_calls':0,'action_retry':False}
    target=result.with_name(result.name.replace('-native-launch-result.json','-native-manager-result.json')) if result else root/'manager-result.json'
    def save():
        if target.parent.exists():target.write_text(json.dumps(receipt,indent=2)+'\n',encoding='utf-8')
    def call(exe,action,*args,timeout=65):
        at=time.monotonic();proc=subprocess.Popen([str(exe),'--home',str(home),action,*args],env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        row={'action':action,'args':list(args),'caller_pid':proc.pid,'started_monotonic':at};events.append(row);save()
        try:out,err=proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill();out,err=proc.communicate(timeout=5);row.update(elapsed=time.monotonic()-at,caller_exit=proc.returncode,outcome='unknown');save();raise
        row.update(elapsed=time.monotonic()-at,caller_exit=proc.returncode,stdout=out[:65536].decode('utf-8',errors='replace'),stderr=err[:8192].decode('utf-8',errors='replace'));save()
        assert proc.returncode==0,(action,proc.returncode,row['stderr'])
        assert len(out)<=65536 and len(err)<=8192,'Managed fixture output exceeded bound'
        value=json.loads(out);row['result']=value;save();return value
    def exact(value):
        assert value['schema']=='com.microsoft.amplifier.slack-native-status/1'
        assert value['id']==setup['id'] and exact_path_identity(value['home'],home)
        assert exact_path_identity(value['executable'],installed)
        assert value['autostart'] is False and value['startup_change_supported'] is False
    def ready(value,request,process=None):
        exact(value)
        assert value['state']=='ready' and value['profile_matches'] is True
        assert value['request_current'] is True and value['launch_observed'] is True
        assert value['request']['request_id']==request and value['pid']==value['process_identity']['pid']
        assert value['process_identity']['pid']>0 and len(value['process_identity'])>=3
        if process is not None:assert value['process_identity']==process,'Managed generation changed after caller exit'
        return value['process_identity']
    def stopped(value,request,start,process,phase='inactive'):
        exact(value);assert value['state']==phase and value['control_current'] is True
        c=value['control'];assert c['request_id']==request and c['start_request_id']==start
        assert c['action']==('retire' if phase=='retired' else 'stop') and c['phase']==phase
        assert c['process_identity']==process and c['process_absent'] is True
    def protected():
        paths=[home/'adapter.json',home/'native-installation.json']+sorted((home/'secrets').rglob('*'))
        return {str(p.relative_to(home)):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths if p.is_file()}
    def console_probe(process):
        if os.name!='nt':return {'applicable':False,'reason':'Windows console observation'}
        # A separate no-console observer attaches only long enough to establish
        # whether this exact live GUI-subsystem generation has a console.
        code="""import ctypes,json,sys
k=ctypes.WinDLL('kernel32',use_last_error=True)
k.FreeConsole()
ok=bool(k.AttachConsole(int(sys.argv[1])))
error=0 if ok else ctypes.get_last_error()
if ok:k.FreeConsole()
print(json.dumps({'attached':ok,'error':error}))
"""
        observed=subprocess.run([sys.executable,'-I','-B','-c',code,str(process['pid'])],env=env,stdin=subprocess.DEVNULL,capture_output=True,timeout=10,creationflags=subprocess.CREATE_NO_WINDOW)
        assert observed.returncode==0,observed.stderr
        proof=json.loads(observed.stdout);assert proof=={'attached':False,'error':6},proof
        return proof
    try:
        setup=call(binary,'native-setup',timeout=60);assert setup['created'] is True and setup['serving'] is False
        home_entry=home.lstat();assert stat.S_ISDIR(home_entry.st_mode) and not home.is_symlink()
        home_identity=(home_entry.st_dev,home_entry.st_ino)
        installed=Path(setup['executable']);assert installed.is_file()
        initial_config=json.loads((home/'adapter.json').read_text(encoding='utf-8'));assert initial_config['enabled'] is False and initial_config['setup_complete'] is False and not initial_config['acp']['command']
        first=str(uuid.uuid4());receipt['first_request_id']=first
        initial=call(installed,'native-start','--request-id',first);generation=ready(initial,first)
        receipt['service']=initial['service'];receipt['first_generation']=generation;receipt['first_process_relationship']=initial['process_relationship']
        assert generation['pid']!=events[-1]['caller_pid'],'Native start remained the service process'
        baseline=protected();receipt['protected_before']=baseline
        time.sleep(.25)
        observed=call(installed,'native-status');ready(observed,first,generation);assert observed['service']==initial['service']
        receipt['console_observation']=console_probe(generation)
        ready(call(installed,'native-status'),first,generation)
        stop_id=str(uuid.uuid4());stopped(call(installed,'native-stop','--request-id',stop_id),stop_id,first,generation)
        inactive=call(installed,'native-status');exact(inactive);assert inactive['state']=='inactive' and inactive['profile_matches'] is True
        assert protected()==baseline,'Stop changed existing configuration or private state'
        second=str(uuid.uuid4());receipt['second_request_id']=second
        restarted=call(installed,'native-start','--request-id',second);next_generation=ready(restarted,second)
        assert next_generation!=generation and restarted['service']==initial['service'],'Restart changed profile or reused old generation'
        ready(call(installed,'native-status'),second,next_generation)
        assert protected()==baseline,'Restart changed existing configuration or private state'
        second_stop=str(uuid.uuid4());stopped(call(installed,'native-stop','--request-id',second_stop),second_stop,second,next_generation)
        retire=str(uuid.uuid4());stopped(call(installed,'native-retire','--request-id',retire),retire,second,next_generation,'retired')
        absent=call(installed,'native-status');exact(absent);assert absent['state']=='retired' and absent['request_current'] is True
        assert protected()==baseline,'Retirement changed retained installation data'
        receipt.update(passed=True,second_generation=next_generation,second_process_relationship=restarted['process_relationship'],protected_after=protected(),registration_absent=True,cleanup='checked profile retired; exact fresh home removal pending')
        save();shutil.rmtree(root);receipt['cleanup']='checked profile retired; exact fresh home removed';save()
    except BaseException as error:
        receipt.update(passed=False,error_type=type(error).__name__,error=str(error)[:2048],retained_home=home.exists(),cleanup='not attempted after failed or unknown action')
        retain_managed_failure(receipt,home,home_identity,events,save);raise
    return receipt

def run(package,result,browser=False,verify_setup_tool=False,verify_startup=False):
    checks=[];manager_receipt=None;hook_receipt=None
    with retained_fixture_root() as temporary:
        root=Path(temporary);bundle=root/'download';bundle.mkdir();extract(package,bundle)
        binary=next(p for p in bundle.rglob('acp-slack-adapter.exe' if os.name=='nt' else 'acp-slack-adapter') if p.is_file())
        binary.chmod(0o755);home=root/'private connection';env={k:v for k,v in os.environ.items() if not k.upper().startswith(('SLACK_','OPENAI_','ANTHROPIC_','AZURE_','GOOGLE_','AMPLIFIER_'))};env['PATH']=str(root/'no-tools');env['PYTHONDONTWRITEBYTECODE']='1';env['USER']='Native café 子';env['USERNAME']='Native café 子'
        if verify_startup:env['PATH']=str(Path(os.environ['SystemRoot'])/'System32') if os.name=='nt' else '/usr/bin:/bin'
        checks.extend(finite_setup(binary,root,env))
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
            cookie=headers['set-cookie'].split(';',1)[0];fresh_setup_status(home,base,{'Cookie':cookie});checks.append('guided Slack setup GET reads saved state without creating a helper, directory or credentials')
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
                prepare_fresh_setup_tool(home,base,{'Cookie':cookie,'X-CSRF-Token':session['csrf']})
                checks.append('one explicit fresh-home local discard materializes the exact helper; subsequent read-only saved setup succeeds without authorization/network or account/configuration changes')
                code="import importlib.util,pathlib,subprocess,sys; p=pathlib.Path(sys.argv[1]); spec=importlib.util.spec_from_file_location('setup',p/'secrets/slack-setup-helper.py'); m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); b=m.ensure_cli(p/'secrets/slack-setup'); r=subprocess.run([str(b),'version','--skip-update'],capture_output=True,text=True,encoding='utf-8',timeout=30); assert r.returncode==0 and '4.12.0' in r.stdout, (r.returncode,r.stdout); print('Official Slack CLI 4.12.0 starts')"
                vendor_env={**env,'SLACK_SKIP_UPDATE':'true','NO_COLOR':'1'}
                verified=subprocess.run([str(private_python),'-I','-B','-X','utf8','-c',code,str(home)],env=vendor_env,capture_output=True,text=True,encoding='utf-8',timeout=150)
                assert verified.returncode==0,verified.stderr
                checks.append('actual checksum-pinned official Slack CLI starts on this platform')
                hook_receipt=verify_packaged_hooks(root,home/'secrets/slack-setup-helper.py',result,env)
                checks.append('actual packaged Python/native-shell get-hooks, manifest and fake-only install preserve settings with OS-only PATH; not a CLI deploy/auth chain')

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
        if verify_startup:
            manager_receipt=managed_lifecycle(binary,env,result)
            checks.append('actual fixed native manager survives caller exit; exact generation, stop, same-profile restart and checked retirement preserve private state')
    receipt={'passed':True,'checks':checks,'package':str(package),'package_sha256':hashlib.sha256(package.read_bytes()).hexdigest(),'limitations':['No real Slack authorization or model/provider call','No publisher signing/notarization claim','Current-user startup registration exercised only when verify_startup is enabled; automatic sign-in launch is not simulated']}
    receipt['managed_lifecycle']=manager_receipt
    receipt['native_hook_contract']=hook_receipt
    receipt['limitations'].append('Hook contract substitutes Python for Go parent; no actual CLI auth/deploy or universal no-console proof')
    if result:result.write_text(json.dumps(receipt,indent=2)+'\n')
    print(json.dumps(receipt,indent=2))
if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--package',required=True,type=Path);p.add_argument('--result',type=Path);p.add_argument('--browser',action='store_true');p.add_argument('--verify-setup-tool',action='store_true');p.add_argument('--verify-startup',action='store_true');a=p.parse_args();run(a.package.resolve(),a.result,a.browser,a.verify_setup_tool,a.verify_startup)
