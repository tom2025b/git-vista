//! M2.20c (#229): the fetch slice's *behavioural* tests — the ones that spawn
//! real git processes against real remotes, because the three properties this
//! slice claims cannot be proved any other way.
//!
//! `planner::fetch`'s own inline tests cover the pure functions (the progress
//! parser, the failure classifier, the ref diff) with paired negatives. This
//! file covers the claims a pure test cannot reach:
//!
//! * **progress genuinely reaches the operation record**, so the SSE stream
//!   has something finer than "executing" to send — with a paired leg proving
//!   a no-op fetch publishes *nothing*, so the first leg is not passing on a
//!   value some other code path put there;
//! * **cancellation genuinely terminates the child process** — asserted by
//!   finding the process in `/proc` while it runs and finding it *gone*
//!   afterwards, not by trusting a status code;
//! * **the dropped-connection replay** the issue asks for by name: a request
//!   whose response is lost, re-sent under the same idempotency key, replays
//!   the recorded result instead of running `git fetch` a second time;
//! * **a credential leaked by the remote never reaches the operation record**,
//!   with the premise asserted (unredacted, the same run *does* contain it).
//!
//! # The fixture shape, and why it looks odd
//!
//! Every remote here is a bare repository **inside the served repository's own
//! tree**. That is not tidiness — it is the sandbox: #66 Task 6 grants the
//! served repository and the system trees and nothing else, so a bare remote
//! in a sibling tempdir is denied outright and every fetch fails with git's
//! "does not appear to be a git repository" for a reason that has nothing to
//! do with what is under test. Under the repository's own grant, `upload-pack`
//! runs read-only and a fetch works.
//!
//! These are therefore **local transports**: no socket, no port, no DNS, and
//! no dependence on a network being present. The operation is still classified
//! `NetworkNeed::Remote` (classification is by typed operation, not by URL —
//! #66's D3), so every spawn here still runs through the Network tier, #228's
//! forced `-c core.askpass=` and the redaction chokepoint. The real-socket
//! half of that tier has its own coverage elsewhere:
//! `sandbox::network_exec`'s `https_suite` (a substituted-port policy against
//! a real HTTP server) and `contract_suite`'s push fixture (a `git daemon` on
//! the arbitrated port 9418).

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};
use git_vista_protocol::{
    FetchError, FetchFailureKind, FetchSuccess, GitOperation, IdempotencyKey, RemoteName,
    RepositoryToken, TransferPhase, WorktreeToken,
};

use super::{operation_hash, PlanSource};
use crate::operations::{Admission, Record};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn run(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed in {dir:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("fetch-suite-repo").unwrap(),
        WorktreeToken::new("fetch-suite-worktree").unwrap(),
    )
}

/// Keys are process-global registry state shared with every other test in this
/// binary, so each test mints its own.
fn key(name: &str) -> IdempotencyKey {
    IdempotencyKey::new(format!("fetch-suite-{name}")).unwrap()
}

fn fetch_op() -> GitOperation {
    GitOperation::FetchRemote {
        remote: RemoteName::new("origin").unwrap(),
    }
}

