# Local validation and benchmarks

[Back to the overview](../README.md)

These checks use the public Windows **v0.2.1** executable. Synthetic stress tests measure
scale; private real conversations check compatibility and recovery. Their savings are
different measurements and should not be used interchangeably.

## Results: 1, 5 and 10 GB

Measured on 2026-09-06: Windows 11 Pro, build 26200 (kernel 10.0), Intel Core i5-1135G7,
4 cores / 8 logical processors, 11.75 GiB of OS-visible physical memory, NTFS system volume.
All byte-based table units below are decimal. Each size was run once.

| Input | Peak CLI RAM, all operations | Compact | Restore | Net saved, with backups + index | Exact restore / final deep doctor |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GB | 23.2 MB | 7.99 s | 21.14 s | 73.39% | Passed |
| 5 GB | 27.0 MB | 166.75 s | 171.19 s | 73.37% | Passed |
| 10 GB | 27.2 MB | 158.41 s | 346.94 s | 73.37% | Passed |

Every size completed all 15 measured operations, including archive/deep verification,
compaction, exact restore, index, search and verified reads before and after restoration.
Peak memory stayed below 28 MB in this workload; doubling input from 5 to 10 GB changed
the observed maximum from 27.0 to 27.2 MB. This demonstrates bounded memory for these
inputs, not a guarantee for every record size or future format.

The timing variation, including the faster 10 GB compaction than 5 GB compaction, is from
this single ordinary-desktop run with an OS-managed cache. It should not be read as a
comparative throughput result. The 20 GB option is implemented but was not run locally.

[Full per-operation table](validation/stress-windows-0.2.1.md) ·
[JSON: bytes, RAM, I/O, storage and SHA-256 checks](validation/stress-windows-0.2.1.json)

## Reproduce the multi-GB test

Use Windows and Python 3.11 or later. The harness has no third-party Python dependencies.
Point it at an extracted release executable, not a debug build:

```powershell
python scripts/benchmark.py --binary C:/Tools/CodexVault/codex-vault.exe --sizes-gb 1 5 10
# Optional, on a machine with enough free disk space:
python scripts/benchmark.py --binary C:/Tools/CodexVault/codex-vault.exe --sizes-gb 20
```

Sizes are decimal GB (1,000,000,000 bytes), with at most a large record's worth of overshoot.
Each size runs in a new isolated directory. A fixed seed generates user/assistant messages,
tool calls and outputs, turn contexts, Unicode, three checkpoints and repeated turns.
Tool records are normally 128 KiB, with a 2 MiB record every 127 turns; their payload mixes
repeated diagnostics and fresh pseudorandom base64. The newest checkpoint is near 99% of
the target size. These deliberately compressible synthetic files are **not a prediction of
savings on a user's conversations**.

Vault indexes user and assistant messages; tool payloads are streamed but are not included
in full-text search. A corpus dominated by long message text may have different indexing
time and storage costs from this tool-heavy workload.

The sequence measures scan, analyze, archive, deep archive verification, compact dry run,
compact, doctor, deep compact verification, index, search, verified read, restore, final
deep doctor, refreshed index and another verified read. Restore must match the generator's
SHA-256 exactly. Search must retrieve a historical sentinel from a verified backup after
its native prefix is removed, and retrieve the same text again after restoration.

The run exports `report.json`, `report.md` and per-operation metrics under ignored
`validation/`. Each command records elapsed time, Windows peak working set, native input
and output sizes, backup/index bytes, logical storage delta, approximate process I/O and
sampled temporary storage. The case summary includes backup compression ratio and total
net savings. `--keep-data` retains generated rollouts and backups; otherwise only the
generator's own marked data trees are removed after successful verification. Failed runs
retain their data and logs. Never treat an incomplete report as a successful size test.

The disk check reserves 1.4 times the target size plus 2 GB for this generator's compression
profile. A 20 GB run therefore requires at least 30 GB free. This is not a general capacity
estimate for arbitrary or incompressible conversations.

## Measurement limits

- RAM is each CLI process's Windows `PeakWorkingSetSize`, queried through its process
  handle. It excludes Python generation, the operating system and unrelated processes.
- I/O counters measure logical transfers, not physical device traffic. Multiple verification
  passes and the filesystem cache affect both counters and elapsed time.
