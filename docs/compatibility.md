# Codex compatibility matrix

[Back to the overview](../README.md) · [Differential harness](differential-testing.md)

Vault treats Codex compatibility as a tested property, not as a consequence of the parser still
compiling. A version is marked **tested** only when the pinned Codex binary passes the synthetic
reconstruction suite, a representative real-rollout corpus, the fresh-writer format audit, and
byte-identity checks for protected/refused sessions.

| Codex | Synthetic | Representative real corpus | Fresh-writer type audit | Refusal behavior | Multi-GB | Status |
| --- | --- | --- | --- | --- | --- | --- |
| 0.150.0 | PASS | PASS (5 cases) | PASS | PASS, byte-identical | Not version-oracled | tested |
| 0.151.0 | PASS | PASS (5 cases) | PASS | PASS, byte-identical | Not version-oracled | tested |
| 0.152.1 | PASS | PASS (5 cases) | PASS | PASS, byte-identical | Not version-oracled | tested |
| 0.153.4 | PASS | PASS (5 cases) | PASS | PASS, byte-identical | Not version-oracled | tested |

The machine-readable record is
[`docs/validation/codex-compatibility.json`](validation/codex-compatibility.json). CI checks that
every `tested` row has all required evidence and that the documented versions exactly match the
GitHub Actions Codex matrix.

## What each column proves

**Synthetic.** The public corpus contains compactable, archive-only and spawned-thread refusal
cases. For compactable sessions the harness compares two consecutive resumed turns from the
original transcript with two from the compacted transcript. It also runs a negative control that
must detect deliberate over-compaction, the long archive/compact/restore lifecycle, and read-only
Vault MCP discovery.

**Representative real corpus.** Five private rollouts are exercised locally: three compactable
sessions spanning small through hundreds-of-megabytes inputs, one archive-only session and one
spawned-thread refusal. Paths, IDs, prompts and captured requests remain private; only anonymous
status/count evidence is recorded publicly.

**Fresh-writer type audit.** The pinned Codex binary starts a brand-new conversation against the
local mock provider. The test reads the rollout that *that binary actually wrote*, verifies its
`session_meta.cli_version` matches the pinned oracle, inventories outer rollout tags,
`event_msg.payload.type` values and response-item types, and fails on anything not explicitly
reviewed. This is deliberately separate from the synthetic fixture's writer version.

**Refusal behavior.** A version does not earn `tested` status merely by reconstructing the happy
path. The corpus must also prove that protected sessions stay byte-identical when Vault refuses
destructive compaction or falls back to archive-only behavior.

**Multi-GB.** Vault's 1/5/10 GB benchmark validates scale, recovery and integrity, but it does not
invoke a pinned Codex reconstruction oracle. It therefore remains useful performance evidence,
not version-specific Codex compatibility evidence, and is shown as `Not version-oracled` rather
than an invented PASS.

## Scope and version provenance

The matrix certifies the declared reconstruction-oracle/corpus combination. It is **not** a claim
that every rollout ever written by a listed release is safe to compact. Transcript
`session_meta.cli_version` records the writer that produced that file; `codex_oracle_version` in
differential reports records the pinned binary used to reconstruct it. Those values are kept
separate so a synthetic fixture cannot masquerade as output from another Codex release.

Unknown top-level rollout item types remain a destructive-compaction stop condition. A new Codex
release is added only after its fresh writer output has been reviewed and the complete evidence
set above has passed.

## Reproducing a version

On Windows, generate the public corpus and pin one official Codex release:

```powershell
$env:CODEX_VAULT_DIFF_CASES = .\scripts\New-SyntheticCorpus.ps1
$env:CODEX_VAULT_CODEX_BIN = .\scripts\Get-TestCodex.ps1 -Version 0.151.0
.\test-differential.ps1
```

For a local real corpus, point `CODEX_VAULT_DIFF_CASES` at a private case list with explicit
`expected` classifications before running the same harness. Never publish that case list, raw
logs or captured model requests.
