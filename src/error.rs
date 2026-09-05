//! A typed error domain for the vault.
//!
//! Every failure carries a stable machine-readable `code` and a process exit class, so the
//! CLI can emit a JSON error document instead of a `Debug`-formatted `io::Error`. Integrity
//! failures are deliberately a distinct class from plain I/O: a caller scripting `compact-safe`
//! must be able to tell "the disk was busy" from "a hash did not match".

use serde_json::json;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, VaultError>;

/// Process exit classes. Kept small and stable; scripts branch on these.
pub mod exit {
    /// Unexpected internal invariant violation.
    pub const INTERNAL: u8 = 1;
    /// The request itself was invalid (bad arguments, unsupported session kind).
    pub const USAGE: u8 = 2;
    /// The requested session or backup does not exist.
    pub const NOT_FOUND: u8 = 3;
    /// A hash, size or JSON check failed. Nothing was left in a worse state.
    pub const INTEGRITY: u8 = 4;
    /// The session is in use, or changed underneath us.
    pub const BUSY: u8 = 5;
    /// Underlying filesystem error.
    pub const IO: u8 = 6;
}

#[derive(Debug)]
pub enum VaultError {
    /// An error after atomic replacement. The prepared recovery journal remains authoritative.
    AfterReplacement {
        source: Box<VaultError>,
        manifest: PathBuf,
    },
    Io {
        path: Option<PathBuf>,
        context: &'static str,
        source: io::Error,
    },
    Json {
        path: Option<PathBuf>,
        context: &'static str,
        source: serde_json::Error,
    },
    SessionNotFound {
        reference: String,
    },
    AmbiguousSession {
        reference: String,
        matches: Vec<PathBuf>,
    },
    NotPlainJsonl {
        path: PathBuf,
    },
    CodexManagedZstd {
        path: PathBuf,
    },
    SessionLocked {
        path: PathBuf,
        source: io::Error,
    },
    /// The transcript changed underneath an operation that had already verified it.
    SessionChanged {
        stage: &'static str,
    },
    /// A hash or size did not match what the recovery journal recorded.
    IntegrityMismatch {
        what: &'static str,
        expected: String,
        actual: String,
    },
    /// The manifest is missing, unparseable, or missing a field a safety check requires.
    ManifestInvalid {
        path: PathBuf,
        reason: String,
    },
    BackupMissing {
        path: PathBuf,
    },
    /// A destructive batch was requested without a scope.
    RefusedImplicitBatch,
    /// The rollout belongs to a thread Codex spawned, whose compaction has not been proven safe.
    SpawnedThreadRefused {
        path: PathBuf,
        thread_source: Option<String>,
    },
    /// Another page of the same thread continues from this rollout at a byte offset.
    LineageSourceRefused {
        path: PathBuf,
        successors: Vec<PathBuf>,
    },
    /// Two conflicting session references were given for the same command.
    ConflictingArguments {
        detail: &'static str,
    },
    /// An invariant established by `analyze` no longer holds. Indicates a bug, not bad input.
    Internal {
        detail: &'static str,
    },
}

