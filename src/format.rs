//! Parsing of the Codex rollout envelope into semantic record kinds.

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub enum RecordKind {
    SessionMeta,
    Compacted {
        replacement_present: bool,
        replacement_items: Option<usize>,
        window_number: Option<u64>,
    },
    TurnContext {
        turn_id: Option<String>,
    },
    Event(EventKind),
    ResponseItem {
        counts_as_user_turn: bool,
    },
    InterAgentCommunication,
    WorldState {
        full: bool,
    },
    UnknownOuter {
        tag: String,
    },
    Other,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    ThreadRolledBack,
    ItemCompleted {
        turn_id: Option<String>,
        is_user_message: bool,
    },
    TurnComplete {
        turn_id: Option<String>,
    },
    TurnAborted {
        turn_id: Option<String>,
    },
    TurnStarted {
        turn_id: Option<String>,
    },
    UserMessage,
    Other,
}

pub fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    Some(cursor)
}

pub fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    value_at(value, path)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub fn u64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

pub fn record_payload(value: &Value) -> &Value {
    value.get("payload").unwrap_or(value)
}

pub fn outer_type(value: &Value) -> &str {
    value.get("type").and_then(Value::as_str).unwrap_or("")
}

pub fn normalize_tag(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn extract_session_id(value: &Value) -> Option<String> {
    let payload = record_payload(value);
    string_at(payload, &["meta", "id"])
        .or_else(|| string_at(payload, &["meta", "session_id"]))
        .or_else(|| string_at(payload, &["id"]))
        .or_else(|| string_at(payload, &["session_id"]))
        .or_else(|| string_at(value, &["session_id"]))
}

/// Where a paginated thread's history continues from.
///
/// Codex splits a long thread across rollout files. Each page after the first records the page it
/// continues from, together with a **byte offset** into that file. Shortening a page that another
/// one continues from therefore invalidates the chain: Codex then refuses to resume the thread
/// with "invalid paginated history lineage: cutoff byte offset is past the source rollout".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HistoryBase {
    /// The page this one continues from, named by that page's own id.
    pub thread_id: Option<String>,
    pub end_ordinal_exclusive: Option<u64>,
    pub end_byte_offset: Option<u64>,
}

/// Provenance a `session_meta` record carries about the Codex build that wrote the transcript.
///
/// This is strictly better than asking the installed `codex` for its version: it describes the
/// build that actually produced *this* file, which may be months older than what is on PATH now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionProvenance {
    /// `payload.cli_version`, e.g. "0.150.0-alpha.12.2".
    pub cli_version: Option<String>,
    /// `payload.originator`, e.g. "Codex Desktop".
    pub originator: Option<String>,
    /// `payload.source`, e.g. "vscode".
    pub source: Option<String>,
    /// `payload.history_mode`, e.g. "paginated" — a reconstruction-relevant signal.
    pub history_mode: Option<String>,
    /// `payload.context_window.window_id`, which the `window_number` of a compaction refers to.
    pub context_window_id: Option<String>,
    /// `payload.thread_source`: `user` for a session a person started, `subagent` for one Codex
    /// spawned. A sub-agent rollout cannot be resumed on its own — Codex refuses with
    /// "cannot resume an unloaded multi-agent v2 sub-agent through its parent".
    pub thread_source: Option<String>,
    /// `payload.parent_thread_id`, set on sub-agent rollouts.
    pub parent_thread_id: Option<String>,
    /// `payload.history_base`, present on every page of a paginated thread except the first.
    pub history_base: Option<HistoryBase>,
}

impl SessionProvenance {
    /// True when this rollout belongs to a thread Codex spawned (`subagent`,
    /// `guardian_review`, ...) rather than to a user thread.
    ///
    /// Worth knowing beyond curiosity: on these rollouts `payload.session_id` names the *parent*
    /// thread while `payload.id` names this one, so anything keyed on the wrong field would
    /// collide a sub-agent with its parent.
    pub fn is_spawned_thread(&self) -> bool {
        self.parent_thread_id.is_some()
            || matches!(
                self.thread_source.as_deref(),
                Some("subagent") | Some("guardian_review")
            )
    }
}

pub fn extract_provenance(value: &Value) -> SessionProvenance {
    let payload = record_payload(value);
    let pick = |keys: &[&[&str]]| keys.iter().find_map(|k| string_at(payload, k));
    SessionProvenance {
        cli_version: pick(&[&["cli_version"], &["meta", "cli_version"], &["version"]]),
        originator: pick(&[&["originator"], &["meta", "originator"]]),
        source: pick(&[&["source"], &["meta", "source"]]),
        history_mode: pick(&[&["history_mode"], &["meta", "history_mode"]]),
        context_window_id: pick(&[
            &["context_window", "window_id"],
            &["meta", "context_window", "window_id"],
        ]),
        thread_source: pick(&[&["thread_source"], &["meta", "thread_source"]]),
        parent_thread_id: pick(&[&["parent_thread_id"], &["meta", "parent_thread_id"]]),
        history_base: value_at(payload, &["history_base"])
            .or_else(|| value_at(payload, &["meta", "history_base"]))
            .filter(|v| v.is_object())
            .map(|v| HistoryBase {
                thread_id: string_at(v, &["thread_id"]),
                end_ordinal_exclusive: v.get("end_ordinal_exclusive").and_then(u64_value),
                end_byte_offset: v.get("end_byte_offset").and_then(u64_value),
            }),
    }
}

