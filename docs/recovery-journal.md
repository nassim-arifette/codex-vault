# Recovery journal and storage

[Back to the overview](../README.md)

The journal records which verified backups can restore each rollout. Keep the vault directory with your backups: an archive without its journal loses its recorded recovery references.

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
