//! Codex Vault: a conservative archive and compaction library for Codex JSONL sessions.
//!
//! The crate is split so that the destructive half (`backup`, `fsatomic`, `manifest`, `ops`)
//! can be exercised directly from integration tests and from the differential compatibility
//! harness that will replay Codex's own reconstruction.

pub mod analysis;
pub mod backup;
pub mod commands;
pub mod discovery;
pub mod error;
pub mod format;
pub mod fsatomic;
pub mod hashing;
pub mod manifest;
pub mod ops;
pub mod parallel;
pub mod paths;
pub mod rollout;
pub mod util;
