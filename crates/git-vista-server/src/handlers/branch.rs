//! Branch endpoints: creating a branch (Issue #18) and the branch operations
//! (Issue #33 follow-up) — checkout / merge / push / delete / force-delete.
//!
//! Since M1.06b (#143) these handlers no longer run git themselves: each one
//! validates its request (unchanged wording), builds the matching
//! [`GitOperation`] variant, and hands it to [`planner::plan_and_execute`] —
//! the one place a mutating git argv is constructed. The old per-endpoint
//! behaviors (B3 error forwarding, pre-delete tip capture for the journal,
//! checkout/merge no-op detection) live on inside the planner's executor.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{
    BranchName, BranchRequest, CreateBranchRequest, ForcePublish, GitOperation, RemoteName,
};

use crate::planner;
use crate::state::reject_if_read_only;

/// Create a branch in the served repository at a given commit (Issue #18):
/// `git branch <name> <commit>` via [`GitOperation::CreateBranch`]. git does
/// the heavy lifting — it validates the ref name, refuses a name that already
/// exists, and reports a clear message on stderr, forwarded verbatim to the UI
/// on failure. We additionally reject an empty name and one starting with `-`
/// so it can't be read as a git option (the same gates [`BranchName`] encodes).
pub(crate) async fn create_branch(Json(req): Json<CreateBranchRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let name = req.name.trim();
    let commit = req.commit.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Branch name can't be empty.".to_string(),
        );
    }
    if name.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Branch name can't start with '-'.".to_string(),
        );
    }
    let name = match BranchName::new(name) {
        Ok(name) => name,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    // D2 (#66, Task 7): the validated resolution, replacing a raw
    // `state::current()` call — see `state::resolve_target`'s doc comment.
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };
    // The operation pins an exact commit id. The UI always sends the tapped
    // node's full oid (taken as-is); a symbolic or abbreviated start point in
    // a hand-crafted request is resolved first.
    let at = match planner::resolve_commit_oid(&repo, commit).await {
        Ok(at) => at,
        Err(refused) => return refused,
    };
    planner::plan_and_execute(GitOperation::CreateBranch { name, at }).await
}

/// Check out a branch (iPad-testing follow-up): `git checkout <branch>` via
/// [`GitOperation::CheckoutBranch`], moving HEAD and the working tree. Git
/// itself refuses when local changes would be overwritten; that error is
/// forwarded verbatim. Asking for the branch already checked out is a no-op
/// the executor answers ("Already on …") without journalling a phantom event.
pub(crate) async fn checkout_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    branch_op(req, |branch| GitOperation::CheckoutBranch { branch }).await
}

/// Merge a branch into the currently checked-out branch (Issue #33 follow-up):
/// `git merge --no-edit <branch>` via [`GitOperation::MergeBranch`]. A merge
/// lands in whatever HEAD points at, so the UI labels this with the current
/// branch and never switches branches itself.
pub(crate) async fn merge_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    branch_op(req, |branch| GitOperation::MergeBranch { branch }).await
}

/// Push a branch to `origin` (Issue #33 follow-up): `git push origin <branch>`
/// via [`GitOperation::PushBranch`]. A non-origin remote (or none) makes git
/// error; that text is forwarded to the UI.
///
/// M2.20a (#227) widened [`GitOperation::PushBranch`] with `set_upstream` and
/// `force`. This endpoint pins both to the values that reproduce the argv it
/// has always run — no upstream write, no force — so its behaviour is
/// unchanged. It is `/api/push`'s *own* posture, not a default the type
/// supplies: `ForcePublish` has no `Default` impl and the fields have no
/// `#[serde(default)]`, so every construction site has to say this out loud.
/// Offering either capability here is M2.20g's (#231) to design, together
/// with the UI ceremony a force deserves.
pub(crate) async fn push_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    branch_op(req, |branch| GitOperation::PushBranch {
        branch,
        remote: RemoteName::new("origin").expect("'origin' is a valid remote name"),
        set_upstream: false,
        force: ForcePublish::None,
    })
    .await
}

/// Delete a branch (Issue #33 follow-up): `git branch -d <branch>` via
/// [`GitOperation::DeleteBranch`]. The lowercase `-d` is the *safe* delete —
/// git refuses to drop a branch whose commits aren't merged. The UI also
/// confirms first, so deletion takes both a click-through and a merged branch.
pub(crate) async fn delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    branch_op(req, |branch| GitOperation::DeleteBranch { branch }).await
}

/// Force-delete a branch (Issue #33 follow-up): `git branch -D <branch>` via
/// [`GitOperation::ForceDeleteBranch`], discarding any commits it alone holds.
/// The UI only reaches here after the safe delete was refused for "not fully
/// merged" and the user confirmed the override.
pub(crate) async fn force_delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    branch_op(req, |branch| GitOperation::ForceDeleteBranch { branch }).await
}

/// Shared front half of the branch-operation endpoints: the write gate, then
/// the branch-name validation every one of them applied (non-empty, not
/// option-shaped — same wording as always), then the typed operation into the
/// planner. The git execution, error forwarding and journaling that used to
/// follow here are the planner executor's now.
async fn branch_op(
    req: BranchRequest,
    to_op: impl FnOnce(BranchName) -> GitOperation,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let branch = req.branch.trim();
    if branch.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Branch name can't be empty.".to_string(),
        );
    }
    if branch.starts_with('-') {
        return (
            StatusCode::BAD_REQUEST,
            "Branch name can't start with '-'.".to_string(),
        );
    }
    let branch = match BranchName::new(branch) {
        Ok(branch) => branch,
        // Unreachable after the two checks above; kept total rather than panic.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    planner::plan_and_execute(to_op(branch)).await
}
