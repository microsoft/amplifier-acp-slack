//! Presentation-only de-duplication. Source records and access checks stay intact.
use serde_json::{Value, json};

pub const META: &str = "com.microsoft.amplifier.conversation-projection";

pub fn compact(input: &mut Value) {
    let history = input["history"].as_array().cloned().unwrap_or_default();
    fn walk(value: &mut Value, history: &[Value]) {
        if let Some(obj) = value.as_object_mut() {
            let matching = history.iter().find(|m| {
                m["deleted"] == false
                    && obj.get("message_id").and_then(Value::as_str) == m["ts"].as_str()
                    && m["ts"].is_string()
                    && m["revision"].is_string()
                    && m["channel"].is_string()
                    && m["sender"].is_string()
                    && obj.get("conversation_id") == Some(&m["channel"])
                    && obj.get("revision") == Some(&m["revision"])
                    && obj.get("actor_id") == Some(&m["sender"])
                    && obj.get("text").and_then(Value::as_str).is_some_and(|text| {
                        m["text"].as_str().is_some_and(|full| {
                            text == full
                                || (obj.get("truncated") == Some(&Value::Bool(true))
                                    && full.starts_with(text))
                        })
                    })
            });
            if let Some(m) = matching {
                obj.remove("text");
                obj.insert(
                    "text_in_history".into(),
                    json!({"channel":m["channel"],"message_id":m["ts"],"revision":m["revision"]}),
                );
            }
            for child in obj.values_mut() {
                walk(child, history);
            }
        } else if let Some(a) = value.as_array_mut() {
            for child in a {
                walk(child, history);
            }
        }
    }
    if let Some(orientation) = input.get_mut("orientation") {
        walk(orientation, &history);
    }
}

pub fn metadata(input: &Value, provider: &str, init: &Value) -> Option<Value> {
    if init["_meta"]["io.github.bkrabach.agent-instance"]["continuity"]["connectorProjectionVersion"]
        != 1
        || !input["orientation"].is_object()
    {
        return None;
    }
    Some(
        json!({"version":1,"provider_id":provider,"conversation_id":input["conversation"]["channel"],
        "thread_id":input["conversation"]["thread"].as_str().filter(|s|!s.is_empty())}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> Value {
        json!({"history":[{"channel":"room","ts":"1.0","revision":"2.0","sender":"person","text":"full original message","deleted":false}],
          "orientation":{"thread":{"messages":[{"conversation_id":"room","message_id":"1.0","revision":"2.0","actor_id":"person","text":"full original","truncated":true,"url":"https://example.test/source"}]}}})
    }
    #[test]
    fn full_history_is_preserved_and_excerpt_references_it() {
        let mut v = input();
        let history = v["history"].clone();
        compact(&mut v);
        assert_eq!(v["history"], history);
        let m = &v["orientation"]["thread"]["messages"][0];
        assert!(m.get("text").is_none());
        assert_eq!(m["text_in_history"]["revision"], "2.0");
        assert_eq!(m["url"], "https://example.test/source");
        assert_eq!(m["truncated"], true);
    }
    #[test]
    fn deleted_or_unversioned_history_does_not_replace_source_text() {
        for (key, value) in [("deleted", json!(true)), ("revision", Value::Null)] {
            let mut v = input();
            v["history"][0][key] = value.clone();
            if key == "revision" {
                v["orientation"]["thread"]["messages"][0][key] = value;
            }
            compact(&mut v);
            assert!(v["orientation"]["thread"]["messages"][0]["text"].is_string());
        }
    }
    #[test]
    fn projection_requires_negotiation_and_existing_orientation() {
        let mut v = input();
        v["conversation"] = json!({"channel":"room","thread":""});
        let mut init = json!({});
        assert!(metadata(&v, "slack:fixture", &init).is_none());
        init["_meta"]["io.github.bkrabach.agent-instance"]["continuity"]["connectorProjectionVersion"] =
            json!(1);
        assert_eq!(
            metadata(&v, "slack:fixture", &init).unwrap(),
            json!({"version":1,"provider_id":"slack:fixture","conversation_id":"room","thread_id":null})
        );
        v["orientation"] = Value::Null;
        assert!(metadata(&v, "slack:fixture", &init).is_none());
    }
    #[test]
    fn changed_or_unproved_sources_are_never_elided() {
        for (key, value) in [
            ("revision", json!("3.0")),
            ("revision", Value::Null),
            ("conversation_id", json!("elsewhere")),
            ("actor_id", json!("someone-else")),
            ("text", json!("changed text")),
            ("truncated", json!(false)),
        ] {
            let mut v = input();
            v["orientation"]["thread"]["messages"][0][key] = value;
            compact(&mut v);
            assert!(
                v["orientation"]["thread"]["messages"][0]["text"].is_string(),
                "{key}"
            );
        }
    }
}
