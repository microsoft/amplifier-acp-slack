"""Process-level interrupted-work and missed-DM recovery; no external service."""
import asyncio,getpass,json,sys,tempfile,time,uuid,sqlite3
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,ROOT,BINARY,private_json,until,rows
class Fixture(SlackFixture):
 def timestamp(self):return f'{time.time():.6f}'
 async def api(self,r):
  if r.match_info['method']=='users.conversations':return web.json_response({'ok':True,'channels':[{'id':c,'is_im':True} for c in sorted({m['channel'] for m in self.messages})]})
  return await super().api(r)
async def main():
 f=Fixture();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 try:
  with tempfile.TemporaryDirectory(prefix='acp-morning-') as temp:
   home=Path(temp);work=home/'work';work.mkdir();home.chmod(0o700);(home/'secrets').mkdir()
   c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'mentions','acp':{'command':sys.executable,'args':[str(ROOT/'tests/handoff_acp_fixture.py')],'cwd':str(work),'timeout_seconds':30,'client':{'approvals_required':False}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
   private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
   log=(home/'service.log').open('wb');process=None
   async def start():
    nonlocal process
    process=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log);await until(lambda:'alpha' in f.sockets,'started')
   async def stop():
    if process and process.returncode is None:process.terminate();await asyncio.wait_for(process.wait(),10)
    await until(lambda:'alpha' not in f.sockets,'stopped')
   try:
    await start();await f.send('Dwork','Build a branch and PR');await until(lambda:(work/'branches').exists(),'branch completed');await f.send('Dwork','Can I ask questions while you work?')
    await until(lambda:any(p['channel']=='Dwork' for p in f.posts),'successor reply',35);assert (work/'branches').read_text()=='1';assert (work/'prs').read_text()=='1';await until(lambda:len([p for p in f.posts if p['channel']=='Dwork'])==2,'queued follow-up reply')
    print('PASS ordinary follow-up preserves work, approvals, both replies and single effects',flush=True)
    (work/'branches').unlink();(work/'prs').unlink();source=await f.send('Dstop','Build a branch and PR');await until(lambda:(work/'branches').exists(),'second branch');await f.edit(source,'Stop; do not open the PR')
    await until(lambda:any(p['channel']=='Dstop' for p in f.posts),'stop acknowledged');assert not (work/'prs').exists();print('PASS source retraction stops remaining effects',flush=True)
    await stop();missed=await f.send('Doffline','Hello while offline');await f.send('Doffline','bot chatter',user='Uother',bot_id='Bother');await start()
    await until(lambda:any(p['channel']=='Doffline' for p in f.posts),'missed human DM',35);assert len([p for p in f.posts if p['channel']=='Doffline'])==1
    print('PASS reconnect answers unseen human DM without requiring a fresh message',flush=True)
    # Parent predates the reconnect history window, so only the known-thread
    # index can discover the new reply. Real history intentionally omits replies.
    old_root=f'{time.time()-172800:.6f}'
    await f.send('Dthread','Establish thread',thread=old_root)
    await until(lambda:any(p['channel']=='Dthread' for p in f.posts),'initial thread reply')
    await stop()
    with sqlite3.connect(home/'state.sqlite3') as db:
     db.execute("UPDATE settings SET value=? WHERE key='socket_checkpoint'",(str(int(time.time()*1000)),))
    missed_thread=await f.send('Dthread','Please continue here while offline',thread=old_root)
    await start()
    await until(lambda:len([p for p in f.posts if p['channel']=='Dthread'])==2,'missed thread reply',35)
    assert all(p.get('thread_ts')==old_root for p in f.posts if p['channel']=='Dthread')
    print('PASS reconnect recovers an old-root thread reply in its original thread',flush=True)
    count=len(f.posts);await stop();await start();await asyncio.sleep(2);assert len(f.posts)==count;print('PASS restart does not replay completed or accepted work',flush=True)
   except Exception:
    print((home/'service.log').read_text()[-3000:]);print((work/'fixture-error.txt').read_text() if (work/'fixture-error.txt').exists() else 'no fixture error')
    print(rows(home,"SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 20"));raise
   finally:await stop();log.close()
 finally:await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