impl VaultError {
    pub fn code(&self) -> &'static str {
        match self {
            VaultError::AfterReplacement { source, .. } => source.code(),
            VaultError::Io { .. } => "io_error",
            VaultError::Json { .. } => "json_error",
            VaultError::SessionNotFound { .. } => "session_not_found",
            VaultError::AmbiguousSession { .. } => "ambiguous_session",
            VaultError::NotPlainJsonl { .. } => "not_a_rollout",
            VaultError::CodexManagedZstd { .. } => "codex_managed_zstd",
            VaultError::SessionLocked { .. } => "session_locked",
            VaultError::SessionChanged { .. } => "session_changed",
            VaultError::IntegrityMismatch { .. } => "integrity_mismatch",
            VaultError::ManifestInvalid { .. } => "manifest_invalid",
            VaultError::BackupMissing { .. } => "backup_missing",
            VaultError::RefusedImplicitBatch => "refused_implicit_batch",
            VaultError::SpawnedThreadRefused { .. } => "spawned_thread_refused",
            VaultError::LineageSourceRefused { .. } => "lineage_source_refused",
            VaultError::ConflictingArguments { .. } => "conflicting_arguments",
            VaultError::Internal { .. } => "internal_error",
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            VaultError::AfterReplacement { source, .. } => source.exit_code(),
            VaultError::Io { .. } => exit::IO,
            VaultError::Json { .. } => exit::INTEGRITY,
            VaultError::SessionNotFound { .. } | VaultError::BackupMissing { .. } => {
                exit::NOT_FOUND
            }
            VaultError::AmbiguousSession { .. }
            | VaultError::NotPlainJsonl { .. }
            | VaultError::CodexManagedZstd { .. }
            | VaultError::RefusedImplicitBatch
            | VaultError::SpawnedThreadRefused { .. }
            | VaultError::LineageSourceRefused { .. }
            | VaultError::ConflictingArguments { .. } => exit::USAGE,
            VaultError::SessionLocked { .. } | VaultError::SessionChanged { .. } => exit::BUSY,
            VaultError::IntegrityMismatch { .. } | VaultError::ManifestInvalid { .. } => {
                exit::INTEGRITY
            }
            VaultError::Internal { .. } => exit::INTERNAL,
        }
    }

    /// Structured fields for the JSON error document, so scripts never have to parse prose.
    pub fn details(&self) -> serde_json::Value {
        match self {
            VaultError::AfterReplacement { source, manifest } => {
                json!({"cause": source.details(), "recovery_manifest": manifest})
            }
            VaultError::Io { path, context, .. } | VaultError::Json { path, context, .. } => {
                json!({"path": path.as_ref().map(|p| p.to_string_lossy()), "while": context})
            }
            VaultError::SessionNotFound { reference } => json!({ "reference": reference }),
            VaultError::AmbiguousSession { reference, matches } => json!({
                "reference": reference,
                "matches": matches.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            }),
            VaultError::NotPlainJsonl { path }
            | VaultError::CodexManagedZstd { path }
            | VaultError::BackupMissing { path } => {
                json!({ "path": path.to_string_lossy() })
            }
            VaultError::SessionLocked { path, .. } => json!({ "path": path.to_string_lossy() }),
            VaultError::SessionChanged { stage } => json!({ "stage": stage }),
            VaultError::IntegrityMismatch {
                what,
                expected,
                actual,
            } => json!({"what": what, "expected": expected, "actual": actual}),
            VaultError::ManifestInvalid { path, reason } => {
                json!({"path": path.to_string_lossy(), "reason": reason})
            }
            VaultError::RefusedImplicitBatch => json!({}),
            VaultError::SpawnedThreadRefused {
                path,
                thread_source,
            } => json!({"path": path.to_string_lossy(), "thread_source": thread_source}),
            VaultError::LineageSourceRefused { path, successors } => json!({
                "path": path.to_string_lossy(),
                "continued_by": successors.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            }),
            VaultError::ConflictingArguments { detail } => json!({ "detail": detail }),
            VaultError::Internal { detail } => json!({ "detail": detail }),
        }
    }

    /// True when the native transcript is guaranteed untouched by the failed operation.
    pub fn transcript_untouched(&self) -> bool {
        !matches!(
            self,
            VaultError::Internal { .. } | VaultError::AfterReplacement { .. }
        )
    }

    pub fn after_replacement(self, manifest: &Path) -> Self {
        Self::AfterReplacement {
            source: Box::new(self),
            manifest: manifest.to_path_buf(),
        }
    }

    pub fn io(context: &'static str, path: &Path, source: io::Error) -> Self {
        VaultError::Io {
            path: Some(path.to_path_buf()),
            context,
            source,
        }
    }

    pub fn json(context: &'static str, path: &Path, source: serde_json::Error) -> Self {
        VaultError::Json {
            path: Some(path.to_path_buf()),
            context,
            source,
        }
    }

    pub fn mismatch(
        what: &'static str,
        expected: impl fmt::Display,
        actual: impl fmt::Display,
    ) -> Self {
        VaultError::IntegrityMismatch {
            what,
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VaultError::AfterReplacement { source, manifest } => write!(f, "{source}; transcript was replaced; recovery journal: {}. Run doctor then restore if needed", manifest.display()),
            VaultError::Io {
                path,
                context,
                source,
            } => match path {
                Some(p) => write!(f, "{context} ({}): {source}", p.display()),
                None => write!(f, "{context}: {source}"),
            },
            VaultError::Json {
                path,
                context,
                source,
            } => match path {
                Some(p) => write!(f, "{context} ({}): {source}", p.display()),
                None => write!(f, "{context}: {source}"),
            },
            VaultError::SessionNotFound { reference } => {
                write!(f, "no session matching `{reference}`")
            }
            VaultError::AmbiguousSession { reference, matches } => write!(
                f,
                "multiple sessions match `{reference}`: {}",
                matches
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            VaultError::NotPlainJsonl { path } => {
                write!(f, "{} is not a .jsonl rollout", path.display())
            }
            VaultError::CodexManagedZstd { path } => write!(
                f,
                "{} is managed as .jsonl.zst by Codex; v0.1 treats Codex-compressed rollouts as \
                 read-only. Rematerialize it in Codex before archive/compact/restore.",
                path.display()
            ),
            VaultError::SessionLocked { path, source } => write!(
                f,
                "cannot obtain write-exclusive access to {} (session may still be open in Codex): \
                 {source}",
                path.display()
            ),
            VaultError::SessionChanged { stage } => {
                write!(f, "session changed during {stage}; the transcript was not modified")
            }
            VaultError::IntegrityMismatch {
                what,
                expected,
                actual,
            } => write!(f, "{what}: expected {expected}, got {actual}"),
            VaultError::ManifestInvalid { path, reason } => {
                write!(f, "manifest {} is invalid: {reason}", path.display())
            }
            VaultError::BackupMissing { path } => {
                write!(f, "backup not found: {}", path.display())
            }
            VaultError::RefusedImplicitBatch => write!(
                f,
                "refusing to compact every Codex session implicitly; pass --session <id> or \
                 --cwd <path>"
            ),
            VaultError::SpawnedThreadRefused {
                path,
                thread_source,
            } => write!(
                f,
                "{} belongs to a `{}` thread Codex spawned, not a user thread. Codex refuses to \
                 resume such a rollout on its own, so compacting it cannot be validated against \
                 Codex's own reconstruction, and its history may be replayed through its parent. \
                 Pass --allow-spawned-threads to override.",
                path.display(),
                thread_source.as_deref().unwrap_or("spawned")
            ),
            VaultError::LineageSourceRefused { path, successors } => write!(
                f,
                "{} is not the newest page of its thread: {} other rollout(s) continue from it at \
                 a recorded byte offset. Shortening it would make Codex refuse to resume the \
                 whole thread (\"cutoff byte offset is past the source rollout\"). Only the \
                 newest page of a paginated thread can be compacted.",
                path.display(),
                successors.len()
            ),
            VaultError::ConflictingArguments { detail } => write!(f, "{detail}"),
            VaultError::Internal { detail } => write!(f, "internal invariant violated: {detail}"),
        }
    }
}

impl std::error::Error for VaultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VaultError::AfterReplacement { source, .. } => Some(source),
            VaultError::Io { source, .. } | VaultError::SessionLocked { source, .. } => {
                Some(source)
            }
            VaultError::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Bare `?` on an `io::Error` is still accepted so that plumbing stays readable; call sites that
/// know which file they touched use [`VaultError::io`] to attach the path.
impl From<io::Error> for VaultError {
    fn from(source: io::Error) -> Self {
        VaultError::Io {
            path: None,
            context: "filesystem operation",
            source,
        }
    }
}

impl From<serde_json::Error> for VaultError {
    fn from(source: serde_json::Error) -> Self {
        VaultError::Json {
            path: None,
            context: "JSON serialization",
            source,
        }
    }
}

impl VaultError {
    /// The JSON document printed on stderr when a command fails.
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "status": "error",
            "code": self.code(),
            "message": self.to_string(),
            "details": self.details(),
            "native_transcript_changed": !self.transcript_untouched(),
        })
    }
}
