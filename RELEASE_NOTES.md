Codex Vault 0.2.3 adds a downloadable Linux x86_64 package alongside Windows.

- The Linux tarball includes a static musl executable with SQLite and zstd bundled, a checksum-verifying installer, the MIT license and documentation. Install into `~/.local/bin` without administrator privileges or a development toolchain.
- CI builds both platforms, checks the Linux executable for dynamic runtime dependencies, and tests installation from each archive on fresh runners before publication. A shared `SHA256SUMS.txt` covers the Windows ZIP and Linux tarball.
- The README now presents Windows and Linux installation and starts with direct CLI commands. The optional `menu` command remains available; its navigation and presentation are on the roadmap for improvement.
- The readable scan improvements from v0.2.2 remain: five largest files by default, `--all` for every result, `--paths` for full paths and complete JSON output for scripts.
- Local WSL testing exposed a replacement-verification failure on Windows-mounted drives. Linux `compact` and `restore` now refuse 9p/DrvFS mounts before any mutation. Use the Windows executable for Windows conversations; native Linux files and WSL's Linux filesystem use the Linux package.

On Linux, download the `.tar.gz` and `SHA256SUMS.txt` into the same directory, run `sha256sum --check --ignore-missing SHA256SUMS.txt`, then extract the archive and run `sh install.sh`. Add `~/.local/bin` to PATH if needed. WSL2 uses the same package. On Windows, verify the ZIP's SHA-256, extract it and run `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1`.

The archive format, index schema and reconstruction rules are unchanged. This remains a preview: Windows and Linux x86_64 are shipped; macOS and ARM64 are not yet packaged. The README's 1/5/10 GB storage measurements were performed with the v0.2.1 Windows executable.

Local validation: Ubuntu 24.04 / WSL2 passed installation, static ELF checks, SQLite search and exact recovery of synthetic and 278.40 MB real-copy workloads on the Linux filesystem. The original real source stayed unchanged. A separate regression check verifies that Linux compact/restore refuse a Windows-drive mount without changing either the transcript or recovery files. Anonymous results are linked from `docs/benchmarks.md`.
