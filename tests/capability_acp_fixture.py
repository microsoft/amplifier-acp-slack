"""Independent ACP peer exercising client callbacks, not an agent runtime."""
import json, os, sys, uuid
from pathlib import Path
ROOT=Path(sys.argv[1])

def send(value):
    print(json.dumps({'jsonrpc':'2.0',**value}),flush=True)

def record(kind,**data):
    with (ROOT/'callbacks.jsonl').open('a') as f:
        f.write(json.dumps({'kind':kind,**data})+'\n')

def call(method,params):
    rid=str(uuid.uuid4());send({'id':rid,'method':method,'params':params})
    result=json.loads(sys.stdin.readline());assert result['id']==rid,result
    record(method,result=result)
    return result

for line in sys.stdin:
    r=json.loads(line);p=r.get('params',{});m=r.get('method')
    if m=='initialize':
        record('initialize',params=p); caps=p['clientCapabilities']
        send({'id':r['id'],'result':{'protocolVersion':1,'agentInfo':{'name':'callback-fixture'},'agentCapabilities':{'loadSession':True}}})
    elif m in ('session/new','session/load'):
        sid=p.get('sessionId',str(uuid.uuid4()));cwd=Path(p['cwd'])
        if caps.get('fs',{}).get('writeTextFile'):
            base={'sessionId':sid};path=str(cwd/'lifecycle.txt')
            assert call('fs/write_text_file',{**base,'path':path,'content':'lifecycle callback'})['result']=={}
            assert call('fs/read_text_file',{**base,'path':path})['result']['content']=='lifecycle callback'
            assert 'error' in call('fs/read_text_file',{**base,'sessionId':'foreign','path':path})
            record('setup_callbacks',method=m)
        send({'id':r['id'],'result':{'sessionId':sid}})
    elif m=='session/prompt':
        payload=json.loads(p['prompt'][0]['text'].split('Latest conversation projection:\n',1)[1])
        trigger=payload['trigger']['text'];sid=p['sessionId']
        output='fixture done'
        if 'permission' in trigger:
            send({'method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'tool_call',**{'toolCallId':'fixture-tool','title':'Create the harmless approval-check file','kind':'edit','locations':[{'path':str(cwd/'approved.txt')}],'rawInput':{'path':str(cwd/'approved.txt'),'content':'Explicitly approved'}}}}})
            options=[{'optionId':'yes','name':'Allow once','kind':'allow_once'},{'optionId':'no','name':'Deny once','kind':'reject_once'},{'optionId':'forever','name':'Always allow','kind':'allow_always'}]
            if 'ambiguous' in trigger:options[1]['optionId']='yes'
            response=call('session/request_permission',{'sessionId':sid,'toolCall':{'toolCallId':'fixture-tool'},'options':options})
            if 'ambiguous' in trigger:
                assert 'error' in response
                send({'method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ambiguous options refused'}}}})
                send({'id':r['id'],'result':{'stopReason':'end_turn'}})
                continue
            outcome=response['result']['outcome']
            output='permission '+json.dumps(outcome)
            if outcome.get('optionId')=='yes':
                (cwd/'approved.txt').write_text('Explicitly approved')
        elif 'callbacks' in trigger:
            base={'sessionId':sid};path=str(cwd/'client-file.txt')
            assert call('fs/write_text_file',{**base,'path':path,'content':'first\nsecond\n'})['result']=={}
            assert call('fs/read_text_file',{**base,'path':path,'line':2,'limit':1})['result']['content']=='second\n'
            assert 'error' in call('fs/read_text_file',{**base,'path':'/etc/passwd'})
            assert 'error' in call('fs/write_text_file',{**base,'path':str(cwd/'escape/no.txt'),'content':'bad'})
            assert 'error' in call('fs/read_text_file',{**base,'sessionId':'wrong','path':path})
            tid=call('terminal/create',{**base,'command':'/bin/sh','args':['-c','printf terminal-ok; test ! -e /etc/passwd; test ! -e /home/operator/.codex/auth.json'],'outputByteLimit':100})['result']['terminalId']
            term={**base,'terminalId':tid}
            assert call('terminal/wait_for_exit',term)['result']['exitCode']==0
            assert call('terminal/output',term)['result']['output']=='terminal-ok'
            assert call('terminal/release',term)['result']=={}
            assert 'error' in call('terminal/output',term)
            # A pending wait must not prevent a later kill callback from being read.
            tid=call('terminal/create',{**base,'command':'/bin/sleep','args':['20']})['result']['terminalId'];term={**base,'terminalId':tid}
            send({'id':'waiting','method':'terminal/wait_for_exit','params':term})
            send({'id':'killing','method':'terminal/kill','params':term})
            responses=[json.loads(sys.stdin.readline()) for _ in range(2)]
            assert {x['id'] for x in responses}=={'waiting','killing'}
            record('concurrent_wait_kill',responses=responses)
            assert call('terminal/release',term)['result']=={}
            output='callbacks verified'
        send({'method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':output}}}})
        send({'id':r['id'],'result':{'stopReason':'cancelled' if 'cancelled turn' in trigger else 'end_turn'}})
        record('turn_finished',trigger=trigger)
