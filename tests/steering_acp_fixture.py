"""Controllable concurrent ACP peer; no external service or model."""
import json,sys,time,threading
from pathlib import Path
root=Path.cwd();mode=sys.argv[1];lock=threading.Lock();waits={};updates=[];active=None
peer_mode=mode.startswith('peer')
ambient_modes=('attention','attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold')

def send(v):
 with lock:print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
def log(kind,value):
 with lock:
  with (root/'events.jsonl').open('a') as f:f.write(json.dumps({'kind':kind,**value})+'\n')
def update(sid,**fields):send({'method':'session/update','params':{'sessionId':sid,'update':fields}})
def permission(sid,name):
 event=threading.Event();waits[name]=[event,None]
 send({'id':name,'method':'session/request_permission','params':{'sessionId':sid,'toolCall':{'toolCallId':name,'title':'Fixture operation','kind':'execute'},'options':[{'optionId':'once','kind':'allow_once','name':'Allow once'}]}})
 assert event.wait(12),'permission timed out'
 result=waits[name][1];log('permission',{'name':name,'result':result});assert result['result']['outcome']['outcome']=='selected'
def run(req,data):
 global active
 sid=req['params']['sessionId'];text=data['trigger']['text'];is_root=text=='Build'
 log('prompt',{'text':text,'channel':data['trigger']['channel'],'thread':data['trigger']['thread']})
 if mode=='attentionContinuation' and not data.get('accepted_work_continuation'):
  update(sid,sessionUpdate='agent_message_chunk',content={'type':'text','text':json.dumps({'decision':'speak','text':'Initial progress','reason':'fixture'})+'\nSLACK_FOLLOW_UP {"after_seconds":5,"goal":"Finish the accepted work"}'})
  send({'id':req['id'],'result':{'stopReason':'end_turn'}});return
 if mode=='recovery' and not is_root:
  send({'id':req['id'],'error':{'code':-32010,'message':'Previous input outcome requires review. private-provider-detail'}});return
 if is_root:
  active=req;permission(sid,'first');log('effect',{'name':'first'});(root/'started').write_text('ready')
  deadline=time.monotonic()+20
  while not (root/'finish').exists():
   assert time.monotonic()<deadline
   time.sleep(.01)
  permission(sid,'second');log('effect',{'name':'second'});text='Finished original work'+(' with '+','.join(u['text'] for u in updates) if updates else '')
 if mode=='peerRevision':
  update(sid,sessionUpdate='agent_message_chunk',messageId='old',content={'type':'text','text':json.dumps({'decision':'speak','text':'Old answer','reason':'before peer update'})})
  update(sid,sessionUpdate='agent_message_chunk',messageId='final',content={'type':'text','text':json.dumps({'decision':'pass','reason':'peer already answered'})})
  update(sid,sessionUpdate='agent_message_chunk',messageId='old',content={'type':'text','text':' '})
 else:update(sid,sessionUpdate='agent_message_chunk',messageId='final' ,content={'type':'text','text':json.dumps({'decision':'speak','text':text,'reason':'fixture'}) if (mode in ambient_modes or peer_mode) else text})
 receipts=[{'inputId':u['id'],'state':'incorporated','turnId':'fixture-turn','sequence':n}for n,u in enumerate(updates)] if is_root and mode!='peerRemoteQueued' else []
 send({'id':req['id'],'result':{'stopReason':'end_turn','_meta':{'io.github.bkrabach.agent-runtime':{'inputReceipts':receipts}}}});active=None
