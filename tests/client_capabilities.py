"""Real adapter + independent ACP peer + Slack Socket Mode fixture.

Tests permission provenance/lifecycle and standard local client callbacks. No
credentials, external Slack messages or model calls. Requires aiohttp.
"""
import asyncio, getpass, json, os, shutil, ssl, sys, tempfile, uuid
from pathlib import Path
from aiohttp import web, ClientSession
from process_integration import SlackFixture, until, rows, count, private_json, BINARY, ROOT

class InteractiveSlack(SlackFixture):
    def __init__(self):
        super().__init__();self.approvals=[];self.updated=[];self.views=[]
    async def api(self,request):
        if request.method=='POST' and request.match_info['method'] in ('chat.postMessage','chat.update','views.open'):
            body=await request.json()
            if request.match_info['method']=='views.open':
                self.views.append(body);return web.json_response({'ok':True,'view':{'id':'Vfixture'}})
            if 'blocks' in body:
                if request.match_info['method']=='chat.update':
                    self.updated.append(body);return web.json_response({'ok':True,'ts':body['ts']})
                assert 'markdown_text' not in body
                message={'type':'message','channel':body['channel'],'channel_type':'im','user':'Ualpha','bot_id':'Balpha','text':body['text'],'ts':self.timestamp(),'client_msg_id':body['client_msg_id'],'metadata':body['metadata']}
                if body.get('thread_ts'):message['thread_ts']=body['thread_ts']
                self.messages.append(message)
                self.approvals.append({**body,'ts':message['ts']})
                await self.deliver(message)
                return web.json_response({'ok':True,'ts':message['ts']})
        return await super().api(request)
    async def action(self,post,index,action_id='acp_permission',**overrides):
        payload={'type':'block_actions','team':{'id':'Ttest'},'api_app_id':'Aalpha','user':{'id':'UHUMAN'},'container':{'type':'message','channel_id':post['channel'],'message_ts':post['ts']},'trigger_id':'fixture-trigger','actions':[{'action_id':action_id,'value':post['client_msg_id']+(':'+str(index) if action_id=='acp_permission' else '')}]}
        payload.update(overrides)
        eid=str(uuid.uuid4());await self.sockets['alpha'].send_json({'type':'interactive','envelope_id':eid,'payload':payload})
        await until(lambda:eid in self.acks,'interaction acknowledged')

