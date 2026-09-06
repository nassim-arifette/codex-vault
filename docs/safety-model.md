# Safety model

[Back to the overview](../README.md)

Vault creates and verifies a recovery snapshot before replacing a native transcript. It refuses compaction when it cannot establish the supported reconstruction conditions. These checks reduce risk; they do not make an undocumented transcript format a stable API.

## Recovery safety properties

- streaming processing; large tool outputs are parsed and discarded immediately, and only a bounded window of structural reconstruction records + byte offsets is retained, so analysis memory does not grow with the file;
- `scan` only inspects the head of each transcript for SessionMeta instead of reading every GB;
- process locks acquired before reading the journal, serializing vault mutations with pruning
  and protecting the same transcript even across different vault homes;
- Windows deny-write handle acquired before reading the native transcript;
- SHA-256 of original, compressed backup and compacted result, computed during streaming passes;
- zstd backup is decoded and checked before any native transcript is changed;
- the compaction pass re-hashes the source as it copies, which is what proves no concurrent write slipped in;
- recovery manifest is durably written as `status: prepared` **before** the destructive rename, then committed to `status: ok` only after post-replacement hash/JSON verification;
- temporary-file write + `sync_all()` + handle-based `FileRenameInfoEx` replacement on Windows
  10+ filesystems that support POSIX rename semantics (unsupported systems refuse the operation);
- every scratch file is owned by an RAII guard, so any failure path deletes it instead of stranding megabytes next to the live transcript;
- the replacement temp file is held with a deny-write/exclusive handle across the rename, so the lock follows the replacement inode while verification runs; Windows preserves the native file's DACL, while Linux uses advisory file locks and rename;
- malformed JSON disables `compact`, which then falls back to a verified archive-only snapshot, records that snapshot in the journal, and leaves the native transcript unchanged;
- automatic restore if the compacted file fails post-replacement verification;
- `restore` captures the current transcript and commits its undo anchor in a `prepared` journal
  *before* replacing it, then verifies the live result and commits `ok`; journal errors propagate;
- `prune` checks the union of all recovery journals before judging a backup unreferenced;
  a sibling's legacy backup is retained, and an unreadable journal blocks backup deletion;
- a destructive `--cwd` batch only matches sessions whose own `cwd` is inside the given path;
- Codex-managed `.jsonl.zst` rollouts are discoverable, analyzable and searchable but deliberately read-only; Codex must rematerialize them before Vault mutates them;
- `CODEX_HOME` is respected, and `CODEX_VAULT_HOME` can override Vault storage.

## Storage accounting

`compact --dry-run` estimates the compressed snapshot size without creating files. Its net
estimate excludes journal growth. Completed compactions compare logical bytes before and after
the operation, including retained backups, the index and other vault metadata. This is file
length, not filesystem allocation, compression, deduplication or cloud-storage billing.

A negative `net_saved_bytes` value means the operation increased total usage. Repeated snapshots
remain available for recovery and can reduce or eliminate disk savings. An already-compact
rollout is a no-op with zero savings. `prune` does not remove referenced recovery snapshots.

## Boundaries

On Unix, new vault directories are created with mode `0700`, and sensitive output files with
`0600`, independently of the shell's umask. Indexing also restricts an existing index to `0600`.
Compact and restore preserve the transcript's owner, group and Unix permission bits; inability
to preserve ownership refuses replacement. Extended POSIX ACLs are not copied. Existing custom
vault directories keep their directory permissions; generated contents are private.

Close the relevant Codex session before compaction or restoration. Locks and hash checks protect
the operation itself; they are not a claim that compacting actively used conversations is validated.
Keep the recovery journal with its backups. SQLite is a derived search index and is not needed
to restore a recorded state. Rebuild it with `index --rebuild` if it is lost or corrupt.

The Windows replacement uses filesystem support for the documented rename semantics; unsupported
operations are refused. See Microsoft's
[SetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)
and [FILE_RENAME_INFORMATION](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information).

Linux compaction and restoration check the transcript filesystem before creating a journal or
changing any bytes. They refuse 9p/DrvFS mounts, including Windows drives exposed inside WSL.
Local testing found that a locked replacement on such a mount could succeed and then fail to
reopen for verification. The preflight refusal avoids entering that state. Use the Windows
executable for Windows files or a copy on the Linux filesystem. Read-only operations and dry-run
previews remain available. Linux advisory locks do not prevent writes by programs that ignore
those locks; close the relevant Codex session before a mutation on either platform.

[Codex format assumptions](codex-format.md) · [Journal and interrupted operations](recovery-journal.md)
