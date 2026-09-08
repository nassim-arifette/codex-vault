//! Integration coverage for the half of the tool that can destroy data.
//!
//! Everything here drives the real operations against a throwaway `CODEX_HOME` /
//! `CODEX_VAULT_HOME`, because the invariants that matter — "the bytes come back exactly", "no
//! appended turn is ever lost", "a failed run leaves nothing behind" — only exist end to end.

use codex_vault::chain::{compact_conversation, restore_conversation};
use codex_vault::commands::{
    analyze_command, compact_conversation_command, compact_result_value, compact_safe_command,
    doctor_command, prune_command, restore_conversation_command, BatchOptions,
};
use codex_vault::error::VaultError;
use codex_vault::manifest::{load_manifest, CodexVersionSource, Status};
use codex_vault::ops::{
    archive_impl, compact_safe_impl, compact_safe_impl_with, doctor_one, prune_one, restore_impl,
    CommandResult, CompactOptions, DoctorDepth, RestoreTarget,
};
use codex_vault::parallel::ProgressMode;
use codex_vault::paths::{ensure_vault_paths, manifest_path, VaultKey};
use codex_vault::rollout::DEFAULT_SCAN_WINDOW;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------- harness
/// Batch options for tests: fixed worker count, never any progress output.
fn quiet_batch(jobs: usize) -> BatchOptions {
    BatchOptions {
        jobs,
        progress: ProgressMode::Never,
    }
}

/// `CODEX_HOME` is process-global, so tests that install one run one at a time.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Sandbox {
    dir: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = env_lock();
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("codex/sessions")).unwrap();
        fs::create_dir_all(dir.path().join("vault")).unwrap();
        std::env::set_var("CODEX_HOME", dir.path().join("codex"));
        std::env::set_var("CODEX_VAULT_HOME", dir.path().join("vault"));
        Sandbox { dir, _guard: guard }
    }

    fn sessions(&self) -> PathBuf {
        self.dir.path().join("codex/sessions")
    }

    fn vault(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    /// Write a rollout whose suffix is a provable bounded reconstruction.
    fn compactable_session(&self, name: &str, id: &str, cwd: &str) -> PathBuf {
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[]}}),
            json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t0"}}),
            json!({"type":"turn_context","payload":{"turn_id":"t0","model":"gpt"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"old"}}),
            json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t0"}}),
            json!({"type":"compacted",
                   "payload":{"replacement_history":[{"role":"user"}],"window_number":3}}),
        ];
        lines.extend(completed_turn("t1"));
        let path = self.sessions().join(name);
        write_jsonl(&path, &lines);
        path
    }

    fn backups(&self) -> Vec<String> {
        let dir = self.vault().join("backups");
        let mut out: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
}

fn completed_turn(turn: &str) -> Vec<Value> {
    vec![
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":turn}}),
        json!({"type":"turn_context","payload":{"turn_id":turn,"model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":turn}}),
    ]
}

fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn append_jsonl(path: &Path, lines: &[Value]) {
    let mut body = fs::read_to_string(path).unwrap();
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn run_crash_stage(sb: &Sandbox, args: &[&str], stage: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codex-vault"))
        .args(args)
        .env("CODEX_HOME", sb.dir.path().join("codex"))
        .env("CODEX_VAULT_HOME", sb.vault())
        .env("CODEX_VAULT_TEST_ABORT_STAGE", stage)
        .output()
        .unwrap()
}

fn run_io_failure(sb: &Sandbox, path: &Path, stage: &str, error: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codex-vault"))
        .args(["--json", "--no-progress", "compact", path.to_str().unwrap()])
        .env("CODEX_HOME", sb.dir.path().join("codex"))
        .env("CODEX_VAULT_HOME", sb.vault())
        .env("CODEX_VAULT_TEST_IO_FAIL_STAGE", stage)
        .env("CODEX_VAULT_TEST_IO_ERROR", error)
        .output()
        .unwrap()
}

fn cli_error(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "expected JSON CLI error, got parse error {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_manifest_has_no_temp_anchor(path: &Path) {
    let key = VaultKey::for_rollout(path);
    let vault = ensure_vault_paths().unwrap();
    let manifest_file = manifest_path(&vault, &key);
    if let Some(manifest) = load_manifest(&manifest_file).unwrap() {
        assert!(manifest.anchors().iter().all(|anchor| {
            !anchor
                .backup_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
        }));
    }
}

fn append_race_marker(path: &Path, marker: &str) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    writeln!(
        file,
        "{}",
        json!({"type":"event_msg","payload":{"type":"user_message","message":marker}})
    )?;
    file.sync_all()
}

