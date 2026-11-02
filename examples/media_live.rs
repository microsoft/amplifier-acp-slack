//! Explicit, single-message live Slack media smoke check. Never runs in CI.
use acp_slack_adapter::{config::Config, media, slack::Slack, store::Outgoing};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(
        (args.len() == 4 && args[0] == "--send-one-live-check")
            || (args.len() == 5 && args[0] == "--verify-existing-check"),
        "Usage: media_live --send-one-live-check ADAPTER_HOME EXISTING_DM FIXTURE_JSON; or --verify-existing-check ADAPTER_HOME EXISTING_DM FIXTURE_JSON MESSAGE_TS"
    );
    let home = PathBuf::from(&args[1]);
    let channel = &args[2];
    ensure!(
        channel.starts_with('D') && channel.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Only an existing private DM is accepted"
    );
    let c = Config::load(&home)?;
    ensure!(
        c.media.incoming && c.media.outgoing && !c.slack.loopback_test,
        "Live media must be enabled first"
    );
    let slack = Slack::new(&c, c.secrets(&home)?)?;
    slack.verify(&c).await?;
    ensure!(
        slack.media_readiness(&c)["ready"] == true,
        "Installed file grants are required"
    );
    let info: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get("https://slack.com/api/conversations.info")
        .query(&[("channel", channel)])
        .bearer_auth(c.secrets(&home)?.bot_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(info["ok"] == true, "Existing DM could not be verified");
    ensure!(
        info["channel"]["is_im"] == true
            && info["channel"]["user"] == c.context.owner_slack_user_id,
        "Destination must be the configured owner's existing DM"
    );
    let blocks: Vec<Value> = serde_json::from_slice(&std::fs::read(&args[3])?)?;
    ensure!(
        blocks.len() == 3,
        "Expected exactly three tiny fixture blocks"
    );
    let output: Vec<_> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| media::output(b, i)?.context("Missing fixture output"))
        .collect::<Result<_>>()?;
    ensure!(
        output
            .iter()
            .map(|f| f.bytes().map(|b| b.len()))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .sum::<usize>()
            < 16384,
        "Live fixtures are limited to 16KB total"
    );
    let message = Outgoing {
        id: uuid::Uuid::new_v4().to_string(), channel: channel.clone(), thread: String::new(),
        text: "**Automated media check** — one tiny test image, a brief silent audio file, and a text attachment. These verify the Slack adapter’s upload/download path for native ACP content. They are test fixtures, not model-generated media. No action needed.".into(),
        media: output.clone(),
    };
    // No retry: an uncertain write must be inspected before anything is resent.
    let posted = args[0] == "--send-one-live-check";
    let ts = if posted {
        slack
            .publish(&message)
            .await
            .map_err(|e| anyhow::anyhow!("Live delivery: {e:?}; inspect the DM before retrying"))?
    } else {
        ensure!(
            args[4].bytes().all(|b| b.is_ascii_digit() || b == b'.'),
            "Invalid existing timestamp"
        );
        args[4].clone()
    };
    let mut files = Vec::new();
    for _ in 0..12 {
        let replies = slack
            .call(
                "conversations.replies",
                json!({"channel":channel,"ts":ts,"limit":20}),
                false,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Read delivered files: {e:?}"))?;
        ensure!(
            replies["messages"][0]["user"] == c.slack.bot_user_id
                && replies["messages"][0]["text"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("*Automated media check*")),
            "Expected this app's existing verification message"
        );
        files = replies["messages"]
            .as_array()
            .context("Missing thread messages")?
            .iter()
            .filter(|m| m["user"] == c.slack.bot_user_id)
            .flat_map(|m| m["files"].as_array().cloned().unwrap_or_default())
            .collect();
        if files.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    ensure!(
        files.len() == 3,
        "Expected three delivered files; inspect the DM before retrying"
    );
    let mut checks = Vec::new();
    for (index, expected) in output.iter().enumerate() {
        let file = files
            .iter()
            .find(|f| f["name"] == expected.name)
            .context("Delivered fixture file missing")?;
        let metadata = media::SlackFile::from_slack(file).context("Invalid Slack file metadata")?;
        let translated = media::prompt(
            &c,
            &home,
            &json!({"trigger":{"files":[metadata]}}),
            &json!({"image":true,"audio":true,"embeddedContext":true}),
        )
        .await?;
        ensure!(translated.len() == 1, "Missing input translation");
        let actual = media::output(&translated[0], index)?
            .context("Incoming fixture was not native content")?;
        let bytes = expected.bytes()?;
        ensure!(actual.bytes()? == bytes, "Round-trip file bytes differ");
        checks.push(json!({"id":file["id"],"name":expected.name,"mime_type":expected.mime_type,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"native_acp_input_type":translated[0]["type"],"verified":true}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"kind":"live-slack-transport","adapter":c.label,"app_id":c.slack.app_id,"channel":channel,"message_ts":ts,"message_url":format!("https://app.slack.com/client/{}/{}?message_ts={}",c.slack.team_id,channel,ts),"posts_in_this_run":if posted {1}else{0},"single_original_verification_message":true,"files":checks,"model_invoked":false,"acp_source":"synthetic native blocks through production output and input translation","verified_at":acp_slack_adapter::model::now_ms()})
        )?
    );
    Ok(())
}
