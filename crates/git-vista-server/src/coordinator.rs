//! The mutation coordinator (M1.07, #60): one guard per shared repository, so
//! two app-initiated writes can never interleave.
//!
//! Since M1.06 every write takes one path — `planner::plan_and_execute` — which
//! builds a reviewable plan, re-verifies it against the live repository (#145)
//! and only then executes. That gate *detects* drift; it cannot prevent two
//! requests from both passing it against the same state and then both mutating.
//! A double-clicked Commit lands in exactly that window. This module closes it:
//! the planner holds a guard from before its first observation until after its
//! last mutation, so the whole pipeline is atomic against other app writes.
//!
//! **The key is [`RepositoryId`], not `WorktreeId`.** Refs, packed-refs and the
//! object store are shared by every linked worktree of one clone, and
//! `RepositoryId` is derived from precisely that shared common directory — so a
//! per-repository guard also serializes every worktree of it. A per-worktree
//! guard would leave two linked worktrees free to race on one ref.
//!
//! **What this does not do:** it binds *this process's* mutations only. A git
//! command run from a terminal is outside it, by construction — see
//! [`refuse_if_git_busy`] for how that is detected instead, and ADR 0019 for
//! why detection rather than exclusion is the honest posture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::http::StatusCode;
use tokio::sync::{Mutex, OwnedMutexGuard};

use git_vista_core::identity::RepositoryId;

/// One async guard per shared repository, created on first use.
///
/// The outer `std` mutex protects only the map and is **never** held across an
/// `.await`: [`lock`] clones the `Arc` out and drops the map guard before
/// awaiting the inner one. The map only ever grows — one entry per repository
/// this process has served, bounded by the catalog — so there is no eviction
/// path to get wrong.
static LOCKS: OnceLock<StdMutex<HashMap<Key, Arc<Mutex<()>>>>> = OnceLock::new();

/// A repository, or the single fallback bucket for degraded-mode writes.
///
/// Degraded mode is a served path that would not classify as a git repository,
/// so it has no catalog entry and no id. Those writes share one guard rather
/// than skipping serialization: "we don't know which repository this is" must
/// never mean "so let them all run at once".
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Repository(RepositoryId),
    Unregistered,
}

fn locks() -> &'static StdMutex<HashMap<Key, Arc<Mutex<()>>>> {
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Acquire the mutation guard for `repo`, waiting for any in-flight mutation of
/// the same repository to finish.
///
/// Waiters are served in order (`tokio::sync::Mutex` is FIFO-fair), so the wait
/// queue *is* the queue — there is no separate queue type to introspect or
/// drain. Awaiting is **cancel-safe in the way #60 needs**: dropping the
/// returned future — which is what axum does when the client disconnects —
/// removes that waiter without ever acquiring, so a cancelled request runs no
/// git at all.
///
/// The guard releases when the returned value drops, including while unwinding
/// from a panic (`tokio::sync::Mutex` does not poison; ADR 0019 records why
/// that is the accepted posture here).
pub(crate) async fn lock(repo: Option<RepositoryId>) -> OwnedMutexGuard<()> {
    let key = match repo {
        Some(id) => Key::Repository(id),
        None => Key::Unregistered,
    };
    let guard = {
        let mut map = locks().lock().expect("coordinator registry lock");
        Arc::clone(map.entry(key).or_default())
        // the std guard drops here — before the await below, never across it
    };
    guard.lock_owned().await
}

