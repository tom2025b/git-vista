//! The regression battery for **which host a remote-reaching operation may
//! actually talk to** (#229 follow-up, ADR 0044).
//!
//! # The hole these tests were written against
//!
//! `handlers/fetch.rs`'s own doc comment named the defence:
//!
//! > [`FetchRequest`] carries a name, which the plan's
//! > `Precondition::RemoteConfigured` then requires to exist in the
//! > repository's own configuration. A request that could carry a URL would
//! > let any authenticated client point this server — and whatever credential
//! > helper or SSH agent the host offers it — at a host of the client's
//! > choosing.
//!
//! Neither half of that held.
//!
//! * `RemoteName`'s validator was [`require_git_safe`] — non-empty, not
//!   starting with `-`. `https://attacker.example/r.git` satisfies both, so a
//!   URL *could* be carried by a request. Nothing downstream re-checked the
//!   shape, and `git fetch <url>` treats an argument it does not recognise as
//!   a configured remote as a URL.
//! * `Precondition::RemoteConfigured` could not catch it, because
//!   [`super::enforce_fresh`] re-verifies **only** the preconditions that
//!   *held* at build time. An unconfigured remote fails at build time, so the
//!   gate skipped it and the executor ran anyway. The comment there
//!   ("the executor's own legacy guard refuses it") is true for every
//!   precondition whose executor really has one — and false for this one:
//!   `git fetch` does not refuse an unknown remote, it reinterprets it.
//!
//! Verified against git 2.43.0 before either fix existed: `git fetch
//! ghost.git` inside a repository with no `ghost.git` remote fetches from the
//! *directory* `ghost.git` and writes `.git/FETCH_HEAD` — and, because
//! `refs/remotes/ghost.git/*` never moves, `exec_fetch`'s before/after diff is
//! empty and the endpoint answers `200 … already up to date`.
//!
//! # What each test proves, and why it cannot pass vacuously
//!
//! The load-bearing test binds a **real listener** and asserts nothing
//! connects to it. On its own that is worthless — a listener nothing could
//! ever reach satisfies it too. So it carries a **paired positive control on
//! the same run**: the same URL, the same listener, the same sandbox, reached
//! through a *configured* remote, and the watcher must see the connection.
//! The negative leg runs first and requires the connection count not to move
//! across it; the control leg runs second and requires the same count to
//! move.
//!
//! The port is [`crate::test_ports::PortClaim::PORT`] (9418) and not an
//! ephemeral one, deliberately: it is the only unprivileged entry in
//! `sandbox::DEFAULT_GIT_PORTS`, so it is the only port a Network-tier
//! Landlock connect grant covers. A test on an ephemeral port would assert
//! "nothing connected" against a connect the *sandbox* refused, and would
//! keep passing with both fixes reverted.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;

use git_vista_protocol::{
    BranchName, FetchError, GitOperation, MergeStrategy, Precondition, RemoteName, RepositoryToken,
    WorktreeToken,
};

use crate::handlers::fetch::validate_remote;
use crate::test_ports::PortClaim;

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
        RepositoryToken::new("remote-boundary-repo").unwrap(),
        WorktreeToken::new("remote-boundary-worktree").unwrap(),
    )
}

/// A repository with one commit and **no remote configured** — the state in
/// which every "an unconfigured remote must not be reached" leg runs.
fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "seed\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);
    (dir, repo)
}