/// A repository whose `origin` (a bare repo inside its own tree — see the
/// module doc) is `ahead` **content** commits in front of the local
/// remote-tracking ref, so a fetch has genuinely several objects to enumerate,
/// count and compress.
///
/// Five is not an arbitrary number: measured against git 2.43.0, fetching a
/// single *empty* commit prints **no progress records at all** (just the
/// `From …` header and a ref line), while five content commits reliably
/// produce `Enumerating`/`Counting`/`Compressing` records. A progress test
/// built on the smaller fixture would have proved nothing.
///
/// `ahead == 0` gives the up-to-date fixture: `origin` is configured and
/// current, so a fetch runs and finds nothing to do.
///
/// # Why the extra commits are authored somewhere else
///
/// The obvious fixture — commit in the served repository, push, then rewind
/// it — produces a repository that *already holds every object* the fetch
/// would transfer, because it created them. git's negotiation then sends an
/// almost-empty pack and prints **no transfer progress at all**, and a
/// progress test built on it passes over nothing. The commits are therefore
/// authored in a scratch clone outside the served tree (with plain,
/// unsandboxed git, as all fixture setup here is) and pushed to the remote
/// from there, so the served repository genuinely lacks them.
fn repo_with_remote_ahead(ahead: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "seed\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    let remote = repo.join("upstream.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    run(&repo, &["push", "-q", "origin", "main"]);

    if ahead > 0 {
        // Drop the remote-tracking ref the push just wrote, so "the fetch
        // created it" is an observable fact rather than a no-op. For the
        // `ahead == 0` fixture the ref is deliberately *kept*: that fixture's
        // whole job is to be already up to date.
        run(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let authoring = dir.path().join("authoring");
        run(
            dir.path(),
            &[
                "clone",
                "-q",
                &remote.display().to_string(),
                &authoring.display().to_string(),
            ],
        );
        run(&authoring, &["config", "user.email", "t@example.invalid"]);
        run(&authoring, &["config", "user.name", "t"]);
        for n in 0..ahead {
            let name = format!("f{n}.txt");
            std::fs::write(authoring.join(&name), format!("content {n}\n")).unwrap();
            run(&authoring, &["add", &name]);
            run(&authoring, &["commit", "-q", "-m", &format!("c{n}")]);
        }
        run(&authoring, &["push", "-q", "origin", "main"]);
    }
    (dir, repo)
}

/// Admit one fetch operation into the registry and return everything the
/// caller needs to drive it the way `plan_and_execute_tracked` would.
fn admit_fetch(name: &str) -> (crate::operations::OperationHandle, std::sync::Arc<Record>) {
    let key = key(name);
    let op = fetch_op();
    let hash = operation_hash(&op);
    let (repository, worktree) = tokens();
    match crate::operations::admit(&key, &op, &hash, repository, worktree, None) {
        Admission::Fresh(handle, record) => (handle, record),
        _ => panic!("a fresh key must be admitted"),
    }
}

/// Run the guarded pipeline for `op` under `record`, so the executor sees the
/// operation's progress sink and cancellation latch — the same scope
/// `plan_and_execute_tracked`'s detached task establishes in production.
async fn run_tracked(
    repo: &Path,
    record: std::sync::Arc<Record>,
    op: GitOperation,
) -> (StatusCode, String) {
    let repo = repo.to_path_buf();
    crate::operations::with_progress(record, async move {
        super::plan_and_execute_in(&repo, None, tokens(), op).await
    })
    .await
}

/// Every live process whose argv names `repo` **and** is a fetch — i.e. the
/// sandbox shim / git child this server spawned for this repository.
///
/// A `/proc` scan rather than a pid handed back from the runner, on purpose:
/// the point is to observe the process from *outside* the code under test. A
/// pid the runner reported would be the runner's own claim about what it
/// spawned.
fn live_fetch_processes(repo: &Path) -> Vec<i32> {
    let needle = repo.display().to_string();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv = String::from_utf8_lossy(&raw).replace('\0', " ");
        if argv.contains(&needle) && argv.contains("fetch") {
            out.push(pid);
        }
    }
    out
}

/// Poll `f` until it is true or `limit` elapses; returns whether it became
/// true. Bounded so a broken expectation fails the test in seconds rather than
/// hanging the suite.
async fn within<F: FnMut() -> bool>(limit: Duration, mut f: F) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if f() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// A real fetch publishes real transfer progress onto its own operation
/// record, so the SSE stream has something finer than `Executing` to send.
///
/// The subscriber collects *every* published snapshot rather than reading the
/// final one: `watch` coalesces, and a test that only looked at the end state
/// would be satisfied by a single write at the finish line — which is exactly
/// the "one opaque running state" this slice exists to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_fetch_publishes_transfer_progress_on_its_operation_record() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    let (handle, record) = admit_fetch("progress");

    let mut rx = record.subscribe();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow_and_update();
            if let Some(p) = snapshot.progress {
                seen.push(p);
            }
            if snapshot.is_terminal() {
                break;
            }
        }
        seen
    });

    let (status, body) = run_tracked(&repo, record.clone(), fetch_op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    handle.finish(status, body, None);

    let seen = collector.await.unwrap();
    assert!(
        !seen.is_empty(),
        "a five-commit fetch must publish at least one transfer report; the \
         stream would otherwise show nothing but `executing` for the whole \
         operation"
    );
    assert!(
        seen.iter().any(|p| matches!(
            p.phase,
            TransferPhase::Enumerating | TransferPhase::Counting | TransferPhase::Compressing
        )),
        "expected a real git phase, saw {seen:?}"
    );
    for p in &seen {
        assert!(
            p.percent.is_none_or(|pct| pct <= 100),
            "a published percentage must be one git actually printed: {p:?}"
        );
        if let (Some(done), Some(total)) = (p.objects, p.total_objects) {
            assert!(done <= total, "counts must be sane: {p:?}");
        }
    }
}

