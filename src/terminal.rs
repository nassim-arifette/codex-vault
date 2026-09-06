//! Human-facing terminal output and optional menu. All actions use the same CLI core.
use codex_vault::analysis::analyze_session;
use codex_vault::discovery::{
    discover_sessions, lineage_successors, parse_filter, resolve_session_reference,
};
use codex_vault::error::Result;
use codex_vault::ops::{
    archive_impl, compact_safe_impl, doctor_one, list_anchors, restore_impl, DoctorDepth,
    RestoreTarget,
};
use codex_vault::rollout::{is_codex_zstd_jsonl, read_session_head};
use codex_vault::util::format_size;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn short(text: &str, width: usize) -> String {
    let text = clean(text);
    if text.chars().count() <= width {
        text
    } else {
        format!(
            "{}…",
            text.chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    }
}

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

fn prompt(text: &str) -> Result<Option<String>> {
    print!("{text}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

fn show_action(action: Result<Value>) {
    match action {
        Ok(value) => render(&value),
        Err(err) => eprintln!("Error [{}]: {}", err.code(), clean(&err.to_string())),
    }
}

fn conversation(path: &Path) -> Result<()> {
    loop {
        println!("\nConversation: {}", clean(&path.display().to_string()));
        println!("1. Analyze\n2. Back up the current state\n3. Compact with an automatic recovery snapshot\n4. Verify backups and conversation\n5. Restore a backup\n0. Return");
        let Some(choice) = prompt("Action > ")? else {
            return Ok(());
        };
        match choice.as_str() {
            "0" | "q" => return Ok(()),
            "1" => show_action(analyze_session(path).map(|a| json!({"analysis": a}))),
            "2" => show_action(archive_impl(path, true).map(|v| json!(v))),
            "3" => {
                let head = read_session_head(path)?;
                if is_codex_zstd_jsonl(path)
                    || head.provenance.is_spawned_thread()
                    || !lineage_successors(&head.session_id, &head.page_id).is_empty()
                {
                    println!("This file is protected from compaction (Codex compression, a spawned thread or a page with a successor). You can analyze it or verify its backups.");
                    continue;
                }
                let analysis = analyze_session(path)?;
                if analysis.can_compact && analysis.estimated_removed_bytes == Some(0) {
                    println!("Already compact: no changes needed.");
                    continue;
                }
                if !analysis.can_compact {
                    render(&json!({"analysis": analysis}));
                    continue;
                }
                let preview = codex_vault::ops::compact_safe_impl_with(
                    path,
                    codex_vault::ops::CompactOptions {
                        dry_run: true,
                        ..Default::default()
                    },
                )?;
                render(&json!(preview));
                if prompt("Compact this conversation? [y/N] > ")?
                    .is_some_and(|s| s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes"))
                {
                    show_action(compact_safe_impl(path).map(|v| json!(v)));
                }
            }
            "4" => show_action(doctor_one(path, DoctorDepth::Deep).map(|v| json!(v))),
            "5" => {
                let states = list_anchors(path)?;
                render(&states);
                let anchors = states["anchors"].as_array().cloned().unwrap_or_default();
                if anchors.is_empty() {
                    continue;
                }
                let Some(input) = prompt("Backup number (0 = return) > ")? else {
                    return Ok(());
                };
                let selected = input
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| anchors.get(n));
                if let Some(anchor) = selected {
                    let backup = PathBuf::from(anchor["backup_path"].as_str().unwrap());
                    println!(
                        "Restore {}. The current state will be saved so you can undo this.",
                        clean(&backup.display().to_string())
                    );
                    if prompt("Restore this conversation? [y/N] > ")?.is_some_and(|s| {
                        s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes")
                    }) {
                        show_action(
                            restore_impl(path, RestoreTarget::Backup(backup)).map(|v| json!(v)),
                        );
                    }
                } else if input != "0" {
                    println!("Invalid number.");
                }
            }
            _ => println!("Choose a number from 0 to 5."),
        }
    }
}

pub fn menu(cwd: Option<String>) -> Result<()> {
    let filter = parse_filter(cwd)?;
    let mut sessions = discover_sessions(filter.as_deref())?;
    let mut query = String::new();
    let mut page = 0usize;
    let mut include_spawned = false;
    let mut by_size = true;
    const PAGE: usize = 12;
    loop {
        let total_size: u64 = sessions.iter().map(|s| s.size_bytes).sum();
        let mut rows: Vec<_> = sessions
            .iter()
            .filter(|s| include_spawned || !s.is_spawned_thread)
            .filter(|s| {
                format!(
                    "{} {} {}",
                    s.title.as_deref().unwrap_or(""),
                    s.cwd_hint.as_deref().unwrap_or(""),
                    s.session_id
                )
                .to_lowercase()
                .contains(&query)
            })
            .collect();
        if by_size {
            rows.sort_by_key(|s| std::cmp::Reverse(s.size_bytes));
        } else {
            rows.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
        }
        page = page.min(rows.len().saturating_sub(1) / PAGE);
        println!(
            "\nCODEX VAULT — {} total\n{} matching rollouts — page {}/{}",
            format_size(total_size),
            rows.len(),
            page + 1,
            rows.len().div_ceil(PAGE).max(1)
        );
        for (i, s) in rows.iter().enumerate().skip(page * PAGE).take(PAGE) {
            let date = chrono::DateTime::from_timestamp(s.modified_at as i64, 0)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            println!(
                "{:>3}. {:>8} | {} | {}{}",
                i + 1,
                format_size(s.size_bytes),
                date,
                short(s.title.as_deref().unwrap_or(&s.session_id), 58),
                if s.is_spawned_thread {
                    " [spawned thread]"
                } else {
                    ""
                }
            );
            println!("     {}", short(&s.file_stem, 110));
            println!(
                "     {}",
                short(s.cwd_hint.as_deref().unwrap_or("Unknown project"), 110)
            );
        }
        println!("Number = open | /text = filter title or project | n/p = pages | r = refresh\ns = size | d = date | a = show/hide spawned threads | f = file | q = quit");
        let Some(input) = prompt("Choice > ")? else {
            return Ok(());
        };
        match input.as_str() {
            "q" | "0" => return Ok(()),
            "n" => page += 1,
            "p" => page = page.saturating_sub(1),
            "r" => sessions = discover_sessions(filter.as_deref())?,
            "s" => {
                by_size = true;
                page = 0;
            }
            "d" => {
                by_size = false;
                page = 0;
            }
            "a" => {
                include_spawned = !include_spawned;
                page = 0;
            }
            "f" => {
                if let Some(reference) = prompt("Rollout file path (Enter = return) > ")? {
                    if reference.is_empty() {
                        continue;
                    }
                    match resolve_session_reference(reference.trim_matches('"'), None)
                        .and_then(|p| conversation(&p))
                    {
                        Ok(()) => {}
                        Err(err) => eprintln!("{}", clean(&err.to_string())),
                    }
                }
            }
            _ if input.starts_with('/') => {
                query = input[1..].to_lowercase();
                page = 0;
            }
            _ => {
                if let Some(session) = input
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| rows.get(n))
                {
                    if let Err(err) = conversation(&session.path) {
                        eprintln!("{}", clean(&err.to_string()));
                    }
                } else {
                    println!("Invalid choice.");
                }
            }
        }
    }
}
