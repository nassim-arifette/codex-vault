//! Exercise the shipped executable, including its exit contract and interactive decisions.
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;
mod common;

#[test]
fn long_lifecycle_keeps_every_recovery_generation_reachable_and_exact() {
    let sb = CliSandbox::new();
    common::seed(&sb.session, "cli-test", &sb.dir.path().join("project"));
    common::lifecycle(
        &sb.session,
        &sb.dir.path().join("codex"),
        &sb.dir.path().join("vault"),
        &sb.dir.path().join("project"),
    );
}

struct CliSandbox {
    dir: TempDir,
    session: PathBuf,
}

impl CliSandbox {
    fn ok(&self, args: &[&str]) -> Value {
        let result = self.run(args, None);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        Self::value(&result)
    }

    fn add_historical_message(&self) {
        let mut lines: Vec<Value> = fs::read_to_string(&self.session)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        lines.insert(1, json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Decision: authentication uses rotating refresh tokens. Café 🦀"}]}}));
        fs::write(
            &self.session,
            lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
    }
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("codex/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let session = sessions.join("rollout-cli-test.jsonl");
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":"cli-test","cwd":"C:/sample project","cli_version":"0.152.1"}}),
        ];
        for turn in ["old", "new"] {
            if turn == "new" {
                lines.push(json!({"type":"compacted","payload":{"replacement_history":[{"role":"user"}],"window_number":3}}));
            }
            lines.extend([
                json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":turn}}),
                json!({"type":"turn_context","payload":{"turn_id":turn,"model":"gpt"}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
                json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":turn}}),
            ]);
        }
        fs::write(
            &session,
            lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        fs::write(
            dir.path().join("codex/session_index.jsonl"),
            format!(
                "{}\n",
                json!({"id":"cli-test","thread_name":"Test conversation"})
            ),
        )
        .unwrap();
        Self { dir, session }
    }
    fn run(&self, args: &[&str], input: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-vault"));
        command
            .args(args)
            .env("CODEX_HOME", self.dir.path().join("codex"))
            .env("CODEX_VAULT_HOME", self.dir.path().join("vault"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        } else {
            drop(child.stdin.take());
        }
        child.wait_with_output().unwrap()
    }
    fn value(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn scan_readable_output_limits_and_sorts_without_truncating_json() {
    let sb = CliSandbox::new();
    let mut titles = fs::OpenOptions::new()
        .append(true)
        .open(sb.dir.path().join("codex/session_index.jsonl"))
        .unwrap();
    for rank in [3, 1, 6, 2, 5, 4] {
        let id = format!("scan-{rank}");
        let meta =
            json!({"type":"session_meta","payload":{"id":id,"cwd":"C:\\projects\\sample-app"}});
        let message = json!({"type":"event_msg","payload":{"type":"user_message","message":"x".repeat(rank * 4096)}});
        fs::write(
            sb.session
                .parent()
                .unwrap()
                .join(format!("rollout-{id}.jsonl")),
            format!("{meta}\n{message}\n"),
        )
        .unwrap();
        writeln!(
            titles,
            "{}",
            json!({"id":id,"thread_name":format!("Conversation {rank}")})
        )
        .unwrap();
    }
    let output = sb.run(&["--human", "scan"], None);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("7 conversation files"));
    assert!(text.contains("Showing 5 of 7, largest first."));
    assert_eq!(text.matches("Ref:").count(), 5);
    assert!(text.contains("2 more files. Use --all"));
    assert!(text.contains("sample-app"));
    assert!(!text.contains("C:\\projects"));
    assert!(!text.contains("rollout-scan"));
    assert!(!text.contains("Conversation 1"));
    assert!(!text.contains("Test conversation"));
    let positions: Vec<_> = (2..=6)
        .rev()
        .map(|rank| text.find(&format!("Conversation {rank}")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    let output = sb.run(&["scan", "--human", "--all"], None);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Showing 7 of 7, largest first."));
    assert_eq!(text.matches("Ref:").count(), 7);
    assert!(text.contains("Conversation 1"));
    assert!(text.contains("Test conversation"));
    assert!(!text.contains("more files"));

    let output = sb.run(&["--human", "scan", "--paths"], None);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Showing 5 of 7"));
    assert!(text.contains("Project: C:\\projects\\sample-app"));
    let displayed_path = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Path: "))
        .unwrap();
    assert_eq!(
        PathBuf::from(displayed_path).canonicalize().unwrap(),
        sb.session
            .parent()
            .unwrap()
            .join("rollout-scan-6.jsonl")
            .canonicalize()
            .unwrap()
    );

    let redirected = sb.ok(&["scan"]);
    let explicit = sb.ok(&["--json", "scan"]);
    let with_flags = sb.ok(&["--json", "scan", "--all", "--paths"]);
    assert_eq!(redirected["sessions"].as_array().unwrap().len(), 7);
    assert_eq!(redirected["sessions"], explicit["sessions"]);
    assert_eq!(explicit["sessions"], with_flags["sessions"]);
}

#[cfg(unix)]
#[test]
fn unix_recovery_preserves_transcript_mode_and_keeps_vault_content_private() {
    use std::os::unix::fs::PermissionsExt;
    let sb = CliSandbox::new();
    fs::set_permissions(&sb.session, fs::Permissions::from_mode(0o640)).unwrap();
    let original = fs::read(&sb.session).unwrap();
    sb.ok(&["index"]);
    let vault = sb.dir.path().join("vault");
    assert_eq!(
        fs::metadata(&vault).unwrap().permissions().mode() & 0o777,
        0o700
    );
    sb.ok(&["archive", "cli-test"]);
    sb.ok(&["compact", "cli-test"]);
    assert_eq!(
        fs::metadata(&sb.session).unwrap().permissions().mode() & 0o777,
        0o640
    );
    sb.ok(&["restore", "cli-test", "--original"]);
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    assert_eq!(
        fs::metadata(&sb.session).unwrap().permissions().mode() & 0o777,
        0o640
    );
    sb.ok(&["index", "--rebuild"]);
    for entry in walkdir::WalkDir::new(&vault) {
        let entry = entry.unwrap();
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o077,
            0,
            "Vault entry must not be accessible to group/other: {}",
            entry.path().display()
        );
    }
}

#[test]
fn scan_references_disambiguate_pages_and_empty_filters_are_clear() {
    let sb = CliSandbox::new();
    let other = sb
        .session
        .parent()
        .unwrap()
        .join("rollout-another-page.jsonl");
    fs::copy(&sb.session, &other).unwrap();
    let output = sb.run(&["--human", "scan"], None);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Ref: rollout-cli-test"));
    assert!(text.contains("Ref: rollout-another-page"));
    assert!(!text.contains("Ref: cli-test"));
    // Equal-size pages have a deterministic path order; either displayed reference resolves.
    assert!(text.find("Ref: rollout-another-page") < text.find("Ref: rollout-cli-test"));
    sb.ok(&["analyze", "rollout-cli-test"]);
    sb.ok(&["analyze", "rollout-another-page"]);

    let output = sb.run(&["--human", "scan", "--cwd", "/no-matching-project"], None);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("0 conversation files"));
    assert!(text.contains("No matching conversations found."));
    assert!(!text.contains("Ref:"));
}

fn file_tree(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    if !root.exists() {
        return Vec::new();
    }
    let mut files: Vec<_> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
            (relative, fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn storage_inventory_is_read_only_and_fails_closed_on_unreadable_journals() {
    let sb = CliSandbox::new();
    let native_bytes = fs::metadata(&sb.session).unwrap().len();

    // Inventory must not create an empty Vault merely because none exists yet.
    let empty = sb.ok(&["storage"]);
    assert_eq!(empty["kind"], "storage_inventory");
    assert_eq!(empty["read_only"], true);
    assert_eq!(empty["native_rollouts"]["files"], 1);
    assert_eq!(empty["native_rollouts"]["bytes"], native_bytes);
    assert_eq!(empty["required_recovery_anchors"]["files"], 0);
    assert_eq!(empty["search_index"]["rebuildable"], true);
    assert!(!sb.dir.path().join("vault").exists());

    sb.add_historical_message();
    sb.ok(&["index"]);
    sb.ok(&["archive", "cli-test"]);
    let vault = sb.dir.path().join("vault");
    let orphan = vault.join("backups/rollout-cli-test.snapshot-orphan.jsonl.zst");
    fs::write(&orphan, b"orphan bytes").unwrap();
    let missing_owner = vault.join("backups/missing-owner.snapshot-1.jsonl.zst");
    fs::write(&missing_owner, b"unknown owner").unwrap();
    let not_a_backup = vault.join("backups/notes.txt");
    fs::write(&not_a_backup, b"not a backup archive").unwrap();
    let before = file_tree(sb.dir.path());

    let report = sb.ok(&["storage"]);
    assert_eq!(report["read_only"], true);
    assert_eq!(report["required_recovery_anchors"]["files"], 1);
    assert!(
        report["required_recovery_anchors"]["bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        report["required_recovery_anchors"]["immutable_originals"]["files"],
        1
    );
    assert_eq!(
        report["unreferenced_backups"]["classification"],
        "not_referenced_by_readable_owner_journal"
    );
    assert_eq!(report["unreferenced_backups"]["files"], 1);
    assert_eq!(
        report["unreferenced_backups"]["bytes"],
        fs::metadata(&orphan).unwrap().len()
    );
    assert_eq!(
        report["unreferenced_backups"]["retention_eligibility"],
        "not_assessed"
    );
    assert_eq!(report["ambiguous_backups"]["files"], 1);
    assert_eq!(
        report["ambiguous_backups"]["bytes"],
        fs::metadata(&missing_owner).unwrap().len()
    );
    assert_eq!(
        report["ambiguous_backups"]["missing_owner_journal_files"],
        1
    );
    assert_eq!(report["other_backup_directory_files"]["files"], 1);
    assert_eq!(
        report["other_backup_directory_files"]["bytes"],
        fs::metadata(&not_a_backup).unwrap().len()
    );
    assert_eq!(report["retention_eligibility"], "not_assessed");
    assert_eq!(
        report["vault"]["backup_bytes"].as_u64().unwrap(),
        report["required_recovery_anchors"]["bytes"]
            .as_u64()
            .unwrap()
            + report["unreferenced_backups"]["bytes"].as_u64().unwrap()
            + report["ambiguous_backups"]["bytes"].as_u64().unwrap()
            + report["other_backup_directory_files"]["bytes"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        report["total_bytes"].as_u64().unwrap(),
        report["native_rollouts"]["bytes"].as_u64().unwrap()
            + report["vault"]["bytes"].as_u64().unwrap()
    );
    assert!(report["search_index"]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(report["search_index"]["derived"], true);
    assert_eq!(
        report["search_index"]["rebuild_command"],
        "codex-vault index --rebuild"
    );
    assert_eq!(
        file_tree(sb.dir.path()),
        before,
        "storage command mutated files"
    );

    // An unreadable journal could reference the apparent orphan. Fail closed instead of calling
    // it garbage or safe-to-prune.
    fs::write(vault.join("manifests/unreadable.json"), b"{ not valid json").unwrap();
    let before = file_tree(sb.dir.path());
    let ambiguous = sb.ok(&["storage"]);
    assert_eq!(ambiguous["recovery_metadata"]["unreadable_manifests"], 1);
    assert_eq!(
        ambiguous["unreferenced_backups"]["classification"],
        "indeterminate"
    );
    assert!(ambiguous["unreferenced_backups"]["bytes"].is_null());
    assert_eq!(
        ambiguous["unreferenced_backups"]["retention_eligibility"],
        "not_assessed"
    );
    assert_eq!(ambiguous["ambiguous_backups"]["files"], 2);
    assert_eq!(
        ambiguous["ambiguous_backups"]["bytes"],
        fs::metadata(&orphan).unwrap().len() + fs::metadata(&missing_owner).unwrap().len()
    );
    assert_eq!(ambiguous["required_recovery_anchors"]["files"], 1);
    assert_eq!(
        file_tree(sb.dir.path()),
        before,
        "storage command mutated files"
    );

    let human = sb.run(&["--human", "storage"], None);
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("Storage inventory"));
    assert!(text.contains("Search index (rebuildable)"));
    assert!(text.contains("Ambiguous backups"));
    assert!(text.contains("No retention eligibility was assessed and no action was taken."));
}

#[test]
fn cli_short_commands_roundtrip_and_integrity_exit_codes() {
    let sb = CliSandbox::new();
    let original = fs::read(&sb.session).unwrap();
    let scan = sb.run(&["--json", "scan"], None);
    assert!(scan.status.success());
    assert_eq!(
        CliSandbox::value(&scan)["sessions"][0]["title"],
        "Test conversation"
    );
    sb.ok(&["index"]);
    let compact = sb.run(&["--json", "compact", "cli-test"], None);
    assert!(
        compact.status.success(),
        "{}",
        String::from_utf8_lossy(&compact.stderr)
    );
    let compact_value = CliSandbox::value(&compact);
    assert_eq!(compact_value["search_index"]["may_be_stale"], true);
    assert_eq!(
        compact_value["search_index"]["refresh_command"],
        "codex-vault index"
    );
    assert_eq!(compact_value["search_index"]["rebuildable"], true);
    assert_eq!(compact_value["search_index"]["canonical"], false);
    assert!(fs::metadata(&sb.session).unwrap().len() < original.len() as u64);
    let restored = sb.run(&["--json", "restore", "cli-test", "--original"], None);
    assert!(restored.status.success());
    assert_eq!(
        CliSandbox::value(&restored)["search_index"]["may_be_stale"],
        true
    );
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    let backup = PathBuf::from(CliSandbox::value(&compact)["backup"].as_str().unwrap());
    fs::write(backup, b"damaged archive").unwrap();
    let failed = sb.run(&["--json", "restore", "cli-test", "--original"], None);
    assert_eq!(failed.status.code(), Some(4));
    assert_eq!(CliSandbox::value(&failed)["status"], "failed");
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    let doctor = sb.run(&["--json", "doctor", "cli-test"], None);
    assert_eq!(doctor.status.code(), Some(4));
}

#[test]
fn stale_index_hints_only_follow_relevant_source_changes() {
    let no_index = CliSandbox::new();
    let archive_without_index = no_index.ok(&["archive", "cli-test"]);
    assert!(archive_without_index.get("search_index").is_none());
    assert!(!no_index.dir.path().join("vault/index.sqlite").exists());

    let sb = CliSandbox::new();
    sb.ok(&["index"]);
    let index_path = sb.dir.path().join("vault/index.sqlite");
    let index_before = fs::read(&index_path).unwrap();

    let preview = sb.ok(&["compact", "cli-test", "--dry-run"]);
    assert!(preview.get("search_index").is_none());

    let archived = sb.ok(&["archive", "cli-test"]);
    assert_eq!(archived["search_index"]["may_be_stale"], true);
    let existing = sb.ok(&["archive", "cli-test"]);
    assert_eq!(existing["status"], "exists");
    assert!(existing.get("search_index").is_none());
    let listed = sb.ok(&["restore", "cli-test", "--list"]);
    assert!(listed.get("search_index").is_none());

    let human = sb.run(&["--human", "archive", "cli-test", "--force"], None);
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("Search index may be stale."));
    assert!(human.contains("Run: codex-vault index"));

    let compacted = sb.ok(&["compact", "cli-test"]);
    assert_eq!(compacted["search_index"]["may_be_stale"], true);
    let no_op = sb.ok(&["compact", "cli-test"]);
    assert_eq!(no_op["status"], "already_compact");
    assert!(no_op.get("search_index").is_none());

    let restored = sb.ok(&["restore", "cli-test", "--original"]);
    assert_eq!(restored["search_index"]["may_be_stale"], true);
    let already_restored = sb.ok(&["restore", "cli-test", "--original"]);
    assert!(already_restored["stats"]["pre_restore_backup"].is_null());
    assert!(already_restored.get("search_index").is_none());
    assert_eq!(
        fs::read(&index_path).unwrap(),
        index_before,
        "archive/compact/restore hints must never refresh the index automatically"
    );

    let batch = CliSandbox::new();
    batch.ok(&["index"]);
    let changed_batch = batch.ok(&["compact", "--cwd", "C:/sample project"]);
    assert_eq!(changed_batch["search_index"]["may_be_stale"], true);
    let no_op_batch = batch.ok(&["compact", "--cwd", "C:/sample project"]);
    assert!(no_op_batch.get("search_index").is_none());
}

#[test]
fn unscoped_compaction_and_conflicting_targets_are_refused() {
    let sb = CliSandbox::new();
    let before = fs::read(&sb.session).unwrap();
    assert_eq!(sb.run(&["compact"], None).status.code(), Some(2));
    assert_eq!(
        sb.run(&["compact", "cli-test", "--session", "other"], None)
            .status
            .code(),
        Some(2)
    );
    assert_eq!(sb.run(&["archive"], None).status.code(), Some(2));
    assert_eq!(fs::read(&sb.session).unwrap(), before);
}

#[test]
fn menu_cancellation_and_eof_leave_native_bytes_untouched() {
    let sb = CliSandbox::new();
    let before = fs::read(&sb.session).unwrap();
    for input in ["1\n3\nn\n0\nq\n", "1\n3\n", "999\n"] {
        let result = sb.run(&["menu"], Some(input));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read(&sb.session).unwrap(), before);
    }
    assert!(!sb.dir.path().join("vault/backups").exists());
}

#[test]
fn menu_can_archive_compact_verify_and_restore_selected_session() {
    let sb = CliSandbox::new();
    let before = fs::read(&sb.session).unwrap();
    sb.ok(&["index"]);
    let index_path = sb.dir.path().join("vault/index.sqlite");
    let index_before = fs::read(&index_path).unwrap();
    let result = sb.run(
        &["menu"],
        Some("/sample project\n1\n2\n3\ny\n5\n1\nyes\n4\n0\nq\n"),
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&sb.session).unwrap(), before);
    let states = CliSandbox::value(&sb.run(&["--json", "restore", "cli-test", "--list"], None));
    assert!(states["anchors"].as_array().unwrap().len() >= 2);
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(text.contains("Test conversation"));
    assert!(text.contains("Net savings, including backups and metadata:"));
    assert!(text.contains("Restore this conversation? [y/N]"));
    assert!(text.matches("Search index may be stale.").count() >= 3);
    assert!(text.matches("Run: codex-vault index").count() >= 3);
    assert_eq!(fs::read(index_path).unwrap(), index_before);
}

#[test]
fn busy_batch_returns_nonzero_and_a_stable_error_code() {
    use codex_vault::fsatomic::MutationGuard;
    let sb = CliSandbox::new();
    let vault = sb.dir.path().join("vault");
    fs::create_dir(&vault).unwrap();
    let _guard = MutationGuard::acquire(&vault, &sb.session).unwrap();
    let result = sb.run(&["--json", "compact", "--cwd", "C:/sample project"], None);
    assert_eq!(result.status.code(), Some(5));
    assert_eq!(
        CliSandbox::value(&result)["sessions"][0]["code"],
        "session_locked"
    );
}

#[test]
fn dry_run_predicts_backup_bytes_without_creating_a_vault() {
    let sb = CliSandbox::new();
    let original = fs::read(&sb.session).unwrap();
    let preview = sb.run(&["--json", "compact", "cli-test", "--dry-run"], None);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan = CliSandbox::value(&preview);
    assert_eq!(plan["status"], "preview");
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    assert!(!sb.dir.path().join("vault").exists());
    let actual = CliSandbox::value(&sb.run(&["--json", "compact", "cli-test"], None));
    let backup = PathBuf::from(actual["backup"].as_str().unwrap());
    assert_eq!(
        plan["stats"]["storage_preview"]["new_backup_bytes"],
        fs::metadata(backup).unwrap().len()
    );
    let disk_after = fs::metadata(&sb.session).unwrap().len()
        + codex_vault::storage::directory_bytes(&sb.dir.path().join("vault")).unwrap();
    assert_eq!(
        actual["stats"]["storage"]["net_saved_bytes"]
            .as_i64()
            .unwrap(),
        original.len() as i64 - disk_after as i64
    );
    // Tiny transcripts cost more to back up and journal than they save.
    assert_eq!(actual["stats"]["storage"]["space_increased"], true);
}

#[test]
fn multiple_compactions_preserve_each_cycle_and_account_for_retained_snapshots() {
    let sb = CliSandbox::new();
    let original = fs::read(&sb.session).unwrap();
    assert!(sb.run(&["compact", "cli-test"], None).status.success());
    let first_backup_bytes =
        codex_vault::storage::directory_bytes(&sb.dir.path().join("vault/backups")).unwrap();
    let mut growing = fs::OpenOptions::new()
        .append(true)
        .open(&sb.session)
        .unwrap();
    for record in [
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic retained detail ".repeat(3000)}]}}),
        json!({"type":"compacted","payload":{"replacement_history":[{"role":"user"}],"window_number":4}}),
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"cycle-two"}}),
        json!({"type":"turn_context","payload":{"turn_id":"cycle-two","model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"continue synthetic project"}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"cycle-two"}}),
    ] {
        writeln!(growing, "{record}").unwrap();
    }
    drop(growing);
    let grown = fs::read(&sb.session).unwrap();
    let vault_before = codex_vault::storage::directory_bytes(&sb.dir.path().join("vault")).unwrap();
    let second = sb.run(&["--json", "compact", "cli-test"], None);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let report = CliSandbox::value(&second);
    let vault_after = codex_vault::storage::directory_bytes(&sb.dir.path().join("vault")).unwrap();
    let native_after = fs::metadata(&sb.session).unwrap().len();
    assert_eq!(
        report["stats"]["storage"]["net_saved_bytes"]
            .as_i64()
            .unwrap(),
        (grown.len() as i64 + vault_before as i64) - (native_after as i64 + vault_after as i64)
    );
    assert!(
        report["stats"]["storage"]["after"]["backup_bytes"]
            .as_u64()
            .unwrap()
            > first_backup_bytes
    );
    println!("synthetic-cycle-two: native_before={} native_after={} vault_before={} vault_after={} net_saved={}", grown.len(), native_after, vault_before, vault_after, report["stats"]["storage"]["net_saved_bytes"]);
    let no_op = CliSandbox::value(&sb.run(&["--json", "compact", "cli-test"], None));
    assert_eq!(no_op["status"], "already_compact");
    assert_eq!(no_op["stats"]["storage"]["net_saved_bytes"], 0);
    assert!(sb.run(&["restore", "cli-test"], None).status.success());
    assert_eq!(fs::read(&sb.session).unwrap(), grown);
    assert!(sb
        .run(&["restore", "cli-test", "--original"], None)
        .status
        .success());
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    assert!(sb
        .run(&["doctor", "cli-test", "--deep"], None)
        .status
        .success());
}

#[test]
fn search_survives_compaction_reindex_corruption_rebuild_and_restore() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    let original = fs::read(&sb.session).unwrap();
    let first = sb.ok(&["index"]);
    assert!(first["index_bytes"].as_u64().unwrap() > 0);
    let matches = sb.ok(&["search", "authentication tokens"]);
    assert_eq!(matches["matches"].as_array().unwrap().len(), 1);
    let id = matches["matches"][0]["id"].as_str().unwrap();
    let read = sb.ok(&["read", id]);
    assert_eq!(
        read["text"],
        "Decision: authentication uses rotating refresh tokens. Café 🦀"
    );
    assert_eq!(read["verified_reference"]["line"], 2);
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    let compact = sb.ok(&["compact", "cli-test"]);
    assert_eq!(compact["search_index"]["may_be_stale"], true);
    let stale_read = sb.run(&["read", id], None);
    assert_eq!(stale_read.status.code(), Some(4));
    sb.ok(&["index"]);
    let after = sb.ok(&["search", "authentication tokens"]);
    assert_eq!(after["matches"][0]["id"], id);
    assert_eq!(sb.ok(&["read", id])["verified_reference"]["kind"], "backup");
    let unchanged = sb.ok(&["index"]);
    assert_eq!(unchanged["updated_sources"], 0);
    assert_eq!(unchanged["unchanged_sources"], 2);
    fs::write(
        sb.dir.path().join("vault/index.sqlite"),
        b"corrupt derived index",
    )
    .unwrap();
    assert_eq!(
        sb.run(&["search", "authentication"], None).status.code(),
        Some(4)
    );
    sb.ok(&["index", "--rebuild"]);
    assert_eq!(sb.ok(&["search", "authentication"])["matches"][0]["id"], id);
    let restore = sb.ok(&["restore", "cli-test", "--original"]);
    assert_eq!(restore["search_index"]["may_be_stale"], true);
    sb.ok(&["index"]);
    assert_eq!(
        sb.ok(&["search", "authentication"])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sb.ok(&["read", id])["text"], read["text"]);
    assert_eq!(fs::read(&sb.session).unwrap(), original);
    fs::remove_file(&sb.session).unwrap();
    sb.ok(&["index"]);
    assert_eq!(sb.ok(&["read", id])["text"], read["text"]);
}

#[test]
fn index_deduplicates_passage_bodies_across_recovery_snapshots() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    let first = sb.ok(&["index"]);
    let passages = first["passages"].as_i64().unwrap();
    let occurrences = first["occurrences"].as_i64().unwrap();
    assert!(first["indexed_text_bytes"].as_i64().unwrap() > 0);

    sb.ok(&["archive", "cli-test"]);
    let with_snapshot = sb.ok(&["index"]);
    assert_eq!(with_snapshot["sources"], 2);
    assert_eq!(with_snapshot["passages"], passages);
    assert!(with_snapshot["occurrences"].as_i64().unwrap() > occurrences);
    assert_eq!(
        with_snapshot["duplicate_occurrences_without_duplicate_body"],
        with_snapshot["occurrences"].as_i64().unwrap() - passages
    );
    let expected_dedup = (with_snapshot["occurrences"].as_i64().unwrap() - passages) as f64
        / with_snapshot["occurrences"].as_i64().unwrap() as f64;
    assert!(
        (with_snapshot["deduplication_ratio"].as_f64().unwrap() - expected_dedup).abs()
            < f64::EPSILON
    );
    assert_eq!(
        with_snapshot["index_bytes"].as_u64().unwrap(),
        fs::metadata(sb.dir.path().join("vault/index.sqlite"))
            .unwrap()
            .len()
    );
    assert!(
        with_snapshot["index_to_indexed_text_ratio"]
            .as_f64()
            .unwrap()
            > 0.0
    );

    let found = sb.ok(&["search", "authentication tokens"]);
    let id = found["matches"][0]["id"].as_str().unwrap();
    let read = sb.ok(&["read", id]);
    assert_eq!(
        read["text"],
        "Decision: authentication uses rotating refresh tokens. Café 🦀"
    );
    assert!(read["verified_reference"].is_object());
}

#[test]
fn project_filters_cannot_leak_into_a_similarly_named_project() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    let other = sb.dir.path().join("codex/sessions/rollout-other.jsonl");
    let text = fs::read_to_string(&sb.session)
        .unwrap()
        .replace("cli-test", "other-test")
        .replace("C:/sample project", "C:/sample project-other");
    fs::write(other, text).unwrap();
    sb.ok(&["index", "--cwd", "C:/sample project"]);
    assert_eq!(
        sb.ok(&["search", "authentication"])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    sb.ok(&["index", "--cwd", "C:/sample project-other"]);
    let all = sb.ok(&["search", "authentication"]);
    assert_eq!(all["matches"].as_array().unwrap().len(), 2);
    let filtered = sb.ok(&["search", "authentication", "--cwd", "C:/sample project"]);
    assert_eq!(filtered["matches"].as_array().unwrap().len(), 1);
    let other_id = all["matches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["session_id"] == "other-test")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        sb.run(&["read", other_id, "--cwd", "C:/sample project"], None)
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        sb.run(&["index", "--rebuild", "--cwd", "C:/sample project"], None)
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn failed_index_update_preserves_previous_search_results() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    sb.ok(&["index"]);
    let id = sb.ok(&["search", "authentication"])["matches"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&sb.session)
        .unwrap();
    writeln!(file, "broken json").unwrap();
    drop(file);
    assert_eq!(sb.run(&["index"], None).status.code(), Some(4));
    assert_eq!(sb.run(&["index", "--rebuild"], None).status.code(), Some(4));
    assert_eq!(sb.ok(&["search", "authentication"])["matches"][0]["id"], id);
    assert_eq!(sb.run(&["read", &id], None).status.code(), Some(4));
}

#[test]
fn mcp_handshake_tools_and_project_scope_are_read_only() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    sb.ok(&["index"]);
    let id = sb.ok(&["search", "authentication"])["matches"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let native = fs::read(&sb.session).unwrap();
    let database = fs::read(sb.dir.path().join("vault/index.sqlite")).unwrap();
    let messages = [
        json!({"jsonrpc":"2.0","id":0,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"synthetic-client","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":null,"_meta":{"progressToken":"synthetic"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vault_search","arguments":{"query":"authentication"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"vault_read","arguments":{"id":id}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"vault_search","arguments":{"query":"authentication","cwd":"C:/"}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"compact","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":7,"method":"compact"}),
        json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"vault_search","arguments":{"query":"authentication","unexpected":"refuse"}}}),
    ];
    let input = messages
        .iter()
        .map(|m| format!("{m}\n"))
        .collect::<String>();
    let result = sb.run(&["mcp", "--cwd", "C:/sample project"], Some(&input));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(responses.len(), 9);
    assert_eq!(responses[0]["error"]["code"], -32002);
    assert_eq!(responses[1]["result"]["protocolVersion"], "2025-11-25");
    let tools = responses[2]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert!(tools
        .iter()
        .all(|t| t["annotations"]["readOnlyHint"] == true));
    assert_eq!(
        responses[3]["result"]["structuredContent"]["matches"][0]["id"],
        id
    );
    assert_eq!(
        responses[4]["result"]["structuredContent"]["text"],
        "Decision: authentication uses rotating refresh tokens. Café 🦀"
    );
    assert_eq!(responses[5]["result"]["isError"], true);
    assert_eq!(responses[6]["error"]["code"], -32602);
    assert_eq!(responses[7]["error"]["code"], -32601);
    assert_eq!(responses[8]["result"]["isError"], true);
    assert_eq!(fs::read(&sb.session).unwrap(), native);
    assert_eq!(
        fs::read(sb.dir.path().join("vault/index.sqlite")).unwrap(),
        database
    );
}

#[test]
fn mcp_reports_parse_errors_and_bounds_request_size() {
    let sb = CliSandbox::new();
    let result = sb.run(
        &["mcp"],
        Some("broken json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"),
    );
    assert!(result.status.success());
    let frames: Vec<Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(frames[0]["error"]["code"], -32700);
    assert_eq!(frames[1]["result"], json!({}));
    assert!(!sb.dir.path().join("vault").exists());
    let large = "x".repeat(1024 * 1024 + 1);
    assert_eq!(sb.run(&["mcp"], Some(&large)).status.code(), Some(2));
}

#[test]
fn oversized_records_are_counted_without_losing_following_references() {
    let sb = CliSandbox::new();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&sb.session)
        .unwrap();
    writeln!(file,"{}",json!({"type":"response_item","payload":{"type":"function_call_output","output":"x".repeat(16*1024*1024)}})).unwrap();
    writeln!(file,"{}",json!({"type":"event_msg","payload":{"type":"user_message","message":"after oversized record"}})).unwrap();
    drop(file);
    let report = sb.ok(&["index"]);
    assert_eq!(report["skipped_oversized_records"], 1);
    let id = sb.ok(&["search", "oversized"])["matches"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let found = sb.ok(&["read", &id]);
    assert_eq!(found["text"], "after oversized record");
    assert!(
        found["verified_reference"]["decoded_byte_offset"]
            .as_u64()
            .unwrap()
            > 16 * 1024 * 1024
    );
}

#[cfg(windows)]
#[test]
fn indexing_defers_busy_native_sources_and_keeps_their_previous_snapshot() {
    let sb = CliSandbox::new();
    sb.add_historical_message();
    sb.ok(&["index"]);
    let _writer = fs::OpenOptions::new()
        .write(true)
        .open(&sb.session)
        .unwrap();
    let report = sb.ok(&["index"]);
    assert_eq!(report["deferred_busy_sources"], 1);
    assert_eq!(report["removed_sources"], 0);
    assert_eq!(
        sb.ok(&["search", "authentication"])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sb.run(&["index", "--rebuild"], None).status.code(), Some(5));
}
