# Codex rollout format and compatibility

[Back to the overview](../README.md)

A rollout is a JSONL file that records a Codex conversation. A conversation may span several rollouts. This document describes the format assumptions used by Vault, rather than a public Codex format specification.

## Why the cutoff is conservative

The tested Codex versions reconstruct history with a bounded reverse scan. A safe suffix requires both a compaction checkpoint with `replacement_history` + `window_number` and sufficient completed-turn context. A compaction missing either field, or a rollback marker in the required suffix, forces a scan back to the beginning. Vault mirrors those conditions rather than assuming that “latest `compacted` line” is always enough.

The native JSONL is an envelope such as:

```json
{"timestamp":"...","type":"compacted","payload":{"replacement_history":[...],"window_number":7}}
```

so the implementation parses Codex fields from `payload`. Current `session_meta` fields are flattened inside that payload (`payload.id`, `payload.cwd`); the parser also tolerates the older/nested `payload.meta.*` shape.

## The bounded scan window

The reconstruction proof is a bounded reverse scan, so the analysis retains a window of
reconstruction-relevant records rather than the whole file.

`--scan-window N` (default 100 000 records) sets that retention. If the reverse walk exhausts the
window, the analysis **refuses to compact** and says the window was exhausted, which is a
different statement from "this transcript has no cutoff". For any transcript that fits inside the
window the verdict is identical to retaining everything — `tests/scan_window.rs` asserts that
differentially against the same code run unbounded.

## Pagination and spawned threads

Codex can store one conversation across multiple rollout files. A later page's `history_base`
records a byte offset into its predecessor. Shortening that predecessor invalidates the offset
and can make the thread impossible to resume. Vault refuses to compact a page with a successor,
without an override. Only the newest page can be a compaction candidate. `doctor` reports broken
lineage; restoring a suitable recorded state can recover the required predecessor bytes.

Spawned threads, including sub-agents and guardian reviews, are protected by default. Codex will
not resume them standalone, so the differential oracle cannot validate their reconstruction in
the same way as user threads. `--allow-spawned-threads` is an explicit override for these
unvalidated cases. It does not bypass the pagination guard.

Codex-managed `.jsonl.zst` files can be discovered, analyzed and indexed but are read-only in Vault.
Codex must rematerialize a native JSONL before Vault can modify it.

## Transcript compatibility limitation

The code mirrors the current bounded-scan rules structurally, but it is not linked against Codex's private Rust types and the transcript format is not a stable public API. Before using `compact` on irreplaceable sessions, test `analyze`, `archive`, `doctor`, and `restore` on copies of several real rollouts from your installed Codex version.

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
