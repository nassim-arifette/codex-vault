# Development

[Back to the overview](../README.md)

Build from the repository root with a stable Rust toolchain. SQLite and the Windows C runtime are bundled in the release executable. Running a release does not require Rust.

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
├── ops.rs        archive / compact / restore / doctor / prune
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
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

## Documentation and releases

The README is the entry point; detailed guides live in `docs/`. Public examples and CI fixtures
must be synthetic. Keep local conversations, indices, benchmarks and project paths out of commits.

For a Windows ZIP with its executable checksum, README, guides, license and installer:

```powershell
.\scripts\Package-Windows.ps1
.\scripts\Test-Distribution.ps1 -Archive .\dist\release\codex-vault-0.2.1-windows-x86_64.zip
```

`-SkipBuild` packages an executable already built under `target/release`. The installer smoke
test uses a temporary profile, verifies bundled runtime dependencies and exercises index,
search and read. It does not change your user PATH or real Codex profile.

For a release, update the Cargo version and lockfile, review `RELEASE_NOTES.md`, and push the
tested changes. A matching `vVERSION` tag triggers CI; publication requires Windows/Linux checks,
the pinned Codex differential matrix and fresh-runner installation to pass. The release includes
a Windows ZIP and its SHA-256 file. Release artifacts include only explicitly selected public files.

[Run the differential harness](differential-testing.md) · [Planned work](roadmap.md)
