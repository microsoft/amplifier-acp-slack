use acp_slack_adapter::{
    config::{Attention, Config},
    diagnostics, driver,
    model::Message,
    store::Store,
};
use serde_json::json;

async fn run(
    case: &str,
) -> (
    tempfile::TempDir,
    anyhow::Result<acp_slack_adapter::model::Decision>,
) {
    let t = tempfile::tempdir().unwrap();
    let script = t.path().join("agent.py");
    std::fs::write(&script,r#"import sys,json
case=sys.argv[1]
def out(v):print(json.dumps({'jsonrpc':'2.0',**v}),flush=True)
for line in sys.stdin:
 r=json.loads(line);m=r.get('method');i=r.get('id');p=r.get('params',{})
 if m=='initialize':
  result={'protocolVersion':1,'agentCapabilities':{},'_meta':{'com.microsoft.amplifier.attention':{'version':1,'enabled':True,'method':'_amplifier/attention','noForwardRejectionVersion':1}}}
  if case=='unnegotiated':del result['_meta']['com.microsoft.amplifier.attention']['noForwardRejectionVersion']
 elif m=='session/new':result={'sessionId':'original-session'}
 elif m=='_amplifier/attention':result={'action':'engage','id':'offer','ticket':'private-ticket'}
 elif m=='session/prompt':
  if case in ['attested','mismatch','unnegotiated','generic']:
   evidence={'version':1,'id':'other-offer' if case=='mismatch' else 'offer','sessionId':p['sessionId'],'outcome':'not_forwarded'}
   err={'code':-32002,'message':'PRIVATE provider or host text'}
   if case!='generic':err['data']={'com.microsoft.amplifier.attention':evidence}
   out({'id':i,'error':err});continue
  if case in ['limit','unsettled']:
   for status in ['in_progress']+(['completed'] if case=='limit' else []):
    out({'method':'session/update','params':{'sessionId':p['sessionId'],'update':{'sessionUpdate':'tool_call','toolCallId':'effect','kind':'execute','title':'Inspect existing result','status':status}}})
  result={'stopReason':{'limit':'max_turn_requests','unsettled':'max_turn_requests','cancel':'cancelled','refuse':'refusal'}.get(case,'end_turn')}
 else:result={}
 if i is not None:out({'id':i,'result':result})
"#).unwrap();
    let mut c = Config::new("Fixture".into(), "owner".into());
    c.context.enabled = false;
    c.enabled = true;
    c.participation = "ambient".into();
    c.acp.command = (if cfg!(windows) { "python" } else { "python3" }).into();
    c.acp.args = vec![script.to_string_lossy().into(), case.into()];
    c.acp.cwd = t.path().to_string_lossy().into();
    let m = Message {
        team: "Tfixture".into(),
        channel: "Croom".into(),
        thread: "".into(),
        ts: "1800000000.000001".into(),
        revision: "1".into(),
        sender: "Uhuman".into(),
        bot: false,
        text: "Complete the requested check".into(),
        deleted: false,
        files: vec![],
    };
    let mut s = Store::open(t.path()).unwrap();
    s.ingest("source", &m, true).unwrap();
    let j = s
        .claim(&Attention {
            settle_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    let mut input = driver::request(&c, &m, s.history(&m).unwrap());
    input["job_id"] = json!(j.id);
    let result = driver::decide(c, t.path().to_owned(), input).await;
    (t, result)
}

#[tokio::test]
async fn confirmed_turn_limit_continues_existing_work_without_publishing_partial_output() {
    let (t, r) = run("limit").await;
    let d = r.unwrap();
    assert_eq!(d.decision, "pass");
    assert!(d.text.is_empty());
    let s = Store::open(t.path()).unwrap();
    assert_eq!(
        s.db.query_row(
            "SELECT count(*) FROM followups WHERE status='proposed'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}
#[tokio::test]
async fn unresolved_tools_at_limit_are_not_replayed_or_rescheduled() {
    let (t, r) = run("unsettled").await;
    assert_eq!(
        diagnostics::details(&r.unwrap_err()).1,
        "turn_limit_unsettled"
    );
    let s = Store::open(t.path()).unwrap();
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM followups", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn only_negotiated_exact_no_forward_attestation_changes_failure_category() {
    for case in ["attested", "mismatch", "unnegotiated", "generic"] {
        let (t, r) = run(case).await;
        let e = r.unwrap_err();
        let (_, category, delivery, summary) = diagnostics::details(&e);
        assert_eq!(
            category,
            if case == "attested" {
                "not_forwarded"
            } else {
                "backend_or_transport"
            },
            "{case}"
        );
        assert_eq!(
            delivery,
            if case == "attested" {
                "not_started"
            } else {
                "unknown"
            }
        );
        assert!(!summary.contains("PRIVATE"));
        let s = Store::open(t.path()).unwrap();
        assert_eq!(
            s.db.query_row("SELECT count(*) FROM followups", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
#[tokio::test]
async fn cancellation_and_refusal_never_schedule_another_turn() {
    for case in ["cancel", "refuse"] {
        let (t, r) = run(case).await;
        if case == "cancel" {
            assert_eq!(r.unwrap().decision, "pass");
        } else {
            assert_eq!(diagnostics::details(&r.unwrap_err()).1, "refusal");
        }
        let s = Store::open(t.path()).unwrap();
        assert_eq!(
            s.db.query_row("SELECT count(*) FROM followups", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
