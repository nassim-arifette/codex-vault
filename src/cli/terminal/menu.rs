use super::render::render;
use super::text::{clean, short};
use codex_vault::analysis::analyze_session;
use codex_vault::commands::{archive_command, compact_safe_command, restore_command, BatchOptions};
use codex_vault::discovery::{
    discover_sessions, lineage_successors, parse_filter, resolve_session_reference,
};
use codex_vault::error::Result;
use codex_vault::ops::{doctor_one, CompactOptions, DoctorDepth};
use codex_vault::rollout::{is_codex_zstd_jsonl, read_session_head};
use codex_vault::util::format_size;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    let session = path.to_string_lossy().into_owned();
    loop {
        println!("\nConversation: {}", clean(&path.display().to_string()));
        println!("1. Analyze\n2. Back up the current state\n3. Compact with an automatic recovery snapshot\n4. Verify backups and conversation\n5. Restore a backup\n0. Return");
        let Some(choice) = prompt("Action > ")? else {
            return Ok(());
        };
        match choice.as_str() {
            "0" | "q" => return Ok(()),
            "1" => show_action(analyze_session(path).map(|a| json!({"analysis": a}))),
            "2" => show_action(archive_command(session.clone(), None, true)),
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
                let preview = compact_safe_command(
                    Some(session.clone()),
                    None,
                    CompactOptions {
                        dry_run: true,
                        ..Default::default()
                    },
                    BatchOptions::default(),
                )?;
                render(&preview);
                if prompt("Compact this conversation? [y/N] > ")?
                    .is_some_and(|s| s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes"))
                {
                    show_action(compact_safe_command(
                        Some(session.clone()),
                        None,
                        CompactOptions::default(),
                        BatchOptions::default(),
                    ));
                }
            }
            "4" => show_action(doctor_one(path, DoctorDepth::Deep).map(|v| json!(v))),
            "5" => {
                let states = restore_command(session.clone(), None, false, None, true)?;
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
                        show_action(restore_command(
                            session.clone(),
                            None,
                            false,
                            Some(backup.to_string_lossy().into_owned()),
                            false,
                        ));
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
