//! Atomic replacement, advisory locking, temporary-file management and transcript rewrites.

mod lock;
mod platform;
mod rewrite;
mod temp;

pub use lock::{MultiMutationGuard, MutationGuard};
pub use platform::{
    atomic_replace, create_private_file, ensure_supported_mutation_filesystem, file_identity,
    lock_session, path_file_identity, FileIdentity,
};
pub use rewrite::{
    copy_compacted_paginated_page, copy_compacted_transcript, CompactionCopy,
    PaginatedCompactionCopy,
};
pub use temp::{stale_temp_files, temp_path_for, TempFile, TEMP_SUFFIX};
