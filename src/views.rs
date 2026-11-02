use crate::{
    auth,
    error::{ApiError, ApiResult},
    state::{AppState, now_ms},
};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

pub async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "ui", false)?;
    Ok(Json(json!({"items":state.list_records("views")?})))
}
pub async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "ui", true)?.actor;
    let label = body["label"].as_str().unwrap_or("Browser");
    if label.len() > 200 || sensitive(&json!(label)) {
        return Err(ApiError::bad("Invalid view label"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let v = json!({"id":id,"label":label,"revision":1,"updated_at":now_ms(),"actor":actor,"ack":null,"state":{"route":if state.instance_config().setup_complete{"dashboard"}else{"setup"},"step":0,"draft":{},"filters":{},"selected_conversation":null,"focus":null,"scroll":{"x":0,"y":0},"modal":null}});
    state.set_record("views", &id, v.clone())?;
    Ok(Json(v))
}
pub async fn get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::check(&state, &headers, "ui", false)?;
    Ok(Json(
        state
            .get_record("views", &id)?
            .ok_or_else(ApiError::missing)?,
    ))
}
fn sensitive(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, v)| {
            [
                "password",
                "token",
                "app_token",
                "bot_token",
                "secret",
                "secret_value",
                "authorization",
                "csrf",
                "control-token",
            ]
            .contains(&k.to_lowercase().as_str())
                || sensitive(v)
        }),
        Value::Array(a) => a.iter().any(sensitive),
        Value::String(s) => crate::config::looks_secret(s) || s.starts_with("Bearer "),
        _ => false,
    }
}
fn valid(v: &Value) -> ApiResult<()> {
    if !v.is_object()
        || ![
            "setup",
            "dashboard",
            "configuration",
            "conversations",
            "activity",
            "access",
        ]
        .contains(&v["route"].as_str().unwrap_or(""))
    {
        return Err(ApiError::bad("Unknown view route"));
    }
    if v.to_string().len() > 256 * 1024
        || !v["draft"].is_object()
        || !v["filters"].is_object()
        || v["step"].as_u64().is_none_or(|s| s > 20)
    {
        return Err(ApiError::bad("Invalid view state"));
    }
    if sensitive(v) {
        return Err(ApiError::bad("Credentials must stay out of view state"));
    }
    Ok(())
}
fn set_path(root: &mut Value, path: &str, value: Value) -> ApiResult<()> {
    let p = path.split('.').collect::<Vec<_>>();
    if p.len() > 12
        || p.iter()
            .any(|s| s.is_empty() || ["__proto__", "prototype", "constructor"].contains(s))
    {
        return Err(ApiError::bad("Invalid draft path"));
    }
    let mut c = root;
    for key in &p[..p.len() - 1] {
        if !c.is_object() {
            *c = json!({});
        }
        c = c
            .as_object_mut()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| json!({}));
    }
    if !c.is_object() {
        *c = json!({});
    }
    c.as_object_mut()
        .unwrap()
        .insert(p.last().unwrap().to_string(), value);
    Ok(())
}
pub fn apply_command(state: &mut Value, command: &Value) -> ApiResult<()> {
    match command["type"].as_str().unwrap_or("") {
        "navigate" => {
            state["route"] = command["route"].clone();
            if command.get("step").is_some() {
                state["step"] = command["step"].clone();
            }
        }
        "set_draft" => set_path(
            &mut state["draft"],
            command["path"]
                .as_str()
                .ok_or_else(|| ApiError::bad("Draft path required"))?,
            command["value"].clone(),
        )?,
        "set_filter" => {
            let key = command["key"]
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() < 100
                        && !["__proto__", "prototype", "constructor"].contains(s)
                })
                .ok_or_else(|| ApiError::bad("Filter key required"))?;
            state["filters"][key] = command["value"].clone();
        }
        "select_conversation" => {
            state["selected_conversation"] = command["conversation_id"].clone()
        }
        "focus" => state["focus"] = command["target"].clone(),
        "scroll" => {
            state["scroll"] = json!({"x":command["x"].as_f64().unwrap_or(0.0),"y":command["y"].as_f64().unwrap_or(0.0)})
        }
        "open_modal" => state["modal"] = command["modal"].clone(),
        "close_modal" => state["modal"] = Value::Null,
        _ => return Err(ApiError::bad("Unknown view command")),
    };
    valid(state)
}
pub async fn commands(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "ui", true)?.actor;
    let expected = body["expected_revision"]
        .as_u64()
        .ok_or_else(|| ApiError::bad("expected_revision is required"))?;
    let commands = body["commands"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 100)
        .ok_or_else(|| ApiError::bad("Provide 1..100 commands"))?;
    let db = state.db.lock().unwrap();
    let text: Option<String> = db
        .query_row(
            "SELECT value FROM records WHERE collection='views' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    let mut v: Value = serde_json::from_str(&text.ok_or_else(ApiError::missing)?)
        .map_err(|_| ApiError::bad("Invalid stored view"))?;
    if v["revision"].as_u64() != Some(expected) {
        return Err(ApiError::conflict(json!({"revision":v["revision"]})));
    }
    for command in commands {
        apply_command(&mut v["state"], command)?;
    }
    v["revision"] = json!(expected + 1);
    v["updated_at"] = json!(now_ms());
    v["actor"] = json!(actor);
    v["ack"] = Value::Null;
    db.execute(
        "UPDATE records SET value=?1,updated_at=?2 WHERE collection='views' AND id=?3",
        params![v.to_string(), now_ms(), id],
    )?;
    Ok(Json(v))
}
pub async fn ack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::check(&state, &headers, "ui", true)?.actor;
    let status = body["status"].as_str().unwrap_or("applied");
    if !["applied", "error"].contains(&status) {
        return Err(ApiError::bad(
            "Acknowledgement status must be applied or error",
        ));
    }
    let db = state.db.lock().unwrap();
    let text: Option<String> = db
        .query_row(
            "SELECT value FROM records WHERE collection='views' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    let mut v: Value = serde_json::from_str(&text.ok_or_else(ApiError::missing)?)
        .map_err(|_| ApiError::bad("Invalid stored view"))?;
    if body["revision"].as_u64().is_none() || v["revision"] != body["revision"] {
        return Err(ApiError::conflict(json!({"revision":v["revision"]})));
    }
    v["ack"] = json!({"revision":body["revision"],"status":status,"at":now_ms(),"actor":actor});
    db.execute(
        "UPDATE records SET value=?1,updated_at=?2 WHERE collection='views' AND id=?3",
        params![v.to_string(), now_ms(), id],
    )?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> Value {
        json!({"route":"setup","step":0,"draft":{},"filters":{},"focus":null,"scroll":{"x":0,"y":0},"modal":null,"selected_conversation":null})
    }
    #[test]
    fn rejects_credentials_and_prototype_paths() {
        for command in [
            json!({"type":"set_draft","path":"bot_token","value":"something"}),
            json!({"type":"set_draft","path":"anything","value":"xoxb-private"}),
            json!({"type":"set_filter","key":"password","value":"hidden"}),
            json!({"type":"set_draft","path":"__proto__.pollute","value":true}),
        ] {
            assert!(apply_command(&mut view(), &command).is_err());
        }
    }
    #[test]
    fn semantic_navigation_and_selection() {
        let mut v = view();
        apply_command(&mut v, &json!({"type":"navigate","route":"conversations"})).unwrap();
        apply_command(
            &mut v,
            &json!({"type":"select_conversation","conversation_id":"C123"}),
        )
        .unwrap();
        assert_eq!(v["selected_conversation"], "C123");
        assert_eq!(v["route"], "conversations");
    }
}