/// The paired negative: a fetch with nothing to transfer publishes **no**
/// progress at all.
///
/// Without this leg, the test above would also pass for an implementation that
/// published a fabricated `TransferProgress` unconditionally on entry —
/// a progress bar that always moves is not a progress bar.
/// `refs/remotes/origin/HEAD` is a *symbolic* ref pointing at a branch that is
/// already reported, so counting it reports one branch movement twice.
///
/// This is a real regression, not a hypothetical: git 2.54 writes that symref
/// during `fetch` and git 2.43 does not, so five fetch tests passed on the
/// development box (2.43) and failed in CI (2.54) — the observed result
/// depended on the host's git.
///
/// The symref is planted explicitly here rather than left to whichever git is
/// installed, so this test fails on EVERY git version if the filter in
/// `remote_tracking_refs` is removed. A test that only fails on git ≥ 2.54
/// would be green on the machine most likely to reintroduce the bug.
#[tokio::test(flavor = "multi_thread")]
async fn the_remote_head_symref_is_not_reported_as_a_moved_branch() {
    let (_dir, repo) = repo_with_remote_ahead(1);
    // Point origin/HEAD at origin/main by hand. Git may or may not create this
    // itself depending on version; the assertion must not depend on that.
    run(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let (handle, record) = admit_fetch("head-symref");
    let (status, body) = run_tracked(&repo, record.clone(), fetch_op()).await;
    handle.finish(status, body.clone(), None);
    assert_eq!(status, StatusCode::OK, "{body}");

    let success: FetchSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        !success.updated_refs.is_empty(),
        "the fixture is one commit behind, so a real branch update must be \
         reported — otherwise this test would pass by reporting nothing at all"
    );
    assert!(
        success
            .updated_refs
            .iter()
            .all(|u| u.ref_name != "refs/remotes/origin/HEAD"),
        "origin/HEAD is a symref onto a branch already listed; reporting it \
         double-counts one movement: {:?}",
        success.updated_refs
    );
}

/// The paired negative: a fetch with nothing to transfer publishes **no**
/// progress at all.
///
/// Without this leg, the test above would also pass for an implementation that
/// published a fabricated `TransferProgress` unconditionally on entry —
/// a progress bar that always moves is not a progress bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_with_nothing_to_transfer_publishes_no_progress() {
    let (_dir, repo) = repo_with_remote_ahead(0);
    let (handle, record) = admit_fetch("no-progress");

    let (status, body) = run_tracked(&repo, record.clone(), fetch_op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let success: FetchSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        success.updated_refs.is_empty(),
        "the fixture must have nothing to fetch, or this proves nothing: {:?}",
        success.updated_refs
    );
    assert_eq!(
        record.status().progress,
        None,
        "an up-to-date fetch transfers nothing, so it must report nothing"
    );
    handle.finish(status, body, None);
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// How long the hung remote below blocks for.
///
/// This is **load-bearing, not a tuning knob**: every "the cancel was prompt"
/// assertion in this file is only worth something because it is bounded far
/// below this. See [`PROMPT`].
const HANG: Duration = Duration::from_secs(20);

/// The budget a cancel gets, both for the endpoint to answer and for the child
/// to be gone from `/proc`.
///
/// **Why a budget at all, and why this one.** A cancelled fetch under
/// [`hang_the_next_fetch`] ends up dead either way *eventually* — the hung
/// `upload-pack` exits on its own after [`HANG`], so `child.wait()` returns
/// with or without a kill. A test that only asked "is the process gone
/// afterwards?" with a timeout of [`HANG`]'s own order therefore passes
/// identically for an implementation that never kills anything and merely
/// waits the remote out. That is not a hypothetical: deleting
/// `child.start_kill()` from `git_cmd::git_streamed_for` and re-running this
/// file made all seven tests pass, ~8s slower.
///
/// So the discriminating question is **promptness**, and it is only a fair
/// question if the natural exit is provably later than the budget. The
/// cancellation test establishes that empirically — it dwells `PROMPT`
/// *before* cancelling and asserts the fetch is still running — so a pass
/// requires the child to die within `PROMPT` of the cancel when it had at
/// least `HANG - 2 * PROMPT` (14s) of hanging left to do. Only a kill does
/// that; the real one lands in well under a second.
const PROMPT: Duration = Duration::from_secs(3);

/// A remote whose `upload-pack` sleeps for [`HANG`], so `git fetch` hangs at
/// the point a real transfer would be running — deterministically, with no
/// socket, no port to bind and no timing race.
///
/// `remote.<name>.uploadpack` is a repository-local config key git honours for
/// a path remote (verified against git 2.43.0: the fetch blocks until the
/// sleep ends). The sleep is short enough that a leaked grandchild (the kill
/// reaches the direct child, not the `sh` git started — ADR 0043) dies on its
/// own well inside one test session, and long enough that a cancel that only
/// *waited* could never be mistaken for one that killed.
fn hang_the_next_fetch(repo: &Path) {
    run(
        repo,
        &[
            "config",
            "remote.origin.uploadpack",
            &format!("sh -c 'sleep {}' --", HANG.as_secs()),
        ],
    );
}

