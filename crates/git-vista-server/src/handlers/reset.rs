//! `POST /api/reset-test-repo` (iPad-testing follow-up): restore a seeded *test
//! repo* to its recorded state. Since M1.06b (#143) the composite restore runs
//! inside the planner's executor as [`GitOperation::ResetTestRepo`]; this file
//! keeps [`has_seed`] — the gate the graph read uses to offer the action at
//! all — beside the endpoint it guards.

use std::path::Path;

use axum::http::StatusCode;

use git_vista_protocol::GitOperation;

use crate::journal;
use crate::planner;

/// Whether this repo carries a recorded test-repo seed (`gv --seed`) — the gate
/// for offering "Reset Test Repo" at all.
pub(crate) fn has_seed(repo: &Path) -> bool {
    journal::state_dir(repo)
        .is_some_and(|d| d.join("seed-refs").exists() && d.join("seed-head").exists())
}

/// Reset a *test repo* to its recorded seed (iPad-testing follow-up): move
/// every seeded branch back to its recorded tip, check out the seeded HEAD
/// branch, force the worktree clean, DELETE branches the seed doesn't know —
/// allowed nowhere else in git-vista — and wipe the app journal. Hard-gated:
/// only a repo explicitly opted in with `gv --seed <path>` has seed files, and
/// a read-only clone is refused outright (the planner's write gate).
pub(crate) async fn reset_test_repo() -> (StatusCode, String) {
    planner::plan_and_execute(GitOperation::ResetTestRepo).await
}
