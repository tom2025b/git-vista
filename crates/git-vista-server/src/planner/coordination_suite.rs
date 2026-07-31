//! The M1.07 coordination suite (#60): proof that concurrent app mutations of
//! one repository serialize, that a cancelled request never runs git, that
//! linked worktrees of one clone share a single guard, and that an external git
//! process is detected rather than collided with.
//!
//! Like `contract_suite`, these drive the injectable entry point
//! [`plan_and_execute_in`] rather than `plan_and_execute`, which reads the
//! process-global selection (`state::CURRENT` is set-once per process and owned
//! by `state`'s own test — see the invariant note there).

use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use git_vista_core::identity::RepositoryId;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn run(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed in {repo:?}"
    );
}

/// A fresh repository on `main` with one commit and a clean working tree.
fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
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

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

/// The real [`RepositoryId`] for a repository on disk — the same derivation the
/// catalog performs, so these tests key the guard exactly as production does.
fn repo_id(repo: &Path) -> RepositoryId {
    git_vista_git::read_repo_facts(repo)
        .expect("a seeded repo classifies")
        .handle
        .repository
}

/// How many commits `rev` has, for asserting a mutation landed exactly once.
fn commit_count(repo: &Path, rev: &str) -> usize {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", rev])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn commit(message: &str) -> GitOperation {
    GitOperation::CommitOnHead {
        message: CommitMessage::new(message).unwrap(),
        allow_empty: true,
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion 1 — conflicting operations cannot run concurrently
// ---------------------------------------------------------------------------

/// **The double-click.** Two identical commit requests fired at once produce
/// exactly ONE commit: the guard serializes them, and the loser's staleness
/// gate (#145) then sees the repository has moved and refuses.
///
/// Before #60 both requests observed the same generation, both passed the gate,
/// and both committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_clicked_commit_creates_exactly_one_commit() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    let (a, b) = tokio::join!(
        plan_and_execute_in(&repo, Some(id), tokens(), commit("double click")),
        plan_and_execute_in(&repo, Some(id), tokens(), commit("double click")),
    );

    let statuses = [a.0, b.0];
    assert!(
        statuses.contains(&StatusCode::OK),
        "one request must succeed, got {statuses:?}"
    );
    assert!(
        statuses.contains(&StatusCode::CONFLICT),
        "the duplicate must be refused 409, got {a:?} / {b:?}"
    );
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before + 1,
        "exactly one commit must have landed"
    );
}

/// **No interleaving.** Several requests creating the same branch: exactly one
/// succeeds and the repository is left with one branch at one tip — never a
/// half-applied or duplicated ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_branch_creates_leave_one_branch() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);
    let at = rev_parse(&repo, "HEAD")
        .await
        .expect("git runs in a fixture repo")
        .expect("HEAD resolves");

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let repo = repo.clone();
        let at = at.clone();
        tasks.push(tokio::spawn(async move {
            plan_and_execute_in(
                &repo,
                Some(id),
                tokens(),
                GitOperation::CreateBranch {
                    name: BranchName::new("feature").unwrap(),
                    at: CommitOid::new(at).unwrap(),
                },
            )
            .await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.expect("no task panicked").0 == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1, "exactly one create must succeed");
    assert!(
        rev_parse(&repo, "feature")
            .await
            .expect("git runs in a fixture repo")
            .is_some(),
        "the branch exists"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion 3 — queued operations can be cancelled before start
// ---------------------------------------------------------------------------

/// A queued request whose client disconnects is aborted before it ever reaches
/// git: the repository is untouched by it, and the guard is free afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_queued_request_never_mutates() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    // Hold the guard so the request below can only queue, never start.
    let held = crate::coordinator::lock(Some(id)).await;

    let queued = {
        let repo = repo.clone();
        tokio::spawn(async move {
            plan_and_execute_in(&repo, Some(id), tokens(), commit("never runs")).await
        })
    };
    // Let it reach the guard and block there.
    tokio::time::sleep(Duration::from_millis(50)).await;
    queued.abort(); // the client disconnected
    let _ = queued.await;

    drop(held);
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before,
        "a cancelled request must not have committed"
    );

    // The guard is usable afterwards: a real request still succeeds.
    let (status, body) =
        plan_and_execute_in(&repo, Some(id), tokens(), commit("after the cancel")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(commit_count(&repo, "HEAD"), before + 1);
}

// ---------------------------------------------------------------------------
// Acceptance criterion 2 — linked-worktree ref races
// ---------------------------------------------------------------------------

/// **The premise.** A linked worktree is a distinct worktree but the SAME
/// shared repository — which is precisely why the guard is keyed by
/// `RepositoryId` and not `WorktreeId`.
#[test]
fn a_linked_worktree_shares_the_repository_id_and_not_the_worktree_id() {
    let (_dir, repo) = seeded_repo();
    let linked = repo.parent().unwrap().join("linked");
    run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );

    let main = git_vista_git::read_repo_facts(&repo).unwrap().handle;
    let side = git_vista_git::read_repo_facts(&linked).unwrap().handle;

    assert_eq!(
        main.repository, side.repository,
        "linked worktrees share one repository id — the guard key"
    );
    assert_ne!(
        main.worktree, side.worktree,
        "…but they are distinct worktrees, which is why the worktree id would \
         be the wrong key"
    );
}

/// **The race.** Two worktrees of one clone mutating the shared ref store at
/// the same time serialize on one guard: only one touches it at a time, every
/// outcome is either success or a clean refusal, and the store stays intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mutations_from_two_linked_worktrees_serialize() {
    let (_dir, repo) = seeded_repo();
    let linked = repo.parent().unwrap().join("linked");
    run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );

    let id = repo_id(&repo);
    assert_eq!(id, repo_id(&linked), "one repository, two worktrees");
    let at = rev_parse(&repo, "HEAD")
        .await
        .expect("git runs in a fixture repo")
        .expect("HEAD resolves");

    // Each worktree creates its own branch in the shared ref store, at once.
    let (a, b) = tokio::join!(
        plan_and_execute_in(
            &repo,
            Some(id),
            tokens(),
            GitOperation::CreateBranch {
                name: BranchName::new("from-main-worktree").unwrap(),
                at: CommitOid::new(at.clone()).unwrap(),
            },
        ),
        plan_and_execute_in(
            &linked,
            Some(id),
            tokens(),
            GitOperation::CreateBranch {
                name: BranchName::new("from-linked-worktree").unwrap(),
                at: CommitOid::new(at.clone()).unwrap(),
            },
        ),
    );

    // Whichever ran second may have seen a moved generation and been refused —
    // that is the serialization working, not a bug. What must never happen is a
    // corrupted or half-written ref store.
    let landed = [a.0, b.0].iter().filter(|s| **s == StatusCode::OK).count();
    assert!(landed >= 1, "at least one must land: {a:?} / {b:?}");
    for (status, body) in [&a, &b] {
        assert!(
            *status == StatusCode::OK || *status == StatusCode::CONFLICT,
            "only success or a clean 409 refusal is acceptable, got {status}: {body}"
        );
    }
    assert!(
        std::process::Command::new("git")
            .args(["fsck", "--no-progress"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success(),
        "the shared object/ref store must be intact"
    );
}

// ---------------------------------------------------------------------------
// External git — detected, not collided with
// ---------------------------------------------------------------------------

/// An external git process holds `index.lock`. The server refuses in words a
/// browser-only user can act on, and — the part that matters — does not mutate.
#[tokio::test]
async fn a_repository_busy_with_an_external_git_is_refused() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    std::fs::write(repo.join(".git").join("index.lock"), "").unwrap();

    let (status, body) = plan_and_execute_in(&repo, Some(id), tokens(), commit("blocked")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body.contains("Another git process is working in this repository"),
        "{body}"
    );
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before,
        "a busy repository must not be mutated"
    );

    // Once the external process finishes, writes work again.
    std::fs::remove_file(repo.join(".git").join("index.lock")).unwrap();
    let (status, body) = plan_and_execute_in(&repo, Some(id), tokens(), commit("unblocked")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(commit_count(&repo, "HEAD"), before + 1);
}

