//! Rebuildable FTS5 index. Recovery remains independent of this derived database.
use crate::discovery::discover_sessions;
use crate::error::{Result, VaultError};
use crate::fsatomic::{lock_session, MutationGuard, TempFile};
use crate::hashing::sha256_file;
use crate::manifest::load_manifest;
use crate::paths::{normalized_path, vault_paths};
use crate::rollout::{open_rollout_reader, read_session_head};
use crate::storage::directory_bytes;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 1;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const SCHEMA: &str = "
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

fn invalid(reason: &str) -> VaultError {
    VaultError::InvalidInput {
        reason: reason.into(),
    }
}
pub fn database_path() -> PathBuf {
    vault_paths().root.join("index.sqlite")
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Normalize component boundaries, resolving existing paths but also deleted project paths.
pub fn project_key(path: &Path) -> String {
    let normalized = normalized_path(path);
    let raw = normalized.to_string_lossy().replace('\\', "/");
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "." => {}
            ".." if parts.len() > 1 => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let key = parts.join("/");
    let key = if key.len() > 1 {
        key.trim_end_matches('/').to_string()
    } else {
        key
    };
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

pub fn in_project(candidate: &str, root: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
        || root == "/" && candidate.starts_with('/')
}

#[derive(Clone)]
struct Source {
    path: PathBuf,
    kind: &'static str,
    expected_hash: Option<String>,
    title: String,
}

fn sources() -> Result<Vec<Source>> {
    let mut items = BTreeMap::new();
    for s in discover_sessions(None)? {
        items.insert(
            project_key(&s.path),
            Source {
                path: s.path,
                kind: "native",
                expected_hash: None,
                title: s.title.unwrap_or_default(),
            },
        );
    }
    let manifests = vault_paths().manifests;
    if manifests.is_dir() {
        for entry in fs::read_dir(manifests)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Some(m) = load_manifest(&path)? {
                for anchor in m.anchors() {
                    if !anchor.backup_path.is_file() {
                        return Err(VaultError::BackupMissing {
                            path: anchor.backup_path,
                        });
                    }
                    let source = Source {
                        path: anchor.backup_path,
                        kind: "backup",
                        expected_hash: Some(anchor.backup_sha256),
                        title: String::new(),
                    };
                    let key = project_key(&source.path);
                    if let Some(previous) = items.get(&key) {
                        if previous.expected_hash.is_some()
                            && previous.expected_hash != source.expected_hash
                        {
                            return Err(VaultError::Index {
                                reason: "journals disagree about an archive hash".into(),
                            });
                        }
                    }
                    items.insert(key, source);
                }
            }
        }
    }
    Ok(items.into_values().collect())
}

fn check_schema(conn: &Connection) -> Result<()> {
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

fn open_readonly() -> Result<Connection> {
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

/// A bounded line reader still counts discarded bytes so later source references stay exact.
fn next_record(reader: &mut dyn BufRead, line: &mut Vec<u8>) -> Result<Option<(u64, bool)>> {
    line.clear();
    let mut bytes = 0u64;
    let mut oversized = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok((bytes != 0).then_some((bytes, oversized)));
        }
        let take = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |n| n + 1);
        let finished = buf[take - 1] == b'\n';
        if !oversized && line.len() + take <= MAX_LINE_BYTES {
            line.extend_from_slice(&buf[..take]);
        } else {
            oversized = true;
            line.clear();
        }
        bytes += take as u64;
        reader.consume(take);
        if finished {
            return Ok(Some((bytes, oversized)));
        }
    }
}

