//! Rebuildable FTS5 index. Recovery remains independent of this derived database.

mod build;
mod identity;
mod ingest;
mod query;
mod schema;
mod scope;
mod sources;

pub use build::{build, status};
pub use query::{read, search};
pub use schema::database_path;
pub use scope::{in_project, project_key};

use crate::error::VaultError;

fn invalid(reason: &str) -> VaultError {
    VaultError::InvalidInput {
        reason: reason.into(),
    }
}
