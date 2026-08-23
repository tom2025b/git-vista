//! #72 (M2.19), "hooks cannot freeze the UI" — behavioural proof that a
//! `pre-commit`/`post-commit` hook which never returns cannot hang a request
//! forever, and cannot hold the per-repository mutation guard forever either.
//!
//! Real fixture repos, real hook scripts, real spawns — the same house
//! pattern `pull_suite`/`push_suite`/`coordination_suite` use, because the
//! claim ("a hung child process is killed, and the guard it was running
//! under releases") cannot be proved against a mock.
//!
//! The bound needs to be small for these tests to run in reasonable time —
//! [`super::HOOKED_GIT_TIMEOUT_OVERRIDE`] is the test-only, thread-scoped
//! knob that shrinks it; see that item's doc for why a `thread_local` and not
//! a process-wide value.

use super::*;
use std::path::PathBuf;
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
        RepositoryToken::new("hook-timeout-suite-repo").unwrap(),
        WorktreeToken::new("hook-timeout-suite-worktree").unwrap(),
    )
}

/// The real [`RepositoryId`] for a repository on disk — the same derivation
/// the catalog performs, so these tests key the guard exactly as production
/// does.
fn repo_id(repo: &Path) -> RepositoryId {
    git_vista_git::read_repo_facts(repo)
        .expect("a seeded repo classifies")
        .handle
        .repository
}

/// How many commits `rev` has, for asserting a timed-out commit really left
/// no trace.
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

/// Install an executable hook that never returns. `name` is one of
/// `pre-commit`, `post-commit`, … — the argv these tests exercise is always
/// `git commit`, so any hook `git commit` itself invokes proves the same
/// point.
fn write_sleep_forever_hook(repo: &Path, name: &str) {
    let hooks_dir = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let path = hooks_dir.join(name);
    // A real number, not a symbolic "forever": long enough that no bound
    // this suite sets could ever legitimately outlast it, short enough that
    // a leaked, un-killed process (a regression this suite exists to catch)
    // does not sit on the test-runner box indefinitely.
    std::fs::write(&path, "#!/bin/sh\nsleep 10000\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// Set this test's thread-local [`super::HOOKED_GIT_TIMEOUT_OVERRIDE`] for
/// its whole duration. Sound only because `#[tokio::test]`'s default
/// current-thread runtime runs the entire test — including every
/// `tokio::spawn`ed subtask — on the one OS thread the Rust test harness
/// already dedicated to this test function; see the override's own doc for
/// the full argument. **Every test in this file must stay on that default
/// flavor** — `flavor = "multi_thread"` would let the runtime move a
/// polled future to a different worker thread, where this override would
/// silently stop applying.
fn set_test_hooked_timeout(d: Duration) {
    HOOKED_GIT_TIMEOUT_OVERRIDE.with(|c| c.set(Some(d)));
}

/// Install this test's [`super::LOCK_ACQUIRED_SIGNAL`] and hand back the
/// `Notify` to await on — mirrors [`set_test_hooked_timeout`] above; see the
/// thread-local's own doc for why a thread-scoped `Notify` and not some
/// process-wide mechanism.
fn set_test_lock_acquired_signal() -> std::rc::Rc<tokio::sync::Notify> {
    let notify = std::rc::Rc::new(tokio::sync::Notify::new());
    LOCK_ACQUIRED_SIGNAL.with(|c| c.set(Some(std::rc::Rc::clone(&notify))));
    notify
}

// ---------------------------------------------------------------------------
// The timeout fires
// ---------------------------------------------------------------------------

/// A `pre-commit` that sleeps forever cannot hang `/api/commit` forever: the
/// bounded spawn is killed, and the request answers with a typed-in-prose
/// refusal that names the timeout and states — correctly, because
/// `pre-commit` runs before any commit object exists — that no commit was
/// created.
///
/// The outer `tokio::time::timeout` is the belt to `run_git_hooked`'s own
/// suspenders, the same pattern `git_output_bounded_reports_timed_out_when_the_bound_is_too_tight`
/// uses: if the mechanism under test ever stopped enforcing its own bound,
/// this test must still fail in bounded time rather than wedging the suite.
///
/// Mutation tried: revert `exec_commit_on_head` to call `run_git` instead of
/// `run_git_hooked` — the request never returns, the outer 10s belt elapses,
/// and `.expect(...)` panics the test instead of the assertions below ever
/// running.
#[tokio::test]
async fn a_commit_with_a_hook_that_sleeps_forever_times_out_and_answers() {
    set_test_hooked_timeout(Duration::from_millis(400));
    let (_dir, repo) = seeded_repo();
    write_sleep_forever_hook(&repo, "pre-commit");
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    let (status, body) = tokio::time::timeout(
        Duration::from_secs(10),
        plan_and_execute_in(&repo, Some(id), tokens(), commit("should hang forever")),
    )
    .await
    .expect(
        "a hooked commit must answer on its own within the 400ms bound, well inside this \
         test's 10s outer belt — a hang here is exactly the regression this test exists to \
         catch",
    );

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a killed hook is a refusal, not a couldn't-run 500 or a false success: {body}"
    );
    assert!(
        body.contains("didn't finish within") && body.contains("400ms"),
        "the refusal must name the actual bound it ran under, not just say it happened: {body}"
    );
    assert!(
        body.contains("No commit was created"),
        "pre-commit runs before the commit object exists, so the honest answer is that \
         nothing landed: {body}"
    );
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before,
        "HEAD must genuinely be unchanged, not just described that way"
    );
}

