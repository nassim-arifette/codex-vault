use super::identity::hash;
use super::ingest::ingest;
use super::invalid;
use super::schema::{check_schema, database_path, open_readonly, SCHEMA, SCHEMA_VERSION};
use super::scope::{in_project, project_key};
use super::sources::sources;
use crate::discovery::session_titles;
use crate::error::{Result, VaultError};
use crate::fsatomic::{lock_session, MutationGuard, TempFile};
use crate::hashing::{sha256_file, sha256_reader};
use crate::paths::vault_paths;
use crate::rollout::read_session_head;
use crate::storage::directory_bytes;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug)]
struct PreviousSource {
    hash: String,
    title: String,
    session_id: String,
    project: String,
}

impl PreviousSource {
    /// Content identity is SHA-256, never a size/mtime fingerprint.
    ///
    /// Files can be rewritten in place with identical length and an mtime restored to its previous
    /// value. A requested index refresh must still notice that content change, so reuse is gated on
    /// a freshly computed SHA-256. Do not introduce a metadata-only short circuit here without a
    /// platform change token whose integrity semantics are independently proven.
    fn content_is_unchanged(&self, current_hash: &str) -> bool {
        self.hash == current_hash
    }
}

#[derive(Clone, Copy, Debug)]
struct IndexStats {
    sources: i64,
    passages: i64,
    occurrences: i64,
    indexed_text_bytes: i64,
    skipped: i64,
}

fn hash_locked_source(file: &mut File) -> Result<String> {
    sha256_reader(file)
}

fn previous_sources(conn: &Connection) -> Result<HashMap<String, PreviousSource>> {
    let mut statement = conn.prepare("SELECT id,hash,title,session_id,project FROM sources")?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            PreviousSource {
                hash: r.get(1)?,
                title: r.get(2)?,
                session_id: r.get(3)?,
                project: r.get(4)?,
            },
        ))
    })?;
    rows.collect::<std::result::Result<HashMap<_, _>, _>>()
        .map_err(Into::into)
}

fn collect_index_stats(conn: &Connection) -> Result<IndexStats> {
    conn.query_row(
        "WITH
            source_stats AS (
                SELECT count(*) AS sources,
                       coalesce(sum(skipped_records),0) AS skipped
                FROM sources
            ),
            passage_stats AS (
                SELECT count(*) AS passages,
                       coalesce(sum(length(CAST(text AS BLOB))),0) AS indexed_text_bytes
                FROM passages
            ),
            occurrence_stats AS (
                SELECT count(*) AS occurrences FROM occurrences
            )
         SELECT sources,passages,occurrences,indexed_text_bytes,skipped
         FROM source_stats,passage_stats,occurrence_stats",
        [],
        |r| {
            Ok(IndexStats {
                sources: r.get(0)?,
                passages: r.get(1)?,
                occurrences: r.get(2)?,
                indexed_text_bytes: r.get(3)?,
                skipped: r.get(4)?,
            })
        },
    )
    .map_err(Into::into)
}

