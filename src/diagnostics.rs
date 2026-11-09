//! Fixed diagnostic categories; never expose provider bodies, prompts or stderr.
use rusqlite::OptionalExtension;
#[derive(Debug)]
pub struct Failure {
    pub stage: &'static str,
    pub category: &'static str,
    pub delivery: &'static str,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.summary())
    }
}
impl std::error::Error for Failure {}
impl Failure {
    pub fn not_forwarded() -> Self {
        Self {
            stage: "attention_admission",
            category: "not_forwarded",
            delivery: "not_started",
        }
    }
    pub fn turn_limit() -> Self {
        Self {
            stage: "prompt_terminal",
            category: "turn_limit",
            delivery: "not_sent",
        }
    }
    pub fn unsettled_limit() -> Self {
        Self {
            stage: "prompt_terminal",
            category: "turn_limit_unsettled",
            delivery: "unknown",
        }
    }
    pub fn refused() -> Self {
        Self {
            stage: "prompt_terminal",
            category: "refusal",
            delivery: "not_sent",
        }
    }
    pub fn answer() -> Self {
        Self {
            stage: "answer_decode",
            category: "invalid_answer",
            delivery: "not_sent",
        }
    }
    pub fn summary(&self) -> &'static str {
        match self.category {
            "not_forwarded" => {
                "The host did not forward this attempt to the agent. Earlier work may still exist; this attempt has not been retried."
            }
            "turn_limit" => {
                "The agent reached its turn limit. Earlier work may be complete; no continuation was scheduled. Check the existing results before repeating work."
            }
            "turn_limit_unsettled" => {
                "The agent reached its turn limit with unresolved operations. Their outcome must be checked before continuing; no work was repeated."
            }
            "refusal" => {
                "The agent declined to continue this request. No automatic retry was scheduled."
            }
            "invalid_answer" => {
                "The agent finished, but its answer could not be read. No reply was sent; completed work was not repeated."
            }
            _ => {
                "The agent connection stopped before completion was confirmed. Check activity before repeating work."
            }
        }
    }
}
pub fn details(error: &anyhow::Error) -> (&'static str, &'static str, &'static str, &'static str) {
    if let Some(f) = error.downcast_ref::<Failure>() {
        (f.stage, f.category, f.delivery, f.summary())
    } else {
        (
            "execution",
            "backend_or_transport",
            "unknown",
            "The agent connection stopped before completion was confirmed. Check activity before repeating work.",
        )
    }
}

/// Public status uses fixed connector facts, never a provider's error text.
pub fn recorded_summary(
    store: &crate::store::Store,
    job: &str,
) -> anyhow::Result<Option<&'static str>> {
    let category:Option<String>=store.db.query_row(
        "SELECT json_extract(data,'$.category') FROM audit WHERE kind='driver_failed' AND json_extract(data,'$.job')=?1 ORDER BY seq DESC LIMIT 1",
        [job],|r|r.get(0),
    ).optional()?;
    Ok(match category.as_deref() {
        Some("invalid_answer") => Some(Failure::answer().summary()),
        Some("not_forwarded") => Some(Failure::not_forwarded().summary()),
        Some("turn_limit") => Some(Failure::turn_limit().summary()),
        Some("turn_limit_unsettled") => Some(Failure::unsettled_limit().summary()),
        Some("refusal") => Some(Failure::refused().summary()),
        _ => None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn never_repeats_untrusted_error_content() {
        let e = anyhow::anyhow!("private credential and prompt").context(Failure::answer());
        let d = details(&e);
        assert_eq!(d.0, "answer_decode");
        assert_eq!(d.2, "not_sent");
        assert!(!d.3.contains("credential"));
    }
}
