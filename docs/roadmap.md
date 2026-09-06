# Roadmap

[Back to the overview](../README.md)

These are planned improvements, not promises of a release date or features already available.
The current CLI provides archive, compact, doctor, restore, search and read-only MCP access.

## Guided repair

A future `repair` command should help with explicitly supported cases of damage or interrupted
operations. The intended workflow is to diagnose the problem, show a proposed plan, preserve
the current state and apply only changes whose recovery behavior has been tested.

The first step is a fixture matrix defining what can be repaired and what must be refused.
Repair must not invent missing conversation content or silently discard unknown records.
It should report what was changed and what remains unresolved, with an undo path.

Today, use `doctor` to diagnose and `restore` to recover a state in the journal. There is no
`repair` command, and rebuilding Vault's index does not repair Codex's own databases.

## Everyday use

- Make refreshing the search index after compaction/restoration easier to discover and run.
- Explain the cumulative storage cost of repeated snapshots and offer reviewable retention
  choices that respect recovery references.
- Improve navigation between a search result, its rollout and its available recovery states.

## Compatibility and testing

- Add Codex versions as they are validated against the reconstruction oracle.
- Expand synthetic coverage for pagination, forks, rollbacks and long histories of different sizes.
- Exercise longer sequences of conversation growth, compact, reindex and restore.
- Keep unsupported layouts protected until their reconstruction and recovery behavior is understood.

## Distribution

Windows is the first downloadable target. Native Linux/macOS packages and Windows code signing
are future work. Linux CI is useful coverage but is not the same as validating every supported
filesystem, installation method or running Codex environment.
