"""Actual Slack loop/ACP/MCP continuation and failure presentation; no real model."""
import asyncio,getpass,json,os,socket,sys,tempfile,uuid
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,ROOT,BINARY,private_json,until,rows

class Fixture(SlackFixture):
    async def api(self,r):
        if r.match_info['method']=='conversations.info':
            b=dict(r.query) if r.method=='GET' else await r.json();c=b['channel']
            return web.json_response({'ok':True,'channel':{'id':c,'is_im':c.startswith('D'),'user':'UHUMAN','is_member':True,'name':c}})
        return await super().api(r)

async def main():
    f=Fixture();app=web.Application();app.router.add_get('/api/{method}',f.api);app.router.add_post('/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket)
    runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
    with tempfile.TemporaryDirectory(prefix='slack-followthrough-') as temp:
        home=Path(temp);home.chmod(0o700);(home/'secrets').mkdir();work=home/'work';work.mkdir()
        with socket.socket() as reservation:reservation.bind(('127.0.0.1',0));port=reservation.getsockname()[1]
        c={'schema':1,'id':str(uuid.uuid4()),'label':'alpha','revision':1,'enabled':True,'setup_complete':True,'participation':'ambient','acp':{'command':sys.executable,'args':[str(ROOT/'tests/followthrough_acp_fixture.py'),str(work)],'cwd':str(work),'timeout_seconds':30,'client':{'approvals_required':False}},'slack':{'team_id':'Ttest','app_id':'Aalpha','bot_user_id':'Ualpha','bot_id':'Balpha','channels':[],'allow_dms':True,'api_base':f'http://127.0.0.1:{f.port}/api/','loopback_test':True},'context':{'enabled':True,'owner_slack_user_id':'UHUMAN'},'attention':{'settle_ms':10,'minimum_reply_interval_seconds':0,'max_replies_per_agent_per_hour':100,'max_decisions_per_agent_per_hour':100},'server':{'bind':'127.0.0.1','port':port,'owner':getpass.getuser(),'pam_service':'acp-slack-adapter','public_url':f'https://127.0.0.1:{port}','embed_origins':[]}}
        private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-alpha-app','bot_token':'test-alpha'})
        process=None;log=(home/'service.log').open('wb')
        async def start(binary=BINARY):
            nonlocal process
            process=await asyncio.create_subprocess_exec(str(binary),'serve','--home',str(home),stdout=log,stderr=log)
            await until(lambda:'alpha' in f.sockets,'connector connected')
        async def stop():
            if process and process.returncode is None:process.terminate();await asyncio.wait_for(process.wait(),10)
            await until(lambda:'alpha' not in f.sockets,'connector stopped')
        def posts(ch):return [p for p in f.posts if p['channel']==ch]
        try:
            await start();await f.send('Dbackground','Run and check the result, quietly until done.')
            await until(lambda:bool(rows(home,"SELECT 1 FROM followups f JOIN jobs j ON j.id=f.parent WHERE j.status='pass'")),'quiet scheduled result check')
            rollback=os.environ.get('ACP_SLACK_TEST_ROLLBACK_BINARY')
            assert not posts('Dbackground');await stop();await start(Path(rollback) if rollback else BINARY)
            await until(lambda:len(posts('Dbackground'))==1,'failed background result reported after restart',20)
            assert 'failed' in posts('Dbackground')[0]['markdown_text'];assert (work/'Dbackground-starts').read_text()=='1';assert (work/'Dbackground-prompts').read_text()=='2'
            print('PASS durable MCP schedule + quiet terminal + restart + failed background result, one execution',flush=True)
            if rollback:
                print('PASS previous release reads new continuation journal and follows its finite rollback behavior',flush=True)
                await stop();await start()
            await f.send('Dlimit','Complete the artifact and report.')
            await until(lambda:len(posts('Dlimit'))==1,'turn-limit follow-through',20)
            assert 'existing artifact' in posts('Dlimit')[0]['markdown_text'];assert (work/'Dlimit-starts').read_text()=='1';assert (work/'Dlimit-prompts').read_text()=='2'
            assert all('PARTIAL' not in p['markdown_text'] for p in f.posts)
            print('PASS standard max-turn terminal resumes same session and inspects single existing effect',flush=True)
            for channel in ['Dquiet','Dquietpass','Dquietunsettled','Dquiettransport']:
                await f.send(channel,'Inspect the existing result and only report something useful.')
                expected='failed' if channel in ['Dquietunsettled','Dquiettransport'] else 'pass'
                await until(lambda:bool(rows(home,"SELECT 1 FROM jobs j JOIN followups f ON f.child=j.id WHERE j.status=? AND json_extract(j.trigger,'$.channel')=?",(expected,channel))),'settled quiet continuation or honest unresolved outcome',20)
                assert not posts(channel)
                assert (work/(channel+'-starts')).read_text()=='1'
                assert (work/(channel+'-prompts')).read_text()=='2'
                assert 'You have been addressed directly' not in (work/(channel+'-continuation-prompt')).read_text()
                if expected=='pass':
                    assert not rows(home,"SELECT 1 FROM audit a JOIN jobs j ON j.id=json_extract(a.data,'$.job') WHERE a.kind='driver_failed' AND json_extract(j.trigger,'$.channel')=?",(channel,))
                    await until(lambda:not rows(home,"SELECT 1 FROM response_indicators i JOIN jobs j ON j.id=i.job_id WHERE json_extract(j.trigger,'$.channel')=? AND i.state!='finished'",(channel,)),'quiet indicator cleanup')
                    assert not any(i['channel']==channel and 'couldn’t confirm' in i.get('text','') for i in f.indicator_updates)
                else:
                    failure=rows(home,"SELECT a.data FROM audit a JOIN jobs j ON j.id=json_extract(a.data,'$.job') WHERE a.kind='driver_failed' AND json_extract(j.trigger,'$.channel')=?",(channel,))
                    assert json.loads(failure[-1][0])['delivery']=='unknown'
            await f.send('Dempty','Please answer this direct request.')
            await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE status='failed' AND json_extract(trigger,'$.channel')='Dempty'")),'missing direct answer remains a failure')
            failure=rows(home,"SELECT a.data FROM audit a JOIN jobs j ON j.id=json_extract(a.data,'$.job') WHERE a.kind='driver_failed' AND json_extract(j.trigger,'$.channel')='Dempty'")
            assert json.loads(failure[-1][0])['category']=='invalid_answer'
            assert not posts('Dempty') and (work/'Dempty-prompts').read_text()=='1'
            print('PASS quiet accepted checks finish without filler or false failure; unresolved tools/transport and unanswered direct requests stay distinct',flush=True)
            for channel in ['Cpeer','Cpeerdeleted']:
                await f.send(channel,'Please check the existing artifact.',user='Ubeta',bot_id='Bbeta')
                await until(lambda:bool(rows(home,"SELECT 1 FROM followups f JOIN jobs j ON j.id=f.parent WHERE j.status='pass' AND json_extract(j.trigger,'$.channel')=?",(channel,))),'accepted peer quiet check')
            assert not posts('Cpeer') and not posts('Cpeerdeleted')
            await stop()
            f.messages=[m for m in f.messages if not (m.get('channel')=='Cpeerdeleted' and m.get('user')=='Ubeta')]
            await start()
            await until(lambda:len(posts('Cpeer'))==1,'peer continuation after restart',20)
            await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE status='failed' AND json_extract(trigger,'$.channel')='Cpeerdeleted'")),'deleted peer source refusal')
            assert (work/'Cpeer-starts').read_text()=='1' and (work/'Cpeer-prompts').read_text()=='2'
            assert (work/'Cpeerdeleted-prompts').read_text()=='1' and not posts('Cpeerdeleted')
            print('PASS peer MCP continuation retains exact source/session across restart; deleted source cannot run',flush=True)
            for channel in ['Crefused','Cgeneric']:
                await f.send(channel,'Please review this.')
                await until(lambda:bool(rows(home,"SELECT 1 FROM jobs WHERE status='failed' AND json_extract(trigger,'$.channel')=?",(channel,))),'failure recorded')
                await until(lambda:any(i['channel']==channel and ('did not forward' in i.get('text','') if channel=='Crefused' else 'couldn’t confirm' in i.get('text','')) for i in f.indicators),'accurate public outcome')
                assert (work/(channel+'-prompts')).read_text()=='1'
            assert not posts('Crefused') and not posts('Cgeneric')
            print('PASS exact no-forward marker has honest status; same generic error remains uncertain; neither retries',flush=True)
            await asyncio.sleep(1);assert len(posts('Dbackground'))==len(posts('Dlimit'))==1
        except BaseException:
            print((home/'service.log').read_text(),file=sys.stderr);print(rows(home,'SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 12'),file=sys.stderr);raise
        finally:await stop();log.close();await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
