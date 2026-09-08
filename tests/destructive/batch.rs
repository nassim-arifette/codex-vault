use super::*;

#[test]
fn a_batch_reports_a_broken_session_instead_of_aborting() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-q1.jsonl", "sess-q1", "C:/work/q");
    unreadable_session(&sb, "rollout-q2.jsonl", "sess-q2", "C:/work/q");
    sb.compactable_session("rollout-q3.jsonl", "sess-q3", "C:/work/q");

    let out = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(4)).unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "every session must be accounted for");

    let errors: Vec<&Value> = rows.iter().filter(|r| r["status"] == "error").collect();
    assert_eq!(errors.len(), 1, "{rows:#?}");
    assert_eq!(errors[0]["session_id"], "sess-q2");
    assert_eq!(errors[0]["code"], "io_error");

    let analysed = rows.iter().filter(|r| r.get("analysis").is_some()).count();
    assert_eq!(analysed, 2, "the healthy sessions must still be analysed");
}

#[test]
fn parallel_and_serial_batches_produce_identical_output() {
    let sb = Sandbox::new();
    for i in 0..6 {
        sb.compactable_session(
            &format!("rollout-r{i}.jsonl"),
            &format!("sess-r{i}"),
            "C:/work/r",
        );
    }
    // Ordering must come from the session list, never from which worker finished first.
    let serial = doctor_command(None, None, false, quiet_batch(1)).unwrap();
    let parallel = doctor_command(None, None, false, quiet_batch(8)).unwrap();
    assert_eq!(serial, parallel);
    assert_eq!(serial.as_array().unwrap().len(), 6);

    let serial = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(1)).unwrap();
    let parallel = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(8)).unwrap();
    assert_eq!(serial, parallel);
}

#[test]
fn doctor_batch_preserves_paginated_lineage_findings() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-doctor-thr.jsonl", "doctor-thr", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let _tail = lineage_page(
        &sb,
        "rollout-doctor-thr_page2.jsonl",
        "doctor-thr",
        Some(("doctor-thr", root_size + 1)),
        "TAIL",
    );

    let out = doctor_command(None, None, false, quiet_batch(4)).unwrap();
    let rows = out.as_array().unwrap();
    let root_row = rows
        .iter()
        .find(|row| {
            row["session_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("rollout-doctor-thr.jsonl"))
        })
        .unwrap();
    let tail_row = rows
        .iter()
        .find(|row| {
            row["session_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("rollout-doctor-thr_page2.jsonl"))
        })
        .unwrap();
    assert_eq!(root_row["lineage_broken"], true, "{root_row:#?}");
    assert_eq!(root_row["status"], "warning");
    assert_eq!(tail_row["lineage_broken"], false, "{tail_row:#?}");
}

#[test]
fn batch_prune_keeps_global_fail_closed_audit_but_removes_temp_debris() {
    let sb = Sandbox::new();
    let mut sessions = Vec::new();
    for i in 0..3 {
        let session = sb.compactable_session(
            &format!("rollout-prune-batch-{i}.jsonl"),
            &format!("prune-batch-{i}"),
            "C:/work/prune-batch",
        );
        archive_impl(&session, false).unwrap();
        sessions.push(session);
    }

    let vault = ensure_vault_paths().unwrap();
    let orphan = vault
        .backups
        .join("rollout-prune-batch-0.snapshot-orphan.jsonl.zst");
    fs::write(&orphan, b"not a recovery source").unwrap();
    fs::write(vault.manifests.join("broken-global.json"), b"{ broken").unwrap();

    let temps: Vec<_> = sessions
        .iter()
        .map(|session| {
            let temp = session.with_file_name(format!(
                "{}.compact.1.tmp",
                session.file_name().unwrap().to_string_lossy()
            ));
            fs::write(&temp, b"debris").unwrap();
            temp
        })
        .collect();

    let result = prune_command(None, None, true, true).unwrap();
    let rows = result["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), sessions.len());
    assert!(
        orphan.exists(),
        "an unreadable journal must block backup deletion"
    );
    assert!(temps.iter().all(|path| !path.exists()));
    assert!(rows.iter().all(|row| {
        row["note"]
            .as_str()
            .is_some_and(|note| note.contains("refusing to delete backups"))
    }));
    assert!(rows.iter().all(|row| row["unreferenced_backups"] == 0));
}

#[test]
fn a_destructive_batch_never_reaches_a_parent_projects_sessions() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-s1.jsonl", "sess-here", "C:/work/repo/frontend");
    sb.compactable_session(
        "rollout-s2.jsonl",
        "sess-below",
        "C:/work/repo/frontend/src",
    );
    sb.compactable_session("rollout-s3.jsonl", "sess-parent", "C:/work");
    sb.compactable_session("rollout-s4.jsonl", "sess-sibling", "C:/work/repo/backend");

    let touched = compact_safe_command(
        None,
        Some("C:/work/repo/frontend".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let ids: Vec<String> = touched["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"sess-here".to_string()));
    assert!(ids.contains(&"sess-below".to_string()));
    assert!(!ids.contains(&"sess-parent".to_string()), "{ids:?}");
    assert!(!ids.contains(&"sess-sibling".to_string()), "{ids:?}");
}
