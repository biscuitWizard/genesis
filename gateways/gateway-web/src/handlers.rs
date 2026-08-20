//! One handler per client action.
//!
//! Adding a capability to the chat surface means adding a function here and a
//! line to the table in `dispatch` — nothing else in the gateway changes.

use crate::genesis::harness::session as host;
use crate::genesis::harness::sys;
use crate::genesis::harness::types::Attachment;
use crate::render;
use crate::{GatewayAction, OutboundEvent};
use serde_json::{json, Value};

pub fn reply(value: Value) -> GatewayAction {
    GatewayAction::Reply(value.to_string())
}

pub fn error(message: impl AsRef<str>) -> GatewayAction {
    reply(json!({ "type": "error", "message": message.as_ref() }))
}

/// Routes an inbound frame to its handler.
pub fn dispatch(frame: &Value) -> Vec<GatewayAction> {
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let id = frame.get("id").and_then(Value::as_str);
    let previous = frame.get("previous").and_then(Value::as_str);

    match kind {
        "hello" => vec![catalog(), sessions()],
        "list" => vec![sessions()],
        "catalog" => vec![catalog()],

        "open" => match id {
            Some(session) => open(session, previous),
            None => vec![error("open requires an id")],
        },

        "new" => new_session(frame, previous),
        "send" => match id {
            Some(session) => send(session, frame),
            None => vec![error("send requires an id")],
        },

        "rename" => match (id, frame.get("title").and_then(Value::as_str)) {
            (Some(session), Some(title)) if !title.trim().is_empty() => {
                host::rename_session(session, title.trim());
                vec![sessions()]
            }
            _ => vec![error("rename requires an id and a title")],
        },

        "archive" => match id {
            Some(session) => vec![
                {
                    host::archive_session(session, true);
                    GatewayAction::Unsubscribe(session.to_string())
                },
                sessions(),
            ],
            None => vec![error("archive requires an id")],
        },

        "set-mode" => match (id, frame.get("mode").and_then(Value::as_str)) {
            (Some(session), Some(mode)) => {
                host::set_session_mode(session, mode);
                vec![session_settings(session), sessions()]
            }
            _ => vec![error("set-mode requires an id and a mode")],
        },

        "skills" => match id {
            Some(session) => vec![skills(session)],
            None => vec![error("skills requires an id")],
        },

        "set-skill" => match (
            id,
            frame.get("skill").and_then(Value::as_str),
            frame.get("enabled").and_then(Value::as_bool),
        ) {
            (Some(session), Some(skill), Some(enabled)) => {
                host::set_skill_enabled(session, skill, enabled);
                vec![skills(session)]
            }
            _ => vec![error("set-skill requires an id, a skill and enabled")],
        },

        "tools" => match id {
            Some(session) => vec![tools(session)],
            None => vec![error("tools requires an id")],
        },

        "set-model" => match (id, frame.get("model").and_then(Value::as_str)) {
            (Some(session), Some(model)) => {
                host::set_session_model(session, model);
                vec![session_settings(session), sessions()]
            }
            _ => vec![error("set-model requires an id and a model")],
        },

        other => vec![error(format!("unknown frame type: {other}"))],
    }
}

// --- frames -----------------------------------------------------------------

/// What the pickers offer. Sent once on connect.
pub fn catalog() -> GatewayAction {
    reply(json!({
        "type": "catalog",
        "models": sys::list_models().iter().map(|m| json!({
            "id": m.id, "label": m.label,
        })).collect::<Vec<_>>(),
        "modes": sys::list_modes().iter().map(|m| json!({
            "id": m.id, "label": m.label, "description": m.description,
        })).collect::<Vec<_>>(),
    }))
}

pub fn sessions() -> GatewayAction {
    reply(json!({
        "type": "sessions",
        "sessions": host::list_sessions(false),
    }))
}

/// The skills on offer, each with whether it is attached to this conversation.
fn skills(session_id: &str) -> GatewayAction {
    reply(json!({
        "type": "skills",
        "session": session_id,
        "skills": host::list_skills(session_id).iter().map(|s| json!({
            "id": s.id,
            "name": s.name,
            "description": s.description,
            "instructions": s.instructions,
            "enabled": s.enabled,
        })).collect::<Vec<_>>(),
    }))
}

/// Exactly the tools the agent would offer for this conversation's mode.
fn tools(session_id: &str) -> GatewayAction {
    reply(json!({
        "type": "tools",
        "session": session_id,
        "tools": host::available_tools(session_id).iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "schema": t.args_schema_json,
            "capabilities": t.capabilities,
        })).collect::<Vec<_>>(),
    }))
}

fn session_settings(session_id: &str) -> GatewayAction {
    let meta = host::get_session(session_id);
    reply(json!({
        "type": "settings",
        "session": session_id,
        "mode": meta.as_ref().map(|m| m.mode.clone()).unwrap_or_default(),
        "model": meta.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
    }))
}

fn history(session_id: &str) -> GatewayAction {
    let meta = host::get_session(session_id);
    let events: Vec<Value> = host::events(session_id, 0)
        .iter()
        .filter_map(|record| {
            render::event(&OutboundEvent {
                session_id: session_id.to_string(),
                seq: Some(record.seq),
                ts_ms: record.ts_ms,
                event: record.event.clone(),
            })
        })
        .collect();

    reply(json!({
        "type": "history",
        "session": session_id,
        "title": meta.as_ref().map(|m| m.title.clone()).unwrap_or_default(),
        "mode": meta.as_ref().map(|m| m.mode.clone()).unwrap_or_default(),
        "model": meta.as_ref().map(|m| m.model.clone()).unwrap_or_default(),
        "events": events,
    }))
}

// --- actions ----------------------------------------------------------------

fn open(session_id: &str, previous: Option<&str>) -> Vec<GatewayAction> {
    let mut actions = Vec::new();
    if let Some(prev) = previous {
        if prev != session_id {
            actions.push(GatewayAction::Unsubscribe(prev.to_string()));
        }
    }
    actions.push(GatewayAction::Subscribe(session_id.to_string()));
    actions.push(history(session_id));
    actions
}

fn new_session(frame: &Value, previous: Option<&str>) -> Vec<GatewayAction> {
    let title = frame
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = host::create_session(title.as_deref());

    let mut actions = vec![sessions()];
    actions.extend(open(&id, previous));
    actions.push(reply(json!({ "type": "opened", "session": id })));
    actions
}

fn send(session_id: &str, frame: &Value) -> Vec<GatewayAction> {
    let text = frame
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let attachments = parse_attachments(frame);

    // An empty message with no files is a stray keypress, not a turn.
    if text.is_empty() && attachments.is_empty() {
        return Vec::new();
    }

    host::submit(session_id, &text, &attachments);
    // Nothing is echoed back: the message returns through the event stream, so
    // the transcript stays a pure function of the log.
    Vec::new()
}

fn parse_attachments(frame: &Value) -> Vec<Attachment> {
    frame
        .get("attachments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Attachment {
                        name: item.get("name").and_then(Value::as_str)?.to_string(),
                        mime: item.get("mime").and_then(Value::as_str)?.to_string(),
                        data_base64: item.get("data").and_then(Value::as_str)?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
