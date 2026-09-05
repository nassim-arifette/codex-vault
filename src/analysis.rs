//! The bounded-reconstruction analysis that decides whether a safe cutoff exists.

use crate::error::Result;
use crate::format::{EventKind, RecordKind};
use crate::rollout::{scan_rollout_metadata, scan_rollout_metadata_within, MetadataScan};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
pub struct CompactionAnalysis {
    pub can_compact: bool,
    /// SHA-256 of the transcript as the analysis read it. Callers reuse this instead of paying
    /// for another full pass, and compare it later to detect concurrent writes.
    #[serde(skip)]
    pub content_sha256: String,
    pub cutoff_index: Option<usize>,
    pub checkpoint_index: Option<usize>,
    pub session_meta_index: Option<usize>,
    pub reasons: Vec<String>,
    pub total_lines: usize,
    pub parsed_lines: usize,
    pub malformed_lines: usize,
    pub valid_checkpoint_count: usize,
    pub invalid_checkpoint_count: usize,
    pub replacement_history_items_at_checkpoint: Option<usize>,
    pub window_number: Option<u64>,
    /// How many records the reverse walk had available, and whether older ones were dropped.
    pub retained_records: usize,
    pub scan_window: usize,
    pub scan_window_truncated: bool,
    pub original_size_bytes: u64,
    pub estimated_result_size_bytes: Option<u64>,
    pub estimated_removed_bytes: Option<u64>,
    pub estimated_reduction_percent: Option<f64>,
    pub removable_checkpoint_bytes: u64,
}

#[derive(Default, Debug)]
pub struct ActiveSegment {
    pub turn_id: Option<String>,
    pub has_user_turn: bool,
    pub has_turn_context: bool,
    pub has_full_world_state: bool,
    pub saw_compaction: bool,
}

pub fn turn_ids_compatible(active: Option<&str>, item: Option<&str>) -> bool {
    active.is_none_or(|a| item.is_none_or(|b| a == b))
}

