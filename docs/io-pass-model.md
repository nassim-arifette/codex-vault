# Full-file I/O pass model

[Back to local validation](benchmarks.md)

This document is the source-level companion to the PERF-001 benchmark. It records where Vault
streams an entire rollout or recovery archive, which passes also hash/parse/compress data, and
which apparently redundant reads are deliberate race or integrity barriers.

## Counting rules

`N` means the current native rollout, `B` a compressed recovery backup and `R` a materialized
result/temp file. A `×1` is one complete sequential content pass. Small head reads, directory
walks, manifest/SQLite metadata reads and `stat` calls are excluded because they do not scale with
the full transcript size in the same way.

The counts below describe the important successful/no-op paths in the current implementation.
Conditional work is shown explicitly; a command can do less when it exits early or more when it
must create a new recovery snapshot. Hash counts are logical integrity computations, not extra I/O
when marked as piggybacked on a read that already had to happen.

| Operation / path | Full sequential reads | Full sequential writes | Compression / decompression | Full-file hashing and why |
| --- | --- | --- | --- | --- |
| `scan` | none guaranteed | none | none | none; discovery reads bounded head/metadata only |
| `analyze` | `N ×1` | none | decoder only for read-only native `.jsonl.zst` | native SHA-256 is computed during the analysis scan, so parsing + hashing share one pass |
| `archive` creating an original/snapshot | `N ×2`, then `B ×1` | `B ×1` compressed | compress `×1`, decompress `×1` | compression hashes `N`; a second `N` hash closes the append-after-EOF race; the single `B` read now hashes both compressed bytes and decoded content |
| `archive` when immutable original already exists | no full content pass | none | none | early no-op after bounded head/journal checks |
| `compact --dry-run` | `N ×1`, plus either `B ×1` to prove a reusable original or `N ×1` to estimate a new compressed backup | none | optional backup decompress or compression-to-counter | analysis hash is piggybacked; preview intentionally performs enough work to make the storage estimate meaningful |
| `compact` with an already-valid matching recovery anchor | `N ×4`, `B ×1`, compacted `R ×3` | compacted `R ×1` | `B` decompress `×1` | analysis hashes `N`; backup raw+decoded hashes share `B ×1`; compaction copy hashes source+result while copying; two later native hashes guard the pre-replacement race windows; result JSON verification and post-replace hash checks remain separate |
| `compact` when a new pre-compaction backup is required | previous row + `N ×2`, new `B ×1` | previous row + new compressed `B ×1` | + compress `×1`, decompress `×1` | new backup uses the same source-race and raw+decoded archive verification as `archive` |
| `compact` already compact | `N ×1` | none | none | analysis scan/hash only |
| `restore` to an already-recorded state, without an undo snapshot | current `N ×1`, target `B ×2`, restored `R ×3` | restored `R ×1` | target decompress `×1` | target compressed hash, current-state hash, temp content hash and post-replace active hash are independent checks; JSON verification is another result read |
| `restore` when current bytes differ | previous row + current `N ×2`, new undo `B ×1`, plus an optional current-state prefix read | previous row + compressed undo `B ×1` | + compress `×1`, undo-backup decompress `×1` | the extra reads create and verify a reversible pre-restore snapshot before any native replacement; the prefix proof distinguishes append-only growth from replacement |
| `restore --list` | none | none | none | manifest-only operation |
| `doctor` standard, exact recorded state | up to `N ×1`; each `B ×1` | none | none | native lineage prefix hash proves exact/append-only descent; each compressed backup hash checks stored bytes; JSON reparse is deliberately skipped when byte-identical |
| `doctor --deep` | up to `N ×2`; each `B ×2` | none | each backup decompress `×1` | adds native JSON reparse and decoded backup hash/size verification to the standard checks |
| `index`, unchanged source | each source `×1` | SQLite only | none for native source | source hash is enough to reuse unchanged indexed rows |
| `index`, changed source | each changed source `×3` | SQLite only | the ingestion pass decodes `.zst` backups | initial hash detects change; ingestion parses dialogue; final hash proves the source did not change during ingestion |
| `search` | none | none | none | FTS/SQLite only |
| `read` | one full raw hash per candidate backing source until one verifies | none | none | exact backing-source hash is the stale-index safety check; passage text itself comes from SQLite |
| `storage` / `prune` | none | none except deletions | none | metadata/journal classification only; neither command scans transcript contents |
| `compact-conversation`, terminal page | `N ×6`, new `B ×1`, result `R ×2` | `B ×1`, `R ×1` | compress `×1`, backup decompress `×1` | analysis + explicit plan hash + backup source checks + rewrite + pre-commit hash; result is JSON-verified before replacement and hashed after replacement |
| `compact-conversation`, predecessor page | `N ×5` plus one declared-prefix analysis read, new `B ×1`, result `R ×2` | `B ×1`, `R ×1` | compress `×1`, backup decompress `×1` | same transaction proof as the terminal page, but bounded reconstruction analyzes only the successor-declared prefix |
| `restore-conversation`, per page | current `N ×3`, undo `B ×1`, target `B ×1`, restored `R ×2` | undo `B ×1`, restored `R ×1` | compress `×1`, decompress `×2` | snapshots current page, verifies undo backup in one pass, materializes target, hashes temp and then committed page |

