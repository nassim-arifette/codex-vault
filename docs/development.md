# Development

[Back to the overview](../README.md)

Build from the repository root with a stable Rust toolchain. SQLite and the C runtime are bundled in release executables. Running a release does not require Rust.

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
├── common/mod.rs   shared long lifecycle and recovery-generation assertions
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

## Build on Linux

For a native development build, use `cargo test --locked` and `cargo build --locked --release`.
The downloadable Linux x86_64 package uses musl for a static executable:

```bash
# Debian/Ubuntu build dependencies (not required to run a release)
sudo apt-get install build-essential musl-tools python3
rustup target add x86_64-unknown-linux-musl
bash scripts/package-linux.sh
python3 scripts/test-linux-distribution.py dist/release/codex-vault-0.2.4-linux-x86_64.tar.gz
```

The packager rejects executables with a dynamic interpreter or shared-library dependencies.
`--skip-build` uses the existing musl release binary. `CARGO_TARGET_DIR` is supported for builds
outside the checkout. The archive contains the executable, installer, checksums, license and guides.

The distribution check uses a temporary installation and Codex/Vault profile. It verifies static
ELF headers, readable scan, archive, compact, deep doctor, exact SHA-256 restoration, bundled
SQLite search and verified reads. Python 3.12+ is used for this test, not for running the CLI.
An optional `--real-rollout PATH` exercises recovery on an isolated copy and checks that the
read-only source is unchanged. `--report PATH` writes anonymous results without project paths.
To verify the mounted-filesystem guard in WSL, point `TMPDIR` to a disposable directory on
`/mnt/c` and add `--expect-9p-refusal`. It checks that compact and restore leave the transcript
and recovery files unchanged. The normal lifecycle test belongs on a native Linux filesystem.

## Documentation and releases

The README is the entry point; detailed guides live in `docs/`. Public examples and CI fixtures
must be synthetic. Keep local conversations, indices, raw benchmark logs and project paths out
of commits. Only reviewed, anonymous measurements belong in `docs/validation/`.

For a Windows ZIP with its executable checksum, README, guides, license and installer:

```powershell
.\scripts\Package-Windows.ps1
.\scripts\Test-Distribution.ps1 -Archive .\dist\release\codex-vault-0.2.4-windows-x86_64.zip
```

`-SkipBuild` packages an executable already built under `target/release`. The installer smoke
test uses a temporary profile, verifies bundled runtime dependencies and exercises index,
search and read. It does not change your user PATH or real Codex profile.

`scripts/Test-LocalRelease.ps1` is the separate real-machine check: it downloads a public ZIP,
installs it into the default user directory, updates User PATH and tests recovery on an isolated
copy of a real rollout. `scripts/benchmark.py` generates and verifies 1/5/10 GB rollouts, with
20 GB optional. See [local validation](benchmarks.md) for commands, metrics and limitations.

For a release, update the Cargo version and lockfile, review `RELEASE_NOTES.md`, and push the
tested changes. A matching `vVERSION` tag triggers CI; publication requires Windows/Linux checks,
the pinned Codex differential matrix and fresh-runner installation on both platforms to pass.
The release includes a Windows ZIP, a static Linux x86_64 tarball and one `SHA256SUMS.txt` covering
both archives. Release artifacts include only explicitly selected public files.

[Run the differential harness](differential-testing.md) · [Planned work](roadmap.md)
