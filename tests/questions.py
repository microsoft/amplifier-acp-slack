"""Actual adapter/ACP/socket/modal integration with a loopback Slack service."""
import asyncio,getpass,json,ssl,tempfile,uuid,sys
from pathlib import Path
from aiohttp import web,ClientSession
from client_capabilities import InteractiveSlack
from process_integration import ROOT,BINARY,until,private_json,rows

async def main():
 f=InteractiveSlack();app=web.Application();app.router.add_post('/api/{method}',f.api);app.router.add_get('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
 runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='acp-questions-') as temp:
  h=Path(temp);(h/'secrets').mkdir();h.chmod(0o700)
  c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'mentions','acp':{'command':sys.executable,'args':[str(ROOT/'tests/questions_acp_fixture.py')],'cwd':str(h),'timeout_seconds':45,'client':{'approval_users':['UHUMAN'],'permission_timeout_seconds':20}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
  private_json(h/'adapter.json',c);private_json(h/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
  log=(h/'log').open('wb');proc=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(h),stdout=log,stderr=log)
  async def api(method,path,body=None):
   ep=json.loads((h/'endpoints.json').read_text())['base_url'];trust=ssl.create_default_context(cafile=str(h/'tls/root-ca.crt'))
   async with ClientSession() as client:
    async with client.request(method,ep+'/api/v1/'+path,json=body,ssl=trust,headers={'Authorization':'Bearer '+(h/'secrets/control-token').read_text().strip()}) as r:return r.status,await r.json()
  async def view_response(post,action=None,values=None,user='UHUMAN',view='Vfixture'):
   p={'type':'block_actions' if action else 'view_submission','team':{'id':'Ttest'},'api_app_id':'Aalpha','user':{'id':user},'view':{'id':view,'callback_id':'acp_question','private_metadata':post['client_msg_id'],'state':{'values':values or {}}}}
   if action:p['actions']=[{'action_id':action,'value':post['client_msg_id']}]
   eid=str(uuid.uuid4());await f.sockets['alpha'].send_json({'type':'interactive','envelope_id':eid,'payload':p});await until(lambda:eid in f.acks,'view ack')
  async def question(n):
   await until(lambda:len(f.approvals)>=n,'question delivered');post=f.approvals[n-1]
   await f.action(post,0,action_id='acp_question_open',user={'id':'UOTHER'});assert len(f.views)==n-1
   await f.action(post,0,action_id='acp_question_open');await until(lambda:len(f.views)>=n,'private dialog');return post
  try:
   await until(lambda:'alpha' in f.sockets,'socket started');await f.send('Dprivate','Please help')
   p=await question(1);assert f.views[-1]['view']['blocks'][1]['element']['options'][0]['value']=='browser'
   await view_response(p,values={'field-0':{'value':{'selected_option':{'value':'browser'}}}})
   p=await question(2);assert 'private-login-value' in json.dumps(f.views[-1]);assert 'private-login-value' not in json.dumps(f.approvals)
   await view_response(p,action='acp_question_url');p=await question(3)
   await view_response(p,values={'field-0':{'value':{'value':'2'}}},user='UOTHER');assert not f.posts
   await view_response(p,values={'field-0':{'value':{'value':'2'}}},view='Vother');assert not f.posts
   await view_response(p,values={'field-0':{'value':{'value':'99'}}});assert not f.posts
   await view_response(p,values={'field-0':{'value':{'value':'2'}}});await until(lambda:any(p.get('markdown_text')=='Private answers received' for p in f.posts),'final reply')
   status,data=await api('GET','questions');assert status==200 and data['items']==[]
   assert not rows(h,"SELECT 1 FROM audit WHERE data LIKE '%private-login-value%'")
   assert not rows(h,"SELECT 1 FROM messages WHERE body LIKE '%needs a private response%'")
   before=rows(h,'SELECT context,endpoint,session_id FROM session_bindings');assert len(before)==1
   await f.send('Dprivate','Try this saved conversation again')
   p=await question(4);await view_response(p,action='acp_question_decline')
   await until(lambda:bool(rows(h,"SELECT 1 FROM jobs WHERE status='failed'")),'cancelled authentication ends turn')
   assert rows(h,'SELECT context,endpoint,session_id FROM session_bindings')==before
   assert not rows(h,"SELECT 1 FROM audit WHERE kind='session_replaced'")
   assert len(f.approvals)==4,'declining sign-in must not create another login question'
   print('PASS cancelled reconnect authentication keeps the original session binding')
   print('PASS agent authentication + URL + typed form over actual ACP/socket/modal flow')
   print('PASS person/view/workspace scope, invalid answers, private URL, no transcript contamination')
  except Exception:
   print((h/'log').read_text(),file=sys.stderr);print(rows(h,'SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 15'),file=sys.stderr);raise
  finally:
   if proc.returncode is None:proc.terminate();await asyncio.wait_for(proc.wait(),10)
   log.close();await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
