//! Named, job-scoped reply attachments for agents that return text through ACP.
use crate::{
    config::Config,
    media::{OutputFile, filename, is_text},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const TOOL: &str = "slack_attach_file";

pub fn initialize(store: &Store) -> Result<()> {
    store.db.execute_batch("CREATE TABLE IF NOT EXISTS reply_files(job_id TEXT NOT NULL,name TEXT NOT NULL,mime_type TEXT NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(job_id,name));
        CREATE TRIGGER IF NOT EXISTS retire_reply_files AFTER UPDATE OF status ON jobs WHEN NEW.status!='running' BEGIN DELETE FROM reply_files WHERE job_id=NEW.id; END;")?;
    Ok(())
}

pub fn tool() -> Value {
    json!({"name":TOOL,"description":"Attach supplied file contents to your next reply here. Use a workspace attachment tool when available for an existing file. Otherwise supply the exact filename and either UTF-8 text or base64 file bytes. Pass sha256 from the original to check the copy. The file is staged until your normal reply is sent; use a speak reply in an ambient channel. No host paths or URLs are opened.","inputSchema":{"type":"object","properties":{"filename":{"type":"string","maxLength":120},"mime_type":{"type":"string"},"text":{"type":"string"},"base64":{"type":"string"},"sha256":{"type":"string"}},"required":["filename","mime_type"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    filename: String,
    mime_type: String,
    text: Option<String>,
    base64: Option<String>,
    sha256: Option<String>,
}

pub fn stage(
    home: &Path,
    config: &Config,
    job: &str,
    conversation: (&str, &str),
    args: &Value,
) -> Result<Value> {
    ensure!(
        config.media.outgoing,
        "Outgoing attachments are disabled for this connection"
    );
    let doc: Document = serde_json::from_value(args.clone())
        .context("Supply a filename, MIME type and either text or base64 contents")?;
    ensure!(
        !doc.filename.is_empty()
            && filename(&doc.filename) == doc.filename
            && !doc.filename.starts_with('.')
            && !doc
                .filename
                .chars()
                .any(|c| matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')),
        "Use a plain filename without directory or control characters"
    );
    let bytes = match (doc.text, doc.base64) {
        (Some(text), None) if is_text(&doc.mime_type) => text.into_bytes(),
        (None, Some(data)) => {
            ensure!(
                data.len() <= config.media.max_file_bytes.div_ceil(3) * 4,
                "File exceeds this connection's byte limit"
            );
            STANDARD.decode(data).context("Invalid base64 data")?
        }
        _ => anyhow::bail!(
            "Supply exactly one of UTF-8 text for a text document, or base64 contents"
        ),
    };
    ensure!(
        !bytes.is_empty() && bytes.len() <= config.media.max_file_bytes,
        "Empty file or file exceeds this connection's byte limit"
    );
    crate::media::validate_bytes(&doc.mime_type, &bytes)?;
    let sha = format!("{:x}", Sha256::digest(&bytes));
    ensure!(
        doc.sha256
            .as_ref()
            .is_none_or(|expected| expected.eq_ignore_ascii_case(&sha)),
        "The supplied contents do not match the original SHA-256"
    );
    let mut store = Store::open(home)?;
    let tx = store
        .db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let source = crate::huddles::reply_job_source(&tx, config, job)?;
    ensure!(
        (source.channel.as_str(), source.thread.as_str()) == conversation,
        "Attachment grant differs from the current reply destination"
    );
    let prior: Option<bool> = tx
        .query_row(
            "SELECT bytes=?3 AND mime_type=?4 FROM reply_files WHERE job_id=?1 AND name=?2",
            params![job, doc.filename, bytes, doc.mime_type],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(same) = prior {
        ensure!(
            same,
            "A different attachment already uses that filename in this reply"
        );
    } else {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM reply_files WHERE job_id=?1",
            [job],
            |r| r.get(0),
        )?;
        ensure!(
            count < config.media.max_files as i64,
            "Attachment count limit reached for this reply"
        );
        tx.execute(
            "INSERT INTO reply_files(job_id,name,mime_type,bytes) VALUES(?1,?2,?3,?4)",
            params![job, doc.filename, doc.mime_type, bytes],
        )?;
    }
    tx.commit()?;
    Ok(
        json!({"status":"staged_for_reply","filename":doc.filename,"bytes":bytes.len(),"sha256":sha}),
    )
}

pub fn for_reply(store: &Store, job: &str) -> Result<Vec<OutputFile>> {
    let mut q = store
        .db
        .prepare("SELECT name,mime_type,bytes FROM reply_files WHERE job_id=?1 ORDER BY rowid")?;
    let rows = q.query_map([job], |r| {
        Ok(OutputFile {
            name: r.get(0)?,
            mime_type: r.get(1)?,
            data: STANDARD.encode(r.get::<_, Vec<u8>>(2)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
