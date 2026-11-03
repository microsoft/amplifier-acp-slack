use acp_slack_adapter::{
    client_tools, config::Config, driver, model::Message, permissions, store::Store,
};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    c: Config,
    input: Value,
}
impl Fixture {
    fn new(scenario: &str, bot: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("adapter");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let peer = temp.path().join("agent.py");
        std::fs::write(&peer, r#"import json,pathlib,sys,time
root=pathlib.Path(sys.argv[1]);scenario=sys.argv[2]
def send(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');p=r.get('params',{});i=r.get('id')
 if m=='initialize':
  assert p['clientCapabilities']['elicitation']=={'form':{},'url':{}}
  send({'id':i,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True}}})
 elif m in ('session/new','session/load'):send({'id':i,'result':{'sessionId':'test-session'}})
 elif m=='session/prompt':
  options=[{'optionId':'once','name':'Allow once','kind':'allow_once'}, {'optionId':'deny','name':'Decline','kind':'reject_once'}]
  if scenario=='deny':options=options[1:]
  if scenario=='persistent':options=[{'optionId':'always','name':'Always','kind':'allow_always'}]
  if scenario=='unknown':options=[{'optionId':'new','name':'Future','kind':'future_kind'}]
  if scenario=='duplicate':options[1]['optionId']='once'
  (root/'ready').write_text('ready')
  if scenario=='wait':
   deadline=time.monotonic()+10
   while not (root/'proceed').exists():
    assert time.monotonic()<deadline
    time.sleep(.01)
  send({'id':'permission','method':'session/request_permission','params':{'sessionId':'foreign' if scenario=='foreign' else p['sessionId'],'toolCall':{'toolCallId':'tool','title':'Synthetic action','kind':'execute'},'options':options}})
  result=json.loads(sys.stdin.readline());assert result['id']=='permission'
  (root/'response.json').write_text(json.dumps(result))
  send({'method':'session/update','params':{'sessionId':p['sessionId'],'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':json.dumps(result)}}}})
  send({'id':i,'result':{'stopReason':'end_turn'}})
"#).unwrap();
        let mut c = Config::new("Policy fixture".into(), "owner".into());
        c.enabled = true;
        c.setup_complete = true;
        c.acp.command = "python3".into();
        c.acp.args = vec![
            peer.to_string_lossy().into_owned(),
            home.to_string_lossy().into_owned(),
            scenario.into(),
        ];
        c.acp.cwd = workspace.to_string_lossy().into_owned();
        c.acp.client.approvals_required = false;
        c.slack.team_id = "TTEST".into();
        c.slack.app_id = "ATEST".into();
        c.slack.bot_user_id = "UAGENT".into();
        c.attention.settle_ms = 0;
        c.save(&home).unwrap();
        let mut store = Store::open(&home).unwrap();
        let source = Message {
            team: "TTEST".into(),
            channel: "DTEST".into(),
            thread: "".into(),
            ts: "123.456".into(),
            revision: "123.456".into(),
            sender: if bot { "UPEER" } else { "UHUMAN" }.into(),
            bot,
            text: "Please perform this synthetic action".into(),
            deleted: false,
            files: vec![],
        };
        store.ingest("event", &source, true).unwrap();
        let job = store.claim(&c.attention).unwrap().unwrap();
        let mut input = driver::request(&c, &source, vec![source.clone()]);
        input["job_id"] = json!(job.id);
        input["orientation"] = json!({"conversation_kind":"im"});
        Self {
            _temp: temp,
            home,
            c,
            input,
        }
    }
    async fn run(&self) -> Value {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            driver::decide(self.c.clone(), self.home.clone(), self.input.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        serde_json::from_str(&result.text).unwrap()
    }
    fn store(&self) -> Store {
        Store::open(&self.home).unwrap()
    }
}

#[tokio::test]
async fn automatic_policy_uses_single_use_choice_without_slack_credentials_or_human_wait() {
    for bot in [false, true] {
        let f = Fixture::new("allow", bot);
        let result = f.run().await;
        assert_eq!(
            result["result"]["outcome"],
            json!({"outcome":"selected","optionId":"once"})
        );
        let items = permissions::list(&f.store()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["status"], "approved");
        assert_eq!(items[0]["actor"], "policy:automatic");
        assert_eq!(items[0]["slack_ts"], Value::Null);
        assert_eq!(
            permissions::waiting_ms(&f.store(), f.input["job_id"].as_str().unwrap(), i64::MAX)
                .unwrap(),
            0
        );
        let count: i64 = f
            .store()
            .db
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='permission_auto_approved'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert!(!f.home.join("secrets/slack.json").exists());
    }
}

