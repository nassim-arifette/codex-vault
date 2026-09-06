# Search and MCP

[Back to the overview](../README.md)

Vault indexes local history in its own SQLite database. Search and read are available in the CLI and through two read-only MCP tools for Codex.

## Search local history

```powershell
codex-vault index --cwd C:\path\to\project
codex-vault search "authentication tokens" --cwd C:\path\to\project
codex-vault read PASSAGE_ID
codex-vault index --status
codex-vault index --rebuild
```

Run `index` again after conversations change, or after compacting/restoring. It reuses unchanged
sources, updates changed sources transactionally, and removes references to deleted sources.
Busy native transcripts are deferred, retaining their previous indexed snapshot. A full rebuild
requires readable sources and atomically replaces the derived database only after success.
`--rebuild` covers the entire corpus and cannot be combined with `--cwd`.

`search` combines literal whitespace-separated terms with AND and supports `--limit` / `--offset`.
The project filter matches the selected directory and its children, respecting path boundaries.
`read` verifies a backing file's SHA-256 and returns exact text plus source line and decoded byte
offset. Passage IDs remain stable when the same message moves from a native rollout into an
archive. Repeated copies in snapshots are deduplicated; all indexed occurrences retain references.
Search results describe the last indexed snapshot, not a live filesystem view.

The index covers user and assistant text messages, including event-message variants. It excludes
tool payloads, images and instruction envelopes. Records larger than 16 MiB are skipped and
counted explicitly. `read --offset N --limit N` paginates text by Unicode characters.
The database lives in the Vault directory as `index.sqlite`; SQLite is bundled in the executable.
`index --status` reports the index size, retained vault bytes and coverage. All indexed content
remains local; publishing the repository does not publish your index or conversations.

## Use from Codex through MCP

Build the index first, then register the installed executable with Codex:

```powershell
codex-vault index --cwd C:\path\to\project
codex mcp add vault -- codex-vault mcp --cwd C:\path\to\project
```

Use the absolute executable path in the registration command if it is not on `PATH`.
The stdio server exposes only `vault_search` and `vault_read`. It never indexes or modifies
transcripts, archives or the database. Its optional `--cwd` is an upper bound: tool arguments
can narrow that scope but cannot widen it. Returned history is explicitly marked as untrusted
data, with verified source references. Re-run the CLI `index` command to refresh the snapshot.

The server supports MCP protocol versions `2025-11-25`, `2025-06-18` and `2025-03-26`.
There is no HTTP listener or separate database service. CLI commands remain usable without MCP.
