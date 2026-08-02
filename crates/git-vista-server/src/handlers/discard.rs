//! Discard/delete endpoints for uncommitted working-tree changes (#219,
//! M2.18a): `POST /api/discard-tracked-paths` (`git checkout -- <paths>`) and
//! `POST /api/delete-untracked-paths` (`git clean -f -- <paths>`) — two
//! separate, typed operations (#71), never one endpoint parameterised by a
//! bool.
//!
//! `DeleteUntrackedPaths` is the first operation in this codebase with **no
//! journal-backed undo at all**: an untracked path was never written to
//! git's object database, so once it is gone there is nothing anywhere in
//! this repository to recover it from. Every guard between this handler and
//! the executor — the [`WorktreePath`] newtype's wire-boundary validation
//! here, the race re-verification, the symlink-containment check (both in
//! `crate::planner`, beside the two `exec_*` functions this endpoint
//! reaches) — exists because of that fact, not despite it.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{GitOperation, WorktreePath, WorktreePathsRequest};

use crate::planner;
use crate::state::reject_if_read_only;

/// Validate a [`WorktreePathsRequest`] into `Vec<WorktreePath>`: at least one
/// path, and every one passes the newtype's own wire-boundary gate (non-empty,
/// not option-shaped, not absolute, no `..` component — see
/// [`WorktreePath`]'s doc comment for the full rule and why it is necessary
/// but not sufficient on its own).
fn validate_paths(req: WorktreePathsRequest) -> Result<Vec<WorktreePath>, (StatusCode, String)> {
    if req.paths.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Name at least one path.".to_string(),
        ));
    }
    req.paths
        .into_iter()
        .map(|p| WorktreePath::new(p).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string())))
        .collect()
}

/// `git checkout -- <paths>` (#219): discard uncommitted changes to
/// already-tracked paths, restoring each to its checked-out (index, else
/// HEAD) version via [`GitOperation::DiscardTrackedPaths`]. Destructive, and
/// only *sometimes* undoable outside git-vista — see that variant's own doc
/// comment for the exact, qualified recovery story.
pub(crate) async fn discard_tracked_paths(
    Json(req): Json<WorktreePathsRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let paths = match validate_paths(req) {
        Ok(paths) => paths,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::DiscardTrackedPaths { paths }).await
}

/// `git clean -f -- <paths>` (#219): delete untracked paths from the working
/// tree outright via [`GitOperation::DeleteUntrackedPaths`]. **Irrecoverable**
/// — see that variant's own doc comment.
pub(crate) async fn delete_untracked_paths(
    Json(req): Json<WorktreePathsRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let paths = match validate_paths(req) {
        Ok(paths) => paths,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::DeleteUntrackedPaths { paths }).await
}
