//! Branch endpoints: creating a branch (Issue #18) and the branch operations
//! (Issue #33 follow-up) — checkout / merge / push / delete / force-delete — which
//! all share the [`run_branch_op`] runner. Each op captures the journal state it
//! needs and records the operation on success.

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::activity::ActivityKind;
use git_vista_protocol::{BranchRequest, CreateBranchRequest};

use crate::git_cmd::rev_parse;
use crate::journal;
use crate::state::{current, reject_if_read_only};

use super::journal_app_event;

/// Create a branch in the served repository at a given commit (Issue #18).
///
/// B3 from the design discussion: shell out to `git branch <name> <commit>` rather
/// than write the ref ourselves. git does the heavy lifting — it validates the ref
/// name, refuses a name that already exists, resolves the start-point, and reports
/// a clear message on stderr — which we forward verbatim to the UI on failure.
///
/// Args are passed as separate argv entries (never a shell line), so a crafted
/// name/commit can't inject a command. We additionally reject an empty name and
/// one starting with `-` so it can't be read as a git option.
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

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(current().0)
        .arg("branch")
        .arg(name)
        .arg(commit)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/branch couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[/api/branch] created branch '{name}' at {commit}");
        // Journal the creation with the resolved tip (the user may have given
        // an abbreviated or symbolic start point).
        let repo = current().0;
        let tip = rev_parse(&repo, name).await;
        journal_app_event(
            &repo,
            ActivityKind::BranchCreated,
            Some(name.to_string()),
            None,
            tip,
            format!("created branch ‘{name}’"),
        );
        (StatusCode::OK, format!("Created branch '{name}'."))
    } else {
        // git already explains the failure (name exists, bad name, unknown commit,
        // …) on stderr; surface that so the UI can show the real reason.
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git branch failed.".to_string()
        } else {
            msg
        };
        eprintln!("git-vista: /api/branch failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Check out a branch (iPad-testing follow-up): `git checkout <branch>`, moving
/// HEAD and the working tree to it. Git itself refuses when local changes would
/// be overwritten; that error is forwarded verbatim. Asking for the branch
/// already checked out is a no-op — git exits 0 (and logs a "moving from X to X"
/// reflog line the feed drops as noise), so journalling it would put an event in
/// the Activity feed that changed nothing (the same phantom-event trap the
/// merge/rebase no-ops guard against). A real switch *is* journaled: git also
/// logs it on the HEAD reflog, but the feed's dedup collapses that copy into
/// this one, which knows it came from the app.
pub(crate) async fn checkout_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    let repo = current().0;
    // Pre-checkout branch and tip, read before the switch: the branch is the
    // no-op test above, the tip is the journaled "moved from" oid.
    let before = git_vista_git::read_head_branch(&repo);
    let old = rev_parse(&repo, "HEAD").await;
    let resp = run_branch_op(
        "/api/checkout",
        &req.branch,
        &["checkout"],
        format!("checked out '{}'", req.branch.trim()),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let branch = req.branch.trim();
        if before.as_deref() == Some(branch) {
            return (
                StatusCode::OK,
                format!("Already on ‘{branch}’ — it's the checked-out branch."),
            );
        }
        let new = rev_parse(&repo, "HEAD").await;
        journal_app_event(
            &repo,
            ActivityKind::Checkout,
            Some(branch.to_string()),
            old,
            new,
            format!("checked out ‘{branch}’"),
        );
    }
    resp
}

/// Merge a branch into the currently checked-out branch (Issue #33 follow-up):
/// `git merge --no-edit <branch>`. `--no-edit` takes git's default merge message
/// (the server has no editor). A merge lands in whatever HEAD points at, so the UI
/// labels this with the current branch and never switches branches itself.
pub(crate) async fn merge_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    // Pre-merge tip, captured for the journal: it's the "undo merge" target.
    let repo = current().0;
    let old = rev_parse(&repo, "HEAD").await;
    let resp = run_branch_op(
        "/api/merge",
        &req.branch,
        &["merge", "--no-edit"],
        format!("merged '{}' into HEAD", req.branch.trim()),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let branch = req.branch.trim();
        let new = rev_parse(&repo, "HEAD").await;
        // git exits 0 with "Already up to date." when the branch brings nothing
        // in — HEAD hasn't moved. That's no merge: journalling one would put an
        // event in the Activity feed that never happened (with nothing to undo),
        // and a silent OK reads in the UI like a refresh failure. Say what
        // happened instead; the frontend surfaces this body verbatim.
        if new == old {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ has no commits the current branch doesn’t already have."),
            );
        }
        let into = git_vista_git::read_head_branch(&repo).unwrap_or_else(|| "HEAD".into());
        journal_app_event(
            &repo,
            ActivityKind::Merge,
            Some(into.clone()),
            old,
            new,
            format!("merged ‘{branch}’ into ‘{into}’"),
        );
    }
    resp
}

