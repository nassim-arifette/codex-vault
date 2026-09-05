//! Format-tolerance and cutoff-proof specifications, exercised through the public API.

use codex_vault::analysis::analyze_session;
use codex_vault::format::{parse_record, response_item_counts_as_user_turn, RecordKind};
use codex_vault::rollout::read_session_head;
use serde_json::{json, Value};
use std::io::Write;
use tempfile::NamedTempFile;

fn write_fixture(lines: &[Value]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    for line in lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
    file.flush().unwrap();
    file
}

fn session_meta() -> Value {
    json!({
        "timestamp":"2026-09-05T00:00:00Z",
        "type":"session_meta",
        "payload":{"id":"session-123","cwd":"C:\\work\\repo"}
    })
}

fn compacted(window: Option<u64>) -> Value {
    let mut payload = json!({"replacement_history":[{"role":"user"}]});
    if let Some(w) = window {
        payload["window_number"] = json!(w);
    }
    json!({"timestamp":"2026-09-05T00:00:01Z","type":"compacted","payload":payload})
}

fn completed_turn(turn: &str) -> Vec<Value> {
    vec![
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":turn}}),
        json!({"type":"turn_context","payload":{"turn_id":turn,"model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":turn}}),
    ]
}

#[test]
fn parses_current_flattened_session_meta_identity() {
    let file = write_fixture(&[session_meta()]);
    let head = read_session_head(file.path()).unwrap();
    let (id, cwd) = (head.session_id, head.cwd_hint);
    assert_eq!(id, "session-123");
    assert_eq!(cwd.as_deref(), Some("C:\\work\\repo"));
}

#[test]
fn parses_nested_session_meta_compatibility_shape() {
    let nested = json!({
        "type":"session_meta",
        "payload":{"meta":{"id":"session-nested","cwd":"C:\\old\\repo"}}
    });
    let file = write_fixture(&[nested]);
    let head = read_session_head(file.path()).unwrap();
    let (id, cwd) = (head.session_id, head.cwd_hint);
    assert_eq!(id, "session-nested");
    assert_eq!(cwd.as_deref(), Some("C:\\old\\repo"));
}

#[test]
fn proves_cutoff_with_valid_checkpoint_and_turn_context() {
    let mut lines = vec![
        session_meta(),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[]}}),
        compacted(Some(1)),
    ];
    lines.extend(completed_turn("t1"));
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(analysis.can_compact, "{:?}", analysis.reasons);
    assert_eq!(analysis.checkpoint_index, Some(2));
    assert_eq!(analysis.cutoff_index, Some(2));
    assert!(analysis.estimated_removed_bytes.unwrap_or(0) > 0);
}

#[test]
fn legacy_compaction_without_window_disables_cutoff() {
    let mut lines = vec![session_meta(), compacted(None)];
    lines.extend(completed_turn("t1"));
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(!analysis.can_compact);
    assert!(analysis.reasons.iter().any(|r| r.contains("window_number")));
}

#[test]
fn rollback_in_required_suffix_disables_cutoff() {
    let mut lines = vec![session_meta(), compacted(Some(1))];
    lines.extend(completed_turn("t1"));
    lines.push(json!({
        "type":"event_msg",
        "payload":{"type":"thread_rolled_back","num_turns":1}
    }));
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(!analysis.can_compact);
    assert!(analysis.reasons.iter().any(|r| r.contains("rollback")));
}

#[test]
fn outer_envelope_compaction_is_detected() {
    let kind = parse_record(&compacted(Some(7)));
    match kind {
        RecordKind::Compacted {
            replacement_present,
            replacement_items,
            window_number,
        } => {
            assert!(replacement_present);
            assert_eq!(replacement_items, Some(1));
            assert_eq!(window_number, Some(7));
        }
        _ => panic!("wrong record kind"),
    }
}
#[test]
fn accepts_v1_task_aliases_and_pascal_case_user_item() {
    let mut lines = vec![session_meta(), compacted(Some(3))];
    lines.extend(vec![
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"legacy"}}),
        json!({"type":"turn_context","payload":{"turn_id":"legacy","model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"legacy","item":{"type":"UserMessage","content":[]}}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"legacy"}}),
    ]);
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(analysis.can_compact, "{:?}", analysis.reasons);
    assert_eq!(analysis.checkpoint_index, Some(1));
    assert_eq!(analysis.cutoff_index, Some(1));
}

#[test]
fn wrapped_response_item_envelope_is_supported() {
    let lines = vec![
        session_meta(),
        compacted(Some(1)),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t"}}),
        json!({"type":"turn_context","payload":{"turn_id":"t","model":"gpt"}}),
        json!({"type":"response_item","payload":{"item":{"type":"agent_message","author":"a","recipient":"b","content":[]},"metadata":{"source":"fixture"}}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t"}}),
    ];
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(analysis.can_compact, "{:?}", analysis.reasons);
}

#[test]
fn assistant_inter_agent_json_message_counts_as_boundary() {
    let encoded = serde_json::to_string(&json!({
        "author":"agent/a",
        "recipient":"agent/b",
        "content":"handoff",
        "trigger_turn":true
    }))
    .unwrap();
    let item = json!({
        "type":"message",
        "role":"assistant",
        "content":[{"type":"output_text","text":encoded}]
    });
    assert!(response_item_counts_as_user_turn(&item));
}

#[test]
fn unknown_outer_rollout_type_disables_compaction() {
    let mut lines = vec![session_meta(), compacted(Some(1))];
    lines.extend(completed_turn("t1"));
    lines.push(json!({"type":"future_semantic_record","payload":{"x":1}}));
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(!analysis.can_compact);
    assert!(analysis
        .reasons
        .iter()
        .any(|r| r.contains("unknown rollout item type")));
}

#[test]
fn full_world_state_newer_than_compaction_can_establish_baseline() {
    let lines = vec![
        session_meta(),
        compacted(Some(1)),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t"}}),
        json!({"type":"turn_context","payload":{"turn_id":"t","model":"gpt"}}),
        json!({"type":"world_state","payload":{"full":true,"state":{}}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t"}}),
    ];
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(analysis.can_compact, "{:?}", analysis.reasons);
}

#[test]
fn full_world_state_older_than_turn_compaction_is_not_a_baseline() {
    let lines = vec![
        session_meta(),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t"}}),
        json!({"type":"turn_context","payload":{"turn_id":"t","model":"gpt"}}),
        json!({"type":"world_state","payload":{"full":true,"state":{}}}),
        compacted(Some(1)),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t"}}),
    ];
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(!analysis.can_compact);
}

#[test]
fn raw_role_user_response_item_is_not_a_turn_boundary() {
    let lines = vec![
        session_meta(),
        compacted(Some(1)),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t"}}),
        json!({"type":"turn_context","payload":{"turn_id":"t","model":"gpt"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[]}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t"}}),
    ];
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(!analysis.can_compact);
}

#[test]
fn agent_message_response_item_can_establish_turn_boundary() {
    let lines = vec![
        session_meta(),
        compacted(Some(1)),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t"}}),
        json!({"type":"turn_context","payload":{"turn_id":"t","model":"gpt"}}),
        json!({"type":"response_item","payload":{"type":"agent_message","author":"a","recipient":"b","content":[]}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t"}}),
    ];
    let file = write_fixture(&lines);
    let analysis = analyze_session(file.path()).unwrap();
    assert!(analysis.can_compact, "{:?}", analysis.reasons);
}
