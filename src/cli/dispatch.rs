use super::args::Command;
use codex_vault::commands::BatchOptions;
use codex_vault::commands::{
    analyze_command, archive_command, compact_conversation_command, compact_safe_command,
    doctor_command, prune_command, restore_command, restore_conversation_command, scan_command,
};
use codex_vault::error::{Result, VaultError};
use codex_vault::ops::CompactOptions;
use serde_json::Value;

pub(super) fn run_command(command: Command, batch: BatchOptions) -> Result<Value> {
    match command {
        Command::Mcp { .. } => unreachable!("MCP uses its own stdio transport"),
        Command::Index {
            cwd,
            rebuild,
            status,
        } => {
            if status {
                codex_vault::index::status()
            } else {
                codex_vault::index::build(cwd.as_deref().map(std::path::Path::new), rebuild)
            }
        }
        Command::Search {
            query,
            cwd,
            limit,
            offset,
        } => codex_vault::index::search(
            &query,
            cwd.as_deref().map(std::path::Path::new),
            limit,
            offset,
        ),
        Command::Read {
            id,
            cwd,
            limit,
            offset,
        } => codex_vault::index::read(&id, cwd.as_deref().map(std::path::Path::new), offset, limit),
        Command::Storage => codex_vault::storage::inventory(),
        Command::Menu { .. } => unreachable!("menu handled before JSON commands"),
        Command::Scan { cwd, .. } => scan_command(cwd, batch),
        Command::Analyze {
            session,
            session_flag,
            cwd,
            scan_window,
        } => analyze_command(session.or(session_flag), cwd, scan_window, batch),
        Command::Archive {
            session,
            session_flag,
            cwd,
            force,
        } => archive_command(
            session.or(session_flag).expect("required target"),
            cwd,
            force,
        ),
        Command::CompactSafe {
            dry_run,
            session,
            session_flag,
            cwd,
            scan_window,
            allow_spawned_threads,
        } => compact_safe_command(
            session.or(session_flag),
            cwd,
            CompactOptions {
                dry_run,
                scan_window,
                allow_spawned_threads,
            },
            batch,
        ),
        Command::CompactConversation {
            session,
            cwd,
            dry_run,
            scan_window,
        } => compact_conversation_command(
            session,
            cwd,
            CompactOptions {
                dry_run,
                scan_window,
                allow_spawned_threads: false,
            },
        ),
        Command::Prune {
            session,
            cwd,
            unreferenced_backups,
            apply,
        } => prune_command(session, cwd, unreferenced_backups, apply),
        Command::Restore {
            session,
            cwd,
            original,
            to,
            list,
        } => restore_command(session, cwd, original, to, list),
        Command::RestoreConversation { session, cwd } => restore_conversation_command(session, cwd),
        Command::Doctor {
            session,
            session_flag,
            cwd,
            deep,
        } => {
            let chosen = match (session, session_flag) {
                (Some(a), Some(b)) if a != b => {
                    return Err(VaultError::ConflictingArguments {
                        detail: "doctor received two different session references",
                    })
                }
                (Some(a), _) => Some(a),
                (_, Some(b)) => Some(b),
                (None, None) => None,
            };
            doctor_command(chosen, cwd, deep, batch)
        }
    }
}
