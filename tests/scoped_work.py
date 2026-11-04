"""End-to-end ordinary ACP + Slack-loopback response visibility and follow-through."""
import asyncio,getpass,json,os,ssl,tempfile,time,sys,uuid
from pathlib import Path
from aiohttp import web,ClientSession
from process_integration import ROOT,BINARY,SlackFixture,until,private_json,rows

class Fixture(SlackFixture):
 def __init__(self):super().__init__();self.statuses=[];self.lost=False;self.revoked=set()
 async def api(self,request):
  method=request.match_info['method'];body=dict(request.query) if request.method=='GET' else await request.json()
  if method=='conversations.members' and body.get('channel') in self.revoked:
   return web.json_response({'ok':True,'members':['Ualpha'],'response_metadata':{'next_cursor':''}})
  if method=='agents.sessions.setStatus':
   self.statuses.append(body);return web.json_response({'ok':False,'error':'not_authorized'})
  if method=='chat.postMessage' and body.get('metadata',{}).get('event_type')!='acp_work_status' and self.lost:
   self.lost=False;self.posts.append(body)
   self.messages.append({**body,'ts':self.timestamp(),'text':body['markdown_text'],'user':'Ualpha','bot_id':'Balpha'})
   return web.Response(status=503) # accepted remotely, no socket echo, lost HTTP receipt
  return await super().api(request)

async def main():
 f=Fixture();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='acp-scoped-work-') as temp:
  home=Path(temp);(home/'secrets').mkdir();home.chmod(0o700)
  c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient','acp':{'command':sys.executable,'args':[str(ROOT/'tests/work_status_acp_fixture.py')],'cwd':str(home),'timeout_seconds':60,'client':{'approvals_required':True,'permission_timeout_seconds':20}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
  private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
  log=(home/'service.log').open('wb');process=None
  async def start():
   nonlocal process
   process=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log);await until(lambda:'alpha' in f.sockets,'started')
  async def stop():
   if process and process.returncode is None:process.kill();await process.wait()
   await until(lambda:'alpha' not in f.sockets,'stopped')
  def indicators(ch):return [i for i in f.indicators if i['channel']==ch]
  def posts(ch):return [p for p in f.posts if p['channel']==ch]
  def finished(ch):return indicators(ch) and all(i.get('deleted') for i in indicators(ch))
  try:
   await start()
   await f.send('Dnormal','Please help');await until(lambda:finished('Dnormal'),'DM indicator cleared')
   assert len(indicators('Dnormal'))==1 and not indicators('Dnormal')[0].get('thread_ts');assert not f.statuses
   print('PASS DM work stays in the main DM, with one temporary indicator and no synthetic thread',flush=True)
   before=len(f.indicators);await f.send('Cquiet','People are discussing detailed options');await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE status='pass'")),'quiet pass');assert len(f.indicators)==before
   print('PASS Quiet participation emits no public activity',flush=True)
   await f.send('Cgroup','Please contribute to this idea',thread='1800000000.000001');await asyncio.sleep(.3);assert not indicators('Cgroup')
   await until(lambda:finished('Cgroup'),'group done');assert indicators('Cgroup')[0]['thread_ts']=='1800000000.000001';assert len(f.statuses)==1
   print('PASS Group assessment is private; accepted reply has exact-thread fallback despite rejected native status',flush=True)
   await f.send('Dtools','Please show detailed work')
   await f.send('Ctools','<@Ualpha> Please show detailed work',thread='1800000000.000002')
   await until(lambda:finished('Dtools') and finished('Ctools'),'detailed activity finished',timeout=25)
   for ch,thread in [('Dtools',''),('Ctools','1800000000.000002')]:
    updates=[u for u in f.indicator_updates if u['channel']==ch]
    assert len(indicators(ch))==1 and all(u.get('thread_ts','')==thread for u in updates)
    labels=[u['text'] for u in updates]
    assert 'Running · Run unit tests (workspace_run)' in labels,labels
    assert 'Reading · Read '+ch+' notes <!channel> · +1 tool' in labels,labels
    assert 'Preparing the response…' in labels,labels
    assert all('\n' not in label and len(label)<=180 for label in labels)
    assert all(marker not in label for label in labels for marker in ['PRIVATE_ARGUMENT','PRIVATE_RESULT','WRONG_SESSION','Ctools' if ch=='Dtools' else 'Dtools'])
   print('PASS Partial ACP updates retain tool labels, concurrent tools share one plain-text line, and simultaneous conversations stay isolated',flush=True)
   # A failed actual turn leaves an honest scoped outcome instead of vanishing.
   await f.send('Dfail','Please fail');await until(lambda:indicators('Dfail') and 'couldn’t confirm' in indicators('Dfail')[0].get('text',''),'visible failure')
   assert not indicators('Dfail')[0].get('deleted')
   print('PASS Failure changes the working message into a visible outcome',flush=True)
   # Persisted active work gets a failure outcome after process death, not replay.
   await f.send('Dcrash','stall');await until(lambda:bool(indicators('Dcrash')),'working before crash');await stop();await start();await until(lambda:'couldn’t confirm' in indicators('Dcrash')[0].get('text',''),'interrupted outcome')
   assert len(indicators('Dcrash'))==1
   print('PASS Crash cleanup reports interrupted work without replaying it',flush=True)
   f.lost=True;await f.send('Dreceipt','Please help');await until(lambda:bool(rows(home,"SELECT 1 FROM outbox WHERE channel='Dreceipt' AND status='sent'")),'lost receipt recovered',timeout=40)
   assert len(posts('Dreceipt'))==1
   print('PASS Accepted Slack message with lost HTTP/socket receipts is recovered from history without duplicate posting',flush=True)
   await f.send('Dfollow','Please followup');await until(lambda:len(posts('Dfollow'))==1,'initial followup response');await stop();await start();await until(lambda:len(posts('Dfollow'))==2,'durable continuation',timeout=15)
   assert all(not p.get('thread_ts') for p in posts('Dfollow'));await asyncio.sleep(1);assert len(posts('Dfollow'))==2
   print('PASS A bounded continuation survives restart and finishes in its original DM without another human message',flush=True)
   await f.send('Drevoked','Please followup');await until(lambda:len(posts('Drevoked'))==1,'initial revoked response');f.revoked.add('Drevoked')
   await f.send('Ddeleted','Please followup');await until(lambda:len(posts('Ddeleted'))==1,'initial deleted response');f.messages=[m for m in f.messages if not (m.get('channel')=='Ddeleted' and m.get('user')=='UHUMAN')]
   await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE json_extract(trigger,'$.channel')='Drevoked' AND status='failed'")),'revoked followup rejected',timeout=15)
   assert len(posts('Drevoked'))==1
   print('PASS A revoked source audience stops a scheduled continuation before another ACP turn',flush=True)
   await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE json_extract(trigger,'$.channel')='Ddeleted' AND status='failed'")),'deleted source rejected',timeout=15)
   assert len(posts('Ddeleted'))==1
   print('PASS A deleted source stops a scheduled continuation before another ACP turn',flush=True)
   assert not any('users.setPresence' in s for s in f.statuses)
  except BaseException:
   print((home/'service.log').read_text(),file=sys.stderr);print(json.dumps(f.indicators,indent=2),file=sys.stderr);raise
  finally:await stop();log.close();await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