#[tokio::test]
async fn unknown_denial_only_persistent_ambiguous_and_foreign_requests_never_grant() {
    for scenario in ["deny", "persistent", "unknown", "duplicate", "foreign"] {
        let f = Fixture::new(scenario, false);
        let result = f.run().await;
        if ["duplicate", "foreign"].contains(&scenario) {
            assert!(result["error"].is_object(), "{scenario}: {result}");
        } else {
            assert_eq!(
                result["result"]["outcome"]["outcome"], "cancelled",
                "{scenario}: {result}"
            );
        }
        assert!(permissions::list(&f.store()).unwrap().is_empty());
    }
}

#[tokio::test]
async fn automatic_policy_rechecks_persisted_source_and_live_configuration_before_grant() {
    for change in [
        "edited", "deleted", "finished", "revision", "policy", "paused", "sender", "bot",
    ] {
        let f = Fixture::new("wait", false);
        let (c, home, input) = (f.c.clone(), f.home.clone(), f.input.clone());
        let task = tokio::spawn(driver::decide(c, home, input));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !f.home.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        match change {
            "edited" => {
                f.store()
                    .db
                    .execute("UPDATE messages SET revision='changed'", [])
                    .unwrap();
            }
            "deleted" => {
                f.store()
                    .db
                    .execute(
                        "UPDATE messages SET body=json_set(body,'$.deleted',json('true'))",
                        [],
                    )
                    .unwrap();
            }
            "finished" => {
                f.store()
                    .db
                    .execute("UPDATE jobs SET status='interrupted'", [])
                    .unwrap();
            }
            "sender" => {
                f.store()
                    .db
                    .execute(
                        "UPDATE messages SET body=json_set(body,'$.sender','UOTHER')",
                        [],
                    )
                    .unwrap();
            }
            "bot" => {
                f.store()
                    .db
                    .execute(
                        "UPDATE messages SET body=json_set(body,'$.bot',json('true'))",
                        [],
                    )
                    .unwrap();
            }
            _ => {
                let mut c = f.c.clone();
                match change {
                    "revision" => c.revision += 1,
                    "policy" => c.acp.client.approvals_required = true,
                    "paused" => c.enabled = false,
                    _ => unreachable!(),
                }
                c.save(&f.home).unwrap();
            }
        }
        std::fs::write(f.home.join("proceed"), "go").unwrap();
        task.await.unwrap().unwrap();
        let reply: Value =
            serde_json::from_slice(&std::fs::read(f.home.join("response.json")).unwrap()).unwrap();
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "cancelled",
            "{change}: {reply}"
        );
        assert!(
            permissions::list(&f.store()).unwrap().is_empty(),
            "{change}"
        );
    }
}

#[tokio::test]
async fn human_policy_does_not_let_a_bot_pose_as_a_requester() {
    let mut f = Fixture::new("allow", true);
    f.c.acp.client.approvals_required = true;
    f.c.save(&f.home).unwrap();
    assert_eq!(f.run().await["result"]["outcome"]["outcome"], "cancelled");
    f.input["trigger"]["bot"] = json!(false);
    assert_eq!(f.run().await["result"]["outcome"]["outcome"], "cancelled");
    assert!(permissions::list(&f.store()).unwrap().is_empty());
}

#[test]
fn legacy_configuration_requires_approval_but_questions_do_not_require_an_allowlist() {
    let c = Config::new("Legacy".into(), "owner".into());
    let mut value = serde_json::to_value(&c).unwrap();
    value["acp"]["client"]
        .as_object_mut()
        .unwrap()
        .remove("approvals_required");
    let restored: Config = serde_json::from_value(value).unwrap();
    assert!(restored.acp.client.approvals_required);
    assert!(restored.acp.client.approval_users.is_empty());
    assert_eq!(
        client_tools::advertised(&restored)["elicitation"],
        json!({"form":{},"url":{}})
    );
    let schema = serde_json::to_value(schemars::schema_for!(Config))
        .unwrap()
        .to_string();
    assert!(schema.contains("approvals_required"));
}
