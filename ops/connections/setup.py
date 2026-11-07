#!/usr/bin/env python3
"""Installation-owned typed Add provider. Linux slots/routes are approved locally.

No host path, command, credential or endpoint is accepted from the web form.
The existing domain installer creates a separate paused Slack connection. Its
own web setup performs Slack authorization. No model or Slack message is sent.
"""
import argparse,fcntl,hashlib,json,os,re,stat,subprocess,sys,tempfile,urllib.request,urllib.parse,ssl
from pathlib import Path

def read(path):return json.loads(Path(path).read_text())
def write(path,value):
    path=Path(path);path.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
    with tempfile.NamedTemporaryFile(dir=path.parent,mode='w',delete=False) as f:
        json.dump(value,f);f.flush();os.fsync(f.fileno());temporary=f.name
    os.chmod(temporary,0o600);os.replace(temporary,path)

def load(path):
    data=Path(path).read_bytes();c=json.loads(data)
    if c.get('schema')!=1 or not c.get('slots') or len(c['slots'])>16:raise ValueError('invalid_setup_configuration')
    for field in ['state','data_root','agent_home','binary','installer']:
        if not Path(c[field]).is_absolute():raise ValueError('absolute_owner_paths_required')
    if Path(c['state']).resolve().is_relative_to(Path(c['data_root']).resolve()):raise ValueError('state_must_be_outside_connector_homes')
    ids=set();homes=set();ports=set()
    for slot in c['slots']:
        if not re.fullmatch('[a-z][a-z0-9-]{0,40}',slot['id']) or slot['id'] in ids or not isinstance(slot['port'],int) or not 1024<=slot['port']<=65535 or slot['port'] in ports:raise ValueError('invalid_or_duplicate_slot')
        home=Path(c['data_root'])/slot['id'];ids.add(slot['id']);homes.add(str(home));ports.add(slot['port'])
        credential(slot) # Explicit owner-prepared ACP scope; never inherit local owner credentials.
        url=urllib.parse.urlparse(slot['public_url'])
        if url.scheme!='https' or not url.hostname or url.username or url.password or url.query or url.fragment:raise ValueError('approved_https_setup_route_required')
    c['revision']=hashlib.sha256(data).hexdigest();return c

def enrollment(c,path):
    script=str(Path(__file__).resolve());args=[sys.executable,script,'--config',str(Path(path).resolve())]
    return {'id':'slack','label':'Slack','add':{'revision':c['revision'],'fields':[{'name':'label','label':'Connection name','kind':'text','required':True,'max_length':120},{'name':'slot','label':'Installation','kind':'choice','required':True,'choices':[{'value':s['id'],'label':s['label']} for s in c['slots']]}]},'add_commands':{a:args+['--action',a] for a in ['add','status']}}

def credential(slot):
    binding=slot.get('acp_credential',{});p=Path(binding.get('file',''))
    if not p.is_absolute() or not p.is_file() or p.is_symlink() or stat.S_IMODE(p.stat().st_mode)&0o077:raise ValueError('private_scoped_acp_credential_required')
    raw=p.read_bytes()
    if len(raw)>16384 or hashlib.sha256(raw).hexdigest()!=binding.get('sha256'):raise ValueError('prepared_acp_credential_changed')
    value=json.loads(raw)
    if set(value.get('scopes',[]))!={'acp','tools:use'} or not isinstance(value.get('token'),str) or not 16<=len(value['token'])<=8192 or any(x.isspace() for x in value['token']):raise ValueError('scoped_acp_credential_required')
    return value['token']

def local_api(home,path,body=None,method=None):
    endpoints=read(home/'endpoints.json')
    context=ssl.create_default_context(cafile=endpoints.get('trust_certificate_file',endpoints.get('certificate_file')))
    headers={'Authorization':'Bearer '+(home/'secrets/control-token').read_text().strip()}
    if body is not None:headers['Content-Type']='application/json'
    request=urllib.request.Request(endpoints['base_url'].rstrip('/')+'/api/v1/'+path,headers=headers,data=None if body is None else json.dumps(body).encode(),method=method)
    with urllib.request.urlopen(request,context=context,timeout=3) as response:return json.loads(response.read(65536))

def configure_scoped(c,home,api=local_api):
    slot=next(s for s in c['slots'] if Path(c['data_root'])/s['id']==home)
    token=credential(slot);path=home/'secrets/agent-acp-token'
    if path.exists():
        if path.read_text()!=token:raise ValueError('saved_acp_credential_changed')
    else:
        # The exact prepared credential belongs to this unused slot. Lost replies
        # observe this file and configuration; no token issuance is retried.
        fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as f:f.write(token);f.flush();os.fsync(f.fileno())
    configuration=api(home,'config');revision=configuration['revision']
    old=['acp','--home',c['agent_home']];wanted=old+['--token-file',str(path)]
    if configuration['enabled'] or configuration['context']['enabled'] or configuration['acp']['command']!=str(Path(c['agent_home'])/'bin/agent-instance') or configuration['acp']['args'] not in [old,wanted]:raise ValueError('prepared_endpoint_changed')
    if configuration['acp']['args']!=wanted:
        configuration['acp']['args']=wanted
        api(home,'config',{'expected_revision':revision,'config':configuration},'PUT')
    current=api(home,'config')
    if current['enabled'] or current['acp']['args']!=wanted or current['context']['enabled']:raise ValueError('scoped_configuration_not_confirmed')

