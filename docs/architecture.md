# Codex Vault architecture

Codex Vault is intentionally kept as one Cargo package for now. The executable and library are
one product with one release cadence; module boundaries are used to isolate responsibilities
without introducing workspace/package overhead before there is a real independent consumer.

## Dependency direction

The intended dependency flow is:

```text
binary CLI / terminal / MCP adapters
                |
                v
        application commands
                |
                v
      operations / chain flows
                |
                v
 recovery model + rollout analysis
                |
                v
 filesystem / hashing / SQLite / platform
```

Lower layers must not depend on presentation layers. In particular:

- domain/recovery code must not depend on Clap or terminal rendering;
- filesystem primitives must not depend on operations;
- hashing must not depend on commands or presentation;
- storage and index code must not depend on terminal rendering;
- platform-specific `unsafe` code stays behind safe filesystem/process APIs.

## Module responsibilities

### `ops/`

Single-rollout mutation and verification flows. Each operation owns its orchestration while
journal/manifest construction is shared through the private `ops::shared` module.

- `archive`: capture an exact verified recovery source.
- `compact`: bounded safe compaction and its recovery protocol.
- `restore`: restore a recorded anchor while preserving the state being replaced.
- `doctor`: integrity and lineage checks.
- `prune`: conservative cleanup of scratch/unreferenced files.
- `journal`: common manifest/journal state transitions used by single-file and chain operations.
- `types`: stable operation result/options types exposed by the `ops` facade.

### `chain/`

Whole-conversation operations over a linear pagination chain.

- `plan`: prove every page/prefix before mutation and build immutable plans.
- `transaction`: durable multi-page transaction records and rollback helpers.
- `compact`: coordinated chain compaction.
- `restore`: coordinated reversible chain restore.
- `types`: transaction and prepared-page data shared by the private chain modules.

The persisted chain transaction schema is versioned data, not an internal scratch format. Moving
its Rust type does not permit changing its serialized fields or semantics.

### `fsatomic/`

Safe low-level mutation primitives only.

- `temp`: RAII temporary files and stale-temp discovery.
- `lock`: process/session mutation guards and source locking.
- `replace`: atomic replacement and file identity checks.
- `rewrite`: transcript rewrite/copy passes.
- `platform`: platform-specific filesystem implementation, including all filesystem `unsafe`.

Callers use the safe facade re-exported from `fsatomic::mod`; platform modules are private.

### `index/`

The SQLite index is derived/rebuildable and never participates in recovery correctness.

- `schema`: schema/opening/database path.
- `scope`: project path normalization and containment.
- `sources`: discovery of native and verified recovery sources.
- `ingest`: bounded JSONL ingestion and visible-message extraction.
- `build`: atomic refresh/rebuild orchestration.
- `query`: status/search/read paths.

### `storage/`

Logical storage measurement and reporting.

- `measure`: reusable directory/vault size primitives.
- `inventory`: read-only storage inventory.
- `preview`: compression-size estimates and compaction preview accounting.
- `process`: process high-water memory measurement.

Operation-scoped accounting belongs above these primitives. P3 must not reintroduce whole-vault
scans into operation hot paths merely because inventory reporting can perform them.

### Binary `cli/`

The binary is a presentation adapter, not part of recovery correctness.

- `args`: Clap declarations only.
- `dispatch`: parsed command to application/library call.
- `output`: JSON/human output and exit handling.
- `terminal`: interactive menu and human renderer.

`src/main.rs` should remain a minimal call into this adapter.

## Public API policy

Top-level public modules are compatibility facades. Their internal submodules are private and only
the symbols used by the executable, tests, differential harness or plausible library consumers are
re-exported. New helpers default to `pub(crate)` or private; `pub` requires an external reason.

Refactors preserve existing public paths unless an API break is explicitly planned for a release.

## Testing policy

- Unit tests live next to implementation when they verify private algorithms or invariants.
- Integration tests validate observable library/CLI/recovery behaviour.
- Large integration targets are split into Rust submodules instead of adding many top-level files;
  every top-level file under `tests/` becomes a separate test binary.
- Destructive tests must continue to cover crash boundaries, concurrent writers, exact restores,
  tampered backups, pagination transactions and permission/atomic-replacement behaviour.

## Refactor safety rules

Structural changes are behaviour-neutral:

1. no JSON field, status string, error code or persistent manifest/transaction format changes;
2. no integrity verification pass is removed as part of a file move;
3. no optimisation is mixed into a module-extraction commit;
4. `cargo fmt`, Clippy and the full non-external test suite must remain green at each major stage;
5. Windows and Linux platform code stays behind the same safe contracts.

Internally, decisions should use typed enums/structs when the set of states is known. Existing
serialized `serde_json::Value` payloads remain at compatibility boundaries until they can be
replaced without changing the CLI/MCP JSON contract; callers should not spread stringly-typed
status interpretation when a typed helper can centralize it.

## When to introduce a Cargo workspace

Stay with one package until a component has an independent release/dependency boundary. Examples
that would justify a workspace later are a separately versioned reusable core library, a standalone
MCP server, or another binary that needs only a strict subset of dependencies. File size alone is
not a reason to create crates.
