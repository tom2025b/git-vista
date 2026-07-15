//! The activity/undo endpoints: `GET /api/activity` (the assembled feed),
//! `GET /api/undoables/{id}` (undo actions for one tapped commit, computed
//! live), and `POST /api/undo` (execute one [`UndoAction`]).
//!
//! The feed handler is deliberately thin: it *collects* (current branches, the
//! journal, every reflog, the remote-commit set), lets the pure, unit-tested
//! [`assemble_feed`] do the folding, and maintains the ref snapshot on the
//! way through. The interesting logic lives in `git_vista_core::activity`
//! and `crate::journal`.
//!
//! Snapshot upkeep happens in `activity_feed` — and only there — because
//! detection and bookkeeping must be one atomic step: whoever rewrites the
//! snapshot must first synthesize deletion events for branches that vanished
//! since the last one, or those deletions are silently forgotten. Keeping a
//! single writer makes that invariant easy to hold (which is why `undoables`
//! reads the same sources but leaves the snapshot alone).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use git_vista_core::activity::{
    assemble_feed, ActivityEvent, ActivityKind, ActivitySource, UndoAction, Undoable,
};
use git_vista_core::model::RefKind;
use git_vista_git::{read_commit, read_reflogs, read_refs, read_remote_commits, RepoError};

use crate::journal;

/// How many events the feed returns by default, and at most. The panel shows
/// a scrollable list, not an archive; anyone needing more can raise `limit`
/// up to the cap.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

/// Reflog entries read per ref. A rebase writes one line per replayed commit,
/// so this is deliberately far above the feed limit.
const REFLOG_PER_REF: usize = 200;

#[derive(Deserialize)]
pub struct ActivityParams {
    pub limit: Option<usize>,
}

