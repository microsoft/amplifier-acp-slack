"""Real saved Connect/Disconnect with loopback Socket Mode; no ACP/model prompt."""
import asyncio,getpass,json,sqlite3,sys,tempfile,uuid
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,until,BINARY

async def main():
 fixture=SlackFixture();app=web.Application();app.router.add_post('/api/{method}',fixture.api);app.router.add_get('/api/{method}',fixture.api);app.router.add_get('/socket/{name}',fixture.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();fixture.port=site._server.sockets[0].getsockname()[1]
 process=None
 with tempfile.TemporaryDirectory(prefix='saved-slack-process-') as tmp:
  home=Path(tmp)
  async def cli(*args,body=None):
   p=await asyncio.create_subprocess_exec(str(BINARY),'--home',str(home),*args,stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
   out,err=await asyncio.wait_for(p.communicate(None if body is None else json.dumps(body).encode()),10)
   assert p.returncode==0,err.decode();return json.loads(out)
  try:
   await cli('init','--label','alpha','--owner',getpass.getuser())
   cfg=json.loads((home/'adapter.json').read_text());cfg.update(setup_complete=True,enabled=False)
   cfg['acp'].update(command=sys.executable,args=['-c',"raise RuntimeError('No prompt is permitted by this fixture')"],cwd=str(home))
   cfg['slack'].update(team_id='Ttest',app_id='Aalpha',bot_user_id='Ualpha',bot_id='Balpha',channels=['Calpha'],loopback_test=True,api_base=f'http://127.0.0.1:{fixture.port}/api/')
   (home/'adapter.json').write_text(json.dumps(cfg));creds=json.dumps({'app_token':'test-alpha-app','bot_token':'test-alpha'});(home/'secrets/slack.json').write_text(creds);(home/'secrets/slack.json').chmod(0o600)
   process=await asyncio.create_subprocess_exec(str(BINARY),'--home',str(home),'serve',stdout=asyncio.subprocess.DEVNULL,stderr=asyncio.subprocess.DEVNULL)
   await until(lambda:(home/'endpoints.json').exists(),'local TLS')
   await cli('status')
   db=sqlite3.connect(home/'state.sqlite3');db.execute("INSERT INTO session_bindings(context,endpoint,session_id,resumable,channel,thread,updated) VALUES('room','unchanged','original',1,'Calpha','',1)");db.commit();db.close()
   def binding():
    with sqlite3.connect(home/'state.sqlite3') as db:return db.execute("SELECT * FROM session_bindings WHERE context='room'").fetchall()
   original=binding()
   entry=await cli('connection-enrollment')
   async def operation(action):
    c=dict(schema=1,connection_id=entry['id'],revision=entry['revision'],action=action,request_id=str(uuid.uuid4()))
    first=await cli('connection',action,body=c)
    assert first['context_ready' if action=='connect' else 'context_quiesced'] is True
    assert not first['settled'];c['action']='status'
    for _ in range(60):
     r=await cli('connection','status',body=c)
     if r['settled']:
      assert r['state']==('connected' if action=='connect' else 'disconnected') and not r['failed'],r;return r
     await asyncio.sleep(.1)
    raise AssertionError('Connection did not settle')
   await operation('connect');assert 'alpha' in fixture.sockets;assert not (home/'connection-gate.json').exists()
   await operation('disconnect');await until(lambda:'alpha' not in fixture.sockets,'socket closed');assert (home/'connection-gate.json').exists()
   await operation('connect');assert 'alpha' in fixture.sockets
   assert binding()==original;assert (home/'secrets/slack.json').read_text()==creds;assert fixture.posts==[] and fixture.messages==[]
   print('PASS actual fixed CLI/TLS Connect -> Disconnect -> Connect; same binding/credentials, no prompt or Slack post')
  finally:
   if process and process.returncode is None:
    process.terminate()
    try:await asyncio.wait_for(process.wait(),10)
    except TimeoutError:process.kill();await process.wait()
   await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
