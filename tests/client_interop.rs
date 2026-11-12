use acp_slack_adapter::{acp_auth, config::Config, driver};
use serde_json::json;
#[tokio::test]
async fn bare_agent_negotiates_optional_reads_during_new_and_load_without_management_extensions() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("adapter");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("note.txt"), "before prompt").unwrap();
    let script = temp.path().join("bare.py");
    std::fs::write(&script, r#"import sys,json,pathlib

def send(v): print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
def call(i,m,p):
 send({'id':i,'method':m,'params':p}); r=json.loads(sys.stdin.readline()); assert r['id']==i; return r
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
 if m=='initialize':
  caps=p['clientCapabilities'];send({'id':i,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True}}})
 elif m in ('session/new','session/load'):
  sid=p.get('sessionId','bare-session');path=str(pathlib.Path(p['cwd'])/'note.txt')
  assert '_meta' not in p
  if caps.get('fs',{}).get('readTextFile'):
   assert call('read','fs/read_text_file',{'sessionId':sid,'path':path})['result']['content']=='before prompt'
   assert 'error' in call('foreign','fs/read_text_file',{'sessionId':'foreign','path':path})
   assert 'error' in call('outside','fs/read_text_file',{'sessionId':sid,'path':'/etc/passwd'})
  send({'id':i,'result':{'sessionId':sid}})
 elif m=='session/prompt':
  send({'method':'session/update','params':{'sessionId':p['sessionId'],'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'setup callbacks worked'}}}})
  send({'id':i,'result':{'stopReason':'end_turn'}})
"#).unwrap();
    let mut c = Config::new("Bare ACP".into(), "owner".into());
    c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
    c.acp.args = vec![script.to_string_lossy().into_owned()];
    c.acp.cwd = std::fs::canonicalize(&workspace)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    c.acp.client.read_text_file = cfg!(unix);
    if cfg!(unix) {
        c.acp.client.workspace_roots = vec![c.acp.cwd.clone()];
    }
    c.context.enabled = false;
    let input = json!({"conversation":{"context":"slack:T:C:","channel":"C","thread":""},"trigger":{"text":"hello","sender":"user","bot":false},"orientation":{"conversation_kind":"im"}});
    for _ in 0..2 {
        let result = driver::decide(c.clone(), home.clone(), input.clone())
            .await
            .unwrap();
        assert_eq!(result.text, "setup callbacks worked");
    }
    // Operator probes deliberately offer no local client tools.
    assert_eq!(driver::check(&c, &home).await.unwrap()["ok"], true);
}
#[tokio::test]
async fn auth_required_is_actionable_and_does_not_echo_endpoint_error_data() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("auth.py");
    std::fs::write(&script,r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 result={'protocolVersion':1,'authMethods':[{'id':'login','name':'Browser login'}]} if r['method']=='initialize' else None
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],**({'result':result} if result else {'error':{'code':-32000,'message':'DO NOT ECHO PROVIDER SECRET'}})}),flush=True)
"#).unwrap();
    let mut c = Config::new("Auth".into(), "owner".into());
    c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
    c.acp.args = vec![script.to_string_lossy().into_owned()];
    c.acp.cwd = temp.path().to_string_lossy().into_owned();
    let error = driver::check(&c, temp.path()).await.unwrap_err();
    assert!(acp_auth::authentication_required(&error));
    assert!(error.to_string().contains("same service account"));
    assert!(!error.to_string().contains("PROVIDER SECRET"));
}
