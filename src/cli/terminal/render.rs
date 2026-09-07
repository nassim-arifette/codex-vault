use super::text::{clean, short};
use codex_vault::util::format_size;
use serde_json::Value;
use std::collections::HashMap;

fn label(status: &str) -> &str {
    match status {
        "ok" => "OK",
        "already_compact" => "Already compact: no changes needed",
        "exists" => "Backup already exists",
        "snapshot_created" => "New snapshot saved",
        "archived_only" => "Backup saved; compaction is not applicable",
        "warning" => "Verification: review the reported issues",
        "failed" | "verification_failed" => "VERIFICATION FAILED",
        "restored_after_failed_verification" => "Compaction undone; previous state restored",
        "skipped_lineage_source" => "Page retained: a later page depends on it",
        "skipped_spawned_thread" => "Spawned thread retained",
        "read_only_native_zstd" => "Already compressed by Codex (read-only)",
        other => other,
    }
}

fn render_index_stale_hint(value: &Value) {
    if value["search_index"]["may_be_stale"] == true {
        println!("Search index may be stale.");
        println!(
            "Run: {}",
            clean(
                value["search_index"]["refresh_command"]
                    .as_str()
                    .unwrap_or("codex-vault index")
            )
        );
    }
}

/// Keep scan presentation separate from batch reports and the complete JSON contract.
pub fn render_scan(value: &Value, all: bool, paths: bool) {
    let Some(sessions) = value["sessions"].as_array() else {
        return;
    };
    println!(
        "{} conversation file{} — {} total",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" },
        value["total_size_human"].as_str().unwrap_or("0 B")
    );
    if sessions.is_empty() {
        println!("No matching conversations found.");
        return;
    }

    let mut rows: Vec<_> = sessions.iter().collect();
    rows.sort_by(|a, b| {
        b["size_bytes"]
            .as_u64()
            .cmp(&a["size_bytes"].as_u64())
            .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
    });
    // A thread can have several rollout pages. Count all results, including hidden ones,
    // so a displayed reference selects a file rather than an ambiguous thread ID.
    let mut references = HashMap::new();
    for row in &rows {
        let id = row["session_id"].as_str().unwrap_or("");
        let stem = row["file_stem"].as_str().unwrap_or("");
        *references.entry(id).or_insert(0usize) += 1;
        if stem != id {
            *references.entry(stem).or_insert(0usize) += 1;
        }
    }
    let shown = if all { rows.len() } else { rows.len().min(5) };
    println!("Showing {shown} of {}, largest first.\n", rows.len());
    println!("{:>10}  {:42}  PROJECT", "SIZE", "CONVERSATION");
    for row in rows.iter().take(shown) {
        let project_path = row["cwd_hint"].as_str().unwrap_or("");
        // Handle both path separators even when inspecting Windows rollouts on Unix.
        let project = project_path
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or("(unknown)");
        println!(
            "{:>10}  {:42}  {}",
            format_size(row["size_bytes"].as_u64().unwrap_or(0)),
            short(row["title"].as_str().unwrap_or("Untitled conversation"), 42),
            short(project, 22)
        );
        if paths {
            println!("            Project: {}", clean(project_path));
            println!(
                "            Path: {}",
                clean(row["path"].as_str().unwrap_or(""))
            );
        } else {
            let reference = ["session_id", "file_stem"]
                .iter()
                .filter_map(|key| row[key].as_str())
                .find(|reference| !reference.is_empty() && references.get(reference) == Some(&1))
                .unwrap_or(row["path"].as_str().unwrap_or(""));
            println!("            Ref: {}", clean(reference));
        }
    }
    if shown < rows.len() {
        let remaining = rows.len() - shown;
        println!(
            "\n{remaining} more file{}. Use --all to show every result, or --cwd PATH to filter by project.",
            if remaining == 1 { "" } else { "s" }
        );
    }
    if !paths {
        println!("\nUse Ref with analyze, compact or doctor. Add --paths for full paths.");
    }
}

