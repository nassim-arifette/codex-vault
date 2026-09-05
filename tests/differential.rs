//! Differential reconstruction harness.
//!
//! Everything else in this repo tests the vault against the vault's own model of Codex. This
//! file tests the model itself: it makes Codex reconstruct a session twice — once from the
//! original transcript, once from the compacted one — and asserts that what Codex would send to
//! the model is the same both times.
//!
//! Codex is treated as a black box. The ground truth is the request body it puts on the wire,
//! captured by pointing Codex at a local mock provider (`-c model_provider=…`), so no TLS
//! interception, no API cost, and no dependence on Codex's private Rust types.
//!
//! These tests are `#[ignore]`d: they need the `codex` CLI and a corpus of real sessions.
//!
//! ```text
//! cargo test --test differential -- --ignored --nocapture
//! ```
//!
//! Cases are discovered from the live `CODEX_HOME` (read-only; every fixture is copied into a
//! throwaway sandbox before anything runs) or listed explicitly in a JSON file named by
//! `CODEX_VAULT_DIFF_CASES`.

use codex_vault::analysis::{analyze_session, CompactionAnalysis};
use codex_vault::discovery::{discover_sessions, lineage_successors, SessionInfo};
use codex_vault::ops::compact_safe_impl;
use codex_vault::paths::resolve_codex_binary;
use codex_vault::rollout::{is_codex_zstd_jsonl, read_session_head};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// How long a single `codex exec resume` may take. Large transcripts are slow to replay.
const CODEX_TIMEOUT: Duration = Duration::from_secs(300);

/// Fields that legitimately differ between two runs of the same session.
///
/// This is an allowlist on purpose: anything *not* named here that differs fails the test, so a
/// new field introduced by a future Codex cannot silently hide a real regression.
const VOLATILE_TOP_LEVEL: &[&str] = &["client_metadata"];
const VOLATILE_ITEM_FIELDS: &[&str] = &["id", "internal_chat_message_metadata_passthrough"];

// ============================================================================ capture server

