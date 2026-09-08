use clap::{Parser, Subcommand};
use codex_vault::parallel::default_jobs;
use codex_vault::rollout::DEFAULT_SCAN_WINDOW;

#[derive(Parser)]
#[command(
    name = "codex-vault",
    version,
    about = "Recover, verify and safely compact local Codex conversations",
    after_help = "Examples:\n  codex-vault menu\n  codex-vault compact SESSION --dry-run\n  codex-vault storage\n  codex-vault index --cwd .\n  codex-vault search \"authentication tokens\" --cwd .\n  codex-vault read PASSAGE_ID\n\nUse COMMAND --help for details."
)]
pub(super) struct Cli {
    /// Print compact JSON, including in an interactive terminal.
    #[arg(long, global = true)]
    pub(super) json: bool,

    /// Print human-readable output, including when redirected.
    #[arg(long, global = true, conflicts_with = "json")]
    pub(super) human: bool,

    /// Worker threads for the read-only batch commands (`scan`, `analyze`, `doctor`). `compact`
    /// is always serial.
    #[arg(long, global = true, default_value_t = default_jobs())]
    pub(super) jobs: usize,

    /// Emit one JSON progress line per finished session on stderr. Defaults to on when stderr
    /// is a terminal.
    #[arg(long, global = true, conflicts_with = "no_progress")]
    pub(super) progress: bool,

    /// Disable progress messages on stderr.
    #[arg(long, global = true)]
    pub(super) no_progress: bool,

    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Subcommand)]
pub(super) enum Command {
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
    /// Show native, recovery and rebuildable index storage without changing anything.
    #[command(
        after_help = "Example:\n  codex-vault storage\n\nReports native rollout bytes, required recovery anchors, unreferenced or ambiguous backups, recovery metadata and the rebuildable search index. This command never deletes files."
    )]
    #[command(display_order = 11)]
    Storage,
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
    /// Compact every page of one eligible paginated conversation as one coordinated transaction.
    #[command(
        name = "compact-conversation",
        after_help = "Examples:\n  codex-vault compact-conversation THREAD_ID --dry-run\n  codex-vault compact-conversation codex://threads/THREAD_ID\n\nOnly complete linear pagination chains are currently supported. Forks, cycles, missing pages and unknown boundaries are refused before mutation."
    )]
    #[command(display_order = 6)]
    CompactConversation {
        /// Thread ID, codex://threads/ reference, filename stem or full path to any page.
        session: String,
        /// Limit discovery to related project paths.
        #[arg(long)]
        cwd: Option<String>,
        /// Preview the complete chain and storage estimate without changing native files.
        #[arg(long)]
        dry_run: bool,
        /// Maximum reconstruction records retained for each page/prefix proof.
        #[arg(long, default_value_t = DEFAULT_SCAN_WINDOW)]
        scan_window: usize,
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
    /// Restore every page from the exact pre-operation state of the latest chain transaction.
    #[command(
        name = "restore-conversation",
        after_help = "Examples:\n  codex-vault restore-conversation THREAD_ID\n  codex-vault restore-conversation codex://threads/THREAD_ID\n\nThe current complete chain is snapshotted before replacement so the restore is itself recoverable."
    )]
    #[command(display_order = 8)]
    RestoreConversation {
        /// Thread ID, codex://threads/ reference, filename stem or full path to any current page.
        session: String,
        /// Limit discovery to related project paths.
        #[arg(long)]
        cwd: Option<String>,
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