pub fn render(value: &Value) {
    if value["kind"] == "storage_inventory" {
        let required = &value["required_recovery_anchors"];
        println!("Storage inventory (logical file bytes; read-only)\n");
        println!(
            "{:<34} {:>12}",
            "Native rollouts",
            format_size(value["native_rollouts"]["bytes"].as_u64().unwrap_or(0))
        );
        println!(
            "{:<34} {:>12}",
            "Immutable originals",
            format_size(
                required["immutable_originals"]["bytes"]
                    .as_u64()
                    .unwrap_or(0)
            )
        );
        println!(
            "{:<34} {:>12}",
            "Pre-compact snapshots",
            format_size(
                required["precompact_snapshots"]["bytes"]
                    .as_u64()
                    .unwrap_or(0)
            )
        );
        println!(
            "{:<34} {:>12}",
            "Whole-chain snapshots",
            format_size(
                required["prechain_snapshots"]["bytes"]
                    .as_u64()
                    .unwrap_or(0)
            )
        );
        println!(
            "{:<34} {:>12}",
            "Pre-restore snapshots",
            format_size(
                required["prerestore_snapshots"]["bytes"]
                    .as_u64()
                    .unwrap_or(0)
            )
        );
        println!(
            "{:<34} {:>12}",
            "Pre-restore chain snapshots",
            format_size(
                required["prerestore_chain_snapshots"]["bytes"]
                    .as_u64()
                    .unwrap_or(0)
            )
        );
        println!(
            "{:<34} {:>12}",
            "Other required snapshots",
            format_size(
                required["manual_snapshots"]["bytes"].as_u64().unwrap_or(0)
                    + required["other_recovery_anchors"]["bytes"]
                        .as_u64()
                        .unwrap_or(0)
            )
        );
        if let Some(bytes) = value["unreferenced_backups"]["bytes"].as_u64() {
            println!("{:<34} {:>12}", "Unreferenced backups", format_size(bytes));
        } else {
            println!("{:<34} {:>12}", "Unreferenced backups", "unknown");
        }
        if value["ambiguous_backups"]["files"].as_u64().unwrap_or(0) > 0 {
            println!(
                "{:<34} {:>12}",
                "Ambiguous backups",
                format_size(value["ambiguous_backups"]["bytes"].as_u64().unwrap_or(0))
            );
            println!("  Some backup archives cannot be classified from the readable journals.");
        }
        if value["other_backup_directory_files"]["files"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            println!(
                "{:<34} {:>12}",
                "Other files in backups directory",
                format_size(
                    value["other_backup_directory_files"]["bytes"]
                        .as_u64()
                        .unwrap_or(0)
                )
            );
        }
        println!(
            "{:<34} {:>12}",
            "Recovery metadata",
            format_size(value["recovery_metadata"]["bytes"].as_u64().unwrap_or(0))
        );
        println!(
            "{:<34} {:>12}",
            "Search index (rebuildable)",
            format_size(value["search_index"]["bytes"].as_u64().unwrap_or(0))
        );
        println!(
            "\n{:<34} {:>12}",
            "Total",
            format_size(value["total_bytes"].as_u64().unwrap_or(0))
        );
        let missing = required["missing_referenced_files"].as_u64().unwrap_or(0);
        if missing > 0 {
            println!("Warning: {missing} recorded recovery anchor(s) are missing from disk.");
        }
        println!("No retention eligibility was assessed and no action was taken.");
        return;
    }
    if let Some(matches) = value["matches"].as_array() {
        for hit in matches {
            println!(
                "{} | {} | {}\n{}\n",
                clean(hit["id"].as_str().unwrap_or("")),
                clean(hit["project"].as_str().unwrap_or("")),
                clean(hit["role"].as_str().unwrap_or("")),
                clean(hit["excerpt"].as_str().unwrap_or(""))
            );
        }
        if matches.is_empty() {
            println!("No matches in the index. Run `codex-vault index` to refresh it.");
        }
        if let Some(offset) = value["next_offset"].as_u64() {
            println!("Next page: --offset {offset}");
        }
        return;
    }
    if let Some(text) = value["text"].as_str() {
        println!("{}", clean(text));
        if let Some(reference) = value["verified_reference"].as_object() {
            println!(
                "Verified source: {} | line {}",
                clean(reference["path"].as_str().unwrap_or("")),
                reference["line"]
            );
        }
        if let Some(offset) = value["next_offset"].as_u64() {
            println!("Next page: --offset {offset}");
        }
        return;
    }
    if value.get("index_bytes").is_some() {
        println!(
            "Index: {} sources, {} passages, {}. Total vault size: {}.",
            value["sources"],
            value["passages"],
            format_size(value["index_bytes"].as_u64().unwrap_or(0)),
            format_size(value["vault_bytes"].as_u64().unwrap_or(0))
        );
        println!(
            "Oversized records skipped: {}",
            value["skipped_oversized_records"]
        );
        return;
    }
    if let Some(rows) = value.as_array() {
        for row in rows {
            render(row);
        }
        return;
    }
    if let Some(rows) = value.get("sessions").and_then(Value::as_array) {
        if let Some(total) = value.get("total_size_human").and_then(Value::as_str) {
            println!("{} conversation files — {}", rows.len(), total);
        }
        for row in rows {
            render(row);
        }
        render_index_stale_hint(value);
        return;
    }
    if value.get("file_stem").is_some() {
        let size = value["size_bytes"].as_u64().unwrap_or(0);
        println!(
            "{} | {} | {}",
            format_size(size),
            clean(
                value["title"]
                    .as_str()
                    .unwrap_or(value["session_id"].as_str().unwrap_or("Conversation"))
            ),
            clean(value["cwd_hint"].as_str().unwrap_or(""))
        );
        println!("  {}", clean(value["path"].as_str().unwrap_or("")));
        return;
    }
    if let Some(session) = value.get("session").and_then(Value::as_str) {
        println!("\n{}", clean(session));
    }
    if let Some(result) = value.get("result") {
        render(result);
        return;
    }
    if let Some(a) = value.get("analysis") {
        if a["can_compact"] == true && a["estimated_removed_bytes"] == 0 {
            println!("Already compact: no bytes to remove.");
            return;
        }
        if a["can_compact"] == true {
            println!(
                "Compaction candidate: {} removable from a {} rollout.",
                format_size(a["estimated_removed_bytes"].as_u64().unwrap_or(0)),
                format_size(a["original_size_bytes"].as_u64().unwrap_or(0))
            );
            println!("Pagination, spawned-thread and file-lock checks still apply when the operation runs.");
        } else {
            println!("Compaction is not applicable; you can still create a backup.");
        }
        for reason in a["reasons"].as_array().into_iter().flatten() {
            if let Some(s) = reason.as_str() {
                println!("  {}", clean(s));
            }
        }
        return;
    }
    if let Some(status) = value["status"].as_str() {
        println!("{}", label(status));
    }
    for key in ["reason", "notes"] {
        for item in value[key].as_array().into_iter().flatten() {
            if let Some(s) = item.as_str() {
                println!("  {}", clean(s));
            }
        }
    }
    for key in ["error", "message", "note"] {
        if let Some(s) = value[key].as_str() {
            println!("{}", clean(s));
        }
    }
    if let Some(backup) = value["backup"].as_str() {
        println!("Backup: {}", clean(backup));
    }
    if let Some(stats) = value.get("stats") {
        if let Some(storage) = stats.get("storage") {
            let net = storage["net_saved_bytes"].as_i64().unwrap_or(0);
            println!(
                "Net savings, including backups and metadata: {}{}",
                if net < 0 { "-" } else { "" },
                format_size(net.unsigned_abs())
            );
            if storage["space_increased"] == true {
                println!("Warning: this operation increased total storage usage.");
            }
        }
        if let Some(plan) = stats.get("storage_preview") {
            let net = plan["estimated_net_saved_bytes_excluding_metadata"]
                .as_i64()
                .unwrap_or(0);
            println!(
                "New backup: {}. Estimated net savings: {}{} (excluding journal growth).",
                format_size(plan["new_backup_bytes"].as_u64().unwrap_or(0)),
                if net < 0 { "-" } else { "" },
                format_size(net.unsigned_abs())
            );
            if plan["may_increase_usage"] == true {
                println!("Warning: total storage may increase after including the backup.");
            }
        }
        if let (Some(before), Some(after)) =
            (stats["input_size"].as_u64(), stats["result_size"].as_u64())
        {
            println!(
                "{} → {} ({} removed from the active rollout)",
                format_size(before),
                format_size(after),
                format_size(before.saturating_sub(after))
            );
        }
    }
    render_index_stale_hint(value);
    if let Some(anchors) = value["anchors"].as_array() {
        for (i, a) in anchors.iter().enumerate() {
            println!(
                "{}. {} | {}{}",
                i + 1,
                a["source_size_human"].as_str().unwrap_or(""),
                clean(a["backup_path"].as_str().unwrap_or("")),
                if a["is_current_restore_target"] == true {
                    " [latest saved state]"
                } else {
                    ""
                }
            );
        }
        if anchors.is_empty() {
            println!("No recorded backups.");
        }
    }
    if let Some(candidates) = value["candidates"].as_array() {
        for path in candidates {
            println!("  {}", clean(path.as_str().unwrap_or("")));
        }
    }
}
