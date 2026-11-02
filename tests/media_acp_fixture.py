"""Media-capable ordinary ACP fixture, independent of connector modules."""
import json,sys,uuid,pathlib,base64
home=pathlib.Path(sys.argv[1]);session=''
PNG='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5WQAAAAASUVORK5CYII='
for line in sys.stdin:
 request=json.loads(line);method=request.get('method');params=request.get('params',{});result={}
 if method=='initialize':result={'protocolVersion':1,'agentCapabilities':{'loadSession':True,'promptCapabilities':json.loads((home/'caps.json').read_text())}}
 elif method in ['session/new','session/load']:session=params.get('sessionId',str(uuid.uuid4()));result={'sessionId':session}
 elif method=='session/prompt':
  blocks=params['prompt'];(home/'prompt.json').write_text(json.dumps(blocks));projection=json.loads(blocks[0]['text'].split('Latest conversation projection:\n',1)[1]);text=projection['trigger']['text'];content=[{'type':'text','text':'Media handled: '+text}]
  if text.startswith('native-output'):content.append({'type':'image','mimeType':'image/png','data':PNG})
  if text.startswith('remote-output'):content.append({'type':'resource_link','uri':'file:///etc/passwd','name':'private'})
  for block in content:print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session,'update':{'sessionUpdate':'agent_message_chunk','content':block}}}),flush=True)
  result={'stopReason':'end_turn'}
 else:continue
 print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