/// A one-shot local provider that records what Codex sends and answers just enough to end the
/// turn.
///
/// Cutting the connection instead would make Codex retry, so the reply is a minimal but valid
/// Responses-API event stream.
struct CaptureServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CaptureServer {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_bodies = Arc::clone(&bodies);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                if let Some(body) = handle_request(stream) {
                    thread_bodies
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(body);
                }
            }
        });

        Ok(CaptureServer {
            addr,
            bodies,
            stop,
            handle: Some(handle),
        })
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.addr.port())
    }

    /// The first request body, which is the one carrying the reconstructed session.
    fn first_body(&self) -> Option<Vec<u8>> {
        self.bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .cloned()
    }

    fn request_count(&self) -> usize {
        self.bodies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the blocking `accept` so the thread can observe the stop flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_request(mut stream: TcpStream) -> Option<Vec<u8>> {
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let is_post = line.starts_with("POST");

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    let events = [
        json!({"type":"response.created","response":{"id":"resp_mock","output":[]}}),
        json!({"type":"response.output_item.done",
               "item":{"type":"message","role":"assistant",
                       "content":[{"type":"output_text","text":"ok"}]}}),
        json!({"type":"response.completed",
               "response":{"id":"resp_mock","output":[],
                           "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ];
    let names = [
        "response.created",
        "response.output_item.done",
        "response.completed",
    ];
    let mut payload = String::new();
    for (name, event) in names.iter().zip(events.iter()) {
        payload.push_str(&format!("event: {name}\ndata: {event}\n\n"));
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);

    (is_post && !body.is_empty()).then_some(body)
}

// ================================================================================== sandbox

/// A throwaway `CODEX_HOME` holding one copy of a fixture session.
///
/// Both runs of a comparison use the *same* sandbox: the developer prompt embeds absolute paths
/// (skill roots under `CODEX_HOME`, the working directory), so two parallel sandboxes would
/// differ before reconstruction is even considered.
struct DiffSandbox {
    dir: TempDir,
    session_id: String,
    session_path: PathBuf,
}

impl DiffSandbox {
    fn new(fixture: &Path, session_id: &str) -> std::io::Result<Self> {
        let dir = TempDir::new()?;
        let sessions = dir
            .path()
            .join("codex/sessions")
            .join(fixture_date_dir(fixture));
        fs::create_dir_all(&sessions)?;
        let name = fixture
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "rollout.jsonl".into());
        let session_path = sessions.join(name);
        fs::copy(fixture, &session_path)?;
        // Copy the other pages too: both the Codex oracle and Vault's lineage guard must see
        // the same isolated graph. No test operation may resolve against the live CODEX_HOME.
        for sibling in corpus_sessions()
            .iter()
            .filter(|s| s.session_id == session_id)
        {
            if sibling.path.file_name() == fixture.file_name() {
                continue;
            }
            let parent = dir
                .path()
                .join("codex/sessions")
                .join(fixture_date_dir(&sibling.path));
            fs::create_dir_all(&parent)?;
            fs::copy(
                &sibling.path,
                parent.join(sibling.path.file_name().unwrap()),
            )?;
        }
        fs::create_dir_all(dir.path().join("vault"))?;
        Ok(DiffSandbox {
            dir,
            session_id: session_id.to_string(),
            session_path,
        })
    }

    fn codex_home(&self) -> PathBuf {
        self.dir.path().join("codex")
    }

    fn vault_home(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    /// The working directory both runs share. Codex embeds it in the prompt.
    fn cwd(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn restore_from(&self, fixture: &Path) -> std::io::Result<()> {
        // Reset Codex's appended turns, index and auxiliary pages too. Both arms must start
        // from the same on-disk state, especially when comparing more than one resumed turn.
        let home = self.codex_home();
        assert!(home.starts_with(self.dir.path()));
        fs::remove_dir_all(&home)?;
        fs::create_dir_all(self.session_path.parent().unwrap())?;
        fs::copy(fixture, &self.session_path)?;
        for sibling in corpus_sessions()
            .iter()
            .filter(|s| s.session_id == self.session_id)
        {
            if sibling.path.file_name() == fixture.file_name() {
                continue;
            }
            let parent = home.join("sessions").join(fixture_date_dir(&sibling.path));
            fs::create_dir_all(&parent)?;
            fs::copy(
                &sibling.path,
                parent.join(sibling.path.file_name().unwrap()),
            )?;
        }
        Ok(())
    }
}

/// Codex stores rollouts under `sessions/YYYY/MM/DD/`. The date is in the filename.
fn fixture_date_dir(fixture: &Path) -> PathBuf {
    let name = fixture.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let rest = name.strip_prefix("rollout-").unwrap_or(name);
    let parts: Vec<&str> = rest.splitn(4, '-').collect();
    if parts.len() >= 3 && parts[0].len() == 4 {
        return PathBuf::from(parts[0])
            .join(parts[1])
            .join(parts[2].split('T').next().unwrap());
    }
    PathBuf::from("2026").join("01").join("01")
}

/// Resume the session in `sandbox` against a capture server and return the request Codex sent.
fn capture_reconstruction(sandbox: &DiffSandbox) -> Result<Value, String> {
    let codex = resolve_codex_binary().ok_or("codex executable not found")?;
    let server = CaptureServer::start().map_err(|e| format!("capture server: {e}"))?;

    let provider = format!(
        "model_providers.mock={{name=\"mock\",base_url=\"{}\",wire_api=\"responses\",env_key=\"OPENAI_API_KEY\"}}",
        server.base_url()
    );
    let mut command = Command::new(&codex);
    command
        .arg("exec")
        .arg("resume")
        .arg(&sandbox.session_id)
        .arg("ping")
        .arg("--skip-git-repo-check")
        .args(["-c", "model_provider=mock"])
        .args(["-c", &provider])
        .env("CODEX_HOME", sandbox.codex_home())
        .env("OPENAI_API_KEY", "differential-harness-mock")
        .current_dir(sandbox.cwd())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawning codex: {e}"))?;

    let deadline = std::time::Instant::now() + CODEX_TIMEOUT;
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("codex did not finish within {CODEX_TIMEOUT:?}"));
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waiting for codex: {e}")),
        }
    };
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("codex exited with {status}: {}", stderr.trim()));
    }

    let body = server.first_body().ok_or_else(|| {
        format!(
            "codex sent no request (captured {} requests). stderr:\n{}",
            server.request_count(),
            stderr.trim()
        )
    })?;
    serde_json::from_slice(&body).map_err(|e| format!("captured body is not JSON: {e}"))
}

// =============================================================================== comparison

fn strip_item_fields(item: &Value) -> Value {
    match item.as_object() {
        Some(map) => {
            let kept: Map<String, Value> = map
                .iter()
                .filter(|(k, _)| !VOLATILE_ITEM_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Value::Object(kept)
        }
        None => item.clone(),
    }
}

/// Compare two captured requests, ignoring only the explicitly volatile fields.
fn compare_requests(a: &Value, b: &Value) -> Result<usize, String> {
    let (ao, bo) = match (a.as_object(), b.as_object()) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err("a captured request was not a JSON object".to_string()),
    };

    let ak: BTreeSet<&str> = ao.keys().map(String::as_str).collect();
    let bk: BTreeSet<&str> = bo.keys().map(String::as_str).collect();
    if ak != bk {
        return Err(format!(
            "top-level keys differ: only in A {:?}, only in B {:?}",
            ak.difference(&bk).collect::<Vec<_>>(),
            bk.difference(&ak).collect::<Vec<_>>()
        ));
    }

    for key in &ak {
        if *key == "input" || VOLATILE_TOP_LEVEL.contains(key) {
            continue;
        }
        if ao[*key] != bo[*key] {
            return Err(format!(
                "top-level `{key}` differs:\n  A: {}\n  B: {}",
                truncate(&ao[*key]),
                truncate(&bo[*key])
            ));
        }
    }

    let empty = Vec::new();
    let ai = ao.get("input").and_then(Value::as_array).unwrap_or(&empty);
    let bi = bo.get("input").and_then(Value::as_array).unwrap_or(&empty);
    if ai.is_empty() {
        return Err("the reconstructed context was empty; the capture proves nothing".to_string());
    }
    if ai.len() != bi.len() {
        return Err(format!(
            "context length differs: A has {} elements, B has {}",
            ai.len(),
            bi.len()
        ));
    }
    for (index, (x, y)) in ai.iter().zip(bi.iter()).enumerate() {
        let (sx, sy) = (strip_item_fields(x), strip_item_fields(y));
        if sx != sy {
            return Err(format!(
                "context element {index} differs:\n  A: {}\n  B: {}",
                truncate(&sx),
                truncate(&sy)
            ));
        }
    }
    Ok(ai.len())
}

fn truncate(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 400 {
        format!("{}…", &s[..400])
    } else {
        s
    }
}

// ============================================================================ case discovery

#[derive(Clone, Debug)]
struct Case {
    name: String,
    session_id: String,
    path: PathBuf,
    size: u64,
    can_compact: bool,
    cutoff_index: Option<usize>,
    session_meta_index: Option<usize>,
    refusal: Option<String>,
    project: Option<String>,
}

/// Fixtures per category. Kept small so the harness stays runnable, not so small that a category
/// is represented by a single lucky transcript.
const PER_CATEGORY: usize = 3;
/// Ceiling on how much transcript the discovery pass will read looking for cases.
const DISCOVERY_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

fn discover_cases() -> Vec<Case> {
    static CASES: OnceLock<Vec<Case>> = OnceLock::new();
    CASES.get_or_init(discover_cases_uncached).clone()
}

fn corpus_sessions() -> &'static Vec<SessionInfo> {
    static CORPUS: OnceLock<Vec<SessionInfo>> = OnceLock::new();
    CORPUS.get_or_init(|| discover_sessions(None).expect("discover live corpus (read-only)"))
}