/// Refuse the operation when an **external** git process is mid-write in this
/// repository, identified by the `index.lock` git itself takes.
///
/// The guard above binds only this server's writes. A `git` run from a
/// terminal, an editor's git integration, or another agent on the same box is
/// outside it and always will be. This turns the resulting collision from a
/// confusing raw git error into one sentence a browser-only user can act on.
///
/// This is a **courtesy check, not a guarantee** — the external process can
/// take the lock in the moment after this returns. When that happens git
/// refuses on its own and its stderr is forwarded verbatim, exactly as before
/// this existed. The real mutual exclusion against outside git has always been,
/// and remains, git's own lock file.
///
/// The path is resolved with `git rev-parse --absolute-git-dir` rather than
/// assumed to be `<repo>/.git`: a linked worktree's git dir lives under the
/// common directory and keeps its own index (and so its own `index.lock`),
/// while `.git` in that worktree is a *file* pointing there. A path whose git
/// dir cannot be resolved is left alone — the planner's own stages surface
/// git's error for something that isn't a repository.
pub(crate) async fn refuse_if_git_busy(repo: &Path) -> Option<(StatusCode, String)> {
    match absolute_git_dir(repo).await {
        // git could not be run (D5, #66 Task 19). This preflight had the same
        // polarity bug as `Precondition::RefAbsent`: `absolute_git_dir`
        // answered `None` both for "git ran and said this is not a
        // repository" and for "git could not be run", `?` propagated that
        // `None` straight out, and `None` from this function means **not
        // busy** — so the one input that proves we know nothing about the
        // repository read as a clean bill of health and the mutation went
        // ahead. Nothing downstream re-checked it: the whole point of this
        // function is that the lock it looks for is git's, not ours.
        Err(e) => Some(crate::planner::couldnt_run(
            "busy preflight",
            &format!(
                "couldn't resolve the git directory of {}: {e}",
                repo.display()
            ),
        )),
        // git ran and this is not a repository. Left alone exactly as before —
        // the planner's own stages surface git's error for it.
        Ok(None) => None,
        Ok(Some(git_dir)) => git_dir.join("index.lock").exists().then(|| {
            (
                StatusCode::CONFLICT,
                "Another git process is working in this repository — wait for it to \
                 finish and try again."
                    .to_string(),
            )
        }),
    }
}

