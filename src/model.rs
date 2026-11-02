use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Message {
    pub team: String,
    pub channel: String,
    pub thread: String,
    pub ts: String,
    pub revision: String,
    pub sender: String,
    pub bot: bool,
    pub text: String,
    pub deleted: bool,
    #[serde(default)]
    pub files: Vec<crate::media::SlackFile>,
}
impl Message {
    pub fn context(&self) -> String {
        context_id(&self.team, &self.channel, &self.thread)
    }
}
pub fn context_id(team: &str, channel: &str, thread: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("slack:{team}:{channel}:{thread}").as_bytes(),
    )
    .to_string()
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_deserializing)]
    pub media: Vec<crate::media::OutputFile>,
}
impl Decision {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.media.len() <= 4, "Too many output files");
        for file in &self.media {
            file.bytes()?;
        }
        anyhow::ensure!(
            ["speak", "pass"].contains(&self.decision.as_str()),
            "Expected speak or pass"
        );
        anyhow::ensure!(self.reason.len() <= 4000, "Decision reason too large");
        if self.decision == "speak" {
            anyhow::ensure!(
                !self.text.trim().is_empty() && self.text.chars().count() <= 3800,
                "Public contribution must contain 1..3800 characters"
            );
        } else {
            anyhow::ensure!(
                self.text.trim().is_empty(),
                "Pass cannot contain public text"
            );
            anyhow::ensure!(self.media.is_empty(), "Pass cannot contain media");
        }
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
