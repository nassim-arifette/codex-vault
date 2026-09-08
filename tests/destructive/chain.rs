use super::*;

#[test]
fn a_linear_paginated_conversation_compacts_and_restores_as_one_transaction() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-10-chain.jsonl", "chain", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let middle = lineage_page(
        &sb,
        "rollout-11-chain_mid.jsonl",
        "chain",
        Some(("chain", root_size)),
        "MIDDLE",
    );
    let middle_size = fs::metadata(&middle).unwrap().len();
    let leaf = lineage_page(
        &sb,
        "rollout-12-chain_leaf.jsonl",
        "chain",
        Some(("mid", middle_size)),
        "LEAF",
    );
    let original = [
        fs::read(&root).unwrap(),
        fs::read(&middle).unwrap(),
        fs::read(&leaf).unwrap(),
    ];

    codex_vault::index::build(None, true).unwrap();
    let preview = compact_conversation_command(
        "codex://threads/chain".to_string(),
        None,
        CompactOptions {
            dry_run: true,
            ..CompactOptions::default()
        },
    )
    .unwrap();
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["page_count"], 3);
    assert_eq!(preview["storage"]["accounting_version"], 2);
    assert_eq!(preview["storage"]["scope"], "current_operation_preview");
    assert!(preview["storage"].get("vault_total_bytes_before").is_none());
    assert!(preview["storage"]
        .get("retained_backup_bytes_before")
        .is_none());
    assert!(preview.get("search_index").is_none());

    let result =
        compact_conversation_command("chain".to_string(), None, CompactOptions::default()).unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["page_count"], 3);
    assert_eq!(result["storage"]["accounting_version"], 2);
    assert_eq!(result["storage"]["scope"], "current_operation");
    assert_eq!(
        result["storage"]["net_saved_bytes"].as_i64().unwrap(),
        result["storage"]["native_saved_bytes"].as_u64().unwrap() as i64
            - result["storage"]["persistent_vault_delta_bytes"]
                .as_i64()
                .unwrap()
    );
    assert!(result["storage"]["backup_bytes_created"].as_u64().unwrap() > 0);
    let created = result["storage"]["files_created"].as_array().unwrap();
    for kind in ["backup", "manifest", "summary", "transaction"] {
        assert!(
            created.iter().any(|file| file["kind"] == kind),
            "chain compaction should report the created {kind}"
        );
    }
    assert_eq!(result["search_index"]["may_be_stale"], true);
    assert!(fs::metadata(&root).unwrap().len() < original[0].len() as u64);
    assert!(fs::metadata(&middle).unwrap().len() < original[1].len() as u64);
    assert!(fs::metadata(&leaf).unwrap().len() < original[2].len() as u64);

    let middle_meta: Value =
        serde_json::from_str(fs::read_to_string(&middle).unwrap().lines().next().unwrap()).unwrap();
    let leaf_meta: Value =
        serde_json::from_str(fs::read_to_string(&leaf).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(
        middle_meta["payload"]["history_base"]["end_byte_offset"],
        fs::metadata(&root).unwrap().len()
    );
    assert_eq!(
        leaf_meta["payload"]["history_base"]["end_byte_offset"],
        fs::metadata(&middle).unwrap().len()
    );

    let restored = restore_conversation_command("chain".to_string(), None).unwrap();
    assert_eq!(restored["status"], "ok");
    assert_eq!(restored["search_index"]["may_be_stale"], true);
    assert_eq!(fs::read(&root).unwrap(), original[0]);
    assert_eq!(fs::read(&middle).unwrap(), original[1]);
    assert_eq!(fs::read(&leaf).unwrap(), original[2]);
}

