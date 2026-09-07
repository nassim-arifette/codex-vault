use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandStatus {
    Ok,
    Exists,
    SnapshotCreated,
    Preview,
    AlreadyCompact,
    ArchivedOnly,
    VerificationFailed,
    RestoredAfterFailedVerification,
    Failed,
    Other,
}

#[derive(Debug, Serialize)]
pub struct CommandResult {
    pub status: String,
    pub session: String,
    pub manifest: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub reason: Vec<String>,
    pub stats: Value,
}

impl CommandResult {
    pub(crate) fn status_kind(&self) -> CommandStatus {
        match self.status.as_str() {
            "ok" => CommandStatus::Ok,
            "exists" => CommandStatus::Exists,
            "snapshot_created" => CommandStatus::SnapshotCreated,
            "preview" => CommandStatus::Preview,
            "already_compact" => CommandStatus::AlreadyCompact,
            "archived_only" => CommandStatus::ArchivedOnly,
            "verification_failed" => CommandStatus::VerificationFailed,
            "restored_after_failed_verification" => CommandStatus::RestoredAfterFailedVerification,
            "failed" => CommandStatus::Failed,
            _ => CommandStatus::Other,
        }
    }

    pub(crate) fn recovery_source_created(&self) -> bool {
        self.stats["recovery_source_created"].as_bool() == Some(true)
    }

    pub(crate) fn pre_restore_backup_created(&self) -> bool {
        self.stats["pre_restore_backup"].is_string()
    }
}