fn status_value(stats: IndexStats, index_bytes: u64, vault_bytes: u64) -> Value {
    let duplicate_occurrences = stats.occurrences.saturating_sub(stats.passages).max(0);
    let deduplication_ratio = if stats.occurrences > 0 {
        duplicate_occurrences as f64 / stats.occurrences as f64
    } else {
        0.0
    };
    let index_to_indexed_text_ratio = if stats.indexed_text_bytes > 0 {
        index_bytes as f64 / stats.indexed_text_bytes as f64
    } else {
        0.0
    };
    json!({"status":"ok","schema_version":SCHEMA_VERSION,"sources":stats.sources,"passages":stats.passages,
        "occurrences":stats.occurrences,"duplicate_occurrences_without_duplicate_body":duplicate_occurrences,
        "deduplication_ratio":deduplication_ratio,"indexed_text_bytes":stats.indexed_text_bytes,
        "index_to_indexed_text_ratio":index_to_indexed_text_ratio,
        "skipped_oversized_records":stats.skipped,"index_bytes":index_bytes,
        "vault_bytes":vault_bytes,"coverage":"user and assistant messages; no tool payloads or instruction envelopes"})
}

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
    let titles = session_titles();
    let tx = conn.transaction()?;
    let previous = previous_sources(&tx)?;
    let mut seen = HashSet::with_capacity(previous.len());
    let mut changed = 0;
    let mut reused = 0;
    let mut deferred = 0;
    for source in sources()? {
        let id = hash(&source.key);
        // Hold the native file against writers through both hashing and ingestion on Windows.
        let mut locked = match lock_session(&source.path) {
            Ok(lock) => lock,
            Err(VaultError::SessionLocked { .. }) if source.kind == "native" && !rebuild => {
                seen.insert(id);
                deferred += 1;
                continue;
            }
            Err(err) => return Err(err),
        };

        // Deliberately hash instead of trusting a fingerprint. `size + mtime` cannot distinguish a
        // same-size in-place rewrite whose timestamp was restored, and `index` is an explicit
        // refresh request rather than an opportunistic background cache check.
        // Hash the already-locked handle rather than reopening the path. On a no-op refresh this
        // leaves one open/read pass per source while preserving exact SHA-256 checks.
        let content_hash = hash_locked_source(&mut locked)?;
        if source
            .expected_hash
            .as_ref()
            .is_some_and(|h| h != &content_hash)
        {
            return Err(VaultError::Index {
                reason: "archive hash differs from its recovery journal".into(),
            });
        }
        if let Some(previous) = previous.get(&id) {
            if previous.content_is_unchanged(&content_hash) {
                if filter
                    .as_ref()
                    .is_some_and(|f| !in_project(&previous.project, f))
                {
                    continue;
                }
                seen.insert(id.clone());
                let title = if source.kind == "native" {
                    titles
                        .get(&previous.session_id)
                        .cloned()
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                if title != previous.title {
                    tx.execute(
                        "UPDATE sources SET title=?2 WHERE id=?1",
                        params![id, title],
                    )?;
                }
                reused += 1;
                continue;
            }
        }

        let head = read_session_head(&source.path)?;
        let project = head
            .cwd_hint
            .as_deref()
            .map(|p| project_key(Path::new(p)))
            .unwrap_or_default();
        if filter.as_ref().is_some_and(|f| !in_project(&project, f)) {
            continue;
        }
        seen.insert(id.clone());
        let title = if source.kind == "native" {
            titles.get(&head.session_id).cloned().unwrap_or_default()
        } else {
            String::new()
        };
        tx.execute("DELETE FROM sources WHERE id=?1", [&id])?;
        tx.execute("INSERT INTO sources(id,path,kind,hash,session_id,project,title) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id,source.path.to_string_lossy(),source.kind,content_hash,head.session_id,project,title])?;
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
    let mut removed = 0;
    for (id, previous) in &previous {
        if filter
            .as_ref()
            .is_none_or(|f| in_project(&previous.project, f))
            && !seen.contains(id)
        {
            tx.execute("DELETE FROM sources WHERE id=?1", [id])?;
            removed += 1;
        }
    }
    tx.execute_batch("INSERT INTO passages_fts(passages_fts,rowid,text) SELECT 'delete',rowid,text FROM passages WHERE NOT EXISTS(SELECT 1 FROM occurrences WHERE passage_rowid=passages.rowid);
        DELETE FROM passages WHERE NOT EXISTS(SELECT 1 FROM occurrences WHERE passage_rowid=passages.rowid);")?;
    tx.commit()?;
    // Reuse the already-open connection for status aggregation. The standalone `status` command
    // still opens read-only, but an index refresh no longer reopens SQLite and executes five
    // independent aggregate statements after committing the same database.
    let stats = collect_index_stats(&conn)?;
    drop(conn);
    if let Some(temp) = temp {
        fs::OpenOptions::new()
            .write(true)
            .open(temp.path())?
            .sync_all()?;
        temp.commit_onto(&path)?;
    }
    let index_bytes = fs::metadata(&path)?.len();
    // `vault_bytes` is part of the existing public status contract and means exact logical bytes
    // for the whole Vault. Keeping it exact still requires one recursive inventory here; removing
    // that scan would need an explicit output-contract change or durable accounting index.
    let vault_bytes = directory_bytes(&vault.root)?;
    let mut result = status_value(stats, index_bytes, vault_bytes);
    result["updated_sources"] = json!(changed);
    result["unchanged_sources"] = json!(reused);
    result["removed_sources"] = json!(removed);
    result["deferred_busy_sources"] = json!(deferred);
    result["refresh_integrity"] = json!({
        "content_identity": "sha256",
        "fingerprint": "not_used_for_content_identity"
    });
    result["index_growth_bytes"] =
        json!(result["index_bytes"].as_u64().unwrap_or(0) as i128 - old_bytes as i128);
    Ok(result)
}

pub fn status() -> Result<Value> {
    let conn = open_readonly()?;
    let stats = collect_index_stats(&conn)?;
    let index_bytes = fs::metadata(database_path())?.len();
    let vault_bytes = directory_bytes(&vault_paths().root)?;
    Ok(status_value(stats, index_bytes, vault_bytes))
}

#[cfg(test)]
mod tests {
    use super::PreviousSource;

    fn previous(hash: &str) -> PreviousSource {
        PreviousSource {
            hash: hash.into(),
            title: String::new(),
            session_id: "session".into(),
            project: "project".into(),
        }
    }

    #[test]
    fn content_hash_is_the_only_reuse_identity() {
        let old = previous("old-content-hash");
        assert!(!old.content_is_unchanged("new-content-hash"));
        assert!(old.content_is_unchanged("old-content-hash"));
    }
}
