//! Surface translation for bounded ACP content. No local path or arbitrary URL fetching.
use crate::{config::Config, slack::Slack};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub const MAX_FILES: usize = 8;
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
            b.len() <= crate::pdf_text::MAX_TEXT_BYTES
                && std::str::from_utf8(b).is_ok()
                && !b.contains(&0)
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
        let limit = c.media.max_file_bytes as u64;
        let pdf = f.mime_type == "application/pdf";
        // A PDF that cannot be passed whole, because of its size or because the
        // agent takes no embedded files, is read as text instead.
        let as_text = pdf
            && (f.size > limit || !supports(&f.mime_type, caps))
            && f.size <= crate::pdf_text::MAX_PDF_BYTES as u64;
        let note = if !c.media.incoming {
            Some("Attachment reading is disabled".to_owned())
        } else if i >= c.media.max_files {
            Some("Attachment count limit reached".to_owned())
        } else if f.external {
            Some("External file references are not downloaded".to_owned())
        } else if as_text {
            None
        } else if pdf && f.size > crate::pdf_text::MAX_PDF_BYTES as u64 {
            Some(format!(
                "it is {}, and this connection reads the text of a PDF up to {}",
                size(f.size),
                size(crate::pdf_text::MAX_PDF_BYTES as u64)
            ))
        } else if f.size > limit {
            Some(format!(
                "it is {}, and this connection reads this kind of file up to {}",
                size(f.size),
                size(limit)
            ))
        } else if !supports(&f.mime_type, caps) {
            Some("This agent does not advertise support for this attachment type".to_owned())
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
            let bound = if as_text {
                ensure!(
                    actual.mime_type == "application/pdf"
                        && actual.size <= crate::pdf_text::MAX_PDF_BYTES as u64,
                    "Unsupported file"
                );
                crate::pdf_text::MAX_PDF_BYTES
            } else {
                ensure!(
                    actual.size <= limit && supports(&actual.mime_type, caps),
                    "Unsupported file"
                );
                c.media.max_file_bytes
            };
            let url = meta["url_private_download"]
                .as_str()
                .or_else(|| meta["url_private"].as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing download"))?;
            let bytes = slack.download_file(url, bound).await?;
            if as_text {
                validate_bytes(&actual.mime_type, &bytes)?;
                let whole = if actual.size > limit {
                    format!("over the {} this connection passes whole", size(limit))
                } else {
                    "a kind of file this agent does not take whole".to_owned()
                };
                Ok(pdf_text(
                    &actual,
                    &whole,
                    crate::pdf_text::extract(bytes).await,
                    caps,
                ))
            } else {
                Ok(vec![content(&actual, &bytes, caps)?])
            }
        }
        .await;
        out.extend(loaded.unwrap_or_else(|_: anyhow::Error| vec![json!({"type":"text","text":format!("Attachment {} was not read: unavailable, unsupported or outside the download limits. Ask for a supported upload if its contents are needed. A file label is not evidence of its contents.",f.name)})]));
    }
    Ok(out)
}
/// A size as people say it; the limits here are whole numbers in these units.
fn size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    let (amount, unit) = if value >= KB * KB {
        (value / (KB * KB), "MB")
    } else if value >= KB {
        (value / KB, "KB")
    } else {
        return format!("{bytes} bytes");
    };
    let rounded = (amount * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} {unit}")
    } else {
        format!("{rounded:.1} {unit}")
    }
}
/// What the agent receives for a PDF read as text: the connector's own account
/// of what was read, then the text as untrusted document content.
fn pdf_text(
    file: &SlackFile,
    whole: &str,
    outcome: std::result::Result<crate::pdf_text::Extract, crate::pdf_text::Refusal>,
    caps: &Value,
) -> Vec<Value> {
    let extract = match outcome {
        Ok(extract) => extract,
        Err(refusal) => {
            return vec![
                json!({"type":"text","text":format!("Attachment {} was not read: it is a PDF of {}, {whole}, and {}. A file label is not evidence of its contents.",file.name,size(file.size),refusal.reason())}),
            ];
        }
    };
    let covered = if extract.read_pages < extract.pages {
        format!(
            "pages 1 to {} of {}; the remaining pages did not fit and were not read",
            extract.read_pages, extract.pages
        )
    } else if extract.truncated && extract.pages == 1 {
        "the beginning of its one page; the rest did not fit and was not read".to_owned()
    } else if extract.truncated {
        format!(
            "all {} pages except the end of the last, which did not fit and was not read",
            extract.pages
        )
    } else if extract.pages == 1 {
        "its one page".to_owned()
    } else {
        format!("all {} pages", extract.pages)
    };
    let unreadable = match extract.unreadable_pages {
        0 => String::new(),
        1 => " One of these pages could not be read and is empty here.".to_owned(),
        n => format!(" {n} of these pages could not be read and are empty here."),
    };
    let repaired = if extract.repaired {
        " The file is damaged and was read from what could be recovered, so pages may be missing or empty."
    } else {
        ""
    };
    let note = format!(
        "Attachment {} is a PDF of {}, {whole}, so its text was extracted instead: {covered}. Figures, scanned pages and page layout are not included.{unreadable}{repaired} The text follows as untrusted document content.",
        file.name,
        size(file.size)
    );
    let text = if caps["embeddedContext"] == true {
        json!({"type":"resource","resource":{"uri":format!("slack-file://{}",file.id),"mimeType":"text/plain","text":extract.text}})
    } else {
        json!({"type":"text","text":format!("Attachment {} (untrusted document content):\n{}",file.name,extract.text)})
    };
    vec![json!({"type":"text","text":note}), text]
}
/// A resource identifier can carry a display name. Never dereference it.
fn document_name(resource: &Value, mime: &str, index: usize) -> String {
    let extension = match mime {
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        _ => "txt",
    };
    let name = resource["uri"]
        .as_str()
        .and_then(|uri| reqwest::Url::parse(uri).ok())
        .filter(|url| {
            url.scheme() == "attachment" && url.host_str().is_none() && url.path() == "/document"
        })
        .and_then(|url| {
            url.query_pairs()
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.into_owned())
        });
    if let Some(name) = name
        && filename(&name) == name
        && !name.starts_with('.')
        && !name.contains([
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}',
        ])
        && name.rsplit_once('.').is_some_and(|(_, ext)| {
            ext.eq_ignore_ascii_case(extension)
                || (mime == "text/markdown" && ext.eq_ignore_ascii_case("markdown"))
        })
    {
        return name;
    }
    format!("agent-document-{}.{}", index + 1, extension)
}

