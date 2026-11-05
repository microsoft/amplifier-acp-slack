"""Peer evidence stays in one authorized turn; controls remain human UI only."""
import asyncio,getpass,json,sys,tempfile,uuid
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,ROOT,BINARY,private_json,until,rows
from steering import events

async def case(mode):
 f=SlackFixture();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket);runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 try:
  with tempfile.TemporaryDirectory(prefix='acp-peer-awareness-') as temp:
   home=Path(temp);work=home/'work';work.mkdir();home.chmod(0o700);(home/'secrets').mkdir();channel='Croom';thread='123.001'
   c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient','acp':{'command':sys.executable,'args':[str(ROOT/'tests/steering_acp_fixture.py'),mode],'cwd':str(work),'timeout_seconds':30,'client':{'approvals_required':False}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[channel],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
   private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'});log=(home/'service.log').open('wb');proc=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
   try:
    await until(lambda:'alpha' in f.sockets,'connected');await f.send(channel,'Build',thread=thread);await until(lambda:(work/'started').exists(),'original effect')
    for n,kind in enumerate(('acp_work_status','acp_permission','acp_question')):
     control={'type':'message','channel':channel,'channel_type':'channel','user':'Ualpha' if n==0 else 'Upeer','bot_id':'Balpha' if n==0 else 'Bpeer','ts':f.timestamp(),'thread_ts':thread,'text':'PRIVATE CONTROL','metadata':{'event_type':kind}}
     f.messages.append(control);await f.deliver(control)
     await f.deliver({'type':'message','subtype':'message_changed','channel':channel,'message':control,'previous_message':control,'event_ts':f.timestamp()})
     await f.deliver({'type':'message','subtype':'message_deleted','channel':channel,'deleted_ts':control['ts'],'previous_message':control,'event_ts':f.timestamp()})
    peer_ids=[]
    for n in range(3):
     if mode=='peerReactions':
      original=next(m for m in f.messages if m.get('text')=='Build')
      event={'type':'reaction_added','user':f'UPEER{n}','reaction':'eyes','item':{'type':'message','channel':channel,'ts':original['ts']}}
      event_id=await f.deliver(event);await f.deliver(event,event_id=event_id)
     else:
      m=await f.send(channel,f'peer-{n}',user=f'UPEER{n}',bot_id=f'BPEER{n}',thread=thread);peer_ids.append(m['ts'])
    if mode=='peerFallback':await until(lambda:len(rows(home,"SELECT * FROM steering_inputs WHERE state='historyOnly'"))==3,'unsupported peers retained in source history')
    else:await until(lambda:len([e for e in events(work) if e['kind']=='steer'])>=(1 if mode in ('peerRace','peerIdle','peerUnknown') else 3),'ordered awareness intake')
    (work/'finish').write_text('go');await until(lambda:len(rows(home,"SELECT * FROM jobs WHERE status IN ('running','pending')"))==0,'original turn finished')
    if mode!='peerRevision':await until(lambda:len(f.posts)==1,'single original report')
    await until(lambda:all(i.get('deleted') for i in f.indicators),'single status cleared')
    es=events(work);assert [e['text'] for e in es if e['kind']=='prompt']==['Build'];assert [e['name'] for e in es if e['kind']=='effect']==['first','second'];assert not [e for e in es if e['kind']=='cancel'];assert len([e for e in es if e['kind']=='attention'])==1
    steers=[e for e in es if e['kind']=='steer'];assert len({e['id'] for e in steers})==len(steers);assert all(e['authority']=='UHUMAN' and e['channel']==channel and e['thread']==thread for e in steers)
    if mode=='peerReactions':assert len(steers)==3 and all('reaction observation' in e['text'] for e in steers)
    if mode=='peerRevision':assert not f.posts and not rows(home,"SELECT * FROM audit WHERE kind='driver_failed'")
    if mode=='peerSupported':assert [e['text'] for e in steers]==['peer-0','peer-1','peer-2']
    if mode=='peerFallback':assert not steers
    assert len(rows(home,'SELECT * FROM jobs'))==1;assert len(rows(home,'SELECT * FROM outbox'))==(0 if mode=='peerRevision' else 1);assert len(f.indicators)==1
    assert not rows(home,"SELECT * FROM messages WHERE body LIKE '%PRIVATE CONTROL%'")
    assert all(p['channel']==channel and p['thread_ts']==thread for p in f.posts)
    trace=rows(home,"SELECT data FROM audit WHERE kind='turn_context_coverage' ORDER BY seq DESC LIMIT 1")[0][0];assert 'private source' not in trace and 'PRIVATE CONTROL' not in trace
    stages=rows(home,'SELECT state FROM steering_inputs');assert all(v[0] in ('incorporated','historyOnly','unknown') for v in stages),stages
    if mode=='peerRemoteQueued':assert all(v[0]=='unknown' for v in stages),stages
    if mode=='peerRace':assert any(v[0]=='historyOnly' for v in stages),stages
    print('PASS',mode,flush=True)
   except Exception:
    print((home/'service.log').read_text()[-3000:]);print(events(work));print(rows(home,'SELECT status,note FROM jobs'));print(rows(home,'SELECT state FROM steering_inputs'));raise
   finally:
    if proc.returncode is None:proc.terminate();await asyncio.wait_for(proc.wait(),10)
    log.close()
 finally:await runner.cleanup()
async def main():
 for mode in ('peerSupported','peerFallback','peerIdle','peerUnknown','peerRace','peerRemoteQueued','peerRevision','peerReactions'):await case(mode)
if __name__=='__main__':asyncio.run(main())
