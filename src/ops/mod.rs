//! Single-rollout archive, compaction, restore, verification and cleanup operations.

mod archive;
mod compact;
mod doctor;
mod prune;
mod restore;
mod shared;
mod types;

pub use archive::archive_impl;
pub use compact::{
    compact_safe_impl, compact_safe_impl_with, compact_safe_impl_within, CompactOptions,
};
pub use doctor::{doctor_one, DoctorCheck, DoctorDepth};
pub use prune::prune_one;
pub use restore::{list_anchors, restore_impl, RestoreTarget};
pub use types::CommandResult;

pub(crate) use shared::{commit_chain_page_manifest, prepare_chain_page_manifest};
pub(crate) use types::CommandStatus;
