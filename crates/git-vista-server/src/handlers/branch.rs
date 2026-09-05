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
    AddWorktreeRequest, BranchName, BranchRequest, CreateBranchRequest, ForcePublish, GitOperation,
    PushRequest, RemoteName,
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

/// Open a second working tree on an existing branch (M11.04, #549, ADR 0118):
/// `git worktree add <worktrees_root>/<name> <branch>` via
/// [`GitOperation::AddWorktree`].
///
/// # Its own route rather than the generic plan seam
///
/// ADR 0100: a capability with no door is indistinguishable from one that does
/// not exist. Reached through `POST /api/plan` + `/api/execute-plan` instead,
/// this write would carry no idempotency key, pass neither `api.rs` guard,
/// never enter the operations registry, and be invisible to the authz census —
/// which is the shape that ADR found shipped once already.
///
/// # The request names a desk; the server chooses where it goes
///
/// [`AddWorktreeRequest::name`] is a
/// [`WorktreeName`](git_vista_protocol::WorktreeName), validated at the wire
/// boundary into a single path segment. A value carrying a separator, a `..`,
/// a leading dot or an absolute path is a **400 from serde**, before this
/// function runs and before any path is computed. That is the refusal the
/// fence rests on, and it is the server's — a picker declining to offer a bad
/// name is a courtesy on top, never the enforcement.
pub(crate) async fn add_worktree(Json(req): Json<AddWorktreeRequest>) -> (StatusCode, String) {
    if let Some(refused) = crate::state::reject_if_read_only() {
        return refused;
    }
    planner::plan_and_execute(GitOperation::AddWorktree {
        name: req.name,
        branch: req.branch,
    })
    .await
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
/// `force` and this endpoint pinned both to the values that reproduced the argv
/// it had always run. M2.20e (#231, ADR 0045) wired execution for all four
/// combinations, so the endpoint now *offers* them — otherwise the executor
/// would be code with no caller, which is the shape #228 shipped once and this
/// project has been paying for since.
///
/// # What is still not a default
///
/// [`PushRequest`]'s two new fields default to the endpoint's long-standing
/// behaviour, so a client that sends `{"branch": …}` — which is every client
/// written before this slice, including the live frontend — gets a plain
/// fast-forward push with no upstream write, byte for byte. That defaulting
/// lives **here, in the request**, and stops here: `ForcePublish` still has no
/// `Default` impl and [`GitOperation::PushBranch`]'s fields still carry no
/// `#[serde(default)]`, so the line below is a construction site stating its
/// posture out loud, exactly as #227 intended. The default points at *less*
/// capability, never more, which is the only direction a default is allowed to
/// point for a flag like this.
///
/// The confirmation ceremony a force-publish deserves in the UI is M2.20g's
/// (#232) — the plan this builds already carries `RiskLevel::Destructive` for a
/// lease push (M2.20a), which is what the frontend will scale that ceremony
/// from.
///
/// # Why the body is two lines
///
/// The mapping lives in [`push_operation`], which **consumes the request whole**,
/// so this function has no field left to drop on the way past. That is not
/// tidiness: `planner::push` is proved to the hilt over every `ForcePublish`
/// value, and none of it means anything if the one production path into it
/// quietly hands the executor a `ForcePublish::None` the user never asked for.
/// A user who approved a force-with-lease would get a silent fast-forward push
/// and every test in the repository would stay green — which is exactly what a
/// mutation of the previous version of this function demonstrated. The mapping
/// is now a pure function with a literal-valued census test
/// ([`tests::the_request_reaches_the_operation_whole`]).
pub(crate) async fn push_branch(Json(req): Json<PushRequest>) -> (StatusCode, String) {
    let (branch, to_op) = push_operation(req);
    branch_op(branch, to_op).await
}

/// Turn a `/api/push` body into the branch-name gate's input and the reviewed
/// [`GitOperation::PushBranch`] it becomes — the **only** place in this server
/// where an HTTP request becomes a push operation.
///
/// Takes [`PushRequest`] **by value** and destructures it exhaustively, so
/// every field is consumed here or the compiler complains; there is no version
/// of the caller that can read one field and forget another. The return is a
/// closure rather than a finished operation because the branch name has to
/// clear [`branch_op`]'s validation gate first, and that gate is shared with
/// every other branch endpoint.
///
/// The `remote` is `origin`, fixed here and never taken from the request: a
/// client-named remote would be a request that can aim this server's credential
/// helpers and SSH agent at a host of its choosing (see `docs/SECURITY_MODEL.md`,
/// the push annotation).
fn push_operation(req: PushRequest) -> (BranchRequest, impl FnOnce(BranchName) -> GitOperation) {
    let PushRequest {
        branch,
        set_upstream,
        force,
    } = req;
    (BranchRequest { branch }, move |branch| {
        GitOperation::PushBranch {
            branch,
            remote: RemoteName::new("origin").expect("'origin' is a valid remote name"),
            set_upstream,
            // The one place `/api/push` turns "the body said nothing about
            // forcing" into a value. Written out rather than derived, so the
            // safe reading is visible at the site that chose it.
            force: force.unwrap_or(ForcePublish::None),
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::CommitOid;

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn lease(c: char) -> ForcePublish {
        ForcePublish::WithLease {
            expected_remote_tip: oid(c),
        }
    }

    /// Apply the real mapping to a body and get the operation it becomes,
    /// through the same branch-name gate the handler uses.
    fn operation_for(req: PushRequest) -> GitOperation {
        let (branch_request, to_op) = push_operation(req);
        let branch = BranchName::new(branch_request.branch.trim()).expect("a valid branch name");
        to_op(branch)
    }

    /// **The gap this test exists to close.** Everything `planner::push` proves
    /// about `ForcePublish` is proved about the *executor*; `/api/push` is the
    /// only production path into it, and until this test the mapping between the
    /// two had no coverage at all.
    ///
    /// Demonstrated rather than assumed: replacing
    /// `force.unwrap_or(ForcePublish::None)` with a bare `ForcePublish::None` —
    /// i.e. an endpoint that silently downgrades every approved force-publish to
    /// a fast-forward push — left all 700 tests in this crate green. This is the
    /// one that dies.
    ///
    /// Asserted against **literal** operations over the whole request space, not
    /// by re-deriving the expected value from the request: a test that wrote
    /// `set_upstream: req.set_upstream` on the right-hand side would agree with
    /// any mapping, including one that swapped the two flags.
    #[test]
    fn the_request_reaches_the_operation_whole() {
        let origin = RemoteName::new("origin").unwrap();

        for (req, expected) in [
            (
                PushRequest {
                    branch: "main".to_string(),
                    set_upstream: false,
                    force: None,
                },
                GitOperation::PushBranch {
                    branch: BranchName::new("main").unwrap(),
                    remote: origin.clone(),
                    set_upstream: false,
                    force: ForcePublish::None,
                },
            ),
            (
                PushRequest {
                    branch: "main".to_string(),
                    set_upstream: true,
                    force: None,
                },
                GitOperation::PushBranch {
                    branch: BranchName::new("main").unwrap(),
                    remote: origin.clone(),
                    set_upstream: true,
                    force: ForcePublish::None,
                },
            ),
            (
                PushRequest {
                    branch: "release/2026-08".to_string(),
                    set_upstream: false,
                    force: Some(lease('4')),
                },
                GitOperation::PushBranch {
                    branch: BranchName::new("release/2026-08").unwrap(),
                    remote: origin.clone(),
                    set_upstream: false,
                    force: lease('4'),
                },
            ),
            (
                PushRequest {
                    branch: "feature/x".to_string(),
                    set_upstream: true,
                    force: Some(lease('a')),
                },
                GitOperation::PushBranch {
                    branch: BranchName::new("feature/x").unwrap(),
                    remote: origin.clone(),
                    set_upstream: true,
                    force: lease('a'),
                },
            ),
            (
                // An explicit `"force": {"mode": "none"}` is the same operation
                // as an omitted one — so the default below is provably a
                // *default* and not the only value this endpoint can produce.
                PushRequest {
                    branch: "main".to_string(),
                    set_upstream: false,
                    force: Some(ForcePublish::None),
                },
                GitOperation::PushBranch {
                    branch: BranchName::new("main").unwrap(),
                    remote: origin.clone(),
                    set_upstream: false,
                    force: ForcePublish::None,
                },
            ),
        ] {
            let got = operation_for(req.clone());
            assert_eq!(got, expected, "for {req:?}");
        }
    }

    /// The census that keeps the table above whole: every `ForcePublish`
    /// variant is exercised by it, counted through an exhaustive `match` with
    /// no wildcard, so a third variant fails to compile here rather than
    /// silently going unmapped.
    #[test]
    fn every_force_mode_the_wire_can_carry_is_exercised_by_the_mapping_table() {
        fn name(force: &ForcePublish) -> &'static str {
            match force {
                ForcePublish::None => "none",
                ForcePublish::WithLease { .. } => "with_lease",
            }
        }
        let exercised = [ForcePublish::None, lease('4')];
        let mut names: Vec<&str> = exercised.iter().map(name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            2,
            "ForcePublish grew a variant — add it to the mapping table above, \
             or /api/push's handling of it is untested"
        );
        // …and each of them really does survive the mapping, so the census
        // counts values this endpoint can actually produce.
        for force in exercised {
            let got = operation_for(PushRequest {
                branch: "main".to_string(),
                set_upstream: false,
                force: Some(force.clone()),
            });
            match got {
                GitOperation::PushBranch { force: got, .. } => assert_eq!(got, force),
                other => panic!("/api/push must build a PushBranch, got {other:?}"),
            }
        }
    }

    /// The remote is this endpoint's, never the client's — the request type has
    /// no `remote` field (and `deny_unknown_fields` rejects one), and the
    /// mapping hardcodes `origin`.
    #[test]
    fn the_remote_is_always_origin_and_never_comes_from_the_request() {
        assert!(
            serde_json::from_str::<PushRequest>(r#"{"branch":"main","remote":"evil"}"#).is_err(),
            "a push body may not carry a remote at all"
        );
        match operation_for(PushRequest {
            branch: "main".to_string(),
            set_upstream: true,
            force: Some(lease('4')),
        }) {
            GitOperation::PushBranch { remote, .. } => {
                assert_eq!(remote, RemoteName::new("origin").unwrap())
            }
            other => panic!("/api/push must build a PushBranch, got {other:?}"),
        }
    }
}
