//! Locating sessions on disk and resolving user-supplied references.

use crate::error::{Result, VaultError};
use crate::paths::{
    codex_root, is_path_related, is_path_within, normalized_path, strip_verbatim_prefix,
};
use crate::rollout::{
    is_codex_zstd_jsonl, is_plain_jsonl, read_session_head, rollout_stem, strip_rollout_extension,
    SessionIdSource,
};
use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// How a `--cwd` filter is matched against a session's recorded working directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterScope {
    /// Either path may contain the other. Convenient for discovery: standing in a subdirectory
    /// still finds the session that was started at the repository root.
    Related,
    /// The session's cwd must be the filter or live inside it. Required before anything
    /// destructive, so `compact --cwd .` can never reach a parent directory's project.
    Within,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub title: Option<String>,
    pub session_id: String,
    /// Whether `session_id` came from the transcript or was guessed from the filename. The vault
    /// keys every manifest and backup on this id, so a guess is worth surfacing.
    pub session_id_source: SessionIdSource,
    pub file_stem: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub cwd_hint: Option<String>,
    pub source: String,
    /// The Codex build that wrote the rollout, straight from its `session_meta`.
    pub cli_version: Option<String>,
    pub originator: Option<String>,
    /// `user`, `subagent`, `guardian_review`, ... Only user threads can be resumed standalone.
    pub thread_source: Option<String>,
    pub is_spawned_thread: bool,
}

pub fn discover_sessions(cwd_filter: Option<&Path>) -> Result<Vec<SessionInfo>> {
    discover_sessions_scoped(cwd_filter, FilterScope::Related)
}

pub fn discover_sessions_scoped(
    cwd_filter: Option<&Path>,
    scope: FilterScope,
) -> Result<Vec<SessionInfo>> {
    let mut items = Vec::new();
    let titles = session_titles();
    for source in ["sessions", "archived_sessions"] {
        let base = codex_root().join(source);
        if !base.exists() {
            continue;
        }
        for entry in WalkDir::new(&base)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !path.is_file() || (!is_plain_jsonl(path) && !is_codex_zstd_jsonl(path)) {
                continue;
            }
            let head = match read_session_head(path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(filter) = cwd_filter {
                let Some(hint) = head.cwd_hint.as_deref() else {
                    continue;
                };
                let keep = match scope {
                    FilterScope::Related => is_path_related(Path::new(hint), filter),
                    FilterScope::Within => is_path_within(Path::new(hint), filter),
                };
                if !keep {
                    continue;
                }
            }
            let metadata = fs::metadata(path)
                .map_err(|e| VaultError::io("reading session metadata", path, e))?;
            let modified_at = metadata
                .modified()
                .map_err(|e| VaultError::io("reading session mtime", path, e))?
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            items.push(SessionInfo {
                title: titles.get(&head.session_id).cloned(),
                session_id: head.session_id,
                session_id_source: head.id_source,
                file_stem: rollout_stem(path),
                path: path.to_path_buf(),
                size_bytes: metadata.len(),
                modified_at,
                cwd_hint: head.cwd_hint,
                cli_version: head.provenance.cli_version.clone(),
                originator: head.provenance.originator.clone(),
                thread_source: head.provenance.thread_source.clone(),
                is_spawned_thread: head.provenance.is_spawned_thread(),
                source: if is_codex_zstd_jsonl(path) {
                    format!("{source}:zstd")
                } else {
                    source.to_string()
                },
            });
        }
    }
    items.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
    Ok(items)
}

fn session_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    if let Ok(file) = fs::File::open(codex_root().join("session_index.jsonl")) {
        for line in BufReader::new(file)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                if let (Some(id), Some(name)) =
                    (value["id"].as_str(), value["thread_name"].as_str())
                {
                    titles.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    titles
}

/// One rollout that continues from another, and the byte offset it depends on.
#[derive(Clone, Debug, Serialize)]
pub struct LineageSuccessor {
    pub path: PathBuf,
    /// Byte offset into the *source* page that this one continues from.
    pub end_byte_offset: Option<u64>,
}

impl LineageSuccessor {
    /// True when the source page is now shorter than the offset this successor points at, i.e.
    /// the chain is already broken and Codex can no longer resume the thread.
    pub fn is_broken_by(&self, source_size: u64) -> bool {
        self.end_byte_offset
            .is_some_and(|offset| offset > source_size)
    }
}

/// Rollouts whose `history_base` continues from `page_id`.
///
/// Codex stores a long thread as several rollout files, and each page records a **byte offset**
/// into the one before it. Shortening a page that something continues from breaks the whole
/// thread: `codex resume` then fails with "invalid paginated history lineage: cutoff byte offset
/// is past the source rollout". Only the newest page of a thread has no successor and can safely
/// be rewritten.
///
/// Successors always live in the same thread, so the search is narrowed by filename before any
/// file is opened.
pub fn lineage_successors(thread_id: &str, page_id: &str) -> Vec<LineageSuccessor> {
    let mut found = Vec::new();
    for source in ["sessions", "archived_sessions"] {
        let base = codex_root().join(source);
        if !base.exists() {
            continue;
        }
        for entry in WalkDir::new(&base)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !path.is_file() || (!is_plain_jsonl(path) && !is_codex_zstd_jsonl(path)) {
                continue;
            }
            let matches_thread = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(thread_id));
            if !matches_thread {
                continue;
            }
            let Ok(head) = read_session_head(path) else {
                continue;
            };
            let Some(base_ref) = head.provenance.history_base.as_ref() else {
                continue;
            };
            if base_ref.thread_id.as_deref() == Some(page_id) {
                found.push(LineageSuccessor {
                    path: path.to_path_buf(),
                    end_byte_offset: base_ref.end_byte_offset,
                });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

pub fn resolve_session_reference(reference: &str, cwd_filter: Option<&Path>) -> Result<PathBuf> {
    let as_path = Path::new(reference);
    if as_path.exists() {
        return as_path
            .canonicalize()
            .map(|p| strip_verbatim_prefix(&p))
            .map_err(|e| VaultError::io("resolving session path", as_path, e));
    }
    // A reference that is not a path is used verbatim; only a filename-shaped one has its
    // rollout extension stripped. There is no in-band sentinel for "no filename" any more.
    let wanted = as_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(strip_rollout_extension)
        .unwrap_or(reference);
    let matches: Vec<SessionInfo> = discover_sessions(cwd_filter)?
        .into_iter()
        .filter(|s| s.session_id == wanted || s.file_stem == wanted)
        .collect();
    match matches.as_slice() {
        [] => Err(VaultError::SessionNotFound {
            reference: reference.to_string(),
        }),
        [one] => Ok(one.path.clone()),
        many => Err(VaultError::AmbiguousSession {
            reference: reference.to_string(),
            matches: many.iter().map(|s| s.path.clone()).collect(),
        }),
    }
}

pub fn parse_filter(value: Option<String>) -> Result<Option<PathBuf>> {
    match value {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() || raw.trim() == "." => env::current_dir()
            .map(Some)
            .map_err(|e| VaultError::io("resolving the current directory", Path::new("."), e)),
        Some(raw) => Ok(Some(normalized_path(Path::new(raw.trim())))),
    }
}
