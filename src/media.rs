//! Surface translation for bounded ACP content. No local path or arbitrary URL fetching.
use crate::{config::Config, slack::Slack};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlackFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub external: bool,
}
impl SlackFile {
    pub fn from_slack(v: &Value) -> Option<Self> {
        let id = v["id"].as_str()?;
        if !id.starts_with('F') || id.len() > 80 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Self {
            id: id.into(),
            name: filename(v["name"].as_str().unwrap_or("attachment")),
            mime_type: match v["mimetype"].as_str().unwrap_or("application/octet-stream") {
                // Slack identifies uploaded WAV files using this IANA name.
                // ACP/model clients generally negotiate the audio/wav spelling.
                "audio/vnd.wave" | "audio/x-wav" => "audio/wav",
                mime => mime,
            }
            .chars()
            .take(100)
            .collect(),
            size: v["size"].as_u64().unwrap_or(0),
            external: v["is_external"] == true || v["mode"] == "external",
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputFile {
    pub name: String,
    pub mime_type: String,
    pub data: String,
}
impl OutputFile {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            self.data.len() <= MAX_BYTES.div_ceil(3) * 4,
            "Output file too large"
        );
        let bytes = STANDARD.decode(&self.data)?;
        ensure!(
            bytes.len() <= MAX_BYTES && !bytes.is_empty(),
            "Invalid output file size"
        );
        validate_bytes(&self.mime_type, &bytes)?;
        Ok(bytes)
    }
}
pub fn filename(value: &str) -> String {
    let name: String = value
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
        .take(120)
        .collect();
    if name.trim().is_empty() || name == "." || name == ".." {
        "attachment".into()
    } else {
        name
    }
}
pub fn is_text(mime: &str) -> bool {
    matches!(
        mime,
        "text/plain" | "text/markdown" | "text/csv" | "application/json"
    )
}
pub fn validate_bytes(mime: &str, b: &[u8]) -> Result<()> {
    let valid = match mime {
        "image/png" => b.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => b.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a"),
        "image/webp" => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"),
        "audio/wav" | "audio/x-wav" => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE"),
        "audio/mpeg" => {
            b.starts_with(b"ID3")
                || (b.first() == Some(&0xff) && b.get(1).is_some_and(|v| v & 0xe0 == 0xe0))
        }
        "application/pdf" => b.starts_with(b"%PDF-"),
        _ if is_text(mime) => {
            b.len() <= 200_000 && std::str::from_utf8(b).is_ok() && !b.contains(&0)
        }
        _ => false,
    };
    ensure!(valid, "Unsupported attachment type or mismatched content");
    Ok(())
}
pub fn supports(mime: &str, caps: &Value) -> bool {
    is_text(mime)
        || (mime.starts_with("image/") && caps["image"] == true)
        || (mime.starts_with("audio/") && caps["audio"] == true)
        || (mime == "application/pdf" && caps["embeddedContext"] == true)
}
pub fn content(file: &SlackFile, bytes: &[u8], caps: &Value) -> Result<Value> {
    validate_bytes(&file.mime_type, bytes)?;
    ensure!(
        supports(&file.mime_type, caps),
        "Agent does not accept this attachment type"
    );
    if is_text(&file.mime_type) {
        let text = std::str::from_utf8(bytes)?;
        if caps["embeddedContext"] == true {
            Ok(
                json!({"type":"resource","resource":{"uri":format!("slack-file://{}",file.id),"mimeType":file.mime_type,"text":text}}),
            )
        } else {
            Ok(
                json!({"type":"text","text":format!("Attachment {} (untrusted document content):\n{}",file.name,text)}),
            )
        }
    } else if file.mime_type.starts_with("image/") || file.mime_type.starts_with("audio/") {
        Ok(
            json!({"type":if file.mime_type.starts_with("image/") {"image"} else {"audio"},"mimeType":file.mime_type,"data":STANDARD.encode(bytes)}),
        )
    } else {
        Ok(
            json!({"type":"resource","resource":{"uri":format!("slack-file://{}",file.id),"mimeType":file.mime_type,"blob":STANDARD.encode(bytes)}}),
        )
    }
}
pub async fn prompt(c: &Config, home: &Path, input: &Value, caps: &Value) -> Result<Vec<Value>> {
    let files = input["trigger"]["files"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if files.is_empty() {
        return Ok(vec![]);
    }
    let slack = Slack::new(c, c.secrets(home)?)?;
    let mut out = vec![];
    for (i, value) in files.iter().enumerate() {
        let f: SlackFile = serde_json::from_value(value.clone())?;
        let note = if !c.media.incoming {
            Some("Attachment reading is disabled")
        } else if i >= c.media.max_files {
            Some("Attachment count limit reached")
        } else if f.external {
            Some("External file references are not downloaded")
        } else if f.size > c.media.max_file_bytes as u64 {
            Some("Attachment exceeds the configured size limit")
        } else if !supports(&f.mime_type, caps) {
            Some("This agent does not advertise support for this attachment type")
        } else {
            None
        };
        if let Some(note) = note {
            out.push(
                json!({"type":"text","text":format!("Attachment {} was not read: {note}.",f.name)}),
            );
            continue;
        }
        // Only current-trigger IDs from a verified Slack event are used. Re-fetch
        // metadata rather than trusting stale signed links in message history.
        let loaded = async {
            let metadata = slack
                .call("files.info", json!({"file":f.id}), false)
                .await
                .map_err(|_| anyhow::anyhow!("File metadata unavailable"))?;
            let meta = &metadata["file"];
            ensure!(
                meta["id"] == f.id && meta["is_external"] != true && meta["mode"] != "external",
                "External or mismatched file"
            );
            let actual = SlackFile::from_slack(meta)
                .ok_or_else(|| anyhow::anyhow!("Invalid file metadata"))?;
            ensure!(
                actual.size <= c.media.max_file_bytes as u64 && supports(&actual.mime_type, caps),
                "Unsupported file"
            );
            let url = meta["url_private_download"]
                .as_str()
                .or_else(|| meta["url_private"].as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing download"))?;
            let bytes = slack.download_file(url, c.media.max_file_bytes).await?;
            content(&actual, &bytes, caps)
        }
        .await;
        out.push(loaded.unwrap_or_else(|_| json!({"type":"text","text":format!("Attachment {} was not read: unavailable, unsupported or outside the download limits. Ask for a supported upload if its contents are needed. A file label is not evidence of its contents.",f.name)})));
    }
    Ok(out)
}
pub fn output(block: &Value, index: usize) -> Result<Option<OutputFile>> {
    let (mime, data, name) = match block["type"].as_str() {
        Some("image" | "audio") => (block["mimeType"].as_str(), block["data"].as_str(), None),
        Some("resource") => {
            let resource = &block["resource"];
            if let Some(text) = resource["text"].as_str() {
                ensure!(text.len() <= 200_000, "Output document too large");
                return Ok(Some(OutputFile {
                    name: format!("agent-document-{}.txt", index + 1),
                    mime_type: "text/plain".into(),
                    data: STANDARD.encode(text),
                }));
            }
            (
                resource["mimeType"].as_str(),
                resource["blob"].as_str(),
                None,
            )
        }
        Some("resource_link") => {
            return Err(anyhow::anyhow!(
                "Referenced resources cannot be uploaded without embedded contents"
            ));
        }
        _ => return Ok(None),
    };
    let mime = mime.ok_or_else(|| anyhow::anyhow!("Missing output MIME type"))?;
    let extension = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        _ => "txt",
    };
    let file = OutputFile {
        name: name.unwrap_or_else(|| format!("agent-attachment-{}.{}", index + 1, extension)),
        mime_type: mime.into(),
        data: data
            .ok_or_else(|| anyhow::anyhow!("Missing embedded content"))?
            .into(),
    };
    file.bytes()?;
    Ok(Some(file))
}

/// Authentication is never sent to arbitrary remote-file links or redirects.
pub fn file_url(value: &str, test: bool) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials forbidden"
    );
    ensure!(
        if test {
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
        } else {
            url.scheme() == "https"
                && url.host_str() == Some("files.slack.com")
                && url.port_or_known_default() == Some(443)
        },
        "Untrusted file host"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slack_wave_mime_aliases_keep_native_audio_and_validate_bytes() {
        let bytes = b"RIFF\x04\x00\x00\x00WAVE";
        for mime in ["audio/vnd.wave", "audio/x-wav", "audio/wav"] {
            let file = SlackFile::from_slack(
                &json!({"id":"FWAVE","name":"tone.wav","mimetype":mime,"size":bytes.len()}),
            )
            .unwrap();
            assert_eq!(file.mime_type, "audio/wav");
            let block = content(&file, bytes, &json!({"audio":true})).unwrap();
            assert_eq!(block["type"], "audio");
            assert_eq!(block["mimeType"], "audio/wav");
            assert_eq!(output(&block, 0).unwrap().unwrap().bytes().unwrap(), bytes);
            assert!(content(&file, bytes, &json!({})).is_err());
            assert!(content(&file, b"not a wav", &json!({"audio":true})).is_err());
        }
    }
    #[test]
    fn strict_hosts_and_media_boundaries() {
        for bad in [
            "http://files.slack.com/a",
            "https://files.slack.com.attacker.test/a",
            "https://files.slack.com@evil.test/a",
            "https://files.slack.com:444/a",
            "https://127.0.0.1/a",
            "file:///etc/passwd",
        ] {
            assert!(file_url(bad, false).is_err());
        }
        assert!(file_url("https://files.slack.com/files-pri/x", false).is_ok());
        assert!(file_url("http://127.0.0.1:9000/file", true).is_ok());
        assert!(validate_bytes("image/png", b"not a png").is_err());
        assert!(validate_bytes("text/html", b"<script>bad</script>").is_err());
        let f = SlackFile {
            id: "FTEST".into(),
            name: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size: 4,
            external: false,
        };
        assert_eq!(content(&f, b"note", &json!({})).unwrap()["type"], "text");
        assert_eq!(
            content(&f, b"note", &json!({"embeddedContext":true})).unwrap()["type"],
            "resource"
        );
        assert!(!supports("image/png", &json!({})));
        assert!(supports("image/png", &json!({"image":true})));
        assert!(
            output(
                &json!({"type":"resource_link","uri":"file:///etc/passwd"}),
                0
            )
            .is_err()
        );
    }
}
