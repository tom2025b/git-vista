//! Close a linked sibling worktree (M11.05, #550): `git worktree remove
//! <path>` via [`GitOperation::RemoveWorktree`], reached through its own
//! route rather than the generic `/api/plan` seam — the same ADR 0100
//! reasoning `/api/select-worktree` and every other established write here
//! already takes: a capability with no door is indistinguishable from one
//! that does not exist, and the generic plan seam carries no idempotency
//! key, no operations-registry row, and no authz census entry.
//!
//! The request names a desk by its opaque census id and nothing else. See
//! [`GitOperation::RemoveWorktree`]'s own doc comment for the full
//! compare-and-swap this reaches into, and
//! `planner::worktree_exec::exec_remove_worktree` for where it actually
//! runs — this handler does nothing but validate the write gate and hand the
//! operation to the ordinary pipeline.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{GitOperation, RemoveWorktreeRequest};

use crate::planner;
use crate::state::reject_if_read_only;

pub(crate) async fn remove_worktree(
    Json(req): Json<RemoveWorktreeRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::RemoveWorktree { id: req.id }).await
}
