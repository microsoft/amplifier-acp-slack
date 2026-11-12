use crate::{
    config::{self, Config},
    driver, slack, store,
};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{sync::Notify, task::JoinSet};

#[derive(Debug)]
struct UpdateAdmissionUnavailable;
impl std::fmt::Display for UpdateAdmissionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connector update coordination is unavailable; check installation storage and permissions")
    }
}
impl std::error::Error for UpdateAdmissionUnavailable {}

fn failure_details(error: &anyhow::Error) -> (&'static str, &'static str) {
    if error.is::<UpdateAdmissionUnavailable>() {
        (
            "update_coordination_unavailable",
            "Connector update coordination is unavailable; check installation storage and permissions",
        )
    } else {
        (
            "connection_failed",
            "Connection could not start or stopped unexpectedly; check connector activity",
        )
    }
}

async fn run(home: PathBuf, mut changes: tokio::sync::watch::Receiver<u64>) -> Result<()> {
    let home = std::fs::canonicalize(home)?;
    config::private_dir(&home)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join("process.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("This installation already has a running process"))?;
    let c = Config::load(&home)?;
    if !c.enabled {
        return Ok(());
    }
    let client = slack::Slack::new(&c, c.secrets(&home)?)?;
    client.verify(&c).await?;
    let store = Arc::new(Mutex::new(store::Store::open(&home)?));
    store.lock().unwrap().bind_identity(&c)?;
    store.lock().unwrap().recover()?;
    store.lock().unwrap().apply_scope(&c)?;
    runtime_status(
        &home,
        &c,
        "running",
        "Slack credentials verified; connecting to Slack",
    )?;
    let _portrait = AbortTask(tokio::spawn(crate::identity::run(home.clone(), c.clone())));
    let mut work_status = AbortTask(tokio::spawn(crate::work_status::run(
        home.clone(),
        c.clone(),
        client.clone(),
    )));
    let notify = Arc::new(Notify::new());
    let mut socket = AbortTask(tokio::spawn(client.clone().listen(
        c.clone(),
        home.clone(),
        store.clone(),
        notify.clone(),
    )));
    let sender_store = store.clone();
    let sender = client.clone();
    let sender_home = home.clone();
    let sender_revision = c.revision;
    let mut publisher = AbortTask(tokio::spawn(async move {
        loop {
            let current = Config::load(&sender_home)?;
            crate::permissions::refresh_messages(&sender_home, &current, &sender).await?;
            if current.revision != sender_revision || !current.enabled {
                break;
            }
            let _ = crate::delivery_recovery::reconcile(&sender_home, &sender).await;
            let _ = crate::public_speech::reconcile(&sender_home, &sender).await;
            if crate::public_speech::pump(&sender_home, &sender, &sender_store, &current).await? {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                continue;
            }
            let next = sender_store.lock().unwrap().claim_outgoing(&current)?;
            if let Some(o) = next {
                match sender.publish_durable(&o, &sender_store).await {
                    Ok(ts) => sender_store.lock().unwrap().delivery(
                        &o.id,
                        "sent",
                        Some(&ts),
                        "Confirmed by Slack HTTP response",
                        0,
                    )?,
                    Err(slack::ApiError::RateLimited(delay)) => {
                        sender_store.lock().unwrap().delivery(
                            &o.id,
                            "ready",
                            None,
                            "Slack rate limit",
                            (delay.max(1) * 1000) as i64,
                        )?
                    }
                    Err(slack::ApiError::Rejected(reason)) => sender_store
                        .lock()
                        .unwrap()
                        .delivery(&o.id, "failed", None, &reason, 0)?,
                    Err(slack::ApiError::Uncertain) => {
                        // Do not overwrite an echo that arrived while HTTP was pending.
                        let s = sender_store.lock().unwrap();
                        s.db.execute("UPDATE outbox SET status='uncertain',note='Delivery uncertain; do not resend without checking Slack' WHERE id=?1 AND status='sending'",[&o.id])?;
                        s.audit("delivery_uncertain", json!({"id":o.id}))?;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    }));
    let mut workers = JoinSet::new();
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(500));
    let result = loop {
        tokio::select! {
            _=changes.changed()=>break Ok(()),
            r=&mut socket.0=>break Err(anyhow::anyhow!("Slack listener stopped: {}",r.is_err())),
            r=&mut publisher.0=>break Err(anyhow::anyhow!("Slack publisher stopped: {}",r.is_err())),
            Some(r)=workers.join_next(),if !workers.is_empty()=>{
                if r.is_err(){store.lock().unwrap().audit("worker_panic",json!({}))?;}
            },
            _=timer.tick()=>{},
            _=notify.notified()=>{},
        }
        crate::followup::pump(&home, &c)?;
        while workers.len() < c.attention.parallel_contexts {
            let Some(admission) =
                crate::updates::admission(&home).context(UpdateAdmissionUnavailable)?
            else {
                break;
            };
            let next = store.lock().unwrap().claim(&c.attention)?;
            drop(admission);
            let Some(job) = next else { break };
            if !c.allows(&job.trigger.channel, "")
                || !c.should_wake(&job.trigger.channel, &job.trigger.text)
            {
                store.lock().unwrap().db.execute("UPDATE jobs SET status='superseded',note='Conversation no longer in the configured scope' WHERE id=?1",[&job.id])?;
                continue;
            }
            let c = c.clone();
            let home = home.clone();
            let store = store.clone();
            let client = client.clone();
            workers.spawn(async move {
                let participants=client.participants(&job.trigger.channel).await;
                let history_result=client.recent(&job.trigger).await;
                let prepared=(||->Result<_>{
                    let mut s=store.lock().unwrap();
                    match history_result {
                        Ok(messages)=>for m in messages {
                            if s.db.query_row("SELECT EXISTS(SELECT 1 FROM permissions WHERE channel=?1 AND slack_ts=?2)",rusqlite::params![m.channel,m.ts],|r|r.get::<_,bool>(0))? {continue;}
                            let id=format!("history:{}:{}:{}",m.channel,m.ts,m.revision);
                            s.ingest(&id,&m,false)?;
                        },
                        Err(_)=>s.audit("history_incomplete",json!({"context":job.context,"note":"Using locally observed messages; Slack history unavailable"}))?,
                    }
                    let job=s.prepare(job.clone())?;
                    let history=s.history(&job.trigger)?;
                    let delivery=s.response_delivery(&job.context,&job.id)?;
                    Ok((job,history,delivery))
                })();
                let (job,result)=match prepared {
                    Ok((job,history,delivery))=>{
                        let mut input=driver::request(&c,&job.trigger,history);
                        input["job_id"]=json!(job.id);
                        input["response_delivery"]=delivery;
                        input["side_conversation"]=crate::huddles::projection(&store.lock().unwrap(),&c,&input)?;
                        if let Some(handoff)=crate::handoff::attach(&store.lock().unwrap(),&job,&driver::endpoint_key(&c))? { input["work_handoff"]=handoff; }
                        if let Some(followup)=crate::followup::input(&home,&job.id)? { input["accepted_work_continuation"]=followup; }
                        if job.reconsidered { input["continuation_note"]=json!("An earlier response was withheld and was NOT posted to Slack. Other participants did not see that draft. Use work_handoff to re-evaluate unfinished work against the newest instruction. A status question alone does not cancel the original objective. Continue still-authorized steps, respecting cancellations and scope changes. Review the actual conversation and delivery receipts before deciding what to say; do not treat a private proposal as shared or agreed. Work may already have been performed: check its result instead of repeating actions. If that evidence is unavailable, say so."); }
                        input["participants_from_slack"]=serde_json::to_value(participants)?;
                        if c.context.enabled {
                            match crate::context::orientation_for(&c,&home,&input).await {
                                Ok(orientation)=>input["orientation"]=orientation,
                                Err(_)=> {
                                    input["context_notice"]=json!("Live conversation orientation could not be fetched. Use the supplied bounded projection and available context tools; do not claim the projection is a complete conversation archive.");
                                    store.lock().unwrap().audit("context_unavailable",json!({"context":job.context,"reason":"Live conversation orientation is temporarily unavailable"}))?;
                                }
                            }
                        }
                        let result=driver::decide(c.clone(),home.clone(),input).await;
                        (job,result)
                    },
                    Err(e)=>(job,Err(e))
                };
                // Errors are categorized without logging prompts, secrets, or
                // untrusted harness stderr to the shared service journal.
                if result.is_err(){eprintln!("ACP turn failed in a conversation; see adapter activity");}
                let result=store.lock().unwrap().finish(&job,result,&c.attention);
                crate::work_status::changed(&home);
                result
            });
        }
    };
    socket.0.abort();
    publisher.0.abort();
    workers.abort_all();
    while workers.join_next().await.is_some() {}
    work_status.0.abort();
    let _ = (&mut work_status.0).await;
    crate::work_status::shutdown(&home).await;
    crate::permissions::cancel_all(&store.lock().unwrap(), "Connector stopped")?;
    crate::permissions::refresh_messages(&home, &c, &client).await?;
    store.lock().unwrap().audit("process_stopped", json!({}))?;
    drop(lock);
    result
}

struct AbortTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn runtime_status(home: &std::path::Path, c: &Config, state: &str, reason: &str) -> Result<()> {
    config::private_write(
        &home.join("runtime.json"),
        &serde_json::to_vec(
            &json!({"state":state,"config_revision":c.revision,"updated_at":crate::model::now_ms(),"reason":reason,"connections":[]}),
        )?,
    )
}
pub async fn supervise(
    home: PathBuf,
    mut changes: tokio::sync::watch::Receiver<u64>,
) -> Result<()> {
    loop {
        changes.borrow_and_update();
        let c = Config::load(&home)?;
        if !c.enabled {
            crate::work_status::shutdown(&home).await;
            runtime_status(&home, &c, "stopped", "Connection paused")?;
            if changes.changed().await.is_err() {
                return Ok(());
            }
            continue;
        }
        if c.validate_ready().is_err() || c.secrets(&home).is_err() {
            runtime_status(
                &home,
                &c,
                "failed",
                "Finish setup and verify credentials before starting",
            )?;
            if changes.changed().await.is_err() {
                return Ok(());
            }
            continue;
        }
        runtime_status(
            &home,
            &c,
            "connecting",
            "Connecting the Slack app and ACP endpoint",
        )?;
        let observed = c.revision;
        let result = run(home.clone(), changes.clone()).await;
        if let Err(error) = result {
            // Never expose raw paths, credentials, prompts or endpoint stderr.
            let (category, reason) = failure_details(&error);
            runtime_status(&home, &c, "failed", reason)?;
            let s = store::Store::open(&home)?;
            s.audit(
                "connection_failed",
                json!({"category":category,"reason":reason}),
            )?;
        }
        if *changes.borrow() != observed {
            continue;
        }
        tokio::select! { r=changes.changed()=>if r.is_err(){return Ok(());}, _=tokio::time::sleep(std::time::Duration::from_secs(15))=>{} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_diagnostics_identify_coordination_without_leaking_causes() {
        let error =
            anyhow::anyhow!("private path or credential").context(UpdateAdmissionUnavailable);
        let (category, reason) = failure_details(&error);
        assert_eq!(category, "update_coordination_unavailable");
        assert!(!reason.contains("credential"));
        assert!(!reason.contains("private path"));
        assert_eq!(
            failure_details(&anyhow::anyhow!("secret")).0,
            "connection_failed"
        );
    }
}
