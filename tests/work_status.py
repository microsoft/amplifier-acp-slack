"""Actual process/ACP/Socket Mode status transitions, exclusively on loopback."""
import asyncio,getpass,json,ssl,sys,tempfile,time,uuid
from pathlib import Path
from aiohttp import web,ClientSession
from client_capabilities import InteractiveSlack
from process_integration import ROOT,BINARY,until,private_json,rows

class StatusSlack(InteractiveSlack):
 def __init__(self):super().__init__();self.statuses=[];self.error=None;self.rate_once=False;self.uncertain_once=False;self.delivery_gate=None;self.delivery_started=False
 async def api(self,request):
  if request.match_info['method']=='agents.sessions.setStatus':
   assert request.headers['Authorization'].startswith('Bearer test-')
   body=await request.json();assert set(body)=={'channel_id','thread_ts','status'}
   self.statuses.append({**body,'at':time.monotonic()})
   if self.rate_once:self.rate_once=False;return web.Response(status=429,headers={'Retry-After':'1'})
   if self.uncertain_once:self.uncertain_once=False;return web.Response(status=503)
   if self.error:return web.json_response({'ok':False,'error':self.error})
   return web.json_response({'ok':True,'status':body['status'],'agent_status':body['status']})
  if request.match_info['method']=='chat.postMessage' and self.delivery_gate and 'markdown_text' in await request.json():
   self.delivery_started=True;await self.delivery_gate.wait()
  return await super().api(request)