/// Add a bare repository named `name` **inside the served repository's tree**,
/// carrying one commit the served repository does not have.
///
/// Inside the tree because of the sandbox: #66 Task 6 grants the served
/// repository and the system trees and nothing else, so a sibling tempdir
/// would be denied for a reason that has nothing to do with what is under
/// test. Inside the grant, a fetch from it genuinely works — which is exactly
/// what makes it a fair target for "the server must refuse to fetch this".
fn bare_target_inside(dir: &Path, repo: &Path, name: &str) -> PathBuf {
    let target = repo.join(name);
    std::fs::create_dir_all(&target).unwrap();
    run(&target, &["init", "-q", "--bare", "-b", "main"]);

    let authoring = dir.join(format!("authoring-{name}"));
    run(
        dir,
        &[
            "clone",
            "-q",
            &target.display().to_string(),
            &authoring.display().to_string(),
        ],
    );
    run(&authoring, &["config", "user.email", "t@example.invalid"]);
    run(&authoring, &["config", "user.name", "t"]);
    std::fs::write(authoring.join("secret.txt"), "payload\n").unwrap();
    run(&authoring, &["add", "secret.txt"]);
    run(&authoring, &["commit", "-q", "-m", "payload"]);
    run(
        &authoring,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    target
}

/// The pipeline the handler runs, entered at the same two steps
/// `handlers::fetch::fetch_remote` runs in the same order: the handler's own
/// request-shape gate ([`validate_remote`], which is where `RemoteName::new`
/// is called on wire input), then the guarded plan/execute pipeline.
///
/// The one thing not driven here is `reject_if_read_only`, which reads the
/// process-global selection `state::CURRENT` — set once per process and owned
/// by `state`'s own test (see its comment). Everything downstream of it is the
/// production path: `plan_and_execute` differs from `plan_and_execute_in` only
/// by reading that same global for the repository path and the idempotency
/// wrapper, neither of which any assertion below depends on.
async fn drive_fetch(repo: &Path, raw_remote: &str) -> (StatusCode, String) {
    match validate_remote(raw_remote) {
        Ok(remote) => {
            super::plan_and_execute_in(repo, None, tokens(), GitOperation::FetchRemote { remote })
                .await
        }
        Err(refused) => refused,
    }
}

// ---------------------------------------------------------------------------
// The listener the fetch must never reach
// ---------------------------------------------------------------------------

/// How long [`ConnectWatcher::settle`] waits for a connection that is already
/// in the kernel's accept queue to be picked up by the watcher thread.
///
/// **Load-bearing, not a tuning knob.** The watcher polls a non-blocking
/// `accept()`, so a connection completed microseconds before the fetch
/// returned can still be sitting in the backlog, unaccepted, when the
/// assertion reads the count. Reading the count immediately would then report
/// "nothing connected" for a fetch that had already reached the far side —
/// the false *pass* this whole file exists to prevent. Two orders of magnitude
/// above the 10 ms poll, and it costs that much only on the legs that
/// legitimately expect no connection.
const SETTLE: Duration = Duration::from_secs(1);

/// A loopback listener on [`PortClaim::PORT`] that counts **completed TCP
/// connections to it**.
///
/// A `/proc`-free, protocol-free observation from outside the code under test:
/// the accept either happened or it did not. The listener speaks no git
/// protocol at all — it accepts and immediately closes — because the question
/// is "did a connection happen", not "did a fetch succeed".
///
/// # Why a count and not a bool
///
/// The assertion that matters is "*this* call did not connect", not "nothing
/// has ever connected to this port". A count read either side of the call
/// under test says exactly that, and it makes the two failure modes
/// distinguishable in the message: a count that was already non-zero *before*
/// the call is a fixture problem (something else on this host touched the
/// port), while a count that grew *across* the call is the security
/// regression. A bool conflates them, and this suite saw exactly that
/// confusion once during development — a single unreproduced failure whose
/// message could not say which of the two it was.
struct ConnectWatcher {
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
    /// Dropped **last** (declaration order), after the watcher thread has been
    /// joined and its listener closed — releasing the claim while the port is
    /// still bound hands the next claimant an occupied port.
    _claim: PortClaim,
}

impl ConnectWatcher {
    fn bind() -> Self {
        let claim = PortClaim::acquire();
        let listener = TcpListener::bind(("127.0.0.1", PortClaim::PORT))
            .expect("the claim guarantees the port is free");
        listener
            .set_nonblocking(true)
            .expect("a listener can be made non-blocking");
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let connections = Arc::clone(&connections);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connections.fetch_add(1, Ordering::SeqCst);
                            drop(stream);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            connections,
            stop,
            watcher: Some(watcher),
            _claim: claim,
        }
    }

    /// A `git://` URL naming this listener. No DNS, no hostname, no route off
    /// the loopback interface.
    fn url(&self) -> String {
        format!("git://127.0.0.1:{}/anything.git", PortClaim::PORT)
    }

    fn count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The count, once any connection already in the accept queue has had time
    /// to be picked up. Returns as soon as it moves past `baseline`, so the
    /// leg that *expects* a connection pays nothing; the leg that expects none
    /// pays [`SETTLE`], which is the price of not being able to pass falsely.
    async fn settle(&self, baseline: usize) -> usize {
        let deadline = tokio::time::Instant::now() + SETTLE;
        loop {
            let now = self.count();
            if now > baseline || tokio::time::Instant::now() >= deadline {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for ConnectWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The load-bearing test
// ---------------------------------------------------------------------------

/// **A URL-shaped `remote` never reaches the network.**
///
/// Leg 1 (the regression): `POST /api/fetch` is driven with
/// `git://127.0.0.1:9418/anything.git` as the *remote name*. It must be
/// refused, and — the assertion that actually matters — the listener on that
/// port must never have been connected to. A status code alone would not
/// prove this: before the fix the endpoint answered `200 … already up to
/// date` for a fetch that had genuinely talked to the far side, because the
/// before/after diff of `refs/remotes/<url>/*` is empty for an ad-hoc URL.
///
/// Leg 2 (the positive control, on the same run): the *same* URL, configured
/// as a real remote, reached through the *same* pipeline, must connect. This
/// is what makes leg 1 non-vacuous — it proves the listener is reachable, the
/// sandbox's Network-tier grant permits this connect, and the pipeline does
/// open sockets when it is supposed to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_url_shaped_remote_never_reaches_the_network() {
    let watcher = ConnectWatcher::bind();
    let (_dir, repo) = seeded_repo();
    let url = watcher.url();

    // --- leg 1: the URL passed as the remote *name* ---------------------
    let before = watcher.count();
    assert_eq!(
        before,
        0,
        "something connected to 127.0.0.1:{} before the fetch under test ran. \
         That is a fixture problem — a stray process or a leaked listener — \
         not the regression this test is about; the port claim is supposed to \
         make it impossible.",
        PortClaim::PORT
    );
    let (status, body) = drive_fetch(&repo, &url).await;
    let after = watcher.settle(before).await;
    assert_eq!(
        after,
        before,
        "a URL-shaped `remote` reached the network: the fetch opened {} \
         connection(s) to {url}. That is an authenticated client pointing this \
         server — and whatever credential helper or SSH agent the host offers \
         it — at a host of the client's choosing. The endpoint answered \
         {status}: {body}",
        after - before
    );
    assert!(
        status.is_client_error(),
        "a URL-shaped remote must be refused, got {status}: {body}"
    );
    assert!(
        !repo.join(".git/FETCH_HEAD").exists(),
        "the refused fetch still wrote FETCH_HEAD, so git ran"
    );

    // --- leg 2: the positive control ------------------------------------
    // The same URL, now a genuinely configured remote. `RemoteConfigured`
    // holds, so the pipeline executes and git dials the listener. The fetch
    // itself fails (the listener speaks no git protocol) — irrelevant: the
    // assertion is that the connection happened at all.
    run(&repo, &["remote", "add", "control", &url]);
    let (control_status, control_body) = drive_fetch(&repo, "control").await;
    assert!(
        watcher.settle(after).await > after,
        "the positive control never connected to {url}, so leg 1 proved \
         nothing — the listener, the sandbox's Network-tier connect grant, or \
         the pipeline itself is not what this test assumes. Control answered \
         {control_status}: {control_body}"
    );
}

// ---------------------------------------------------------------------------
// The precondition half: a token-shaped remote that is simply not configured
// ---------------------------------------------------------------------------

/// **An unconfigured remote is refused before `git fetch` runs at all.**
///
/// This is the half a stricter `RemoteName` cannot reach: `ghost.git` is an
/// ordinary token-shaped name that any remote-name validator must accept, and
/// git resolves it — when it is *not* a configured remote — as a path relative
/// to the repository, fetching from it for real.
///
/// Observed by the effect, not the status code: `.git/FETCH_HEAD` is written
/// by any fetch that reached a target, and it is absent before. Before the
/// fix this test failed on the FETCH_HEAD assertion *while the endpoint
/// answered `200 … already up to date`* — the status code was not merely
/// insufficient evidence, it was the wrong answer.
///
/// The paired positive is [`a_configured_remote_still_fetches`]: the same
/// target, configured, still fetches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfigured_remote_is_never_fetched_from() {
    let (dir, repo) = seeded_repo();
    let target = bare_target_inside(dir.path(), &repo, "ghost.git");
    assert!(
        target.join("HEAD").exists(),
        "the fixture target must exist"
    );
    assert!(
        !repo.join(".git/FETCH_HEAD").exists(),
        "the fixture must start with no FETCH_HEAD, or the assertion below \
         proves nothing"
    );

    let (status, body) = drive_fetch(&repo, "ghost.git").await;

    assert!(
        !repo.join(".git/FETCH_HEAD").exists(),
        "git fetch ran against an unconfigured remote: FETCH_HEAD is now {:?}. \
         The endpoint answered {status}: {body}",
        std::fs::read_to_string(repo.join(".git/FETCH_HEAD")).unwrap_or_default()
    );
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an unconfigured remote must be refused by the staleness gate: {body}"
    );
    assert!(
        body.contains("ghost.git") && body.contains("not configured"),
        "the refusal must name the remote it refused and why: {body}"
    );
    // The refusal is plain text, like every other `enforce_fresh` refusal
    // (the staleness 409 has always been). `handlers::fetch`'s `FetchError`
    // contract covers the refusals *that handler* makes itself — see the gap
    // note in ADR 0044.
    assert!(
        serde_json::from_str::<FetchError>(&body).is_err(),
        "if this ever becomes a FetchError, the assertion above is the one to \
         tighten, not to delete: {body}"
    );
}

/// **The paired positive: a legitimately configured remote still fetches.**
///
/// Without this, every assertion above is satisfied by a server that refuses
/// all fetches. Same fixture, same target, same pipeline — the only difference
/// is `git remote add`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_remote_still_fetches() {
    let (dir, repo) = seeded_repo();
    let target = bare_target_inside(dir.path(), &repo, "upstream.git");
    run(
        &repo,
        &["remote", "add", "origin", &target.display().to_string()],
    );

    let (status, body) = drive_fetch(&repo, "origin").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a configured fetch must work: {body}"
    );

    let success: git_vista_protocol::FetchSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        success
            .updated_refs
            .iter()
            .any(|u| u.ref_name == "refs/remotes/origin/main"),
        "the fetch must have created the remote-tracking ref: {:?}",
        success.updated_refs
    );
    assert!(
        repo.join(".git/refs/remotes/origin/main").exists()
            || std::fs::read_to_string(repo.join(".git/packed-refs"))
                .map(|s| s.contains("refs/remotes/origin/main"))
                .unwrap_or(false),
        "the repository itself must agree that the ref was created"
    );
}

