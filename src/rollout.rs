//! Reading `.jsonl` / `.jsonl.zst` rollouts and scanning their structure.

use crate::error::{Result, VaultError};
use crate::format::{
    extract_cwd_hint, extract_provenance, extract_session_id, parse_record, EventKind, RecordKind,
    SessionProvenance,
};
use crate::util::HEAD_RECORD_LIMIT;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use zstd::stream::Decoder;

#[derive(Clone, Debug)]
pub struct LineMeta {
    pub physical_index: usize,
    pub start_offset: u64,
    pub bytes: u64,
    pub kind: RecordKind,
}

pub fn is_plain_jsonl(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase().ends_with(".jsonl"))
        .unwrap_or(false)
}

pub fn is_codex_zstd_jsonl(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase().ends_with(".jsonl.zst"))
        .unwrap_or(false)
}

/// Drop a rollout's `.jsonl` / `.jsonl.zst` extension.
pub fn strip_rollout_extension(name: &str) -> &str {
    name.strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))
        .unwrap_or(name)
}

pub fn rollout_stem(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(strip_rollout_extension)
        .unwrap_or("session")
        .to_string()
}

pub fn open_rollout_reader(path: &Path) -> Result<Box<dyn BufRead>> {
    if is_codex_zstd_jsonl(path) {
        let decoder = Decoder::new(File::open(path)?)?;
        return Ok(Box::new(BufReader::new(decoder)));
    }
    Ok(Box::new(BufReader::new(File::open(path)?)))
}

pub fn ensure_plain_native_session(path: &Path) -> Result<()> {
    if is_plain_jsonl(path) {
        return Ok(());
    }
    if is_codex_zstd_jsonl(path) {
        return Err(VaultError::CodexManagedZstd {
            path: path.to_path_buf(),
        });
    }
    Err(VaultError::NotPlainJsonl {
        path: path.to_path_buf(),
    })
}

pub fn record_is_relevant(kind: &RecordKind) -> bool {
    !matches!(
        kind,
        RecordKind::Other
            | RecordKind::Event(EventKind::Other)
            | RecordKind::ResponseItem {
                counts_as_user_turn: false
            }
    )
}

/// Where a session's identity came from.
///
/// The vault keys every manifest, backup and summary on the session id, so falling back to the
/// filename is a fact worth carrying rather than hiding: two rollouts with unreadable metadata
/// would otherwise collide silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdSource {
    SessionMeta,
    FilenameFallback,
}

#[derive(Clone, Debug)]
pub struct SessionHead {
    pub session_id: String,
    pub cwd_hint: Option<String>,
    pub id_source: SessionIdSource,
    /// What the transcript itself says about the Codex build that wrote it.
    pub provenance: SessionProvenance,
    /// This rollout's own identity within its thread; see [`page_id`].
    pub page_id: String,
}

/// A rollout's identity as a *page* of its thread.
///
/// A paginated thread's later pages all carry the same `session_meta.id` — the thread's — and are
/// told apart by the suffix Codex puts in the filename: `rollout-<ts>-<thread>_<page>.jsonl`. A
/// successor's `history_base.thread_id` names exactly this value, which is what lets the chain be
/// followed without guessing from timestamps.
pub fn page_id(path: &Path, session_id: &str) -> String {
    let stem = rollout_stem(path);
    match stem.rsplit_once('_') {
        Some((_, suffix)) if !suffix.is_empty() => suffix.to_string(),
        _ => session_id.to_string(),
    }
}

pub fn read_session_head(path: &Path) -> Result<SessionHead> {
    let file_stem = rollout_stem(path);
    let mut reader = open_rollout_reader(path)?;
    let mut line = String::new();
    let mut seen = 0usize;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut provenance = SessionProvenance::default();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || seen >= HEAD_RECORD_LIMIT {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        seen += 1;
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue;
        };
        if matches!(parse_record(&value), RecordKind::SessionMeta) {
            session_id = extract_session_id(&value).or(session_id);
            cwd = extract_cwd_hint(&value).or(cwd);
            if provenance == SessionProvenance::default() {
                provenance = extract_provenance(&value);
            }
            if session_id.is_some() && cwd.is_some() {
                break;
            }
        }
    }

    let id_source = if session_id.is_some() {
        SessionIdSource::SessionMeta
    } else {
        SessionIdSource::FilenameFallback
    };
    let session_id = session_id.unwrap_or(file_stem);
    Ok(SessionHead {
        page_id: page_id(path, &session_id),
        session_id,
        cwd_hint: cwd,
        id_source,
        provenance,
    })
}

/// Reconstruction-relevant records retained for the reverse walk, by default.
///
/// The proof is a bounded reverse scan, so retaining the whole file is waste: on a real 22 589
/// line rollout the cutoff was established from the last 358 lines. This cap keeps the analysis
/// O(window) instead of O(file) while leaving roughly three orders of magnitude of headroom.
pub const DEFAULT_SCAN_WINDOW: usize = 100_000;

/// Individual unknown-type lines named in the analysis before it switches to a summary.
pub const MAX_UNKNOWN_EXAMPLES: usize = 16;

