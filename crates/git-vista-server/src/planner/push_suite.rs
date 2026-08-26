//! M2.20e (#231): the push slice's *behavioural* tests — the ones that spawn a
//! real `git push` against a real remote, because the properties this slice
//! claims cannot be proved any other way.
//!
//! `planner::push`'s own inline tests cover the pure functions (the argv
//! builder over the whole `ForcePublish` space, the failure classifier, the
//! replaced-tip reader) with paired negatives. This file covers the claims a
//! pure test cannot reach, and the ordering below is the order of how much they
//! matter:
//!
//! * **the lease is a real compare-and-swap, refused in both of its two
//!   distinct ways** — by *this server*, before anything is spawned, when the
//!   reviewed tip does not match the local remote-tracking ref; and by *git*,
//!   against the remote's own advertisement, when someone else pushed in
//!   between. Both are asserted against the **remote's ref listing**, never a
//!   status code: the property is "the other party's commits are still there",
//!   and only the remote can answer that;
//! * **a correct lease really does force-publish**, with the anti-vacuity leg
//!   that makes it mean something — the same fixture is first pushed *without*
//!   a force and refused, so the lease test is not passing over a
//!   fast-forward that never needed one;
//! * **`--set-upstream` actually records an upstream**, read back out of git's
//!   own config, with the paired negative that a push which did not ask for one
//!   does not get one;
//! * **push publishes its own transfer progress**, including the `Writing`
//!   phase a fetch can never produce, with the paired negative that an
//!   up-to-date push publishes nothing;
//! * **cancellation kills the child**, observed in `/proc` and bounded by a
//!   promptness budget, with the remote's ref proving nothing landed — and its
//!   other half, the cancel that lands *after* the remote-tracking ref has
//!   already advanced, which must say that much was accepted rather than
//!   reassure. Those are the two arms of `cancelled_response`, and each test
//!   asserts the other arm's wording is absent.
//!
//! # The fixture shape, and why there are two of them
//!
//! **Pushes that must reach a remote go over `git://` on loopback.** Under the
//! sandbox (#66 Task 6) a filesystem-path remote is dead twice over: a path
//! outside the grant is denied outright, and even a *granted* path fails,
//! because `receive-pack`'s quarantine migration is a cross-directory rename and
//! the shim deliberately withholds `LANDLOCK_ACCESS_FS_REFER`. That is the
//! intended posture — production remotes are URLs, where receive-pack runs on
//! the far side, outside the pusher's sandbox — and it is why
//! `contract_suite::push_branch_executes_through_the_pipeline` already serves
//! its remote with `git daemon`. [`Fixture`] does the same, taking
//! `test_ports::PortClaim` because 9418 is the only unprivileged port a
//! Network-tier Landlock connect grant covers and three unrelated tests in this
//! binary contend for it.
//!
//! **Pushes that must be refused *before* anything is spawned use a path
//! remote**, and that is not a shortcut — it is the proof. Such a remote also
//! carries a `pre-receive` hook that writes a sentinel file, so "no push reached
//! the remote" is an observed absence rather than an inference from a status
//! code. And because the refusal is supposed to happen before any spawn, the
//! fact that this remote *could not* have been pushed to successfully takes
//! nothing away: if the code under test ever did spawn, the sentinel would
//! appear and the test would fail on the mechanism.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};
use git_vista_protocol::{
    BranchName, CommitOid, ForcePublish, GitOperation, IdempotencyKey, RemoteName, RepositoryToken,
    TransferPhase, WorktreeToken,
};

use super::operation_hash;
use crate::operations::{Admission, Record};
use crate::test_ports::PortClaim;

// ---------------------------------------------------------------------------
// Shared helpers
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

