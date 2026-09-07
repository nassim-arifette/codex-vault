# Contributing to Codex Vault

Codex Vault treats recovery correctness as the primary constraint. Structural cleanup, feature
work and performance work should stay separable so a reviewer can tell which invariant changed.

## Before opening a change

Read `docs/architecture.md` and keep dependencies flowing from presentation to application to
operations to low-level storage/platform code. New helpers are private or `pub(crate)` by default;
top-level public modules are compatibility facades rather than invitations to expose internals.

For a structural refactor, preserve serialized JSON, persistent manifests/transactions, status and
error strings, verification guarantees and public facade paths. Move code first; change behaviour
in a separate change with dedicated tests.

## Local checks

Run the same core gates as CI:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python scripts/test-benchmark-report.py
```

The ignored differential tests require an external Codex binary/corpus and are exercised by the
compatibility CI job. Platform-specific mutation changes should be validated on both Windows and
Linux before release.

## Tests

Keep private algorithm tests beside their implementation. Integration tests should exercise an
observable recovery, CLI or compatibility contract. Large integration targets should use thematic
submodules below the same target instead of adding many top-level files under `tests/`, because
each top-level file is compiled as a separate integration-test executable.

Any destructive-path change needs coverage for the relevant crash/race/tamper boundary. Do not
remove an integrity check merely because another check usually observes the same bytes.
