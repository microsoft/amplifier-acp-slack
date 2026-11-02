"""Local Slack/ACP process tests for attachment trust, capability and delivery boundaries."""
import asyncio,base64,json,os,signal,tempfile
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,until,private_json,rows,BINARY,ROOT
PNG=base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5WQAAAAASUVORK5CYII=')
WAV=b'RIFF\x04\x00\x00\x00WAVE'
class Fixture(SlackFixture):
 def __init__(self):super().__init__();self.files={};self.downloads=[];self.uploads=[];self.completions=[];self.fail_completion=False
 async def api(self,request):
  method=request.match_info['method']
  if method=='files.info':
   assert request.headers.get('Authorization')=='Bearer test-pony';return web.json_response({'ok':True,'file':self.files[request.query['file']]})
  if method=='files.getUploadURLExternal':
   body=await request.post();assert body['filename'].endswith('.png');assert int(body['length'])==len(PNG)
   return web.json_response({'ok':True,'file_id':'FOUTPUT','upload_url':f'http://127.0.0.1:{self.port}/upload'})
  if method=='files.completeUploadExternal':
   body=await request.json();self.completions.append(body)
   return web.Response(status=500) if self.fail_completion else web.json_response({'ok':True,'files':[{'id':'FOUTPUT'}]})
  return await super().api(request)
 async def download(self,request):
  self.downloads.append(request.match_info['id']);assert request.headers.get('Authorization')=='Bearer test-pony'
  key=request.match_info['id']
  if key=='FREDIRECT':raise web.HTTPFound('http://localhost:1/never-fetch')
  if key=='FBIG':return web.Response(body=b'a'*(2*1024*1024+1))
  return web.Response(body=PNG if key=='FIMAGE' else WAV if key=='FAUDIO' else b'notes for the agent',content_type='application/octet-stream')
 async def upload(self,request):
  assert not request.headers.get('Authorization'),'Slack token leaked to upload URL'
  self.uploads.append(await request.read());return web.Response(text='OK')
 async def with_file(self,key,mime,*,size=20,external=False,text='read file'):
  file={'id':key,'name':'notes.png' if mime.startswith('image') else 'notes.wav' if mime.startswith('audio') else 'notes.txt','mimetype':mime,'size':size,'is_external':external,'url_private':f'http://127.0.0.1:{self.port}/download/{key}'};self.files[key]=file
  m={'type':'message','subtype':'file_share','channel':'Droom','channel_type':'im','user':'UHUMAN','text':text,'ts':self.timestamp(),'files':[file]};self.messages.append(m);await self.deliver(m)

