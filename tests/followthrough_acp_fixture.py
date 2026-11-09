"""Ordinary ACP fixture using the real connector MCP follow-up tool on loopback."""
import json, pathlib, ssl, subprocess, sys, urllib.request

root=pathlib.Path(sys.argv[1]); server=None

def out(v):
    print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)

def rpc(method,args):
    assert server['url'].startswith('https://127.0.0.1:')
    headers={h['name']:h['value'] for h in server['headers']}
    headers['Content-Type']='application/json'
    request=urllib.request.Request(server['url'],data=json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':args}).encode(),headers=headers)
    with urllib.request.urlopen(request,context=ssl._create_unverified_context(),timeout=10) as response:
        return json.load(response)['result']

for line in sys.stdin:
    r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
    if m=='initialize':
        result={'protocolVersion':1,'agentCapabilities':{'loadSession':True,'mcpCapabilities':{'http':True}},'_meta':{'com.microsoft.amplifier.attention':{'version':1,'enabled':True,'method':'_amplifier/attention','noForwardRejectionVersion':1}}}
    elif m in ['session/new','session/load']:
        server=p['mcpServers'][0];result={'sessionId':'original-fixture'} if m=='session/new' else {}
    elif m=='_amplifier/attention':
        result={'action':'engage','id':'offer','ticket':'ticket'}
    elif m=='session/prompt':
        text=''.join(b.get('text','') for b in p['prompt']);v=json.loads(text.split('Latest conversation projection:\n',1)[1]);channel=v['trigger']['channel']
        sid=p['sessionId']; count=root/(channel+'-prompts');count.write_text(str(int(count.read_text())+1) if count.exists() else '1')
        def update(**fields):out({'method':'session/update','params':{'sessionId':sid,'update':fields}})
        def speak(value):update(sessionUpdate='agent_message_chunk',content={'type':'text','text':value})
        if channel in ['Crefused','Cgeneric']:
            error={'code':-32002,'message':'PRIVATE generic transport-looking text'}
            if channel=='Crefused':error['data']={'com.microsoft.amplifier.attention':{'version':1,'id':'offer','sessionId':sid,'outcome':'not_forwarded'}}
            out({'id':i,'error':error});continue
        if not v.get('accepted_work_continuation'):
            starts=root/(channel+'-starts');assert not starts.exists();starts.write_text('1')
            if channel=='Dbackground':
                result_file=root/'background-result'
                # A real disposable background process fails after the ACP turn.
                subprocess.Popen([sys.executable,'-c',"import pathlib,sys,time;time.sleep(1);pathlib.Path(sys.argv[1]).write_text('failed: test run exited 6');sys.exit(6)",str(result_file)],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
                tools=rpc('tools/list',{});assert any(t['name']=='slack_follow_up' for t in tools['tools'])
                args={'name':'slack_follow_up','arguments':{'after_seconds':5,'goal':'Read background-result and report failure or success; never relaunch.'}}
                first=rpc('tools/call',args);assert first['structuredContent']['scheduled']
                assert rpc('tools/call',args)==first # repeated call after a lost receipt
                # The successful schedule is the only output; no public filler or pass envelope.
                result={'stopReason':'end_turn'}
            elif channel.startswith('Cpeer'):
                assert v['trigger']['bot'] is True and v['trigger']['sender']=='Ubeta'
                (root/(channel+'-source')).write_text(json.dumps(v['trigger'],sort_keys=True))
                tools=rpc('tools/list',{});assert any(t['name']=='slack_follow_up' for t in tools['tools'])
                args={'name':'slack_follow_up','arguments':{'after_seconds':5,'goal':'Inspect the existing peer artifact; do not create it twice.'}}
                first=rpc('tools/call',args);assert first['structuredContent']['scheduled']
                assert rpc('tools/call',args)==first
                result={'stopReason':'end_turn'}
            elif channel=='Dlimit':
                update(sessionUpdate='tool_call',toolCallId='single-effect',kind='execute',title='Write artifact',status='completed')
                (root/'artifact').write_text('existing result')
                speak('PARTIAL MUST NOT BE PUBLISHED')
                result={'stopReason':'max_turn_requests'}
        else:
            assert sid=='original-fixture'
            if channel.startswith('Cpeer'):
                assert json.loads((root/(channel+'-source')).read_text())==v['trigger']
                assert v['accepted_work_continuation']['maximum_turns'] is None
                assert v['accepted_work_continuation']['deadline'] is None
                speak('The accepted peer result is ready; its original artifact was preserved.')
            elif channel=='Dbackground':
                assert (root/'background-result').read_text().startswith('failed:')
                speak('The existing background run failed; I did not launch it again.')
            else:
                assert (root/'artifact').read_text()=='existing result'
                speak('I checked the existing artifact and completed the remaining report.')
            result={'stopReason':'end_turn'}
    else:result={}
    if i is not None:out({'id':i,'result':result})
