use super::*;

#[test]
fn compacting_a_spawned_thread_is_refused_by_default() {
    let sb = Sandbox::new();
    let path = spawned_session(&sb, "rollout-sub.jsonl", "sub-1", "C:/work/s", "subagent");
    let before = fs::read(&path).unwrap();

    let err = compact_safe_impl(&path).unwrap_err();
    match &err {
        VaultError::SpawnedThreadRefused { thread_source, .. } => {
            assert_eq!(thread_source.as_deref(), Some("subagent"))
        }
        other => panic!("expected a spawned-thread refusal, got {other}"),
    }
    assert_eq!(fs::read(&path).unwrap(), before);

    // Archiving is non-destructive, so it stays allowed.
    assert_eq!(archive_impl(&path, false).unwrap().status, "ok");
    assert_eq!(fs::read(&path).unwrap(), before);

    // And the refusal is an opt-out, not a wall.
    let allowed = compact_safe_impl_with(
        &path,
        CompactOptions {
            allow_spawned_threads: true,
            ..CompactOptions::default()
        },
    )
    .unwrap();
    assert_eq!(allowed.status, "ok");
    assert!(fs::read(&path).unwrap().len() < before.len());
}

#[test]
fn a_guardian_review_thread_is_refused_too() {
    let sb = Sandbox::new();
    let path = spawned_session(
        &sb,
        "rollout-gr.jsonl",
        "gr-1",
        "C:/work/s",
        "guardian_review",
    );
    assert!(matches!(
        compact_safe_impl(&path).unwrap_err(),
        VaultError::SpawnedThreadRefused { .. }
    ));
}

#[test]
fn a_batch_skips_spawned_threads_without_failing() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-u1.jsonl", "sess-u1", "C:/work/u");
    spawned_session(&sb, "rollout-u2.jsonl", "sub-u2", "C:/work/u", "subagent");

    let out = compact_safe_command(
        None,
        Some("C:/work/u".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let statuses: Vec<&str> = rows
        .iter()
        .map(|r| {
            r.get("status")
                .or_else(|| r.pointer("/result/status"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        })
        .collect();
    assert!(statuses.contains(&"ok"), "{rows:#?}");
    assert!(statuses.contains(&"skipped_spawned_thread"), "{rows:#?}");
}
