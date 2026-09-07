use super::identity::hash;
use super::sources::Source;
use crate::error::{Result, VaultError};
use crate::rollout::open_rollout_reader;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::io::BufRead;

const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

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

pub(super) fn ingest(
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
