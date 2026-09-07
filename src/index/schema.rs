use crate::error::{Result, VaultError};
use crate::paths::vault_paths;
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::time::Duration;

pub(super) const SCHEMA_VERSION: i64 = 1;
pub(super) const SCHEMA: &str = "
PRAGMA foreign_keys=ON;
CREATE TABLE sources(id TEXT PRIMARY KEY, path TEXT NOT NULL, kind TEXT NOT NULL,
    hash TEXT NOT NULL, session_id TEXT NOT NULL, project TEXT NOT NULL, title TEXT NOT NULL,
    skipped_records INTEGER NOT NULL DEFAULT 0);
CREATE TABLE passages(rowid INTEGER PRIMARY KEY, id TEXT UNIQUE NOT NULL, session_id TEXT NOT NULL,
    project TEXT NOT NULL, role TEXT NOT NULL, text TEXT NOT NULL);
CREATE VIRTUAL TABLE passages_fts USING fts5(text, content='passages', content_rowid='rowid', tokenize='unicode61');
CREATE TABLE occurrences(source_id TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    passage_rowid INTEGER NOT NULL REFERENCES passages(rowid), line INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL, record_bytes INTEGER NOT NULL);
CREATE INDEX occurrences_passage ON occurrences(passage_rowid);
CREATE INDEX occurrences_source ON occurrences(source_id);
CREATE INDEX passages_project ON passages(project);
PRAGMA user_version=1;";

impl From<rusqlite::Error> for VaultError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Index {
            reason: error.to_string(),
        }
    }
}

pub fn database_path() -> PathBuf {
    vault_paths().root.join("index.sqlite")
}

pub(super) fn check_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(VaultError::Index {
            reason: format!(
                "unsupported schema {version}; rebuild with a compatible Vault version"
            ),
        });
    }
    Ok(())
}

pub(super) fn open_readonly() -> Result<Connection> {
    let path = database_path();
    if !path.is_file() {
        return Err(VaultError::Index {
            reason: "no index yet; run codex-vault index".into(),
        });
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    check_schema(&conn)?;
    Ok(conn)
}
