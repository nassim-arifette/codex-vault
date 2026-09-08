//! Locating sessions on disk and resolving user-supplied references.

use crate::error::{Result, VaultError};
use crate::parallel::map_ordered;
use crate::paths::{
    codex_root, is_path_related, is_path_within, normalized_path, strip_verbatim_prefix,
};
use crate::rollout::{
    is_codex_zstd_jsonl, is_plain_jsonl, read_session_head, read_session_head_from_file,
    rollout_stem, strip_rollout_extension, SessionIdSource,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

/// Discovery variant for latency-sensitive read-only commands. Directory enumeration stays
/// serial and deterministic, while independent rollout-head reads may run concurrently.
pub fn discover_sessions_with_jobs(
    cwd_filter: Option<&Path>,
    scope: FilterScope,
    jobs: usize,
) -> Result<Vec<SessionInfo>> {
    discover_sessions_scoped_with_jobs(cwd_filter, scope, jobs)
}

pub fn discover_sessions_scoped(
    cwd_filter: Option<&Path>,
    scope: FilterScope,
) -> Result<Vec<SessionInfo>> {
    discover_sessions_scoped_with_jobs(cwd_filter, scope, 1)
}

fn discover_sessions_scoped_with_jobs(
    cwd_filter: Option<&Path>,
    scope: FilterScope,
    jobs: usize,
) -> Result<Vec<SessionInfo>> {
    let titles = session_titles();
    let mut candidates = Vec::new();
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
            // WalkDir already resolved the entry type. Calling `Path::is_file()` here performs a
            // second metadata lookup per rollout on Windows before the later metadata read.
            if !entry.file_type().is_file() || (!is_plain_jsonl(path) && !is_codex_zstd_jsonl(path))
            {
                continue;
            }
            candidates.push((path.to_path_buf(), source));
        }
    }

    let rows = map_ordered(
        &candidates,
        jobs,
        |_, (path, source)| -> Result<Option<SessionInfo>> {
            let file = match fs::File::open(path) {
                Ok(file) => file,
                Err(_) => return Ok(None),
            };
            // Preserve the historical error contract: unreadable/malformed heads are skipped,
            // while metadata errors for an otherwise readable session abort discovery.
            let metadata = file.metadata();
            let head = match read_session_head_from_file(path, file) {
                Ok(v) => v,
                Err(_) => return Ok(None),
            };
            let metadata =
                metadata.map_err(|e| VaultError::io("reading session metadata", path, e))?;
            if let Some(filter) = cwd_filter {
                let Some(hint) = head.cwd_hint.as_deref() else {
                    return Ok(None);
                };
                let keep = match scope {
                    FilterScope::Related => is_path_related(Path::new(hint), filter),
                    FilterScope::Within => is_path_within(Path::new(hint), filter),
                };
                if !keep {
                    return Ok(None);
                }
            }
            let modified_at = metadata
                .modified()
                .map_err(|e| VaultError::io("reading session mtime", path, e))?
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Ok(Some(SessionInfo {
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
                    (*source).to_string()
                },
            }))
        },
    );

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(item) = row? {
            items.push(item);
        }
    }
    items.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
    Ok(items)
}