/// This worktree's own git directory, absolute.
///
/// `Ok(None)` is git's own answer that this path is not a repository (a
/// non-zero exit, or an empty one) — a fact, handled downstream. `Err` is git
/// not running at all, which is a fact about nothing; see [`refuse_if_git_busy`].
async fn absolute_git_dir(repo: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    let output = crate::git_cmd::git_output(repo, &["rev-parse", "--absolute-git-dir"]).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!dir.is_empty()).then(|| PathBuf::from(dir)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Two distinct ids, derived exactly the way production derives them.
    fn ids() -> (RepositoryId, RepositoryId) {
        (
            RepositoryId::from_common_dir("/tmp/coordinator-test/one/.git"),
            RepositoryId::from_common_dir("/tmp/coordinator-test/two/.git"),
        )
    }

    /// The core contract: while one guard is held, a second acquire for the
    /// same repository does not complete.
    #[tokio::test]
    async fn a_second_acquire_waits_while_the_first_is_held() {
        let one = RepositoryId::from_common_dir("/tmp/coordinator-test/waits/.git");
        let held = lock(Some(one)).await;

        let waiter = tokio::time::timeout(Duration::from_millis(100), lock(Some(one))).await;
        assert!(
            waiter.is_err(),
            "a second acquire must not complete while the first is held"
        );

        drop(held);
        let after = tokio::time::timeout(Duration::from_millis(100), lock(Some(one))).await;
        assert!(
            after.is_ok(),
            "releasing the guard must let the next waiter in"
        );
    }

    /// Different repositories never block each other — the guard is per
    /// repository, not process-wide.
    #[tokio::test]
    async fn distinct_repositories_do_not_block_each_other() {
        let (one, two) = ids();
        let _held = lock(Some(one)).await;
        let other = tokio::time::timeout(Duration::from_millis(100), lock(Some(two))).await;
        assert!(
            other.is_ok(),
            "a different repository must acquire immediately"
        );
    }

    /// Degraded mode (no catalog entry, so no id) still serializes, on one
    /// shared fallback guard — fail-closed, never "no id means no lock".
    #[tokio::test]
    async fn degraded_mode_writes_still_serialize() {
        let _held = lock(None).await;
        let second = tokio::time::timeout(Duration::from_millis(100), lock(None)).await;
        assert!(second.is_err(), "degraded-mode writes must serialize too");
    }

    /// Acceptance criterion 3: a waiter dropped before it acquires leaves the
    /// queue cleanly — it never runs, and it strands the guard for nobody.
    #[tokio::test]
    async fn a_dropped_waiter_never_acquires_and_never_strands_the_guard() {
        let one = RepositoryId::from_common_dir("/tmp/coordinator-test/cancel/.git");
        let held = lock(Some(one)).await;

        let mut waiter = Box::pin(lock(Some(one)));
        // Poll it far enough to be queued, then confirm it is still pending.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "the waiter should still be queued"
        );
        drop(waiter); // the client disconnected: cancel before start

        drop(held);
        let next = tokio::time::timeout(Duration::from_millis(100), lock(Some(one))).await;
        assert!(
            next.is_ok(),
            "a cancelled waiter must not hold the guard hostage"
        );
    }

    /// Serialization under real contention: several tasks increment a counter
    /// while holding the guard, asserting no two are ever inside at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_holders_never_overlap() {
        let one = RepositoryId::from_common_dir("/tmp/coordinator-test/contended/.git");
        let inside = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let inside = Arc::clone(&inside);
            handles.push(tokio::spawn(async move {
                let _guard = lock(Some(one)).await;
                let seen = inside.fetch_add(1, Ordering::SeqCst);
                assert_eq!(seen, 0, "two holders were inside the guard at once");
                tokio::time::sleep(Duration::from_millis(5)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.expect("no task panicked");
        }
    }

    /// A repository with no git process working in it is not refused; one with
    /// a live `index.lock` is, in words a browser-only user can act on.
    #[tokio::test]
    async fn an_index_lock_marks_the_repository_busy() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        assert!(
            refuse_if_git_busy(&repo).await.is_none(),
            "an idle repository is not busy"
        );

        std::fs::write(repo.join(".git").join("index.lock"), "").unwrap();
        let (status, why) = refuse_if_git_busy(&repo).await.expect("busy");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            why.contains("Another git process is working in this repository"),
            "{why}"
        );

        std::fs::remove_file(repo.join(".git").join("index.lock")).unwrap();
        assert!(
            refuse_if_git_busy(&repo).await.is_none(),
            "once the external process finishes, writes are allowed again"
        );
    }

    /// A path that is not a repository is left alone here — the planner's own
    /// stages surface git's error for it, unchanged.
    #[tokio::test]
    async fn a_path_that_is_not_a_repository_is_not_reported_busy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refuse_if_git_busy(dir.path()).await.is_none());
    }

    /// D5 (#66, Task 19): the busy preflight's own polarity bug.
    ///
    /// `None` from this function means **proceed with the mutation**, and the
    /// old body reached it via `absolute_git_dir(repo).await?` — where
    /// `absolute_git_dir` returned `None` for "git could not be run" just as
    /// readily as for "git says this is not a repository". So the one answer
    /// that means *we know nothing about this repository* read as a clean bill
    /// of health.
    ///
    /// The two assertions are the whole point: the fixture must land on the
    /// git-unavailable path (a 500) while the plain non-repository above still
    /// lands on `None`. If both answered the same, this preflight would have
    /// been "fixed" into refusing every degraded-mode write.
    #[tokio::test]
    async fn a_repository_git_cannot_run_in_is_refused_not_declared_idle() {
        let (_dir, hostile) = crate::git_cmd::unrunnable_repo();

        // The pre-D5 expression, written out against the same fixture.
        let pre_d5_said_not_busy = crate::git_cmd::git_output(&hostile, &["rev-parse"])
            .await
            .is_err();
        assert!(
            pre_d5_said_not_busy,
            "the fixture must be one where git genuinely cannot run, or this \
             test pins nothing"
        );

        let (status, why) = refuse_if_git_busy(&hostile)
            .await
            .expect("git failing to run is not evidence that nobody holds the index");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(why.contains("Couldn't run git"), "{why}");
    }
}
