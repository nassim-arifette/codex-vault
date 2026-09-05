//! Command-line entry point. All logic lives in the `codex_vault` library crate.

use clap::{CommandFactory, Parser, Subcommand};
mod terminal;
use codex_vault::commands::BatchOptions;
use codex_vault::commands::{
    analyze_command, archive_command, compact_safe_command, doctor_command, prune_command,
    restore_command, scan_command,
};
use codex_vault::error::{Result, VaultError};
use codex_vault::ops::CompactOptions;
use codex_vault::parallel::{default_jobs, ProgressMode};
use codex_vault::rollout::DEFAULT_SCAN_WINDOW;
use serde::Serialize;
use serde_json::Value;
use std::io::{self, IsTerminal, Write as IoWrite};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "codex-vault", version, about = "Codex Session Vault MVP")]
struct Cli {
    /// Emit each result as a single compact JSON line instead of pretty-printed JSON, so
    /// output can be piped straight into `jq`, a log, or a JSONL file.
    #[arg(long, global = true)]
    json: bool,

    /// Afficher un compte rendu lisible, meme si la sortie est redirigee.
    #[arg(long, global = true, conflicts_with = "json")]
    human: bool,

    /// Worker threads for the read-only batch commands (`analyze`, `doctor`). `compact-safe`
    /// is always serial.
    #[arg(long, global = true, default_value_t = default_jobs())]
    jobs: usize,

    /// Emit one JSON progress line per finished session on stderr. Defaults to on when stderr
    /// is a terminal.
    #[arg(long, global = true, conflicts_with = "no_progress")]
    progress: bool,

