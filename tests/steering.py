"""Busy bursts, ordinary ACP fallback, attribution/routing and visible uncertainty."""
import asyncio,getpass,json,sys,tempfile,uuid
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,ROOT,BINARY,private_json,until,rows

def events(work):
 p=work/'events.jsonl'
 return [json.loads(l) for l in p.read_text().splitlines()] if p.exists() else []
async def case(mode):
 f=SlackFixture();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket);runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 try:
  with tempfile.TemporaryDirectory(prefix='acp-steering-') as temp:
   home=Path(temp);work=home/'work';work.mkdir();home.chmod(0o700);(home/'secrets').mkdir()
   channel='Croom' if mode in ('attention','attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold') else 'Dwork';thread='123.001'
   c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient' if mode in ('attention','attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold') else 'mentions','acp':{'command':sys.executable,'args':[str(ROOT/'tests/steering_acp_fixture.py'),mode],'cwd':str(work),'timeout_seconds':2 if mode=='timeout' else 30,'client':{'approvals_required':False}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[channel],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
   private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'});log=(home/'service.log').open('wb');proc=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
   try:
    await until(lambda:'alpha' in f.sockets,'connected');await f.send(channel,'Build',thread=thread);await until(lambda:(work/'started').exists(),'first effect')
    for n in range(1 if mode in ('attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold') else 3):await f.send(channel,f'update-{n}',user=f'UHUMAN{n}',thread=thread)
    if mode=='attentionHold':await until(lambda:len(rows(home,"SELECT * FROM steering_inputs WHERE state='held'"))==1,'attention held')
    elif mode=='fallback':await until(lambda:len(rows(home,'SELECT * FROM steering_inputs'))==3,'durable fallback queue')
    else:await until(lambda:len([e for e in events(work)if e['kind']=='steer'])==(3 if mode in ('supported','attention') else 1),'steering intake')
    if mode in ('supported','attention'):
     es=[e for e in events(work)if e['kind']=='steer'];assert [e['text']for e in es]==['update-0','update-1','update-2'];assert [e['sender']for e in es]==['UHUMAN0','UHUMAN1','UHUMAN2'];assert all(e['channel']==channel and e['thread']==thread for e in es)
     assert len({e['id']for e in es})==3
    (work/'finish').write_text('go')
    expected=1 if mode in ('supported','attention','attentionHold') else 2 if mode in ('attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold') else 3 if mode in ('disconnect','startedNewTurn','race','timeout') else 4
    await until(lambda:len(f.posts)>=expected,'ordered results and visible errors',35)
    assert all(p['channel']==channel and p.get('thread_ts')==thread for p in f.posts)
    es=events(work);assert not [e for e in es if e['kind']=='cancel'];effects=[e['name']for e in es if e['kind']=='effect'];assert effects.count('first')==1
    if mode not in ('disconnect','startedNewTurn'):assert effects.count('second')==1
    prompts=[e['text']for e in es if e['kind']=='prompt']
    if mode in ('supported','attention'):assert prompts==['Build'];assert len(f.posts)==1
    elif mode in ('fallback','promptRequired'):assert prompts==['Build','update-0','update-1','update-2']
    elif mode=='attentionRestore':
     assert prompts==['Build','update-0'];assert len([e for e in es if e['kind']=='attention'])==2
    elif mode=='attentionRace':
     assert prompts==['Build','update-0'];assert len([e for e in es if e['kind']=='attention'])==2
    elif mode=='attentionHold':
     assert prompts==['Build'];assert len([e for e in es if e['kind']=='attention'])==2
    elif mode=='attentionContinuation':
     assert prompts==['Build','Build'];assert len([e for e in es if e['kind']=='attention'])==2
    elif mode=='attentionConsumed':
     assert prompts==['Build'];assert len([e for e in es if e['kind']=='attention'])==2
     assert any('not' in p['markdown_text'] and 'again' in p['markdown_text']for p in f.posts)
    elif mode=='race':assert prompts==['Build','update-1','update-2']
    else:
     assert 'update-0' not in prompts;assert any('not' in p['markdown_text'] and 'again' in p['markdown_text']for p in f.posts)
    if mode=='recovery':
     assert len([p for p in f.posts if 'earlier input whose outcome is uncertain' in p['markdown_text']])==2
     assert not any('private-provider-detail' in p['markdown_text'] for p in f.posts)
     notices=[p for p in f.posts if p.get('metadata',{}).get('event_type')=='acp_connector_notice']
     assert len(notices)==3
     await until(lambda:all(i.get('deleted') for i in f.indicators),'detailed notices replace temporary indicators')
     assert not any('couldn’t confirm' in i['text'] for i in f.indicator_updates)
     # Replay these diagnostics as another agent's Socket Mode messages. They
     # must not start another ACP prompt and generate another recovery notice.
     before_jobs=len(rows(home,'SELECT * FROM jobs'));before_posts=len(f.posts)
     for notice in notices:
      event=await f.send(channel,notice['markdown_text'],user='Upeer',bot_id='Bpeer',thread=thread,metadata=notice['metadata'])
      await until(lambda:bool(rows(home,'SELECT 1 FROM ignored_activity WHERE ts=?',(event['ts'],))),'peer diagnostic excluded')
     assert len(rows(home,'SELECT * FROM jobs'))==before_jobs and len(f.posts)==before_posts
    steers=[e for e in es if e['kind']=='steer'];assert len({e['id']for e in steers})==len(steers)
    if mode=='fallback':assert not steers
    elif mode in ('supported','attention'):assert len(steers)==3
    print('PASS',mode,flush=True)
   except Exception:
    print((home/'service.log').read_text()[-2000:]);print(events(work));print(rows(home,'SELECT status,note FROM jobs'));print(rows(home,'SELECT state FROM steering_inputs'));raise
   finally:
    if proc.returncode is None:proc.terminate();await asyncio.wait_for(proc.wait(),10)
    log.close()
 finally:await runner.cleanup()
async def main():
 for mode in ['supported','fallback','promptRequired','failed','invalid','startedNewTurn','disconnect','attention','attentionRestore','attentionConsumed','attentionContinuation','attentionRace','attentionHold','race','timeout','recovery']:await case(mode)
if __name__=='__main__':asyncio.run(main())