- Input/output byte columns mean the native transcript's size before/after the command.
  Doctor, index and read may also read compressed backups; use the I/O counters for that cost.
- Storage uses logical file lengths, not NTFS allocation units. Temporary space is sampled
  every 200 ms and may miss a short-lived peak. Logs and generated reports are excluded.
- Commands run sequentially with one Vault worker on an ordinary desktop, with an
  OS-managed cache and no cache flush. This is a local measurement, not a throughput SLA.
- Headline net savings compare the original transcript with the compacted transcript plus
  all retained backups, journal metadata and the first search index. The JSON also preserves
  the compaction-only saving and per-operation storage, including later restore snapshots.

## Five representative real rollouts

All mutations and Codex resumptions operated on isolated copies. Only ordinal aliases,
counts and outcomes are published in [the JSON report](validation/real-rollouts.json).
The private fixture list declares an expected classification for every case.

| Case | Input | Profile | Expected and observed result |
| --- | ---: | --- | --- |
| R01 | 20.4 KB | Short conversation, no checkpoint, older writer | `ARCHIVE_ONLY`; byte-identical |
| R02 | 3.49 MB | 2 checkpoints, 268 tool outputs, rollback, Unicode | `COMPACT_ALLOWED`; two resumed turns match |
| R03 | 38.86 MB | 3 checkpoints, tool output up to 2.71 MB, rollback | `COMPACT_ALLOWED`; two resumed turns match |
| R04 | 278.40 MB | 32 checkpoints, 3,056 tool outputs, largest 11.73 MB, paginated format | `COMPACT_ALLOWED`; two resumed turns match |
| R05 | 25.6 KB | Spawned/sub-agent conversation | `COMPACT_REFUSED`; byte-identical |

The oracle was Codex **0.153.4**. An additional already-compacted paginated rollout passed
the no-op check; it is not counted as a successful reduction. The deliberate over-compaction
negative control was detected. The shared long lifecycle also passed the two-turn Codex
comparison. CI runs its synthetic version with Codex **0.152.1 and 0.153.4**.

This covers small/large conversations, multiple checkpoints, tool-heavy workloads, large
outputs, rollback, spawned threads, Unicode, Windows project paths and older writer versions.
It does not establish comprehensive real-corpus coverage of forks, every pagination layout,
unknown record types or future formats. Growth after archive and multiple restore generations
are exercised by the synthetic lifecycle test. See [the harness](differential-testing.md).

## Real Windows release installation

The GitHub ZIP was downloaded again, its published SHA-256 checked, and `install.ps1` run
against the default per-user installation directory. A new PowerShell process resolved
`codex-vault` using the persistent machine/user PATH, then ran help and a read-only scan of
real Codex sessions. This was a local Windows machine, independently of the CI runner.

The 278.40 MB real copy then survived archive, deep archive verification, compact, deep
verification, indexing, search, verified read, exact restore, final deep doctor and reindex.
Its original source stayed unchanged. Native size after compaction was **3.11 MB**, while
the transcript plus retained backup and metadata occupied **117.05 MB**: **57.96% net saved**.
The search index subsequently occupied 0.93 MB, bringing the total to **117.98 MB** and the
net saving to **57.62% including the index**. This is one real example, not a corpus average.

[Machine-readable release results](validation/windows-release-0.2.1.json)

To repeat this check with your own eligible conversation:

```powershell
./scripts/Test-LocalRelease.ps1 -RealRollout 'C:/private/rollout.jsonl' -Version 0.2.1
```

This maintainer test uses `gh` for the download and **installs the release and updates your
User PATH**. Normal ZIP users do not need `gh`, Python, Rust, Node or external SQLite.
The PE import check and execution passed without an external Visual C++/SQLite/zstd DLL
requirement. The executable is unsigned (`NotSigned`). Command-line installation succeeded;
the interactive browser-download/SmartScreen flow was not exercised, so this test does not
claim that users will never see a warning.

## Documentation visuals

The menu and help previews use actual output from the release executable with synthetic
conversations. They are rendered text captures in SVG/PNG, **not native Windows screenshots**.
Reproduce them with `scripts/capture_cli.py --binary PATH_TO_EXE`; add `--png` with Pillow
installed for raster output. No real project names, paths or messages are included.
