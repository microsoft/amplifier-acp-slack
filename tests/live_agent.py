"""Opt-in real ACP agent exercise against loopback Slack; never posts to Slack."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import shutil
import signal
import tempfile
import uuid

from aiohttp import web
from process_integration import SlackFixture, until, private_json, rows

ROOT = Path(__file__).resolve().parents[1]


async def main(args):
    if not args.allow_model_call:
        raise SystemExit('Pass --allow-model-call to authorize the selected bounded model checks.')
    fixture = SlackFixture()
    web_app = web.Application()
    web_app.router.add_route('*','/api/{method}',fixture.api)
    web_app.router.add_get('/socket/{name}',fixture.socket)
    runner = web.AppRunner(web_app)
    await runner.setup()
    site = web.TCPSite(runner,'127.0.0.1',0)
    await site.start()
    fixture.port = site._server.sockets[0].getsockname()[1]
    binary = args.binary.absolute()
    with tempfile.TemporaryDirectory(prefix='acp-slack-real-agent-') as temporary:
        home = Path(temporary)/'connector'
        init_args = [str(binary),'init','--home',str(home),'--label','Local connector validation']
        if args.agent_home:
            init_args += ['--agent-home',str(args.agent_home.absolute())]
        init = await asyncio.create_subprocess_exec(*init_args,
            stdout=asyncio.subprocess.DEVNULL)
        assert await init.wait() == 0
        c = json.loads((home/'adapter.json').read_text())
        if args.acp_config:
            c['acp'] = json.loads(args.acp_config.read_text())
        for name in args.credential_environment:
            if not name.isidentifier() or not os.environ.get(name):
                raise ValueError('Requested credential environment is missing or invalid')
            secret = 'live-check-' + name.lower().replace('_', '-')
            destination = home/'secrets'/secret
            destination.write_text(os.environ[name])
            destination.chmod(0o600)
            c['acp'].setdefault('secret_env', {})[name] = secret
        c['enabled'] = c['setup_complete'] = True
        c['slack'].update(team_id='Ttest',app_id='Apony',bot_user_id='Upony',bot_id='Bpony',
            loopback_test=True,api_base=f'http://127.0.0.1:{fixture.port}/api/',channels=['Dfirst','Dsecond','Cgroup'])
        if args.group_check:
            c['participation']='ambient'
        c['attention'].update(settle_ms=50,minimum_reply_interval_seconds=0)
        private_json(home/'adapter.json',c)
        private_json(home/'secrets/slack.json',{'app_token':'test-pony-app','bot_token':'test-pony'})
        log_path = home/'service.log'
        with log_path.open('wb') as log:
            service = await asyncio.create_subprocess_exec(str(binary),'serve','--home',str(home),stdout=log,stderr=log,start_new_session=True)
            async def await_replies(total, label, timeout=180):
                def ready():
                    failed = rows(home, "SELECT COUNT(*) FROM jobs WHERE status='failed'")[0][0]
                    assert not failed, f'{label}: an ACP turn failed; see the captured audit'
                    return len(fixture.posts) >= total
                await until(ready, label, timeout=timeout)
            try:
                await until(lambda:'pony' in fixture.sockets,'real-agent connector started')
                marker = 'ROOM_' + uuid.uuid4().hex
                await fixture.send('Dfirst',f'This is an operator-authorized local connector check. No tools or outside actions. Remember this private room marker: {marker}. Reply with your agent name and the marker.')
                await fixture.send('Dsecond','This is a separate operator-authorized local connector check. No tools or outside actions. If another room has supplied a private marker, do not guess it. Reply exactly SECOND_ROOM_READY.')
                await await_replies(2,'two real agent replies',timeout=360)
                replies={m['channel']:m['markdown_text'] for m in fixture.posts}
                assert args.expected_name.lower() in replies['Dfirst'].lower(), replies['Dfirst']
                assert marker in replies['Dfirst'], replies['Dfirst']
                assert 'SECOND_ROOM_READY' in replies['Dsecond'], replies['Dsecond']
                assert marker not in replies['Dsecond']
                bindings=rows(home,'SELECT context,session_id,resumable FROM session_bindings ORDER BY context')
                assert len(bindings)==2 and len({r[1] for r in bindings})==2
                await fixture.send('Dfirst','One final operator-authorized local check. No tools. Repeat the private room marker I gave you earlier, followed by SAME_AGENT_READY.')
                await await_replies(3,'resumed real agent reply')
                assert fixture.posts[-1]['channel']=='Dfirst' and 'SAME_AGENT_READY' in fixture.posts[-1]['markdown_text']
                assert marker in fixture.posts[-1]['markdown_text']
                after=rows(home,'SELECT context,session_id,resumable FROM session_bindings ORDER BY context')
                supports_load=all(r[2] for r in bindings)
                if supports_load:
                    assert after==bindings,'Resumable endpoint did not retain conversation sessions'
                report={'passed':True,'model_turns':3,'real_agent_name_verified':True,
                        'ordinary_acp_config':bool(args.acp_config),'private_marker_recalled':True,
                        'simultaneous_conversations':2,'independent_sessions':True,
                        'session_resume_advertised':supports_load,'session_resume_verified':supports_load,
                        'slack_posts':0,'loopback_deliveries':len(fixture.posts)}
                if args.tool_check:
                    workspace=Path(c['acp']['cwd'])
                    source=workspace/('.acp-slack-check-'+uuid.uuid4().hex+'.txt')
                    destination=source.with_suffix('.copy')
                    file_marker='FILE_'+uuid.uuid4().hex
                    source.write_text(file_marker+'\n')
                    source.chmod(0o600)
                    try:
                        instruction=f'Operator-authorized tool check: read the file {source.name} in your working directory using a tool and return its exact contents. Do not guess or inspect unrelated files.'
                        if args.tool_check=='write':
                            instruction+=f' Also use a tool to copy those same contents into {destination.name} in your working directory.'
                        await fixture.send('Dfirst',instruction)
                        await await_replies(4,'tool-backed reply',timeout=300)
                        assert file_marker in fixture.posts[-1]['markdown_text'],fixture.posts[-1]['markdown_text']
                        if args.tool_check=='write':
                            assert destination.read_text().strip()==file_marker
                        report.update(tool_read_verified=True,tool_write_verified=args.tool_check=='write')
                        report['model_turns']+=1
                    finally:
                        source.unlink(missing_ok=True)
                        destination.unlink(missing_ok=True)
                if args.group_check:
                    before=len(fixture.posts)
                    baseline=len(rows(home,"SELECT seq FROM audit WHERE kind='decision'"))
                    await fixture.send('Cgroup','Human-to-human note: Robin, lunch at noon works for me. We do not need any agent response or action on this exchange.')
                    await until(lambda:len(rows(home,"SELECT seq FROM audit WHERE kind='decision'"))>baseline,'ambient participation decision',timeout=180)
                    last=json.loads(rows(home,"SELECT data FROM audit WHERE kind='decision' ORDER BY seq DESC LIMIT 1")[0][0])
                    assert last['decision']=='pass',last
                    assert len(fixture.posts)==before
                    await fixture.send('Cgroup','<@Upony> Operator-authorized direct-address check. Reply exactly GROUP_MENTION_READY. No tools or other actions.')
                    await await_replies(before+1,'explicit group mention reply')
                    assert 'GROUP_MENTION_READY' in fixture.posts[-1]['markdown_text']
                    report.update(ambient_pass_verified=True,explicit_group_reply_verified=True)
                    report['model_turns']+=2
                report['loopback_deliveries']=len(fixture.posts)
                report['initialization_retries']=rows(home,"SELECT COUNT(*) FROM audit WHERE kind='acp_initialization_retry'")[0][0]
                print(json.dumps(report,indent=2))
                if args.output:
                    args.output.parent.mkdir(parents=True,exist_ok=True)
                    args.output.write_text(json.dumps(report,indent=2)+'\n')
            except BaseException:
                failure={'failed':True,'deliveries':len(fixture.posts),
                    'audit':rows(home,'SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 20')}
                print(json.dumps(failure,indent=2),flush=True)
                if args.output:
                    args.output.parent.mkdir(parents=True,exist_ok=True)
                    args.output.write_text(json.dumps(failure,indent=2)+'\n')
                    args.output.chmod(0o600)
                    shutil.copyfile(log_path,args.output.with_suffix('.log'))
                    args.output.with_suffix('.log').chmod(0o600)
                raise
            finally:
                if service.returncode is None:
                    service.send_signal(signal.SIGTERM)
                    try: await asyncio.wait_for(service.wait(),20)
                    except asyncio.TimeoutError: os.killpg(service.pid,signal.SIGKILL); await service.wait()
                await runner.cleanup()


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    endpoint=parser.add_mutually_exclusive_group(required=True)
    endpoint.add_argument('--agent-home',type=Path,help='Optional Agent Instance convenience integration')
    endpoint.add_argument('--acp-config',type=Path,help='Ordinary command/args/env/cwd ACP configuration; no Agent Instance required')
    parser.add_argument('--expected-name',required=True)
    parser.add_argument('--binary',type=Path,default=ROOT/'target/debug/acp-slack-adapter')
    parser.add_argument('--allow-model-call',action='store_true')
    parser.add_argument('--tool-check',choices=['read','write'],help='Exercise only uniquely named fixture files inside the configured workspace')
    parser.add_argument('--group-check',action='store_true',help='Check optional silence and explicit group addressing')
    parser.add_argument('--output',type=Path)
    parser.add_argument('--credential-environment',action='append',default=[],help='Copy only this named environment secret to the temporary connector; never print its value')
    asyncio.run(main(parser.parse_args()))