Whole-conversation totals are the sum of these per-page paths across the proven linear chain. PERF-001
does not yet publish a chain workload, so this document records source-level pass counts but does not
invent an empirical amplification ratio for them.

## What PERF-001 already measures

The published v0.2.1 1/5/10 GB Windows run predates the optimization below, but it is useful as the
empirical baseline. Logical process reads normalized by the original generated rollout were very
stable across sizes:

| Operation | Observed logical read amplification | Observed logical write amplification |
| --- | ---: | ---: |
| `analyze` | `1.000×` | ~`0×` |
| `archive` | `2.496×` | `0.248×` |
| `doctor` after archive, deep | `2.496×` | ~`0×` |
| `compact --dry-run` | `1.248×` | ~`0×` |
| `compact` | `2.526×` | `0.010×` |
| `doctor` after compact, deep | `0.516×` | ~`0×` |
| initial `index` | `0.780–0.821×` | `0.008–0.048×` |
| verified `read` from backup | `0.248×` | ~`0×` |
| `restore` | `3.521×` | `1.002×` |
| final deep `doctor` | `2.501×` | ~`0×` |
| post-restore `index` | `3.297–3.351×` | `0.004–0.005×` |

Across that complete benchmark lifecycle, the three sizes consumed about `21.13–21.23×` the
original rollout size in logical reads and `1.27–1.31×` in logical writes. These ratios describe
that deliberately compressible fixture and the v0.2.1 code; they are observations, not an SLA or a
hard regression threshold. Compression ratio, source mix and later safety hardening all change the
absolute transfer count.

New schema-v3 benchmark reports serialize `logical_read_amplification` and
`logical_write_amplification` for every operation. The denominator is always the original generated
rollout size for the case. It is intentionally *not* the command-local native size: after compaction
the native file is tiny while commands can still verify a full recovery source, and using the tiny
denominator would make the ratio misleading.

## Reduction made for PERF-002

Verified backup creation used to read the finished `.zst` twice: one decompression pass to prove
that decoded bytes hash to the source, then a second raw pass to record the compressed archive
SHA-256. `verify_zstd_archive` now wraps the archive reader with a SHA-256 reader while the zstd
decoder hashes decoded output. One physical archive read therefore proves both properties:

1. stored compressed bytes have a recorded SHA-256;
2. decompressed bytes hash exactly to the source and have the expected size.

The source is **still re-hashed after compression**. That separate native pass closes the race where
Codex appends after the compressor observes EOF, so removing it would weaken the concurrency
guarantee. The optimization removes only a redundant archive read-back, not a safety barrier.

The same one-pass verification is reused when compaction/archive revalidates an existing immutable
backup. Future PERF-001 runs will show the actual end-to-end effect; the historical v0.2.1 numbers
above are intentionally left unchanged rather than retroactively adjusted.

A 0.02 GB release-build smoke run on 2026-09-08 confirms the expected change on the same synthetic
fixture shape: `archive` now reports `2.248×` logical reads versus the historical `2.496×`. The
`0.248×` difference is exactly one compressed-backup-sized read for this fixture. The same smoke
reports `compact = 4.550×` reads; that higher current number reflects the additional P1 race-closing
native rehashes described in the table above, which were added after the v0.2.1 large-file run.
See the [smoke Markdown](validation/perf002-smoke-0.2.4.md) and
[JSON](validation/perf002-smoke-0.2.4.json).

## Remaining amplification worth investigating

The largest remaining candidates all need an equivalent proof before any read disappears:

- `doctor --deep` currently reads each compressed backup once for the raw archive hash and once to
  hash decoded content. It can eventually use the same dual-hash reader after the concurrent
  doctor changes settle.
- `doctor` can hash several candidate recorded prefixes by reopening the native rollout for each
  candidate. A single monotonic scan that snapshots SHA state at the sorted candidate boundaries
  could preserve every prefix proof while reducing the worst case from the sum of candidate prefix
  lengths to at most one native pass.
- Successful compact/restore verifies JSON and hashes the materialized result in separate full
  passes. A parser that also returns the byte hash could merge those reads without dropping either
  assertion.
- Changed-source indexing does `hash -> ingest -> hash`. The final hash is a race detector, not
  accidental duplication. It can only be merged if ingestion hashes the exact same raw source bytes
  (including compressed bytes for backup sources) while it parses decoded records.
- Verified `read` intentionally hashes a backing source before returning indexed text. Removing that
  pass would make stale references silently trusted; caching such verification would require a
  separate, proven invalidation model.

The rule for future optimization is therefore simple: count physical passes first, identify which
proof each pass contributes, and merge passes only when the replacement computes every equivalent
proof over the same bytes and preserves the same race boundary.