fn refusal_for(path: &Path, analysis: &CompactionAnalysis) -> Option<String> {
    let head = read_session_head(path).expect("fixture metadata");
    if is_codex_zstd_jsonl(path) {
        return Some("codex_managed_zstd".into());
    }
    if head.provenance.is_spawned_thread() {
        return Some("spawned_thread_refused".into());
    }
    if !lineage_successors(&head.session_id, &head.page_id).is_empty() {
        return Some("lineage_source_refused".into());
    }
    if analysis.can_compact && analysis.estimated_removed_bytes == Some(0) {
        return Some("already_compact".into());
    }
    None
}

fn discover_cases_uncached() -> Vec<Case> {
    if let Ok(file) = std::env::var("CODEX_VAULT_DIFF_CASES") {
        return load_cases(Path::new(&file));
    }
    let default_cases = Path::new("differential-cases.json");
    if default_cases.exists() {
        return load_cases(default_cases);
    }
    let sessions = corpus_sessions().clone();
    // Only user threads can be resumed. Codex refuses a spawned one outright:
    // "cannot resume an unloaded multi-agent v2 sub-agent through its parent". In a real corpus
    // these are the overwhelming majority, so filtering them out first — on the cheap head read
    // discovery already did — is also what keeps this pass affordable.
    let mut seen_ids = BTreeSet::new();
    let mut sessions: Vec<_> = sessions
        .into_iter()
        .filter(|s| !s.is_spawned_thread)
        // A session id that appears in several rollout files cannot be named unambiguously on
        // the `codex exec resume` command line.
        .filter(|s| seen_ids.insert(s.session_id.clone()))
        .collect();
    // Smallest first: cheap fixtures fill most categories, and the ordering is deterministic.
    sessions.sort_by(|a, b| a.size_bytes.cmp(&b.size_bytes).then(a.path.cmp(&b.path)));

    let mut buckets: Vec<(&str, Vec<Case>)> = vec![
        ("compactable", Vec::new()),
        ("multi-checkpoint", Vec::new()),
        ("large-tool-output", Vec::new()),
        ("rollback", Vec::new()),
        ("no-checkpoint", Vec::new()),
    ];
    let mut budget = 0u64;

    for info in sessions {
        if buckets.iter().all(|(_, v)| v.len() >= PER_CATEGORY) || budget > DISCOVERY_BUDGET_BYTES {
            break;
        }
        budget += info.size_bytes;
        let Ok(analysis) = analyze_session(&info.path) else {
            continue;
        };
        let reasons = analysis.reasons.join(" ").to_lowercase();
        let per_line = analysis.original_size_bytes / analysis.total_lines.max(1) as u64;

        let mut wanted: Vec<&str> = Vec::new();
        if analysis.can_compact {
            wanted.push("compactable");
            if analysis.valid_checkpoint_count > 1 {
                wanted.push("multi-checkpoint");
            }
            if per_line > 20_000 {
                wanted.push("large-tool-output");
            }
        } else if reasons.contains("rollback") {
            wanted.push("rollback");
        } else if analysis.valid_checkpoint_count == 0 && analysis.invalid_checkpoint_count == 0 {
            wanted.push("no-checkpoint");
        }

        for name in wanted {
            if let Some((_, slot)) = buckets
                .iter_mut()
                .find(|(n, v)| *n == name && v.len() < PER_CATEGORY)
            {
                slot.push(Case {
                    name: format!(
                        "{name}/{}",
                        &info.session_id[..8.min(info.session_id.len())]
                    ),
                    session_id: info.session_id.clone(),
                    path: info.path.clone(),
                    size: info.size_bytes,
                    can_compact: analysis.can_compact
                        && refusal_for(&info.path, &analysis).is_none(),
                    cutoff_index: analysis.cutoff_index,
                    session_meta_index: analysis.session_meta_index,
                    refusal: refusal_for(&info.path, &analysis),
                    project: info.cwd_hint.clone(),
                });
            }
        }
    }
    buckets.into_iter().flat_map(|(_, v)| v).collect()
}