// ---------------------------------------------------------------------------
// The same protection, on pull
// ---------------------------------------------------------------------------

/// **Pull inherits both halves**, which is why the fix belongs on this branch
/// rather than the pull one: `GitOperation::PullBranch` carries the identical
/// `Precondition::RemoteConfigured`, and its `remote` field is the identical
/// [`RemoteName`].
///
/// Pull's executor is still `501` on this branch (#230 wires it). That is what
/// makes this assertion meaningful rather than incidental: the refusal must
/// arrive from the **staleness gate** — a `409` — *before* `execute` is
/// reached at all. A `501` here would mean the guard is downstream of the
/// dispatch and would evaporate the moment pull's executor lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_refuses_an_unconfigured_remote_before_reaching_its_executor() {
    let (dir, repo) = seeded_repo();
    bare_target_inside(dir.path(), &repo, "ghost.git");

    let (status, body) = super::plan_and_execute_in(
        &repo,
        None,
        tokens(),
        GitOperation::PullBranch {
            remote: RemoteName::new("ghost.git").unwrap(),
            branch: BranchName::new("main").unwrap(),
            strategy: MergeStrategy::Merge,
        },
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "pull must be refused by the same gate fetch is, before its executor \
         is dispatched to (a 501 here would mean the guard sits downstream of \
         the dispatch and would vanish when #230 lands): {body}"
    );
    assert!(
        body.contains("ghost.git"),
        "the refusal must name the remote: {body}"
    );
    assert!(
        !repo.join(".git/FETCH_HEAD").exists(),
        "pull's fetch half must not have run"
    );
}