// ---------------------------------------------------------------------------
// The coordinator guard releases
// ---------------------------------------------------------------------------

/// The property the acceptance criterion actually names: a hung hook costs
/// the guard at most the bound, never forever. A `CreateBranch` queued behind
/// a timing-out commit on the **same repository** completes once the timeout
/// arm returns — proving the guard was dropped, not merely that the timeout
/// fired.
///
/// `tokio::join!` (not `tokio::spawn` on a `multi_thread` runtime) drives the
/// two operations: both futures are polled on this test's one OS thread —
/// true, and load-bearing, since it is what keeps
/// [`set_test_hooked_timeout`]'s thread-local override valid for both. **It
/// does not, on its own, make guard arrival a deterministic queue.**
/// `build_plan` runs before `coordinator::lock` is taken (deliberately — see
/// [`plan_and_execute_in`]'s own doc) and does genuine OS-scheduled work
/// before either future ever reaches the guard: a `rev_parse` subprocess
/// spawn and a `refs_digest_input` `spawn_blocking`. Both futures sharing one
/// OS thread says nothing about which one's subprocess or blocking thread the
/// OS returns control to first — that part really is a race, and it used to
/// decide this test's outcome: when `CreateBranch` won it, it created its ref
/// before the commit's plan was re-checked, moved the generation the
/// commit's plan was built against, and made the commit's `enforce_fresh`
/// correctly refuse with 409 ("the repository changed") instead of the 400
/// this test means to observe — an intermittent CI failure (#444), not a
/// production bug.
///
/// So this test does not lean on scheduling order at all: it installs
/// [`set_test_lock_acquired_signal`] and gates the create-branch leg on it,
/// so `CreateBranch` cannot even begin building its plan until the commit leg
/// is provably holding `coordinator::lock` — see that signal's own doc
/// ([`super::LOCK_ACQUIRED_SIGNAL`]) for the full argument.
///
/// Mutation tried: have the timeout arm `return` before the coordinator
/// guard's scope ends without actually letting `execute()`'s stack frame
/// unwind (e.g. `std::mem::forget` the guard) — the create-branch future
/// then never wakes, the outer belt elapses, and the `.expect` panics.
#[tokio::test]
async fn the_coordinator_lock_is_released_after_a_hook_timeout() {
    set_test_hooked_timeout(Duration::from_millis(400));
    let (_dir, repo) = seeded_repo();
    write_sleep_forever_hook(&repo, "pre-commit");
    let id = repo_id(&repo);
    let head = rev_parse(&repo, "HEAD")
        .await
        .expect("git runs in a fixture repo")
        .expect("HEAD resolves");
    let lock_acquired = set_test_lock_acquired_signal();

    let (commit_result, branch_result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            plan_and_execute_in(&repo, Some(id), tokens(), commit("hangs")),
            async {
                // Do not even begin building a plan for `CreateBranch` until
                // the commit leg is provably holding `coordinator::lock` —
                // see `LOCK_ACQUIRED_SIGNAL`'s doc for why anything looser
                // races `build_plan`'s genuinely concurrent pre-lock work.
                lock_acquired.notified().await;
                plan_and_execute_in(
                    &repo,
                    Some(id),
                    tokens(),
                    GitOperation::CreateBranch {
                        name: BranchName::new("after-hook-timeout").unwrap(),
                        at: CommitOid::new(head).unwrap(),
                    },
                )
                .await
            },
        )
    })
    .await
    .expect(
        "both operations together must finish well inside 10s if the guard truly releases \
         after the 400ms bound",
    );

    assert_eq!(
        commit_result.0,
        StatusCode::BAD_REQUEST,
        "the hooked commit must still be refused as a timeout: {}",
        commit_result.1
    );
    assert_eq!(
        branch_result.0,
        StatusCode::OK,
        "the queued create-branch must succeed once the guard is free: {}",
        branch_result.1
    );
    assert!(
        rev_parse(&repo, "after-hook-timeout")
            .await
            .expect("git runs in a fixture repo")
            .is_some(),
        "the branch created while queued behind the timed-out commit must really exist"
    );
}

