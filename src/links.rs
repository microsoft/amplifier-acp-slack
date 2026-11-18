//! Addresses of Slack conversations and messages, in the form Slack itself
//! gives out ("Copy link", `chat.getPermalink`). Slack opens that form in the
//! client a reader is already using, and shows it as a message reference when it
//! is written into a message.
//!
//! The workspace address comes from `auth.test` at startup and is kept with the
//! installation's state. Until it is known, addresses keep the web client's
//! form, which needs only identifiers the connector already has.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

const SETTING: &str = "slack_workspace_url";

/// Accept only what `auth.test` reports for a workspace: HTTPS on a slack.com
/// workspace host, with nothing else in the address.
pub fn workspace(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    (parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && host.ends_with(".slack.com")
        && host.len() > ".slack.com".len()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none())
    .then(|| format!("https://{host}/"))
}

/// Keep the workspace address Slack reported. A missing or unusable value
/// leaves an earlier one in place.
pub fn remember(store: &Store, url: Option<&str>) -> Result<()> {
    if let Some(base) = url.and_then(workspace) {
        store.db.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",
            [SETTING, base.as_str()],
        )?;
    }
    Ok(())
}

/// A message timestamp as Slack writes it in a link: its digits, without the dot.
fn stamp(ts: &str) -> Option<String> {
    let (seconds, fraction) = ts.split_once('.')?;
    (!seconds.is_empty()
        && !fraction.is_empty()
        && ts.len() <= 40
        && seconds
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit()))
    .then(|| format!("{seconds}{fraction}"))
}

#[derive(Clone, Debug, Default)]
pub struct Links {
    base: Option<String>,
    team: String,
}

impl Links {
    pub fn new(base: Option<&str>, team: &str) -> Self {
        Self {
            base: base.and_then(workspace),
            team: team.into(),
        }
    }
    pub fn load(db: &Connection, team: &str) -> Self {
        let saved: Option<String> = db
            .query_row("SELECT value FROM settings WHERE key=?1", [SETTING], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten();
        Self::new(saved.as_deref(), team)
    }
    /// Whether addresses are in Slack's own form.
    pub fn exact(&self) -> bool {
        self.base.is_some()
    }
    pub fn conversation(&self, channel: &str) -> String {
        match &self.base {
            Some(base) => format!("{base}archives/{channel}"),
            None => format!("https://app.slack.com/client/{}/{channel}", self.team),
        }
    }
    /// `thread` is the root message's timestamp when the message belongs to a
    /// thread, the root included; Slack then opens the thread beside the channel.
    pub fn message(&self, channel: &str, ts: &str, thread: Option<&str>) -> String {
        let (Some(base), Some(digits)) = (&self.base, stamp(ts)) else {
            return format!(
                "https://app.slack.com/client/{}/{channel}?message_ts={ts}",
                self.team
            );
        };
        let mut url = format!("{base}archives/{channel}/p{digits}");
        if let Some(thread) = thread.filter(|t| stamp(t).is_some()) {
            url.push_str(&format!("?thread_ts={thread}&cid={channel}"));
        }
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The expected addresses are what chat.getPermalink returned for a channel
    // message, a thread reply, a thread's root and a direct message in a real
    // workspace, with its identifiers and times replaced.
    #[test]
    fn addresses_match_the_form_slack_gives_out() {
        let links = Links::new(Some("https://example-team.slack.com/"), "T0EXAMPLE1");
        assert!(links.exact());
        assert_eq!(
            links.message("C0EXAMPLE22", "1700000681.505459", None),
            "https://example-team.slack.com/archives/C0EXAMPLE22/p1700000681505459"
        );
        assert_eq!(
            links.message(
                "C0EXAMPLE22",
                "1700000631.231219",
                Some("1700000053.677019")
            ),
            "https://example-team.slack.com/archives/C0EXAMPLE22/p1700000631231219?thread_ts=1700000053.677019&cid=C0EXAMPLE22"
        );
        assert_eq!(
            links.message(
                "C0EXAMPLE22",
                "1700000053.677019",
                Some("1700000053.677019")
            ),
            "https://example-team.slack.com/archives/C0EXAMPLE22/p1700000053677019?thread_ts=1700000053.677019&cid=C0EXAMPLE22"
        );
        assert_eq!(
            links.message("D0EXAMPLE33", "1700000163.984379", None),
            "https://example-team.slack.com/archives/D0EXAMPLE33/p1700000163984379"
        );
        assert_eq!(
            links.conversation("C0EXAMPLE22"),
            "https://example-team.slack.com/archives/C0EXAMPLE22"
        );
    }

    #[test]
    fn an_unknown_workspace_address_keeps_the_web_client_form() {
        let links = Links::new(None, "T0EXAMPLE1");
        assert!(!links.exact());
        assert_eq!(
            links.message("C0EXAMPLE22", "1.000001", Some("1.000000")),
            "https://app.slack.com/client/T0EXAMPLE1/C0EXAMPLE22?message_ts=1.000001"
        );
        assert_eq!(
            links.conversation("C0EXAMPLE22"),
            "https://app.slack.com/client/T0EXAMPLE1/C0EXAMPLE22"
        );
        // A timestamp Slack would not write does not become part of a path.
        let exact = Links::new(Some("https://example-team.slack.com/"), "T0EXAMPLE1");
        assert_eq!(
            exact.message("C0EXAMPLE22", "1700000681", None),
            "https://app.slack.com/client/T0EXAMPLE1/C0EXAMPLE22?message_ts=1700000681"
        );
        assert_eq!(
            exact.message("C0EXAMPLE22", "1.000001", Some("not-a-timestamp")),
            "https://example-team.slack.com/archives/C0EXAMPLE22/p1000001"
        );
    }

    #[test]
    fn only_a_slack_workspace_address_is_accepted() {
        assert_eq!(
            workspace("https://example-team.slack.com/").as_deref(),
            Some("https://example-team.slack.com/")
        );
        assert_eq!(
            workspace("https://example-team.enterprise.slack.com").as_deref(),
            Some("https://example-team.enterprise.slack.com/")
        );
        for refused in [
            "http://example-team.slack.com/",
            "https://slack.com/",
            "https://.slack.com/",
            "https://example-team.slack.com.example.org/",
            "https://exampleslack.com/",
            "https://user@example-team.slack.com/",
            "https://example-team.slack.com:8443/",
            "https://example-team.slack.com/archives",
            "https://example-team.slack.com/?next=1",
            "https://example-team.slack.com/#x",
            "example-team.slack.com",
            "",
        ] {
            assert_eq!(workspace(refused), None, "{refused}");
        }
    }

    #[test]
    fn the_reported_address_is_kept_and_a_missing_one_changes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        assert!(!Links::load(&store.db, "T0EXAMPLE1").exact());
        remember(&store, Some("https://example-team.slack.com/")).unwrap();
        remember(&store, None).unwrap();
        remember(&store, Some("https://elsewhere.example.org/")).unwrap();
        assert_eq!(
            Links::load(&store.db, "T0EXAMPLE1").conversation("C0EXAMPLE22"),
            "https://example-team.slack.com/archives/C0EXAMPLE22"
        );
        remember(&store, Some("https://renamed-team.slack.com/")).unwrap();
        assert_eq!(
            Links::load(&store.db, "T0EXAMPLE1").conversation("C0EXAMPLE22"),
            "https://renamed-team.slack.com/archives/C0EXAMPLE22"
        );
    }
}