/// The mechanism, observed directly: a cancelled [`git_streamed_for`] run
/// leaves a child that was **killed by a signal**, not one that exited.
///
/// This is the assertion the endpoint-level test below cannot make — it only
/// sees a status code and a `/proc` scan — and it is the one that cannot be
/// satisfied by waiting. `WTERMSIG` is set by the kernel when and only when
/// the process was signalled; a child that ran its hung `upload-pack` to the
/// end and exited normally reports `signal() == None` and a real exit code, no
/// matter how patient the runner was.
///
/// The paired negative is the second leg: the *same* helper, on the *same*
/// repository, with no cancel, comes back `cancelled == false` and
/// `signal() == None`. Without it, an implementation that reported
/// `Some(SIGKILL)` unconditionally would pass the first leg.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_stream_leaves_a_signalled_child_not_an_exited_one() {
    use std::os::unix::process::ExitStatusExt;

    let (_dir, repo) = repo_with_remote_ahead(5);
    hang_the_next_fetch(&repo);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let run_repo = repo.clone();
    let cancelled_run = tokio::spawn(async move {
        crate::git_cmd::git_streamed_for(
            &run_repo,
            &["fetch", "--progress", "origin"],
            crate::sandbox::NetworkNeed::Remote,
            Some(rx),
            |_| {},
        )
        .await
    });

    let scan_repo = repo.clone();
    assert!(
        within(HANG, || !live_fetch_processes(&scan_repo).is_empty()).await,
        "no git fetch process appeared for {repo:?} — nothing below would mean \
         anything"
    );
    tx.send(true).unwrap();

    // Deliberately generous — promptness is
    // `cancelling_a_running_fetch_kills_the_child_and_says_nothing_moved`'s
    // job. This test's assertion is about *how* the child ended, and it is
    // worth more for being answerable even when the runner takes its time:
    // a wait-it-out implementation reaches the signal check and fails there,
    // on the mechanism, rather than on a clock.
    let killed = tokio::time::timeout(HANG * 3, cancelled_run)
        .await
        .expect("the cancelled stream must resolve at all")
        .unwrap()
        .expect("the run itself must not error");
    assert!(
        killed.cancelled,
        "the runner must report the cancel it acted on"
    );
    assert_eq!(
        killed.output.status.signal(),
        Some(SIGKILL),
        "the child must have been SIGKILLed. A `signal()` of None means it \
         exited on its own — i.e. the cancel stopped reading and left `git \
         fetch` talking to the remote, which is the exact failure this \
         endpoint exists to prevent. Status was {:?}",
        killed.output.status
    );

    // The paired negative: no cancel, same helper, same repository — an
    // ordinary fetch is neither `cancelled` nor signalled.
    run(&repo, &["config", "--unset", "remote.origin.uploadpack"]);
    let ordinary = crate::git_cmd::git_streamed_for(
        &repo,
        &["fetch", "--progress", "origin"],
        crate::sandbox::NetworkNeed::Remote,
        None,
        |_| {},
    )
    .await
    .expect("an ordinary fetch must run");
    assert!(
        !ordinary.cancelled,
        "a run nobody cancelled must not report a cancel"
    );
    assert_eq!(
        ordinary.output.status.signal(),
        None,
        "an ordinary fetch's child exits; if this also reports a signal, the \
         assertion above is reading something other than the kill"
    );
    assert!(
        ordinary.output.status.success(),
        "the paired-negative fetch must actually have worked: {:?}",
        String::from_utf8_lossy(&ordinary.output.stderr)
    );
}

/// `SIGKILL`, spelled out rather than pulled from a dependency this crate does
/// not otherwise need. Fixed at 9 on every Linux ABI.
const SIGKILL: i32 = 9;

