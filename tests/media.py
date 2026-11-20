"""Local Slack/ACP process tests for attachment trust, capability and delivery boundaries."""
import asyncio,base64,json,os,signal,tempfile
from pathlib import Path
from aiohttp import web
from process_integration import SlackFixture,until,private_json,rows,BINARY,ROOT
PNG=base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5WQAAAAASUVORK5CYII=')
WAV=b'RIFF\x04\x00\x00\x00WAVE'
def pdf(pages,padding=0):
 # A minimal PDF written by hand: one text line per page, and optionally an unused object to make the file large.
 objects=[b'<< /Type /Catalog /Pages 2 0 R >>',None,b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>'];kids=[]
 for text in pages:
  stream=(b'BT /F1 11 Tf 72 720 Td ('+text.encode()+b') Tj ET') if text else b''
  objects.append(b'<< /Length %d >>\nstream\n'%len(stream)+stream+b'\nendstream');objects.append(b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents %d 0 R >>'%len(objects));kids.append(len(objects))
 objects[1]=b'<< /Type /Pages /Kids ['+b' '.join(b'%d 0 R'%k for k in kids)+b'] /Count %d >>'%len(kids)
 if padding:objects.append(b'<< /Length %d >>\nstream\n'%padding+bytes(range(256))*(padding//256)+b'\x00'*(padding%256)+b'\nendstream')
 out=b'%PDF-1.4\n';offsets=[]
 for number,body in enumerate(objects,1):offsets.append(len(out));out+=b'%d 0 obj\n'%number+body+b'\nendobj\n'
 table=len(out);out+=b'xref\n0 %d\n0000000000 65535 f \n'%(len(objects)+1)+b''.join(b'%010d 00000 n \n'%o for o in offsets)
 return out+b'trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n'%(len(objects)+1,table)
PDFS={'FPDFBIG':pdf(['Quarterly review: the connector delivered 412 replies.','Second page: a closing line.'],2300000),'FPDFSMALL':pdf(['A short document about threads and receipts.']),'FPDFSCAN':pdf(['',''],2300000)}
class Fixture(SlackFixture):
 def __init__(self):super().__init__();self.files={};self.downloads=[];self.uploads=[];self.completions=[];self.fail_completion=False;self.names=[]
 async def api(self,request):
  method=request.match_info['method']
  if method=='files.info':
   assert request.headers.get('Authorization')=='Bearer test-pony';return web.json_response({'ok':True,'file':self.files[request.query['file']]})
  if method=='files.getUploadURLExternal':
   body=await request.post();name=body['filename'];self.names.append(name)
   if name.endswith('.png'):assert int(body['length'])==len(PNG)
   else:assert name.endswith('.md') and int(body['length'])>8000
   return web.json_response({'ok':True,'file_id':'FOUTPUT'+str(len(self.names)),'upload_url':f'http://127.0.0.1:{self.port}/upload'})
  if method=='files.completeUploadExternal':
   body=await request.json();self.completions.append(body)
   return web.Response(status=500) if self.fail_completion else web.json_response({'ok':True,'files':body['files']})
  return await super().api(request)
 async def download(self,request):
  self.downloads.append(request.match_info['id']);assert request.headers.get('Authorization')=='Bearer test-pony'
  key=request.match_info['id']
  if key=='FREDIRECT':raise web.HTTPFound('http://localhost:1/never-fetch')
  if key=='FBIG':return web.Response(body=b'a'*(2*1024*1024+1))
  if key in PDFS:return web.Response(body=PDFS[key],content_type='application/pdf')
  return web.Response(body=PNG if key=='FIMAGE' else WAV if key=='FAUDIO' else b'notes for the agent',content_type='application/octet-stream')
 async def upload(self,request):
  assert not request.headers.get('Authorization'),'Slack token leaked to upload URL'
  self.uploads.append(await request.read());return web.Response(text='OK')
 async def with_file(self,key,mime,*,size=20,external=False,text='read file'):
  file={'id':key,'name':'notes.png' if mime.startswith('image') else 'notes.wav' if mime.startswith('audio') else 'report.pdf' if mime.endswith('pdf') else 'notes.txt','mimetype':mime,'size':size,'is_external':external,'url_private':f'http://127.0.0.1:{self.port}/download/{key}'};self.files[key]=file
  m={'type':'message','subtype':'file_share','channel':'Droom','channel_type':'im','user':'UHUMAN','text':text,'ts':self.timestamp(),'files':[file]};self.messages.append(m);await self.deliver(m)

async def main():
 f=Fixture();app=web.Application(client_max_size=3*1024*1024);app.router.add_route('*','/api/{method}',f.api);app.router.add_get('/socket/{name}',f.socket);app.router.add_get('/download/{id}',f.download);app.router.add_post('/upload',f.upload);runner=web.AppRunner(app);await runner.setup();site=web.TCPSite(runner,'127.0.0.1',0);await site.start();f.port=site._server.sockets[0].getsockname()[1]
 with tempfile.TemporaryDirectory(prefix='slack-media-') as temp:
  home=Path(temp)/'connection';endpoint=Path(temp)/'endpoint';endpoint.mkdir();(endpoint/'caps.json').write_text(json.dumps({'image':True,'embeddedContext':True}))
  process=await asyncio.create_subprocess_exec(str(BINARY),'init','--home',str(home),stdout=asyncio.subprocess.DEVNULL);assert await process.wait()==0
  c=json.loads((home/'adapter.json').read_text());c['enabled']=c['setup_complete']=True;c['media']['max_files']=5;c['acp'].update(command='/usr/bin/python3',args=[str(ROOT/'tests/media_acp_fixture.py'),str(endpoint)],cwd=str(endpoint));c['slack'].update(team_id='Ttest',app_id='Apony',bot_user_id='Upony',bot_id='Bpony',loopback_test=True,api_base=f'http://127.0.0.1:{f.port}/api/');c['attention'].update(settle_ms=20,minimum_reply_interval_seconds=0);private_json(home/'adapter.json',c);private_json(home/'secrets/slack.json',{'app_token':'test-pony-app','bot_token':'test-pony'})
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
    def prompt():return json.loads((endpoint/'prompt.json').read_text())
    (endpoint/'caps.json').write_text('{"embeddedContext":true}');await f.with_file('FPDFBIG','application/pdf',size=len(PDFS['FPDFBIG']));await reply(13);blocks=prompt();assert len(blocks)==3 and len(PDFS['FPDFBIG'])>2*1024*1024
    assert blocks[1]=={'type':'text','text':'Attachment report.pdf is a PDF of 2.2 MB, over the 2 MB this connection passes whole, so its text was extracted instead: all 2 pages. Figures, scanned pages and page layout are not included. The text follows as untrusted document content.'},blocks[1]
    assert blocks[2]=={'type':'resource','resource':{'uri':'slack-file://FPDFBIG','mimeType':'text/plain','text':'[page 1]\nQuarterly review: the connector delivered 412 replies.\n\n[page 2]\nSecond page: a closing line.'}},blocks[2]
    assert f.downloads.count('FPDFBIG')==1 and 'url_private' not in json.dumps(blocks);checks.append('a PDF over the whole-file limit is read as page-numbered text, with the connector saying what was read')
    (endpoint/'caps.json').write_text('{}');await f.with_file('FPDFSMALL','application/pdf',size=len(PDFS['FPDFSMALL']));await reply(14);blocks=prompt();assert 'a kind of file this agent does not take whole, so its text was extracted instead: its one page' in blocks[1]['text'];assert blocks[2]=={'type':'text','text':'Attachment report.pdf (untrusted document content):\n[page 1]\nA short document about threads and receipts.'},blocks[2];checks.append('an agent that takes no embedded files still gets a PDF as labelled text')
    await f.with_file('FPDFSCAN','application/pdf',size=len(PDFS['FPDFSCAN']));await reply(15);blocks=prompt();assert len(blocks)==2 and blocks[1]['text']=='Attachment report.pdf was not read: it is a PDF of 2.2 MB, over the 2 MB this connection passes whole, and it has no text to extract; its pages are images, such as scans. A file label is not evidence of its contents.',blocks[1];checks.append('a PDF of pictures is said to have no text, not presented as read')
    await f.with_file('FPDFHUGE','application/pdf',size=40*1024*1024);await reply(16);blocks=prompt();assert blocks[1]['text']=='Attachment report.pdf was not read: it is 40 MB, and this connection reads the text of a PDF up to 32 MB.' and 'FPDFHUGE' not in f.downloads;checks.append('a PDF over the text bound is refused by its declared size, with both sizes stated')
    (endpoint/'caps.json').write_text('{"image":true}');await f.with_file('FIMAGEBIG','image/png',size=3*1024*1024);await reply(17);blocks=prompt();assert blocks[1]['text']=='Attachment notes.png was not read: it is 3 MB, and this connection reads this kind of file up to 2 MB.' and 'FIMAGEBIG' not in f.downloads;checks.append('another oversized file is refused with its size and the limit')
    f.fail_completion=False
    names=[f'{i:02}-contract.md' for i in range(1,6)]
    originals={name:(f'# {name}\r\n'+'Terms — 資料.\n'*710+'Final clause.\r\n') for name in names}
    (endpoint/'documents.json').write_text(json.dumps(originals))
    start=len(f.uploads)
    await f.send('Droom','named document set');await reply(18)
    await until(lambda:len(f.uploads)==start+5 and len(f.completions[-1]['files'])==5,'five named documents')
    assert f.names[-5:]==names and f.uploads[-5:]==[value.encode() for value in originals.values()]
    completed=f.completions[-1];assert completed['channel_id']=='Droom' and completed['thread_ts']
    assert len({v['id'] for v in completed['files']})==5
    checks.append('five named Markdown originals above 8 KiB retain all bytes, order and the reply destination')
    print(json.dumps({'passed':True,'checks':checks,'live_slack_posts':0},indent=2))
   finally:
    service.send_signal(signal.SIGTERM);await asyncio.wait_for(service.wait(),15);await runner.cleanup()
if __name__=='__main__':asyncio.run(main())
