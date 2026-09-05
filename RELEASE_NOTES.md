Codex Vault 0.2.0 is a Windows preview release under the MIT license.

- Preview net storage savings with `compact --dry-run`; completed operations count retained backups and journal growth.
- Build a local SQLite FTS5 index with `index`, search it with `search`, and retrieve exact passages with hash-verified references using `read`.
- Connect Codex through the read-only MCP stdio server with `mcp`. A project scope cannot be widened by tool arguments.
- Recovery archives remain independent of the rebuildable index. Text is deduplicated across native rollouts and snapshots.

Download the Windows x86_64 ZIP and SHA256SUMS.txt. Verify the ZIP checksum, extract it, and run `install.ps1` in the extracted directory. The installer verifies the executable checksum and adds the installation directory to your user PATH. Rust, Node and a separate SQLite installation are not required to run the executable. The Windows binary is unsigned.

CI exercises synthetic conversations against Codex 0.152.1 and 0.153.4, including two resumed turns, refusal cases, a negative control and MCP discovery. These checks establish compatibility for their test cases, not every future transcript format. Compaction of active conversations is not validated. Spawned threads and Codex-managed compressed rollouts remain protected by default.

The index covers user and assistant text messages. Tool payloads, images and instruction envelopes are excluded; records over 16 MiB are skipped and counted. Run `index` after conversations change. Real conversations, project paths, local benchmark reports and indices are never included in release artifacts.
