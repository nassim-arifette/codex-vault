//! The reverse walk is bounded, so the scan retains only a window of records instead of the
//! whole file. These tests hold that change to two promises:
//!
//! 1. for any transcript that fits in the window, the verdict is *identical* to retaining
//!    everything — the reference implementation is the same code run with `usize::MAX`;
//! 2. when the window is exhausted the analysis refuses to compact and says *why*, rather than
//!    reporting the same "no cutoff" it would give for a transcript that genuinely has none.

use codex_vault::analysis::{analyze_session_within, CompactionAnalysis};
use codex_vault::rollout::{scan_rollout_metadata_within, DEFAULT_SCAN_WINDOW};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const UNBOUNDED: usize = usize::MAX;

fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn session_meta() -> Value {
    json!({"type":"session_meta","payload":{"id":"sess-w","cwd":"C:/work/w"}})
}

fn compacted(window: u64) -> Value {
    json!({"type":"compacted",
           "payload":{"replacement_history":[{"role":"user"}],"window_number":window}})
}

fn completed_turn(turn: &str) -> Vec<Value> {
    vec![
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":turn}}),
        json!({"type":"turn_context","payload":{"turn_id":turn,"model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"hi"}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":turn}}),
    ]
}

/// Padding that is *retained* by the scan yet inert to the walk.
///
/// A raw `response_item` would not do: the scan already discards those, which is why the bulk of
/// a transcript never reaches the window in the first place. A non-full `world_state` is kept as
/// reconstruction-relevant but leaves every accumulator untouched, so it exercises the window
/// without perturbing the verdict.
fn filler(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| json!({"type":"world_state","payload":{"full":false,"state":{},"i":i}}))
        .collect()
}

/// Compare everything the analysis reports, minus the window bookkeeping that is expected to
/// differ between a bounded and an unbounded run.
fn assert_same_verdict(bounded: &CompactionAnalysis, reference: &CompactionAnalysis) {
    assert_eq!(bounded.can_compact, reference.can_compact, "can_compact");
    assert_eq!(bounded.cutoff_index, reference.cutoff_index, "cutoff_index");
    assert_eq!(
        bounded.checkpoint_index, reference.checkpoint_index,
        "checkpoint_index"
    );
    assert_eq!(
        bounded.session_meta_index, reference.session_meta_index,
        "session_meta_index"
    );
    assert_eq!(bounded.reasons, reference.reasons, "reasons");
    assert_eq!(bounded.total_lines, reference.total_lines);
    assert_eq!(bounded.parsed_lines, reference.parsed_lines);
    assert_eq!(bounded.malformed_lines, reference.malformed_lines);
    assert_eq!(
        bounded.valid_checkpoint_count,
        reference.valid_checkpoint_count
    );
    assert_eq!(
        bounded.invalid_checkpoint_count,
        reference.invalid_checkpoint_count
    );
    assert_eq!(bounded.window_number, reference.window_number);
    assert_eq!(
        bounded.replacement_history_items_at_checkpoint,
        reference.replacement_history_items_at_checkpoint
    );
    assert_eq!(bounded.original_size_bytes, reference.original_size_bytes);
    assert_eq!(
        bounded.estimated_result_size_bytes,
        reference.estimated_result_size_bytes
    );
    assert_eq!(
        bounded.estimated_removed_bytes,
        reference.estimated_removed_bytes
    );
    assert_eq!(
        bounded.removable_checkpoint_bytes, reference.removable_checkpoint_bytes,
        "removable_checkpoint_bytes"
    );
}

/// Every shape the analysis has an opinion about, so the differential covers the branches and
/// not just the happy path.
fn corpus() -> Vec<(&'static str, Vec<Value>)> {
    let mut out: Vec<(&str, Vec<Value>)> = Vec::new();

    let mut proven = vec![session_meta()];
    proven.extend(filler(40));
    proven.push(compacted(3));
    proven.extend(completed_turn("t1"));
    out.push(("cutoff near the end", proven));

    let mut deep = vec![session_meta(), compacted(4)];
    deep.extend(completed_turn("t1"));
    deep.extend(filler(400));
    out.push(("cutoff far from the end", deep));

    let mut rolled_back = vec![session_meta(), compacted(1)];
    rolled_back.extend(completed_turn("t1"));
    rolled_back.push(json!({"type":"event_msg",
                            "payload":{"type":"thread_rolled_back","num_turns":1}}));
    out.push(("rollback in the suffix", rolled_back));

    let mut invalid = vec![session_meta()];
    invalid.push(json!({"type":"compacted","payload":{"replacement_history":[{"r":1}]}}));
    invalid.extend(completed_turn("t1"));
    out.push(("compaction without window_number", invalid));

    let mut unknown = vec![session_meta(), compacted(2)];
    unknown.extend(completed_turn("t1"));
    for i in 0..40 {
        unknown.push(json!({"type": format!("future_record_{}", i % 3), "payload":{"i":i}}));
    }
    out.push(("many unknown record types", unknown));

    let mut several = vec![session_meta()];
    for w in 1..=5u64 {
        several.push(compacted(w));
        several.extend(completed_turn(&format!("t{w}")));
        several.extend(filler(20));
    }
    several.push(compacted(6));
    several.extend(completed_turn("last"));
    out.push(("several compaction checkpoints", several));

    out.push(("no compaction at all", {
        let mut v = vec![session_meta()];
        v.extend(completed_turn("t1"));
        v.extend(filler(50));
        v
    }));

    out
}

