use acp_slack_adapter::{
    config::Config,
    driver,
    model::{Message, now_ms},
    store::Store,
};
use serde_json::json;
#[tokio::test]
async fn host_attention_holds_without_prompt_and_engages_without_llm_preflight() {
    for action in ["hold", "engage"] {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("adapter");
        std::fs::create_dir(&home).unwrap();
        let script = t.path().join("host.py");
        std::fs::write(&script,r#"import sys,json,pathlib
root=pathlib.Path(sys.argv[1]);action=sys.argv[2]
def out(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
 if m=='initialize':result={'protocolVersion':1,'agentCapabilities':{},'_meta':{'com.microsoft.amplifier.attention':{'version':1,'enabled':True,'method':'_amplifier/attention'}}}
 elif m=='session/new':result={'sessionId':'session'}
 elif m=='_amplifier/attention':
  assert p['context']['conversation_id']=='Cgroup';assert p['occurredAt']>0
  assert 'What are you all working on?'==p['context']['messages'][-1]['text']
  (root/'evaluations').write_text('1');result={'action':action,'id':'event','ticket':'ticket','reason':'fixture'}
 elif m=='session/prompt':
  assert action=='engage';assert p['_meta']['com.microsoft.amplifier.attention']=={'id':'event','ticket':'ticket'}
  text=''.join(b.get('text','') for b in p['prompt']);assert 'Decide quietly' not in text
  path=root/'prompts';path.write_text(str(int(path.read_text())+1) if path.exists() else '1')
  out({'method':'session/update','params':{'sessionId':'session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'{"decision":"speak","text":"Here is my progress.","reason":"Asked the room"}'}}}});result={'stopReason':'end_turn'}
 else:result={}
 if i is not None:out({'id':i,'result':result})
"#).unwrap();
        let mut c = Config::new("Explorer".into(), "owner".into());
        c.context.enabled = false;
        c.participation = "ambient".into();
        c.slack.bot_user_id = "Uagent".into();
        c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
        c.acp.args = vec![
            script.to_string_lossy().into(),
            t.path().to_string_lossy().into(),
            action.into(),
        ];
        c.acp.cwd = t.path().to_string_lossy().into();
        let m = Message {
            team: "T".into(),
            channel: "Cgroup".into(),
            thread: "".into(),
            ts: format!("{}.000", now_ms() / 1000),
            revision: "1".into(),
            sender: "Uhuman".into(),
            bot: false,
            text: "What are you all working on?".into(),
            deleted: false,
            files: vec![],
        };
        let mut store = Store::open(&home).unwrap();
        store.ingest("event", &m, false).unwrap();
        let mut input = driver::request(&c, &m, store.history(&m).unwrap());
        input["orientation"] = json!({"conversation_kind":"channel"});
        input["participants_from_slack"] = json!(
            (0..500)
                .map(|i| json!({"id":format!("U{i}"),"name":format!("Member {i}")}))
                .collect::<Vec<_>>()
        );
        assert!(
            acp_slack_adapter::attention::context(&c, &input).unwrap()["participants"]
                .as_array()
                .unwrap()
                .len()
                <= 64
        );
        let result = driver::decide(c, home, input).await.unwrap();
        assert_eq!(
            result.decision,
            if action == "hold" { "pass" } else { "speak" }
        );
        assert_eq!(t.path().join("prompts").exists(), action == "engage");
        if action == "engage" {
            assert_eq!(
                std::fs::read_to_string(t.path().join("prompts")).unwrap(),
                "1"
            );
        }
    }
}