async def main():
    fixture=InteractiveSlack();app=web.Application();app.router.add_post('/api/{method}',fixture.api);app.router.add_get('/api/{method}',fixture.api);app.router.add_get('/socket/{name}',fixture.socket)
    runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();fixture.port=site._server.sockets[0].getsockname()[1]
    checks=[]
    def passed(name):checks.append(name);print('PASS '+name,flush=True)
    with tempfile.TemporaryDirectory(prefix='acp-client-capabilities-') as tmp:
        base=Path(tmp);home=base/'adapter';workspace=base/'workspace'
        for p in (home,home/'secrets',workspace):p.mkdir();p.chmod(0o700)
        (workspace/'escape').symlink_to(home,target_is_directory=True)
        config={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'mentions','acp':{'command':sys.executable,'args':[str(ROOT/'tests/capability_acp_fixture.py'),str(workspace)],'cwd':str(workspace),'timeout_seconds':40,'client':{'read_text_file':True,'write_text_file':True,'terminal':True,'workspace_roots':[str(workspace)],'approval_users':['UHUMAN'],'permission_timeout_seconds':4,'terminal_timeout_seconds':10}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{fixture.port}/api/','loopback_test':True},'attention':{'parallel_contexts':2,'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_context_per_hour':100,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':200,'max_bot_replies_without_human':4},'server':{'bind':'127.0.0.1','port':0,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':None,'embed_origins':[]}}
        private_json(home/'adapter.json',config);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
        log=(home/'service.log').open('wb');process=None
        async def start():
            nonlocal process
            process=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
            await until(lambda:'alpha' in fixture.sockets or process.returncode is not None,'service started');assert process.returncode is None
        async def stop():
            if process and process.returncode is None:process.terminate();await asyncio.wait_for(process.wait(),10)
            await until(lambda:'alpha' not in fixture.sockets,'service stopped')
        async def api(method,path,body=None):
            endpoint=json.loads((home/'endpoints.json').read_text())['base_url'];trust=ssl.create_default_context(cafile=str(home/'tls/root-ca.crt'));token=(home/'secrets/control-token').read_text().strip()
            async with ClientSession() as client:
                async with client.request(method,endpoint+'/api/v1/'+path,json=body,ssl=trust,headers={'Authorization':'Bearer '+token}) as r:
                    return r.status,await r.json()
        async def permission(channel,text='permission requested'):
            prior=len(fixture.approvals);m=await fixture.send(channel,text)
            await until(lambda:len(fixture.approvals)>prior,'permission displayed');post=fixture.approvals[-1]
            await until(lambda:count(home,'SELECT COUNT(*) FROM permissions WHERE id=? AND slack_ts IS NOT NULL',(post['client_msg_id'],))==1,'permission bound to Slack message')
            return m,post
        try:
            await start()
            await fixture.send('Dtools','callbacks check')
            await until(lambda:any(p.get('markdown_text')=='callbacks verified' for p in fixture.posts),'client callbacks',timeout=30)
            assert (workspace/'client-file.txt').read_text()=='first\nsecond\n';assert not (home/'no.txt').exists()
            passed('Filesystem line ranges, cross-root and symlink rejection; terminal isolation, output, release and concurrent wait/kill')
            _,post=await permission('Dapproval')
            assert all(b.get('elements',[{}])[0].get('value','').split(':')[-1]!='2' for b in post['blocks'])
            for override in ({'user':{'id':'UOTHER'}},{'container':{'channel_id':'Dother','message_ts':post['ts']}},{'team':{'id':'Tother'}},{'api_app_id':'Aother'}):
                await fixture.action(post,0,**override)
                assert count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='pending'",(post['client_msg_id'],))==1
            assert not (workspace/'approved.txt').exists()
            await fixture.action(post,0,action_id='permission_details',user={'id':'UOTHER'})
            await asyncio.sleep(.1);assert not fixture.views
            await fixture.action(post,0,action_id='permission_details')
            await until(lambda:bool(fixture.views),'private review modal')
            assert 'Explicitly approved' in json.dumps(fixture.views[-1])
            await fixture.action(post,0)
            await until(lambda:(workspace/'approved.txt').exists(),'approved tool performed')
            await fixture.action(post,0)
            await until(lambda:any(p.get('channel')=='Dapproval' for p in fixture.updated),'buttons removed')
            assert count(home,"SELECT COUNT(*) FROM permissions WHERE status='approved'")==1
            closed=next(p for p in fixture.updated if p['channel']=='Dapproval')
            assert 'Create the harmless approval-check file' in json.dumps(closed),closed
            assert 'permission_details' in json.dumps(closed) and 'acp_permission' not in json.dumps(closed)
            assert 'not whether the action ran' in json.dumps(closed)
            views_before=len(fixture.views)
            await fixture.action(post,0,action_id='permission_details')
            await until(lambda:len(fixture.views)>views_before,'closed request details available')
            passed('Private Slack details show merged tool-call arguments only to the initiating authorized human')
            passed('Approval requires the initiating allowlisted human and exact app/workspace/message; replay and persistent grants rejected')
            (workspace/'approved.txt').unlink()
            _,post=await permission('Ddenial');await fixture.action(post,1)
            await until(lambda:count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='denied'",(post['client_msg_id'],))==1,'denial recorded')
            await until(lambda:any(p['channel']=='Ddenial' for p in fixture.posts),'denied turn returns')
            assert not (workspace/'approved.txt').exists();passed('Explicit denial returns the endpoint’s reject option without running the tool')
            _,post=await permission('Dcancelled','permission requested, cancelled turn');await fixture.action(post,1)
            await until(lambda:count(home,"SELECT COUNT(*) FROM jobs WHERE status='speak' AND note='ACP turn cancelled'")==1,'cancelled turn finishes')
            await until(lambda:any(p['channel']=='Dcancelled' for p in fixture.posts),'cancellation notice delivered')
            assert all('stopped before finishing' in p.get('markdown_text','') for p in fixture.posts if p['channel']=='Dcancelled')
            assert count(home,"SELECT COUNT(*) FROM audit WHERE kind='acp_turn_cancelled'")==1
            assert count(home,"SELECT COUNT(*) FROM audit WHERE kind='driver_failed'")==0
            passed('A cancelled direct turn explains that work stopped, without posting partial agent output or retrying')
            _,post=await permission('Dexpire')
            await until(lambda:count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='expired'",(post['client_msg_id'],))==1,'permission expires',timeout=10)
            await fixture.action(post,0);assert not (workspace/'approved.txt').exists();passed('Unanswered requests expire and cannot be approved later')
            _,post=await permission('Dadmin');status,data=await api('GET','permissions');assert status==200
            matching=next(p for p in data['items'] if p['id']==post['client_msg_id']);assert matching['request']['toolCall']['rawInput']['content']=='Explicitly approved'
            status,data=await api('POST',f"permissions/{post['client_msg_id']}/decision",{'option_id':'no'});assert status==200,data
            assert rows(home,'SELECT actor FROM permissions WHERE id=?',(post['client_msg_id'],))[0][0]=='token:local-control'
            passed('Dashboard/CLI/API decisions use explicit administrator authority, not a caller-supplied Slack identity')
            m,post=await permission('Dedit');await fixture.edit(m,'Changed my mind, stop')
            await until(lambda:count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='cancelled'",(post['client_msg_id'],))==1,'source change cancels')
            await fixture.action(post,0);assert not (workspace/'approved.txt').exists();passed('Editing the source invalidates an outstanding request')
            _,post=await permission('Drestart');await stop()
            assert count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='cancelled'",(post['client_msg_id'],))==1
            await start();await fixture.action(post,0);assert not (workspace/'approved.txt').exists();passed('Stopping/restarting cancels pending permissions and never replays work')
            assert count(home,"SELECT COUNT(*) FROM messages WHERE body LIKE '%Agent permission needs your decision%'")==0
            passed('Connector control cards stay outside conversation history and cannot make a completed reply stale')
            prior=len(fixture.approvals);await fixture.send('Dambiguous','ambiguous permission')
            await until(lambda:any(p.get('markdown_text')=='ambiguous options refused' for p in fixture.posts),'ambiguous options refused')
            assert len(fixture.approvals)==prior
            passed('Duplicate permission option IDs cannot relabel a rejection as approval')
            await stop()
            config['revision']+=1;config['acp']['timeout_seconds']=1
            private_json(home/'adapter.json',config)
            await start()
            _,post=await permission('Dlongwait')
            await asyncio.sleep(2)
            assert count(home,"SELECT COUNT(*) FROM permissions WHERE id=? AND status='pending'",(post['client_msg_id'],))==1
            await fixture.action(post,0)
            await until(lambda:any(p.get('channel')=='Dlongwait' for p in fixture.posts),'reply after wait longer than execution budget')
            assert (workspace/'approved.txt').exists()
            passed('Human approval waiting does not consume the agent execution budget')
            prior=len(fixture.approvals)
            _,first=await permission('Dsuccessive','successive permission requests')
            await asyncio.sleep(2)
            await fixture.action(first,0)
            await until(lambda:len(fixture.approvals)==prior+2,'second permission displayed')
            second=fixture.approvals[-1]
            first_row=rows(home,'SELECT created,closed_at FROM permissions WHERE id=?',(first['client_msg_id'],))[0]
            second_row=rows(home,'SELECT created,expires FROM permissions WHERE id=?',(second['client_msg_id'],))[0]
            remaining=4000-(first_row[1]-first_row[0])
            assert second_row[1]-second_row[0]==remaining
            assert 0 < remaining < 2500
            label='less than a second' if remaining<1000 else ('1 second' if remaining<2000 else f'{remaining//1000} seconds')
            assert f'About {label} remains' in json.dumps(second),second
            await until(lambda:any(p.get('channel')=='Dsuccessive' for p in fixture.posts),'successive permissions finish')
            output=next(p['markdown_text'] for p in fixture.posts if p.get('channel')=='Dsuccessive')
            outcomes=json.loads(output.removeprefix('successive permissions '))
            assert outcomes==[{'outcome':'selected','optionId':'yes'},{'outcome':'cancelled'},{'outcome':'cancelled'}],outcomes
            assert len(fixture.approvals)==prior+2,'Exhausted budget must not offer a new card'
            passed('Successive permissions share one deadline budget; exhausted requests cancel without a misleading new card')
            print(json.dumps({'passed':len(checks),'checks':checks},indent=2))
        except Exception:
            print((home/'service.log').read_text(),file=sys.stderr)
            print(rows(home,'SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 20'),file=sys.stderr)
            raise
        finally:
            await stop();log.close();await runner.cleanup()

if __name__ == "__main__":
    asyncio.run(main())
