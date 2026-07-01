//! Native git-history reading for git-vista.
//!
//! Uses `gix` (pure-Rust gitoxide) to open a repository, seed a revision walk
//! from HEAD plus every ref tip, and traverse newest-first, mapping each commit
//! to a [`CommitSummary`]. This is deliberately a **separate, native-only crate**
//! rather than a module in `git-vista-core`: gix reads a filesystem repo and
//! can't compile for wasm, so keeping it out of `core` lets the browser frontend
//! depend on a clean, wasm-compatible core without any `#[cfg]` gating. The
//! native backend (the Tauri shell today) depends on this crate; the frontend
//! never does.
//!
//! It's UI-independent and headlessly unit-testable against fixture repositories.
//!
//! The work is split into focused modules, all re-exported at the crate root so
//! callers use a flat API (`git_vista_git::walk_history`, etc.):
//!
//! - [`history`] — walking commit history and finding commits present on a remote.
//! - [`refs`]    — reading HEAD, branches and tags, and the checked-out branch.
//! - [`github`]  — turning the `origin` remote URL into a GitHub web base URL.
//!
//! [`CommitSummary`]: git_vista_core::model::CommitSummary

use std::path::PathBuf;

use thiserror::Error;

pub mod github;
pub mod history;
pub mod refs;

pub use github::github_web_base;
pub use history::{read_remote_commits, walk_history};
pub use refs::{read_head_branch, read_refs};

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("could not open a git repository at {path}: {message}")]
    Open { path: PathBuf, message: String },
    #[error("failed to read history: {0}")]
    Walk(String),
}