fn load_cases(file: &Path) -> Vec<Case> {
    let text = fs::read_to_string(file).expect("read requested differential fixture list");
    let entries =
        serde_json::from_str::<Vec<Value>>(&text).expect("parse differential fixture list");
    entries
        .into_iter()
        .map(|e| {
            let path = PathBuf::from(e["path"].as_str().expect("fixture path"));
            let analysis = analyze_session(&path)
                .unwrap_or_else(|e| panic!("cannot analyze fixture {}: {e}", path.display()));
            let refusal = refusal_for(&path, &analysis);
            Case {
                name: e
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("case")
                    .to_string(),
                session_id: e["session_id"]
                    .as_str()
                    .expect("fixture session id")
                    .to_string(),
                size: fs::metadata(&path).expect("fixture size").len(),
                can_compact: analysis.can_compact && refusal.is_none(),
                cutoff_index: analysis.cutoff_index,
                session_meta_index: analysis.session_meta_index,
                refusal,
                project: read_session_head(&path).expect("fixture metadata").cwd_hint,
                path,
            }
        })
        .collect()
}

/// Write the discovered matrix out, so a run can be reproduced exactly.
fn describe(cases: &[Case]) -> String {
    let mut out = String::new();
    for c in cases {
        out.push_str(&format!(
            "  {:<28} {:>8.2} MB  compactable={}  {}\n",
            c.name,
            c.size as f64 / 1_048_576.0,
            c.can_compact,
            c.session_id
        ));
        out.push_str(&format!(
            "    project: {}\n",
            c.project.as_deref().unwrap_or("unknown")
        ));
    }
    out
}

