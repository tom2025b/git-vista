//! Rebase endpoints (Issue #33 follow-up): rebase the checked-out branch onto
//! main (`POST /api/rebase`) and the live gate `GET /api/rebase-status` that tells
//! the menu whether a rebase would do anything right now. Both resolve the base
//! (`origin/main` if present, else `main`) through the shared [`rebase_base`].

use std::path::Path;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_protocol::{GitOperation, RebaseStatus, RefName};

use crate::git_cmd::{git_ref_exists, is_ancestor, rev_parse, ExecUnavailable};
use crate::planner;
use crate::state::{current, reject_if_read_only};

/// Rebase the checked-out branch onto main (Issue #33 follow-up): `git rebase
/// <base>` via [`GitOperation::RebaseOntoBase`]. `<base>` is `origin/main` when
/// that remote-tracking ref exists — the usual feature-branch target, so you
/// rebase onto the freshest pushed main — and the local `main` otherwise. It
/// acts on HEAD, so it takes no request body.
///
/// A failed rebase (almost always conflicts) would leave the repo mid-rebase,
/// which a browser-only user with no shell can't resolve — so the executor
/// runs `git rebase --abort` on failure to restore the pre-rebase state, then
/// forwards git's own error text so the UI can explain why.
pub(crate) async fn rebase() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    // D2 (#66, Task 7): the validated resolution, replacing a raw
    // `state::current()` call — see `state::resolve_target`'s doc comment.
    // `rebase_status` below is a *read* (no `?repo=` selector either, same as
    // this one, but reachable with no write gate) and deliberately keeps its
    // own direct `current()` call — see "Read handlers wire it must NOT
    // bypass what they do today" in the D2 implementation report.
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };
    // D5 (#66, Task 19): the base is *chosen* by a git read, so an unreadable
    // one must not silently fall through to `main`. Rebasing onto the local
    // `main` when the intent was `origin/main` is a different operation on a
    // different commit — and the old `bool` return made that the outcome of
    // every host where git could not be launched.
    let base = match rebase_base(&repo).await {
        Ok(base) => base,
        Err(e) => {
            return planner::couldnt_run(
                "/api/rebase",
                &format!("couldn't determine the rebase base: {e}"),
            )
        }
    };
    let base = RefName::new(base).expect("'origin/main' and 'main' are valid ref names");
    planner::plan_and_execute(GitOperation::RebaseOntoBase { base }).await
}

/// The base "Rebase onto main" rebases onto: `origin/main` when that
/// remote-tracking ref exists — the usual feature-branch target, so you rebase
/// onto the freshest pushed main — and the local `main` otherwise. Shared by
/// the rebase handler and `/api/rebase-status`, so the menu's gate always
/// describes exactly what the rebase would do.
///
/// `Err` when git could not be run: "the remote-tracking ref is not there" and
/// "we could not look" pick different bases, so they may not share a return
/// value (D5).
async fn rebase_base(repo: &Path) -> Result<&'static str, ExecUnavailable> {
    Ok(if git_ref_exists(repo, "refs/remotes/origin/main").await? {
        "origin/main"
    } else {
        "main"
    })
}

/// Whether "Rebase onto main" would do anything right now (see [`RebaseStatus`]),
/// resolved fresh per request like `/api/head-branch` — the graph on screen may
/// predate a rebase or a branch switch. Sent `no-store` like the other live reads.
/// # D5: an unreadable repository answers 500, not `base_exists: false`
///
/// [`RebaseStatus`] is a protocol type with two `bool`s and no way to say "we
/// could not tell", so the honest answer when git cannot be run is not a body
/// at all. It used to be `{base: "main", base_exists: false, up_to_date:
/// false}` — three separate false statements about the repository, which the
/// menu renders as "Rebase onto main — base does not exist". The frontend
/// already treats a failed fetch as "unknown" (`fetch_rebase_status().ok()`),
/// so the degraded item is what it shows for a 500 too, minus the fiction.
pub(crate) async fn rebase_status() -> axum::response::Response {
    let repo = current().0;
    let branch = git_vista_git::read_head_branch(&repo);
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];

    // One fallible block: any of the three reads failing means the answer is
    // unknown, and `?` keeps that from being written any other way.
    let observed = async {
        let base = rebase_base(&repo).await?;
        let base_exists = rev_parse(&repo, base).await?.is_some();
        let up_to_date = base_exists && is_ancestor(&repo, base, "HEAD").await?;
        Ok::<_, ExecUnavailable>((base, base_exists, up_to_date))
    }
    .await;

    let (base, base_exists, up_to_date) = match observed {
        Ok(observed) => observed,
        Err(e) => {
            return (
                no_store,
                planner::couldnt_run(
                    "/api/rebase-status",
                    &format!("couldn't read the rebase state: {e}"),
                ),
            )
                .into_response()
        }
    };
    (
        no_store,
        Json(RebaseStatus {
            branch,
            base: base.to_string(),
            base_exists,
            up_to_date,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(repo: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
    }

    fn seeded_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        (dir, repo)
    }

    /// D5 (#66, Task 19): "no `origin/main`" and "we could not look" chose the
    /// *same* base before this change, because `git_ref_exists` returned a
    /// bare `bool`. They are different rebases onto different commits, so the
    /// second one must not silently become the first.
    #[tokio::test]
    async fn an_unreadable_repository_does_not_silently_pick_the_local_main() {
        let (_dir, repo) = seeded_repo();
        assert_eq!(
            rebase_base(&repo).await.expect("git runs here"),
            "main",
            "with no origin/main, the local main really is the base"
        );

        let (_hostile_dir, hostile) = crate::git_cmd::unrunnable_repo();
        assert!(
            rebase_base(&hostile).await.is_err(),
            "an unreadable repository must not answer ‘main’ — that is a \
             choice of rebase target made on no evidence"
        );
    }
}