for line in sys.stdin:
 req=json.loads(line);method=req.get('method');p=req.get('params',{});rid=req.get('id')
 if method is None and rid in waits:waits[rid][1]=req;waits[rid][0].set();continue
 if method=='initialize':
  meta={} if mode=='fallback' else {'steering':{'supported':True,'idleBehavior':['promptRequired'],'version':1}}
  if peer_mode and mode != 'peerFallback':meta['io.github.bkrabach.agent-instance']={'inputContinuity':{'awareness':{'version':1,'activeOnly':True,'promptIdRequired':True}}}
  if mode=='peerSupported':meta['io.github.bkrabach.agent-instance']['inputContinuity']['sourceDelivery']={'version':1}
  if (mode in ambient_modes or peer_mode):meta['com.microsoft.amplifier.attention']={'version':1,'enabled':True,'method':'_amplifier/attention'}
  result={'protocolVersion':1,'agentCapabilities':{'loadSession':True},'_meta':meta}
 elif method in ('session/new','session/load'):
  root=Path(p['cwd']);result={'sessionId':'fixture-'+str(root)}
 elif method=='_amplifier/attention':
  if mode=='peerSupported':
   assert all(isinstance(m.get('revision'),str) and isinstance(m.get('occurred_at'),int) for m in p['context']['messages'])
  log('attention',{'trigger':p['context']['trigger_id']});result={'action':'engage','id':'offer','ticket':'ticket'}
  if active is not None and mode=='attentionHold':result={'action':'hold','reason':'fixture'}
  if active is not None and mode=='attentionRace':
   (root/'finish').write_text('go')
   def late_offer(request_id=rid,result=result):
    while active is not None:time.sleep(.01)
    time.sleep(.1);send({'id':request_id,'result':result})
   threading.Thread(target=late_offer,daemon=True).start();continue
 elif method=='session/prompt':
  data=json.loads(''.join(b.get('text','')for b in p['prompt']).split('Latest conversation projection:\n',1)[1]);threading.Thread(target=run,args=(req,data),daemon=True).start();continue
 elif method=='_session/steering':
  assert p['_meta']['steering']['idleBehavior']=='promptRequired';assert p['_meta']['steering']['inputId']
  if mode in ambient_modes:assert p['_meta']['com.microsoft.amplifier.attention']=={'id':'offer','ticket':'ticket'}
  data=json.loads(''.join(b.get('text','')for b in p['prompt']).split('Incremental conversation update:\n',1)[1])
  assert 'history' not in data and 'orientation' not in data and 'response_delivery' not in data
  assert 'com.microsoft.amplifier.context-snapshot' not in p['prompt'][0].get('_meta',{})
  source=data.get('awareness_input',data['trigger'])
  if mode=='peerSupported':
   sources=p['_meta']['com.microsoft.amplifier.source-delivery']['sources'];assert len(sources)==1 and sources[0]=={'id':source['ts'],'revision':source['revision'],'occurredAt':int(float(source['ts'])*1000)}
  else:assert 'com.microsoft.amplifier.source-delivery' not in p['_meta']
  u={'id':p['_meta']['steering']['inputId'],'text':source['text'],'sender':source['sender'],'channel':source['channel'],'thread':source['thread']}
  if peer_mode:
   assert p['_meta']['steering']['intent']=='awareness' and p['_meta']['steering']['activePromptId']==active['id']
   assert 'com.microsoft.amplifier.attention' not in p['_meta']
   assert data['trigger']['text']=='Build' and data['trigger']['sender']=='UHUMAN' and not data['trigger']['bot']
   assert source['bot'] and 'not a new human request' in p['prompt'][0]['text']
   u['authority']=data['trigger']['sender'];u['intent']='awareness'
  log('steer',u)
  if active is None:
   result={'outcome':'promptRequired','reason':'noRunningTurn'}
   if mode=='attentionRace':result['_meta']={'com.microsoft.amplifier.attention':{'id':'offer','ticketState':'restored','expiresAt':int(time.time()*1000)+60000}}
   send({'id':rid,'result':result});continue
  if mode=='disconnect':sys.exit(0)
  if mode=='timeout':continue
  if mode=='attentionConsumed':
   result={'outcome':'promptRequired','reason':'noRunningTurn','_meta':{'com.microsoft.amplifier.attention':{'id':'offer','ticketState':'consumed'}}}
  elif mode=='attentionRestore':
   result={'outcome':'promptRequired','reason':'noRunningTurn','_meta':{'com.microsoft.amplifier.attention':{'id':'offer','ticketState':'restored','expiresAt':int(time.time()*1000)+60000}}}
  elif mode=='peerIdle':result={'outcome':'promptRequired','reason':'noRunningTurn'}
  elif mode=='peerUnknown':result={'outcome':'unrecognized'}
  elif mode in ('failed','promptRequired','startedNewTurn'):result={'outcome':mode,'reason':'noRunningTurn' if mode=='promptRequired' else 'fixture'}
  elif mode in ('invalid','recovery'):result={'outcome':'unrecognized'}
  else:
   updates.append(u);result={'outcome':'injected','_meta':{'steering':{'inputId':u['id'],'state':'remoteQueued' if mode=='peerRemoteQueued' else 'received','turnId':'fixture-turn','sequence':len(updates)}}}
   receipt={'inputId':u['id'],'state':'remoteQueued' if mode=='peerRemoteQueued' else 'incorporated','turnId':'fixture-turn','sequence':len(updates)}
   update(p['sessionId'],sessionUpdate='agent_message_chunk',content={'type':'text','text':''},_meta={'io.github.bkrabach.agent-runtime':{'inputReceipt':receipt}})
   if mode in ('race','peerRace'):
    (root/'finish').write_text('go')
    def late_ack(request_id=rid,result=result):
     while active is not None:time.sleep(.01)
     time.sleep(.1);send({'id':request_id,'result':result})
    threading.Thread(target=late_ack,daemon=True).start();continue
 elif method=='session/cancel':log('cancel',{});continue
 else:result={}
 if rid is not None:send({'id':rid,'result':result})
