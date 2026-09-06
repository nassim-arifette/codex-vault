Codex Vault 0.2.2 makes conversation discovery easier to read.

- `scan` shows the five largest matching rollout files first, with aligned sizes, conversation titles and short project names.
- `scan --all` shows every matching file, still sorted by size. `scan --paths` includes full project and rollout paths.
- Each summary row includes a copyable reference. Conversations with multiple rollout pages use a filename stem to distinguish them.
- The summary counts all matching files and explains how to see the rest. JSON output remains complete, including when redirected; scripts keep the existing metadata and ordering.
- CLI help and the English documentation describe the new options. The archive format, index schema and compaction behavior are unchanged.

Download the Windows x86_64 ZIP and SHA256SUMS.txt, verify the ZIP checksum, extract it and run `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1` from the extracted directory. Installation verifies the executable checksum and adds its location to your user PATH. To update an existing installation, close running Vault/MCP processes first. The binary is unsigned and requires no separate Rust, Node, SQLite or Visual C++ runtime installation.

This remains a preview. The README documents the 1/5/10 GB and real-rollout validation performed with v0.2.1; those measurements were not repeated for this presentation change.