/// **The load-bearing cancellation test**: a running fetch is cancelled
/// through the real endpoint, and the child process is *gone promptly*
/// afterwards.
///
/// Four legs, in the order that makes each one mean something:
///
/// 1. While the fetch runs, `/proc` shows a matching process. Without it, "no
///    process afterwards" would also be true of a scan that can never find
///    anything, and the test would pass over a cancel that did nothing at all.
/// 2. **The hang outlives the budget.** The test dwells [`PROMPT`] without
///    cancelling and asserts the fetch is *still* running and the driver has
///    *not* answered. This is what makes leg 4 discriminating: it establishes
///    on this run, not by construction, that the child was not about to exit
///    on its own.
/// 3. `POST /api/operations/{id}/cancel` answers `202`.
/// 4. Within [`PROMPT`] the driver answers **and** the process is gone — with
///    at least `HANG - 2 * PROMPT` of hanging still owed. The terminal record
///    is a `409` carrying `FetchFailureKind::Cancelled` with **no** refs
///    moved: the fetch was still inside `upload-pack`, so nothing local had
///    changed, and the repository is checked directly to confirm it.
///
/// Leg 2 and the tight bound in leg 4 are the fix for a real hole: with the
/// old 20s timeouts against a 10s hang, removing `child.start_kill()` from
/// `git_cmd::git_streamed_for` left every assertion here passing.
/// [`a_cancelled_stream_leaves_a_signalled_child_not_an_exited_one`] proves
/// the same property a second way, by the child's termination signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_running_fetch_kills_the_child_and_says_nothing_moved() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    hang_the_next_fetch(&repo);
    let before_tip = std::fs::read_to_string(repo.join(".git/refs/remotes/origin/main")).ok();

    let (handle, record) = admit_fetch("cancel-kills");
    let id = record.id();

    let driver = {
        let repo = repo.clone();
        let record = record.clone();
        tokio::spawn(async move { run_tracked(&repo, record, fetch_op()).await })
    };

    let scan_repo = repo.clone();
    assert!(
        within(HANG, || !live_fetch_processes(&scan_repo).is_empty()).await,
        "no git fetch process appeared for {repo:?} — the fixture never got \
         as far as spawning one, so nothing below would mean anything"
    );

    // Leg 2: the fixture really does hang past the budget the cancel is held
    // to, so "gone within PROMPT of the cancel" cannot be satisfied by the
    // child simply reaching the end of its sleep.
    tokio::time::sleep(PROMPT).await;
    assert!(
        !live_fetch_processes(&repo).is_empty(),
        "the hung fetch exited on its own inside the promptness budget — the \
         budget below would then prove nothing about killing. Raise HANG."
    );
    assert!(
        !driver.is_finished(),
        "the uncancelled fetch already answered inside the promptness budget; \
         the assertions below could not distinguish a kill from a wait"
    );

    let response =
        crate::handlers::operations::cancel_operation(axum::extract::Path(id.as_str().to_string()))
            .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a running, cancellable operation must accept the cancel"
    );

    let (status, body) = tokio::time::timeout(PROMPT, driver)
        .await
        .expect(
            "the cancelled fetch must return within the promptness budget. \
             Timing out here with the hung remote still owed most of its \
             sleep is what a cancel that merely stops waiting looks like",
        )
        .unwrap();
    handle.finish(status, body.clone(), None);

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error: FetchError = serde_json::from_str(&body).expect("a cancelled fetch is a FetchError");
    assert_eq!(error.kind, FetchFailureKind::Cancelled);
    assert!(
        error.updated_refs.is_empty(),
        "the fetch was cancelled inside upload-pack, so no ref can have moved: {:?}",
        error.updated_refs
    );
    assert!(
        error
            .message
            .contains("before any remote-tracking ref was updated"),
        "the terminal message must say plainly which case this was: {}",
        error.message
    );

    assert!(
        within(PROMPT, || live_fetch_processes(&repo).is_empty()).await,
        "the git fetch child survived the cancel — a cancel that only stops \
         waiting leaves git running against the remote, which is the exact \
         failure this endpoint exists to prevent. Still alive: {:?}",
        live_fetch_processes(&repo)
    );

    assert_eq!(
        std::fs::read_to_string(repo.join(".git/refs/remotes/origin/main")).ok(),
        before_tip,
        "the repository itself must agree with the reported empty ref diff"
    );
}

/// A cancel that arrives *before* the executor spawns anything must stop the
/// fetch from starting at all — the latch is read once more immediately before
/// the spawn, so an operation cancelled while queued behind the repository
/// guard does not then reach out to a remote anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_that_lands_before_execution_stops_the_fetch_starting() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    let (handle, record) = admit_fetch("cancel-early");

    assert!(
        record.request_cancel(),
        "a live record must accept a cancel"
    );
    let (status, body) = run_tracked(&repo, record.clone(), fetch_op()).await;
    handle.finish(status, body.clone(), None);

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error: FetchError = serde_json::from_str(&body).unwrap();
    assert_eq!(error.kind, FetchFailureKind::Cancelled);
    assert!(error.updated_refs.is_empty());
    assert!(
        !repo.join(".git/refs/remotes/origin/main").exists()
            || std::fs::read_to_string(repo.join(".git/FETCH_HEAD"))
                .map(|s| s.trim().is_empty())
                .unwrap_or(true),
        "a fetch cancelled before it started must not have contacted the remote"
    );
}

