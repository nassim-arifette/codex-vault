use super::identity::hash;
use super::ingest::ingest;
use super::invalid;
use super::schema::{check_schema, database_path, open_readonly, SCHEMA, SCHEMA_VERSION};
use super::scope::{in_project, project_key};
use super::sources::sources;
use crate::error::{Result, VaultError};
use crate::fsatomic::{lock_session, MutationGuard, TempFile};
use crate::hashing::sha256_file;
use crate::paths::vault_paths;
use crate::rollout::read_session_head;
use crate::storage::directory_bytes;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub fn build(cwd: Option<&Path>, rebuild: bool) -> Result<Value> {
    if rebuild && cwd.is_some() {
        return Err(invalid("--rebuild rebuilds the entire corpus; omit --cwd"));
    }
    let vault = vault_paths();
    crate::paths::create_private_directory(&vault.root)?;
    let path = database_path();
    let _guard = MutationGuard::acquire(&vault.root, &vault.root)?;
    let old_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let is_new = rebuild || !path.exists();
    let temp = is_new.then(|| TempFile::beside(&path, "reindex"));
    if let Some(temp) = &temp {
        drop(crate::fsatomic::create_private_file(temp.path())?);
    }
    #[cfg(unix)]
    if path.is_file() {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    let mut conn = Connection::open(temp.as_ref().map(|t| t.path()).unwrap_or(&path))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE;")?;
    if is_new {
        conn.execute_batch(SCHEMA)?;
    } else {
        check_schema(&conn)?;
    }
    let filter = cwd.map(project_key);
    let tx = conn.transaction()?;
    let mut seen = BTreeSet::new();
    let mut changed = 0;
    let mut reused = 0;
    let mut deferred = 0;
    for source in sources()? {
        // Hold the native file against writers through both hashing and ingestion on Windows.
        let _lock = match lock_session(&source.path) {
            Ok(lock) => lock,
            Err(VaultError::SessionLocked { .. }) if source.kind == "native" && !rebuild => {
                seen.insert(hash(&project_key(&source.path)));
                deferred += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        let head = read_session_head(&source.path)?;
        let project = head
            .cwd_hint
            .as_deref()
            .map(|p| project_key(Path::new(p)))
            .unwrap_or_default();
        if filter.as_ref().is_some_and(|f| !in_project(&project, f)) {
            continue;
        }
        let id = hash(&project_key(&source.path));
        seen.insert(id.clone());
        let content_hash = sha256_file(&source.path)?;
        if source
            .expected_hash
            .as_ref()
            .is_some_and(|h| h != &content_hash)
        {
            return Err(VaultError::Index {
                reason: "archive hash differs from its recovery journal".into(),
            });
        }
        let previous: Option<(String, String)> = tx
            .query_row("SELECT hash,title FROM sources WHERE id=?1", [&id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if previous
            .as_ref()
            .is_some_and(|(h, t)| h == &content_hash && t == &source.title)
        {
            reused += 1;
            continue;
        }
        tx.execute("DELETE FROM sources WHERE id=?1", [&id])?;
        tx.execute("INSERT INTO sources(id,path,kind,hash,session_id,project,title) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id,source.path.to_string_lossy(),source.kind,content_hash,head.session_id,project,source.title])?;
        let skipped = ingest(&tx, &source, &id, &head.session_id, &project)?;
        if sha256_file(&source.path)? != content_hash {
            return Err(VaultError::SessionChanged {
                stage: "search indexing",
            });
        }
        tx.execute(
            "UPDATE sources SET skipped_records=?2 WHERE id=?1",
            params![id, skipped as i64],
        )?;
        changed += 1;
    }
    let old_sources: Vec<(String, String)> = tx
        .prepare("SELECT id,project FROM sources")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut removed = 0;
    for (id, project) in old_sources {
        if filter.as_ref().is_none_or(|f| in_project(&project, f)) && !seen.contains(&id) {
            tx.execute("DELETE FROM sources WHERE id=?1", [&id])?;
            removed += 1;
        }
    }
    tx.execute_batch("INSERT INTO passages_fts(passages_fts,rowid,text) SELECT 'delete',rowid,text FROM passages WHERE NOT EXISTS(SELECT 1 FROM occurrences WHERE passage_rowid=passages.rowid);
        DELETE FROM passages WHERE NOT EXISTS(SELECT 1 FROM occurrences WHERE passage_rowid=passages.rowid);")?;
    tx.commit()?;
    drop(conn);
    if let Some(temp) = temp {
        fs::OpenOptions::new()
            .write(true)
            .open(temp.path())?
            .sync_all()?;
        temp.commit_onto(&path)?;
    }
    let mut result = status()?;
    result["updated_sources"] = json!(changed);
    result["unchanged_sources"] = json!(reused);
    result["removed_sources"] = json!(removed);
    result["deferred_busy_sources"] = json!(deferred);
    result["index_growth_bytes"] =
        json!(result["index_bytes"].as_u64().unwrap_or(0) as i128 - old_bytes as i128);
    Ok(result)
}

pub fn status() -> Result<Value> {
    let conn = open_readonly()?;
    let sources: i64 = conn.query_row("SELECT count(*) FROM sources", [], |r| r.get(0))?;
    let passages: i64 = conn.query_row("SELECT count(*) FROM passages", [], |r| r.get(0))?;
    let occurrences: i64 = conn.query_row("SELECT count(*) FROM occurrences", [], |r| r.get(0))?;
    let indexed_text_bytes: i64 = conn.query_row(
        "SELECT coalesce(sum(length(CAST(text AS BLOB))),0) FROM passages",
        [],
        |r| r.get(0),
    )?;
    let skipped: i64 = conn.query_row(
        "SELECT coalesce(sum(skipped_records),0) FROM sources",
        [],
        |r| r.get(0),
    )?;
    let index_bytes = fs::metadata(database_path())?.len();
    let duplicate_occurrences = occurrences.saturating_sub(passages).max(0);
    let deduplication_ratio = if occurrences > 0 {
        duplicate_occurrences as f64 / occurrences as f64
    } else {
        0.0
    };
    let index_to_indexed_text_ratio = if indexed_text_bytes > 0 {
        index_bytes as f64 / indexed_text_bytes as f64
    } else {
        0.0
    };
    Ok(
        json!({"status":"ok","schema_version":SCHEMA_VERSION,"sources":sources,"passages":passages,
        "occurrences":occurrences,"duplicate_occurrences_without_duplicate_body":duplicate_occurrences,
        "deduplication_ratio":deduplication_ratio,"indexed_text_bytes":indexed_text_bytes,
        "index_to_indexed_text_ratio":index_to_indexed_text_ratio,
        "skipped_oversized_records":skipped,"index_bytes":index_bytes,
        "vault_bytes":directory_bytes(&vault_paths().root)?,"coverage":"user and assistant messages; no tool payloads or instruction envelopes"}),
    )
}