/// Index visible dialogue, excluding tool payloads, images and instruction envelopes.
fn message_text(value: &Value) -> Vec<(String, String)> {
    let p = &value["payload"];
    match (value["type"].as_str(), p["type"].as_str()) {
        (Some("response_item"), Some("message")) => {
            let role = p["role"].as_str().unwrap_or("");
            if !["user", "assistant"].contains(&role) {
                return vec![];
            }
            if let Some(text) = p["content"].as_str() {
                return vec![(role.into(), text.into())];
            }
            p["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| match item["type"].as_str() {
                    Some("input_text" | "output_text" | "text") => {
                        item["text"].as_str().map(|s| (role.into(), s.into()))
                    }
                    _ => None,
                })
                .collect()
        }
        (Some("event_msg"), Some("user_message" | "agent_message")) => p["message"]
            .as_str()
            .map(|text| {
                vec![(
                    if p["type"] == "user_message" {
                        "user"
                    } else {
                        "assistant"
                    }
                    .into(),
                    text.into(),
                )]
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn ingest(
    conn: &Connection,
    source: &Source,
    source_id: &str,
    session: &str,
    project: &str,
) -> Result<u64> {
    let mut reader = open_rollout_reader(&source.path)?;
    let mut line = Vec::new();
    let mut line_number = 0u64;
    let mut offset = 0u64;
    let mut skipped = 0;
    while let Some((bytes, oversized)) = next_record(reader.as_mut(), &mut line)? {
        line_number += 1;
        if oversized {
            skipped += 1;
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            let value: Value = serde_json::from_slice(&line).map_err(|e| VaultError::Index {
                reason: format!("invalid JSON at source line {line_number}: {e}"),
            })?;
            for (role, text) in message_text(&value) {
                if text.trim().is_empty() {
                    continue;
                }
                let id = hash(&json!([session, project, role, text]).to_string());
                let inserted = conn.execute("INSERT OR IGNORE INTO passages(id,session_id,project,role,text) VALUES(?1,?2,?3,?4,?5)", params![id,session,project,role,text])?;
                let rowid: i64 =
                    conn.query_row("SELECT rowid FROM passages WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })?;
                if inserted != 0 {
                    conn.execute(
                        "INSERT INTO passages_fts(rowid,text) VALUES(?1,?2)",
                        params![rowid, text],
                    )?;
                }
                conn.execute("INSERT INTO occurrences(source_id,passage_rowid,line,byte_offset,record_bytes) VALUES(?1,?2,?3,?4,?5)", params![source_id,rowid,line_number as i64,offset as i64,bytes as i64])?;
            }
        }
        offset += bytes;
    }
    Ok(skipped)
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
    let skipped: i64 = conn.query_row(
        "SELECT coalesce(sum(skipped_records),0) FROM sources",
        [],
        |r| r.get(0),
    )?;
    Ok(
        json!({"status":"ok","schema_version":SCHEMA_VERSION,"sources":sources,"passages":passages,
        "occurrences":occurrences,"skipped_oversized_records":skipped,"index_bytes":fs::metadata(database_path())?.len(),
        "vault_bytes":directory_bytes(&vault_paths().root)?,"coverage":"user and assistant messages; no tool payloads or instruction envelopes"}),
    )
}

pub fn search(query: &str, cwd: Option<&Path>, limit: usize, offset: usize) -> Result<Value> {
    if query.trim().is_empty() || query.chars().count() > 512 {
        return Err(invalid("query must contain between 1 and 512 characters"));
    }
    if !(1..=100).contains(&limit) || offset > 1_000_000 {
        return Err(invalid("limit must be 1..100 and offset at most 1000000"));
    }
    // Literal tokens with AND semantics; caller input never becomes raw FTS or SQL syntax.
    let terms = query
        .split_whitespace()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ");
    let conn = open_readonly()?;
    let project = cwd
        .map(project_key)
        .map(|p| if p == "/" { String::new() } else { p });
    let sql = "SELECT p.id,p.session_id,p.project,p.role,snippet(passages_fts,0,'[',']',' … ',40),length(p.text)
        FROM passages_fts JOIN passages p ON p.rowid=passages_fts.rowid WHERE passages_fts MATCH ?1
        AND (?2 IS NULL OR p.project=?2 OR substr(p.project,1,length(?2)+1)=?2||'/')
        ORDER BY bm25(passages_fts),p.id LIMIT ?3 OFFSET ?4";
    let matches: Vec<Value> = conn.prepare(sql)?.query_map(params![terms,project,(limit+1) as i64,offset as i64], |r| {
        Ok(json!({"id":r.get::<_,String>(0)?,"session_id":r.get::<_,String>(1)?,"project":r.get::<_,String>(2)?,
            "role":r.get::<_,String>(3)?,"excerpt":r.get::<_,String>(4)?,"characters":r.get::<_,i64>(5)?}))
    })?.collect::<std::result::Result<_,_>>()?;
    let more = matches.len() > limit;
    Ok(
        json!({"status":"ok","matches":matches.into_iter().take(limit).collect::<Vec<_>>(),
        "next_offset":if more {Some(offset+limit)} else {None},"source":"indexed snapshots; run index to refresh"}),
    )
}

pub fn read(id: &str, cwd: Option<&Path>, offset: usize, limit: usize) -> Result<Value> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) || !(1..=32000).contains(&limit)
    {
        return Err(invalid(
            "use a passage id returned by search; limit must be 1..32000 characters",
        ));
    }
    let conn = open_readonly()?;
    let row: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT rowid,session_id,project,role,text FROM passages WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((rowid, session, project, role, text)) = row else {
        return Err(invalid("passage not found in this index"));
    };
    if hash(&json!([session, project, role, text]).to_string()) != id {
        return Err(VaultError::Index {
            reason: "passage content failed its identity check; rebuild the index".into(),
        });
    }
    if cwd.is_some_and(|f| !in_project(&project, &project_key(f))) {
        return Err(invalid("passage is outside the selected project"));
    }
    let references: Vec<Value> = conn.prepare("SELECT s.path,s.kind,s.hash,o.line,o.byte_offset,o.record_bytes FROM occurrences o JOIN sources s ON s.id=o.source_id WHERE o.passage_rowid=?1 ORDER BY s.kind,s.path,o.line LIMIT 100")?.query_map([rowid], |r| Ok(json!({"path":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,"sha256":r.get::<_,String>(2)?,"line":r.get::<_,i64>(3)?,"decoded_byte_offset":r.get::<_,i64>(4)?,"record_bytes":r.get::<_,i64>(5)?})))?.collect::<std::result::Result<_,_>>()?;
    let mut verified = None;
    for reference in &references {
        let path = Path::new(reference["path"].as_str().unwrap());
        if let Ok(digest) = sha256_file(path) {
            if digest == reference["sha256"] {
                verified = Some(reference.clone());
                break;
            }
        }
    }
    if verified.is_none() {
        return Err(VaultError::Index {
            reason: "backing sources have changed or disappeared; refresh the index".into(),
        });
    }
    let characters = text.chars().count();
    if offset > characters {
        return Err(invalid("offset is past the end of this passage"));
    }
    let excerpt: String = text.chars().skip(offset).take(limit).collect();
    let next = offset + excerpt.chars().count();
    Ok(
        json!({"status":"ok","id":id,"session_id":session,"project":project,"role":role,"text":excerpt,
        "character_offset":offset,"total_characters":characters,"next_offset":if next<characters {Some(next)} else {None},
        "verified_reference":verified,"references":references,"content_is_untrusted_history":true}),
    )
}
