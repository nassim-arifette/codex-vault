//! Human-facing terminal output and optional menu. All actions use the same CLI core.
use codex_vault::analysis::analyze_session;
use codex_vault::discovery::{
    discover_sessions, lineage_successors, parse_filter, resolve_session_reference,
};
use codex_vault::error::Result;
use codex_vault::ops::{
    archive_impl, compact_safe_impl, doctor_one, list_anchors, restore_impl, DoctorDepth,
    RestoreTarget,
};
use codex_vault::rollout::{is_codex_zstd_jsonl, read_session_head};
use codex_vault::util::format_size;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn short(text: &str, width: usize) -> String {
    let text = clean(text);
    if text.chars().count() <= width {
        text
    } else {
        format!(
            "{}…",
            text.chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn label(status: &str) -> &str {
    match status {
        "ok" => "OK",
        "already_compact" => "Deja compacte : aucune modification necessaire",
        "exists" => "Sauvegarde deja presente",
        "snapshot_created" => "Nouvelle sauvegarde creee",
        "archived_only" => "Sauvegarde effectuee ; compactage non applicable",
        "warning" => "Verification : points a examiner",
        "failed" | "verification_failed" => "ECHEC DE VERIFICATION",
        "restored_after_failed_verification" => "Compactage annule ; etat precedent restaure",
        "skipped_lineage_source" => "Page conservee : une page suivante en depend",
        "skipped_spawned_thread" => "Sous-agent conserve",
        "read_only_native_zstd" => "Deja compresse par Codex (lecture seule)",
        other => other,
    }
}

pub fn render(value: &Value) {
    if let Some(rows) = value.as_array() {
        for row in rows {
            render(row);
        }
        return;
    }
    if let Some(rows) = value.get("sessions").and_then(Value::as_array) {
        if let Some(total) = value.get("total_size_human").and_then(Value::as_str) {
            println!("{} fichiers de conversation — {}", rows.len(), total);
        }
        for row in rows {
            render(row);
        }
        return;
    }
    if value.get("file_stem").is_some() {
        let size = value["size_bytes"].as_u64().unwrap_or(0);
        println!(
            "{} | {} | {}",
            format_size(size),
            clean(
                value["title"]
                    .as_str()
                    .unwrap_or(value["session_id"].as_str().unwrap_or("Conversation"))
            ),
            clean(value["cwd_hint"].as_str().unwrap_or(""))
        );
        println!("  {}", clean(value["path"].as_str().unwrap_or("")));
        return;
    }
    if let Some(session) = value.get("session").and_then(Value::as_str) {
        println!("\n{}", clean(session));
    }
    if let Some(result) = value.get("result") {
        render(result);
        return;
    }
    if let Some(a) = value.get("analysis") {
        if a["can_compact"] == true && a["estimated_removed_bytes"] == 0 {
            println!("Deja compacte : aucun octet a retirer.");
            return;
        }
        if a["can_compact"] == true {
            println!(
                "Structure compactable : {} recuperables sur {}.",
                format_size(a["estimated_removed_bytes"].as_u64().unwrap_or(0)),
                format_size(a["original_size_bytes"].as_u64().unwrap_or(0))
            );
            println!("Les protections de pagination, de sous-agent et de fichier ouvert restent appliquees a l'execution.");
        } else {
            println!("Compactage non applicable ; la sauvegarde reste disponible.");
        }
        for reason in a["reasons"].as_array().into_iter().flatten() {
            if let Some(s) = reason.as_str() {
                println!("  {}", clean(s));
            }
        }
        return;
    }
    if let Some(status) = value["status"].as_str() {
        println!("{}", label(status));
    }
    for key in ["reason", "notes"] {
        for item in value[key].as_array().into_iter().flatten() {
            if let Some(s) = item.as_str() {
                println!("  {}", clean(s));
            }
        }
    }
    for key in ["error", "message", "note"] {
        if let Some(s) = value[key].as_str() {
            println!("{}", clean(s));
        }
    }
    if let Some(backup) = value["backup"].as_str() {
        println!("Sauvegarde : {}", clean(backup));
    }
    if let Some(stats) = value.get("stats") {
        if let Some(storage) = stats.get("storage") {
            let net = storage["net_saved_bytes"].as_i64().unwrap_or(0);
            println!(
                "Gain net, sauvegardes et journaux compris : {}{}",
                if net < 0 { "-" } else { "" },
                format_size(net.unsigned_abs())
            );
            if storage["space_increased"] == true {
                println!("Attention : cette operation augmente l'espace total occupe.");
            }
        }
        if let Some(plan) = stats.get("storage_preview") {
            let net = plan["estimated_net_saved_bytes_excluding_metadata"]
                .as_i64()
                .unwrap_or(0);
            println!(
                "Nouvelle sauvegarde : {}. Gain net estime : {}{} (hors croissance du journal).",
                format_size(plan["new_backup_bytes"].as_u64().unwrap_or(0)),
                if net < 0 { "-" } else { "" },
                format_size(net.unsigned_abs())
            );
            if plan["may_increase_usage"] == true {
                println!("Attention : le total peut augmenter, sauvegarde comprise.");
            }
        }
        if let (Some(before), Some(after)) =
            (stats["input_size"].as_u64(), stats["result_size"].as_u64())
        {
            println!(
                "{} → {} ({} recuperables sur le fichier actif)",
                format_size(before),
                format_size(after),
                format_size(before.saturating_sub(after))
            );
        }
    }
    if let Some(anchors) = value["anchors"].as_array() {
        for (i, a) in anchors.iter().enumerate() {
            println!(
                "{}. {} | {}{}",
                i + 1,
                a["source_size_human"].as_str().unwrap_or(""),
                clean(a["backup_path"].as_str().unwrap_or("")),
                if a["is_current_restore_target"] == true {
                    " [dernier etat sauvegarde]"
                } else {
                    ""
                }
            );
        }
        if anchors.is_empty() {
            println!("Aucune sauvegarde enregistree.");
        }
    }
    if let Some(candidates) = value["candidates"].as_array() {
        for path in candidates {
            println!("  {}", clean(path.as_str().unwrap_or("")));
        }
    }
}

fn prompt(text: &str) -> Result<Option<String>> {
    print!("{text}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

fn show_action(action: Result<Value>) {
    match action {
        Ok(value) => render(&value),
        Err(err) => eprintln!("Erreur [{}] : {}", err.code(), clean(&err.to_string())),
    }
}

fn conversation(path: &Path) -> Result<()> {
    loop {
        println!("\nConversation : {}", clean(&path.display().to_string()));
        println!("1. Analyser\n2. Sauvegarder l'etat actuel\n3. Compacter avec sauvegarde automatique\n4. Verifier les sauvegardes et la conversation\n5. Restaurer une sauvegarde\n0. Retour");
        let Some(choice) = prompt("Action > ")? else {
            return Ok(());
        };
        match choice.as_str() {
            "0" | "q" => return Ok(()),
            "1" => show_action(analyze_session(path).map(|a| json!({"analysis": a}))),
            "2" => show_action(archive_impl(path, true).map(|v| json!(v))),
            "3" => {
                let head = read_session_head(path)?;
                if is_codex_zstd_jsonl(path)
                    || head.provenance.is_spawned_thread()
                    || !lineage_successors(&head.session_id, &head.page_id).is_empty()
                {
                    println!("Ce fichier est protege du compactage (compression Codex, sous-agent ou page suivie). Tu peux l'analyser ou verifier ses sauvegardes.");
                    continue;
                }
                let analysis = analyze_session(path)?;
                if analysis.can_compact && analysis.estimated_removed_bytes == Some(0) {
                    println!("Deja compacte : aucune modification necessaire.");
                    continue;
                }
                if !analysis.can_compact {
                    render(&json!({"analysis": analysis}));
                    continue;
                }
                let preview = codex_vault::ops::compact_safe_impl_with(
                    path,
                    codex_vault::ops::CompactOptions {
                        dry_run: true,
                        ..Default::default()
                    },
                )?;
                render(&json!(preview));
                if prompt("Compacter cette conversation ? [o/N] > ")?
                    .is_some_and(|s| s.eq_ignore_ascii_case("o"))
                {
                    show_action(compact_safe_impl(path).map(|v| json!(v)));
                }
            }
            "4" => show_action(doctor_one(path, DoctorDepth::Deep).map(|v| json!(v))),
            "5" => {
                let states = list_anchors(path)?;
                render(&states);
                let anchors = states["anchors"].as_array().cloned().unwrap_or_default();
                if anchors.is_empty() {
                    continue;
                }
                let Some(input) = prompt("Numero de la sauvegarde (0 = retour) > ")? else {
                    return Ok(());
                };
                let selected = input
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| anchors.get(n));
                if let Some(anchor) = selected {
                    let backup = PathBuf::from(anchor["backup_path"].as_str().unwrap());
                    println!(
                        "Restaurer {}. L'etat actuel sera sauvegarde pour pouvoir annuler.",
                        clean(&backup.display().to_string())
                    );
                    if prompt("Restaurer cette conversation ? [o/N] > ")?
                        .is_some_and(|s| s.eq_ignore_ascii_case("o"))
                    {
                        show_action(
                            restore_impl(path, RestoreTarget::Backup(backup)).map(|v| json!(v)),
                        );
                    }
                } else if input != "0" {
                    println!("Numero invalide.");
                }
            }
            _ => println!("Choisis un numero de 0 a 5."),
        }
    }
}

pub fn menu(cwd: Option<String>) -> Result<()> {
    let filter = parse_filter(cwd)?;
    let mut sessions = discover_sessions(filter.as_deref())?;
    let mut query = String::new();
    let mut page = 0usize;
    let mut include_spawned = false;
    let mut by_size = true;
    const PAGE: usize = 12;
    loop {
        let total_size: u64 = sessions.iter().map(|s| s.size_bytes).sum();
        let mut rows: Vec<_> = sessions
            .iter()
            .filter(|s| include_spawned || !s.is_spawned_thread)
            .filter(|s| {
                format!(
                    "{} {} {}",
                    s.title.as_deref().unwrap_or(""),
                    s.cwd_hint.as_deref().unwrap_or(""),
                    s.session_id
                )
                .to_lowercase()
                .contains(&query)
            })
            .collect();
        if by_size {
            rows.sort_by_key(|s| std::cmp::Reverse(s.size_bytes));
        } else {
            rows.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
        }
        page = page.min(rows.len().saturating_sub(1) / PAGE);
        println!(
            "\nCODEX VAULT — {} au total\n{} rollouts affiches — page {}/{}",
            format_size(total_size),
            rows.len(),
            page + 1,
            rows.len().div_ceil(PAGE).max(1)
        );
        for (i, s) in rows.iter().enumerate().skip(page * PAGE).take(PAGE) {
            let date = chrono::DateTime::from_timestamp(s.modified_at as i64, 0)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%d/%m %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            println!(
                "{:>3}. {:>8} | {} | {}{}",
                i + 1,
                format_size(s.size_bytes),
                date,
                short(s.title.as_deref().unwrap_or(&s.session_id), 58),
                if s.is_spawned_thread {
                    " [sous-agent]"
                } else {
                    ""
                }
            );
            println!("     {}", short(&s.file_stem, 110));
            println!(
                "     {}",
                short(s.cwd_hint.as_deref().unwrap_or("Projet inconnu"), 110)
            );
        }
        println!("Numero = ouvrir | /texte = rechercher un titre ou projet | n/p = pages | r = actualiser\ns = taille | d = date | a = afficher/masquer les sous-agents | f = fichier | q = quitter");
        let Some(input) = prompt("Choix > ")? else {
            return Ok(());
        };
        match input.as_str() {
            "q" | "0" => return Ok(()),
            "n" => page += 1,
            "p" => page = page.saturating_sub(1),
            "r" => sessions = discover_sessions(filter.as_deref())?,
            "s" => {
                by_size = true;
                page = 0;
            }
            "d" => {
                by_size = false;
                page = 0;
            }
            "a" => {
                include_spawned = !include_spawned;
                page = 0;
            }
            "f" => {
                if let Some(reference) = prompt("Chemin du fichier .jsonl (Entree = retour) > ")? {
                    if reference.is_empty() {
                        continue;
                    }
                    match resolve_session_reference(reference.trim_matches('"'), None)
                        .and_then(|p| conversation(&p))
                    {
                        Ok(()) => {}
                        Err(err) => eprintln!("{}", clean(&err.to_string())),
                    }
                }
            }
            _ if input.starts_with('/') => {
                query = input[1..].to_lowercase();
                page = 0;
            }
            _ => {
                if let Some(session) = input
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| rows.get(n))
                {
                    if let Err(err) = conversation(&session.path) {
                        eprintln!("{}", clean(&err.to_string()));
                    }
                } else {
                    println!("Choix invalide.");
                }
            }
        }
    }
}