/// A linked worktree keeps its own index — and so its own `index.lock` — under
/// the common dir, while its `.git` is a *file*. The check must resolve the
/// real git dir rather than assume `<root>/.git` is a directory.
#[tokio::test]
async fn the_busy_check_finds_a_linked_worktrees_own_index_lock() {
    let (_dir, repo) = seeded_repo();
    let linked = repo.parent().unwrap().join("linked");
    run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );

    let git_dir = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(&linked)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(
        repo.join(".git").is_dir() && !linked.join(".git").is_dir(),
        "the linked worktree's .git is a file, the main one's is a directory"
    );

    std::fs::write(PathBuf::from(&git_dir).join("index.lock"), "").unwrap();
    assert!(
        crate::coordinator::refuse_if_git_busy(&linked)
            .await
            .is_some(),
        "a linked worktree's own index.lock must be found"
    );
    // The main worktree has its own index and is NOT busy.
    assert!(
        crate::coordinator::refuse_if_git_busy(&repo)
            .await
            .is_none(),
        "one worktree's lock must not report a sibling as busy"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion 4 — blocking git work does not stall Tokio workers
// ---------------------------------------------------------------------------

/// A write in flight must not stall the runtime: other tasks keep making
/// progress while it runs. Driven on a deliberately small runtime (2 workers)
/// so blocking work on a worker thread would be observable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_write_does_not_stall_other_tasks() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);

    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            for _ in 0..200 {
                ticks.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };

    let (status, body) =
        plan_and_execute_in(&repo, Some(id), tokens(), commit("while others run")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert!(
        ticks.load(Ordering::SeqCst) > 1,
        "other tasks must keep running while a write is in flight"
    );
    counter.abort();
}

/// The planner path must not call the synchronous filesystem readers directly:
/// they go through `spawn_blocking`. A source-level pin, in the style of the
/// contract suite's funnel proof — it catches a future edit that quietly
/// reintroduces blocking work on an async worker thread.
#[test]
fn the_planner_path_does_not_call_sync_filesystem_readers_directly() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/planner.rs"))
        .expect("planner.rs is readable");
    let lines: Vec<&str> = src.lines().collect();
    for blocking in [
        "git_vista_git::read_head_branch(",
        "git_vista_git::read_refs(",
        "journal::append(",
        "journal::remove_from_snapshot(",
        "journal::clear(",
    ] {
        for (n, line) in lines.iter().enumerate() {
            if !line.contains(blocking) || line.trim_start().starts_with("//") {
                continue;
            }
            let window = lines[n.saturating_sub(6)..=n].join("\n");
            assert!(
                window.contains("spawn_blocking"),
                "planner.rs:{} calls {blocking} outside spawn_blocking — that runs \
                 synchronous filesystem work on an async worker thread",
                n + 1
            );
        }
    }
}