fn require_codex() -> Option<PathBuf> {
    Some(resolve_codex_binary().expect("no codex executable: install Codex or set CODEX_VAULT_CODEX_BIN; differential validation has NOT run"))
}

#[test]
#[ignore = "requires a Codex binary"]
fn codex_discovers_the_readonly_vault_mcp_tools() {
    // Inspect Codex's actual MCP inventory. A mock model provider may omit all
    // tools from model requests, independently of successful MCP discovery.
    let codex = require_codex().unwrap();
    let sandbox = TempDir::new().unwrap();
    fs::create_dir(sandbox.path().join("codex")).unwrap();
    let executable = serde_json::to_string(env!("CARGO_BIN_EXE_codex-vault")).unwrap();
    let configuration = format!("mcp_servers.vault={{command={executable},args=[\"mcp\"],required=true,startup_timeout_sec=30}}");
    let mut process = KillOnDrop(
        Command::new(codex)
            .args(["app-server", "-c", &configuration])
            .env("CODEX_HOME", sandbox.path().join("codex"))
            .env("CODEX_VAULT_HOME", sandbox.path().join("vault"))
            .current_dir(sandbox.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start Codex app-server"),
    );
    let mut input = process.0.stdin.take().unwrap();
    let output = process.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if sender.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let await_response = |id: u64| -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let value = receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("Codex app-server response timed out");
            if value["id"] == id {
                assert!(value.get("error").is_none(), "Codex RPC error: {value}");
                return value["result"].clone();
            }
        }
    };
    writeln!(input, "{}", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"vault-test","version":"1.0"},"capabilities":{}}})).unwrap();
    input.flush().unwrap();
    await_response(1);
    writeln!(input, "{}", json!({"method":"initialized"})).unwrap();
    writeln!(
        input,
        "{}",
        json!({"id":2,"method":"mcpServerStatus/list","params":{"detail":"full"}})
    )
    .unwrap();
    input.flush().unwrap();
    let inventory = await_response(2);
    let server = inventory["data"]
        .as_array()
        .expect("MCP inventory")
        .iter()
        .find(|s| s["name"] == "vault")
        .expect("Vault server registered");
    let tools = server["tools"].as_object().expect("discovered tool map");
    for name in ["vault_search", "vault_read"] {
        assert!(
            tools
                .values()
                .any(|tool| tool["name"] == name && tool["annotations"]["readOnlyHint"] == true),
            "Missing read-only tool {name}: {server}"
        );
    }
    assert_eq!(tools.len(), 2);
    assert!(
        !sandbox.path().join("vault").exists(),
        "MCP discovery must not initialize or mutate storage"
    );
    eprintln!("Codex discovered both read-only Vault MCP tools.");
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct SandboxEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl SandboxEnv {
    fn enter(sandbox: &DiffSandbox) -> Self {
        let guard = vault_env_lock();
        let mut old = Vec::new();
        for (key, value) in [
            ("CODEX_HOME", sandbox.codex_home()),
            ("CODEX_VAULT_HOME", sandbox.vault_home()),
        ] {
            old.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { _guard: guard, old }
    }
}

impl Drop for SandboxEnv {
    fn drop(&mut self) {
        for (key, value) in &self.old {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// `CODEX_VAULT_HOME` is process-global, so the tests that point it at a sandbox take turns.
fn vault_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn compact_in_sandbox(sandbox: &DiffSandbox) -> Result<(u64, u64), String> {
    let before = fs::metadata(&sandbox.session_path)
        .map_err(|e| e.to_string())?
        .len();
    let _env = SandboxEnv::enter(sandbox);
    let result = compact_safe_impl(&sandbox.session_path);
    let result = result.map_err(|e| e.to_string())?;
    if result.status != "ok" {
        return Err(format!("compact-safe returned `{}`", result.status));
    }
    let after = fs::metadata(&sandbox.session_path)
        .map_err(|e| e.to_string())?
        .len();
    Ok((before, after))
}

// ===================================================================================== tests

#[test]
#[ignore = "needs the codex CLI and a corpus of real sessions"]
fn reconstruction_is_identical_after_compaction() {
    let Some(_codex) = require_codex() else {
        return;
    };
    let cases: Vec<Case> = discover_cases()
        .into_iter()
        .filter(|c| c.can_compact)
        .collect();
    assert!(
        !cases.is_empty(),
        "no compactable session found to test against"
    );
    println!(
        "fixture matrix:
{}",
        describe(&cases)
    );

    let mut failures = Vec::new();
    for case in &cases {
        println!(
            "--- {} ({:.2} MB, session {})",
            case.name,
            case.size as f64 / 1_048_576.0,
            case.session_id
        );
        let sandbox = DiffSandbox::new(&case.path, &case.session_id).expect("sandbox");

        let before = match capture_reconstruction(&sandbox) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: capture A failed: {e}", case.name));
                continue;
            }
        };
        let before_second = capture_reconstruction(&sandbox).expect("capture original second turn");

        // Resuming appends a turn, so the compaction must start from the pristine transcript.
        sandbox.restore_from(&case.path).expect("restore");
        let (size_before, size_after) = match compact_in_sandbox(&sandbox) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: compaction failed: {e}", case.name));
                continue;
            }
        };
        // Guard against a vacuous pass: a compaction that removed nothing proves nothing.
        assert!(
            size_after < size_before,
            "{}: compaction did not shrink the transcript",
            case.name
        );

        let after = match capture_reconstruction(&sandbox) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: capture B failed: {e}", case.name));
                continue;
            }
        };
        let after_second = capture_reconstruction(&sandbox).expect("capture compacted second turn");
        if let Err(diff) = compare_requests(&before_second, &after_second) {
            failures.push(format!("{} second resumed turn: {diff}", case.name));
        }

        match compare_requests(&before, &after) {
            Ok(elements) => println!(
                "    identical over two resumed turns: {elements} initial context elements, {:.2} MB -> {:.2} MB",
                size_before as f64 / 1_048_576.0,
                size_after as f64 / 1_048_576.0
            ),
            Err(diff) => failures.push(format!("{}: {diff}", case.name)),
        }
    }

    assert!(
        failures.is_empty(),
        "reconstruction diverged on {} of {} case(s):\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}

#[test]
#[ignore = "needs the codex CLI and a corpus of real sessions"]
fn the_harness_detects_an_over_compaction() {
    // A differential harness that has never gone red is worthless: it could be comparing a file
    // with itself. This deliberately cuts one line before the proven cutoff — removing the
    // `compacted` record that carries `replacement_history` — and requires the comparison to
    // fail.
    let Some(_codex) = require_codex() else {
        return;
    };
    let case = discover_cases()
        .into_iter()
        .find(|c| c.can_compact && c.cutoff_index.is_some())
        .expect("no compactable session found");

    println!("--- negative control on {}", case.session_id);
    let sandbox = DiffSandbox::new(&case.path, &case.session_id).expect("sandbox");
    let before = capture_reconstruction(&sandbox).expect("capture A");

    sandbox.restore_from(&case.path).expect("restore");
    over_compact(
        &case.path,
        &sandbox.session_path,
        case.session_meta_index.unwrap_or(0),
        case.cutoff_index.unwrap() + 1,
    )
    .expect("over-compaction");

    let after = capture_reconstruction(&sandbox).expect("capture B");
    match compare_requests(&before, &after) {
        Ok(n) => panic!(
            "the harness accepted an over-compacted transcript ({n} elements both sides); it \
             cannot be trusted to detect a real regression"
        ),
        Err(diff) => println!(
            "    detected, as required: {}",
            diff.lines().next().unwrap_or("")
        ),
    }
}

/// Write `src` to `dst` keeping only the session_meta line and everything from `cutoff` on.
fn over_compact(src: &Path, dst: &Path, meta: usize, cutoff: usize) -> std::io::Result<()> {
    let input = fs::File::open(src)?;
    let mut out = fs::File::create(dst)?;
    for (index, line) in BufReader::new(input).split(b'\n').enumerate() {
        let line = line?;
        if index == meta || index >= cutoff {
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()
}

#[test]
#[ignore = "needs the codex CLI and a corpus of real sessions"]
fn a_refused_session_is_left_byte_identical() {
    // For sessions the vault refuses, comparing reconstructions would pass vacuously — the file
    // was never touched. The meaningful assertion is the refusal itself.
    let cases: Vec<Case> = discover_cases()
        .into_iter()
        .filter(|c| !c.can_compact)
        .collect();
    assert!(
        !cases.is_empty(),
        "no refused session found to test against"
    );

    for case in &cases {
        let sandbox = DiffSandbox::new(&case.path, &case.session_id).expect("sandbox");
        let original = fs::read(&sandbox.session_path).expect("read");

        let env = SandboxEnv::enter(&sandbox);
        let result = compact_safe_impl(&sandbox.session_path);
        drop(env);

        if let Some(code) = &case.refusal {
            if code == "already_compact" {
                assert_eq!(result.unwrap().status, "already_compact", "{}", case.name);
            } else {
                assert_eq!(result.unwrap_err().code(), code, "{}", case.name);
            }
        } else {
            assert_eq!(
                result.expect("archive-only fallback").status,
                "archived_only",
                "{}",
                case.name
            );
        }
        assert_eq!(
            fs::read(&sandbox.session_path).expect("read"),
            original,
            "{}: a refused session must not be modified",
            case.name
        );
        println!(
            "--- {}: {} and untouched",
            case.name,
            case.refusal.as_deref().unwrap_or("archived_only")
        );
    }
}