/// Unix seconds now; the timestamp journaled onto synthesized events.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The feed: journal + reflogs + snapshot diff, folded newest-first.
pub async fn activity_feed(
    Query(params): Query<ActivityParams>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (repo, read_only) = crate::state::current();
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    // The current local branch → tip map: the baseline for undo hints and for
    // the next snapshot.
    let refs = read_refs(&repo).map_err(|e| {
        eprintln!("git-vista: /api/activity failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let branches: HashMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();

    // Snapshot diff: a branch known to the last snapshot but absent now was
    // deleted outside the app (app deletions remove their branch from the
    // snapshot the moment they happen — see the delete handlers). Journal the
    // synthesized event *before* rewriting the snapshot, so it's remembered
    // exactly once, with the last tip we saw — which is what makes even a
    // terminal deletion restorable.
    if let Some(snapshot) = journal::read_snapshot(&repo) {
        for (name, tip) in &snapshot {
            if !branches.contains_key(name) {
                journal::append(
                    &repo,
                    &ActivityEvent {
                        time: now_secs(),
                        kind: ActivityKind::BranchDeleted,
                        ref_name: Some(name.clone()),
                        summary: format!("deleted branch ‘{name}’ (outside git-vista)"),
                        old_oid: Some(tip.clone()),
                        new_oid: None,
                        source: ActivitySource::External,
                        undo: None,
                    },
                );
                println!(
                    "[/api/activity] noticed external deletion of branch '{name}' (was {tip})"
                );
            }
        }
    }
    journal::write_snapshot(&repo, &branches);

    let journal_events = journal::read_all(&repo);
    let reflog = read_reflogs(&repo, REFLOG_PER_REF).map_err(|e| {
        eprintln!("git-vista: /api/activity failed reading reflogs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // Which commits are on the remote — feeds the "already pushed" warning on
    // reset-style undo hints. Best-effort: no remote (or a failed walk) just
    // means no warnings.
    let remote: HashSet<String> =
        read_remote_commits(&repo, crate::state::HISTORY_LIMIT).unwrap_or_default();

    let mut feed = assemble_feed(journal_events, reflog, &branches, &remote, limit);
    // A read-only clone can't undo anything (`/api/undo` would 403), so its
    // feed carries no hints — the UI then simply never shows an Undo control.
    if read_only {
        for event in &mut feed {
            event.undo = None;
        }
    }
    let app_count = feed
        .iter()
        .filter(|e| e.source == ActivitySource::App)
        .count();
    println!(
        "[/api/activity] {} — {} event(s) ({app_count} via app), {} undoable",
        repo.display(),
        feed.len(),
        feed.iter().filter(|e| e.undo.is_some()).count(),
    );

    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(feed)))
}

/// The conventional 7-char short id, for labels and log lines.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Belt-and-braces before an id goes anywhere near argv: real ids are hex.
/// Same check `/api/diff/{id}` does.
fn is_hex_id(id: &str) -> bool {
    id.len() >= 4 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A branch name safe to hand to git as its own argv entry: non-empty and not
/// option-shaped. git itself validates the rest (bad characters, collisions).
fn is_safe_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-')
}

/// Run `git -C <repo> <args…>`, mapping failure to git's own explanation
/// (stderr, falling back to stdout, then a generic line — the same posture as
/// every other write handler).
async fn git(repo: &Path, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return Err(stderr);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(if stdout.is_empty() {
        "git failed.".to_string()
    } else {
        stdout
    })
}

/// `GET /api/undoables/{id}` — every undo action that applies to one commit,
/// computed live (step 5). The graph menu fetches this when it opens, so the
/// undo section always reflects the repo *now*, not the possibly-stale graph.
///
/// Two sources:
///  * the assembled feed's own hints (same fold as `/api/activity`, minus the
///    snapshot upkeep — that single-writer invariant belongs to the feed
///    handler): a reset-style undo whose result is this commit, or a deleted
///    branch whose lost tip was this commit;
///  * a revert offer for any non-merge commit — the history-preserving undo,
///    valid even for commits buried deep or already pushed. (Reverting a
///    merge needs a `-m` parent choice, so merges don't get the offer.)
///
/// A read-only clone gets an empty list: `/api/undo` would refuse anyway, so
/// the UI never shows a section it can't act on.
pub async fn undoables(
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    let (repo, read_only) = crate::state::current();
    if !is_hex_id(&id) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    if read_only {
        return Ok((no_store, Json(Vec::<Undoable>::new())));
    }
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/undoables/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let full = detail.id.0.clone();

    let refs = read_refs(&repo).map_err(|e| {
        eprintln!("git-vista: /api/undoables failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let branches: HashMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();
    let journal_events = journal::read_all(&repo);
    let reflog = read_reflogs(&repo, REFLOG_PER_REF).map_err(|e| {
        eprintln!("git-vista: /api/undoables failed reading reflogs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let remote: HashSet<String> =
        read_remote_commits(&repo, crate::state::HISTORY_LIMIT).unwrap_or_default();

    let feed = assemble_feed(journal_events, reflog, &branches, &remote, MAX_LIMIT);
    let mut out: Vec<Undoable> = Vec::new();
    for event in feed {
        let Some(undoable) = event.undo else { continue };
        // The commit each hint is "about": the state a reset would discard,
        // or the tip a restore would bring back — i.e. the dot being tapped.
        let about = match &undoable.action {
            UndoAction::ResetBranch { expected_tip, .. } => expected_tip.as_str(),
            UndoAction::RestoreBranch { tip, .. } => tip.as_str(),
            UndoAction::RevertCommit { commit } => commit.as_str(),
        };
        if about == full && !out.contains(&undoable) {
            out.push(undoable);
        }
    }
    if detail.parents.len() <= 1 {
        out.push(Undoable {
            action: UndoAction::RevertCommit {
                commit: full.clone(),
            },
            label: format!("Revert {} (adds an inverse commit)", short(&full)),
            warn_pushed: false,
        });
    }
    println!(
        "[/api/undoables] {} → {} action(s)",
        short(&full),
        out.len()
    );
    Ok((no_store, Json(out)))
}

/// `POST /api/undo` — execute one [`UndoAction`] (step 5). The body is the
/// tagged action exactly as `/api/activity` / `/api/undoables` handed it out.
///
/// Safety posture, in order:
///  * read-only clones are refused outright (403);
///  * ids and branch names are validated before they go near argv;
///  * `ResetBranch` is compare-and-swap — the branch must still point at
///    `expected_tip`, so a hint from a stale menu can never reset away work
///    that happened after it was shown (409 when it has moved);
///  * resetting the *checked-out* branch requires a clean working tree
///    (`git status --porcelain` empty) — `git reset --hard` must never eat
///    uncommitted work, and a fully-clean tree is the one state where it
///    can't (even an untracked file could be overwritten if the target
///    commit tracks that path). A branch that isn't checked out moves with
///    `git branch -f`, which touches no worktree at all;
///  * a conflicted `git revert` is auto-aborted (like `/api/rebase`), so a
///    browser-only user is never left mid-revert with no shell to fix it.
///
/// Every successful undo is itself journaled — it's a repo event like any
/// other, shows up in the feed attributed to the app, and (for a reset) gets
/// its own undo hint, which is what makes "undo the undo" fall out for free.
pub async fn undo(Json(action): Json<UndoAction>) -> (StatusCode, String) {
    if let Some(rejected) = crate::state::reject_if_read_only() {
        return rejected;
    }
    let repo = crate::state::current().0;
    match action {
        UndoAction::RestoreBranch { name, tip } => {
            let name = name.trim();
            if !is_safe_branch_name(name) {
                return (StatusCode::BAD_REQUEST, "Bad branch name.".to_string());
            }
            if !is_hex_id(&tip) {
                return (StatusCode::BAD_REQUEST, "Not a commit id.".to_string());
            }
            // The safe undo: `git branch` creates, never destroys, and fails
            // by itself if the name came back into use since the hint.
            match git(&repo, &["branch", name, &tip]).await {
                Ok(()) => {
                    println!("[/api/undo] restored branch '{name}' at {}", short(&tip));
                    crate::handlers::journal_app_event(
                        &repo,
                        ActivityKind::BranchCreated,
                        Some(name.to_string()),
                        None,
                        Some(tip.clone()),
                        format!("restored branch ‘{name}’ at {}", short(&tip)),
                    );
                    (StatusCode::OK, format!("Restored branch ‘{name}’."))
                }
                Err(msg) => {
                    eprintln!("git-vista: /api/undo restore failed: {msg}");
                    (StatusCode::BAD_REQUEST, msg)
                }
            }
        }
        UndoAction::ResetBranch {
            branch,
            to,
            expected_tip,
        } => {
            let branch = branch.trim();
            if !is_safe_branch_name(branch) {
                return (StatusCode::BAD_REQUEST, "Bad branch name.".to_string());
            }
            if !is_hex_id(&to) || !is_hex_id(&expected_tip) {
                return (StatusCode::BAD_REQUEST, "Not a commit id.".to_string());
            }
            // Compare-and-swap: the hint was computed against `expected_tip`;
            // if the branch has moved since, this undo would discard newer
            // work the user never saw in the dialog — refuse instead.
            let actual = crate::git_cmd::rev_parse(&repo, branch).await;
            if actual.as_deref() != Some(expected_tip.as_str()) {
                return (
                    StatusCode::CONFLICT,
                    format!(
                        "‘{branch}’ has moved since this undo was offered — refresh and try again."
                    ),
                );
            }
            let checked_out = git_vista_git::read_head_branch(&repo).as_deref() == Some(branch);
            let result = if checked_out {
                // `git reset --hard` rewrites the working tree, so it runs
                // only against a fully clean one — never eat uncommitted work.
                match worktree_dirty(&repo).await {
                    Err(msg) => Err(msg),
                    Ok(true) => {
                        return (
                            StatusCode::CONFLICT,
                            "The working tree has uncommitted changes — commit them first \
                             so the undo can't destroy them."
                                .to_string(),
                        );
                    }
                    Ok(false) => git(&repo, &["reset", "--hard", &to]).await,
                }
            } else {
                // Not checked out: move the ref alone, no worktree involved.
                git(&repo, &["branch", "-f", branch, &to]).await
            };
            match result {
                Ok(()) => {
                    println!("[/api/undo] reset branch '{branch}' to {}", short(&to));
                    crate::handlers::journal_app_event(
                        &repo,
                        ActivityKind::Reset,
                        Some(branch.to_string()),
                        Some(expected_tip.clone()),
                        Some(to.clone()),
                        format!("undid — reset ‘{branch}’ to {}", short(&to)),
                    );
                    (
                        StatusCode::OK,
                        format!("Reset ‘{branch}’ to {}.", short(&to)),
                    )
                }
                Err(msg) => {
                    eprintln!("git-vista: /api/undo reset failed: {msg}");
                    (StatusCode::BAD_REQUEST, msg)
                }
            }
        }
        UndoAction::RevertCommit { commit } => {
            if !is_hex_id(&commit) {
                return (StatusCode::BAD_REQUEST, "Not a commit id.".to_string());
            }
            let old = crate::git_cmd::rev_parse(&repo, "HEAD").await;
            match git(&repo, &["revert", "--no-edit", &commit]).await {
                Ok(()) => {
                    println!("[/api/undo] reverted {}", short(&commit));
                    let new = crate::git_cmd::rev_parse(&repo, "HEAD").await;
                    let branch =
                        git_vista_git::read_head_branch(&repo).unwrap_or_else(|| "HEAD".into());
                    crate::handlers::journal_app_event(
                        &repo,
                        ActivityKind::Revert,
                        Some(branch),
                        old,
                        new,
                        format!("reverted {}", short(&commit)),
                    );
                    (StatusCode::OK, format!("Reverted {}.", short(&commit)))
                }
                Err(msg) => {
                    // Back out of a conflicted half-applied revert so the tree
                    // isn't stuck (same posture as /api/rebase). Harmless when
                    // no revert is in progress.
                    let _ = git(&repo, &["revert", "--abort"]).await;
                    eprintln!("git-vista: /api/undo revert failed (aborted): {msg}");
                    (StatusCode::BAD_REQUEST, msg)
                }
            }
        }
    }
}

/// Whether the working tree has any change at all (`git status --porcelain`
/// non-empty): staged, unstaged, untracked or conflicted — any of them makes
/// a hard reset unsafe.
async fn worktree_dirty(repo: &Path) -> Result<bool, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            "git status failed.".to_string()
        } else {
            msg
        });
    }
    Ok(!output.stdout.is_empty())
}
