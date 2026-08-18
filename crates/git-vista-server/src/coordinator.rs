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
/// This is a **courtesy check, not a guarantee** — a live external process can
/// take the lock in the moment after this returns. When that happens git
/// refuses on its own and its stderr is forwarded verbatim, exactly as before
/// this existed. The real mutual exclusion against outside git has always been,
/// and remains, git's own lock file.
///
/// **A present lock is verified live, not just present** (#72 defect fix): a
/// `git` process (including one of ours, killed mid-hook) that dies without
/// releasing `index.lock` leaves it on disk, and existence alone cannot tell
/// that apart from a real in-progress write — see
/// [`index_lock_is_open_by_a_live_process`]. Confusing the two used to make
/// this function assert "another git process is working" as a fact it could
/// not know, and once true, that assertion could never become false again:
/// every following request against the repository was refused, forever,
/// recoverable only by a human with shell access
/// (docs/superpowers/evidence/m1.13-design-trail/m1.13-findings.md, I9/I11).
/// A lock confirmed to have no live holder is therefore removed here before
/// answering "not busy" — necessary, not just tidy: git's own lockfile
/// creation is `O_CREAT|O_EXCL`, so leaving the orphan on disk would still
/// make the very next git command fail with its own permanent "File exists".
///
/// The path is resolved with `git rev-parse --absolute-git-dir` rather than
/// assumed to be `<repo>/.git`: a linked worktree's git dir lives under the
/// common directory and keeps its own index (and so its own `index.lock`),
/// while `.git` in that worktree is a *file* pointing there. A path git itself
/// reports is not a repository is left alone — the planner's own stages surface
/// git's error for it. A path where git could not be *run* is refused instead;
/// see the `Err` arm below for why those two had to stop sharing an answer.
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
        Ok(Some(git_dir)) => {
            let lock_path = git_dir.join("index.lock");
            if !lock_path.exists() {
                return None;
            }
            if index_lock_is_open_by_a_live_process(&lock_path) {
                return Some((
                    StatusCode::CONFLICT,
                    "Another git process is working in this repository — wait for it to \
                     finish and try again."
                        .to_string(),
                ));
            }
            // Verified stale: nothing has this file open. Whatever wrote it
            // died before renaming it onto `index` (success) or unlinking it
            // (abort) — either way there is no in-progress write for this to
            // interrupt, and discarding it is exactly what an aborted write
            // should have done itself. `remove_file` failing here (already
            // gone, e.g. a genuine race with the dying process's own cleanup)
            // is not an error worth surfacing — the outcome is "not busy"
            // either way.
            let _ = std::fs::remove_file(&lock_path);
            None
        }
    }
}