def observed(c,home):
    configure_scoped(c,home)
    configuration=read(home/'adapter.json');status=local_api(home,'status')
    if status.get('id')!=configuration['id'] or configuration['enabled'] or status['enabled']:raise ValueError('new_connection_must_be_paused')
    binary=home/'bin/acp-slack-adapter'
    result=subprocess.run([str(binary),'--home',str(home),'connection-enrollment'],capture_output=True,timeout=5,check=True)
    connection=json.loads(result.stdout);connection['settings_url']=configuration['server']['public_url']
    return connection

def operation(c,request,invoke=None,inspect=None):
    invoke=invoke or subprocess.run;inspect=inspect or observed
    if set(request)!={'schema','service_id','revision','action','request_id','values'} or request['schema']!=1 or request['service_id']!='slack' or request['revision']!=c['revision'] or request['action'] not in ['add','status']:raise ValueError('invalid_setup_request')
    import uuid
    rid=str(uuid.UUID(request['request_id']));values=request['values']
    if set(values)!={'label','slot'} or not isinstance(values['label'],str) or not 1<=len(values['label'].strip())<=120:raise ValueError('invalid_setup_values')
    slot=next((s for s in c['slots'] if s['id']==values['slot']),None)
    if not slot:raise ValueError('unapproved_slot')
    home=Path(c['data_root'])/slot['id'];path=Path(c['state'])/'operations'/f'{rid}.json'
    identity={'id':rid,'values':values,'revision':c['revision']}
    response={'schema':1,'service_id':'slack','request_id':rid,'state':'unknown','settled':False,'message':'Setup has not been confirmed.'}
    if path.exists():
        journal=read(path)
        if journal['identity']!=identity:raise ValueError('request_id_reused')
        if journal.get('result'):return journal['result']
    else:
        if request['action']=='status':return response
        journal={'identity':identity,'phase':'prepared'}
        if home.exists() or not Path('/etc/pam.d/acp-slack-adapter').exists() or not Path(c['binary']).is_file() or not Path(c['installer']).is_file():
            response.update(state='failed',settled=True,message='This slot already exists or its installation prerequisites are unavailable. Existing connections were preserved.')
            journal['result']=response;write(path,journal);return response
        # Record ownership before installer effects. Never invoke the installer again
        # after timeout/crash; status inspects the same owned installation only.
        journal['phase']='installing';write(path,journal)
        home.mkdir(parents=True,exist_ok=False,mode=0o700)
        write(home/'.setup-request.json',identity)
        write(home/'connection-gate.json',{'request_id':rid})
        agent=read(Path(c['agent_home'])/'config.json').get('config',{})
        management=agent.get('server',{}).get('public_url')
        if management:write(home/'saved-connections.json',{'url':management.rstrip('/')+'/#connections'})
        args=[sys.executable,c['installer'],'--home',str(home),'--label='+values['label'],'--agent-home',c['agent_home'],'--bind',slot['bind'],'--port',str(slot['port']),'--public-url',slot['public_url'],'--binary',c['binary'],'--no-open']
        try:invoke(args,capture_output=True,timeout=20,check=True,stdin=subprocess.DEVNULL)
        except Exception:return response
    try:
        if read(home/'.setup-request.json')!=identity:raise ValueError('setup_ownership_changed')
        connection=inspect(c,home)
        response.update(state='saved',settled=True,message='Connection saved. Open Settings to authorize Slack before connecting.',connection=connection)
        journal['result']=response;write(path,journal)
    except Exception:response.update(state='busy',message='Waiting for the new paused setup service; the installer will not be repeated.')
    return response

def main():
    os.umask(0o077);p=argparse.ArgumentParser(description=__doc__);p.add_argument('--config',type=Path,required=True);p.add_argument('--action',choices=['enrollment','add','status'],required=True);a=p.parse_args();c=load(a.config)
    if a.action=='enrollment':print(json.dumps(enrollment(c,a.config)));return
    data=sys.stdin.buffer.read(65537)
    if len(data)>65536:raise ValueError('request_too_large')
    request=json.loads(data)
    if request.get('action')!=a.action:raise ValueError('fixed_action_mismatch')
    state=Path(c['state']);state.mkdir(parents=True,exist_ok=True,mode=0o700)
    with (state/'installation.lock').open('a+b') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB);print(json.dumps(operation(c,request)))
if __name__=='__main__':main()
