use crate::analysis::{analyze_session_prefix_within, analyze_session_within, CompactionAnalysis};
use crate::discovery::ConversationChain;
use crate::error::{Result, VaultError};
use crate::hashing::sha256_file;
use crate::ops::CompactOptions;
use crate::rollout::ensure_plain_native_session;
use crate::storage::{compressed_size, process_peak_rss_bytes};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

pub(super) struct PagePlan {
    pub(super) path: PathBuf,
    pub(super) page_id: String,
    pub(super) consumed_prefix: u64,
    pub(super) analysis: CompactionAnalysis,
    pub(super) input_size: u64,
    pub(super) input_sha256: String,
}

pub(super) fn chain_paths(chain: &ConversationChain) -> Vec<PathBuf> {
    chain.pages.iter().map(|p| p.path.clone()).collect()
}

pub(super) fn build_plans(
    chain: &ConversationChain,
    options: CompactOptions,
) -> Result<Vec<PagePlan>> {
    if chain.pages.len() < 2 {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "conversation `{}` has only one discovered rollout; use ordinary `compact`",
                chain.session_id
            ),
        });
    }
    let mut plans = Vec::with_capacity(chain.pages.len());
    for (index, page) in chain.pages.iter().enumerate() {
        ensure_plain_native_session(&page.path)?;
        let input_size = fs::metadata(&page.path)
            .map_err(|e| VaultError::io("reading chain page size", &page.path, e))?
            .len();
        let (consumed_prefix, analysis) = if let Some(successor) = chain.pages.get(index + 1) {
            let boundary =
                successor
                    .predecessor_end_byte_offset
                    .ok_or(VaultError::InvalidInput {
                        reason: format!("successor `{}` has no byte boundary", successor.page_id),
                    })?;
            if boundary > input_size {
                return Err(VaultError::InvalidInput {
                    reason: format!(
                        "page `{}` is {input_size} bytes but successor `{}` requires prefix {boundary}",
                        page.page_id, successor.page_id
                    ),
                });
            }
            (
                boundary,
                analyze_session_prefix_within(&page.path, boundary, options.scan_window)?,
            )
        } else {
            (
                input_size,
                analyze_session_within(&page.path, options.scan_window)?,
            )
        };
        if !analysis.can_compact {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "whole-conversation compaction refused: page `{}` has no proven compactable reconstruction prefix: {}",
                    page.page_id,
                    analysis.reasons.join("; ")
                ),
            });
        }
        let input_sha256 = sha256_file(&page.path)?;
        plans.push(PagePlan {
            path: page.path.clone(),
            page_id: page.page_id.clone(),
            consumed_prefix,
            analysis,
            input_size,
            input_sha256,
        });
    }
    Ok(plans)
}

pub(super) fn preview(
    chain: &ConversationChain,
    plans: &[PagePlan],
    started: Instant,
) -> Result<Value> {
    let native_before: u64 = plans.iter().map(|p| p.input_size).sum();
    let mut estimated_native_after = 0u64;
    let mut estimated_new_backups = 0u64;
    let mut rows = Vec::with_capacity(plans.len());
    for plan in plans {
        let prefix_after = plan
            .analysis
            .estimated_result_size_bytes
            .unwrap_or(plan.consumed_prefix);
        let tail = plan.input_size.saturating_sub(plan.consumed_prefix);
        let result = prefix_after.saturating_add(tail);
        estimated_native_after = estimated_native_after.saturating_add(result);
        let backup = compressed_size(&plan.path)?;
        estimated_new_backups = estimated_new_backups.saturating_add(backup);
        rows.push(json!({
            "page_id": plan.page_id,
            "path": plan.path,
            "input_bytes": plan.input_size,
            "consumed_prefix_bytes": plan.consumed_prefix,
            "estimated_result_bytes_before_history_base_rewrite": result,
            "estimated_removed_bytes": plan.input_size.saturating_sub(result),
            "estimated_new_backup_bytes": backup,
        }));
    }
    let estimated_net =
        native_before as i128 - estimated_native_after as i128 - estimated_new_backups as i128;
    let runtime_ms = started.elapsed().as_millis();
    Ok(json!({
        "status": "preview",
        "session_id": chain.session_id,
        "page_count": chain.pages.len(),
        "layout": "linear_paginated_chain",
        "pages": rows,
        "storage": {
            "accounting_version": 2,
            "scope": "current_operation_preview",
            "measurement": "logical_bytes",
            "native_before_bytes": native_before,
            "estimated_native_after_bytes": estimated_native_after,
            "estimated_new_backup_bytes": estimated_new_backups,
            "estimated_net_saved_bytes_excluding_metadata_and_history_base_line_size_delta": estimated_net,
            "estimated_peak_temporary_disk_bytes": estimated_native_after,
            "note": "Dry-run is operation-scoped: it estimates newly created backups without scanning unrelated Vault files, and excludes manifest/summary/transaction growth plus the small serialized session_meta size delta. Completed operation measures the exact persistent files it touched."
        },
        "performance": {
            "runtime_ms": runtime_ms,
            "process_peak_ram_bytes": process_peak_rss_bytes(),
            "peak_ram_scope": "process_lifetime_high_water_mark"
        }
    }))
}
