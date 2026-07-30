//! `POST /api/commit` (Issue #33): create a commit — either a plain commit on
//! HEAD, or (the branch-stub path) an empty commit written directly onto a
//! branch that isn't checked out — plus `POST /api/stage` / `POST /api/unstage`.
//!
//! Since M1.06b (#143) these handlers validate the request (unchanged
//! wording), build the matching [`GitOperation`], and hand it to
//! [`planner::plan_and_execute`]; the git execution and journaling live in the
//! planner's executor.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{BranchName, CommitMessage, CommitOid, CreateCommitRequest, GitOperation};

use crate::git_cmd::rev_parse;
use crate::planner;
use crate::state::reject_if_read_only;

/// Create a commit in the served repository (Issue #33).
///
/// With no `branch` in the request — or one that turns out to be the
/// checked-out branch — this is [`GitOperation::CommitOnHead`]: a plain
/// `git commit`, which lands exactly where the UI offered it (the HEAD tip).
/// A *different* branch (the UI offers this on branch stubs, for empty commits
/// only) becomes [`GitOperation::EmptyCommitOnBranch`] instead: `git commit`
/// can only ever commit on HEAD, so that operation writes the commit object
/// directly and moves just the named ref, compare-and-swapped on the tip
/// resolved here. An empty message is rejected, as always.
pub(crate) async fn create_commit(Json(req): Json<CreateCommitRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let message = req.message.trim();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Commit message can't be empty.".to_string(),
        );
    }
    let message = match CommitMessage::new(message) {
        Ok(message) => message,
        // Unreachable after the emptiness check above; kept total.
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };

    // D2 (#66, Task 7): the validated resolution, replacing a raw
    // `state::current()` call — see `state::resolve_target`'s doc comment.
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };

    // A named target that isn't the checked-out branch takes the ref-write
    // path. The checked-out branch itself falls through to the plain
    // `git commit` below — same result, plus HEAD's own reflog entry.
    if let Some(branch) = req.branch.as_deref().map(str::trim) {
        if git_vista_git::read_head_branch(&repo).as_deref() != Some(branch) {
            return commit_empty_on_branch(branch, message, req.allow_empty).await;
        }
    }

    planner::plan_and_execute(GitOperation::CommitOnHead {
        message,
        allow_empty: req.allow_empty,
    })
    .await
}

/// Stage all working-tree changes (`POST /api/stage`):
/// [`GitOperation::StageAll`], a plain `git add -A`, so the user can stage
/// from the UI and then commit. `-A` stages modified, new and deleted paths
/// (honouring `.gitignore`) — what a "Stage Changes" button is expected to do.
pub(crate) async fn stage_all() -> (StatusCode, String) {
    planner::plan_and_execute(GitOperation::StageAll).await
}

/// Unstage everything (`POST /api/unstage`): [`GitOperation::UnstageAll`], a
/// plain `git reset -q HEAD` — the exact inverse of [`stage_all`]; the working
/// tree keeps every edit, so nothing is lost. The UI offers it only while
/// something is staged, but running it with a clean index is a harmless no-op.
pub(crate) async fn unstage_all() -> (StatusCode, String) {
    planner::plan_and_execute(GitOperation::UnstageAll).await
}

/// The branch-stub path of `/api/commit`: validate the named branch (same
/// wording as always), resolve its tip — which also confirms a local branch by
/// that name exists (the graph the menu came from may be stale) — and build
/// the compare-and-swap [`GitOperation::EmptyCommitOnBranch`].
///
/// Only empty commits are meaningful here: staged changes live in the
/// checked-out branch's index, so a staged commit aimed at another branch is
/// rejected — the UI keeps that item disabled on stubs, this is belt and
/// braces.
async fn commit_empty_on_branch(
    branch: &str,
    message: CommitMessage,
    allow_empty: bool,
) -> (StatusCode, String) {
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
    if !allow_empty {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Staged changes can only be committed on the checked-out branch, \
                 not ‘{branch}’. Check it out first, or create an empty commit."
            ),
        );
    }
    // Resolve the branch's tip — also confirms a local branch by that name
    // exists. This is the tip the operation's compare-and-swap is pinned to.
    // D2 (#66, Task 7): the validated resolution, replacing a raw
    // `state::current()` call — see `state::resolve_target`'s doc comment.
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };
    let refname = format!("refs/heads/{branch}");
    let Some(tip) = rev_parse(&repo, &refname).await else {
        return (
            StatusCode::BAD_REQUEST,
            format!("No local branch named ‘{branch}’ — refresh and try again."),
        );
    };
    let (Ok(branch), Ok(expected_tip)) = (BranchName::new(branch), CommitOid::new(tip)) else {
        // Unreachable: the name passed the checks above and rev-parse returns
        // a full lowercase-hex id; kept total rather than panic.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Couldn't build the commit operation.".to_string(),
        );
    };
    planner::plan_and_execute(GitOperation::EmptyCommitOnBranch {
        branch,
        message,
        expected_tip,
    })
    .await
}