// ---------------------------------------------------------------------------
// The type-level half, at the wire boundary
// ---------------------------------------------------------------------------

/// Every URL and path shape a client could put in a `remote` field is refused
/// by [`RemoteName`] itself — so no consumer of the type, present or future,
/// can be pointed at one, whether or not it carries a `RemoteConfigured`
/// precondition.
///
/// Deserialization is asserted alongside `new` because that is the path wire
/// input actually takes for `PullBranch`/`PushBranch`/`PushTag`/
/// `DeleteRemoteTag`, whose `remote` fields are typed `RemoteName` inside the
/// `GitOperation` a plan carries.
#[test]
fn remote_name_refuses_every_url_and_path_shape() {
    for hostile in [
        "https://attacker.example/r.git",
        "http://127.0.0.1:9418/r.git",
        "git://127.0.0.1:9418/r.git",
        "ssh://git@attacker.example/r.git",
        "git@attacker.example:r.git",
        "file:///etc",
        "/etc/passwd",
        "./ghost.git",
        "../sibling.git",
        "~/private.git",
        "ext::sh -c 'curl attacker.example'",
        "-u",
        "--upload-pack=/bin/sh",
        "",
        "   ",
        ".",
        "..",
        "a..b",
        ".hidden",
        "has space",
        "semi;colon",
        "new\nline",
    ] {
        assert!(
            RemoteName::new(hostile).is_err(),
            "RemoteName must refuse {hostile:?}"
        );
        assert!(
            serde_json::from_str::<RemoteName>(&serde_json::to_string(hostile).unwrap()).is_err(),
            "the wire boundary must refuse {hostile:?} too"
        );
    }
}

