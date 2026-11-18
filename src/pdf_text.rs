//! The text of a PDF that cannot be passed to the agent whole.
//!
//! A PDF is untrusted input, and reading one means parsing it. That happens in
//! a short-lived child of this same executable, started with `pdf-text`, which
//! is given the file's bytes on its input and nothing else: no installation
//! folder, no environment, no credentials. The parent bounds how long it waits
//! and how much it reads back. The child bounds how far any stream may inflate,
//! and on Unix also its own address space and processor time, before it looks
//! at the file. A child that fails, is killed or says something unexpected
//! means only that the file was not read.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The command-line word that makes this executable the extraction child.
pub const COMMAND: &str = "pdf-text";
/// Largest PDF whose text is read.
pub const MAX_PDF_BYTES: usize = 32 * 1024 * 1024;
/// Most text taken from one PDF: the bound that already applies to a text file.
pub const MAX_TEXT_BYTES: usize = 200_000;
/// How far one compressed stream may inflate while the file is loaded or a page read.
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHILD_MEMORY: u64 = 1024 * 1024 * 1024;
const MAX_CHILD_CPU_SECONDS: u64 = 30;
const WAIT: Duration = Duration::from_secs(45);
/// Fewer letters and digits than this in a whole document is a document of pictures.
const MIN_TEXT_CHARS: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extract {
    /// Pages in the document.
    pub pages: usize,
    /// Leading pages whose text is in `text`.
    pub read_pages: usize,
    /// Of those, pages whose content could not be decoded.
    pub unreadable_pages: usize,
    /// Whether later pages, or the rest of an oversized page, were left out.
    pub truncated: bool,
    /// Whether the file was damaged and read from what could be recovered of it.
    pub repaired: bool,
    pub text: String,
}

/// Why a PDF's text was not produced, in words for the note the agent reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Protected,
    Damaged,
    NoText,
    Failed,
}
impl Refusal {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Protected => "it is protected by a password",
            Self::Damaged => "it is damaged or not a PDF this connection can read",
            Self::NoText => "it has no text to extract; its pages are images, such as scans",
            Self::Failed => "reading its text failed or took too long",
        }
    }
}

