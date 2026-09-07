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
