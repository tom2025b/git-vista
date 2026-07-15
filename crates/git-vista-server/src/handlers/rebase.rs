//! Rebase endpoints (Issue #33 follow-up): rebase the checked-out branch onto
//! main (`POST /api/rebase`) and the live gate `GET /api/rebase-status` that tells
//! the menu whether a rebase would do anything right now. Both resolve the base
//! (`origin/main` if present, else `main`) through the shared [`rebase_base`].

use std::path::Path;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_core::activity::ActivityKind;
use git_vista_core::model::RebaseStatus;

use crate::git_cmd::{git_ref_exists, is_ancestor, rev_parse};
use crate::state::{current, reject_if_read_only};

use super::journal_app_event;

/// Rebase the checked-out branch onto main (Issue #33 follow-up): `git rebase
/// <base>`. `<base>` is `origin/main` when that remote-tracking ref exists — the
/// usual feature-branch target, so you rebase onto the freshest pushed main — and
/// the local `main` otherwise. It acts on HEAD, so it takes no request body.
///
/// A failed rebase (almost always conflicts) would leave the repo mid-rebase,
/// which a browser-only user with no shell can't resolve — so on failure it runs
/// `git rebase --abort` to restore the pre-rebase state, then forwards git's own
/// error text so the UI can explain why it couldn't complete.
pub(crate) async fn rebase() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    // Pre-rebase tip, for the journal: it's the "undo rebase" target.
    let old = rev_parse(&repo, "HEAD").await;
    let base = rebase_base(&repo).await;

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("rebase")
        .arg(base)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/rebase couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        let new = rev_parse(&repo, "HEAD").await;
        let branch = git_vista_git::read_head_branch(&repo).unwrap_or_else(|| "HEAD".into());
        // git exits 0 without moving HEAD when the branch is already based on
        // the base ("Current branch … is up to date"). That's no rebase:
        // journalling one puts a phantom "rebased ‘main’ onto main" event in
        // the Activity feed with nothing to undo, and "Rebased onto …" in the
        // UI claims something happened. Say what (didn't) happen instead.
        if new == old {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ is already based on {base}."),
            );
        }
        println!("[/api/rebase] rebased HEAD onto {base}");
        journal_app_event(
            &repo,
            ActivityKind::Rebase,
            Some(branch.clone()),
            old,
            new,
            format!("rebased ‘{branch}’ onto {base}"),
        );
        (StatusCode::OK, format!("Rebased onto {base}."))
    } else {
        // git explains conflicts on stderr (some notices go to stdout); prefer
        // stderr, fall back to stdout, then a generic line — matching the others.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git rebase failed.".to_string()
            } else {
                stdout
            }
        };
        // Best-effort: back out of the half-applied rebase so the working tree isn't
        // stuck mid-rebase. Harmless (exits non-zero, ignored) when none is running.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("rebase")
            .arg("--abort")
            .output()
            .await;
        eprintln!("git-vista: /api/rebase failed (aborted): {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// The base "Rebase onto main" rebases onto: `origin/main` when that
/// remote-tracking ref exists — the usual feature-branch target, so you rebase
/// onto the freshest pushed main — and the local `main` otherwise. Shared by
/// the rebase handler and `/api/rebase-status`, so the menu's gate always
/// describes exactly what the rebase would do.
async fn rebase_base(repo: &Path) -> &'static str {
    if git_ref_exists(repo, "refs/remotes/origin/main").await {
        "origin/main"
    } else {
        "main"
    }
}

/// Whether "Rebase onto main" would do anything right now (see [`RebaseStatus`]),
/// resolved fresh per request like `/api/head-branch` — the graph on screen may
/// predate a rebase or a branch switch. Sent `no-store` like the other live reads.
pub(crate) async fn rebase_status() -> impl IntoResponse {
    let repo = current().0;
    let branch = git_vista_git::read_head_branch(&repo);
    let base = rebase_base(&repo).await;
    let base_exists = rev_parse(&repo, base).await.is_some();
    let up_to_date = base_exists && is_ancestor(&repo, base, "HEAD").await;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    (
        no_store,
        Json(RebaseStatus {
            branch,
            base: base.to_string(),
            base_exists,
            up_to_date,
        }),
    )
}
