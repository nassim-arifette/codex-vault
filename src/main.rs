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
#[command(
    name = "codex-vault",
    version,
    about = "Recover, verify and safely compact local Codex conversations",
    after_help = "Examples:\n  codex-vault menu\n  codex-vault compact SESSION --dry-run\n  codex-vault index --cwd .\n  codex-vault search \"authentication tokens\" --cwd .\n  codex-vault read PASSAGE_ID\n\nUse COMMAND --help for details."
)]
struct Cli {
    /// Print compact JSON, including in an interactive terminal.
    #[arg(long, global = true)]
    json: bool,

    /// Print human-readable output, including when redirected.
    #[arg(long, global = true, conflicts_with = "json")]
    human: bool,

    /// Worker threads for the read-only batch commands (`analyze`, `doctor`). `compact`
    /// is always serial.
    #[arg(long, global = true, default_value_t = default_jobs())]
    jobs: usize,

    /// Emit one JSON progress line per finished session on stderr. Defaults to on when stderr
    /// is a terminal.
    #[arg(long, global = true, conflicts_with = "no_progress")]
    progress: bool,

    /// Disable progress messages on stderr.
    #[arg(long, global = true)]
    no_progress: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve read-only history search over MCP stdio for Codex and other MCP clients.
    #[command(
        after_help = "Example:\n  codex-vault mcp --cwd C:\\projects\\sample-app\n\nBuild the index first with `codex-vault index`. MCP reads and writes JSON-RPC on stdio."
    )]
    #[command(display_order = 11)]
    Mcp {
        /// Limit every MCP search/read to this project and its subdirectories.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Build or refresh the local full-text index of conversations and verified archives.
    #[command(
        after_help = "Examples:\n  codex-vault index --cwd .\n  codex-vault index --status\n  codex-vault index --rebuild\n\nRun index again after conversations change, compact or restore. Rebuild always covers all projects."
    )]
    #[command(display_order = 8)]
    Index {
        /// Restrict updates to this project and its subdirectories.
        #[arg(long, conflicts_with = "rebuild")]
        cwd: Option<String>,
        /// Rebuild the entire index atomically, including recovery from a corrupt index.
        #[arg(long)]
        rebuild: bool,
        /// Show index size and coverage without changing it.
        #[arg(long, conflicts_with_all=["cwd","rebuild"])]
        status: bool,
    },
    /// Search indexed messages; whitespace-separated terms are combined with AND.
    #[command(
        after_help = "Examples:\n  codex-vault search \"authentication tokens\" --cwd .\n  codex-vault search \"deployment\" --limit 10 --offset 10\n\nRun `codex-vault index` first. Use an ID from the results with `codex-vault read`."
    )]
    #[command(display_order = 9)]
    Search {
        /// Literal words to find together in a message; quote multi-word queries.
        query: String,
        /// Search only this project and its subdirectories.
        #[arg(long)]
        cwd: Option<String>,
        /// Maximum number of matches to return (1-100).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Number of matches to skip for pagination (0-1000000).
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Read an exact indexed passage and verify a backing source against its saved hash.
    #[command(
        after_help = "Examples:\n  codex-vault read PASSAGE_ID\n  codex-vault read PASSAGE_ID --offset 8000 --limit 8000\n\nCopy PASSAGE_ID from search results. Offsets and limits count Unicode characters, not bytes."
    )]
    #[command(display_order = 10)]
    Read {
        /// The 64-character passage ID returned by search.
        id: String,
        /// Refuse passages outside this project and its subdirectories.
        #[arg(long)]
        cwd: Option<String>,
        /// Maximum number of Unicode characters to return (1-32000).
        #[arg(long, default_value_t = 8000)]
        limit: usize,
        /// Number of Unicode characters to skip in the passage.
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Choose a conversation and an action in the terminal.
    #[command(
        after_help = "Examples:\n  codex-vault menu\n  codex-vault menu --cwd C:\\projects\\sample-app\n\nUse /text to filter titles/projects, s to sort by size, and q to quit."
    )]
    #[command(display_order = 1)]
    Menu {
        /// Show conversations whose project path is related to this directory.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Find native Codex JSONL sessions.
    #[command(
        after_help = "Examples:\n  codex-vault scan\n  codex-vault scan --all\n  codex-vault scan --cwd . --paths\n  codex-vault --json scan\n\nReadable output shows the five largest files first. Use Ref with analyze, archive, compact, doctor or restore. JSON output always includes every matching file and its full paths."
    )]
    #[command(display_order = 2)]
    Scan {
        /// Restrict to sessions whose SessionMeta cwd is related to this path.
        #[arg(long)]
        cwd: Option<String>,
        /// Show every matching file in readable output instead of the five largest.
        #[arg(long)]
        all: bool,
        /// Include full project and rollout paths in readable output.
        #[arg(long)]
        paths: bool,
    },
    /// Analyze whether a session has a provably bounded reconstruction suffix.
    #[command(
        after_help = "Examples:\n  codex-vault analyze SESSION_ID\n  codex-vault analyze --cwd .\n\nWithout a session, analyze checks every matching rollout. To estimate backup costs, use compact --dry-run."
    )]
    #[command(display_order = 3)]
    Analyze {
        /// Session ID, filename stem or full .jsonl/.jsonl.zst path; omit for a batch.
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        /// Named alternative to the positional SESSION argument.
        #[arg(long = "session", value_name = "SESSION")]
        session_flag: Option<String>,
        /// Restrict session discovery to project paths related to this directory.
        #[arg(long)]
        cwd: Option<String>,
        /// How many reconstruction-relevant records to retain for the reverse walk. The proof
        /// is bounded, so this caps memory; exhausting it refuses to compact rather than
        /// claiming no cutoff exists.
        #[arg(long, default_value_t = DEFAULT_SCAN_WINDOW)]
        scan_window: usize,
    },
    /// Create an exact zstd backup without changing the native transcript.
    #[command(group(clap::ArgGroup::new("target").args(["session", "session_flag"]).required(true)),
        after_help = "Examples:\n  codex-vault archive SESSION_ID\n  codex-vault archive SESSION_ID --force\n\nThe original backup is immutable. --force records a new snapshot without replacing it.")]
    #[command(display_order = 4)]
    Archive {
        /// Session ID, filename stem or full rollout path to back up.
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        /// Named alternative to the positional SESSION argument.
        #[arg(long = "session", value_name = "SESSION")]
        session_flag: Option<String>,
        /// Limit lookup by ID or filename to related project paths; explicit paths are used directly.
        #[arg(long)]
        cwd: Option<String>,
        /// Keep the immutable original backup and create an extra timestamped snapshot.
        #[arg(long)]
        force: bool,
    },
    /// Safely shorten a rollout after creating a verified recovery snapshot.
    #[command(
        name = "compact",
        visible_alias = "compact-safe",
        after_help = "Examples:\n  codex-vault compact SESSION_ID --dry-run\n  codex-vault compact SESSION_ID\n  codex-vault compact --cwd C:\\projects\\sample-app --dry-run\n\nSpecify a session or --cwd. Direct commands apply without a prompt; use menu for confirmation.\nPreview excludes journal growth. The completed report includes retained backups and metadata."
    )]
    #[command(display_order = 5)]
    CompactSafe {
        /// Preview the net saving, including the compressed backup, without writing files.
        #[arg(long)]
        dry_run: bool,
        /// Session ID, filename stem or full rollout path; omit with --cwd for a batch.
        #[arg(value_name = "SESSION", conflicts_with = "session_flag")]
        session: Option<String>,
        /// Named alternative to the positional SESSION argument.
        #[arg(long = "session", value_name = "SESSION")]
        session_flag: Option<String>,
        /// Batch only rollouts whose own project is inside this directory; filters lookup by ID.
        #[arg(long)]
        cwd: Option<String>,
        /// Maximum reconstruction records retained for analysis; exhaustion refuses compaction.
        #[arg(long, default_value_t = DEFAULT_SCAN_WINDOW)]
        scan_window: usize,
        /// Also compact rollouts belonging to threads Codex spawned (sub-agents, guardian
        /// reviews). Refused by default: Codex will not resume them standalone, so their
        /// compaction has not been validated against Codex's own reconstruction.
        #[arg(long)]
        allow_spawned_threads: bool,
    },
    /// Restore an exact recovery state recorded by Codex Vault.
    #[command(
        after_help = "Examples:\n  codex-vault restore SESSION_ID --list\n  codex-vault restore SESSION_ID --original\n  codex-vault restore SESSION_ID --to C:\\backups\\recorded-snapshot.jsonl.zst\n\nCopy --to paths from --list. Restore saves the current transcript before replacing it."
    )]
    #[command(display_order = 7)]
    Restore {
        /// Session ID, filename stem or full rollout path to restore.
        session: String,
        /// Limit lookup by ID or filename to related project paths; explicit paths are used directly.
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
    #[command(
        after_help = "Examples:\n  codex-vault prune --session SESSION_ID\n  codex-vault prune --session SESSION_ID --apply\n\nReview the dry run before --apply. Referenced recovery snapshots are retained."
    )]
    #[command(display_order = 12)]
    Prune {
        /// Restrict to one session; otherwise every discovered session is considered.
        #[arg(long)]
        session: Option<String>,
        /// Restrict session discovery to project paths related to this directory.
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
    #[command(
        after_help = "Examples:\n  codex-vault doctor SESSION_ID\n  codex-vault doctor SESSION_ID --deep\n  codex-vault doctor --cwd .\n\nWithout a session, doctor checks every matching rollout. It reports problems; it does not repair them."
    )]
    #[command(display_order = 6)]
    Doctor {
        /// Optional session id/path as a positional argument (`doctor <id>`).
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Compatibility form: `doctor --session <id>`.
        #[arg(long = "session", value_name = "SESSION")]
        session_flag: Option<String>,
        /// Restrict session discovery to project paths related to this directory.
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
        Command::Mcp { .. } => unreachable!("MCP uses its own stdio transport"),
        Command::Index {
            cwd,
            rebuild,
            status,
        } => {
            if status {
                codex_vault::index::status()
            } else {
                codex_vault::index::build(cwd.as_deref().map(std::path::Path::new), rebuild)
            }
        }
        Command::Search {
            query,
            cwd,
            limit,
            offset,
        } => codex_vault::index::search(
            &query,
            cwd.as_deref().map(std::path::Path::new),
            limit,
            offset,
        ),
        Command::Read {
            id,
            cwd,
            limit,
            offset,
        } => codex_vault::index::read(&id, cwd.as_deref().map(std::path::Path::new), offset, limit),
        Command::Menu { .. } => unreachable!("menu handled before JSON commands"),
        Command::Scan { cwd, .. } => scan_command(cwd),
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
    if let Some(Command::Mcp { cwd }) = &cli.command {
        return match codex_vault::mcp::serve(
            io::stdin().lock(),
            io::stdout().lock(),
            cwd.as_deref().map(std::path::Path::new),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(err.exit_code())
            }
        };
    }
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
            eprintln!("The menu is interactive. Use a direct command with --json.");
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
    let scan_display = match &cli.command {
        Some(Command::Scan { all, paths, .. }) => Some((*all, *paths)),
        _ => None,
    };
    let batch = BatchOptions {
        jobs: cli.jobs,
        progress: ProgressMode::from_flags(cli.progress, cli.no_progress),
    };
    match run(cli.command.unwrap(), batch) {
        Ok(output) => {
            if human {
                if let Some((all, paths)) = scan_display {
                    terminal::render_scan(&output, all, paths);
                } else {
                    terminal::render(&output);
                }
            } else {
                print_json(&output, &mut io::stdout().lock(), compact);
            }
            ExitCode::from(codex_vault::commands::output_exit_code(&output))
        }
        Err(err) => {
            if human {
                eprintln!("Error [{}]: {err}", err.code());
            } else {
                print_json(&err.to_json(), &mut io::stderr().lock(), compact);
            }
            ExitCode::from(err.exit_code())
        }
    }
}