/// The cancel endpoint refuses rather than pretending, in each of its three
/// refusal cases — the property that makes a `202` from it worth anything.
#[tokio::test]
async fn the_cancel_endpoint_refuses_rather_than_pretending() {
    // 1. An id this server never minted.
    let unknown = crate::handlers::operations::cancel_operation(axum::extract::Path(
        "0123456789abcdef".to_string(),
    ))
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // 2. An operation whose executor has no cancellation point. `StageAll` is
    //    `honours_cancellation == false`, and answering `202` for it would
    //    tell an operator a cancel is under way that nothing will ever act on.
    let k = key("cancel-uncancellable");
    let op = GitOperation::StageAll;
    let hash = operation_hash(&op);
    let (repository, worktree) = tokens();
    let Admission::Fresh(stage_handle, stage_record) =
        crate::operations::admit(&k, &op, &hash, repository, worktree, None)
    else {
        panic!("a fresh key must be admitted");
    };
    let refused = crate::handlers::operations::cancel_operation(axum::extract::Path(
        stage_record.id().as_str().to_string(),
    ))
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::CONFLICT,
        "an operation with no cancellation point must be refused, not accepted"
    );

    // 3. An operation that has already finished.
    stage_handle.finish(StatusCode::OK, "done".into(), None);
    let (fetch_handle, fetch_record) = admit_fetch("cancel-finished");
    fetch_handle.finish(StatusCode::OK, "done".into(), None);
    let too_late = crate::handlers::operations::cancel_operation(axum::extract::Path(
        fetch_record.id().as_str().to_string(),
    ))
    .await;
    assert_eq!(
        too_late.status(),
        StatusCode::CONFLICT,
        "cancelling a finished operation must not report success"
    );

    // The paired positive: the same endpoint *does* accept a live, cancellable
    // operation — twice, because a client whose response was lost must be able
    // to retry.
    let (live_handle, live_record) = admit_fetch("cancel-live");
    for _ in 0..2 {
        let accepted = crate::handlers::operations::cancel_operation(axum::extract::Path(
            live_record.id().as_str().to_string(),
        ))
        .await;
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    }
    live_handle.finish(StatusCode::CONFLICT, "cancelled".into(), None);
}

// ---------------------------------------------------------------------------
// Dropped-connection replay (the issue's named acceptance criterion)
// ---------------------------------------------------------------------------

/// A client that loses the connection before its `POST /api/fetch` response
/// arrives, and re-sends under the same idempotency key, gets the **recorded
/// result replayed** — `git fetch` does not run a second time.
///
/// The proof that it did not re-run is the body itself, and it is decisive:
/// the first fetch reports `refs/remotes/origin/main` moving. A *second real*
/// fetch of the same remote would necessarily report `updated_refs: []`,
/// because by then there is nothing left to fetch. So a replayed response
/// carrying the ref update, byte-identical to the first, cannot have been
/// produced by running git again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_connection_replays_instead_of_fetching_twice() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    let k = key("dropped-connection");

    // The lost request: `plan_and_execute_tracked` admits the operation and
    // spawns the pipeline *detached* before its first await, so dropping this
    // future (what axum does when the client disconnects) cancels only the
    // waiting. One millisecond is far shorter than any fetch.
    let dropped = tokio::time::timeout(
        Duration::from_millis(1),
        super::plan_and_execute_tracked(
            k.clone(),
            repo.clone(),
            None,
            tokens(),
            PlanSource::Build(fetch_op()),
        ),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the fixture must actually drop the request before it answered, or \
         this is not testing a dropped connection at all"
    );

    // The retry, same key.
    let (status, replayed) = tokio::time::timeout(
        Duration::from_secs(30),
        super::plan_and_execute_tracked(
            k.clone(),
            repo.clone(),
            None,
            tokens(),
            PlanSource::Build(fetch_op()),
        ),
    )
    .await
    .expect("the retry must resolve once the detached pipeline finishes");
    assert_eq!(status, StatusCode::OK, "{replayed}");

    let success: FetchSuccess = serde_json::from_str(&replayed).unwrap();
    assert_eq!(
        success.updated_refs.len(),
        1,
        "the replayed body must be the original fetch's result, which moved a \
         ref; a second real `git fetch` would have found nothing to do and \
         reported no updates: {replayed}"
    );
    assert_eq!(success.updated_refs[0].ref_name, "refs/remotes/origin/main");

    // And a third request under the same key is byte-identical again.
    let (again_status, again) = super::plan_and_execute_tracked(
        k,
        repo.clone(),
        None,
        tokens(),
        PlanSource::Build(fetch_op()),
    )
    .await;
    assert_eq!(again_status, status);
    assert_eq!(again, replayed, "replay must be verbatim, every time");
}

// ---------------------------------------------------------------------------
// Redaction on the live path
// ---------------------------------------------------------------------------

/// A remote that leaks a credential-bearing URL on its own stderr — the
/// realistic shape of the leak ADR 0036 documents, since git forwards the
/// remote side's stderr verbatim.
///
/// (git itself strips userinfo from the URLs it prints; the hole is what
/// *other programs in the pipeline* print, which is precisely why #228's
/// redaction operates on the captured bytes rather than trusting git.)
fn leak_a_credential_on_the_next_fetch(repo: &Path, secret_url: &str) {
    run(
        repo,
        &[
            "config",
            "remote.origin.uploadpack",
            &format!("sh -c 'echo tried {secret_url} >&2; exit 3' --"),
        ],
    );
}

