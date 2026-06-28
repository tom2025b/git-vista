//! Reads commit history out of a git repository.
//!
//! Stub for the scaffold. The real implementation will use `gix` to open the
//! repo at `path`, walk from HEAD (and every ref tip) newest-first, and map each
//! commit to a [`CommitSummary`]. It stays UI-independent so it can be unit-
//! tested headlessly against fixture repositories.

use std::path::Path;

use thiserror::Error;

use crate::model::CommitSummary;

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("repository walking is not implemented yet")]
    NotImplemented,
}

/// Walk a repository's history, newest commit first, up to `limit` commits.
pub fn walk_history(_path: &Path, _limit: usize) -> Result<Vec<CommitSummary>, RepoError> {
    Err(RepoError::NotImplemented)
}
