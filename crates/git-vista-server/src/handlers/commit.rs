//! `POST /api/commit` (Issue #33): create a commit — either a plain commit on
//! HEAD, or (the branch-stub path) an empty commit written directly onto a
//! branch that isn't checked out — plus `POST /api/amend-commit` (M2.19b,
//! #223) and `POST /api/stage` / `POST /api/unstage`.
//!
//! Since M1.06b (#143) these handlers validate the request (unchanged
//! wording), build the matching [`GitOperation`], and hand it to
//! [`planner::plan_and_execute`]; the git execution and journaling live in the
//! planner's executor.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use git_vista_protocol::{
    AmendCommitRequest, AmendFailureKind, BranchName, CommitMessage, CommitOid,
    CreateCommitRequest, GitOperation,
};

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

/// Rewrite the checked-out branch's tip commit (`POST /api/amend-commit`,
/// M2.19b #223, ADR 0040): validate the request the same way [`create_commit`]
/// does (read-only rejection, non-empty message), require `expected_tip` to be
/// a **full** hex commit id, and hand [`GitOperation::AmendCommit`] to the
/// planner. All execution — the compare-and-swap against `expected_tip`, the
/// published-history flag, the hook/signing failure classification — lives in
/// `planner::exec_amend_commit`.
///
/// A deliberately *separate* route from `POST /api/commit`, not a widened
/// `CreateCommitRequest`: an amend sent to a pre-#223 server must fail
/// loudly (404), never be quietly accepted as a plain commit. Folding the
/// amend fields into the commit body would make exactly that downgrade
/// possible — an older server ignoring (or, with `deny_unknown_fields`,
/// rejecting) `expected_tip` — and "created a second commit instead of
/// rewriting the first" is a silent wrong outcome on a history-rewriting
/// request. ADR 0040 records the choice.
///
/// The two validation refusals here answer with the same
/// [`git_vista_protocol::AmendCommitError`] JSON shape the executor's
/// classified failures use — this route's own checks go straight through
/// [`planner::amend_refusal`], which builds the `Response` itself (#323);
/// `exec_amend_commit`'s callers build the same JSON through the
/// `(StatusCode, String)`-shaped `planner::commit_exec::amend_refusal_body` instead,
/// re-labeled `application/json` at the final hop by
/// [`amend_route_response`] — so the endpoint's contract stays simple for
/// M2.19d: **every** 400 body from this route parses as `AmendCommitError`.
pub(crate) async fn amend_commit(Json(req): Json<AmendCommitRequest>) -> Response {
    if let Some(rejected) = reject_if_read_only() {
        // Prose, like every other write handler's read-only refusal —
        // `amend_route_response` (below) is only for `plan_and_execute`'s
        // output, where an already-JSON `AmendCommitError` body needs
        // re-labeling. This one stays `text/plain` so `middleware::rewrap_error`
        // envelopes it the same way it envelopes every other route's
        // read-only refusal.
        return rejected.into_response();
    }
    let message = req.message.trim();
    if message.is_empty() {
        // [`AmendFailureKind::Other`] for both handler-level refusals: these
        // are request-shape problems the UI never produces, not git
        // outcomes, so no finer kind applies.
        return planner::amend_refusal(AmendFailureKind::Other, "Commit message can't be empty.");
    }
    let message = match CommitMessage::new(message) {
        Ok(message) => message,
        // Unreachable after the emptiness check above; kept total.
        Err(e) => return planner::amend_refusal(AmendFailureKind::Other, &e.to_string()),
    };
    // The CAS pin. A full hex id only — never resolved through rev-parse:
    // `expected_tip` is the tip the client *reviewed*, and resolving a
    // symbolic name here would re-read the live repository and pin the swap
    // to whatever the tip is *now*, which asserts nothing (the same
    // reviewed-value-or-no-lease posture `PushBranch`'s lease takes).
    let expected_tip = match CommitOid::new(req.expected_tip.trim()) {
        Ok(tip) => tip,
        Err(e) => {
            return planner::amend_refusal(
                AmendFailureKind::Other,
                &format!(
                    "expected_tip must be the full commit id the amend was reviewed against: {e}"
                ),
            )
        }
    };
    let (status, body) = planner::plan_and_execute(GitOperation::AmendCommit {
        message,
        expected_tip,
        allow_empty: req.allow_empty,
    })
    .await;
    amend_route_response(status, body)
}

/// Re-labels [`planner::plan_and_execute`]'s `(StatusCode, String)` result as
/// `application/json` at the final hop of this route, but *only* when the
/// body actually is JSON — the executor side of the #323 fix.
///
/// `plan_and_execute` is not exclusively the two JSON-shaped outcomes this
/// route cares about (`AmendCommitSuccess` on success, `amend_refusal_body`'s
/// `AmendCommitError` on an executor-classified refusal): the shared pipeline
/// it runs before `exec_amend_commit` ever sees the request can also answer
/// with plain English, by design, from several places that have nothing to
/// do with amend specifically — [`crate::state::reject_if_read_only`]'s
/// "Visualize mode" 403 (re-checked here as defense in depth even though
/// `amend_commit` already checks it first), the idempotency gate's
/// missing-header 400, and — the one that matters most, because it is a real
/// race and not just a defensive check — the staleness gate's re-verification
/// of this operation's own `RefAt`/`BranchCheckedOut` preconditions if the
/// branch tip or checkout moves between this plan being built and executed
/// (`"'refs/heads/…' moved while this plan was pending — refresh and try
/// again."`, a `409` from `verify_precondition`, distinct from
/// `exec_amend_commit`'s own `StaleTip` JSON refusal for the same kind of
/// staleness).
///
/// Labeling *those* `application/json` would be a regression, not a fix:
/// `middleware::rewrap_error` would see the (forged) JSON content-type, skip
/// enveloping, and the client would receive a plain English sentence
/// claiming to be JSON — unparseable, and worse than the double-encoding this
/// issue set out to fix. So this sniffs the body instead of trusting the
/// route: only a body that actually parses as a JSON *object* gets
/// re-labeled; anything else is returned as `plan_and_execute` built it
/// (`String`'s default `text/plain`), which is exactly what
/// `rewrap_error` needs to keep enveloping it correctly.
fn amend_route_response(status: StatusCode, body: String) -> Response {
    let is_json_object = matches!(
        serde_json::from_str::<serde_json::Value>(&body),
        Ok(serde_json::Value::Object(_))
    );
    if is_json_object {
        (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    } else {
        (status, body).into_response()
    }
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
    let tip = match rev_parse(&repo, &refname).await {
        Ok(Some(tip)) => tip,
        // git ran and the branch is not there: the menu the request came from
        // is stale. 400, unchanged wording.
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("No local branch named ‘{branch}’ — refresh and try again."),
            )
        }
        // D5 (#66, Task 19): git never ran. "No local branch named X" would be
        // a claim about the repository, and this is the compare-and-swap pin
        // for the whole operation — the one value the executor trusts to be a
        // real observation of the branch tip. Refuse instead, with a status
        // that says the fault is ours.
        Err(e) => {
            return planner::couldnt_run(
                "/api/commit",
                &format!("couldn't resolve ‘{refname}’ for the compare-and-swap: {e}"),
            )
        }
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
