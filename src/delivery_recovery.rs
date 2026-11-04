//! Resolve lost Slack receipts by reading the exact destination and client ID.
//! Absence in a bounded page is not evidence that a send did not happen.
use crate::{model::now_ms, slack::Slack, store::Store};
use anyhow::Result;
use rusqlite::params;
use std::path::Path;

pub async fn reconcile(home: &Path, slack: &Slack) -> Result<()> {
    let s = Store::open(home)?;
    s.db.execute_batch("CREATE TABLE IF NOT EXISTS delivery_recovery(id TEXT PRIMARY KEY,checked INTEGER NOT NULL);")?;
    let rows=s.db.prepare("SELECT o.id,o.channel,o.thread,o.created FROM outbox o LEFT JOIN delivery_recovery r ON r.id=o.id WHERE o.status='uncertain' AND o.media='[]' AND COALESCE(r.checked,0)<?1 ORDER BY o.created LIMIT 4")?.query_map([now_ms()-30_000],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, channel, thread, created) in rows {
        s.db.execute("INSERT INTO delivery_recovery VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET checked=excluded.checked",params![id,now_ms()])?;
        let parts=s.db.prepare("SELECT part,client_id FROM outbox_parts WHERE outbox_id=?1 AND status IN ('sending','uncertain')")?.query_map([&id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let any_parts: bool = s.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM outbox_parts WHERE outbox_id=?1)",
            [&id],
            |r| r.get(0),
        )?;
        if !any_parts {
            if let Ok(Some(ts)) = slack.find_message(&channel, &thread, &id, created).await {
                s.echo(&id, &ts)?;
                s.audit(
                    "delivery_reconciled",
                    serde_json::json!({"id":id,"evidence":"Slack message with original client ID"}),
                )?;
            }
        } else {
            for (part, client_id) in parts {
                if let Ok(Some(ts)) = slack
                    .find_message(&channel, &thread, &client_id, created)
                    .await
                {
                    s.db.execute("UPDATE outbox_parts SET status='sent',slack_ts=?3 WHERE outbox_id=?1 AND part=?2",params![id,part,ts])?;
                }
            }
            let unknown:bool=s.db.query_row("SELECT EXISTS(SELECT 1 FROM outbox_parts WHERE outbox_id=?1 AND status NOT IN ('ready','sent'))",[&id],|r|r.get(0))?;
            let remaining: bool = s.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox_parts WHERE outbox_id=?1 AND status='ready')",
                [&id],
                |r| r.get(0),
            )?;
            if !unknown {
                if remaining && s.outgoing_source_current(&id)? {
                    s.db.execute("UPDATE outbox SET status='ready',note='Known delivered parts recovered; only unsent parts remain' WHERE id=?1 AND status='uncertain'",[&id])?;
                } else if !remaining {
                    s.db.execute("UPDATE outbox SET status='sent',slack_ts=(SELECT slack_ts FROM outbox_parts WHERE outbox_id=?1 ORDER BY part LIMIT 1),note='All parts confirmed from Slack history' WHERE id=?1 AND status='uncertain'",[&id])?;
                }
            }
        }
    }
    Ok(())
}