fn run_compact_paused_at(
    sb: &Sandbox,
    path: &Path,
    stage: &str,
    allow_writer_races: bool,
    mutate: impl FnOnce(),
) -> Output {
    let ready = sb.dir.path().join(format!("race-{stage}.ready"));
    let go = sb.dir.path().join(format!("race-{stage}.continue"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-vault"));
    command
        .args(["--json", "--no-progress", "compact", path.to_str().unwrap()])
        .env("CODEX_HOME", sb.dir.path().join("codex"))
        .env("CODEX_VAULT_HOME", sb.vault())
        .env("CODEX_VAULT_TEST_PAUSE_STAGE", stage)
        .env("CODEX_VAULT_TEST_STAGE_READY", &ready)
        .env("CODEX_VAULT_TEST_STAGE_CONTINUE", &go)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if allow_writer_races {
        command.env("CODEX_VAULT_TEST_ALLOW_WRITER_RACES", "1");
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("compact child exited before `{stage}` pause: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "compact child never reached `{stage}` pause"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    mutate();
    fs::write(&go, b"continue").unwrap();
    child.wait_with_output().unwrap()
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every scratch file the vault could have left, anywhere it could have left one.
fn leftover_temp_files(sandbox: &Sandbox) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.filter_map(Result::ok) {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("tmp") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(sandbox.dir.path(), &mut out);
    out
}

// ---------------------------------------------------------------- round-trip invariants

fn unreadable_session(sb: &Sandbox, name: &str, id: &str, cwd: &str) -> PathBuf {
    let path = sb.sessions().join(name);
    let mut bytes =
        serde_json::to_vec(&json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}})).unwrap();
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"x\":\"");
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(b"\"}}\n");
    fs::write(&path, bytes).unwrap();
    path
}

fn session_named(sb: &Sandbox, file: &str, id: &str, cwd: &str, marker: &str) -> PathBuf {
    let mut lines = vec![
        json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":format!("HISTORY-{marker}")}]}}),
        json!({"type":"compacted",
               "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}),
    ];
    lines.extend(completed_turn(marker));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

fn paths_equal_str(recorded: &str, actual: &Path) -> bool {
    let norm = |p: &Path| {
        p.canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    };
    norm(Path::new(recorded)) == norm(actual)
}

// ============================================================== spawned threads are refused

fn spawned_session(sb: &Sandbox, file: &str, id: &str, cwd: &str, thread_source: &str) -> PathBuf {
    let mut lines = vec![json!({"type":"session_meta","payload":{
        "id": id,
        "session_id": "the-parent-thread",
        "cwd": cwd,
        "thread_source": thread_source,
        "parent_thread_id": "the-parent-thread",
        "multi_agent_version": "v2",
        "subagent_history_start_ordinal": 11
    }})];
    // Records ahead of the checkpoint, so a permitted compaction has something to remove.
    lines.extend(completed_turn("t0"));
    lines.push(json!({"type":"compacted",
                      "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}));
    lines.extend(completed_turn("t1"));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

fn lineage_page(
    sb: &Sandbox,
    file: &str,
    thread_id: &str,
    continues_from: Option<(&str, u64)>,
    marker: &str,
) -> PathBuf {
    let mut meta = json!({
        "id": thread_id,
        "session_id": thread_id,
        "cwd": "C:/work/lineage",
        "history_mode": "paginated",
        "thread_source": "user",
    });
    if let Some((source_page, offset)) = continues_from {
        meta["history_base"] = json!({
            "thread_id": source_page,
            "end_ordinal_exclusive": 42,
            "end_byte_offset": offset,
        });
    }
    let mut lines = vec![
        json!({"type": "session_meta", "payload": meta}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":format!("PAGE-{marker}")}]}}),
    ];
    lines.extend(completed_turn("t0"));
    lines.push(json!({"type":"compacted",
                      "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}));
    lines.extend(completed_turn("t1"));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

#[path = "destructive/batch.rs"]
mod batch;
#[path = "destructive/chain.rs"]
mod chain;
#[path = "destructive/crash_races.rs"]
mod crash_races;
#[path = "destructive/identity.rs"]
mod identity;
#[path = "destructive/lineage.rs"]
mod lineage;
#[path = "destructive/recovery.rs"]
mod recovery;
#[path = "destructive/spawned.rs"]
mod spawned;
