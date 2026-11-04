"""Deterministic ACP workflow: follow-up questions must not lose accepted work."""
import json,sys,time
from pathlib import Path
root=Path.cwd()
import traceback
sys.excepthook=lambda *args:(root/"fixture-error.txt").write_text("".join(traceback.format_exception(*args)))
def send(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
def update(sid,**v):send({'method':'session/update','params':{'sessionId':sid,'update':v}})
def permission(sid,tool):
 send({'id':tool,'method':'session/request_permission','params':{'sessionId':sid,'toolCall':{'toolCallId':tool,'title':tool,'kind':'execute'},'options':[{'optionId':'yes','name':'Allow once','kind':'allow_once'}]}})
 # ACP notifications can arrive between a callback and its response. Match
 # the response ID, as a real JSON-RPC peer does, instead of assuming ordering.
 for line in sys.stdin:
  r=json.loads(line)
  if r.get('id')==tool and 'method' not in r:
   if 'error' in r:raise RuntimeError('Permission callback failed: '+json.dumps(r['error']))
   return r['result']
  if r.get('method') and 'id' in r:send({'id':r['id'],'result':{}})
 raise RuntimeError('ACP disconnected while awaiting permission')
def effect(name):
 p=root/name;p.write_text(str(int(p.read_text())+1) if p.exists() else '1')
for line in sys.stdin:
 r=json.loads(line);p=r.get('params',{});method=r.get('method');rid=r.get('id')
 if method=='initialize':result={'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
 elif method in ('session/new','session/load'):
  root=Path(p['cwd']);result={'sessionId':'fixture-session'}
 elif method=='session/prompt':
  text=''.join(b.get('text','') for b in p['prompt']);data=json.loads(text.split('Latest conversation projection:\n',1)[1]);trigger=data['trigger']['text'];sid=p['sessionId']
  if trigger.startswith('Build'):
   update(sid,sessionUpdate='tool_call',toolCallId='branch',title='Creating branch',kind='execute',status='in_progress')
   assert permission(sid,'branch')['outcome']['outcome']=='selected';effect('branches');update(sid,sessionUpdate='tool_call_update',toolCallId='branch',status='completed')
   time.sleep(2)
   update(sid,sessionUpdate='tool_call',toolCallId='old-pr',title='Opening PR',kind='execute',status='pending')
   denied=permission(sid,'old-pr');assert denied['outcome']['outcome']=='cancelled';assert denied['_meta']['com.microsoft.amplifier.permission']['reason']=='source_changed'
   update(sid,sessionUpdate='tool_call_update',toolCallId='old-pr',status='failed');answer='Old draft, must not publish'
  elif trigger.startswith('Can I'):
   handoff=data['work_handoff'];assert handoff['earlier_requests'][0]['observed_tools'][0]['status']=='completed'
   assert (root/'branches').read_text()=='1';update(sid,sessionUpdate='tool_call',toolCallId='new-pr',title='Opening PR',kind='execute',status='pending')
   assert permission(sid,'new-pr')['outcome']['outcome']=='selected';effect('prs');update(sid,sessionUpdate='tool_call_update',toolCallId='new-pr',status='completed');answer='Yes, and the existing branch now has a PR.'
  elif trigger.startswith('Stop'):
   assert data.get('work_handoff');answer='Stopped the remaining work.'
  else:answer='Recovered message answered.'
  update(sid,sessionUpdate='agent_message_chunk',content={'type':'text','text':answer});result={'stopReason':'end_turn'}
 else:result={}
 if rid is not None:send({'id':rid,'result':result})
