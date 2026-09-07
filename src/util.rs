//! Small shared helpers: clock and human-readable sizes.

use chrono::Utc;
#[cfg(debug_assertions)]
use std::path::Path;
#[cfg(debug_assertions)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(debug_assertions)]
use std::{fs, thread};

pub const CHUNK_SIZE: usize = 1024 * 1024;
pub const HEAD_RECORD_LIMIT: usize = 32;

pub fn now_iso_utc() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn now_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Deterministic synchronization used only by debug-build race tests.
#[cfg(debug_assertions)]
pub fn test_pause(stage: &str) {
    let Ok(requested) = std::env::var("CODEX_VAULT_TEST_PAUSE_STAGE") else {
        return;
    };
    if requested != stage {
        return;
    }
    if let Ok(ready) = std::env::var("CODEX_VAULT_TEST_STAGE_READY") {
        fs::write(&ready, stage).expect("writing race-test ready marker");
    }
    let Ok(continue_path) = std::env::var("CODEX_VAULT_TEST_STAGE_CONTINUE") else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !Path::new(&continue_path).exists() {
        assert!(
            Instant::now() < deadline,
            "race-test stage `{stage}` timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(debug_assertions))]
pub fn test_pause(_stage: &str) {}

/// Crash injection for destructive-operation recovery tests. This is compiled out of release
/// builds so no environment variable can alter production behaviour.
#[cfg(debug_assertions)]
pub fn test_abort(stage: &str) {
    if std::env::var("CODEX_VAULT_TEST_ABORT_STAGE").as_deref() == Ok(stage) {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
pub fn test_abort(_stage: &str) {}

/// Deterministic I/O failure injection for filesystem error-path tests. The optional error kind
/// keeps the CLI's stable `io_error` classification while allowing tests to exercise disk-full
/// and permission-denied prose separately.
#[cfg(debug_assertions)]
pub fn test_io_fail(stage: &str) -> std::io::Result<()> {
    if std::env::var("CODEX_VAULT_TEST_IO_FAIL_STAGE").as_deref() != Ok(stage) {
        return Ok(());
    }
    let requested =
        std::env::var("CODEX_VAULT_TEST_IO_ERROR").unwrap_or_else(|_| "other".to_string());
    let kind = match requested.as_str() {
        "permission_denied" => std::io::ErrorKind::PermissionDenied,
        "storage_full" => std::io::ErrorKind::StorageFull,
        _ => std::io::ErrorKind::Other,
    };
    Err(std::io::Error::new(
        kind,
        format!("simulated {requested} failure at {stage}"),
    ))
}

#[cfg(not(debug_assertions))]
pub fn test_io_fail(_stage: &str) -> std::io::Result<()> {
    Ok(())
}