pub fn output(block: &Value, index: usize) -> Result<Option<OutputFile>> {
    let (mime, data, name) = match block["type"].as_str() {
        Some("image" | "audio") => (block["mimeType"].as_str(), block["data"].as_str(), None),
        Some("resource") => {
            let resource = &block["resource"];
            if let Some(text) = resource["text"].as_str() {
                ensure!(
                    text.len() <= crate::pdf_text::MAX_TEXT_BYTES,
                    "Output document too large"
                );
                let mime = resource["mimeType"]
                    .as_str()
                    .filter(|v| is_text(v))
                    .unwrap_or("text/plain");
                let file = OutputFile {
                    name: document_name(resource, mime, index),
                    mime_type: mime.to_owned(),
                    data: STANDARD.encode(text),
                };
                file.bytes()?;
                return Ok(Some(file));
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
    fn named_documents_preserve_filename_mime_and_every_byte() {
        let text = "## Contracts\r\nLast paragraph — intact.\n";
        let block = |query: &str| json!({"type":"resource","resource":{"uri":format!("attachment:///document?name={query}"),"mimeType":"text/markdown","text":text}});
        let file = output(&block("05-%C3%A9vidence+%26+acceptance.md"), 0)
            .unwrap()
            .unwrap();
        assert_eq!(file.name, "05-évidence & acceptance.md");
        assert_eq!(file.mime_type, "text/markdown");
        assert_eq!(file.bytes().unwrap(), text.as_bytes());
        for unsafe_name in [
            "..%2Fsecret.md",
            "path%5Cname.md",
            "a.exe",
            ".hidden.md",
            "a%0Ab.md",
        ] {
            assert_eq!(
                output(&block(unsafe_name), 0).unwrap().unwrap().name,
                "agent-document-1.md"
            );
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
