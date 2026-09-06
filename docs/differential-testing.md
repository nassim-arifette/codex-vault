# Differential testing

[Back to the overview](../README.md)

The harness compares what Codex reconstructs from the original and compacted transcripts. The public CI corpus is generated from synthetic conversations; private sessions are never CI fixtures.

## Automated checks and compatibility

GitHub Actions runs formatting, Clippy and synthetic unit/integration tests on Windows and Linux.
Windows compatibility jobs generate synthetic conversations and run the differential harness
against pinned Codex versions 0.152.1 and 0.153.4, checking two resumed turns and the read-only
MCP tool catalog. They also replay a long archive/append/compact/restore/recompact lifecycle.
No real conversation is required by CI. Codex downloads are checked against
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
pass. This is a preview release. The project is licensed under [MIT](../LICENSE).

## The differential reconstruction harness

Tests of Vault's own reconstruction model can agree with each other even when that model is wrong.
`tests/differential.rs` tests the model itself, using Codex as the oracle.

For each fixture it resumes the session twice in one throwaway sandbox — once from the original
transcript, once after `compact` — and asserts that the request Codex puts on the wire is
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

**How the capture works.** Codex is pointed at a local mock provider, so no TLS interception or
paid model API is needed. Model requests stay on localhost. Codex may still attempt its own
plugin metadata requests during startup; the harness does not need those requests to succeed.

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
[{ "name": "private-case-1", "session_id": "REPLACE_WITH_SESSION_ID", "path": "C:/private/rollout.jsonl", "expected": "COMPACT_ALLOWED" }]
```

Copy `differential-cases.example.json` to `differential-cases.json` and replace the placeholders
with your own session IDs and paths. Real conversations, local case lists, validation logs,
recovery files, and `AGENTS.md` / `CLAUDE.md` are excluded from Git. The example contains no
conversation data.

Declare `expected` for a reproducible corpus: `COMPACT_ALLOWED`, `COMPACT_REFUSED`,
`ARCHIVE_ONLY` or `READ_ONLY`. A changed classification fails the test. Legacy case lists
without this field still work, but do not freeze the expected behavior. Protected threads
are tested through the refusal test; only allowed threads are resumed by Codex.

`test-differential.ps1` writes a timestamped private log and an anonymous JSONL result file.
When calling Cargo directly, set `CODEX_VAULT_DIFF_REPORT` to a **new** output filename.
Each case records its ordinal alias, expected/observed classification, byte counts and pass
status. Reconstruction results must have two resumed turns per arm and a smaller output.
A missing record, `passed: false`, or nonzero test exit is a failed/incomplete validation.
Reports append so a failure preserves earlier results; use a fresh path for every run.
Raw logs and captured requests can contain private data and must never be published.

## Long lifecycle regression

`tests/common/mod.rs` supplies the same scenario to the ordinary CLI test and the ignored
Codex differential test. It appends turns, writes native checkpoints, archives, grows,
compacts, grows again, indexes and reads historical text, restores an older state, restores
the pre-restore state, compacts again, then rebuilds the index.

At each journal stage it checks that the original anchor and backup bytes remain unchanged,
history retains its exact previous prefix, every backup is listed and reachable, and deep
doctor succeeds. Every distinct recorded state is restored and SHA-256 checked. The final
Codex comparison checks both resumed turns against the final pre-compaction state.

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
[the references in the safety model](safety-model.md#boundaries).
