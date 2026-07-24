//! Rebase endpoints (Issue #33 follow-up): rebase the checked-out branch onto
//! main (`POST /api/rebase`) and the live gate `GET /api/rebase-status` that tells
//! the menu whether a rebase would do anything right now. Both resolve the base
//! (`origin/main` if present, else `main`) through the shared [`rebase_base`].

use std::path::Path;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_protocol::{GitOperation, RebaseStatus, RefName};

use crate::git_cmd::{git_ref_exists, is_ancestor, rev_parse};
use crate::planner;
use crate::state::{current, reject_if_read_only};

/// Rebase the checked-out branch onto main (Issue #33 follow-up): `git rebase
/// <base>` via [`GitOperation::RebaseOntoBase`]. `<base>` is `origin/main` when
/// that remote-tracking ref exists — the usual feature-branch target, so you
/// rebase onto the freshest pushed main — and the local `main` otherwise. It
/// acts on HEAD, so it takes no request body.
///
/// A failed rebase (almost always conflicts) would leave the repo mid-rebase,
/// which a browser-only user with no shell can't resolve — so the executor
/// runs `git rebase --abort` on failure to restore the pre-rebase state, then
/// forwards git's own error text so the UI can explain why.
pub(crate) async fn rebase() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let base = rebase_base(&repo).await;
    let base = RefName::new(base).expect("'origin/main' and 'main' are valid ref names");
    planner::plan_and_execute(GitOperation::RebaseOntoBase { base }).await
}

/// The base "Rebase onto main" rebases onto: `origin/main` when that
/// remote-tracking ref exists — the usual feature-branch target, so you rebase
/// onto the freshest pushed main — and the local `main` otherwise. Shared by
/// the rebase handler and `/api/rebase-status`, so the menu's gate always
/// describes exactly what the rebase would do.
async fn rebase_base(repo: &Path) -> &'static str {
    if git_ref_exists(repo, "refs/remotes/origin/main").await {
        "origin/main"
    } else {
        "main"
    }
}

/// Whether "Rebase onto main" would do anything right now (see [`RebaseStatus`]),
/// resolved fresh per request like `/api/head-branch` — the graph on screen may
/// predate a rebase or a branch switch. Sent `no-store` like the other live reads.
pub(crate) async fn rebase_status() -> impl IntoResponse {
    let repo = current().0;
    let branch = git_vista_git::read_head_branch(&repo);
    let base = rebase_base(&repo).await;
    let base_exists = rev_parse(&repo, base).await.is_some();
    let up_to_date = base_exists && is_ancestor(&repo, base, "HEAD").await;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    (
        no_store,
        Json(RebaseStatus {
            branch,
            base: base.to_string(),
            base_exists,
            up_to_date,
        }),
    )
}