/// Whether any process on this host currently holds `path` open — the
/// liveness check [`refuse_if_git_busy`] needs to tell a live `index.lock`
/// from an orphaned one.
///
/// Linux-only (this server has no non-Linux target; see the sandbox shim's
/// own landlock/seccomp dependencies): walks `/proc/<pid>/fd`, comparing each
/// open fd's `(device, inode)` against `path`'s — not the fd's path string,
/// because a held lock's directory entry is exactly the thing that can be
/// removed and recreated by an unrelated process while the original file (and
/// the original holder's fd) still exists under a different name. Matching by
/// identity rather than name is what makes that unambiguous.
///
/// A single process's `/proc/<pid>/fd` failing to read (it exited between the
/// listing and the read, or belongs to a different user) is not evidence
/// either way for that one process — skipped, not counted as "not holding
/// it". Only two things are genuinely fail-safe: unable to `stat` `path`
/// itself, or unable to enumerate `/proc` at all — both mean this check
/// cannot be trusted, so both answer `true` (assume live) rather than risk
/// declaring a real in-progress write stale.
#[cfg(unix)]
fn index_lock_is_open_by_a_live_process(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let target = match std::fs::metadata(path) {
        Ok(m) => (m.dev(), m.ino()),
        Err(_) => return true,
    };

    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return true,
    };

    for pid_entry in proc_dir.flatten() {
        let pid_name = pid_entry.file_name();
        let is_pid_dir = pid_name
            .to_str()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid_dir {
            continue; // /proc has non-pid entries too (self, meminfo, ...)
        }

        let fd_dir = match std::fs::read_dir(pid_entry.path().join("fd")) {
            Ok(d) => d,
            // The process exited since the listing, or its fd directory is
            // not ours to read — neither tells us anything about `target`.
            Err(_) => continue,
        };
        for fd_entry in fd_dir.flatten() {
            if let Ok(meta) = std::fs::metadata(fd_entry.path()) {
                if (meta.dev(), meta.ino()) == target {
                    return true;
                }
            }
        }
    }

    false
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

    /// A repository with no git process working in it is not refused; one
    /// where a real process still holds `index.lock` open is, in words a
    /// browser-only user can act on.
    ///
    /// The holder is a real `git add` process, blocked mid-operation inside a
    /// slow repo-local `clean` filter, that keeps `index.lock` open for the
    /// span of the check — not just a file dropped on disk — because that
    /// liveness is exactly what distinguishes this case from the stale-lock
    /// defect covered by `a_stale_index_lock_does_not_refuse_the_repository_forever`
    /// below. It spawns `git`, not a shell, so it cannot regress
    /// `argv_boundary`'s tripwire (this file spawns only `git` literally).
    #[tokio::test]
    async fn an_index_lock_held_by_a_live_process_marks_the_repository_busy() {
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

        let lock_path = repo.join(".git").join("index.lock");
        assert!(std::process::Command::new("git")
            .args(["config", "filter.holdlock.clean", "sleep 5; cat"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.join(".gitattributes"), "held.txt filter=holdlock\n").unwrap();
        std::fs::write(repo.join("held.txt"), "held\n").unwrap();
        let mut holder = std::process::Command::new("git")
            .args(["add", "held.txt"])
            .current_dir(&repo)
            .spawn()
            .expect("spawn git add to hold index.lock via a slow clean filter");
        // Give the filter time to actually start and git time to take the
        // lock before the assertions below race it.
        let deadline = std::time::Instant::now() + Duration::from_millis(2000);
        while !lock_path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            lock_path.exists(),
            "the slow-filter fixture did not take index.lock in time"
        );

        // Mutation tried: skip the liveness probe and go back to
        // existence-only — this assertion still passes (a held lock exists
        // too), but the paired stale-lock test below then fails, which is
        // the whole point of splitting these two cases apart.
        let (status, why) = refuse_if_git_busy(&repo).await.expect("busy");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            why.contains("Another git process is working in this repository"),
            "{why}"
        );
        assert!(
            lock_path.exists(),
            "a lock a live process still holds must never be removed out from \
             under it"
        );

        holder.kill().expect("kill the holder");
        holder.wait().expect("reap the holder");
        std::fs::remove_file(&lock_path).unwrap();
        assert!(
            refuse_if_git_busy(&repo).await.is_none(),
            "once the external process finishes, writes are allowed again"
        );
    }

    /// The confirmed #72 defect: `index.lock` orphaned by a process that has
    /// already died (a SIGKILLed hook, an OOM-kill, a server crash mid-write)
    /// must not refuse the repository forever. Before this fix,
    /// `refuse_if_git_busy` tested only the file's existence and could never
    /// tell this case apart from a live external git — so once one process
    /// died holding the lock, every commit/stage/checkout/merge/rebase/push
    /// against that repository returned 409 "wait for it to finish and try
    /// again" permanently, recoverable only by a human with shell access
    /// (docs/superpowers/evidence/m1.13-design-trail/m1.13-findings.md,
    /// lines 21-24).
    ///
    /// Answering "not busy" alone does not fix this: git's own lockfile
    /// creation is `O_CREAT|O_EXCL`, so a stale `index.lock` left on disk
    /// still makes the *next* git command fail with its own permanent
    /// `fatal: Unable to create '.../index.lock': File exists.` — verified
    /// empirically against real git 2.43 before writing this test. The fix
    /// has to remove a lock it has verified nothing holds open, not merely
    /// stop reporting it.
    #[tokio::test]
    async fn a_stale_index_lock_does_not_refuse_the_repository_forever() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.join("f"), "x").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "f"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        // Orphaned lock: written directly, never opened by any live process
        // — exactly what a killed hook or a crashed server leaves behind.
        // Nothing in this test process, or any other, has this fd open.
        let lock_path = repo.join(".git").join("index.lock");
        std::fs::write(&lock_path, "").unwrap();

        // Mutation tried: revert refuse_if_git_busy to the pre-fix
        // existence-only check — this assertion fails, since the old code
        // reports 409 unconditionally whenever the file exists.
        assert!(
            refuse_if_git_busy(&repo).await.is_none(),
            "a lock nobody holds open must not be reported busy"
        );

        // Mutation tried: make the fix answer `None` without removing the
        // stale file — this assertion then fails, because git's own O_EXCL
        // lockfile creation still refuses to run with the orphan on disk.
        std::fs::write(repo.join("f2"), "y").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "f2"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(
            add.success(),
            "the repository must be usable again, not refused forever"
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
