import sys,json
send=lambda v:print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
def call(i,p):
 send({'id':i,'method':'elicitation/create','params':p});return json.loads(sys.stdin.readline())
authed=False
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r['id']
 if m=='initialize':
  assert p['clientCapabilities']['elicitation']=={'form':{},'url':{}}
  result={'protocolVersion':1,'authMethods':[{'id':'browser','name':'Browser login'}]}
 elif m=='session/new' and not authed:
  send({'id':i,'error':{'code':-32000}});continue
 elif m=='authenticate':
  assert p['methodId']=='browser'
  assert call('login',{'requestId':i,'mode':'url','url':'https://example.test/login?state=private-login-value','elicitationId':'auth','message':'Authorize fixture'})['result']=={'action':'accept'}
  authed=True;result={}
 elif m=='session/new': result={'sessionId':'s'}
 elif m=='session/prompt':
  schema={'type':'object','properties':{'count':{'type':'integer','minimum':1,'maximum':3}},'required':['count']}
  assert call('foreign',{'sessionId':'other','mode':'form','message':'WRONG ROOM','requestedSchema':schema})['error']['code']==-32602
  assert call('form',{'sessionId':'s','mode':'form','message':'How many?','requestedSchema':schema})['result']=={'action':'accept','content':{'count':2}}
  send({'method':'session/update','params':{'sessionId':'s','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'Private answers received'}}}});result={'stopReason':'end_turn'}
 else: result={}
 send({'id':i,'result':result})