/// A secret the remote printed never reaches the operation record.
///
/// The premise is **asserted, not assumed**: the same fixture is first run
/// through plain `git fetch`, outside this server's harness, and the raw
/// stderr is checked to contain the literal secret. Without that leg, a
/// fixture that never leaked anything would make the redaction assertion pass
/// over nothing — the failure mode `network_exec`'s own
/// `unredacted_text_still_contains_the_literal_secret` exists to rule out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_leaked_by_the_remote_never_reaches_the_operation_record() {
    const SECRET: &str = "hunter2-fetch-suite";
    let secret_url = format!("https://svcuser:{SECRET}@leaked-host.invalid/org/repo.git");

    let (_dir, repo) = repo_with_remote_ahead(5);
    leak_a_credential_on_the_next_fetch(&repo, &secret_url);

    // Premise: unredacted, this fixture really does leak.
    let raw = std::process::Command::new("git")
        .args(["fetch", "--progress", "origin"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let raw_stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
    assert!(
        raw_stderr.contains(SECRET),
        "the fixture must actually leak the secret when unredacted, or the \
         assertion below proves nothing. Got: {raw_stderr}"
    );

    // Through the server's own path.
    let (handle, record) = admit_fetch("redaction");
    let (status, body) = run_tracked(&repo, record.clone(), fetch_op()).await;
    handle.finish(status, body.clone(), None);

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error: FetchError = serde_json::from_str(&body).unwrap();
    assert!(
        !error.message.contains(SECRET),
        "the operation record carries a credential the remote leaked: {}",
        error.message
    );
    assert!(
        error.message.contains("leaked-host.invalid"),
        "redaction must strip the userinfo and keep the rest — a message with \
         the host removed too would be useless: {}",
        error.message
    );
    assert!(
        !serde_json::to_string(&record.status())
            .unwrap()
            .contains(SECRET),
        "the whole recorded status must be free of the secret, not just the \
         field this test happened to look at"
    );
}

// ---------------------------------------------------------------------------
// The journal — what the activity feed is told a fetch did
// ---------------------------------------------------------------------------

/// Every `ActivityKind::Fetch` entry this repository's journal holds.
///
/// Read back off disk through `journal::read_all`, i.e. through the same
/// parser `/api/activity` uses — not by inspecting whatever `journal_updates`
/// happened to be handed. A round trip through the JSONL is the only way this
/// proves the feed would actually see the entry.
fn journaled_fetches(repo: &Path) -> Vec<ActivityEvent> {
    crate::journal::read_all(repo)
        .into_iter()
        .filter(|e| e.kind == ActivityKind::Fetch)
        .collect()
}

/// A fetch that moves a remote-tracking ref journals **one `Fetch` entry per
/// moved ref**, carrying the observed before/after oids — so the activity feed
/// can say "origin/main moved from X to Y" instead of "a fetch happened".
///
/// This closes a real gap: `journal_updates` shipped with no coverage at all,
/// and commenting out its call on the success path left all 23 fetch-touching
/// tests in the workspace green. ADR 0043 listed it as unit-tested; it was not.
///
/// The oids are checked against `git rev-parse` rather than against the
/// response body — asserting the journal matches the response would only prove
/// the two came from the same variable. The repository is the referee.
///
/// The paired negative is the test below: an up-to-date fetch journals nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_that_moves_a_ref_journals_it_per_ref() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    assert!(
        journaled_fetches(&repo).is_empty(),
        "the fixture must start with an empty journal, or the count below \
         measures the fixture rather than the fetch"
    );

    let (handle, record) = admit_fetch("journal-moved");
    let (status, body) = run_tracked(&repo, record, fetch_op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    handle.finish(status, body.clone(), None);

    // What the repository itself says the ref is now — the referee.
    let tip = std::process::Command::new("git")
        .args(["rev-parse", "refs/remotes/origin/main"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(tip.status.success(), "the fetch must have created the ref");
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_string();

    let entries = journaled_fetches(&repo);
    assert_eq!(
        entries.len(),
        1,
        "one moved ref must journal exactly one Fetch entry, not zero and not \
         a summary plus a per-ref one: {entries:?}"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.ref_name.as_deref(),
        Some("refs/remotes/origin/main"),
        "the entry must name the ref that moved: {entry:?}"
    );
    assert_eq!(
        entry.new_oid.as_deref(),
        Some(tip.as_str()),
        "the journaled new tip must be the oid git now reports: {entry:?}"
    );
    assert_eq!(
        entry.old_oid, None,
        "the fixture deletes the tracking ref before fetching, so the \
         observed previous tip is Absent — a journal claiming a previous oid \
         would be inventing one: {entry:?}"
    );
    assert_eq!(
        entry.source,
        ActivitySource::App,
        "a fetch this server ran must be attributed to the app: {entry:?}"
    );
    assert!(
        entry.summary.contains("origin/main"),
        "the summary must name the ref that moved: {}",
        entry.summary
    );
    assert!(
        !entry.summary.contains("unknown"),
        "an observed fetch must not be journaled as unknown: {}",
        entry.summary
    );
}

/// The paired negative: an already-up-to-date fetch journals **nothing**.
///
/// Without this leg, `a_fetch_that_moves_a_ref_journals_it_per_ref` would pass
/// for an implementation that journaled an entry on entry to `exec_fetch`
/// regardless of what moved — a feed in which every fetch looks like a change
/// is exactly as uninformative as one in which none do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_up_to_date_fetch_journals_nothing() {
    let (_dir, repo) = repo_with_remote_ahead(0);
    let (handle, record) = admit_fetch("journal-noop");
    let (status, body) = run_tracked(&repo, record, fetch_op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let success: FetchSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        success.updated_refs.is_empty(),
        "the fixture must have nothing to fetch, or this proves nothing: {:?}",
        success.updated_refs
    );
    handle.finish(status, body, None);

    assert!(
        journaled_fetches(&repo).is_empty(),
        "a fetch that moved nothing must leave no trace in the feed: {:?}",
        journaled_fetches(&repo)
    );
}

/// Break the served repository's ref store *during* the fetch, at the moment
/// git commits the ref update.
///
/// `reference-transaction` fires with `committed` once the new
/// `refs/remotes/origin/main` is durable, so the ref genuinely moves and only
/// then does the ref store become unreadable. That is exactly the state
/// `exec_fetch`'s post-fetch re-read has to cope with: the repository changed,
/// and no one can say how.
///
/// A malformed `packed-refs` is the lever because `git for-each-ref` treats it
/// as fatal (`exit 128`, verified against git 2.43.0) while the already-written
/// loose ref stays on disk.
fn blind_the_repository_after_the_fetch(repo: &Path) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = committed ]; then\n\
             printf 'not a packed-refs file\\n' > '{}/.git/packed-refs'\n\
             fi\n\
             exit 0\n",
            repo.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fetch that ran, moved a ref, and then could not be re-read still leaves a
