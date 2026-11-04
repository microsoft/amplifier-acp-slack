"""Actual Socket Mode + independent decision/ACP processes on loopback only.
The deterministic helper checks transport/admission; it is not a social-quality eval.
"""
import asyncio,getpass,json,sys,tempfile,uuid
from pathlib import Path
from aiohttp import web
from process_integration import ROOT,BINARY,SlackFixture,until,private_json,rows

async def main():
 f=SlackFixture();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='participation-') as temp:
  home=Path(temp);home.chmod(0o700);(home/'secrets').mkdir();endpoint=home/'endpoint';endpoint.mkdir()
  helper=home/'decision.py';helper.write_text('''import json,sys,time
c=json.load(sys.stdin)
m=next(m for m in c['messages'] if m['id']==c['trigger_id'])
if 'decision-failure' in m['text']:sys.exit(1)
if 'delayed-admit' in m['text']:time.sleep(1)
print(json.dumps({'policy':'conversation-participation-v1','conversation_id':c['conversation_id'],'trigger_id':c['trigger_id'],'action':'engage' if 'admit' in m['text'] else 'hold'}))
''')
  config={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient','acp':{'command':sys.executable,'args':[str(ROOT/'tests/protocol_acp_fixture.py'),str(endpoint),'alpha'],'cwd':str(endpoint),'timeout_seconds':30},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'decision_service':{'command':sys.executable,'args':[str(helper)],'timeout_seconds':3}},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
  private_json(home/'adapter.json',config);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
  log=(home/'service.log').open('wb');p=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
  def count(kind):return len(rows(home,'SELECT seq FROM audit WHERE kind=?',(kind,)))
  checks=[]
  def passed(text):checks.append(text);print('PASS '+text,flush=True)
  try:
   await until(lambda:'alpha' in f.sockets,'started')
   await f.send('Cquiet','A conversation between people, nothing needed.');await until(lambda:count('participation_gate')==1,'hold')
   assert count('acp_turn_started')==0;assert not f.posts
   protocol=[json.loads(line) for line in (endpoint/'protocol.jsonl').read_text().splitlines()] if (endpoint/'protocol.jsonl').exists() else []
   assert all(row.get('kind')!='prompt' for row in protocol)
   passed('A hold uses only the decision helper: no ACP prompt or public reply')
   await asyncio.sleep(1.5);assert count('participation_decision_attempt')==1
   passed('Unchanged idle conversation makes no repeated decision or ACP requests')
   await f.send('Cgroup',"What are y'all working on? admit");await until(lambda:count('acp_turn_finished')==1,'unnamed group admission')
   assert count('acp_turn_started')==1
   passed('An unnamed group request can reach the ordinary ACP agent after decision admission')
   await f.send('Cmention','Thanks <@Ualpha>!');await until(lambda:count('participation_gate')==3,'mention hold');assert count('acp_turn_started')==1
   passed('An explicit mention is evidence, not an automatic group-model wake')
   await f.send('Cfail','decision-failure');await until(lambda:count('participation_unavailable')==1,'decision failure');assert count('acp_turn_started')==1
   passed('Decision failure is visible and never falls back to an expensive ACP prompt')
   await f.send('Dalpha','Please answer');await until(lambda:count('acp_turn_finished')==2,'direct reply');assert count('participation_decision_attempt')==4
   passed('A one-to-one DM keeps its direct reply contract without optional group gating')
   await f.send('Cstale','delayed-admit');await until(lambda:count('participation_decision_attempt')==5,'delayed gate')
   await f.send('Cstale','Never mind, already handled.');await until(lambda:count('participation_gate')>=5,'stale gate dropped');await asyncio.sleep(1)
   assert count('acp_turn_started')==2
   passed('A changed conversation invalidates delayed admission before any ACP prompt')
   print(json.dumps({'checks':checks,'decision_attempts':count('participation_decision_attempt'),'acp_turns':count('acp_turn_started')},indent=2))
  except Exception:
   print((home/'service.log').read_text(),file=sys.stderr);raise
  finally:
   p.terminate();await asyncio.wait_for(p.wait(),10);log.close();await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