async def main():
 f=Fixture();app=web.Application(client_max_size=3*1024*1024);app.router.add_route('*','/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket);app.router.add_get('/download/{id}',f.download);app.router.add_post('/upload',f.upload);runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='slack-media-') as temp:
  home=Path(temp)/'connection';endpoint=Path(temp)/'endpoint';endpoint.mkdir();(endpoint/'caps.json').write_text(json.dumps({'image':True,'embeddedContext':True}))
  process=await asyncio.create_subprocess_exec(str(BINARY),'init','--home',str(home),stdout=asyncio.subprocess.DEVNULL);assert await process.wait()==0
  c=json.loads((home/'adapter.json').read_text());c['enabled']=c['setup_complete']=True;c['acp'].update(command='/usr/bin/python3',args=[str(ROOT/'tests/media_acp_fixture.py'),str(endpoint)],cwd=str(endpoint));c['slack'].update(team_id='Ttest',app_id='Apony',bot_user_id='Upony',bot_id='Bpony',loopback_test=True,api_base=f'http://127.0.0.1:{f.port}/api/');c['attention'].update(settle_ms=20,minimum_reply_interval_seconds=0);private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-pony-app','bot_token':'test-pony'})
  with (home/'test.log').open('wb') as log:
   service=await asyncio.create_subprocess_exec(str(BINARY),'serve','--home',str(home),stdout=log,stderr=log)
   try:
    await until(lambda:'pony' in f.sockets,'socket');checks=[]
    async def reply(count):await until(lambda:len(f.posts)>=count,'reply')
    await f.with_file('FIMAGE','image/png',size=len(PNG));await reply(1);blocks=json.loads((endpoint/'prompt.json').read_text());assert blocks[1]['type']=='image';assert base64.b64decode(blocks[1]['data'])==PNG;assert 'url_private' not in blocks[0]['text'];checks.append('image capability translation, no private URL in prompt')
    (endpoint/'caps.json').write_text('{}');await f.with_file('FIMAGE','image/png',size=len(PNG));await reply(2);assert f.downloads.count('FIMAGE')==1;assert 'not advertise support' in json.dumps(json.loads((endpoint/'prompt.json').read_text()));checks.append('unsupported image not downloaded or silently read')
    await f.with_file('FTEXT','text/plain');await reply(3);assert 'notes for the agent' in json.dumps(json.loads((endpoint/'prompt.json').read_text()));checks.append('text fallback without embedded resource support')
    (endpoint/'caps.json').write_text('{"embeddedContext":true}');await f.with_file('FTEXT','text/plain');await reply(4);assert json.loads((endpoint/'prompt.json').read_text())[1]['type']=='resource';checks.append('embedded resource negotiation')
    await f.with_file('FEXTERNAL','text/plain',external=True);await reply(5);assert 'FEXTERNAL' not in f.downloads;checks.append('external files never fetch with bot token')
    await f.with_file('FREDIRECT','text/plain');await reply(6);assert 'was not read' in json.loads((endpoint/'prompt.json').read_text())[1]['text'];checks.append('redirect outside exact host denied')
    await f.with_file('FBIG','text/plain');await reply(7);assert 'was not read' in json.loads((endpoint/'prompt.json').read_text())[1]['text'];checks.append('actual byte bound, independent of declared size')
    await f.send('Droom','native-output success');await reply(8);await until(lambda:len(f.completions)==1,'file complete');assert f.uploads==[PNG];assert f.completions[0]['channel_id']=='Droom';assert f.completions[0]['thread_ts'];checks.append('native ACP image uploaded without credentials on upload URL, shared in fixed thread')
    await f.send('Droom','remote-output no fetch');await reply(9);assert 'could not be delivered' in f.posts[-1]['markdown_text'];assert len(f.uploads)==1;checks.append('resource references never open server files or fetch arbitrary URLs')
    f.fail_completion=True;await f.send('Droom','native-output uncertain');await reply(10);await until(lambda:rows(home,"SELECT COUNT(*) FROM outbox WHERE status='uncertain'")[0][0]==1,'uncertain media');await asyncio.sleep(1.2);assert len(f.posts)==10;checks.append('partial media delivery remains uncertain despite text echo; no duplicate resend')
    (endpoint/'caps.json').write_text('{"audio":true}');await f.with_file('FAUDIO','audio/vnd.wave',size=len(WAV));await reply(11);audio=json.loads((endpoint/'prompt.json').read_text())[1];assert audio['type']=='audio' and audio['mimeType']=='audio/wav' and base64.b64decode(audio['data'])==WAV;checks.append('Slack audio/vnd.wave becomes native ACP audio/wav with byte preservation')
    (endpoint/'caps.json').write_text('{}');await f.with_file('FAUDIO','audio/vnd.wave',size=len(WAV));await reply(12);assert f.downloads.count('FAUDIO')==1 and 'not advertise support' in json.dumps(json.loads((endpoint/'prompt.json').read_text()));checks.append('WAV alias does not bypass agent audio capability requirements')
    print(json.dumps({'passed':True,'checks':checks,'live_slack_posts':0},indent=2))
   finally:
    service.send_signal(signal.SIGTERM);await asyncio.wait_for(service.wait(),15);await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