/// `git <args…>` in `dir`, trimmed stdout; asserts success.
fn out(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {dir:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `git config --get <key>`, or `None` when git exits non-zero (the key is
/// unset). Not `out`, because "unset" is the answer half these tests want.
fn config(repo: &Path, key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", key])
        .current_dir(repo)
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("push-suite-repo").unwrap(),
        WorktreeToken::new("push-suite-worktree").unwrap(),
    )
}

/// Keys are process-global registry state shared with every other test in this
/// binary, so each test mints its own.
fn key(name: &str) -> IdempotencyKey {
    IdempotencyKey::new(format!("push-suite-{name}")).unwrap()
}

fn push_op(set_upstream: bool, force: ForcePublish) -> GitOperation {
    GitOperation::PushBranch {
        branch: BranchName::new("main").unwrap(),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream,
        force,
    }
}

fn lease(oid: &str) -> ForcePublish {
    ForcePublish::WithLease {
        expected_remote_tip: CommitOid::new(oid).unwrap(),
    }
}

/// Drive the real pipeline (`build_plan → validate → enforce_fresh → execute`)
/// against `repo`, exactly as `plan_and_execute` would for a live request.
async fn pipeline(repo: &Path, op: GitOperation) -> (StatusCode, String) {
    super::plan_and_execute_in(repo, None, tokens(), op, crate::planner::DropProof::Nothing).await
}

/// Admit one push operation into the registry and return what the caller needs
/// to drive it the way `plan_and_execute_tracked` would.
fn admit_push(
    name: &str,
    op: &GitOperation,
) -> (crate::operations::OperationHandle, std::sync::Arc<Record>) {
    let hash = operation_hash(op);
    let (repository, worktree) = tokens();
    match crate::operations::admit(&key(name), op, &hash, repository, worktree, None) {
        Admission::Fresh(handle, record) => (handle, record),
        _ => panic!("a fresh key must be admitted"),
    }
}

/// Run the guarded pipeline under `record`, so the executor sees the
/// operation's progress sink and cancellation latch — the same scope
/// `plan_and_execute_tracked`'s detached task establishes in production.
async fn run_tracked(
    repo: &Path,
    record: std::sync::Arc<Record>,
    op: GitOperation,
) -> (StatusCode, String) {
    let repo = repo.to_path_buf();
    crate::operations::with_progress(record, async move { pipeline(&repo, op).await }).await
}

/// Every `ActivityKind::Push` entry this repository's journal holds, read back
/// off disk through the same parser `/api/activity` uses — not by inspecting
/// whatever the executor happened to hand the journal.
fn journaled_pushes(repo: &Path) -> Vec<ActivityEvent> {
    crate::journal::read_all(repo)
        .into_iter()
        .filter(|e| e.kind == ActivityKind::Push)
        .collect()
}

/// Poll `f` until it is true or `limit` elapses. Bounded so a broken
/// expectation fails the test in seconds rather than hanging the suite.
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
// Fixture A: a real `git://` remote
// ---------------------------------------------------------------------------

/// Kills the fixture `git daemon` even when an assertion panics first — a
/// leaked daemon would squat 9418 and poison every later run.
///
/// It must kill the **process group**, not the child: `/usr/bin/git` forks
/// `git-daemon` and exits, so the `Child` held here is only the short-lived
/// wrapper. Same shape as `contract_suite::DaemonGuard`, and duplicated rather
/// than shared because that one is private to a `#[cfg(test)]` sibling and a
/// `pub(super)` on it would export a test detail into the planner's namespace.
struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

/// A served repository, a bare remote, and a `git daemon` serving it over
/// `git://127.0.0.1:9418`.
///
/// Field order is drop order, and it is load-bearing: `_daemon` must die before
/// `_claim` is released (a claim released while the daemon still holds the port
/// hands the next claimant an occupied port), and `_dir` must outlive both.
struct Fixture {
    repo: PathBuf,
    remote: PathBuf,
    _daemon: DaemonGuard,
    _claim: PortClaim,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// The remote's `refs/heads/*` listing — **the referee** for every "did the
    /// remote move?" assertion in this file. A status code cannot answer that
    /// question and neither can this repository's own remote-tracking ref,
    /// which is a local cache.
    fn remote_heads(&self) -> String {
        out(&self.remote, &["for-each-ref", "refs/heads"])
    }

    fn remote_tip(&self) -> String {
        out(&self.remote, &["rev-parse", "main"])
    }

    fn local_tip(&self) -> String {
        out(&self.repo, &["rev-parse", "main"])
    }

    fn tracking_tip(&self) -> String {
        out(&self.repo, &["rev-parse", "refs/remotes/origin/main"])
    }
}

/// A repository whose `origin` is a `git daemon`-served bare repo, seeded with
/// one commit already on the remote and `unpushed` content commits ahead of it.
///
/// The seed push is run with plain, unsandboxed git (as all fixture setup here
/// is), which also creates `refs/remotes/origin/main` — so every test starts
/// from the realistic state of a branch that has been pushed before.
///
/// The extra commits carry ~200 KiB of incompressible content each, and that is
/// not decoration: measured against git 2.43.0, pushing a handful of *empty*
/// commits prints no `Writing objects:` progress at all, so a progress test
/// built on the smaller fixture would prove nothing.
fn fixture(unpushed: usize) -> Fixture {
    // Claimed before the daemon is spawned and released after `DaemonGuard`'s
    // `Drop` has killed it — see `Fixture`'s field-order note.
    let claim = PortClaim::acquire();
    let port = PortClaim::PORT;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "seed\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);

    let daemon = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("git")
            .args([
                "daemon",
                "--reuseaddr",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                "--export-all",
                "--enable=receive-pack",
                &format!("--base-path={}", dir.path().display()),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("git daemon spawns")
    };
    let daemon = DaemonGuard(daemon);
    let ready = (0..50).any(|_| {
        std::net::TcpStream::connect(("127.0.0.1", port))
            .map(|_| true)
            .unwrap_or_else(|_| {
                std::thread::sleep(Duration::from_millis(100));
                false
            })
    });
    assert!(ready, "git daemon never came up on 127.0.0.1:{port}");

    run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            &format!("git://127.0.0.1:{port}/remote.git"),
        ],
    );
    run(&repo, &["push", "-q", "origin", "main"]);
    assert_eq!(
        config(&repo, "branch.main.merge"),
        None,
        "the seed push must not set an upstream, or the --set-upstream tests \
         measure the fixture instead of the push"
    );

    for n in 0..unpushed {
        let name = format!("f{n}.txt");
        std::fs::write(repo.join(&name), incompressible(n)).unwrap();
        run(&repo, &["add", &name]);
        run(&repo, &["commit", "-q", "-m", &format!("c{n}")]);
    }

    // The journal must start empty, or every count below measures the fixture.
    assert!(
        journaled_pushes(&repo).is_empty(),
        "fixture setup journaled something"
    );

    Fixture {
        repo,
        remote,
        _daemon: daemon,
        _claim: claim,
        _dir: dir,
    }
}

/// ~200 KiB that does not deltify or zlib away, so `git push --progress` has
/// something to report writing. Deterministic (a cheap LCG), so a failing run
/// reproduces.
fn incompressible(seed: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_u32.wrapping_mul(seed as u32 + 1) | 1;
    (0..200_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixture B: a path remote that records whether anything ever reached it
// ---------------------------------------------------------------------------

/// A repository whose `origin` is a bare repo **inside its own tree** (so the
/// sandbox grant covers it), carrying a `pre-receive` hook that writes a
/// sentinel file the moment `receive-pack` starts processing an update.
///
/// The sentinel is the point. Every test using this fixture asserts a refusal
/// that is supposed to happen *before any git is spawned*, and "no push reached
/// the remote" as an observed absence is a much stronger claim than "the status
/// code was 409". If the executor ever did spawn, git would connect, the hook
/// would run, and the file would appear.
struct SentinelFixture {
    repo: PathBuf,
    remote: PathBuf,
    sentinel: PathBuf,
    _dir: tempfile::TempDir,
}

impl SentinelFixture {
    fn reached_the_remote(&self) -> bool {
        self.sentinel.exists()
    }

    fn remote_heads(&self) -> String {
        out(&self.remote, &["for-each-ref", "refs/heads"])
    }
}

fn sentinel_fixture() -> SentinelFixture {
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
    // Seeded with plain git *before* the hook is installed, so the tracking ref
    // exists and the sentinel still records only what the code under test did.
    run(&repo, &["push", "-q", "origin", "main"]);

    // **Inside the bare remote, which is inside the served repository's tree**,
    // and that placement is load-bearing rather than tidy. The hook runs as a
    // grandchild of the sandboxed `git push`, so it inherits the Landlock
    // ruleset: a sentinel in the tempdir root is outside the grant and the write
    // fails with EACCES — the file never appears *even when the hook ran*, and
    // every assertion resting on its absence passes over nothing. That is
    // exactly what happened on the first run of this suite, and it is why
    // `a_lease_that_matches_the_tracking_ref_is_let_through_the_pre_flight`
    // exists: it is the paired positive that requires the sentinel to actually
    // be writable.
    let sentinel = remote.join("reached-the-remote");
    let hooks = remote.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf reached > '{}'\nexit 0\n",
            sentinel.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !sentinel.exists(),
        "the sentinel must not exist before the test runs"
    );
    SentinelFixture {
        repo,
        remote,
        sentinel,
        _dir: dir,
    }
}

/// Rewrite the local branch so it is **not** a fast-forward of the remote: the
/// seed commit is amended, giving a new tip with the same parent-less shape and
/// a different oid. Returns the new local tip.
fn diverge_locally(repo: &Path) -> String {
    run(repo, &["commit", "-q", "--amend", "-m", "rewritten seed"]);
    out(repo, &["rev-parse", "main"])
}

// ---------------------------------------------------------------------------
// The ordinary push
// ---------------------------------------------------------------------------

/// A fast-forward push reaches the remote, and the journal records **the ref
/// that moved**, with the mode that moved it.
///
/// The remote's own `rev-parse` is the referee, not the response body: a
/// response that said "pushed" while nothing crossed the wire is precisely the
/// failure a status-only assertion cannot see.
#[tokio::test]
async fn a_fast_forward_push_reaches_the_remote_and_journals_the_mode() {
    let fx = fixture(2);
    let was = fx.remote_tip();
    let want = fx.local_tip();
    assert_ne!(was, want, "the fixture must have something to push");

    let (status, body) = pipeline(&fx.repo, push_op(false, ForcePublish::None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fx.remote_tip(),
        want,
        "the remote must actually be at the local tip"
    );
    assert_eq!(
        fx.tracking_tip(),
        want,
        "git must have advanced the remote-tracking ref, which is what the \
         executor observes"
    );

    let entries = journaled_pushes(&fx.repo);
    assert_eq!(
        entries.len(),
        1,
        "one moved ref must journal exactly one Push entry: {entries:?}"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.ref_name.as_deref(),
        Some("refs/remotes/origin/main"),
        "{entry:?}"
    );
    assert_eq!(entry.old_oid.as_deref(), Some(was.as_str()), "{entry:?}");
    assert_eq!(entry.new_oid.as_deref(), Some(want.as_str()), "{entry:?}");
    assert_eq!(entry.source, ActivitySource::App, "{entry:?}");
    assert!(
        entry.summary.starts_with("pushed "),
        "an ordinary push must be journaled as a push: {}",
        entry.summary
    );
    assert!(
        !entry.summary.contains("force-published") && !entry.summary.contains("--set-upstream"),
        "the summary must not claim a mode that did not run: {}",
        entry.summary
    );
    // The paired negative for
    // `a_cancel_that_lands_after_the_ref_moved_reports_what_the_remote_accepted`:
    // a push nobody cancelled must not carry the cancelled tail, or that test
    // would pass for an implementation that appended it unconditionally.
    assert!(
        !entry.summary.contains("cancelled"),
        "a push that ran to completion must not be journaled as cancelled: {}",
        entry.summary
    );
}

/// **The paired negative** for both the journal and the progress stream: a push
/// with nothing to send moves nothing, journals nothing, and publishes no
/// transfer progress.
///
/// Without this leg, `a_fast_forward_push_reaches_the_remote_and_journals_the_mode`
/// would pass for an implementation that journaled an entry on entry to the
/// executor, and
/// `a_push_publishes_transfer_progress_including_the_writing_phase` would pass
/// for one that published a fabricated `TransferProgress` unconditionally. A
/// feed in which every push looks like a change is exactly as uninformative as
/// one in which none do; so is a progress bar that always moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_up_to_date_push_moves_nothing_and_journals_nothing() {
    let fx = fixture(0);
    let was = fx.remote_tip();
    let op = push_op(false, ForcePublish::None);
    let (handle, record) = admit_push("uptodate", &op);

    let (status, body) = run_tracked(&fx.repo, record.clone(), op).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("Already up to date"),
        "a push with nothing to send must say so: {body}"
    );
    handle.finish(status, body, None);

    assert_eq!(fx.remote_tip(), was, "nothing may have moved");
    assert!(
        journaled_pushes(&fx.repo).is_empty(),
        "a push that moved nothing must leave no trace in the feed: {:?}",
        journaled_pushes(&fx.repo)
    );
    assert_eq!(
        record.status().progress,
        None,
        "an up-to-date push transfers nothing, so it must report nothing"
    );
}

/// A real push publishes real transfer progress on its own operation record,
/// **including the `Writing` phase**.
///
/// `Writing` is the load-bearing part. It is the one phase a fetch can never
/// produce, so before #231 widened the shared parser a pushing user's progress
/// stopped at `Compressing` and the whole transfer — the part that takes the
/// time — reported nothing. Asserting only "some progress appeared" would have
/// passed over that.
///
/// The subscriber collects *every* published snapshot rather than reading the
/// final one: `watch` coalesces, and a test that looked only at the end state
/// would be satisfied by a single write at the finish line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_publishes_transfer_progress_including_the_writing_phase() {
    let fx = fixture(4);
    let op = push_op(false, ForcePublish::None);
    let (handle, record) = admit_push("progress", &op);

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

    let (status, body) = run_tracked(&fx.repo, record.clone(), op).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    handle.finish(status, body, None);

    let seen = collector.await.unwrap();
    assert!(
        seen.iter().any(|p| p.phase == TransferPhase::Writing),
        "a push must report the phase in which its objects actually leave this \
         host; without it the stream shows nothing for the whole transfer. Saw \
         {seen:?}"
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

/// Break the *pushing* repository's ref store at the moment git records the
/// remote-tracking ref, so `exec_push`'s post-push re-read fails.
///
/// `reference-transaction` fires with `committed` once
/// `refs/remotes/<remote>/<branch>` is durable — verified against git 2.43.0,
/// which runs the hook for the tracking-ref update a push performs, exactly as
/// it does for a fetch's. The hook is installed after all fixture commits, so
/// the only transaction it ever sees is the push's.
///
/// A malformed `packed-refs` is the lever because `git for-each-ref` treats it
/// as fatal (`exit 128`) while the already-written loose ref stays on disk —
/// which is precisely the state this exit path exists for: the repository
/// changed, and nothing can say how.
fn blind_the_repository_after_the_push(repo: &Path) {
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

/// A push that ran, moved the remote, and then could not be re-read still
/// leaves a journal entry — one that **admits** the outcome is unknown.
///
/// [`super::push::journal_unobserved`] had no test at all until this one, which
/// is the shape of gap this repository keeps finding: a journal write with zero
/// coverage reads as diligence and behaves as nothing. Deleting the call left
/// every other test in the crate green (mutation run below), and the feed would
/// then have claimed nothing happened on the one operation whose effect is on
/// **another machine** — where, unlike a fetch, no later local read can reveal
/// it.
///
/// The premise is asserted rather than assumed, because a fixture that merely
/// failed early would make the whole thing vacuous:
///
/// * the response is the specific "could not be re-read" refusal, so this is
///   the post-push re-read path and not some earlier spawn failure;
/// * the loose `refs/remotes/origin/main` is on disk holding the pushed tip, so
///   a ref really did move while unobservable — the divergence is real;
/// * the entry's oids are `None` **and** the summary says the outcome is
///   unknown, which is what distinguishes `Obs::Unknown` ("git could not be
///   read") from `Obs::Absent` ("there was no such tip") — the whole of D5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_whose_outcome_cannot_be_observed_is_journaled_as_unknown() {
    let fx = fixture(2);
    let want = fx.local_tip();
    blind_the_repository_after_the_push(&fx.repo);

    let (status, body) = pipeline(&fx.repo, push_op(false, ForcePublish::None)).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the fixture must reach the post-push re-read failure: {body}"
    );
    assert!(
        body.contains("could not be re-read"),
        "…and it must be *that* refusal, not some earlier one: {body}"
    );

    let loose = fx.repo.join(".git/refs/remotes/origin/main");
    assert!(
        loose.exists(),
        "the fixture must let the tracking ref actually move before blinding \
         the repository, or there is no divergence for the journal to record"
    );
    assert_eq!(
        std::fs::read_to_string(&loose).unwrap().trim(),
        want,
        "…and it must hold the tip that was pushed"
    );
    assert_eq!(
        fx.remote_tip(),
        want,
        "the remote really did move — this is a push whose *effect* is real and \
         whose *record* is what went missing"
    );

    let entries = journaled_pushes(&fx.repo);
    assert_eq!(
        entries.len(),
        1,
        "an unobservable push must still leave exactly one entry: {entries:?}"
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

// ---------------------------------------------------------------------------
// --set-upstream
// ---------------------------------------------------------------------------

/// `--set-upstream` records an upstream, **read back out of git's own config**,
/// and only when the operation asked for one.
///
/// Both legs run against the same fixture shape, so the difference is
/// attributable to the flag and nothing else. Reading `branch.main.merge` rather
/// than trusting the response is the whole point: the response sentence is built
/// from an observation this test independently repeats, and a server that echoed
/// the request would pass a response-only assertion while recording nothing.
#[tokio::test]
async fn set_upstream_is_recorded_and_only_when_asked() {
    // Leg 1: asked for.
    let fx = fixture(1);
    assert_eq!(config(&fx.repo, "branch.main.merge"), None);
    let (status, body) = pipeline(&fx.repo, push_op(true, ForcePublish::None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        config(&fx.repo, "branch.main.remote").as_deref(),
        Some("origin"),
        "git must have recorded the upstream remote"
    );
    assert_eq!(
        config(&fx.repo, "branch.main.merge").as_deref(),
        Some("refs/heads/main"),
        "git must have recorded the upstream branch"
    );
    assert!(
        body.contains("Upstream set to ‘origin/main’"),
        "the response must report the upstream it observed: {body}"
    );
    assert!(
        journaled_pushes(&fx.repo)[0]
            .summary
            .contains("--set-upstream"),
        "the journal must name the mode that ran: {:?}",
        journaled_pushes(&fx.repo)[0].summary
    );
    drop(fx);

    // Leg 2, the paired negative: not asked for, not recorded, not claimed.
    let fx = fixture(1);
    let (status, body) = pipeline(&fx.repo, push_op(false, ForcePublish::None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        config(&fx.repo, "branch.main.merge"),
        None,
        "a push that did not ask for an upstream must not set one"
    );
    assert!(
        !body.contains("Upstream"),
        "…and must not mention one: {body}"
    );
}

// ---------------------------------------------------------------------------
// The lease
// ---------------------------------------------------------------------------

/// A correct lease force-publishes, and the remote lands **exactly** on the new
/// tip — with the anti-vacuity leg that makes that mean something.
///
/// Leg 1 pushes the same diverged branch with `ForcePublish::None` and requires
/// it to be **refused**, with the remote untouched. Without it, this test would
/// pass identically on a fixture where an ordinary fast-forward would have
/// worked — i.e. it would prove nothing about forcing at all.
///
/// Leg 2 then pushes with a lease naming the tip leg 1 was refused against, and
/// the remote moves. The old remote commit is asserted **unreachable from the
/// remote's `main`**, which is what "force-published" actually means and what a
/// tip comparison alone would not show.
#[tokio::test]
async fn a_correct_lease_force_publishes_and_a_plain_push_of_the_same_branch_does_not() {
    let fx = fixture(0);
    let original = fx.remote_tip();
    let rewritten = diverge_locally(&fx.repo);
    assert_ne!(original, rewritten);
    assert_eq!(
        fx.tracking_tip(),
        original,
        "the local remote-tracking ref must still hold the reviewed tip"
    );

    // Leg 1: the anti-vacuity leg. No force ⇒ git refuses, remote untouched.
    let (status, body) = pipeline(&fx.repo, push_op(false, ForcePublish::None)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a non-fast-forward push must be refused, or the lease below proves \
         nothing: {body}"
    );
    assert_eq!(
        fx.remote_tip(),
        original,
        "a refused push must leave the remote where it was"
    );

    // Leg 2: the lease, naming the tip the reviewer saw.
    let (status, body) = pipeline(&fx.repo, push_op(false, lease(&original))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fx.remote_tip(),
        rewritten,
        "the remote must land exactly on the reviewed local tip"
    );
    let reachable = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &original, "main"])
        .current_dir(&fx.remote)
        .status()
        .unwrap();
    assert!(
        !reachable.success(),
        "the replaced commit must no longer be reachable from the remote's \
         main — that is what force-publishing did, and a tip comparison alone \
         would not show it"
    );

    let entries = journaled_pushes(&fx.repo);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(
        entries[0].summary.contains("force-published (lease) over"),
        "the journal must name the mode and what it replaced: {}",
        entries[0].summary
    );
    assert!(
        entries[0].summary.contains(&original[..8]),
        "…and the tip it replaced must be the observed one: {}",
        entries[0].summary
    );
    assert!(
        body.contains("Force-published"),
        "the response must say which mode ran: {body}"
    );
}

/// **The headline refusal.** Someone else pushed between the plan being
/// reviewed and it being submitted; the lease loses; the remote keeps *their*
/// commit.
///
/// This is the one case only git can catch: this repository's
/// `refs/remotes/origin/main` still holds the reviewed tip (nothing fetched
/// since), so the server's own pre-flight check passes and the compare-and-swap
/// that decides is git's, against what the remote advertises. Both layers exist
/// precisely so this case and the forged-tip case below are each caught by the
/// one that can see them.
///
/// The assertion that matters is **the remote's ref listing**, not the status
/// code: the property is "the other party's commit is still there", and a
/// status code cannot say that.
#[tokio::test]
async fn a_lease_lost_to_a_concurrent_push_is_refused_and_the_remote_keeps_the_other_commit() {
    let fx = fixture(0);
    let reviewed = fx.remote_tip();
    let rewritten = diverge_locally(&fx.repo);

    // A third party pushes, using a clone of its own and plain git — this
    // repository learns nothing about it.
    let theirs = fx._dir.path().join("theirs");
    run(
        fx._dir.path(),
        &[
            "clone",
            "-q",
            &fx.remote.display().to_string(),
            &theirs.display().to_string(),
        ],
    );
    run(&theirs, &["config", "user.email", "o@example.invalid"]);
    run(&theirs, &["config", "user.name", "o"]);
    std::fs::write(theirs.join("theirs.txt"), "their work\n").unwrap();
    run(&theirs, &["add", "theirs.txt"]);
    run(&theirs, &["commit", "-q", "-m", "their work"]);
    run(&theirs, &["push", "-q", "origin", "main"]);
    let theirs_tip = fx.remote_tip();
    assert_ne!(
        theirs_tip, reviewed,
        "the third party must really have pushed"
    );
    assert_eq!(
        fx.tracking_tip(),
        reviewed,
        "this repository must still be unaware — otherwise the server's own \
         pre-flight would catch it and git's check would never be exercised"
    );

    let heads_before = fx.remote_heads();
    let (status, body) = pipeline(&fx.repo, push_op(false, lease(&reviewed))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        fx.remote_heads(),
        heads_before,
        "THE assertion: the remote must be byte-for-byte what it was, with the \
         other party's commit still on it. Response was: {body}"
    );
    assert_eq!(fx.remote_tip(), theirs_tip);
    assert_ne!(
        fx.remote_tip(),
        rewritten,
        "the force-publish must not have landed"
    );
    assert!(
        body.contains("lease"),
        "the refusal must name the lease as the reason, so a user knows a \
         fetch is the remedy: {body}"
    );
    assert!(
        journaled_pushes(&fx.repo).is_empty(),
        "a push that moved nothing must journal nothing: {:?}",
        journaled_pushes(&fx.repo)
    );
}

/// A lease whose tip does not match this repository's own remote-tracking ref
/// — a stale client, or a forged request body — is refused **before any git is
/// spawned**.
///
/// Proved by absence: the remote's `pre-receive` hook writes a sentinel file the
/// moment `receive-pack` starts, and the sentinel must not exist. A status-code
/// assertion could not tell this refusal from one git made after connecting,
/// and the difference is the whole point — an unverified oid handed to
/// `--force-with-lease` is a socket opened, a credential offered, and a remote
/// asked to consider a force nobody's plan justified.
///
/// Two legs, because the reviewed tip can fail to match in two different ways:
/// the ref holds a *different* oid, and the ref is *gone*. The second is the
/// one that would be easiest to write wrong — treating "no tracking ref" as "no
/// lease to check" would turn a lease into an unguarded force.
#[tokio::test]
async fn a_lease_tip_that_does_not_match_the_tracking_ref_never_reaches_the_remote() {
    // Leg 1: the tip is simply wrong.
    let fx = sentinel_fixture();
    let heads_before = fx.remote_heads();
    let (status, body) = pipeline(&fx.repo, push_op(false, lease(&"4".repeat(40)))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        !fx.reached_the_remote(),
        "a lease this server could not verify must never be handed to git: the \
         remote's pre-receive hook ran. Response was: {body}"
    );
    assert_eq!(fx.remote_heads(), heads_before);
    assert!(
        body.contains("refusing to push") && body.contains("4444"),
        "the refusal must name the tip it could not confirm: {body}"
    );

    // Leg 2: the tracking ref is gone, so there is nothing to confirm against.
    let fx = sentinel_fixture();
    let reviewed = out(&fx.repo, &["rev-parse", "refs/remotes/origin/main"]);
    run(&fx.repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let heads_before = fx.remote_heads();
    let (status, body) = pipeline(&fx.repo, push_op(false, lease(&reviewed))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        !fx.reached_the_remote(),
        "a lease against a ref that no longer exists must not be forwarded as \
         though it were unguarded. Response was: {body}"
    );
    assert_eq!(fx.remote_heads(), heads_before);
    assert!(
        body.contains("no longer exists"),
        "the refusal must say which of the two cases this was: {body}"
    );
}

/// The paired positive for the pre-flight: it refuses a mismatch and **passes**
/// a match, so the refusals above are not a gate that is simply always shut.
///
/// Runs on the sentinel fixture with a lease naming the true tracking tip: the
/// pre-flight must let it through, which is observable as the push actually
/// reaching the remote (the hook fires). The push then fails — a path remote
/// cannot survive the sandbox's withheld `LANDLOCK_ACCESS_FS_REFER`, which is
/// exactly why the daemon fixture exists — and that failure is *after* the gate,
/// which is all this leg is about.
#[tokio::test]
async fn a_lease_that_matches_the_tracking_ref_is_let_through_the_pre_flight() {
    let fx = sentinel_fixture();
    let reviewed = out(&fx.repo, &["rev-parse", "refs/remotes/origin/main"]);
    diverge_locally(&fx.repo);
    let (_status, body) = pipeline(&fx.repo, push_op(false, lease(&reviewed))).await;
    assert!(
        fx.reached_the_remote(),
        "a lease naming the true tracking tip must get past the pre-flight — if \
         it does not, the refusals above prove only that this gate is always \
         shut. Response was: {body}"
    );
    assert!(
        !body.contains("refusing to push"),
        "…and the refusal it produced must not be the pre-flight's: {body}"
    );
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// A cancel that arrives *before* the executor spawns anything must stop the
/// push from starting at all — the latch is read once more immediately before
/// the spawn, so an operation cancelled while queued behind the repository
/// guard does not then publish anyway.
///
/// The sentinel proves it: nothing reached the remote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_that_lands_before_execution_stops_the_push_starting() {
    let fx = sentinel_fixture();
    std::fs::write(fx.repo.join("more.txt"), "more\n").unwrap();
    run(&fx.repo, &["add", "more.txt"]);
    run(&fx.repo, &["commit", "-q", "-m", "more"]);

    let op = push_op(false, ForcePublish::None);
    let (handle, record) = admit_push("cancel-early", &op);
    assert!(
        record.request_cancel(),
        "a live record must accept a cancel"
    );

    let (status, body) = run_tracked(&fx.repo, record, op).await;
    handle.finish(status, body.clone(), None);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("cancelled before it started"),
        "the terminal message must say which case this was: {body}"
    );
    assert!(
        !fx.reached_the_remote(),
        "a push cancelled before it started must not have contacted the remote"
    );
}

/// How long the remote's `pre-receive` hook blocks for.
///
/// **Load-bearing, not a tuning knob**: every "the cancel was prompt" assertion
/// below is only worth something because it is bounded far below this. See
/// [`PROMPT`].
const HANG: Duration = Duration::from_secs(20);

/// The budget a cancel gets, both for the endpoint to answer and for the child
/// to be gone from `/proc`.
///
/// A cancelled push against [`hang_the_remotes_next_receive`] ends up dead
/// either way *eventually* — the hook exits after [`HANG`], so `child.wait()`
/// returns with or without a kill. A test that only asked "is the process gone
/// afterwards?" with a timeout of `HANG`'s own order would therefore pass
/// identically for an implementation that never kills anything and merely waits
/// the remote out. So the discriminating question is **promptness**, and it is
/// only a fair question if the natural exit is provably later than the budget:
/// the test below dwells `PROMPT` *before* cancelling and asserts the push is
/// still running, so a pass requires the child to die within `PROMPT` of the
/// cancel when it had at least `HANG - 2 * PROMPT` of hanging left to do.
const PROMPT: Duration = Duration::from_secs(3);

/// `SIGKILL`, spelled out rather than pulled from a dependency this crate does
/// not otherwise need. Fixed at 9 on every Linux ABI.
const SIGKILL: i32 = 9;

/// Make the bare remote's `receive-pack` block for [`HANG`] once the pack has
/// arrived and before any ref is updated.
///
/// A `pre-receive` hook is exactly the right lever: it runs on the far side,
/// after the objects have been transferred (so `git push` is genuinely in
/// flight, past `Writing objects`) and **before** the ref update (so a push
/// killed here provably did not move the remote). The daemon that runs it is
/// spawned unsandboxed by the fixture, as all fixture setup here is.
fn hang_the_remotes_next_receive(remote: &Path) {
    let hooks = remote.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nsleep {}\nexit 0\n", HANG.as_secs()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every live process whose argv names `repo` **and** is a push — i.e. the
/// sandbox shim / git child this server spawned for this repository.
///
/// A `/proc` scan rather than a pid handed back by the runner, on purpose: the
/// point is to observe the process from *outside* the code under test.
fn live_push_processes(repo: &Path) -> Vec<i32> {
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
        if argv.contains(&needle) && argv.contains("push") {
            out.push(pid);
        }
    }
    out
}

/// **The load-bearing cancellation test**: a running push is cancelled through
/// the real endpoint, the child process is gone *promptly*, and the remote's ref
/// did not move.
///
/// Four legs, in the order that makes each one mean something:
///
/// 1. While the push runs, `/proc` shows a matching process. Without it, "no
///    process afterwards" would also be true of a scan that can never find
///    anything.
/// 2. **The hang outlives the budget.** The test dwells [`PROMPT`] without
///    cancelling and asserts the push is *still* running and the driver has
///    *not* answered. This is what makes leg 4 discriminating: it establishes on
///    this run, not by construction, that the child was not about to exit on its
///    own. (This is the hole #229 found by mutation: with a generous timeout,
///    deleting `child.start_kill()` left every assertion green.)
/// 3. `POST /api/operations/{id}/cancel` answers `202`.
/// 4. Within `PROMPT` the driver answers **and** the process is gone — with at
///    least `HANG - 2 * PROMPT` of hanging still owed. The remote's `main` is
///    then read directly and must be unmoved: the hook blocks *before* the ref
///    update, so a killed push cannot have published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_running_push_kills_the_child_and_the_remote_does_not_move() {
    let fx = fixture(3);
    hang_the_remotes_next_receive(&fx.remote);
    let before = fx.remote_tip();

    let op = push_op(false, ForcePublish::None);
    let (handle, record) = admit_push("cancel-kills", &op);
    let id = record.id();

    let driver = {
        let repo = fx.repo.clone();
        let record = record.clone();
        tokio::spawn(async move { run_tracked(&repo, record, op).await })
    };

    let scan_repo = fx.repo.clone();
    assert!(
        within(HANG, || !live_push_processes(&scan_repo).is_empty()).await,
        "no git push process appeared for {:?} — the fixture never got as far \
         as spawning one, so nothing below would mean anything",
        fx.repo
    );

    // Leg 2.
    tokio::time::sleep(PROMPT).await;
    assert!(
        !live_push_processes(&fx.repo).is_empty(),
        "the hung push exited on its own inside the promptness budget — the \
         budget below would then prove nothing about killing. Raise HANG."
    );
    assert!(
        !driver.is_finished(),
        "the uncancelled push already answered inside the promptness budget; \
         the assertions below could not distinguish a kill from a wait"
    );

    let response =
        crate::handlers::operations::cancel_operation(axum::extract::Path(id.as_str().to_string()))
            .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a running push must accept the cancel — `honours_cancellation` \
         promises it does"
    );

    let (status, body) = tokio::time::timeout(PROMPT, driver)
        .await
        .expect(
            "the cancelled push must return within the promptness budget. \
             Timing out here with the remote's hook still owed most of its \
             sleep is what a cancel that merely stops waiting looks like",
        )
        .unwrap();
    handle.finish(status, body.clone(), None);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    assert!(
        within(PROMPT, || live_push_processes(&fx.repo).is_empty()).await,
        "the git push child survived the cancel — a cancel that only stops \
         waiting leaves git talking to the remote. Still alive: {:?}",
        live_push_processes(&fx.repo)
    );

    assert_eq!(
        fx.remote_tip(),
        before,
        "the remote's pre-receive hook blocks before any ref update, so a \
         cancelled push cannot have moved it"
    );
    assert!(
        body.contains("cancelled"),
        "the terminal message must say the push was cancelled: {body}"
    );
    assert!(
        body.contains("Fetch to see where the remote actually is"),
        "a cancelled push must not claim the remote is unchanged — this server \
         stopped talking to it mid-sentence and cannot know: {body}"
    );
}

/// The mechanism, observed directly: a cancelled push leaves a child that was
/// **killed by a signal**, not one that exited.
///
/// The assertion the endpoint-level test cannot make, and the one that cannot be
/// satisfied by waiting: `WTERMSIG` is set by the kernel when and only when the
/// process was signalled. The paired negative is the second leg — the same
/// helper, the same repository, no cancel, comes back `cancelled == false` and
/// `signal() == None`. Without it, an implementation reporting `Some(SIGKILL)`
/// unconditionally would pass the first leg.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_push_stream_leaves_a_signalled_child_not_an_exited_one() {
    use std::os::unix::process::ExitStatusExt;

    let fx = fixture(2);
    hang_the_remotes_next_receive(&fx.remote);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let run_repo = fx.repo.clone();
    let cancelled_run = tokio::spawn(async move {
        crate::git_cmd::git_streamed_for(
            &run_repo,
            &["push", "--progress", "origin", "main"],
            crate::sandbox::NetworkNeed::Remote,
            Some(rx),
            |_| {},
        )
        .await
    });

    let scan_repo = fx.repo.clone();
    assert!(
        within(HANG, || !live_push_processes(&scan_repo).is_empty()).await,
        "no git push process appeared — nothing below would mean anything"
    );
    tx.send(true).unwrap();

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
         push` talking to the remote. Status was {:?}",
        killed.output.status
    );

    // The paired negative: no cancel, no hang, same helper — but a **fresh
    // fixture**, and that is not fastidiousness. The killed push's counterpart
    // on the remote side is a `receive-pack` this test does not own and cannot
    // reap (ADR 0043's grandchild note, and ADR 0045 inherits it): it is still
    // inside its `sleep`, still holding the ref it was about to update. Reusing
    // the same remote makes the second push race that leftover, which is how
    // this leg first failed — with a `cannot lock ref … is at X but expected Y`
    // that says nothing about the property under test.
    //
    // The old fixture is dropped first: it holds the port claim, and
    // `PortClaim::acquire` panics rather than deadlocking if one thread asks
    // twice.
    drop(fx);
    let fx = fixture(2);
    let ordinary = crate::git_cmd::git_streamed_for(
        &fx.repo,
        &["push", "--progress", "origin", "main"],
        crate::sandbox::NetworkNeed::Remote,
        None,
        |_| {},
    )
    .await
    .expect("an ordinary push must run");
    assert!(
        !ordinary.cancelled,
        "a run nobody cancelled must not report a cancel"
    );
    assert_eq!(
        ordinary.output.status.signal(),
        None,
        "an ordinary push's child exits; if this also reports a signal, the \
         assertion above is reading something other than the kill"
    );
    assert!(
        ordinary.output.status.success(),
        "the paired-negative push must actually have worked: {:?}",
        String::from_utf8_lossy(&ordinary.output.stderr)
    );
}

/// `sleep`, by absolute path, for a hook that runs **inside the sandbox**.
///
/// The shim's read-only grant covers `/usr` and `/bin`, but nothing promises the
/// hook a usable `PATH`, so a bare `sleep` could be "command not found" on a host
/// whose environment differs from this one. The hang test's second leg would
/// catch that (an instantly-returning hook fails "the push had already exited"),
/// so the risk is a confusing red rather than a false green — but a hook that
/// resolves its own binary turns a host-shaped flake into no flake at all.
fn sleep_binary() -> &'static str {
    ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("this host must have a sleep binary for the hang fixture")
}

/// Hang the **pushing** repository at the instant `refs/remotes/origin/<branch>`
/// becomes durable, so a cancel arriving now lands on a push whose observed
/// before/after diff is already non-empty.
///
/// `reference-transaction` fires with `committed` once the tracking-ref update
/// is on disk and unlocked — the same fact
/// [`blind_the_repository_after_the_push`] rests on, verified against git
/// 2.43.0. A push held here has therefore *already* had its ref update accepted
/// by the remote and recorded locally, and is still alive to be killed.
///
/// **No remote-side lever can produce this state**, which is why the hang goes
/// in the local repository rather than in the bare remote like
/// [`hang_the_remotes_next_receive`]: `git push` does not call
/// `transport_update_tracking_ref` until `push_refs` has returned, and
/// `push_refs` does not return until `finish_connect` has reaped
/// `receive-pack`. Both `pre-receive` and `post-receive` run on the far side of
/// that wait, so a push hung by either has a tracking ref that has *not* moved —
/// exactly the case
/// `cancelling_a_running_push_kills_the_child_and_the_remote_does_not_move`
/// already covers.
///
/// The hook is installed after all fixture commits, so the only transaction it
/// ever sees is the push's own tracking-ref update.
fn hang_the_repository_once_the_tracking_ref_lands(repo: &Path) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = committed ]; then\n\
             {} {}\n\
             fi\n\
             exit 0\n",
            sleep_binary(),
            HANG.as_secs()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A push cancelled **after** the remote-tracking ref has already advanced says
/// *that much was accepted*, and journals the ref it moved with the cancel
/// admitted in the summary.
///
/// This is the other half of cancellation, and until this test it had none:
/// every cancel fixture in this file blocks on the remote's `pre-receive`, which
/// is by construction before any ref update, so `updated` was always empty when
/// a test cancelled. Mutating [`super::push::cancelled_response`]'s non-empty
/// arm — or deleting `journal_updates`' `" (the push was then cancelled)"` tail
/// — left the whole suite green.
///
/// What makes the run mean something, in the order the legs establish it:
///
/// 1. **The tracking ref really moved**, polled until it holds the local tip.
///    Without this the test would be the empty-diff case again.
/// 2. **The push is still alive** at that moment, and the driver has not
///    answered. A cancel arriving after the child exited takes
///    `run.cancelled == false` and never reaches the branch under test, so this
///    is what makes leg 4 a cancellation result rather than a success.
/// 3. The cancel goes through the real endpoint and is accepted.
/// 4. The answer is the non-empty sentence — and **not** the empty one, which is
///    the paired negative: an implementation stuck on either branch fails here
///    or in the existing `pre-receive` test, and no implementation passes both
///    while ignoring what it observed.
/// 5. The remote is then read directly and *is* at the pushed tip, so the
///    sentence's claim ("accepted by the remote") is checked against the remote
///    rather than against the same diff that produced it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_that_lands_after_the_ref_moved_reports_what_the_remote_accepted() {
    let fx = fixture(2);
    let want = fx.local_tip();
    let was = fx.remote_tip();
    assert_ne!(was, want, "the fixture must have something to push");
    hang_the_repository_once_the_tracking_ref_lands(&fx.repo);

    let op = push_op(false, ForcePublish::None);
    let (handle, record) = admit_push("cancel-after-ref", &op);
    let id = record.id();

    let driver = {
        let repo = fx.repo.clone();
        let record = record.clone();
        tokio::spawn(async move { run_tracked(&repo, record, op).await })
    };

    // Leg 1.
    assert!(
        within(HANG, || fx.tracking_tip() == want).await,
        "refs/remotes/origin/main never reached the pushed tip, so the push \
         never got past the wire and this test would be measuring the \
         empty-diff case the pre-receive test already covers"
    );

    // Leg 2 — the discriminating one.
    assert!(
        !live_push_processes(&fx.repo).is_empty(),
        "the push had already exited when its ref landed, so the cancel below \
         cannot reach the cancelled path at all"
    );
    assert!(
        !driver.is_finished(),
        "the push answered before it was cancelled; the assertions below would \
         then be about a *successful* push's message"
    );

    // Leg 3.
    let response =
        crate::handlers::operations::cancel_operation(axum::extract::Path(id.as_str().to_string()))
            .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a running push must accept the cancel"
    );

    // Leg 4.
    let (status, body) = tokio::time::timeout(PROMPT, driver)
        .await
        .expect(
            "the cancelled push must return within the promptness budget, with \
             the hook still owed most of its sleep",
        )
        .unwrap();
    handle.finish(status, body.clone(), None);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("had already been updated, so that much was accepted by the remote"),
        "a cancel that landed after the ref moved must say so — the user needs \
         to know their push partly landed: {body}"
    );
    assert!(
        !body.contains("never saw the remote accept"),
        "…and must not report an observed ref update as an unobserved one: {body}"
    );

    // Leg 5: the referee.
    assert_eq!(
        fx.remote_tip(),
        want,
        "the message claims the remote accepted the update; the remote itself \
         must agree, or the sentence is a guess dressed as an observation"
    );

    let entries = journaled_pushes(&fx.repo);
    assert_eq!(
        entries.len(),
        1,
        "the one ref that moved must journal exactly one entry: {entries:?}"
    );
    let entry = &entries[0];
    assert_eq!(
        entry.ref_name.as_deref(),
        Some("refs/remotes/origin/main"),
        "{entry:?}"
    );
    assert_eq!(entry.old_oid.as_deref(), Some(was.as_str()), "{entry:?}");
    assert_eq!(entry.new_oid.as_deref(), Some(want.as_str()), "{entry:?}");
    assert_eq!(entry.source, ActivitySource::App, "{entry:?}");
    assert!(
        entry.summary.contains("(the push was then cancelled)"),
        "the feed must not show a ref that moved under a cancelled push as an \
         ordinary completed push — the difference is the whole reason to read \
         the feed after a cancel: {}",
        entry.summary
    );
    assert!(
        entry.summary.starts_with("pushed "),
        "…while still naming the mode that ran: {}",
        entry.summary
    );
}

// ---------------------------------------------------------------------------
// Redaction on the live path
// ---------------------------------------------------------------------------

/// A credential the remote leaks on its own stderr never reaches the operation
/// record — the same guarantee `fetch_suite` proves for fetch, re-proved for the
/// direction that carries a *write* credential.
///
/// The premise is **asserted, not assumed**: the same fixture is first pushed
/// with plain git outside this server's harness and the raw stderr checked to
/// contain the literal secret. Without that leg, a fixture that never leaked
/// would make the redaction assertion pass over nothing.
///
/// The leak is staged in the remote's `pre-receive` hook, which is exactly where
/// a real one comes from: git strips userinfo from URLs it prints itself, and
/// the hole ADR 0036 documents is what *other programs in the pipeline* print.
#[tokio::test]
async fn a_credential_leaked_by_the_remote_never_reaches_the_push_response() {
    const SECRET: &str = "hunter2-push-suite";
    let secret_url = format!("https://svcuser:{SECRET}@leaked-host.invalid/org/repo.git");

    let fx = fixture(1);
    let hooks = fx.remote.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho 'mirroring to {secret_url}' >&2\nexit 1\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Premise: unredacted, this fixture really does leak.
    let raw = std::process::Command::new("git")
        .args(["push", "--progress", "origin", "main"])
        .current_dir(&fx.repo)
        .output()
        .unwrap();
    let raw_stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
    assert!(
        raw_stderr.contains(SECRET),
        "the fixture must actually leak the secret when unredacted, or the \
         assertion below proves nothing. Got: {raw_stderr}"
    );

    let (status, body) = pipeline(&fx.repo, push_op(false, ForcePublish::None)).await;
    assert_ne!(status, StatusCode::OK, "the hook declines: {body}");
    assert!(
        !body.contains(SECRET),
        "the push response carries a credential the remote leaked: {body}"
    );
    assert!(
        body.contains("leaked-host.invalid"),
        "redaction must strip the userinfo and keep the rest — a message with \
         the host removed too would be useless: {body}"
    );
}

// ---------------------------------------------------------------------------
// The upstream observation
// ---------------------------------------------------------------------------

/// [`super::push::upstream_of`] **reads** the repository rather than echoing
/// what was asked for — proved both ways against a real one.
///
/// The negative leg is the load-bearing half. `success_message` claims "the
/// upstream is now X" on the strength of this function, and an implementation
/// that returned a plausible constant (`origin/<branch>` is right almost
/// always) would satisfy every other assertion in this file — including
/// `set_upstream_is_recorded_and_only_when_asked`, which checks the sentence —
/// while reporting an upstream on a branch that has none.
///
/// It lives here rather than beside the function because it needs a real
/// repository, and `argv_boundary`'s spawn tripwire (rightly) will not
/// allowlist a production module as a process-spawn site.
#[tokio::test]
async fn the_upstream_is_read_from_the_repository_not_assumed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().to_path_buf();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    let main = BranchName::new("main").unwrap();
    let side = BranchName::new("side").unwrap();

    // Negative: no upstream configured.
    assert!(
        matches!(
            super::push::upstream_of(&repo, &main).await,
            super::Obs::Absent
        ),
        "a branch with no upstream must report none"
    );

    // Positive: an upstream that genuinely resolves. The `remote add` is
    // required and not decoration — git resolves `@{upstream}` through the
    // remote's *fetch refspec*, so without one it refuses with "upstream branch
    // 'refs/heads/main' not stored as a remote-tracking branch" even though the
    // config keys and the tracking ref both exist. (Verified against git 2.43.0
    // while writing this test, which is the sort of thing that makes a
    // hand-built fixture worth more than a mocked one.)
    let bare = repo.join("up.git");
    std::fs::create_dir_all(&bare).unwrap();
    run(&bare, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &bare.display().to_string()],
    );
    run(
        &repo,
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    run(&repo, &["config", "branch.main.remote", "origin"]);
    run(&repo, &["config", "branch.main.merge", "refs/heads/main"]);
    assert!(
        matches!(super::push::upstream_of(&repo, &main).await, super::Obs::Known(u) if u == "origin/main"),
        "a configured upstream must be reported as git names it"
    );

    // And a branch that is not the configured one still reports none — so the
    // positive leg above is reading *this* branch's config, not any.
    run(&repo, &["branch", "side"]);
    assert!(
        matches!(
            super::push::upstream_of(&repo, &side).await,
            super::Obs::Absent
        ),
        "the read must be per-branch"
    );
}

/// [`super::upstream_of`] (#233) — the `pub(crate)` wrapper `/api/rebase-status`
/// calls — collapses [`super::push::upstream_of`]'s three-state [`super::Obs`]
/// into `Result<Option<String>, ExecUnavailable>`. This pins the mapping this
/// slice's whole correctness rests on: `Obs::Absent` ("git ran and reported no
/// upstream" — the ordinary state of any fresh local branch) must become
/// `Ok(None)`, NOT an `Err`. Getting this backwards would turn
/// `/api/rebase-status` into a 500 for every repository whose checked-out
/// branch simply has no upstream configured yet — the common case, not an edge
/// one — which is exactly the kind of regression `push_suite`'s neighbouring
/// test above already proves `push::upstream_of` itself does not make (it
/// returns `Absent`, never `Unknown`, when git runs and finds none); this test
/// proves the wrapper on top of it preserves that distinction rather than
/// collapsing `Absent` into the `Unknown`/error leg by accident.
#[tokio::test]
async fn the_crate_visible_wrapper_reports_no_upstream_as_ok_none_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().to_path_buf();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    let main = BranchName::new("main").unwrap();

    // The regression this test exists to catch: a fresh branch with no
    // upstream must be a successful `None`, not an `Err`.
    assert_eq!(
        super::upstream_of(&repo, &main).await.unwrap(),
        None,
        "a branch with no upstream must be Ok(None), never an Err — an Err here \
         is what turns /api/rebase-status into a 500 for the ordinary case of a \
         fresh local branch"
    );

    // The paired positive, so the negative above is not merely "always Ok":
    // a genuinely configured upstream still comes through as `Some`.
    let bare = repo.join("up.git");
    std::fs::create_dir_all(&bare).unwrap();
    run(&bare, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &bare.display().to_string()],
    );
    run(
        &repo,
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    run(&repo, &["config", "branch.main.remote", "origin"]);
    run(&repo, &["config", "branch.main.merge", "refs/heads/main"]);
    assert_eq!(
        super::upstream_of(&repo, &main).await.unwrap(),
        Some("origin/main".to_string())
    );
}
