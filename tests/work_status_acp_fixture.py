"""Ordinary ACP fixture for observable work; no network or model calls."""
import json,sys,time
def send(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);method=r.get('method');p=r.get('params',{});rid=r.get('id')
 if method=='initialize':
  result={'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
  if '--host-attention' in sys.argv:result['_meta']={'com.microsoft.amplifier.attention':{'version':1,'enabled':True,'method':'_amplifier/attention'}}
 elif method=='_amplifier/attention':
  text=p['context']['messages'][-1]['text'];time.sleep(.6)
  result={'action':'engage' if 'contribute' in text or '<@Ualpha>' in text else 'hold','id':'fixture-offer','ticket':'fixture-ticket','reason':'fixture'}
 elif method=='session/new':result={'sessionId':'saved-fixture'}
 elif method=='session/prompt':
  if '_meta' in p:assert p['_meta']['com.microsoft.amplifier.attention']=={'id':'fixture-offer','ticket':'fixture-ticket'}
  prompt=''.join(b.get('text','') for b in p['prompt']);input=json.loads(prompt.split('Latest conversation projection:\n',1)[1]);text=input['trigger']['text'];sid=p['sessionId']
  time.sleep(.6)
  if 'detailed' in text:
   def update(**fields):send({'method':'session/update','params':{'sessionId':sid,'update':fields}})
   send({'method':'session/update','params':{'sessionId':'wrong-session','update':{'sessionUpdate':'tool_call','toolCallId':'wrong','title':'WRONG_SESSION','kind':'execute','status':'in_progress'}}})
   update(sessionUpdate='tool_call',toolCallId='test',title='Run unit tests',name='workspace_run',kind='execute',status='pending',rawInput={'secret':'PRIVATE_ARGUMENT'})
   update(sessionUpdate='tool_call_update',toolCallId='test',status='in_progress')
   if 'Decide quietly' not in prompt:time.sleep(2.4)
   update(sessionUpdate='tool_call',toolCallId='read',title='Read '+input['trigger']['channel']+'\nnotes <!channel>',kind='read',status='in_progress',rawOutput='PRIVATE_RESULT')
   if 'Decide quietly' not in prompt:time.sleep(2.2)
   update(sessionUpdate='tool_call_update',toolCallId='read',status='completed')
   if 'Decide quietly' not in prompt:time.sleep(1.8)
   update(sessionUpdate='tool_call_update',toolCallId='test',status='completed')
   if 'Decide quietly' not in prompt:time.sleep(1.8)
  if 'permission' in text:
   send({'id':'permission','method':'session/request_permission','params':{'sessionId':sid,'toolCall':{'toolCallId':'fixture','title':'Perform requested action','kind':'execute'},'options':[{'optionId':'yes','name':'Allow once','kind':'allow_once'},{'optionId':'no','name':'Reject once','kind':'reject_once'}]}})
   json.loads(sys.stdin.readline());time.sleep(.6)
  if 'question' in text:
   send({'id':'question','method':'elicitation/create','params':{'sessionId':sid,'mode':'form','message':'Which direction?','requestedSchema':{'type':'object','properties':{'answer':{'type':'string'}},'required':['answer']}}})
   json.loads(sys.stdin.readline());time.sleep(.6)
  if 'stall' in text:time.sleep(120)
  if 'fail' in text:send({'id':rid,'error':{'code':-32603,'message':'fixture backend unavailable'}});continue
  if 'Decide quietly' in prompt:
   output=json.dumps({'decision':'speak' if 'contribute' in text else 'pass','reason':'fixture decision'})
  else:output=json.dumps({'decision':'speak','text':'Work finished','reason':'Useful'}) if 'Return exactly one final JSON object' in prompt else 'Work finished'
  if 'followup' in text and 'Decide quietly' not in prompt and not input.get('accepted_work_continuation'):output+='\nSLACK_FOLLOW_UP {"after_seconds":5,"goal":"Verify the existing result"}'
  send({'method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':output}}}});result={'stopReason':'end_turn'}
 else:result={}
 if rid is not None:send({'id':rid,'result':result})