/// Aggregate of rollout item types this build does not understand.
///
/// A transcript written by a newer Codex can contain millions of them; keeping one entry per
/// line would be echoed into the manifest and onto stdout, so only examples and the distinct
/// set are retained.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UnknownTags {
    pub count: usize,
    pub examples: Vec<(usize, String)>,
    pub distinct: Vec<String>,
}

impl UnknownTags {
    fn observe(&mut self, index: usize, tag: &str) {
        self.count += 1;
        if self.examples.len() < MAX_UNKNOWN_EXAMPLES {
            self.examples.push((index, tag.to_string()));
        }
        if !self.distinct.iter().any(|t| t == tag) {
            self.distinct.push(tag.to_string());
        }
    }
}

#[derive(Debug)]
pub struct MetadataScan {
    /// The tail of reconstruction-relevant records, capped at `window_capacity`.
    pub window: VecDeque<LineMeta>,
    pub window_capacity: usize,
    /// True when records were dropped off the front, i.e. the proof may not be able to look far
    /// enough back. Distinguishes "no cutoff exists" from "no cutoff was searched for".
    pub window_truncated: bool,
    /// Index and byte length of every `compacted` record. Bounded by how many times the
    /// conversation was compacted, not by how many lines it has.
    pub compactions: Vec<(usize, u64)>,
    pub unknown: UnknownTags,
    /// Index and byte length of the canonical `session_meta` record.
    pub session_meta: Option<(usize, u64)>,
    pub total_lines: usize,
    pub parsed_lines: usize,
    pub malformed_lines: usize,
    pub total_bytes: u64,
    /// SHA-256 of the decoded stream, computed during the scan that had to read it anyway.
    ///
    /// For a plain `.jsonl` rollout this is the file hash; for a Codex-compressed one it is the
    /// hash of the *decompressed* content, so only the plain case may substitute it for
    /// `sha256_file`. Compaction is gated on plain rollouts, which is where it is used.
    pub content_sha256: String,
}

impl MetadataScan {
    pub fn session_meta_index(&self) -> Option<usize> {
        self.session_meta.map(|(index, _)| index)
    }
}

pub fn scan_rollout_metadata(path: &Path) -> Result<MetadataScan> {
    scan_rollout_metadata_within(path, DEFAULT_SCAN_WINDOW)
}

/// Stream the rollout, retaining only what the bounded proof can actually consume.
///
/// `window` is the number of reconstruction-relevant records kept for the reverse walk;
/// `usize::MAX` disables the cap, which the differential tests use as the reference behaviour.
pub fn scan_rollout_metadata_within(path: &Path, window: usize) -> Result<MetadataScan> {
    let capacity = window.max(1);
    let mut reader = open_rollout_reader(path)?;
    let mut hasher = Sha256::new();
    let mut retained: VecDeque<LineMeta> = VecDeque::new();
    let mut window_truncated = false;
    let mut compactions = Vec::new();
    let mut unknown = UnknownTags::default();
    let mut session_meta = None;
    let mut line = String::new();
    let mut physical_index = 0usize;
    let mut parsed_lines = 0usize;
    let mut malformed_lines = 0usize;
    let mut total_bytes = 0u64;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(line.as_bytes());
        let bytes = bytes_read as u64;
        let start_offset = total_bytes;
        total_bytes = total_bytes.saturating_add(bytes);
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            match serde_json::from_str::<Value>(trimmed) {
                Ok(value) => {
                    parsed_lines += 1;
                    let kind = parse_record(&value);
                    match &kind {
                        RecordKind::SessionMeta if session_meta.is_none() => {
                            session_meta = Some((physical_index, bytes));
                        }
                        RecordKind::Compacted { .. } => compactions.push((physical_index, bytes)),
                        RecordKind::UnknownOuter { tag } => unknown.observe(physical_index, tag),
                        _ => {}
                    }
                    if record_is_relevant(&kind) {
                        if retained.len() == capacity {
                            retained.pop_front();
                            window_truncated = true;
                        }
                        retained.push_back(LineMeta {
                            physical_index,
                            start_offset,
                            bytes,
                            kind,
                        });
                    }
                }
                Err(_) => malformed_lines += 1,
            }
        }
        physical_index += 1;
    }

    Ok(MetadataScan {
        window: retained,
        window_capacity: capacity,
        window_truncated,
        compactions,
        unknown,
        session_meta,
        total_lines: physical_index,
        parsed_lines,
        malformed_lines,
        total_bytes,
        content_sha256: format!("{:x}", hasher.finalize()),
    })
}

pub fn verify_jsonl(path: &Path) -> Result<(bool, Vec<String>)> {
    let mut reader = open_rollout_reader(path)?;
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut errors = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        line_no += 1;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if serde_json::from_str::<Value>(trimmed).is_err() && errors.len() < 16 {
            errors.push(format!(
                "invalid JSON on line {}: {}",
                line_no,
                trimmed.chars().take(100).collect::<String>()
            ));
        }
    }
    Ok((errors.is_empty(), errors))
}