/// The census of preconditions that refuse when they were already false at
/// build time is exactly `{RemoteConfigured}`.
///
/// `super::refuses_when_unmet_at_build`'s match is exhaustive, so a new
/// [`Precondition`] variant cannot compile without an arm — but nothing in the
/// compiler stops someone putting a new variant on the `true` side, and a
/// `true` there means "the executor for every operation carrying this
/// precondition does **not** refuse when it is unmet". That claim is about
/// executors, not about types, so it is pinned by hand and widening it is a
/// deliberate edit to this list.
///
/// The paired negative is `SeedRecorded`, which sits on the other side and is
/// the reason the classification is per-precondition rather than blanket:
/// `exec_reset_test_repo` genuinely re-reads the seed and 404s, and
/// `contract_suite::review_window_seed_drift_fails_closed_with_the_never_recorded_refusal`
/// asserts that exact 404. Flipping `SeedRecorded` to `true` would replace a
/// real, tested executor refusal with a paraphrase from the gate — and that
/// test would catch it.
#[test]
fn only_remote_configured_refuses_when_unmet_at_build() {
    let all = every_precondition_variant();
    let refusing: Vec<&Precondition> = all
        .iter()
        .filter(|p| super::refuses_when_unmet_at_build(p))
        .collect();
    assert_eq!(
        refusing.len(),
        1,
        "the census changed — a `true` arm asserts that no executor refuses \
         when this precondition is unmet, which is a claim about every \
         executor that carries it: {refusing:?}"
    );
    assert!(
        matches!(refusing[0], Precondition::RemoteConfigured { .. }),
        "only RemoteConfigured has no downstream guard today, got {:?}",
        refusing[0]
    );
}

/// One of every [`Precondition`] variant.
///
/// The `match` at the bottom is an exhaustiveness anchor, not dead code: it
/// stops compiling the moment a variant is added, so the list above cannot
/// silently fall behind the enum the census test walks.
fn every_precondition_variant() -> Vec<Precondition> {
    let zeros = git_vista_protocol::CommitOid::new("0".repeat(40)).unwrap();
    let all = vec![
        Precondition::RefAt {
            ref_name: git_vista_protocol::RefName::new("refs/heads/main").unwrap(),
            oid: zeros,
        },
        Precondition::RefExists {
            ref_name: git_vista_protocol::RefName::new("refs/heads/main").unwrap(),
        },
        Precondition::RefAbsent {
            ref_name: git_vista_protocol::RefName::new("refs/heads/new").unwrap(),
        },
        Precondition::BranchCheckedOut {
            branch: BranchName::new("main").unwrap(),
        },
        Precondition::BranchNotCheckedOut {
            branch: BranchName::new("main").unwrap(),
        },
        Precondition::CleanWorktree,
        Precondition::RemoteConfigured {
            remote: RemoteName::new("origin").unwrap(),
        },
        Precondition::SeedRecorded,
    ];
    for precondition in &all {
        match precondition {
            Precondition::RefAt { .. }
            | Precondition::RefExists { .. }
            | Precondition::RefAbsent { .. }
            | Precondition::BranchCheckedOut { .. }
            | Precondition::BranchNotCheckedOut { .. }
            | Precondition::CleanWorktree
            | Precondition::RemoteConfigured { .. }
            | Precondition::SeedRecorded => {}
        }
    }
    all
}

/// The paired positive for the validator: every shape a real remote name takes
/// still passes. A validator that refused everything would satisfy the test
/// above and break every fetch on the machine.
#[test]
fn remote_name_still_accepts_real_remote_names() {
    for good in [
        "origin",
        "upstream",
        "fork2",
        "my-remote",
        "my_remote",
        "remote.v2",
        "ghost.git",
        "A",
    ] {
        assert!(
            RemoteName::new(good).is_ok(),
            "RemoteName must accept {good:?}"
        );
    }
}
