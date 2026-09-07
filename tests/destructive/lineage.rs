use super::*;

#[test]
fn a_page_another_one_continues_from_is_never_shortened() {
    // Proven against real transcripts: compacting a non-final page makes Codex refuse the whole
    // thread with "invalid paginated history lineage: cutoff byte offset is past the source
    // rollout". There is deliberately no override for this, unlike the spawned-thread refusal:
    // that one is merely unvalidated, this one is known to break the session.
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-1-thr1.jsonl", "thr1", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let tail = lineage_page(
        &sb,
        "rollout-2-thr1_pg2.jsonl",
        "thr1",
        Some(("thr1", root_size)),
        "TAIL",
    );

    let before = fs::read(&root).unwrap();
    let err = compact_safe_impl(&root).unwrap_err();
    match &err {
        VaultError::LineageSourceRefused { successors, .. } => {
            assert_eq!(successors.len(), 1);
            assert!(successors[0].ends_with("rollout-2-thr1_pg2.jsonl"));
        }
        other => panic!("expected a lineage refusal, got {other}"),
    }
    assert_eq!(
        fs::read(&root).unwrap(),
        before,
        "the source page was modified"
    );

    // The newest page has no successor, so it is safe to compact.
    let tail_before = fs::metadata(&tail).unwrap().len();
    assert_eq!(compact_safe_impl(&tail).unwrap().status, "ok");
    assert!(fs::metadata(&tail).unwrap().len() < tail_before);
}

#[test]
fn a_batch_skips_a_lineage_source_rather_than_failing() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-3-thr2.jsonl", "thr2", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    lineage_page(
        &sb,
        "rollout-4-thr2_pg2.jsonl",
        "thr2",
        Some(("thr2", root_size)),
        "TAIL",
    );
    sb.compactable_session("rollout-5-plain.jsonl", "sess-plain", "C:/work/lineage");

    let out = compact_safe_command(
        None,
        Some("C:/work/lineage".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let statuses: Vec<&str> = rows
        .iter()
        .map(|r| {
            r.get("status")
                .or_else(|| r.pointer("/result/status"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        })
        .collect();
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == "skipped_lineage_source")
            .count(),
        1,
        "the source page should be reported, not abort the batch: {rows:#?}"
    );
    assert!(statuses.contains(&"ok"), "{rows:#?}");
    assert_eq!(fs::read(&root).unwrap().len() as u64, root_size);
}