    #[arg(long, global = true)]
    no_progress: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Menu interactif dans le terminal : choisir une conversation et une action.
    Menu {
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Find native Codex JSONL sessions.
    Scan {
        /// Restrict to sessions whose SessionMeta cwd is related to this path.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Analyze whether a session has a provably bounded reconstruction suffix.
    Analyze {
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        #[arg(long = "session")]
        session_flag: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// How many reconstruction-relevant records to retain for the reverse walk. The proof
        /// is bounded, so this caps memory; exhausting it refuses to compact rather than
        /// claiming no cutoff exists.
        #[arg(long, default_value_t = DEFAULT_SCAN_WINDOW)]
        scan_window: usize,
    },
    /// Create an exact zstd backup without changing the native transcript.
    #[command(group(clap::ArgGroup::new("target").args(["session", "session_flag"]).required(true)))]
    Archive {
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        #[arg(long = "session")]
        session_flag: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Keep the immutable original backup and create an extra timestamped snapshot.
        #[arg(long)]
        force: bool,
    },
    /// Keep canonical SessionMeta + the bounded reconstruction suffix, after a verified backup.
    #[command(name = "compact-safe", visible_alias = "compact")]
    CompactSafe {
        /// Preview the net saving, including the compressed backup, without writing files.
        #[arg(long)]
        dry_run: bool,
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        #[arg(long = "session")]
        session_flag: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// See `analyze --scan-window`.
        #[arg(long, default_value_t = DEFAULT_SCAN_WINDOW)]
        scan_window: usize,
        /// Also compact rollouts belonging to threads Codex spawned (sub-agents, guardian
        /// reviews). Refused by default: Codex will not resume them standalone, so their
        /// compaction has not been validated against Codex's own reconstruction.
        #[arg(long)]
        allow_spawned_threads: bool,
    },
    /// Restore an exact recovery state recorded by Codex Vault.
    Restore {
        session: String,
        #[arg(long)]
        cwd: Option<String>,
        /// Restore the first immutable full backup instead of the newest recorded state.
        #[arg(long, conflicts_with = "to")]
        original: bool,
        /// Restore a specific backup. It must be one of the session's recorded anchors.
        #[arg(long, value_name = "BACKUP")]
        to: Option<String>,
        /// List every recovery anchor for this session instead of restoring.
        #[arg(long)]
        list: bool,
    },
    /// Remove leftover scratch files, and optionally backups the manifest does not reference.
    Prune {
        /// Restrict to one session; otherwise every discovered session is considered.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Also remove backups that no manifest anchor points at.
        #[arg(long)]
        unreferenced_backups: bool,
        /// Actually delete. Without this, `prune` only reports what it would remove.
        #[arg(long)]
        apply: bool,
    },
    /// Verify transcript JSON, manifest hashes and backup recoverability.
    Doctor {
        /// Optional session id/path as a positional argument (`doctor <id>`).
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Compatibility form: `doctor --session <id>`.
        #[arg(long = "session")]
        session_flag: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Also decompress every archive and re-parse the transcript, instead of trusting the
        /// verification recorded when each backup was created.
        #[arg(long)]
        deep: bool,
    },
}

/// Successful results go to stdout as pretty JSON; failures go to stderr as a JSON error
/// document with a stable `code`, so both halves of the CLI are scriptable.
fn print_json<T: Serialize>(value: &T, stream: &mut dyn IoWrite, compact: bool) {
    let rendered = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    match rendered {
        Ok(s) => {
            let _ = stream.write_all(s.as_bytes());
            let _ = stream.write_all(
                b"
",
            );
        }
        Err(err) => {
            let _ = writeln!(
                stream,
                "{{\"status\":\"error\",\"code\":\"json_error\",\"message\":{:?}}}",
                err.to_string()
            );
        }
    }
}

fn run(command: Command, batch: BatchOptions) -> Result<Value> {
    match command {
        Command::Menu { .. } => unreachable!("menu handled before JSON commands"),
        Command::Scan { cwd } => scan_command(cwd),
        Command::Analyze {
            session,
            session_flag,
            cwd,
            scan_window,
        } => analyze_command(session.or(session_flag), cwd, scan_window, batch),
        Command::Archive {
            session,
            session_flag,
            cwd,
            force,
        } => archive_command(
            session.or(session_flag).expect("required target"),
            cwd,
            force,
        ),
        Command::CompactSafe {
            dry_run,
            session,
            session_flag,
            cwd,
            scan_window,
            allow_spawned_threads,
        } => compact_safe_command(
            session.or(session_flag),
            cwd,
            CompactOptions {
                dry_run,
                scan_window,
                allow_spawned_threads,
            },
            batch,
        ),
        Command::Prune {
            session,
            cwd,
            unreferenced_backups,
            apply,
        } => prune_command(session, cwd, unreferenced_backups, apply),
        Command::Restore {
            session,
            cwd,
            original,
            to,
            list,
        } => restore_command(session, cwd, original, to, list),
        Command::Doctor {
            session,
            session_flag,
            cwd,
            deep,
        } => {
            let chosen = match (session, session_flag) {
                (Some(a), Some(b)) if a != b => {
                    return Err(VaultError::ConflictingArguments {
                        detail: "doctor received two different session references",
                    })
                }
                (Some(a), _) => Some(a),
                (_, Some(b)) => Some(b),
                (None, None) => None,
            };
            doctor_command(chosen, cwd, deep, batch)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let menu_cwd = match &cli.command {
        Some(Command::Menu { cwd }) => Some(cwd.clone()),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => Some(None),
        None => {
            let _ = Cli::command().print_help();
            return ExitCode::from(2);
        }
        _ => None,
    };
    if let Some(cwd) = menu_cwd {
        if cli.json {
            eprintln!("Le menu est interactif. Utilise une commande directe avec --json.");
            return ExitCode::from(2);
        }
        return match terminal::menu(cwd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(err.exit_code())
            }
        };
    }
    let compact = cli.json;
    let human = cli.human || (!cli.json && io::stdout().is_terminal());
    let batch = BatchOptions {
        jobs: cli.jobs,
        progress: ProgressMode::from_flags(cli.progress, cli.no_progress),
    };
    match run(cli.command.unwrap(), batch) {
        Ok(output) => {
            if human {
                terminal::render(&output);
            } else {
                print_json(&output, &mut io::stdout().lock(), compact);
            }
            ExitCode::from(codex_vault::commands::output_exit_code(&output))
        }
        Err(err) => {
            if human {
                eprintln!("Erreur [{}] : {err}", err.code());
            } else {
                print_json(&err.to_json(), &mut io::stderr().lock(), compact);
            }
            ExitCode::from(err.exit_code())
        }
    }
}