#[test]
fn a_compacted_chain_can_continue_paginate_recompact_and_restore_the_second_generation() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-13-lifecycle.jsonl", "lifecycle", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let middle = lineage_page(
        &sb,
        "rollout-14-lifecycle_mid.jsonl",
        "lifecycle",
        Some(("lifecycle", root_size)),
        "MIDDLE",
    );
    let middle_size = fs::metadata(&middle).unwrap().len();
    let leaf = lineage_page(
        &sb,
        "rollout-15-lifecycle_leaf.jsonl",
        "lifecycle",
        Some(("mid", middle_size)),
        "LEAF",
    );

    let first = compact_conversation("lifecycle", None, CompactOptions::default()).unwrap();
    assert_eq!(first["status"], "ok");

    // Model a resumed conversation growing the newest page, checkpointing again, then Codex
    // paginating it. The new page points at the complete current leaf exactly as a real successor
    // does; Vault must rediscover the expanded closure rather than remembering the old 3-page set.
    append_jsonl(
        &leaf,
        &completed_turn("continued-after-first-chain-compaction"),
    );
    append_jsonl(
        &leaf,
        &[json!({"type":"compacted","payload":{
            "replacement_history":[{"role":"user"}],"window_number":2
        }})],
    );
    append_jsonl(&leaf, &completed_turn("continued-checkpoint-tail"));
    let leaf_size = fs::metadata(&leaf).unwrap().len();
    let newest = lineage_page(
        &sb,
        "rollout-16-lifecycle_newest.jsonl",
        "lifecycle",
        Some(("leaf", leaf_size)),
        "NEWEST",
    );

    let before_second = [
        fs::read(&root).unwrap(),
        fs::read(&middle).unwrap(),
        fs::read(&leaf).unwrap(),
        fs::read(&newest).unwrap(),
    ];
    let second = compact_conversation("lifecycle", None, CompactOptions::default()).unwrap();
    assert_eq!(second["status"], "ok");
    assert_eq!(second["page_count"], 4);
    assert_eq!(second["storage"]["accounting_version"], 2);
    assert!(
        second["storage"]["native_after_bytes"].as_u64().unwrap()
            < second["storage"]["native_before_bytes"].as_u64().unwrap()
    );

    let restored = restore_conversation("lifecycle", None).unwrap();
    assert_eq!(restored["status"], "ok");
    assert_eq!(restored["page_count"], 4);
    assert_eq!(fs::read(&root).unwrap(), before_second[0]);
    assert_eq!(fs::read(&middle).unwrap(), before_second[1]);
    assert_eq!(fs::read(&leaf).unwrap(), before_second[2]);
    assert_eq!(fs::read(&newest).unwrap(), before_second[3]);
}

#[test]
fn a_forked_paginated_conversation_is_refused_before_any_native_mutation() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-17-forked.jsonl", "forked", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let left = lineage_page(
        &sb,
        "rollout-18-forked_left.jsonl",
        "forked",
        Some(("forked", root_size)),
        "LEFT",
    );
    let right = lineage_page(
        &sb,
        "rollout-19-forked_right.jsonl",
        "forked",
        Some(("forked", root_size)),
        "RIGHT",
    );
    let before = [
        fs::read(&root).unwrap(),
        fs::read(&left).unwrap(),
        fs::read(&right).unwrap(),
    ];
    let backups_before = sb.backups();

    let error = compact_conversation("forked", None, CompactOptions::default()).unwrap_err();
    assert_eq!(error.code(), "invalid_input");
    assert!(error.to_string().contains("fork"));
    assert_eq!(fs::read(&root).unwrap(), before[0]);
    assert_eq!(fs::read(&left).unwrap(), before[1]);
    assert_eq!(fs::read(&right).unwrap(), before[2]);
    assert_eq!(sb.backups(), backups_before);
}