pub(crate) fn session_titles() -> HashMap<String, String> {
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

/// Snapshot of pagination successor relationships for the current Codex catalog.
///
/// Batch diagnostics build this once and reuse it for every rollout instead of recursively
/// walking the whole sessions tree once per target. The snapshot is intentionally ephemeral: a
/// destructive operation that needs lineage freshness still calls `lineage_successors` directly.
#[derive(Default)]
pub(crate) struct LineageIndex {
    successors: HashMap<(String, String), Vec<LineageSuccessor>>,
}

impl LineageIndex {
    pub(crate) fn discover() -> Self {
        let mut index = LineageIndex::default();
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
                if !entry.file_type().is_file()
                    || (!is_plain_jsonl(path) && !is_codex_zstd_jsonl(path))
                {
                    continue;
                }
                let Ok(head) = read_session_head(path) else {
                    continue;
                };
                let Some(base_ref) = head.provenance.history_base.as_ref() else {
                    continue;
                };
                let Some(predecessor_page_id) = base_ref.thread_id.as_ref() else {
                    continue;
                };
                index
                    .successors
                    .entry((head.session_id, predecessor_page_id.clone()))
                    .or_default()
                    .push(LineageSuccessor {
                        path: path.to_path_buf(),
                        end_byte_offset: base_ref.end_byte_offset,
                    });
            }
        }
        for successors in index.successors.values_mut() {
            successors.sort_by(|a, b| a.path.cmp(&b.path));
        }
        index
    }

    pub(crate) fn successors(&self, thread_id: &str, page_id: &str) -> &[LineageSuccessor] {
        self.successors
            .get(&(thread_id.to_string(), page_id.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationPage {
    pub path: PathBuf,
    pub page_id: String,
    pub size_bytes: u64,
    pub predecessor_page_id: Option<String>,
    pub predecessor_end_byte_offset: Option<u64>,
    pub predecessor_end_ordinal_exclusive: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationChain {
    pub session_id: String,
    /// Root-to-leaf order. This first implementation intentionally accepts only a single linear
    /// dependency chain; forks and ambiguous layouts are refused before mutation.
    pub pages: Vec<ConversationPage>,
}

/// Resolve every discovered rollout for a conversation and prove that its pagination graph is a
/// single complete linear chain. This is deliberately stricter than `lineage_successors`: a
/// whole-conversation mutation must know the complete dependency closure before it writes bytes.
pub fn resolve_conversation_chain(
    reference: &str,
    cwd_filter: Option<&Path>,
) -> Result<ConversationChain> {
    let session_id = if let Some(raw) = reference.strip_prefix("codex://threads/") {
        raw.trim_matches('/').to_string()
    } else {
        let as_path = Path::new(reference);
        if as_path.exists() {
            read_session_head(as_path)?.session_id
        } else {
            let wanted = as_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(strip_rollout_extension)
                .unwrap_or(reference);
            let matches: Vec<_> = discover_sessions(cwd_filter)?
                .into_iter()
                .filter(|s| s.session_id == wanted || s.file_stem == wanted)
                .collect();
            if matches.is_empty() {
                return Err(VaultError::SessionNotFound {
                    reference: reference.to_string(),
                });
            }
            let ids: HashSet<_> = matches.iter().map(|s| s.session_id.as_str()).collect();
            if ids.len() != 1 {
                return Err(VaultError::AmbiguousSession {
                    reference: reference.to_string(),
                    matches: matches.into_iter().map(|s| s.path).collect(),
                });
            }
            matches[0].session_id.clone()
        }
    };

    let sessions: Vec<_> = discover_sessions(cwd_filter)?
        .into_iter()
        .filter(|s| s.session_id == session_id)
        .collect();
    if sessions.is_empty() {
        return Err(VaultError::SessionNotFound {
            reference: reference.to_string(),
        });
    }

    let mut by_id: HashMap<String, ConversationPage> = HashMap::new();
    for info in sessions {
        let head = read_session_head(&info.path)?;
        if by_id.contains_key(&head.page_id) {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "conversation `{session_id}` has more than one rollout with page id `{}`",
                    head.page_id
                ),
            });
        }
        let base = head.provenance.history_base.as_ref();
        by_id.insert(
            head.page_id.clone(),
            ConversationPage {
                path: info.path,
                page_id: head.page_id,
                size_bytes: info.size_bytes,
                predecessor_page_id: base.and_then(|b| b.thread_id.clone()),
                predecessor_end_byte_offset: base.and_then(|b| b.end_byte_offset),
                predecessor_end_ordinal_exclusive: base.and_then(|b| b.end_ordinal_exclusive),
            },
        );
    }

    let roots: Vec<_> = by_id
        .values()
        .filter(|p| p.predecessor_page_id.is_none())
        .map(|p| p.page_id.clone())
        .collect();
    if roots.len() != 1 {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "conversation `{session_id}` must have exactly one pagination root; found {}",
                roots.len()
            ),
        });
    }

    let mut next: HashMap<String, String> = HashMap::new();
    for page in by_id.values() {
        let Some(parent) = page.predecessor_page_id.as_ref() else {
            continue;
        };
        if !by_id.contains_key(parent) {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "conversation `{session_id}` is incomplete: page `{}` depends on missing page `{parent}`",
                    page.page_id
                ),
            });
        }
        if page.predecessor_end_byte_offset.is_none()
            || page.predecessor_end_ordinal_exclusive.is_none()
        {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "conversation `{session_id}` page `{}` has an incomplete history_base boundary",
                    page.page_id
                ),
            });
        }
        if let Some(existing) = next.insert(parent.clone(), page.page_id.clone()) {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "conversation `{session_id}` forks at page `{parent}` into `{existing}` and `{}`; fork compaction is not yet proven safe",
                    page.page_id
                ),
            });
        }
    }

    let mut ordered = Vec::with_capacity(by_id.len());
    let mut seen = HashSet::new();
    let mut current = roots[0].clone();
    loop {
        if !seen.insert(current.clone()) {
            return Err(VaultError::InvalidInput {
                reason: format!("conversation `{session_id}` contains a pagination cycle"),
            });
        }
        ordered.push(
            by_id
                .get(&current)
                .expect("page id from validated graph")
                .clone(),
        );
        match next.get(&current) {
            Some(n) => current = n.clone(),
            None => break,
        }
    }
    if ordered.len() != by_id.len() {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "conversation `{session_id}` contains disconnected or cyclic pagination dependencies"
            ),
        });
    }

    Ok(ConversationChain {
        session_id,
        pages: ordered,
    })
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
