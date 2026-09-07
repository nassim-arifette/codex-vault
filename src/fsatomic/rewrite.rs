use super::platform::create_private_file;
use crate::error::{Result, VaultError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// What one compaction pass observed, so no caller has to re-read either file to learn it.
#[derive(Debug)]
pub struct CompactionCopy {
    pub kept_lines: usize,
    pub removed_lines: usize,
    pub kept_bytes: u64,
    pub removed_bytes: u64,
    /// SHA-256 of the source as it was read during this pass.
    pub source_sha256: String,
    /// SHA-256 and length of the file just written.
    pub result_sha256: String,
    pub result_size: u64,
}

#[derive(Debug)]
pub struct PaginatedCompactionCopy {
    pub kept_lines: usize,
    pub removed_lines: usize,
    pub kept_bytes: u64,
    pub removed_bytes: u64,
    pub source_sha256: String,
    pub result_sha256: String,
    pub result_size: u64,
    /// New byte boundary corresponding to the successor's `history_base.end_byte_offset`.
    pub result_prefix_size: u64,
}

fn rewrite_history_base_offset(line: &str, offset: u64) -> Result<Vec<u8>> {
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut value: Value = serde_json::from_str(trimmed)?;
    let payload = if value.get("payload").is_some() {
        value.get_mut("payload").expect("checked payload")
    } else {
        &mut value
    };
    let history = if payload.get("history_base").is_some() {
        payload.get_mut("history_base")
    } else {
        payload
            .get_mut("meta")
            .and_then(Value::as_object_mut)
            .and_then(|m| m.get_mut("history_base"))
    };
    let Some(history) = history.and_then(Value::as_object_mut) else {
        return Err(VaultError::InvalidInput {
            reason: "expected paginated session_meta history_base while rewriting a chain page"
                .to_string(),
        });
    };
    history.insert("end_byte_offset".to_string(), Value::from(offset));
    let mut out = serde_json::to_vec(&value)?;
    out.extend_from_slice(newline.as_bytes());
    Ok(out)
}

/// Compact only the prefix consumed by the next page, preserve any bytes after that boundary,
/// and optionally rewrite this page's own predecessor offset in its canonical session_meta.
pub fn copy_compacted_paginated_page(
    src: &Path,
    dst: &Path,
    consumed_prefix_bytes: u64,
    session_meta_index: usize,
    cutoff_index: usize,
    rewritten_history_base_offset: Option<u64>,
) -> Result<PaginatedCompactionCopy> {
    let mut source_hasher = Sha256::new();
    let mut result_hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(src)?);
    let mut output = BufWriter::new(create_private_file(dst)?);
    let mut line = String::new();
    let mut physical_index = 0usize;
    let mut source_bytes = 0u64;
    let mut result_bytes = 0u64;
    let mut result_prefix_size = None;
    let mut kept_lines = 0usize;
    let mut removed_lines = 0usize;
    let mut removed_bytes = 0u64;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let n64 = n as u64;
        if source_bytes < consumed_prefix_bytes
            && source_bytes.saturating_add(n64) > consumed_prefix_bytes
        {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "paginated history byte offset {consumed_prefix_bytes} falls inside a JSONL record in {}",
                    src.display()
                ),
            });
        }
        source_hasher.update(line.as_bytes());
        let inside_prefix = source_bytes < consumed_prefix_bytes;
        let keep = !inside_prefix
            || physical_index == session_meta_index
            || physical_index >= cutoff_index;
        if keep {
            let bytes = if physical_index == session_meta_index {
                match rewritten_history_base_offset {
                    Some(offset) => rewrite_history_base_offset(&line, offset)?,
                    None => line.as_bytes().to_vec(),
                }
            } else {
                line.as_bytes().to_vec()
            };
            output.write_all(&bytes)?;
            result_hasher.update(&bytes);
            result_bytes = result_bytes.saturating_add(bytes.len() as u64);
            kept_lines += 1;
        } else {
            removed_lines += 1;
            removed_bytes = removed_bytes.saturating_add(n64);
        }
        source_bytes = source_bytes.saturating_add(n64);
        physical_index += 1;
        if source_bytes == consumed_prefix_bytes && result_prefix_size.is_none() {
            result_prefix_size = Some(result_bytes);
        }
    }
    if source_bytes < consumed_prefix_bytes || result_prefix_size.is_none() {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "paginated history byte offset {consumed_prefix_bytes} is past {}",
                src.display()
            ),
        });
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(PaginatedCompactionCopy {
        kept_lines,
        removed_lines,
        kept_bytes: result_bytes,
        removed_bytes,
        source_sha256: format!("{:x}", source_hasher.finalize()),
        result_sha256: format!("{:x}", result_hasher.finalize()),
        result_size: result_bytes,
        result_prefix_size: result_prefix_size.unwrap_or(result_bytes),
    })
}

pub fn copy_compacted_transcript(
    src: &Path,
    dst: &Path,
    session_meta_index: usize,
    cutoff_index: usize,
) -> Result<CompactionCopy> {
    let mut source_hasher = Sha256::new();
    let mut result_hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(src)?);
    let mut output = BufWriter::new(create_private_file(dst)?);
    let mut line = String::new();
    let mut physical_index = 0usize;
    let mut kept_lines = 0usize;
    let mut removed_lines = 0usize;
    let mut kept_bytes = 0u64;
    let mut removed_bytes = 0u64;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        source_hasher.update(line.as_bytes());
        let keep = physical_index == session_meta_index || physical_index >= cutoff_index;
        if keep {
            output.write_all(line.as_bytes())?;
            result_hasher.update(line.as_bytes());
            kept_lines += 1;
            kept_bytes = kept_bytes.saturating_add(n as u64);
        } else {
            removed_lines += 1;
            removed_bytes = removed_bytes.saturating_add(n as u64);
        }
        physical_index += 1;
    }
    crate::util::test_pause("compact_output");
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(CompactionCopy {
        kept_lines,
        removed_lines,
        kept_bytes,
        removed_bytes,
        source_sha256: format!("{:x}", source_hasher.finalize()),
        result_sha256: format!("{:x}", result_hasher.finalize()),
        result_size: kept_bytes,
    })
}