#[test]
fn chain_archives_are_identical_through_cli_index_and_read_only_mcp() {
    let sb = Sandbox::new();
    let root = lineage_page(
        &sb,
        "rollout-22-searchchain.jsonl",
        "searchchain",
        None,
        "ARCHIVEDNEEDLE",
    );
    let root_size = fs::metadata(&root).unwrap().len();
    lineage_page(
        &sb,
        "rollout-23-searchchain_leaf.jsonl",
        "searchchain",
        Some(("searchchain", root_size)),
        "LEAF",
    );
    compact_conversation("searchchain", None, CompactOptions::default()).unwrap();
    assert!(
        !fs::read_to_string(&root)
            .unwrap()
            .contains("ARCHIVEDNEEDLE"),
        "the assertion must exercise an archived passage, not the live compacted page"
    );

    codex_vault::index::build(None, true).unwrap();
    let cli_search = codex_vault::index::search("ARCHIVEDNEEDLE", None, 10, 0).unwrap();
    let passage_id = cli_search["matches"][0]["id"].as_str().unwrap().to_string();
    let cli_read = codex_vault::index::read(&passage_id, None, 0, 8000).unwrap();
    assert_eq!(cli_read["text"], "PAGE-ARCHIVEDNEEDLE");
    assert_eq!(cli_read["verified_reference"]["kind"], "backup");

    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"chain-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault_search","arguments":{"query":"ARCHIVEDNEEDLE"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vault_read","arguments":{"id":passage_id}}}),
    ];
    let input = messages
        .iter()
        .map(|message| format!("{message}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    codex_vault::mcp::serve(std::io::Cursor::new(input), &mut output, None).unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["matches"][0]["id"],
        cli_search["matches"][0]["id"]
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["text"],
        cli_read["text"]
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["verified_reference"],
        cli_read["verified_reference"]
    );
}

#[test]
fn chain_compaction_preserves_predecessor_bytes_after_the_history_boundary() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-20-tailchain.jsonl", "tailchain", None, "ROOT");
    let boundary = fs::metadata(&root).unwrap().len();
    append_jsonl(
        &root,
        &[
            json!({"type":"event_msg","payload":{"type":"user_message","message":"TAIL-AFTER-HISTORY-BOUNDARY"}}),
        ],
    );
    let leaf = lineage_page(
        &sb,
        "rollout-21-tailchain_leaf.jsonl",
        "tailchain",
        Some(("tailchain", boundary)),
        "LEAF",
    );
    let original_root = fs::read(&root).unwrap();
    let original_leaf = fs::read(&leaf).unwrap();

    let result = compact_conversation("tailchain", None, CompactOptions::default()).unwrap();
    assert_eq!(result["status"], "ok");
    let rewritten_boundary = result["pages"][0]["rewritten_successor_boundary_bytes"]
        .as_u64()
        .unwrap();
    let root_after = fs::read_to_string(&root).unwrap();
    assert!(root_after.contains("TAIL-AFTER-HISTORY-BOUNDARY"));
    assert!(
        fs::metadata(&root).unwrap().len() > rewritten_boundary,
        "the preserved predecessor tail must remain after the child's consumed prefix"
    );
    let leaf_meta: Value =
        serde_json::from_str(fs::read_to_string(&leaf).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(
        leaf_meta["payload"]["history_base"]["end_byte_offset"],
        rewritten_boundary
    );

    restore_conversation("tailchain", None).unwrap();
    assert_eq!(fs::read(&root).unwrap(), original_root);
    assert_eq!(fs::read(&leaf).unwrap(), original_leaf);
}

#[test]
fn an_interrupted_chain_transaction_restores_the_complete_preoperation_state() {
    for abort_after in 1..=3 {
        let sb = Sandbox::new();
        let id = format!("crashchain-{abort_after}");
        let root = lineage_page(&sb, &format!("rollout-30-{id}.jsonl"), &id, None, "ROOT");
        let root_size = fs::metadata(&root).unwrap().len();
        let middle = lineage_page(
            &sb,
            &format!("rollout-31-{id}_mid.jsonl"),
            &id,
            Some((&id, root_size)),
            "MIDDLE",
        );
        let middle_size = fs::metadata(&middle).unwrap().len();
        let leaf = lineage_page(
            &sb,
            &format!("rollout-32-{id}_leaf.jsonl"),
            &id,
            Some(("mid", middle_size)),
            "LEAF",
        );
        let original = [
            fs::read(&root).unwrap(),
            fs::read(&middle).unwrap(),
            fs::read(&leaf).unwrap(),
        ];

        let output = Command::new(env!("CARGO_BIN_EXE_codex-vault"))
            .args(["--json", "--no-progress", "compact-conversation", &id])
            .env("CODEX_HOME", sb.dir.path().join("codex"))
            .env("CODEX_VAULT_HOME", sb.vault())
            .env(
                "CODEX_VAULT_TEST_CHAIN_ABORT_AFTER_REPLACE",
                abort_after.to_string(),
            )
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "fault injection must terminate the child before commit at boundary {abort_after}"
        );
        assert_ne!(
            fs::read(&root).unwrap(),
            original[0],
            "the fault point should occur after at least the first native replacement"
        );

        let refused = compact_conversation(&id, None, CompactOptions::default()).unwrap_err();
        assert!(refused.to_string().contains("interrupted"));
        let recovered = restore_conversation(&id, None).unwrap();
        assert_eq!(recovered["status"], "ok");
        assert_eq!(recovered["recovered_interrupted_transaction"], true);
        assert_eq!(fs::read(&root).unwrap(), original[0]);
        assert_eq!(fs::read(&middle).unwrap(), original[1]);
        assert_eq!(fs::read(&leaf).unwrap(), original[2]);
    }
}
