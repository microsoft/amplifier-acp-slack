//! The PDF text child as the connector starts it: the built executable, the
//! file on its input, one bounded line back.
use acp_slack_adapter::pdf_text::{self, Extract, MAX_PDF_BYTES, Refusal};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

mod sample {
    include!("support/pdf_sample.rs");
}

const PROGRAM: &str = env!("CARGO_BIN_EXE_acp-slack-adapter");

#[tokio::test]
async fn the_child_returns_the_text_with_page_numbers() {
    let bytes = sample::document(&["Quarterly review\nThe connector delivered 412 replies.", ""]);
    assert_eq!(
        pdf_text::extract_with(Path::new(PROGRAM), bytes).await,
        Ok(Extract {
            pages: 2,
            read_pages: 2,
            unreadable_pages: 0,
            truncated: false,
            repaired: false,
            text: "[page 1]\nQuarterly review\nThe connector delivered 412 replies.\n\n[page 2]"
                .into(),
        })
    );
}

#[tokio::test]
async fn each_kind_of_unreadable_file_has_its_own_answer() {
    let program = Path::new(PROGRAM);
    assert_eq!(
        pdf_text::extract_with(program, sample::document(&["", ""])).await,
        Err(Refusal::NoText)
    );
    assert_eq!(
        pdf_text::extract_with(program, b"%PDF-1.7\nnoise".to_vec()).await,
        Err(Refusal::Damaged)
    );
    // 80 MiB of blanks in a few tens of kilobytes: over the bound on one stream.
    assert_eq!(
        pdf_text::extract_with(program, sample::inflating(80 * 1024 * 1024)).await,
        Err(Refusal::Failed)
    );
    assert_eq!(
        pdf_text::extract_with(program, vec![b' '; MAX_PDF_BYTES + 1]).await,
        Err(Refusal::Failed)
    );
}

#[tokio::test]
async fn a_program_that_cannot_be_started_or_answers_otherwise_is_a_failed_read() {
    assert_eq!(
        pdf_text::extract_with(
            Path::new("/no/such/program"),
            sample::document(&["Some words that would otherwise be read."])
        )
        .await,
        Err(Refusal::Failed)
    );
}

#[test]
fn the_child_refuses_more_input_than_the_bound_and_prints_nothing() {
    let mut child = Command::new(PROGRAM)
        .arg(pdf_text::COMMAND)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    // The child stops reading at the bound, so the rest of the write may fail.
    let _ = input.write_all(&vec![b'%'; MAX_PDF_BYTES + 2]);
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn a_file_that_makes_the_reader_fail_leaves_nothing_of_itself_on_the_error_stream() {
    let mut child = Command::new(PROGRAM)
        .arg(pdf_text::COMMAND)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"%PDF-1.7\nPRIVATE-WORDS-IN-A-BROKEN-FILE")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        r#"{"refusal":"damaged"}"#
    );
    assert!(output.stderr.is_empty());
}