// ---------------------------------------------------------------------------
// A normal commit is unaffected
// ---------------------------------------------------------------------------

/// Positive control, run under the real production [`HOOKED_GIT_TIMEOUT`]
/// (no override set): a plain commit with no hooks at all completes normally
/// — `run_git_hooked` is not a slower `run_git`, and wiring the bound in did
/// not turn every commit into a 30-second wait.
///
/// Mutation tried: swap `BoundedOutput::Completed(o) => o` for
/// `BoundedOutput::TimedOut => o` (an inverted match) in the arm that
/// unwraps `run_git_hooked`'s result — a fast, successful commit would then
/// be reported as a timeout refusal instead of `StatusCode::OK`.
#[tokio::test]
async fn a_commit_with_no_hooks_completes_normally_under_the_real_bound() {
    let (_dir, repo) = seeded_repo();
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    let (status, body) =
        plan_and_execute_in(&repo, Some(id), tokens(), commit("ordinary commit")).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(commit_count(&repo, "HEAD"), before + 1);
}

// ---------------------------------------------------------------------------
// A hook hanging in post-commit: the commit already landed
// ---------------------------------------------------------------------------

/// The kill races git's own ref write. A `post-commit` hook (which git runs
/// *after* the commit object and ref are already written) that hangs forever
/// still leaves a real commit behind — the timeout arm's bounded HEAD
/// re-read must find it and say so, rather than collapsing every timeout
/// into "nothing happened".
///
/// Mutation tried: collapse `HookTimeoutHeadCheck::Moved(_)` into
/// `HookTimeoutHeadCheck::Unchanged` in `check_head_after_hook_timeout` — the
/// response would then claim "No commit was created" about a repository
/// whose `HEAD` had, provably, just moved.
#[tokio::test]
async fn a_hook_hanging_in_post_commit_reports_the_commit_that_landed() {
    set_test_hooked_timeout(Duration::from_millis(400));
    let (_dir, repo) = seeded_repo();
    write_sleep_forever_hook(&repo, "post-commit");
    let id = repo_id(&repo);
    let before = commit_count(&repo, "HEAD");

    let (status, body) = tokio::time::timeout(
        Duration::from_secs(10),
        plan_and_execute_in(&repo, Some(id), tokens(), commit("lands, then hangs")),
    )
    .await
    .expect("post-commit hanging must not hang the request itself");

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("A commit was created"),
        "HEAD genuinely moved before post-commit hung, and the response must say so: {body}"
    );
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before + 1,
        "the commit git already wrote must not have been rolled back"
    );
}
