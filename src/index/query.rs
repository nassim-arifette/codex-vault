use super::identity::hash;
use super::invalid;
use super::schema::open_readonly;
use super::scope::{in_project, project_key};
use crate::error::{Result, VaultError};
use crate::hashing::sha256_file;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

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
