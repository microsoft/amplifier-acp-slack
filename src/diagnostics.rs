//! Fixed diagnostic categories; never expose provider bodies, prompts or stderr.
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
    pub fn answer() -> Self {
        Self {
            stage: "answer_decode",
            category: "invalid_answer",
            delivery: "not_sent",
        }
    }
    pub fn summary(&self) -> &'static str {
        match self.category {
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
