Codex Vault 0.2.1 improves the CLI and documentation for the Windows preview.

- The interface is consistently English, including the menu, confirmations and readable reports. Confirm menu actions with `y` or `yes`; Enter and `n` cancel.
- `compact` is the primary command shown in help. `compact-safe` remains a compatible alias. Every command has examples, and all arguments and options have descriptions.
- The shorter English README covers installation, quick start and safety. Detailed CLI, format, testing, recovery and MCP guides are under `docs/` and included in the ZIP.
- The roadmap describes planned improvements, including guided `repair`. Repair is not implemented in this release; use `doctor` to diagnose and `restore` to recover a recorded state.
- CI uses current Node 24 actions pinned to commits and refuses to publish a tag that disagrees with the packaged version.

Download the Windows x86_64 ZIP and SHA256SUMS.txt, verify the ZIP checksum, extract it and run `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1` from the extracted directory. The installer verifies the executable checksum and adds its location to your user PATH. The binary is unsigned and requires no separate Rust, Node, SQLite or Visual C++ runtime installation.

The archive format and derived index schema are unchanged from 0.2.0. Refresh the index explicitly with `codex-vault index` after conversations change. This remains a preview: CI validates its synthetic corpus against Codex 0.152.1 and 0.153.4, not every future transcript format. Public fixtures and examples contain no private conversations or project paths.
