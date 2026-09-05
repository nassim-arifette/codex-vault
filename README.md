# Codex Vault

CLI Windows pour sauvegarder, compacter et restaurer les conversations Codex avec verification d'integrite.

## Utilisation simple

Pour Windows x86_64, telecharger le ZIP et `SHA256SUMS.txt` depuis les
[releases](https://github.com/nassim-arifette/codex-vault/releases). Verifier le SHA-256 du ZIP avec
`Get-FileHash`, extraire le ZIP puis lancer `install.ps1`. Ouvrir un nouveau terminal :

```powershell
codex-vault --help
codex-vault menu
```

L'installation se fait dans le profil utilisateur, sans droits administrateur. Rust, Node et une
installation separee de SQLite ou du runtime Visual C++ ne sont pas necessaires. Le binaire Windows est non signe.
Pour compiler depuis les sources, installer Rust stable puis executer `.\build-windows.ps1`.

Codex Vault reste un **CLI**. Pour choisir une conversation sans recopier son identifiant :

```powershell
.\dist\codex-vault.exe menu
# Limiter la liste a un projet :
.\dist\codex-vault.exe menu --cwd C:\chemin\du\projet
```

Le menu affiche les titres, projets, tailles et rollouts. `/texte` recherche un titre ou un
projet, `s` trie par taille et `d` par date. Choisir une conversation permet d'analyser,
sauvegarder, compacter, verifier ou restaurer. Le compactage et la restauration affichent
l'action exacte avant confirmation ; Entree, `n` ou une fin d'entree annulent. Sans argument,
le CLI ouvre aussi le menu s'il est lance dans un terminal interactif.

Les commandes directes restent disponibles (`--session ID` reste accepte) :

```powershell
.\dist\codex-vault.exe analyze ID
.\dist\codex-vault.exe archive ID
.\dist\codex-vault.exe compact ID
.\dist\codex-vault.exe compact ID --dry-run
.\dist\codex-vault.exe doctor ID
.\dist\codex-vault.exe restore ID --original
```

Un chemin `.jsonl` complet peut remplacer l'identifiant, notamment pour une copie dans un
projet du Desktop. Dans un terminal, les comptes rendus sont lisibles ; `--json` fournit la
sortie machine. Une conversation deja compactee retourne `already_compact` sans reecriture
ni changement de la sauvegarde de retour. Les sous-agents et les anciennes pages d'une
conversation paginee restent proteges par defaut.

`compact --dry-run` estimates the new compressed backup without writing files. Its estimate
excludes journal growth. Completed compactions report the actual net change in logical bytes,
including all retained backups and journal files. A negative net saving is reported explicitly;
compaction can reduce the active transcript while increasing total storage. Existing snapshots
are retained for recovery. The menu shows this estimate before confirmation.

A conservative Windows-first CLI for very large Codex session files. The recovery commands are:

- `scan` — discover `.codex/sessions` and `.codex/archived_sessions` rollouts, including Codex-managed `.jsonl.zst`, without reading every multi-GB transcript just to obtain `cwd`;
- `analyze` — stream the transcript and decide whether Codex's current bounded-reconstruction conditions can be proven;
- `archive` — create an exact zstd backup, verify it by decompressing and hashing it, and leave the native transcript unchanged;
- `compact-safe` — preserve the canonical `session_meta` plus the exact bounded reconstruction suffix, after an immutable verified backup;
- `restore` — put back any exact state the recovery journal has recorded;
- `doctor` — verify JSONL, manifest hashes, backup recoverability, and report anything left behind;
- `prune` — remove scratch files from interrupted runs, and optionally backups the journal does not reference.

`analyze` and `doctor` run their batch form across worker threads (`--jobs`, deterministic
output order); `compact-safe` is deliberately serial because it is the destructive one.

The local SQLite FTS5 index is derived from conversations and verified recovery archives.
Recovery does not depend on this index. Hibernation and automatic repair of Codex's own databases
are outside this release.

## Search local history

```powershell
codex-vault index --cwd C:\path\to\project
codex-vault search "authentication tokens" --cwd C:\path\to\project
codex-vault read PASSAGE_ID
codex-vault index --status
codex-vault index --rebuild
```

Run `index` again after conversations change, or after compacting/restoring. It reuses unchanged
sources, updates changed sources transactionally, and removes references to deleted sources.
Busy native transcripts are deferred, retaining their previous indexed snapshot. A full rebuild
requires readable sources and atomically replaces the derived database only after success.
`--rebuild` covers the entire corpus and cannot be combined with `--cwd`.

`search` combines literal whitespace-separated terms with AND and supports `--limit` / `--offset`.
The project filter matches the selected directory and its children, respecting path boundaries.
`read` verifies a backing file's SHA-256 and returns exact text plus source line and decoded byte
offset. Passage IDs remain stable when the same message moves from a native rollout into an
archive. Repeated copies in snapshots are deduplicated; all indexed occurrences retain references.
Search results describe the last indexed snapshot, not a live filesystem view.

The index covers user and assistant text messages, including event-message variants. It excludes
tool payloads, images and instruction envelopes. Records larger than 16 MiB are skipped and
counted explicitly. `read --offset N --limit N` paginates text by Unicode characters.
The database lives in the Vault directory as `index.sqlite`; SQLite is bundled in the executable.
`index --status` reports the index size, retained vault bytes and coverage. All indexed content
remains local; publishing the repository does not publish your index or conversations.

## Use from Codex through MCP

Build the index first, then register the installed executable with Codex:

```powershell
codex-vault index --cwd C:\path\to\project
codex mcp add vault -- codex-vault mcp --cwd C:\path\to\project
```

Use the absolute executable path in the registration command if it is not on `PATH`.
The stdio server exposes only `vault_search` and `vault_read`. It never indexes or modifies
transcripts, archives or the database. Its optional `--cwd` is an upper bound: tool arguments
can narrow that scope but cannot widen it. Returned history is explicitly marked as untrusted
data, with verified source references. Re-run the CLI `index` command to refresh the snapshot.

The server supports MCP protocol versions `2025-11-25`, `2025-06-18` and `2025-03-26`.
There is no HTTP listener or separate database service. CLI commands remain usable without MCP.

## Automated checks and compatibility

GitHub Actions runs formatting, Clippy and synthetic unit/integration tests on Windows and Linux.
Windows compatibility jobs generate synthetic conversations and run the differential harness
against pinned Codex versions 0.152.1 and 0.153.4, checking two resumed turns and the read-only
MCP tool catalog. No real conversation is required by CI. Codex downloads are checked against
the official release asset SHA-256. The standard test suite does not make model API calls.

To reproduce the synthetic matrix on Windows:

```powershell
$env:CODEX_VAULT_DIFF_CASES = .\scripts\New-SyntheticCorpus.ps1
$env:CODEX_VAULT_CODEX_BIN = .\scripts\Get-TestCodex.ps1 -Version 0.153.4
.\test-differential.ps1
```

The version downloader uses an authenticated GitHub CLI (`gh`). Compatibility applies to the
tested cases and does not establish safety for every future Codex format. Release tags publish
a Windows ZIP only after checks, compatibility tests and a fresh-runner installation smoke test
pass. This is a preview release. The project is licensed under [MIT](LICENSE).

## Why the cutoff is conservative

The tested Codex versions reconstruct history with a bounded reverse scan. A safe suffix requires both a compaction checkpoint with `replacement_history` + `window_number` and sufficient completed-turn context. A compaction missing either field, or a rollback marker in the required suffix, forces a scan back to the beginning. Vault mirrors those conditions rather than assuming that “latest `compacted` line” is always enough.

The native JSONL is an envelope such as:

```json
{"timestamp":"...","type":"compacted","payload":{"replacement_history":[...],"window_number":7}}
```

so the implementation parses Codex fields from `payload`. Current `session_meta` fields are flattened inside that payload (`payload.id`, `payload.cwd`); the parser also tolerates the older/nested `payload.meta.*` shape.

## Layout

The crate is a library plus a thin binary. Integration tests and the differential compatibility
harness exercise the same operations as the CLI.

```text
src/
├── format.rs     rollout envelope → semantic record kinds
├── rollout.rs    reading .jsonl / .jsonl.zst, structural scan, JSONL validation
├── analysis.rs   the bounded-reconstruction proof
├── hashing.rs    streaming SHA-256 and zstd
├── fsatomic.rs   atomic replace, locking, the RAII temp-file guard
├── manifest.rs   the typed, versioned recovery journal
├── backup.rs     verified backups and recovery anchors
├── ops.rs        archive / compact-safe / restore / doctor / prune
├── discovery.rs  finding sessions, resolving references, --cwd scoping
├── parallel.rs   ordered parallel batches and stderr progress
├── commands.rs   CLI-facing JSON wrappers
├── storage.rs    net storage accounting and read-only compaction estimates
├── index.rs      rebuildable SQLite FTS5 index and verified passage retrieval
├── mcp.rs        scoped read-only MCP tools over stdio
├── terminal.rs   readable output and optional interactive CLI menu
├── error.rs      the typed error domain and its exit classes
└── main.rs       argument parsing and process exit

tests/
├── analysis.rs     format-tolerance and cutoff-proof specifications
├── cli.rs          executable commands, menu and exit-code contracts
├── destructive.rs  end-to-end coverage of everything that can lose data
├── scan_window.rs  proof that the bounded scan matches retaining everything
└── differential.rs the reconstruction harness: Codex as the oracle (ignored by default)
```

## Build on Windows

Install the stable Rust toolchain, then from this folder:

```powershell
.\build-windows.ps1
```

The executable will be placed at:

```text
dist\codex-vault.exe
```

Or build directly:

```powershell
cargo test
cargo clippy --all-targets
cargo build --release
```

## First-use workflow

Start read-only:

```powershell
.\codex-vault.exe scan
.\codex-vault.exe scan --cwd .
.\codex-vault.exe analyze --cwd .
```

Analyze one session by the actual Codex session id, filename stem, or full JSONL path:

```powershell
.\codex-vault.exe analyze --session <SESSION_ID>
```

Create a backup only:

```powershell
.\codex-vault.exe archive --session <SESSION_ID>
```

Compact safely (`compact` is an alias of `compact-safe`):

```powershell
.\codex-vault.exe compact-safe --session <SESSION_ID>
.\codex-vault.exe compact --cwd .
```

The batch form processes only sessions whose own `cwd` is **inside** the given path. A bare `compact` with neither `--session` nor `--cwd` is refused so it cannot rewrite every Codex session by accident.

A rollout that another page of the same thread **continues from** is refused outright, with no
override. Codex stores a long thread as several rollout files, and each page records a byte
offset into the one before it (`history_base.end_byte_offset`); shortening such a page makes
Codex refuse the entire thread with *"invalid paginated history lineage: cutoff byte offset is
past the source rollout"*. Only the newest page of a thread can be compacted. `doctor` reports a
lineage that is already broken, and `restore` repairs it.

Rollouts belonging to threads Codex *spawned* — sub-agents and guardian reviews — are refused by
default, and skipped rather than failed in a batch. Codex will not resume such a rollout on its
own ("cannot resume an unloaded multi-agent v2 sub-agent through its parent"), so the
differential harness cannot check that compacting one preserves what the model sees; their
`session_meta` also carries `subagent_history_start_ordinal`, which suggests a parent replays a
child's history by position. `--allow-spawned-threads` overrides this.

Verify afterward:

```powershell
.\codex-vault.exe doctor <SESSION_ID>
.\codex-vault.exe doctor --deep <SESSION_ID>
# `doctor --session <SESSION_ID>` is also accepted
```

The standard pass verifies each archive's bytes against the journal and trusts the decompression
check made when the backup was created; `--deep` decompresses every archive and re-parses the
transcript. The standard pass skips decompression, and both catch a modified archive.

Restore:

```powershell
.\codex-vault.exe restore <SESSION_ID> --list      # every state that can be put back
.\codex-vault.exe restore <SESSION_ID>             # the newest recorded state
.\codex-vault.exe restore <SESSION_ID> --original  # the first immutable backup
.\codex-vault.exe restore <SESSION_ID> --to <BACKUP>
```

Clean up after an interrupted run:

```powershell
.\codex-vault.exe prune --session <SESSION_ID>          # dry run: reports only
.\codex-vault.exe prune --session <SESSION_ID> --apply
```

## Batch commands

`analyze` and `doctor` with no `--session` process every discovered session:

```powershell
.\codex-vault.exe --jobs 8 doctor --cwd .
.\codex-vault.exe --progress analyze --cwd .
```

- `--jobs N` sets the worker count for these read-only commands. Output order comes from the
  session list, so `--jobs 8` is byte-identical to `--jobs 1`.
- A session that cannot be read produces an error row rather than aborting the whole batch.
- `--progress` emits one JSON line per finished session on **stderr**, leaving stdout a single
  JSON document. It is on by default when stderr is a terminal; `--no-progress` forces it off.
- `compact-safe` ignores `--jobs`: the destructive path runs one session at a time.

## The bounded scan window

The reconstruction proof is a bounded reverse scan, so the analysis retains a window of
reconstruction-relevant records rather than the whole file.

`--scan-window N` (default 100 000 records) sets that retention. If the reverse walk exhausts the
window, the analysis **refuses to compact** and says the window was exhausted, which is a
different statement from "this transcript has no cutoff". For any transcript that fits inside the
window the verdict is identical to retaining everything — `tests/scan_window.rs` asserts that
differentially against the same code run unbounded.

## Output contract

Results are printed to **stdout** as readable text in an interactive terminal, or pretty JSON
when redirected. `--json` always emits a compact JSON document; `--human` forces readable text
even in a pipe. A batch remains one document containing its result rows.

Failures are printed to **stderr** as a JSON document with a stable `code`, never as a Rust
error dump, and the process exit code says which class of failure it was:

This JSON error format applies when redirected or with `--json`; interactive errors are readable
text. Completed integrity reports (`doctor` warnings, failed restores) and batch error rows also
set a nonzero exit code. Expected batch skips, including a page needed by its successor, do not.
An error after replacement explicitly reports `native_transcript_changed: true` and names the
recovery journal; it never claims that the transcript was left untouched.

```json
{
  "status": "error",
  "code": "session_not_found",
  "message": "no session matching `abc`",
  "details": { "reference": "abc" },
  "native_transcript_changed": false
}
```

`compact-safe` on a spawned thread exits with `spawned_thread_refused`.

| exit | meaning                                                  |
| ---- | -------------------------------------------------------- |
| 0    | success                                                   |
| 1    | internal invariant violation                              |
| 2    | invalid request (bad arguments, unsupported session kind) |
| 3    | session or backup not found                               |
| 4    | integrity failure: a hash, size or JSON check did not pass |
| 5    | the session is in use, or changed underneath the operation |
| 6    | filesystem error                                          |

## Storage

```text
%USERPROFILE%\.codex-vault\
├── index.sqlite
├── backups\
│   ├── SESSION.original.jsonl.zst
│   ├── SESSION.precompact-TIMESTAMP.jsonl.zst
│   ├── SESSION.prerestore-TIMESTAMP.jsonl.zst
│   └── SESSION.snapshot-TIMESTAMP.jsonl.zst
├── manifests\
│   └── SESSION.json
└── summaries\
    └── SESSION.md
```

The `*.original.jsonl.zst` backup is immutable. Every other backup the vault writes is recorded
in the manifest's `history` and is therefore reachable through `restore --list` / `restore --to`;
`doctor` reports any backup on disk that the journal does *not* reference.

## One vault entry per rollout file

The vault is keyed on the **rollout file**, not on the Codex thread id, because a thread spans
several files: `rollout-<timestamp>-<thread>_<fork>.jsonl`. Keying on `session_meta.id` made two
such files share one manifest and one "immutable original", and `restore --original` on the
second then wrote the first one's transcript into it — reporting `ok`, with `doctor` reporting
`ok` afterwards because the file did match *a* recorded state, just the wrong one.

The key is the rollout's stem, which carries a timestamp plus one or two UUIDs. A manifest still
stored under the old thread-id key is adopted **only if it names the same file**; one belonging
to a sibling rollout is ignored, and `doctor` audits backups under both the current and the
legacy prefix so nothing written by an earlier version becomes invisible.

## The recovery journal

`manifests\SESSION.json` is a typed, versioned document (`manifest_version: 2`; the original flat
v1 layout is upgraded on load). Its shape matters because it is the only thing that can undo a
destructive operation:

- `original` and `restore` are **recovery anchors** — each carries the backup path, the archive's
  SHA-256, *and* the decompressed content's SHA-256 and size. Verifying only the archive proves
  the file is intact, not that it holds the content the journal claims;
- every field a safety check consults is required. A manifest that cannot be deserialized is a
  refusal, not a silently skipped verification;
- `history` records every operation and every backup it captured, so no verified snapshot can
  become unreachable;
- `status` moves `prepared` → `ok`. A journal left at `prepared` means a compaction was
  interrupted before it committed; `doctor` reports that as a warning and `restore` still knows
  the exact pre-compaction state.

## Recovery safety properties

- streaming processing; large tool outputs are parsed and discarded immediately, and only a bounded window of structural reconstruction records + byte offsets is retained, so analysis memory does not grow with the file;
- `scan` only inspects the head of each transcript for SessionMeta instead of reading every GB;
- process locks acquired before reading the journal, serializing vault mutations with pruning
  and protecting the same transcript even across different vault homes;
- Windows deny-write handle acquired before reading the native transcript;
- SHA-256 of original, compressed backup and compacted result — computed during the passes that already read the file, so a compaction traverses the transcript three times rather than seven;
- zstd backup is decoded and checked before any native transcript is changed;
- the compaction pass re-hashes the source as it copies, which is what proves no concurrent write slipped in;
- recovery manifest is durably written as `status: prepared` **before** the destructive rename, then committed to `status: ok` only after post-replacement hash/JSON verification;
- temporary-file write + `sync_all()` + handle-based `FileRenameInfoEx` replacement on Windows
  10+ filesystems that support POSIX rename semantics (unsupported systems refuse the operation);
- every scratch file is owned by an RAII guard, so any failure path deletes it instead of stranding megabytes next to the live transcript;
- the replacement temp file is held with a deny-write/exclusive handle across the rename, so the lock follows the replacement inode while verification runs; the native file's DACL is preserved;
- malformed JSON disables `compact-safe`, which then falls back to a verified archive-only snapshot, records that snapshot in the journal, and leaves the native transcript unchanged;
- automatic restore if the compacted file fails post-replacement verification;
- `restore` captures the current transcript and commits its undo anchor in a `prepared` journal
  *before* replacing it, then verifies the live result and commits `ok`; journal errors propagate;
- `prune` checks the union of all recovery journals before judging a backup unreferenced;
  a sibling's legacy backup is retained, and an unreadable journal blocks backup deletion;
- a destructive `--cwd` batch only matches sessions whose own `cwd` is inside the given path;
- Codex-managed `.jsonl.zst` rollouts are discoverable, analyzable and searchable but deliberately read-only; Codex must rematerialize them before Vault mutates them;
- `CODEX_HOME` is respected, and `CODEX_VAULT_HOME` can override Vault storage.

## Transcript compatibility limitation

The code mirrors the current bounded-scan rules structurally, but it is not linked against Codex's private Rust types and the transcript format is not a stable public API. Before using `compact-safe` on irreplaceable sessions, test `analyze`, `archive`, `doctor`, and `restore` on copies of several real rollouts from your installed Codex version.

Each manifest pins the Codex build the transcript came from, read out of its own `session_meta`
(`cli_version`, alongside `originator`, `source`, `history_mode` and the context window id).
That is the build which actually wrote the file; the installed `codex` may be many versions
newer. `codex_version_source` records where the value came from, and both the operation result
and `doctor` say so when it had to fall back to the installed CLI or found nothing at all — a
manifest that cannot pin the transcript layout to a build is a compatibility risk, not a neutral
absence.

`CODEX_VAULT_CODEX_VERSION` supplies a version when the transcript carries none;
`CODEX_VAULT_CODEX_BIN` points at an explicit absolute `codex` executable. The fallback probe
resolves `PATH` itself, accepting only absolute entries, so the current directory can never
supply the binary.

## The differential reconstruction harness

Every other test in this repo checks the vault against *the vault's own model* of Codex. If that
model is wrong, both sides of such a test are wrong identically and it still passes.
`tests/differential.rs` tests the model itself, using Codex as the oracle.

For each fixture it resumes the session twice in one throwaway sandbox — once from the original
transcript, once after `compact-safe` — and asserts that the request Codex puts on the wire is
the same both times. Codex is a black box: nothing here depends on its internal Rust types.

Each arm now resumes **two consecutive turns**, comparing both requests. All related rollout
pages are copied into the sandbox, and both `CODEX_HOME` and `CODEX_VAULT_HOME` are isolated for
Vault operations. The second arm resets Codex's auxiliary files as well as the original rollout.
Missing executables, missing fixtures and a nonzero Codex exit fail validation rather than
silently passing. Already compacted files are tested as no-ops, not counted as successful reductions.

```powershell
cargo test --test differential -- --ignored --nocapture --test-threads=1
# Same suite with a timestamped validation log:
.\test-differential.ps1
```

**How the capture works.** Codex is pointed at a local mock provider, so there is no TLS
interception, no API cost and no network:

```text
codex exec resume <ID> "ping" --skip-git-repo-check
  -c model_provider=mock
  -c 'model_providers.mock={name="mock",base_url="http://127.0.0.1:<port>/v1",
                            wire_api="responses",env_key="OPENAI_API_KEY"}'
```

The mock answers with a minimal but *valid* `response.created` / `response.output_item.done` /
`response.completed` event stream. Dropping the connection instead would make Codex retry.

**Four properties make it trustworthy.**

1. *One sandbox, two runs, sequentially.* The developer prompt embeds absolute paths — skill
   roots under `CODEX_HOME`, the working directory — so two parallel sandboxes would differ
   before reconstruction is even considered. Resuming also appends a turn, so the compaction
   restarts from the pristine copy.
2. *An allowlist, not a wildcard.* Only `client_metadata` and, per context element, `id` and
   `internal_chat_message_metadata_passthrough` are treated as volatile. Anything else that
   differs fails the test, so a field a future Codex adds cannot silently hide a regression.
3. *A negative control.* `the_harness_detects_an_over_compaction` cuts one line before the proven
   cutoff — the `compacted` record carrying `replacement_history` — and *requires* the comparison
   to fail. A harness that has never gone red could be comparing a file with itself.
4. *No vacuous passes.* A compacted case must actually shrink and must yield a non-empty context.
   For sessions the vault refuses, comparing reconstructions would pass trivially because nothing
   changed, so the assertion there is the refusal itself plus byte-identity.

**Fixtures** come from the live `CODEX_HOME` (read-only; every one is copied into a sandbox
first), or from a JSON file named by `CODEX_VAULT_DIFF_CASES` / `differential-cases.json`:

```json
[{ "name": "nominal-large", "session_id": "01a0…", "path": "C:/…/rollout-….jsonl" }]
```

Copy `differential-cases.example.json` to `differential-cases.json` and replace the placeholders
with your own session IDs and paths. Real conversations, local case lists, validation logs,
recovery files, and `AGENTS.md` / `CLAUDE.md` are excluded from Git. The example contains no
conversation data.

Only **user** threads are usable. Codex refuses to resume a spawned one — *"cannot resume an
unloaded multi-agent v2 sub-agent through its parent"*. `scan` reports `thread_source` and
`is_spawned_thread` so they can be filtered. Validation reports and real-corpus measurements
remain local and are excluded from Git.

## What is still not covered

The harness has a bounded scope: two pinned Codex versions and two consecutive resumed turns per compacted synthetic session
are checked in CI. This does not cover every transcript variant or future Codex release.
Codex-managed `.jsonl.zst` sessions remain read-only. Long-running live workloads and more Codex
versions remain useful additions to the compatibility matrix.

The Windows rename behavior is documented by Microsoft in
[SetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)
and [FILE_RENAME_INFORMATION](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information).