fn tidy(page: &str) -> String {
    let mut out = String::with_capacity(page.len());
    let mut blank = 0;
    for line in page.lines() {
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

/// Read the document in this process. Only the child calls this on a real file.
pub fn read(bytes: &[u8], budget: usize) -> std::result::Result<Extract, Refusal> {
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(MAX_STREAM_BYTES),
        ..Default::default()
    };
    let document =
        lopdf::Document::load_mem_with_options(bytes, options).map_err(|error| match error {
            lopdf::Error::InvalidPassword | lopdf::Error::Decryption(_) => Refusal::Protected,
            _ => Refusal::Damaged,
        })?;
    let pages = document.get_pages();
    let mut extract = Extract {
        pages: pages.len(),
        read_pages: 0,
        unreadable_pages: 0,
        truncated: false,
        repaired: matches!(
            document.xref_recovery,
            Some(lopdf::xref::XrefRecovery::Reconstructed)
        ),
        text: String::new(),
    };
    let mut letters = 0;
    for number in pages.keys() {
        let (page, readable) = match document.extract_text_with_limit(&[*number], MAX_STREAM_BYTES)
        {
            Ok(text) => (tidy(&text), true),
            Err(_) => (String::new(), false),
        };
        let mut piece = format!("[page {number}]\n{page}\n\n");
        if extract.text.len() + piece.len() > budget {
            extract.truncated = true;
            if extract.read_pages > 0 {
                break;
            }
            // One page larger than the whole budget: keep its beginning.
            let mut end = budget.min(piece.len());
            while !piece.is_char_boundary(end) {
                end -= 1;
            }
            piece.truncate(end);
        }
        letters += page.chars().filter(|c| c.is_alphanumeric()).count();
        extract.unreadable_pages += usize::from(!readable);
        extract.read_pages += 1;
        extract.text.push_str(&piece);
        if extract.truncated {
            break;
        }
    }
    if letters < MIN_TEXT_CHARS {
        return Err(if extract.repaired {
            Refusal::Damaged
        } else if extract.unreadable_pages > 0 {
            Refusal::Failed
        } else {
            Refusal::NoText
        });
    }
    extract.text.truncate(extract.text.trim_end().len());
    Ok(extract)
}

#[cfg(unix)]
fn limit_self() {
    // SAFETY: plain calls with valid pointers to initialized structures. A limit
    // the system refuses is left as it was; the stream bound still applies.
    unsafe {
        for (resource, value) in [
            (libc::RLIMIT_AS, MAX_CHILD_MEMORY),
            (libc::RLIMIT_CPU, MAX_CHILD_CPU_SECONDS),
            (libc::RLIMIT_CORE, 0),
        ] {
            let limit = libc::rlimit {
                rlim_cur: value as libc::rlim_t,
                rlim_max: value as libc::rlim_t,
            };
            libc::setrlimit(resource, &limit);
        }
    }
}
#[cfg(not(unix))]
fn limit_self() {}

/// The child's whole life: bound itself, read one PDF from its input, print one
/// JSON line, and return its exit code.
pub fn child() -> i32 {
    limit_self();
    // A failed parse must not write the document's content anywhere.
    std::panic::set_hook(Box::new(|_| {}));
    let mut bytes = Vec::new();
    let read_input = std::io::stdin()
        .lock()
        .take(MAX_PDF_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if read_input.is_err() || bytes.len() > MAX_PDF_BYTES {
        return 2;
    }
    let outcome =
        std::panic::catch_unwind(|| read(&bytes, MAX_TEXT_BYTES)).unwrap_or(Err(Refusal::Failed));
    let line = match outcome {
        Ok(extract) => serde_json::json!({"extract":extract}),
        Err(refusal) => serde_json::json!({"refusal":refusal}),
    };
    println!("{line}");
    0
}

/// Read a PDF's text in a child of this executable.
pub async fn extract(bytes: Vec<u8>) -> std::result::Result<Extract, Refusal> {
    let Ok(program) = std::env::current_exe() else {
        return Err(Refusal::Failed);
    };
    extract_with(&program, bytes).await
}

/// As `extract`, with the executable named, so a test can use the built one.
pub async fn extract_with(program: &Path, bytes: Vec<u8>) -> std::result::Result<Extract, Refusal> {
    if bytes.len() > MAX_PDF_BYTES {
        return Err(Refusal::Failed);
    }
    match tokio::time::timeout(WAIT, run(program, bytes)).await {
        Ok(Ok(outcome)) => outcome,
        // A timeout drops the child, which kills it.
        Ok(Err(_)) | Err(_) => Err(Refusal::Failed),
    }
}

async fn run(program: &Path, bytes: Vec<u8>) -> Result<std::result::Result<Extract, Refusal>> {
    let mut command = tokio::process::Command::new(program);
    command
        .arg(COMMAND)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        if let Ok(root) = std::env::var("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    let mut input = child.stdin.take().context("Missing child input")?;
    let output = child.stdout.take().context("Missing child output")?;
    let write = async move {
        // A child that stops reading early has already decided; its answer matters.
        let _ = input.write_all(&bytes).await;
        let _ = input.shutdown().await;
    };
    let read = async move {
        let mut line = Vec::new();
        output
            .take(MAX_TEXT_BYTES as u64 * 2 + 4096)
            .read_to_end(&mut line)
            .await
            .map(|_| line)
    };
    let ((), line) = tokio::join!(write, read);
    let status = child.wait().await?;
    ensure!(status.success(), "Extraction child failed");
    let value: serde_json::Value = serde_json::from_slice(&line?)?;
    if let Some(refusal) = value.get("refusal") {
        return Ok(Err(serde_json::from_value(refusal.clone())?));
    }
    let extract: Extract = serde_json::from_value(value["extract"].clone())?;
    ensure!(
        extract.text.len() <= MAX_TEXT_BYTES
            && extract.read_pages <= extract.pages
            && extract.unreadable_pages <= extract.read_pages,
        "Extraction child returned an impossible result"
    );
    Ok(Ok(extract))
}

#[cfg(test)]
mod sample {
    include!("../tests/support/pdf_sample.rs");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_read_in_order_with_their_numbers() {
        let bytes = sample::document(&[
            "Quarterly review\nThe connector delivered 412 replies.",
            "",
            "Second section\nA closing line.",
        ]);
        assert!(bytes.starts_with(b"%PDF-"));
        let extract = read(&bytes, MAX_TEXT_BYTES).unwrap();
        assert_eq!(
            extract,
            Extract {
                pages: 3,
                read_pages: 3,
                unreadable_pages: 0,
                truncated: false,
                repaired: false,
                text: "[page 1]\nQuarterly review\nThe connector delivered 412 replies.\n\n[page 2]\n\n\n[page 3]\nSecond section\nA closing line.".into(),
            }
        );
    }

    #[test]
    fn text_beyond_the_budget_is_left_out_and_said_to_be() {
        let line = "A line of ordinary words about the connector and its threads.";
        let page = [line; 20].join("\n");
        let pages = vec![page.as_str(); 10];
        let bytes = sample::document(&pages);
        let extract = read(&bytes, 4_000).unwrap();
        assert_eq!(extract.pages, 10);
        assert_eq!(extract.read_pages, 3);
        assert!(extract.truncated && extract.text.len() <= 4_000);
        assert!(extract.text.contains("[page 3]") && !extract.text.contains("[page 4]"));
        // A single page over the whole budget keeps its beginning.
        let one = read(&sample::document(&[page.as_str()]), 300).unwrap();
        assert_eq!((one.pages, one.read_pages, one.truncated), (1, 1, true));
        assert!(one.text.len() <= 300 && one.text.starts_with("[page 1]\nA line of ordinary"));
    }

    #[test]
    fn a_document_of_pictures_or_of_noise_is_not_presented_as_text() {
        assert_eq!(
            read(&sample::document(&["", "", ""]), MAX_TEXT_BYTES),
            Err(Refusal::NoText)
        );
        assert_eq!(
            read(b"%PDF-1.7\nnot a document at all", MAX_TEXT_BYTES),
            Err(Refusal::Damaged)
        );
        assert_eq!(read(b"", MAX_TEXT_BYTES), Err(Refusal::Damaged));
    }

    #[test]
    fn a_damaged_file_is_read_from_what_can_be_recovered_and_said_to_be_damaged() {
        let page = "A line of ordinary words about the connector and its threads.";
        let whole = sample::document(&[page; 8]);
        assert!(!read(&whole, MAX_TEXT_BYTES).unwrap().repaired);
        // Without its table of objects the file is rebuilt by scanning it.
        let table = whole.windows(5).rposition(|w| w == b"xref\n").unwrap();
        let mut damaged = whole.clone();
        damaged[table..table + 4].copy_from_slice(b"lost");
        let extract = read(&damaged, MAX_TEXT_BYTES).unwrap();
        assert!(extract.repaired);
        assert_eq!((extract.pages, extract.read_pages), (8, 8));
        assert!(extract.text.contains(page));
        // Too little survives of this one to call it text.
        assert_eq!(read(&whole[..200], MAX_TEXT_BYTES), Err(Refusal::Damaged));
    }

    #[test]
    fn a_stream_that_inflates_past_the_bound_is_not_inflated() {
        // 80 MiB of blanks compress to a few tens of kilobytes.
        let bytes = sample::inflating(80 * 1024 * 1024);
        assert!(bytes.len() < 1024 * 1024);
        assert_eq!(read(&bytes, MAX_TEXT_BYTES), Err(Refusal::Failed));
    }

    #[test]
    fn control_characters_and_runs_of_blank_lines_are_removed() {
        assert_eq!(
            tidy("  Title \u{0}\u{7}\n\n\n\nBody\tline  \r\n\u{c}\nLast"),
            "  Title\n\nBody\tline\n\nLast"
        );
    }
}