/// Push a branch to `origin` (Issue #33 follow-up): `git push origin <branch>`.
/// A non-origin remote (or none) makes git error; that text is forwarded to the UI.
pub(crate) async fn push_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    let resp = run_branch_op(
        "/api/push",
        &req.branch,
        &["push", "origin"],
        format!("pushed '{}' to origin", req.branch.trim()),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let repo = current().0;
        let branch = req.branch.trim();
        let tip = rev_parse(&repo, branch).await;
        journal_app_event(
            &repo,
            ActivityKind::Push,
            Some(branch.to_string()),
            None,
            tip,
            format!("pushed ‘{branch}’ to origin"),
        );
    }
    resp
}

/// Delete a branch (Issue #33 follow-up): `git branch -d <branch>`. The lowercase
/// `-d` is the *safe* delete — git refuses to drop a branch whose commits aren't
/// merged, forwarding "not fully merged" to the UI. The UI also confirms first, so
/// deletion takes both a click-through and a merged branch.
pub(crate) async fn delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    // The tip must be captured BEFORE the delete: git removes the branch's
    // reflog with the branch, so afterwards nobody knows where it pointed —
    // and this journaled oid is precisely what "Restore branch" replays.
    let repo = current().0;
    let tip = rev_parse(&repo, req.branch.trim()).await;
    let resp = run_branch_op(
        "/api/delete-branch",
        &req.branch,
        &["branch", "-d"],
        format!("deleted branch '{}'", req.branch.trim()),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let branch = req.branch.trim();
        journal_app_event(
            &repo,
            ActivityKind::BranchDeleted,
            Some(branch.to_string()),
            tip,
            None,
            format!("deleted branch ‘{branch}’"),
        );
        // Drop it from the snapshot now, so the feed's snapshot diff can't
        // also report this app deletion as an external one.
        journal::remove_from_snapshot(&repo, branch);
    }
    resp
}

/// Force-delete a branch (Issue #33 follow-up): `git branch -D <branch>`. The
/// uppercase `-D` drops a branch even when its commits aren't merged, discarding
/// any it alone holds. The UI only reaches here after the safe `git branch -d`
/// (see [`delete_branch`]) was refused for "not fully merged" and the user
/// confirmed the override, so this deliberately skips git's merge safety check.
pub(crate) async fn force_delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    // Same pre-delete tip capture as `delete_branch` — even more load-bearing
    // here, since a force-delete may discard commits nothing else reaches:
    // the journaled tip is then the ONLY path back to them (until gc).
    let repo = current().0;
    let tip = rev_parse(&repo, req.branch.trim()).await;
    let resp = run_branch_op(
        "/api/force-delete-branch",
        &req.branch,
        &["branch", "-D"],
        format!("force-deleted branch '{}'", req.branch.trim()),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let branch = req.branch.trim();
        journal_app_event(
            &repo,
            ActivityKind::BranchDeleted,
            Some(branch.to_string()),
            tip,
            None,
            format!("force-deleted branch ‘{branch}’"),
        );
        journal::remove_from_snapshot(&repo, branch);
    }
    resp
}

/// Shared runner for the branch-operation endpoints (merge/push/delete). Validates
/// `branch` (non-empty, not an option), then runs `git -C <repo> <args…> <branch>`
/// with the branch as its own final argv entry — so a crafted name is a git
/// argument, never a shell command. On failure it forwards git's own stderr
/// (falling back to stdout, then a generic line), matching `create_commit`'s posture.
async fn run_branch_op(
    endpoint: &str,
    branch: &str,
    args: &[&str],
    ok_msg: String,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let branch = branch.trim();
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

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(current().0)
        .args(args)
        .arg(branch)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: {endpoint} couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[{endpoint}] {ok_msg}");
        (StatusCode::OK, ok_msg)
    } else {
        // git explains most failures on stderr, but some (e.g. an up-to-date merge)
        // print to stdout with a non-zero exit — so prefer stderr, fall back to stdout.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git command failed.".to_string()
            } else {
                stdout
            }
        };
        eprintln!("git-vista: {endpoint} failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}