/// journal entry — one that **admits** the outcome is unknown.
///
/// This is the one exit path from `exec_fetch` that has no ref diff to
/// describe, and before this it returned silently: the repository had changed
/// and the activity feed said nothing at all, which is the divergence between
/// what happened and what was recorded that the rest of the module observes
/// refs to avoid.
///
/// The premise is asserted three ways rather than assumed, because a fixture
/// that merely failed early would make the whole thing vacuous:
///
/// * the response is the specific "the fetch ran but … could not be re-read"
///   refusal, so this really is the re-read path and not, say, a spawn
///   failure before git ever started;
/// * the loose `refs/remotes/origin/main` is on disk, so a ref really did move
///   while unobservable — the divergence is real, not hypothetical;
/// * the entry's oids are `None` *and* the summary says so, distinguishing
///   "there was no such tip" (`Obs::Absent`) from "git could not be read"
///   (`Obs::Unknown`), which is the whole point of D5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_whose_outcome_cannot_be_observed_is_journaled_as_unknown() {
    let (_dir, repo) = repo_with_remote_ahead(5);
    blind_the_repository_after_the_fetch(&repo);

    let (handle, record) = admit_fetch("journal-blinded");
    let (status, body) = run_tracked(&repo, record, fetch_op()).await;
    handle.finish(status, body.clone(), None);

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the fixture must reach the post-fetch re-read failure: {body}"
    );
    assert!(
        body.contains("could not be re-read"),
        "…and it must be *that* refusal, not some earlier one: {body}"
    );
    assert!(
        repo.join(".git/refs/remotes/origin/main").exists(),
        "the fixture must let the ref actually move before blinding the repo, \
         or there is no divergence for the journal to record"
    );

    let entries = journaled_fetches(&repo);
    assert_eq!(
        entries.len(),
        1,
        "an unobservable fetch must still leave exactly one entry: {entries:?}"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.ref_name, None,
        "which ref moved is precisely what is unknown; naming one would be \
         fabrication: {entry:?}"
    );
    assert_eq!(entry.old_oid, None, "{entry:?}");
    assert_eq!(entry.new_oid, None, "{entry:?}");
    assert!(
        entry.summary.contains("unknown"),
        "the summary must admit the outcome is unknown rather than leave the \
         empty oids to be read as ‘nothing moved’: {}",
        entry.summary
    );
    assert_eq!(entry.source, ActivitySource::App, "{entry:?}");
    assert_eq!(
        entry.undo, None,
        "an outcome nobody observed offers no undo: {entry:?}"
    );
}