#[test]
fn a_bounded_window_gives_the_same_verdict_as_retaining_everything() {
    let dir = TempDir::new().unwrap();
    for (label, lines) in corpus() {
        let path = dir.path().join("rollout.jsonl");
        write_jsonl(&path, &lines);

        let reference = analyze_session_within(&path, UNBOUNDED).unwrap();
        assert!(
            !reference.scan_window_truncated,
            "{label}: the reference run must retain everything"
        );

        // A window comfortably larger than the file, and the shipped default.
        for window in [lines.len() * 4, DEFAULT_SCAN_WINDOW] {
            let bounded = analyze_session_within(&path, window).unwrap();
            assert!(
                !bounded.scan_window_truncated,
                "{label}: window {window} should not have truncated"
            );
            assert_same_verdict(&bounded, &reference);
        }
    }
}

#[test]
fn a_window_that_still_covers_the_proof_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rollout.jsonl");

    // The proof needs the trailing compaction plus one completed turn. Records before that are
    // irrelevant to it, so dropping them must not change the verdict.
    let mut lines = vec![session_meta()];
    lines.extend(filler(500));
    lines.push(compacted(9));
    lines.extend(completed_turn("t1"));
    write_jsonl(&path, &lines);

    let reference = analyze_session_within(&path, UNBOUNDED).unwrap();
    assert!(reference.can_compact, "{:?}", reference.reasons);

    let bounded = analyze_session_within(&path, 8).unwrap();
    assert!(
        bounded.scan_window_truncated,
        "the window should have truncated"
    );
    assert_eq!(bounded.retained_records, 8);
    assert_same_verdict(&bounded, &reference);
}

#[test]
fn exhausting_the_window_refuses_to_compact_and_says_so() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rollout.jsonl");

    // The only checkpoint sits behind more relevant records than the window can hold.
    let mut lines = vec![session_meta(), compacted(1)];
    lines.extend(completed_turn("t1"));
    lines.extend(filler(200));
    write_jsonl(&path, &lines);

    let reference = analyze_session_within(&path, UNBOUNDED).unwrap();
    assert!(reference.can_compact, "{:?}", reference.reasons);

    let starved = analyze_session_within(&path, 5).unwrap();
    assert!(!starved.can_compact, "a starved scan must never compact");
    assert!(starved.scan_window_truncated);
    assert!(
        starved
            .reasons
            .iter()
            .any(|r| r.contains("scan window was exhausted")),
        "the refusal must name the window rather than imply no cutoff exists: {:?}",
        starved.reasons
    );
    assert!(
        starved
            .reasons
            .iter()
            .all(|r| !r.contains("need both a valid compaction checkpoint")),
        "the two situations must not report the same reason: {:?}",
        starved.reasons
    );
}

#[test]
fn retention_is_bounded_by_the_window_not_by_the_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rollout.jsonl");

    let mut lines = vec![session_meta()];
    lines.extend(filler(20_000));
    lines.push(compacted(1));
    lines.extend(completed_turn("t1"));
    write_jsonl(&path, &lines);

    let scan = scan_rollout_metadata_within(&path, 64).unwrap();
    assert_eq!(scan.window.len(), 64, "retention must stop at the window");
    assert!(scan.window_truncated);
    assert_eq!(scan.total_lines, lines.len());

    // Whole-file facts survive the truncation, because they are tracked as aggregates rather
    // than by keeping every record.
    assert_eq!(scan.compactions.len(), 1);
    assert_eq!(scan.unknown.count, 0);
    assert!(scan.session_meta.is_some());

    let unbounded = scan_rollout_metadata_within(&path, UNBOUNDED).unwrap();
    assert_eq!(
        unbounded.window.len(),
        unbounded.total_lines,
        "every record in this fixture is reconstruction-relevant"
    );
    assert!(unbounded.window.len() > 20_000);
    assert!(!unbounded.window_truncated);
    assert_eq!(unbounded.content_sha256, scan.content_sha256);
}

#[test]
fn unknown_record_types_are_summarised_not_listed_one_by_one() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rollout.jsonl");

    let mut lines = vec![session_meta()];
    for i in 0..5_000 {
        lines.push(json!({"type": format!("future_record_{}", i % 3), "payload":{"i":i}}));
    }
    write_jsonl(&path, &lines);

    let analysis = analyze_session_within(&path, DEFAULT_SCAN_WINDOW).unwrap();
    assert!(!analysis.can_compact);
    assert!(
        analysis.reasons.len() <= 20,
        "one reason per unknown line would be 5000: {}",
        analysis.reasons.len()
    );
    assert!(analysis
        .reasons
        .iter()
        .any(|r| r.contains("further unknown rollout item record(s)")
            && r.contains("3 distinct type(s)")));
}