pub fn extract_cwd_hint(value: &Value) -> Option<String> {
    let payload = record_payload(value);
    string_at(payload, &["meta", "cwd"])
        .or_else(|| string_at(payload, &["cwd"]))
        .or_else(|| string_at(payload, &["working_directory"]))
        .or_else(|| string_at(payload, &["working_dir"]))
        .or_else(|| string_at(payload, &["project_root"]))
        .or_else(|| string_at(payload, &["repo_root"]))
}

pub fn looks_like_inter_agent_message_content(content: &[Value]) -> bool {
    // Codex's InterAgentCommunication::from_message_content accepts exactly one input/output
    // text item whose text is JSON deserializable as InterAgentCommunication. Stay conservative:
    // require the required fields rather than treating arbitrary assistant text as a boundary.
    if content.len() != 1 {
        return false;
    }
    let item = &content[0];
    let content_type = normalize_tag(item.get("type").and_then(Value::as_str).unwrap_or(""));
    if content_type != "inputtext" && content_type != "outputtext" {
        return false;
    }
    let Some(text) = item.get("text").and_then(Value::as_str) else {
        return false;
    };
    let Ok(decoded) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(obj) = decoded.as_object() else {
        return false;
    };
    obj.get("author").and_then(Value::as_str).is_some()
        && obj.get("recipient").and_then(Value::as_str).is_some()
        && obj.get("content").and_then(Value::as_str).is_some()
        && obj.get("trigger_turn").and_then(Value::as_bool).is_some()
}

pub fn response_item_counts_as_user_turn(payload: &Value) -> bool {
    // Codex's in-memory history uses ResponseItemEnvelope, while the current rollout wire
    // format persists the raw ResponseItem as `payload`. Accept an item-wrapped shape too for
    // compatibility with older/synthetic transcripts.
    let response_item = payload
        .get("item")
        .filter(|item| item.is_object())
        .unwrap_or(payload);
    let inner_type = normalize_tag(
        response_item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if inner_type == "agentmessage" {
        return true;
    }

    if inner_type == "message"
        && response_item.get("role").and_then(Value::as_str) == Some("assistant")
    {
        if let Some(content) = response_item.get("content").and_then(Value::as_array) {
            return looks_like_inter_agent_message_content(content);
        }
    }
    false
}

pub fn parse_record(value: &Value) -> RecordKind {
    let outer = normalize_tag(outer_type(value));
    let payload = record_payload(value);

    match outer.as_str() {
        "sessionmeta" => RecordKind::SessionMeta,
        "compacted" | "sessioncompacted" => {
            let replacement = payload.get("replacement_history");
            let replacement_present = replacement.is_some_and(|v| !v.is_null());
            let replacement_items = replacement.and_then(Value::as_array).map(Vec::len);
            let window_number = payload.get("window_number").and_then(u64_value);
            RecordKind::Compacted {
                replacement_present,
                replacement_items,
                window_number,
            }
        }
        "turncontext" => RecordKind::TurnContext {
            turn_id: string_at(payload, &["turn_id"]),
        },
        "eventmsg" => {
            let event_type =
                normalize_tag(payload.get("type").and_then(Value::as_str).unwrap_or(""));
            let turn_id = string_at(payload, &["turn_id"]);
            let event = match event_type.as_str() {
                "threadrolledback" | "threadrollback" => EventKind::ThreadRolledBack,
                "itemcompleted" => {
                    let item_type = value_at(payload, &["item", "type"])
                        .and_then(Value::as_str)
                        .or_else(|| {
                            value_at(payload, &["item", "item", "type"]).and_then(Value::as_str)
                        })
                        .unwrap_or("");
                    EventKind::ItemCompleted {
                        turn_id,
                        is_user_message: normalize_tag(item_type) == "usermessage",
                    }
                }
                "turncomplete" | "taskcomplete" => EventKind::TurnComplete { turn_id },
                "turnaborted" | "taskaborted" => EventKind::TurnAborted { turn_id },
                "turnstarted" | "taskstarted" => EventKind::TurnStarted { turn_id },
                "usermessage" => EventKind::UserMessage,
                _ => EventKind::Other,
            };
            RecordKind::Event(event)
        }
        "responseitem" => RecordKind::ResponseItem {
            counts_as_user_turn: response_item_counts_as_user_turn(payload),
        },
        "interagentcommunication" => RecordKind::InterAgentCommunication,
        "worldstate" => RecordKind::WorldState {
            full: payload
                .get("full")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        // Current RolloutItemWire variants that the official bounded scanner ignores.
        "interagentcommunicationmetadata"
        | "realtimeitem"
        | "retainedcontext"
        | "securityriskscore"
        | "tokenusagerecord" => RecordKind::Other,
        _ => RecordKind::UnknownOuter {
            tag: outer_type(value).to_string(),
        },
    }
}