pub fn analyze_scan(scan: MetadataScan) -> CompactionAnalysis {
    let mut reasons = Vec::new();
    if scan.malformed_lines > 0 {
        reasons.push(format!(
            "{} malformed JSONL record(s); destructive compaction is disabled",
            scan.malformed_lines
        ));
    }
    if scan.session_meta.is_none() {
        reasons.push("canonical session_meta record not found".to_string());
    }
    for (index, tag) in &scan.unknown.examples {
        reasons.push(format!(
            "line {}: unknown rollout item type `{}`; destructive compaction is disabled",
            index + 1,
            if tag.is_empty() { "<missing>" } else { tag }
        ));
    }
    if scan.unknown.count > scan.unknown.examples.len() {
        let mut distinct: Vec<&str> = scan.unknown.distinct.iter().map(String::as_str).collect();
        distinct.sort_unstable();
        reasons.push(format!(
            "... and {} further unknown rollout item record(s) across {} distinct type(s): {}",
            scan.unknown.count - scan.unknown.examples.len(),
            distinct.len(),
            distinct.join(", ")
        ));
    }

    let mut must_scan_to_start = false;
    let mut saw_compaction = false;
    let mut saw_completed_turn_context = false;
    let mut active = ActiveSegment::default();
    let mut cutoff_index = None;
    let mut checkpoint_index = None;
    let mut checkpoint_items = None;
    let mut checkpoint_window = None;
    let mut valid_checkpoint_count = 0usize;
    let mut invalid_checkpoint_count = 0usize;

    for record in scan.window.iter().rev() {
        // Nothing later in the reverse walk can rescue a proof that is already impossible.
        if must_scan_to_start {
            break;
        }

        match &record.kind {
            RecordKind::Compacted {
                replacement_present,
                replacement_items,
                window_number,
            } => {
                if !replacement_present || window_number.is_none() {
                    invalid_checkpoint_count += 1;
                    must_scan_to_start = true;
                    reasons.push(format!(
                        "line {}: compaction without replacement_history or window_number forces a full replay",
                        record.physical_index + 1
                    ));
                } else {
                    valid_checkpoint_count += 1;
                    saw_compaction = true;
                    active.saw_compaction = true;
                    if checkpoint_index.is_none() {
                        checkpoint_index = Some(record.physical_index);
                        checkpoint_items = *replacement_items;
                        checkpoint_window = *window_number;
                    }
                }
            }
            RecordKind::Event(EventKind::ThreadRolledBack) => {
                must_scan_to_start = true;
                reasons.push(format!(
                    "line {}: rollback marker forces a full replay",
                    record.physical_index + 1
                ));
            }
            RecordKind::Event(EventKind::ItemCompleted {
                turn_id,
                is_user_message,
            }) => {
                if active.turn_id.is_none() {
                    active.turn_id = turn_id.clone();
                }
                if turn_ids_compatible(active.turn_id.as_deref(), turn_id.as_deref()) {
                    active.has_user_turn |= *is_user_message;
                }
            }
            RecordKind::Event(EventKind::TurnComplete { turn_id }) => {
                if active.turn_id.is_none() {
                    active.turn_id = turn_id.clone();
                }
            }
            RecordKind::Event(EventKind::TurnAborted { turn_id }) => {
                if active.turn_id.is_none() && turn_id.is_some() {
                    active.turn_id = turn_id.clone();
                }
            }
            RecordKind::Event(EventKind::TurnStarted { turn_id }) => {
                if turn_ids_compatible(active.turn_id.as_deref(), turn_id.as_deref()) {
                    if active.has_turn_context
                        && (active.has_user_turn || active.has_full_world_state)
                    {
                        saw_completed_turn_context = true;
                    }
                    active = ActiveSegment::default();
                }
            }
            RecordKind::Event(EventKind::UserMessage) => {
                active.has_user_turn = true;
            }
            RecordKind::TurnContext { turn_id } => {
                if active.turn_id.is_none() {
                    active.turn_id = turn_id.clone();
                }
                if turn_ids_compatible(active.turn_id.as_deref(), turn_id.as_deref()) {
                    active.has_turn_context = true;
                }
            }
            RecordKind::ResponseItem {
                counts_as_user_turn,
            } => {
                active.has_user_turn |= *counts_as_user_turn;
            }
            RecordKind::InterAgentCommunication => {
                active.has_user_turn = true;
            }
            RecordKind::WorldState { full } => {
                active.has_full_world_state |= *full && !active.saw_compaction;
            }
            RecordKind::SessionMeta
            | RecordKind::Event(EventKind::Other)
            | RecordKind::UnknownOuter { .. }
            | RecordKind::Other => {}
        }

        if !must_scan_to_start && saw_compaction && saw_completed_turn_context {
            cutoff_index = Some(record.physical_index);
            break;
        }
    }

    if cutoff_index.is_none() && !must_scan_to_start {
        if scan.window_truncated {
            // The proof ran out of retained records rather than running out of transcript. Say so
            // instead of implying that no cutoff exists anywhere in the file.
            reasons.push(format!(
                "no bounded cutoff proven within the last {} reconstruction-relevant record(s); \
                 the scan window was exhausted (raise --scan-window to search further back)",
                scan.window_capacity
            ));
        } else {
            reasons.push(
                "no bounded cutoff proven: need both a valid compaction checkpoint and completed turn context"
                    .to_string(),
            );
        }
    }

    let mut estimated_result_size = None;
    let mut estimated_removed = None;
    let mut reduction = None;
    let mut removable_checkpoint_bytes = 0u64;

    if let (Some(cutoff), Some((meta_index, meta_record_bytes))) = (cutoff_index, scan.session_meta)
    {
        let cutoff_offset = scan
            .window
            .iter()
            .find(|r| r.physical_index == cutoff)
            .map(|r| r.start_offset)
            .unwrap_or(scan.total_bytes);
        let suffix_bytes = scan.total_bytes.saturating_sub(cutoff_offset);
        let meta_bytes = if meta_index < cutoff {
            meta_record_bytes
        } else {
            0
        };
        let result = suffix_bytes.saturating_add(meta_bytes);
        let removed = scan.total_bytes.saturating_sub(result);
        estimated_result_size = Some(result);
        estimated_removed = Some(removed);
        reduction = if scan.total_bytes > 0 {
            Some((removed as f64 / scan.total_bytes as f64) * 100.0)
        } else {
            Some(0.0)
        };
        // Tracked for the whole file: bounded by how often the conversation was compacted.
        removable_checkpoint_bytes = scan
            .compactions
            .iter()
            .filter(|(index, _)| *index < cutoff)
            .map(|(_, bytes)| *bytes)
            .sum();
    }

    let can_compact = scan.malformed_lines == 0
        && scan.unknown.count == 0
        && scan.session_meta_index().is_some()
        && cutoff_index.is_some()
        && cutoff_index != scan.session_meta_index();

    CompactionAnalysis {
        can_compact,
        content_sha256: scan.content_sha256.clone(),
        cutoff_index,
        checkpoint_index,
        session_meta_index: scan.session_meta_index(),
        retained_records: scan.window.len(),
        scan_window: scan.window_capacity,
        scan_window_truncated: scan.window_truncated,
        reasons,
        total_lines: scan.total_lines,
        parsed_lines: scan.parsed_lines,
        malformed_lines: scan.malformed_lines,
        valid_checkpoint_count,
        invalid_checkpoint_count,
        replacement_history_items_at_checkpoint: checkpoint_items,
        window_number: checkpoint_window,
        original_size_bytes: scan.total_bytes,
        estimated_result_size_bytes: estimated_result_size,
        estimated_removed_bytes: estimated_removed,
        estimated_reduction_percent: reduction,
        removable_checkpoint_bytes,
    }
}

pub fn analyze_session(path: &Path) -> Result<CompactionAnalysis> {
    Ok(analyze_scan(scan_rollout_metadata(path)?))
}

/// Analyze with an explicit retention window. `usize::MAX` retains everything, which is the
/// reference behaviour the differential tests compare against.
pub fn analyze_session_within(path: &Path, window: usize) -> Result<CompactionAnalysis> {
    Ok(analyze_scan(scan_rollout_metadata_within(path, window)?))
}