async def main(host_attention=False):
 f=StatusSlack();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='acp-status-') as temp:
  home=Path(temp);(home/'secrets').mkdir();home.chmod(0o700)
  config={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient','acp':{'command':sys.executable,'args':[str(ROOT/'tests/work_status_acp_fixture.py')]+(['--host-attention'] if host_attention else []),'cwd':str(home),'timeout_seconds':60,'client':{'approvals_required':True,'permission_timeout_seconds':20}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
  private_json(home/'adapter.json',config);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
  log=(home/'service.log').open('wb');process=None;checks=[]
  def passed(text):checks.append(text);print('PASS '+text,flush=True)
  async def start():
   nonlocal process
   process=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
   await until(lambda:'alpha' in f.sockets,'started')
  async def stop(kill=False):
   if process and process.returncode is None:
    process.kill() if kill else process.terminate();await asyncio.wait_for(process.wait(),10)
   await until(lambda:'alpha' not in f.sockets,'stopped')
  async def api(method,path,body=None):
   base=json.loads((home/'endpoints.json').read_text())['base_url'];ctx=ssl.create_default_context(cafile=str(home/'tls/root-ca.crt'))
   async with ClientSession() as client:
    async with client.request(method,base+'/api/v1/'+path,json=body,ssl=ctx,headers={'Authorization':'Bearer '+(home/'secrets/control-token').read_text().strip()}) as response:
     assert response.status==200;return await response.json()
  def transitions(channel):return [s['status'] for s in f.statuses if s['channel_id']==channel]
  async def state(channel,want):await until(lambda:transitions(channel) and transitions(channel)[-1]==want,channel+' '+want)
  original_send=f.send
  async def threaded_send(channel,text,**kwargs):
   kwargs.setdefault('thread','1800000000.123456')
   return await original_send(channel,text,**kwargs)
  f.send=threaded_send
  try:
   await start();m=await f.send('Dnormal','Please help');await state('Dnormal','active')
   assert transitions('Dnormal')==['processing','active'];assert all(s['thread_ts']=='1800000000.123456' for s in f.statuses if s['channel_id']=='Dnormal')
   assert next(p for p in f.posts if p['channel']=='Dnormal').get('thread_ts','')=='1800000000.123456'
   passed('Existing-thread work changes processing to active without changing reply placement')
   previous=len(f.statuses);await f.send('Cambient','People are talking');await until(lambda:bool(rows(home,"SELECT id FROM jobs WHERE status='pass'")),'ambient pass');await asyncio.sleep(.4);assert len(f.statuses)==previous
   passed('Ambient silent assessments emit no work indicator')
   m=await f.send('Cthread','<@Ualpha> Please help',thread='1800000000.123456');await state('Cthread','active');assert all(s['thread_ts']=='1800000000.123456' for s in f.statuses if s['channel_id']=='Cthread')
   passed('Addressed channel work uses the existing thread root')
   # Quiet evaluation stays invisible; an accepted ambient response is observable
   # in precisely the destination receiving the response, including top-level posts.
   for channel,thread in [('Cengage','1800000000.123456'),('Ctop','')]:
    before=len(f.statuses)
    await f.send(channel,'Please contribute detailed work',thread=thread)
    await until(lambda:any(i.get('deleted') for i in f.indicators if i['channel']==channel),'ambient indicator cleanup')
    updates=[u for u in f.indicator_updates if u['channel']==channel]
    assert any(u['text']=='Running · Run unit tests (workspace_run)' for u in updates),updates
    assert all(u.get('thread_ts','')==thread for u in updates),updates
    if thread:await state(channel,'active')
    else:assert len(f.statuses)==before
    assert any(p['channel']==channel and p.get('thread_ts','')==thread for p in f.posts)
   passed('Accepted ambient turns show scoped tool activity and clean up after delivery, with and without host attention')
   before=(len(f.statuses),len(f.indicators));await f.send('Chold','People are talking about detailed work')
   await until(lambda:bool(rows(home,"SELECT id FROM jobs WHERE status='pass' AND json_extract(trigger,'$.channel')='Chold'")),'quiet hold')
   await asyncio.sleep(.3);assert before==(len(f.statuses),len(f.indicators))
   passed('Held ambient events never publish temporary messages or native status')
   await f.send('Dtools','Please show detailed work');await state('Dtools','active')
   await until(lambda:any(i.get('deleted') for i in f.indicators if i['channel']=='Dtools'),'detailed line cleared')
   labels=[u['text'] for u in f.indicator_updates if u['channel']=='Dtools']
   assert 'Running · Run unit tests (workspace_run)' in labels,labels
   assert 'Reading · Read Dtools notes <!channel> · +1 tool' in labels,labels
   assert all(u.get('thread_ts')=='1800000000.123456' for u in f.indicator_updates if u['channel']=='Dtools')
   passed('Native enum-only thread status is supplemented by one scoped tool-detail line and cleaned up')
   await f.send('Dpermission','Need permission');await state('Dpermission','suspended');n=len(f.statuses);await asyncio.sleep(.6);assert len(f.statuses)==n
   await f.action(f.approvals[-1],0);await state('Dpermission','active');assert transitions('Dpermission')==['processing','suspended','processing','active']
   passed('Human approvals suspend work and resume it only after a real decision')
   await f.send('Dquestion','Need a question');await state('Dquestion','suspended');pending=await api('GET','questions');assert len(pending['items'])==1
   await api('POST','questions/'+pending['items'][0]['id']+'/decision',{'action':'decline'});await state('Dquestion','active');assert transitions('Dquestion')==['processing','suspended','processing','active']
   passed('Pending ACP questions use suspended status until answered')
   f.delivery_gate=asyncio.Event();await f.send('Ddelivery','Please help');await until(lambda:f.delivery_started,'delivery started');assert transitions('Ddelivery')[-1]=='processing';await asyncio.sleep(.5);assert transitions('Ddelivery')[-1]=='processing';f.delivery_gate.set();await state('Ddelivery','active');f.delivery_gate=None
   passed('Work remains processing until its pending Slack reply is delivered')
   await f.send('Dcancel','stall');await state('Dcancel','processing');await api('POST','lifecycle',{'action':'stop'});await state('Dcancel','active');await api('POST','lifecycle',{'action':'start'});await until(lambda:'alpha' in f.sockets,'resumed')
   passed('Actual worker cancellation on connector pause clears processing')
   await f.send('Dshutdown','stall');await state('Dshutdown','processing');await stop();assert transitions('Dshutdown')[-1]=='active';await start()
   passed('Graceful shutdown clears processing without posting a message')
   await f.send('Dcrash','stall');await state('Dcrash','processing');before=len(f.posts);await stop(kill=True);await start();await state('Dcrash','active');assert len(f.posts)==before
   passed('Restart reconciles durable stale indicators without replaying interrupted work')
   f.rate_once=True;await f.send('Drate','Please help');await state('Drate','active');values=[s for s in f.statuses if s['channel_id']=='Drate'];assert values[1]['at']-values[0]['at']>=.95
   passed('Retry-After is honored, and completed work replaces a delayed processing state')
   f.uncertain_once=True;f.error='feature_disabled';await f.send('Duncertain','Please help')
   await until(lambda:bool(rows(home,"SELECT 1 FROM work_status_control WHERE disabled=1")),'uncertain delivery then rejection')
   await until(lambda:any(p['channel']=='Duncertain' for p in f.posts),'uncertain work completed')
   await asyncio.sleep(.4);assert rows(home,"SELECT dirty FROM work_status_delivery WHERE channel='Duncertain'")==[(1,)]
   f.error=None;updated=await api('GET','config');updated['label']='alpha reconnected';await api('PUT','config',{'expected_revision':updated['revision'],'config':updated})
   await state('Duncertain','active');await until(lambda:not rows(home,"SELECT 1 FROM work_status_delivery WHERE channel='Duncertain'"),'uncertain cleanup retired')
   passed('An uncertain earlier delivery survives permanent rejection until restored access confirms active cleanup')
   await until(lambda:'alpha' in f.sockets,'reconfigured socket connected');await asyncio.sleep(.5)
   f.error='feature_disabled';await f.send('Dunsupported','Please help');await until(lambda:bool(transitions('Dunsupported')),'unsupported request');n=len(f.statuses);await f.send('Dunsupported2','Please help');await until(lambda:any(p['channel']=='Dunsupported2' for p in f.posts),'work continues');await asyncio.sleep(.6);assert len(f.statuses)==n
   passed('Unavailable workspace capability stops attempts and does not prevent agent replies')
   await until(lambda:not rows(home,'SELECT 1 FROM work_status_delivery') and not rows(home,'SELECT 1 FROM work_status_jobs'),'settled indicators retired')
   passed('Settled and unavailable indicators do not accumulate per-message tracking rows')
   print(json.dumps({'passed':len(checks),'checks':checks},indent=2))
  except Exception:
   print((home/'service.log').read_text(),file=sys.stderr);print(f.statuses,file=sys.stderr);raise
  finally:await stop();log.close();await runner.cleanup()
if __name__=='__main__':asyncio.run(main('--host-attention' in sys.argv))
