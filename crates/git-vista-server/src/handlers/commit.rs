//! `POST /api/commit` (Issue #33): create a commit — either a plain commit on
//! HEAD, or (the branch-stub path) an empty commit written directly onto a branch
//! that isn't checked out.

use std::path::Path;

use axum::http::StatusCode;
use axum::Json;

use git_vista_core::activity::ActivityKind;
use git_vista_core::model::CreateCommitRequest;

use crate::git_cmd::rev_parse;
use crate::state::{current, reject_if_read_only};

use super::journal_app_event;

/// Create a commit in the served repository (Issue #33).
///
/// Same B3 posture as [`create_branch`]: shell out to `git commit` rather than
/// build the commit ourselves. git validates the tree state, refuses an empty
/// commit unless `--allow-empty` is passed, and reports a clear message on stderr
/// (e.g. "nothing to commit") which we forward verbatim to the UI on failure.
///
/// With no `branch` in the request — or one that turns out to be the checked-out
/// branch — this is a plain `git commit` on HEAD, which lands exactly where the
/// UI offered it (the HEAD tip). A *different* branch (the UI offers this on
/// branch stubs, for empty commits only) takes [`commit_empty_on_branch`]
/// instead: `git commit` can only ever commit on HEAD, so that path writes the
/// commit object directly and moves just the named ref. Args are separate argv
/// entries (never a shell line); the message is the value of `-m`, so even a
/// message starting with `-` can't be read as an option. An empty message is
/// rejected.
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

    let repo = current().0;

    // A named target that isn't the checked-out branch takes the ref-write
    // path. The checked-out branch itself falls through to the plain
    // `git commit` below — same result, plus HEAD's own reflog entry.
    if let Some(branch) = req.branch.as_deref().map(str::trim) {
        if git_vista_git::read_head_branch(&repo).as_deref() != Some(branch) {
            return commit_empty_on_branch(&repo, branch, message, req.allow_empty).await;
        }
    }

    // The pre-commit tip, captured for the journal before git moves anything.
    // `None` on an unborn HEAD (first commit) — journaled as a creation-like
    // event with no old state, which is exactly what it is.
    let old = rev_parse(&repo, "HEAD").await;

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(&repo).arg("commit");
    if req.allow_empty {
        cmd.arg("--allow-empty");
    }
    cmd.arg("-m").arg(message);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[/api/commit] created commit (allow_empty={})", req.allow_empty);
        let new = rev_parse(&repo, "HEAD").await;
        // The branch the commit landed on; "HEAD" when detached.
        let branch = git_vista_git::read_head_branch(&repo).unwrap_or_else(|| "HEAD".into());
        let summary = message.lines().next().unwrap_or(message).to_string();
        journal_app_event(&repo, ActivityKind::Commit, Some(branch), old, new, summary);
        (StatusCode::OK, "Created commit.".to_string())
    } else {
        // git explains the failure, but "nothing to commit, working tree clean"
        // goes to *stdout* (not stderr) with a non-zero exit — so prefer stderr,
        // fall back to stdout, and only then to a generic line.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git commit failed.".to_string()
            } else {
                stdout
            }
        };
        eprintln!("git-vista: /api/commit failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Stage all working-tree changes (`POST /api/stage`): a plain `git add -A`, so
/// the user can stage from the UI and then commit. Same read-only gate and
/// git-error-forwarding posture as [`create_commit`]; `-A` stages modified, new
/// and deleted paths (honouring `.gitignore`) — what a "Stage Changes" button is
/// expected to do.
pub(crate) async fn stage_all() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["add", "-A"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/stage couldn't run git: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"));
        }
    };
    if output.status.success() {
        println!("[/api/stage] staged all changes (git add -A)");
        (StatusCode::OK, "Staged changes.".to_string())
    } else {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git add failed.".to_string() } else { msg };
        eprintln!("git-vista: /api/stage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Unstage everything (`POST /api/unstage`): a plain `git reset -q HEAD`, the
/// exact inverse of [`stage_all`] — the index goes back to HEAD while the
/// working tree keeps every edit, so nothing is lost. Same read-only gate and
/// git-error-forwarding posture; the UI offers it only while something is
/// staged, but running it with a clean index is a harmless no-op anyway.
pub(crate) async fn unstage_all() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["reset", "-q", "HEAD"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/unstage couldn't run git: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"));
        }
    };
    if output.status.success() {
        println!("[/api/unstage] unstaged all changes (git reset -q HEAD)");
        (StatusCode::OK, "Unstaged changes.".to_string())
    } else {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git reset failed.".to_string() } else { msg };
        eprintln!("git-vista: /api/unstage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Create an empty commit on a branch that is *not* checked out — the branch-stub
/// path of [`create_commit`], how a new zero-commit branch takes its first commit
/// from the UI without a checkout.
///
/// `git commit` can only commit on HEAD, so this writes the commit object
/// directly: `git commit-tree <tip>^{tree} -p <tip>` reuses the parent's tree
/// (an empty commit by construction), then `git update-ref <ref> <new> <tip>`
/// advances the branch. Passing the expected old value makes the update a
/// compare-and-swap — if the branch moved since the menu was opened, git
/// refuses rather than clobbering it (the same stale-graph posture as undo).
/// HEAD, the index and the working tree are untouched throughout.
///
/// Only empty commits are meaningful here: staged changes live in the
/// checked-out branch's index, so a staged commit aimed at another branch is
/// rejected — the UI keeps that item disabled on stubs, this is belt and braces.
async fn commit_empty_on_branch(
    repo: &Path,
    branch: &str,
    message: &str,
    allow_empty: bool,
) -> (StatusCode, String) {
    if branch.is_empty() {
        return (StatusCode::BAD_REQUEST, "Branch name can't be empty.".to_string());
    }
    if branch.starts_with('-') {
        return (StatusCode::BAD_REQUEST, "Branch name can't start with '-'.".to_string());
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
    // exists (the graph the menu came from may be stale).
    let refname = format!("refs/heads/{branch}");
    let Some(tip) = rev_parse(repo, &refname).await else {
        return (
            StatusCode::BAD_REQUEST,
            format!("No local branch named ‘{branch}’ — refresh and try again."),
        );
    };

    // Write the commit object: the parent's own tree, so nothing changes.
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("commit-tree")
        .arg(format!("{tip}^{{tree}}"))
        .args(["-p", &tip, "-m"])
        .arg(message)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git commit-tree: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"));
        }
    };
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git commit-tree failed.".to_string() } else { msg };
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let new = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "git commit-tree returned no commit id.".to_string(),
        );
    }

    // Advance the ref — compare-and-swap on the tip resolved above, with a
    // reflog line in git's own "commit (empty): …" shape so the activity feed
    // reads it like any other commit.
    let summary = message.lines().next().unwrap_or(message).to_string();
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", "-m"])
        .arg(format!("commit (empty): {summary}"))
        .args([refname.as_str(), new.as_str(), tip.as_str()])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git update-ref: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"));
        }
    };
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("‘{branch}’ has moved since this was offered — refresh and try again.")
        } else {
            msg
        };
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::CONFLICT, msg);
    }

    println!("[/api/commit] created empty commit on '{branch}' ({new})");
    journal_app_event(
        repo,
        ActivityKind::Commit,
        Some(branch.to_string()),
        Some(tip),
        Some(new),
        summary,
    );
    (StatusCode::OK, "Created commit.".to_string())
}
