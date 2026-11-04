"""Ordinary ACP fixture for observable work; no network or model calls."""
import json,sys,time
def send(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);method=r.get('method');p=r.get('params',{});rid=r.get('id')
 if method=='initialize':result={'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
 elif method=='session/new':result={'sessionId':'saved-fixture'}
 elif method=='session/prompt':
  prompt=''.join(b.get('text','') for b in p['prompt']);input=json.loads(prompt.split('Latest conversation projection:\n',1)[1]);text=input['trigger']['text'];sid=p['sessionId']
  time.sleep(.6)
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
