//! The M1.06 contract suite (#146, closes the loop on #59): end-to-end proof
//! that every write action flows **build plan → validate → execute** through
//! the shared planner, for every operation kind, and that the pipeline's
//! refusals actually protect the repository.
//!
//! Four layers, mapped to the parent issue's acceptance criteria:
//!
//!  1. **Pipeline integration** — one test per [`GitOperation`] variant runs
//!     the real `build_plan → validate → enforce_fresh → execute` composition
//!     against a real temporary repository and asserts the mutation landed.
//!     [`covered_by`] matches exhaustively over the enum, so adding a new
//!     variant refuses to compile until it gets a pipeline test. (One
//!     exception so far: `AmendCommit`, M2.19a #222, ships no execution —
//!     its pipeline test asserts the *stub's* refusal and that the
//!     repository stayed untouched, the honest version of this layer's
//!     claim until #223 wires real execution in.)
//!  2. **Single-funnel proof** — a source-level test walks the router's POST
//!     table and every git-write handler, asserting each one reaches
//!     [`plan_and_execute`] (directly or through its named local helper) and
//!     that the route table itself can't grow a write endpoint silently.
//!     The argv tripwire (`argv_boundary`, #144) already pins that no other
//!     code in these crates spawns processes at all; together they close both
//!     halves — every write goes *in* through the planner, and nothing
//!     mutates *outside* it.
//!  3. **Race/tamper/expiry, end-to-end** — #145's unit tests pin each
//!     refusal at the `enforce_fresh`/`validate` seam; here the same attacks
//!     run through the full pipeline and additionally assert the repository
//!     was **not mutated** — the gate doesn't just say no, it protects.
//!  4. **Adversarial wire fixtures** — `argv_boundary`'s serde and wire-level
//!     suites (hostile bodies through real session/CSRF middleware) are the
//!     browser-facing half; they run in this same `cargo test` invocation and
//!     the CI step names all three modules together.
//!
//! Tests here run the pipeline stages directly (with injected tokens) rather
//! than [`plan_and_execute`], which reads the process-global selection —
//! `state::CURRENT` is set-once per process and owned by `state`'s own test
//! (see the invariant note there). The handler→planner funnel that
//! `plan_and_execute` adds on top is exactly what layer 2 proves.

use super::*;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

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

/// `git <args…>` in `repo`, returning trimmed stdout; asserts success.
fn out(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {repo:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A fresh repository on branch `main` with one committed file (`a.txt`) and
/// a clean working tree — same shape as `planner::tests::seeded_repo`.
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

fn tip(repo: &Path, rev: &str) -> String {
    out(repo, &["rev-parse", rev])
}

fn branch(name: &str) -> BranchName {
    BranchName::new(name).unwrap()
}

fn oid(s: &str) -> CommitOid {
    CommitOid::new(s).unwrap()
}

fn message(s: &str) -> CommitMessage {
    CommitMessage::new(s).unwrap()
}

fn wpath(s: &str) -> WorktreePath {
    WorktreePath::new(s).unwrap()
}

/// The full planner pipeline, driven through the real entry point with the
/// process-global selection injected: guard → busy check → build → validate →
/// staleness gate → execute. What every test in layer 1 and 3 drives.
///
/// Since #60 this calls [`plan_and_execute_in`] rather than re-composing the
/// stages, so these tests exercise the production composition — mutation guard
/// included — instead of a copy of it that could drift.
async fn pipeline(repo: &Path, op: GitOperation) -> (StatusCode, String) {
    plan_and_execute_in(repo, None, tokens(), op).await
}

/// The pipeline from `validate` on, for tests that tamper with a built plan
/// or let the repository move between build and execution.
async fn run_prebuilt(repo: &Path, plan: Plan, observed: Observed) -> (StatusCode, String) {
    if let Err(refused) = validate(&plan) {
        return refused;
    }
    if let Err(refused) = enforce_fresh(repo, &plan, &observed).await {
        return refused;
    }
    execute(repo, plan, observed).await
}

fn assert_ok(status: StatusCode, body: &str) {
    assert_eq!(
        status,
        StatusCode::OK,
        "expected success, got {status}: {body}"
    );
}

// ---------------------------------------------------------------------------
// Layer 1 — build → validate → execute for every operation kind
// ---------------------------------------------------------------------------

/// The compile-time coverage guard: every [`GitOperation`] variant names the
/// pipeline test that drives it end-to-end. **No wildcard arm, on purpose** —
/// a new variant fails this match at compile time until it's added here *and*
/// given a real pipeline test below.
fn covered_by(op: &GitOperation) -> &'static str {
    match op {
        GitOperation::CreateBranch { .. } => "create_branch_executes_through_the_pipeline",
        GitOperation::CommitOnHead { .. } => "commit_on_head_executes_through_the_pipeline",
        GitOperation::EmptyCommitOnBranch { .. } => {
            "empty_commit_on_branch_executes_through_the_pipeline"
        }
        GitOperation::StageAll => "stage_all_executes_through_the_pipeline",
        GitOperation::UnstageAll => "unstage_all_executes_through_the_pipeline",
        GitOperation::CheckoutBranch { .. } => "checkout_branch_executes_through_the_pipeline",
        GitOperation::MergeBranch { .. } => "merge_branch_executes_through_the_pipeline",
        GitOperation::PushBranch { .. } => "push_branch_executes_through_the_pipeline",
        GitOperation::DeleteBranch { .. } => "delete_branch_executes_through_the_pipeline",
        GitOperation::ForceDeleteBranch { .. } => {
            "force_delete_branch_executes_through_the_pipeline"
        }
        GitOperation::RebaseOntoBase { .. } => "rebase_onto_base_executes_through_the_pipeline",
        GitOperation::RestoreBranch { .. } => "restore_branch_executes_through_the_pipeline",
        GitOperation::ResetBranch { .. } => "reset_branch_executes_through_the_pipeline",
        GitOperation::RevertCommit { .. } => "revert_commit_executes_through_the_pipeline",
        GitOperation::ResetTestRepo => "reset_test_repo_executes_through_the_pipeline",
        GitOperation::StageSelection { .. } => "stage_selection_executes_through_the_pipeline",
        GitOperation::DiscardTrackedPaths { .. } => {
            "discard_tracked_paths_executes_through_the_pipeline"
        }
        GitOperation::DeleteUntrackedPaths { .. } => {
            "delete_untracked_paths_executes_through_the_pipeline"
        }
        GitOperation::AmendCommit { .. } => "amend_commit_executes_through_the_pipeline",
    }
}

/// One sample per variant, each mapped through [`covered_by`]: the mapping
/// stays total (compile-time) and injective (here) — no two variants may
/// share a pipeline test.
#[test]
fn every_operation_kind_names_a_distinct_pipeline_test() {
    let zeros = "0".repeat(40);
    let samples = vec![
        GitOperation::CreateBranch {
            name: branch("b"),
            at: oid(&zeros),
        },
        GitOperation::CommitOnHead {
            message: message("m"),
            allow_empty: false,
        },
        GitOperation::EmptyCommitOnBranch {
            branch: branch("b"),
            message: message("m"),
            expected_tip: oid(&zeros),
        },
        GitOperation::StageAll,
        GitOperation::UnstageAll,
        GitOperation::CheckoutBranch {
            branch: branch("b"),
        },
        GitOperation::MergeBranch {
            branch: branch("b"),
        },
        GitOperation::PushBranch {
            branch: branch("b"),
            remote: RemoteName::new("origin").unwrap(),
        },
        GitOperation::DeleteBranch {
            branch: branch("b"),
        },
        GitOperation::ForceDeleteBranch {
            branch: branch("b"),
        },
        GitOperation::RebaseOntoBase {
            base: RefName::new("main").unwrap(),
        },
        GitOperation::RestoreBranch {
            name: branch("b"),
            tip: oid(&zeros),
        },
        GitOperation::ResetBranch {
            branch: branch("b"),
            to: oid(&zeros),
            expected_tip: oid(&zeros),
        },
        GitOperation::RevertCommit {
            commit: oid(&zeros),
        },
        GitOperation::ResetTestRepo,
        GitOperation::StageSelection {
            direction: git_vista_protocol::StageDirection::Stage,
            expected_diff_generation: git_vista_protocol::GenerationToken::new("diff-v1:x")
                .unwrap(),
            patch: String::new(),
            whole_files: vec!["a.txt".to_string()],
        },
        GitOperation::DiscardTrackedPaths {
            paths: vec![wpath("a.txt")],
        },
        GitOperation::DeleteUntrackedPaths {
            paths: vec![wpath("a.txt")],
        },
        GitOperation::AmendCommit {
            message: message("m"),
            expected_tip: oid(&zeros),
            allow_empty: false,
        },
    ];
    let names: Vec<&str> = samples.iter().map(covered_by).collect();
    let mut deduped = names.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        names.len(),
        deduped.len(),
        "two GitOperation variants claim the same pipeline test"
    );
    // The suite's own source must define every named test as a *live*
    // `#[tokio::test]` — the attribute is part of the needle, so a test
    // demoted to a plain helper (or commented out along with its attribute)
    // fails the guard, not just a missing name.
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/planner/contract_suite.rs"),
    )
    .unwrap();
    for name in names {
        assert!(
            src.contains(&format!("#[tokio::test]\nasync fn {name}(")),
            "covered_by names ‘{name}’ but no live #[tokio::test] with that name exists"
        );
    }
}

#[tokio::test]
async fn create_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let at = tip(&repo, "HEAD");
    let (status, body) = pipeline(
        &repo,
        GitOperation::CreateBranch {
            name: branch("feature"),
            at: oid(&at),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "feature"), at);
}

#[tokio::test]
async fn commit_on_head_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::CommitOnHead {
            message: message("add b"),
            allow_empty: false,
        },
    )
    .await;
    assert_ok(status, &body);
    assert_ne!(tip(&repo, "HEAD"), before);
    assert_eq!(out(&repo, &["log", "-1", "--format=%s"]), "add b");
}

#[tokio::test]
async fn empty_commit_on_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let base = tip(&repo, "HEAD");
    run(&repo, &["branch", "side"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::EmptyCommitOnBranch {
            branch: branch("side"),
            message: message("note"),
            expected_tip: oid(&base),
        },
    )
    .await;
    assert_ok(status, &body);
    // Side advanced by exactly one commit; HEAD (main) never moved.
    assert_eq!(tip(&repo, "side^"), base);
    assert_eq!(out(&repo, &["log", "-1", "--format=%s", "side"]), "note");
    assert_eq!(tip(&repo, "HEAD"), base);
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// The executor's own compare-and-swap for [`GitOperation::EmptyCommitOnBranch`]:
/// an `expected_tip` that was *already stale at build time* slips past the
/// staleness gate (the precondition never held, so it isn't re-enforced) and
/// must be refused by `git update-ref`'s old-value check — the branch does
/// not move. Pins the CAS argv itself, which no generation check covers.
#[tokio::test]
async fn empty_commit_on_branch_refuses_a_stale_expected_tip() {
    let (_dir, repo) = seeded_repo();
    let old = tip(&repo, "HEAD");
    run(&repo, &["branch", "side"]);
    // Side moves on past the hint the operation was built from.
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["checkout", "-q", "side"]);
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "side moved"]);
    run(&repo, &["checkout", "-q", "main"]);
    let moved = tip(&repo, "side");
    let (status, why) = pipeline(
        &repo,
        GitOperation::EmptyCommitOnBranch {
            branch: branch("side"),
            message: message("too late"),
            expected_tip: oid(&old),
        },
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert_eq!(
        tip(&repo, "side"),
        moved,
        "the refused empty commit must not move the branch"
    );
}

#[tokio::test]
async fn stage_all_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    let (status, body) = pipeline(&repo, GitOperation::StageAll).await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["diff", "--cached", "--name-only"]), "b.txt");
}

#[tokio::test]
async fn stage_selection_executes_through_the_pipeline() {
    // The built form (patch text + pathspecs) rides the full production
    // pipeline — plan build, mutation guard, staleness gate, executor. The
    // hunk-precision proof lives in `planner::tests`; this is the funnel leg.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    run(&repo, &["add", "c.txt"]);
    run(&repo, &["commit", "-q", "-m", "second file"]);
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    std::fs::write(repo.join("c.txt"), "c changed\n").unwrap();
    // Untrimmed capture: a unified diff's final newline is load-bearing.
    let patch_out = std::process::Command::new("git")
        .args(["diff", "--no-color", "--no-textconv", "--", "c.txt"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(patch_out.status.success());
    let patch = String::from_utf8(patch_out.stdout).unwrap();
    let live = crate::handlers::read::staging_diff_for_repo(
        &repo,
        git_vista_protocol::StageDirection::Stage,
    )
    .await
    .unwrap();
    let (status, body) = pipeline(
        &repo,
        GitOperation::StageSelection {
            direction: git_vista_protocol::StageDirection::Stage,
            expected_diff_generation: live.generation,
            patch,
            whole_files: vec!["a.txt".to_string()],
        },
    )
    .await;
    assert_ok(status, &body);
    let staged = out(&repo, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("a.txt"), "{staged}");
    assert!(staged.contains("c.txt"), "{staged}");
}

#[tokio::test]
async fn unstage_all_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    let (status, body) = pipeline(&repo, GitOperation::UnstageAll).await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["diff", "--cached", "--name-only"]), "");
    assert!(repo.join("b.txt").exists(), "unstage must keep the edit");
}

#[tokio::test]
async fn checkout_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::CheckoutBranch {
            branch: branch("side"),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["symbolic-ref", "--short", "HEAD"]), "side");
}

#[tokio::test]
async fn merge_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "side work"]);
    run(&repo, &["checkout", "-q", "main"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::MergeBranch {
            branch: branch("side"),
        },
    )
    .await;
    assert_ok(status, &body);
    // The fast-forward's full effect: main reached side's tip, HEAD stayed on
    // main, and the *working tree* was updated too (a bare ref overwrite
    // would leave s.txt missing and the tree dirty against the new tip).
    assert_eq!(tip(&repo, "main"), tip(&repo, "side"));
    assert_eq!(out(&repo, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert!(
        repo.join("s.txt").exists(),
        "merge must update the worktree"
    );
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// Kills the fixture `git daemon` even when an assertion panics first — a
/// leaked daemon would squat port 9418 and poison every later run.
///
/// It must kill the **process group**, not the child: `/usr/bin/git` forks
/// `git-daemon` and exits (a live daemon shows PPID 1), so the `Child` this
/// holds is only the short-lived wrapper and `Child::kill` would strike a
/// corpse while the real daemon lives on. The spawn below puts the wrapper in
/// its own group (`process_group(0)`), the daemon inherits it, and the
/// wrapper's un-reaped zombie keeps the group id from being recycled until the
/// `wait` here releases it.
struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn push_branch_executes_through_the_pipeline() {
    let (dir, repo) = seeded_repo();
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare"]);

    // The push crosses a real network transport, not the filesystem. Under the
    // sandbox (Task 6) a filesystem-path remote is dead twice over: a path
    // outside the grant is denied outright, and even a *granted* path fails,
    // because receive-pack's quarantine migration is a cross-directory rename
    // and the shim deliberately withholds `LANDLOCK_ACCESS_FS_REFER` (the
    // kernel then reports EXDEV, git says "unable to migrate objects"). That
    // is the intended posture — production remotes are URLs, where
    // receive-pack runs on the far side, outside the pusher's sandbox. So this
    // fixture serves the bare remote over git:// on loopback: 9418 is in
    // `DEFAULT_GIT_PORTS`, the Network tier's Landlock connect grant covers
    // it, and the daemon (spawned unsandboxed by the test) does the receiving.
    //
    // That one port is contended, and not only by other processes. It is the
    // only unprivileged entry in `DEFAULT_GIT_PORTS`, so it is the only port a
    // Network-tier Landlock connect grant covers, which means the sandbox
    // escape battery in this same test binary cannot move off it either:
    // `escape_suite::strict_listener_denied` needs a listener there and
    // `strict_tcp_bind_denied` needs it unbound. `crate::test_ports` is the
    // arbiter — a process-wide claim every holder takes, released only once its
    // listener or daemon is really gone. Acquiring it here both excludes those
    // tests and waits out a *stale* daemon leaked by a run that was SIGKILLed
    // before the guard's `Drop` (such a daemon would otherwise pass the
    // readiness probe below while serving a dead base path, and every push would
    // fail with a baffling "connection reset"). The claim is taken before the
    // daemon is spawned and dropped at the end of the test, after `DaemonGuard`
    // has killed it.
    let _port_claim = crate::test_ports::PortClaim::acquire();
    // Read from the claim, never re-typed: a daemon on a different port than the
    // one claimed would reintroduce exactly the collision the claim prevents.
    let port = crate::test_ports::PortClaim::PORT;
    // `process_group(0)` — see `DaemonGuard`. Stdio all detached: an inherited
    // stdout pipe would keep any harness capturing this test's output alive
    // for as long as the daemon lives, turning a daemon leak into a hang.
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
    let _daemon = DaemonGuard(daemon);
    let ready = (0..50).any(|_| {
        std::net::TcpStream::connect(("127.0.0.1", port))
            .map(|_| true)
            .unwrap_or_else(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
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
    let (status, body) = pipeline(
        &repo,
        GitOperation::PushBranch {
            branch: branch("main"),
            remote: RemoteName::new("origin").unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(tip(&remote, "main"), tip(&repo, "main"));
}

#[tokio::test]
async fn delete_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]); // fully merged — the safe delete allows it
    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteBranch {
            branch: branch("side"),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["branch", "--list", "side"]), "");
}

/// What makes [`GitOperation::DeleteBranch`] the *safe* delete: driven at an
/// unmerged branch it must refuse (git's own `-d` guard, forwarded) and leave
/// the branch standing — the behavioral difference from the force variant,
/// pinned so a regression to `-D`-for-both can't stay green.
#[tokio::test]
async fn delete_branch_refuses_unmerged_work() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "only on side"]);
    run(&repo, &["checkout", "-q", "main"]);
    let (status, why) = pipeline(
        &repo,
        GitOperation::DeleteBranch {
            branch: branch("side"),
        },
    )
    .await;
    assert_ne!(status, StatusCode::OK, "safe delete must refuse: {why}");
    assert!(why.contains("not fully merged"), "{why}");
    assert_ne!(
        out(&repo, &["branch", "--list", "side"]),
        "",
        "the refused safe delete must leave the branch standing"
    );
}

#[tokio::test]
async fn force_delete_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "only on side"]);
    run(&repo, &["checkout", "-q", "main"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::ForceDeleteBranch {
            branch: branch("side"),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["branch", "--list", "side"]), "");
}

#[tokio::test]
async fn rebase_onto_base_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    // Side diverges from main's initial commit, then main advances.
    run(&repo, &["branch", "side"]);
    std::fs::write(repo.join("m.txt"), "m\n").unwrap();
    run(&repo, &["add", "m.txt"]);
    run(&repo, &["commit", "-q", "-m", "main advances"]);
    run(&repo, &["checkout", "-q", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "side work"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::RebaseOntoBase {
            base: RefName::new("main").unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);
    // A true rebase, not a merge and not a ref overwrite: side's own commit
    // was replayed *on top of* main's tip (its parent is exactly main), its
    // subject survived, and HEAD stayed on side.
    assert_eq!(tip(&repo, "side^"), tip(&repo, "main"));
    assert_eq!(
        out(&repo, &["log", "-1", "--format=%s", "side"]),
        "side work"
    );
    assert_eq!(out(&repo, &["symbolic-ref", "--short", "HEAD"]), "side");
}

#[tokio::test]
async fn restore_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]);
    let recorded = tip(&repo, "side");
    run(&repo, &["branch", "-D", "side"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::RestoreBranch {
            name: branch("side"),
            tip: oid(&recorded),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "side"), recorded);
}

#[tokio::test]
async fn reset_branch_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let base = tip(&repo, "HEAD");
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "to be undone"]);
    let moved = tip(&repo, "side");
    run(&repo, &["checkout", "-q", "main"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::ResetBranch {
            branch: branch("side"),
            to: oid(&base),
            expected_tip: oid(&moved),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "side"), base);
}

/// The other of `exec_reset_branch`'s two argv paths: the branch under reset
/// *is* checked out with a clean worktree, so the executor runs `git reset
/// --hard` (moving ref, index and working tree together) rather than
/// `git branch -f`. The worktree assertion is what tells the two apart.
#[tokio::test]
async fn reset_branch_when_checked_out_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let base = tip(&repo, "HEAD");
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "to be undone"]);
    let moved = tip(&repo, "HEAD");
    let (status, body) = pipeline(
        &repo,
        GitOperation::ResetBranch {
            branch: branch("main"),
            to: oid(&base),
            expected_tip: oid(&moved),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "main"), base);
    assert!(
        !repo.join("s.txt").exists(),
        "reset --hard must rewind the working tree, not just the ref"
    );
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// [`GitOperation::ResetBranch`]'s compare-and-swap: a hint whose
/// `expected_tip` was already stale at build time (the branch moved after the
/// undo was offered) is refused by the executor's legacy guard — the exact
/// wording the un-migrated handler used — and the branch stays where it is.
#[tokio::test]
async fn reset_branch_refuses_a_stale_expected_tip() {
    let (_dir, repo) = seeded_repo();
    let base = tip(&repo, "HEAD");
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("s.txt"), "s\n").unwrap();
    run(&repo, &["add", "s.txt"]);
    run(&repo, &["commit", "-q", "-m", "first move"]);
    let stale_hint = tip(&repo, "side");
    std::fs::write(repo.join("t.txt"), "t\n").unwrap();
    run(&repo, &["add", "t.txt"]);
    run(&repo, &["commit", "-q", "-m", "moved again"]);
    let live = tip(&repo, "side");
    run(&repo, &["checkout", "-q", "main"]);
    let (status, why) = pipeline(
        &repo,
        GitOperation::ResetBranch {
            branch: branch("side"),
            to: oid(&base),
            expected_tip: oid(&stale_hint),
        },
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(
        why.contains("has moved since this undo was offered"),
        "{why}"
    );
    assert_eq!(
        tip(&repo, "side"),
        live,
        "the refused reset must not move the branch"
    );
}

#[tokio::test]
async fn revert_commit_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "change a"]);
    let bad = tip(&repo, "HEAD");
    let (status, body) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&bad) }).await;
    assert_ok(status, &body);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert!(out(&repo, &["log", "-1", "--format=%s"]).starts_with("Revert"));
}

#[tokio::test]
async fn reset_test_repo_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let seeded = tip(&repo, "HEAD");
    // Record the seed the way `gv --seed` does: refs + head under
    // .git/git-vista/ (the bundle is optional — objects are still present).
    let state = repo.join(".git/git-vista");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("seed-refs"), format!("{seeded} main\n")).unwrap();
    std::fs::write(state.join("seed-head"), "main\n").unwrap();
    // Drift past the seed: a new commit on main and a stray branch.
    std::fs::write(repo.join("junk.txt"), "j\n").unwrap();
    run(&repo, &["add", "junk.txt"]);
    run(&repo, &["commit", "-q", "-m", "past the seed"]);
    run(&repo, &["branch", "stray"]);

    let (status, body) = pipeline(&repo, GitOperation::ResetTestRepo).await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "main"), seeded);
    assert_eq!(out(&repo, &["branch", "--list", "stray"]), "");
    assert_eq!(out(&repo, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

// --- #219 (M2.18a): discard tracked-path changes / delete untracked paths --

#[tokio::test]
async fn discard_tracked_paths_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
    let (status, body) = pipeline(
        &repo,
        GitOperation::DiscardTrackedPaths {
            paths: vec![wpath("a.txt")],
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn delete_untracked_paths_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("scratch.txt"), "junk\n").unwrap();
    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteUntrackedPaths {
            paths: vec![wpath("scratch.txt")],
        },
    )
    .await;
    assert_ok(status, &body);
    assert!(!repo.join("scratch.txt").exists());
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// The race guard, in isolation: [`exec_delete_untracked_paths`] refuses a
/// path that was previewed as untracked but has since been staged (a
/// concurrent `git add` outside this app's own serialization — exactly the
/// drift #219's race guard exists to catch), called directly rather than
/// through the full pipeline so this pins the guard's own refusal logic
/// deterministically, not merely as an emergent property of the generic
/// whole-repository staleness gate (`enforce_fresh`) that also happens to
/// cover the same drift.
#[tokio::test]
async fn exec_delete_untracked_paths_refuses_a_path_that_changed_since_it_was_previewed() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("scratch.txt"), "junk\n").unwrap();
    run(&repo, &["add", "scratch.txt"]);
    let (status, why) =
        exec_delete_untracked_paths(&repo, NetworkNeed::Local, &[wpath("scratch.txt")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("scratch.txt"), "{why}");
    // Refused, not silently no-op'd: the file is exactly what `git add` left
    // it as — staged, not deleted.
    assert!(repo.join("scratch.txt").exists());
    assert_eq!(
        out(&repo, &["diff", "--cached", "--name-only"]),
        "scratch.txt"
    );
}

/// The discard-side twin of the test above: a path previewed as
/// tracked-and-dirty that has since gone clean (reverted by something
/// outside this app's serialization) is refused, not silently no-op'd.
#[tokio::test]
async fn exec_discard_tracked_paths_refuses_a_path_that_changed_since_it_was_previewed() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
    // Reverted by something other than this operation before it runs.
    run(&repo, &["checkout", "--", "a.txt"]);
    let (status, why) =
        exec_discard_tracked_paths(&repo, NetworkNeed::Local, &[wpath("a.txt")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("a.txt"), "{why}");
}

/// The full-pipeline shape of the race (#219 acceptance): build a plan
/// naming two untracked paths, then let ONE of them get staged before
/// execution — the whole batch must refuse, and even the path that never
/// drifted (`also.txt`) must be left untouched. That second assertion is
/// what proves "refuse, don't partially apply" rather than merely "refuse
/// eventually".
#[tokio::test]
async fn a_raced_delete_untracked_paths_is_refused_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
    std::fs::write(repo.join("also.txt"), "also\n").unwrap();
    let op = GitOperation::DeleteUntrackedPaths {
        paths: vec![wpath("keep.txt"), wpath("also.txt")],
    };
    let (plan, observed) = build_plan(&repo, op, tokens()).await;
    // The race: `keep.txt` gets staged between build and execute.
    run(&repo, &["add", "keep.txt"]);
    let (status, why) = run_prebuilt(&repo, plan, observed).await;
    assert_ne!(status, StatusCode::OK, "{why}");
    assert!(repo.join("keep.txt").exists());
    assert!(repo.join("also.txt").exists());
}

/// The symlink-containment guard, proven against a **real** symlink whose
/// resolved target sits outside the worktree — not a mocked path string.
/// `delete_untracked_paths` on an untracked path that is itself a symlink
/// pointing outside the worktree must refuse, and the target must be left
/// completely untouched.
#[tokio::test]
async fn delete_untracked_paths_refuses_a_real_symlink_escaping_the_worktree() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside content\n").unwrap();

    let (_dir, repo) = seeded_repo();
    let link = repo.join("evil-link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    // Genuinely untracked, and genuinely a symlink escaping the worktree —
    // both facts asserted before the guard is ever exercised.
    assert_eq!(out(&repo, &["status", "--porcelain"]), "?? evil-link");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    let (status, why) =
        exec_delete_untracked_paths(&repo, NetworkNeed::Local, &[wpath("evil-link")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("evil-link"), "{why}");
    assert!(secret.exists(), "the outside target must be untouched");
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "outside content\n"
    );
    // The symlink dirent itself is untouched too — `git clean` never ran.
    assert!(std::fs::symlink_metadata(&link).is_ok());
}

/// The discard-side twin: a tracked, uncommitted-edit path whose on-disk
/// entry is a real symlink resolving outside the worktree must also be
/// refused before `git checkout` ever runs.
#[tokio::test]
async fn discard_tracked_paths_refuses_a_real_symlink_escaping_the_worktree() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside content\n").unwrap();

    let (_dir, repo) = seeded_repo();
    // Track a symlink pointing at a harmless in-repo target first...
    let link = repo.join("link.txt");
    std::os::unix::fs::symlink(repo.join("a.txt"), &link).unwrap();
    run(&repo, &["add", "link.txt"]);
    run(&repo, &["commit", "-q", "-m", "add symlink"]);
    // ...then, without staging, repoint it outside the worktree: a real
    // uncommitted edit (the symlink's own target changed) — exactly what
    // DiscardTrackedPaths is being asked to discard.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    assert_ne!(out(&repo, &["status", "--porcelain"]), "");

    let (status, why) =
        exec_discard_tracked_paths(&repo, NetworkNeed::Local, &[wpath("link.txt")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(secret.exists(), "the outside target must be untouched");
    // The symlink must still point outside — `git checkout` never ran, so it
    // was never reverted to its committed (safe) target either.
    assert_eq!(std::fs::read_link(&link).unwrap(), secret);
}

/// Recovery-language honesty (#219 acceptance): `DeleteUntrackedPaths`'s
/// response and journal text must never sound recoverable — a regression
/// guard on the STRING CONTENT, not just the `RecoveryStrategy::Irrecoverable`
/// tag, so a future edit that quietly softens the wording fails loudly here.
#[tokio::test]
async fn delete_untracked_paths_text_never_sounds_recoverable() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("scratch.txt"), "junk\n").unwrap();
    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteUntrackedPaths {
            paths: vec![wpath("scratch.txt")],
        },
    )
    .await;
    assert_ok(status, &body);
    let body_lower = body.to_lowercase();
    for forbidden in ["undo", "restore", "recover"] {
        assert!(
            !body_lower.contains(forbidden),
            "response text must not sound recoverable (found {forbidden:?}): {body}"
        );
    }
    let journaled = crate::journal::read_all(&repo);
    let entry = journaled
        .last()
        .expect("the delete must have journaled an event");
    let summary_lower = entry.summary.to_lowercase();
    for forbidden in ["undo", "restore", "recover"] {
        assert!(
            !summary_lower.contains(forbidden),
            "journal text must not sound recoverable (found {forbidden:?}): {}",
            entry.summary
        );
    }
    // The exact honest wording is present, not merely "no forbidden words".
    assert!(entry.summary.contains("permanently"), "{}", entry.summary);
}

/// The tracked-discard sibling never implies more recoverability than
/// [`RecoveryStrategy::Irrecoverable`] actually offers: this text is allowed
/// to say a qualified "recoverable", but only the narrow, true claim (staged
/// content survives until `git gc`) — never a blanket "this can be undone".
#[tokio::test]
async fn discard_tracked_paths_text_states_the_qualified_recovery_story() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
    let (status, body) = pipeline(
        &repo,
        GitOperation::DiscardTrackedPaths {
            paths: vec![wpath("a.txt")],
        },
    )
    .await;
    assert_ok(status, &body);
    // The qualifier must be present — "staged" and "gc" — so the claim is
    // never a blanket, unqualified "this can be undone".
    assert!(body.contains("staged"), "{body}");
    assert!(body.contains("gc"), "{body}");
    let journaled = crate::journal::read_all(&repo);
    let entry = journaled
        .last()
        .expect("the discard must have journaled an event");
    assert!(entry.summary.contains("staged"), "{}", entry.summary);
    assert!(entry.summary.contains("gc"), "{}", entry.summary);
}

/// Review finding (blocker): a bare `git checkout -- <path>` is a no-op for
/// a path whose only difference is STAGED (index != HEAD, worktree ==
/// index) — verified empirically against real git before this fix — so the
/// pre-fix executor returned 200 and journaled "discarded" while the file
/// was left exactly as the user staged it. This drives a staged-only edit
/// (`git add` with no further worktree change) through the real executor and
/// asserts the content actually reverts to HEAD.
#[tokio::test]
async fn discard_tracked_paths_actually_reverts_a_staged_only_change() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "staged edit\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    assert_eq!(out(&repo, &["status", "--porcelain"]), "M  a.txt");

    let (status, body) =
        exec_discard_tracked_paths(&repo, NetworkNeed::Local, &[wpath("a.txt")]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        out(&repo, &["status", "--porcelain"]),
        "",
        "a staged-only change must actually revert, not silently survive"
    );
    let content = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    assert_eq!(content, "a\n", "content must be back to HEAD's version");
}

/// Same fix, the mixed case: a path both staged AND further edited
/// unstaged on top must fully revert to HEAD, discarding both layers in one
/// call — not just the unstaged layer bare `checkout --` would have reached.
#[tokio::test]
async fn discard_tracked_paths_reverts_both_staged_and_unstaged_layers() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "staged layer\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    std::fs::write(repo.join("a.txt"), "unstaged layer on top\n").unwrap();
    assert_eq!(out(&repo, &["status", "--porcelain"]), "MM a.txt");

    let (status, body) =
        exec_discard_tracked_paths(&repo, NetworkNeed::Local, &[wpath("a.txt")]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
}

/// Review finding (blocker): a `WorktreePath` naming a wholly-untracked
/// DIRECTORY passed every pre-fix guard and reached `git clean -f`, which
/// recursively deleted every file nested under it while the response
/// reported only the one requested entry. Both operations must refuse a
/// directory-shaped target outright now.
#[tokio::test]
async fn delete_untracked_paths_refuses_a_directory_rather_than_recursing_silently() {
    let (_dir, repo) = seeded_repo();
    std::fs::create_dir_all(repo.join("scratch_dir/nested")).unwrap();
    std::fs::write(repo.join("scratch_dir/one.txt"), "one\n").unwrap();
    std::fs::write(repo.join("scratch_dir/nested/two.txt"), "two\n").unwrap();
    // Exactly what a real `git status --porcelain=v2 -z` reports for a
    // wholly-untracked directory: one collapsed entry, trailing slash.
    assert_eq!(out(&repo, &["status", "--porcelain"]), "?? scratch_dir/");

    let (status, why) =
        exec_delete_untracked_paths(&repo, NetworkNeed::Local, &[wpath("scratch_dir")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(
        repo.join("scratch_dir/one.txt").exists(),
        "nothing nested may be touched by a refusal"
    );
    assert!(repo.join("scratch_dir/nested/two.txt").exists());

    // The exact porcelain spelling (trailing slash) must refuse identically
    // — this is the should-fix half of the same finding (a spurious 409 for
    // the unslashed form would have been the OTHER failure mode; directories
    // are never valid either way now, so the spelling stops mattering).
    let (status2, _) =
        exec_delete_untracked_paths(&repo, NetworkNeed::Local, &[wpath("scratch_dir/")]).await;
    assert_eq!(status2, StatusCode::CONFLICT);
}

/// The same directory refusal, `DiscardTrackedPaths` side — the guard is
/// shared code (`symlink_containment_guard`), but the review finding named
/// both operations explicitly, so both get their own regression proof.
#[tokio::test]
async fn discard_tracked_paths_refuses_a_directory_target() {
    let (_dir, repo) = seeded_repo();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "edited\n").unwrap();
    let (status, why) =
        exec_discard_tracked_paths(&repo, NetworkNeed::Local, &[wpath("src")]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
}

/// The logic [`partial_delete_report`] exists for (review finding):
/// a requested path that `git clean` silently skipped must never be folded
/// into a claimed full success. Driven against a real worktree — the real
/// race that produces this exact mismatch can't be deterministically timed in
/// a permanent test (the review that found this bug used an empirical
/// microsecond-scale stagger to land inside the window), but the honesty
/// property under test here doesn't depend on how the mismatch arose, only on
/// whether it's reported truthfully.
#[test]
fn partial_delete_report_flags_any_requested_path_git_clean_silently_skipped() {
    // The exact scenario the review demonstrated: 3 requested, one silently
    // skipped (since-tracked), git still exits as if fully successful. Here
    // that end state is built directly: x and z gone, y still on disk.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("y.txt"), "skipped\n").unwrap();
    let msg = observe_deletion(
        &repo,
        &["x.txt", "y.txt", "z.txt"],
        &["x.txt", "y.txt", "z.txt"],
    )
    .partial_refusal()
    .expect("a surviving requested path must never be silently folded into success");
    assert!(msg.contains("x.txt"), "{msg}");
    assert!(msg.contains("z.txt"), "{msg}");
    assert!(msg.contains("y.txt"), "{msg}");
    assert!(
        !msg.to_lowercase().contains("undo")
            && !msg.to_lowercase().contains("restore")
            && !msg.to_lowercase().contains("recover"),
        "a partial-failure message for an operation with no undo must not sound \
         reversible either: {msg}"
    );
}

/// #284 defect 1: the report must be silent when the files really are gone,
/// **in any locale**. `git clean`'s `Removing %s` goes through gettext, and
/// production spawns inherit the server's `LANG` (`sandbox::spawn`'s
/// `env_clear` is `#[cfg(test)]`-only by design), so the pre-#284 parse
/// inverted itself under a translated git: three deleted files matched no
/// prefix, all three looked un-deleted, and the endpoint answered 409 —
/// "your files survived" — about files that were already gone for good.
///
/// The positive leg runs a **real** `git clean` and asserts silence. The
/// paired negative re-implements the pre-#284 parse inline and pins that it
/// would have got the same end state wrong, so the positive leg is proven
/// capable of failing rather than merely green.
#[test]
fn partial_delete_report_reads_the_worktree_so_a_translated_git_cannot_invert_it() {
    let (_dir, repo) = seeded_repo();
    for p in ["x.txt", "y.txt", "z.txt"] {
        std::fs::write(repo.join(p), "junk\n").unwrap();
    }
    // Really deleted, by real git.
    run(&repo, &["clean", "-f", "--", "x.txt", "y.txt", "z.txt"]);
    for p in ["x.txt", "y.txt", "z.txt"] {
        assert!(
            std::fs::symlink_metadata(repo.join(p)).is_err(),
            "{p} should be gone before the report is asked anything"
        );
    }
    assert_eq!(
        observe_deletion(
            &repo,
            &["x.txt", "y.txt", "z.txt"],
            &["x.txt", "y.txt", "z.txt"]
        )
        .partial_refusal(),
        None,
        "every requested path is gone from disk — claiming a partial result here \
         is the 409-after-destruction inversion #284 was filed about"
    );

    // Paired negative: the pre-#284 decision, re-implemented here in full and
    // run against the SAME end state, so "the old code would have got this
    // wrong" is demonstrated rather than asserted. `translated_stdout` is what
    // git 2.43 prints for these three deletions under `LANG=fr_FR.UTF-8` with
    // its message catalogs installed; production spawns inherit the server's
    // locale, because `sandbox::spawn`'s `env_clear` is `#[cfg(test)]`-only by
    // design (argv and env cannot change after policy classification), so
    // widening that boundary was never the fix available here.
    let translated_stdout = "Suppression de x.txt\nSuppression de y.txt\nSuppression de z.txt\n";
    let old_verdict_survivors: Vec<&str> = {
        let removed: std::collections::HashSet<&str> = translated_stdout
            .lines()
            .filter_map(|line| line.strip_prefix("Removing "))
            .collect();
        ["x.txt", "y.txt", "z.txt"]
            .into_iter()
            .filter(|p| !removed.contains(p))
            .collect()
    };
    assert_eq!(
        old_verdict_survivors,
        ["x.txt", "y.txt", "z.txt"],
        "the old stdout parse called all three destroyed files survivors, so the \
         endpoint answered 409 — 'your files were NOT deleted' — after they were \
         irreversibly gone. That verdict is what the assertion above must not \
         reproduce."
    );
}

/// #284, the trap in the fix the issue proposed. `Path::exists()` follows a
/// symlink, so a **dangling** symlink — dirent present, target gone —
/// reports as absent. `git clean` can delete dangling symlinks, so an
/// `exists()`-based check cannot tell "clean removed the link" from "clean
/// skipped the link, and the link's target happened to be missing already":
/// both read as deleted, and the second is a false success.
/// `symlink_metadata` stats the entry itself and separates them.
///
/// Both legs run against the same real dangling symlink: skipped (survivor,
/// must be named) and then actually removed by real `git clean` (must be
/// silent).
#[test]
fn partial_delete_report_tells_a_surviving_dangling_symlink_from_a_deleted_one() {
    let (_dir, repo) = seeded_repo();
    let link = repo.join("dangling");
    std::os::unix::fs::symlink(repo.join("no-such-target"), &link).unwrap();

    // Paired negative on the naive fix: `exists()` already says "gone" for
    // this entry, which is still sitting in the worktree.
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "the dirent is there"
    );
    assert!(
        !link.exists(),
        "`exists()` follows the link and reports absent — an `exists()`-based \
         check would call this survivor deleted"
    );

    let msg = observe_deletion(&repo, &["dangling"], &["dangling"])
        .partial_refusal()
        .expect("an entry still in the worktree was not deleted, whatever it points at");
    assert!(msg.contains("dangling"), "{msg}");

    // Paired positive: once real `git clean` removes the same entry — which
    // it does, dangling target and all — the report goes silent.
    run(&repo, &["clean", "-f", "--", "dangling"]);
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(
        observe_deletion(&repo, &["dangling"], &["dangling"]).partial_refusal(),
        None
    );
}

/// #284 review finding: the mirror image of the bug #284 itself fixed. Asking
/// only "is it gone *now*?" reads "absent" as "we deleted it", so a path that
/// something else removed first gets credited to this operation — in the
/// response count, and in the **journal**, which is the durable record for the
/// one operation with no undo of any kind.
///
/// The end state built here is exactly what the race produces: both requested
/// paths are absent from disk, but only `a.txt` was there when the snapshot
/// was taken immediately before the spawn. `git clean -f -- a.txt b.txt`
/// exits 0 and prints nothing about `b.txt` in that case (verified against
/// real git), so the spawn itself offers no evidence either way — the
/// before-snapshot is the only thing that can tell the two apart.
#[test]
fn a_deletion_by_something_else_is_not_credited_to_this_operation() {
    let (_dir, repo) = seeded_repo();
    // `a.txt` is the seed commit's tracked file, so these use names that the
    // fixture does not already put on disk.
    let requested = ["ours.txt", "theirs.txt"];
    // Neither path is on disk now. `ours.txt` was present at the pre-spawn
    // snapshot (we removed it); `theirs.txt` was already gone by then.
    let present_before = ["ours.txt"];
    for p in requested {
        assert!(
            std::fs::symlink_metadata(repo.join(p)).is_err(),
            "{p} must be absent for this end state"
        );
    }

    let outcome = observe_deletion(&repo, &requested, &present_before);
    assert_eq!(
        outcome.deleted,
        ["ours.txt"],
        "only the path this operation actually removed may be counted as ours"
    );
    assert_eq!(
        outcome.already_gone,
        ["theirs.txt"],
        "a path that was already gone before the spawn was not deleted by us"
    );
    assert!(outcome.survived.is_empty(), "{:?}", outcome.survived);

    // Nothing survived, so this is still a success — the honest report is a
    // 200 whose count is 1, not a 409.
    assert_eq!(outcome.partial_refusal(), None);

    // The disclosure that goes into both the response and the journal names
    // the path and says plainly that we did not delete it.
    let note = outcome.already_gone_note();
    assert!(note.contains("theirs.txt"), "{note}");
    assert!(
        note.contains("not deleted by this operation"),
        "the journal must not imply we destroyed it: {note}"
    );
    assert!(
        !note.contains("ours.txt"),
        "the path we really did delete must not be disclaimed: {note}"
    );

    // Paired negative: the two-bucket decision this replaced, re-implemented
    // in full and run against the SAME end state, so "the old shape would
    // have got this wrong" is demonstrated rather than asserted. It had no
    // before-snapshot at all, so every absent path counted as ours.
    let old_deleted: Vec<&str> = requested
        .into_iter()
        .filter(|p| std::fs::symlink_metadata(repo.join(p)).is_err())
        .collect();
    assert_eq!(
        old_deleted,
        ["ours.txt", "theirs.txt"],
        "the pre-fix shape credited this operation with destroying 2 paths when it \
         destroyed 1, and journalled 'deleted 2 untracked paths permanently' — a \
         durable audit record of a destruction it did not perform. That verdict is \
         what the assertions above must not reproduce."
    );
    assert_ne!(
        outcome.deleted.len(),
        old_deleted.len(),
        "if these agree, the before-snapshot is not doing anything"
    );
}

/// The count in the response and the journal is what this operation
/// *destroyed*, not what the client *asked for* — pinned at the one place
/// that computes it, because that is the only place the two can be made to
/// differ.
///
/// This test exists because the first cut of the fix left `let count =
/// paths.len()` reachable in the executor: reverting the count to the
/// requested length passed all 558 tests, since no test could construct a
/// state where the numbers disagree. Composing the report from the observed
/// outcome makes the divergence expressible here.
#[test]
fn a_report_counts_only_what_this_operation_destroyed() {
    // Three requested; one really destroyed by us, two already gone.
    let outcome = DeleteOutcome {
        deleted: vec!["ours.txt"],
        already_gone: vec!["theirs.txt", "alsotheirs.txt"],
        survived: vec![],
    };
    let (status, body, journal) = outcome.report();
    assert_eq!(
        status,
        StatusCode::OK,
        "nothing survived, so this succeeded"
    );
    assert!(
        body.contains("Deleted 1 untracked path permanently"),
        "the count is the 1 we destroyed, not the 3 that are gone: {body}"
    );
    assert!(
        !body.contains("Deleted 3") && !body.contains("Deleted 2"),
        "counting the request instead of the result is the whole bug: {body}"
    );
    assert!(
        body.contains("theirs.txt") && body.contains("alsotheirs.txt"),
        "the ones we did not delete are still disclosed, just not claimed: {body}"
    );
    // The journal is the durable half and must agree with the response — an
    // audit record that credits us with 3 destructions is the real damage.
    assert!(
        journal.contains("deleted 1 untracked path permanently"),
        "{journal}"
    );
    assert!(!journal.contains("deleted 3"), "{journal}");
    assert!(
        !journal.to_lowercase().contains("undo")
            && !journal.to_lowercase().contains("restore")
            && !journal.to_lowercase().contains("recover"),
        "{journal}"
    );
}

/// The plain success path still reads exactly as it did — no stray
/// disclosure sentence when there is nothing to disclose.
#[test]
fn a_clean_success_report_says_nothing_about_foreign_deletions() {
    let outcome = DeleteOutcome {
        deleted: vec!["a.txt", "b.txt"],
        already_gone: vec![],
        survived: vec![],
    };
    let (status, body, journal) = outcome.report();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "Deleted 2 untracked paths permanently. That content was never stored in \
         git, so there is no way to bring it back."
    );
    assert!(!journal.contains("already gone"), "{journal}");
}

/// The two disclosures compose: a path can survive *and* another can have
/// been removed by something else in the same batch, and the 409 body has to
/// carry both facts without confusing them for each other.
#[test]
fn a_refusal_reports_survivors_and_foreign_deletions_as_different_things() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("survivor.txt"), "still here\n").unwrap();
    let requested = ["ours.txt", "theirs.txt", "survivor.txt"];
    // `ours.txt` we deleted; `theirs.txt` was gone before we ran;
    // `survivor.txt` is still on disk.
    let outcome = observe_deletion(&repo, &requested, &["ours.txt", "survivor.txt"]);
    assert_eq!(outcome.deleted, ["ours.txt"]);
    assert_eq!(outcome.already_gone, ["theirs.txt"]);
    assert_eq!(outcome.survived, ["survivor.txt"]);

    let msg = outcome
        .partial_refusal()
        .expect("a surviving requested path must always refuse");
    assert!(
        msg.contains("ours.txt was deleted permanently"),
        "the one we destroyed is named as destroyed: {msg}"
    );
    assert!(
        msg.contains("survivor.txt was not"),
        "the survivor is named as not deleted: {msg}"
    );
    assert!(
        msg.contains("theirs.txt was already gone before this ran"),
        "the foreign deletion is disclosed as a third, distinct outcome: {msg}"
    );
    assert!(
        !msg.to_lowercase().contains("undo")
            && !msg.to_lowercase().contains("restore")
            && !msg.to_lowercase().contains("recover"),
        "still no reversibility implied for an operation that has none: {msg}"
    );
}

/// The bias #284 exists to preserve, pinned against the new three-way split:
/// a path still on disk is a survivor *whoever* put it there, so the
/// before-snapshot can never be read as licence to call a present file
/// deleted. This is the direction that must never invert — the one that makes
/// a user stop looking for data that is gone for good.
#[test]
fn a_path_still_on_disk_is_a_survivor_even_if_the_snapshot_missed_it() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("appeared.txt"), "written after the snapshot\n").unwrap();
    // The pathological input: the snapshot says it was not there, yet it is.
    let outcome = observe_deletion(&repo, &["appeared.txt"], &[]);
    assert_eq!(
        outcome.survived,
        ["appeared.txt"],
        "presence now outranks the snapshot — never report a present file as gone"
    );
    assert!(outcome.deleted.is_empty());
    assert!(outcome.already_gone.is_empty());
    let msg = outcome.partial_refusal().expect("must refuse");
    assert!(
        msg.starts_with("Partial result: nothing was deleted"),
        "with an empty deleted set the message must say so, not trail an empty list: {msg}"
    );
}

/// #284 defect 2, end to end through the two production functions that own
/// the count: `handlers::discard::validate_paths` (where the dedupe lives)
/// composed with the executor whose message says `paths.len()`. The
/// assertion that matters is the last one — the number in the response
/// equals the number of files that actually left the disk.
#[tokio::test]
async fn a_duplicated_delete_request_reports_the_count_that_really_happened() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("scratch.txt"), "junk\n").unwrap();
    std::fs::write(repo.join("other.txt"), "junk\n").unwrap();

    let paths =
        crate::handlers::discard::validate_paths(git_vista_protocol::WorktreePathsRequest {
            paths: vec![
                "scratch.txt".to_string(),
                "other.txt".to_string(),
                "scratch.txt".to_string(),
            ],
        })
        .expect("a repeated path is client sloppiness, not a wire error");

    let (status, body) = exec_delete_untracked_paths(&repo, NetworkNeed::Local, &paths).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Ground truth first: exactly two files actually left the disk, nothing
    // else was touched, and the working tree is clean.
    assert!(std::fs::symlink_metadata(repo.join("scratch.txt")).is_err());
    assert!(std::fs::symlink_metadata(repo.join("other.txt")).is_err());
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
    // The load-bearing assertion, and the one that fails against the pre-#284
    // shape: without the dedupe `paths` held 3 entries, `git clean` still
    // removed the same 2 files, the survivor check still found nothing left
    // behind, and `paths.len()` put "Deleted 3 untracked paths permanently"
    // in the response — an overstated blast radius in the one operation with
    // no undo, where the count is the user's only record of what is gone.
    assert!(
        body.contains("Deleted 2 untracked paths"),
        "the reported count must be the 2 files that actually left the disk, not \
         the 3 entries the client happened to type: {body}"
    );
    assert_eq!(paths.len(), 2, "the repeat must not survive validation");
}

/// The discard-side twin of the count fix: same `validate_paths`, same
/// `paths.len()` message, so a repeated path must not inflate that count
/// either.
#[tokio::test]
async fn a_duplicated_discard_request_reports_the_count_that_really_happened() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();

    let paths =
        crate::handlers::discard::validate_paths(git_vista_protocol::WorktreePathsRequest {
            paths: vec!["a.txt".to_string(), "a.txt".to_string()],
        })
        .expect("a repeated path is client sloppiness, not a wire error");

    let (status, body) = exec_discard_tracked_paths(&repo, NetworkNeed::Local, &paths).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Ground truth: one file, reverted to HEAD, working tree clean.
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
    // The pre-#284 shape said "2 tracked paths" here for the one file it
    // discarded.
    assert!(
        body.contains("1 tracked path.") && !body.contains('2'),
        "one path was discarded, so the response must say 1: {body}"
    );
    assert_eq!(paths.len(), 1, "the repeat must not survive validation");
}

// ---------------------------------------------------------------------------
// Layer 2 — every write route funnels into this planner
// ---------------------------------------------------------------------------

/// Read a server source file relative to the crate root.
fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"))
}

/// The slice of `src` from `async fn <name>` to the next function definition
/// (or EOF) — enough resolution to say what one handler's body calls.
fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let needle = format!("async fn {name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("no ‘async fn {name}’ found"));
    let rest = &src[start + needle.len()..];
    let end = [
        "\nasync fn ",
        "\npub(crate) async fn ",
        "\npub async fn ",
        "\nfn ",
        "\npub fn ",
        "\npub(crate) fn ",
        "\n#[cfg(test)]",
    ]
    .iter()
    .filter_map(|boundary| rest.find(boundary))
    .min()
    .unwrap_or(rest.len());
    &rest[..end]
}

/// The single-funnel proof: the router's POST table is exactly the known
/// write surface, and every **git-mutating** route's handler reaches
/// [`plan_and_execute`] — directly or through the one named local helper it
/// delegates to. A new POST route, a renamed handler, or a handler that stops
/// calling the planner all fail here. (The other half — nothing *outside*
/// the planner spawns a mutating process — is `argv_boundary`'s tripwire.)
#[test]
fn every_git_write_route_reaches_the_planner() {
    let main_src = source("src/main.rs");

    // Every POST route in the router, in order. Repo-management writes
    // (clone/select/rescan/delete-clone) manage the catalog rather than
    // mutating the selected repository's git state; they are listed so a new
    // route *must* be classified here, on purpose, not silently.
    let posts: Vec<&str> = main_src
        .lines()
        .filter(|l| l.contains("post("))
        .map(str::trim)
        .collect();
    let expected: &[(&str, &str)] = &[
        // Session bootstrap (`POST /session` behind the sign-in token) — an
        // auth write, not a git write; routed with `.post(…)` directly.
        ("create_session", "create_session"),
        ("/api/clone", "clone_repo"),
        ("/api/delete-clone", "delete_clone_repo"),
        ("/api/select", "select_repo"),
        ("/api/rescan", "rescan"),
        ("/api/branch", "create_branch"),
        ("/api/commit", "create_commit"),
        ("/api/stage", "stage_all"),
        // Staging selections (M2.17b, #213): apply is a git write and MUST
        // reach the planner (funnel row below). Preview is deliberately not
        // one — it builds the same bytes but mutates nothing and never mints
        // a plan; its refusals (400/409) happen before any operation exists.
        ("/api/staging/preview", "staging_preview"),
        ("/api/staging/apply", "staging_apply"),
        ("/api/unstage", "unstage_all"),
        ("/api/undo", "activity::undo"),
        ("/api/merge", "merge_branch"),
        ("/api/push", "push_branch"),
        ("/api/delete-branch", "delete_branch"),
        ("/api/checkout", "checkout_branch"),
        ("/api/force-delete-branch", "force_delete_branch"),
        ("/api/rebase", "rebase"),
        ("/api/reset-test-repo", "reset_test_repo"),
        // #219 (M2.18a): discard/delete of working-tree paths.
        ("/api/discard-tracked-paths", "discard_tracked_paths"),
        ("/api/delete-untracked-paths", "delete_untracked_paths"),
    ];
    assert_eq!(
        posts.len(),
        expected.len(),
        "the POST route table changed — classify the new/removed route here \
         (git write → must call the planner; catalog write → say so): {posts:#?}"
    );
    for (route, handler) in expected {
        // Exact-quoted route and `post(handler)` — substring drift like
        // `/api/branch` matching `/api/branch-x` can't satisfy a stale entry.
        let hit = posts.iter().any(|l| {
            let route_ok = if route.starts_with('/') {
                l.contains(&format!("\"{route}\""))
            } else {
                true // create_session is routed with a bare `.post(…)`
            };
            route_ok && l.contains(&format!("post({handler})"))
        });
        assert!(hit, "expected POST {route} → {handler} in main.rs's router");
    }

    // Non-POST write methods: the router's only one is session revocation
    // (`DELETE /api/session` — an auth write, not a git write). Any other
    // DELETE, or any PUT/PATCH/generic method filter, would dodge the `post(`
    // tally above — refuse the vocabulary until it's classified here.
    assert_eq!(
        main_src.matches(".delete(").count(),
        1,
        "a new DELETE route in main.rs — classify it in \
         every_git_write_route_reaches_the_planner"
    );
    assert!(
        main_src.contains(".delete(revoke_session)"),
        "the one DELETE route must be session revocation"
    );
    for forbidden in [".put(", ".patch(", "MethodFilter"] {
        assert!(
            !main_src.contains(forbidden),
            "main.rs uses {forbidden} — a non-POST write route? Classify it \
             in every_git_write_route_reaches_the_planner"
        );
    }

    // The git-mutating handlers: every planner path each one owns, as
    // (file, handler, route-to-the-planner) rows. `None` requires the
    // handler's own body to call `plan_and_execute`; `Some(helper)` requires
    // the handler to call that named local helper AND the helper's body to
    // call `plan_and_execute` — the requirements are exact per row, never an
    // either/or (an OR would let a two-path handler like `create_commit`
    // satisfy the check with one path while the other quietly left the
    // planner). Handlers with two write paths appear twice.
    let funnel: &[(&str, &str, Option<&str>)] = &[
        ("src/handlers/branch.rs", "create_branch", None),
        (
            "src/handlers/branch.rs",
            "checkout_branch",
            Some("branch_op"),
        ),
        ("src/handlers/branch.rs", "merge_branch", Some("branch_op")),
        ("src/handlers/branch.rs", "push_branch", Some("branch_op")),
        ("src/handlers/branch.rs", "delete_branch", Some("branch_op")),
        (
            "src/handlers/branch.rs",
            "force_delete_branch",
            Some("branch_op"),
        ),
        // create_commit's CommitOnHead path calls the planner directly…
        ("src/handlers/commit.rs", "create_commit", None),
        // …and its EmptyCommitOnBranch path goes through the helper.
        (
            "src/handlers/commit.rs",
            "create_commit",
            Some("commit_empty_on_branch"),
        ),
        ("src/handlers/commit.rs", "stage_all", None),
        ("src/handlers/commit.rs", "unstage_all", None),
        ("src/handlers/rebase.rs", "rebase", None),
        ("src/handlers/reset.rs", "reset_test_repo", None),
        ("src/activity.rs", "undo", None),
        ("src/handlers/staging.rs", "staging_apply", None),
        ("src/handlers/discard.rs", "discard_tracked_paths", None),
        ("src/handlers/discard.rs", "delete_untracked_paths", None),
    ];
    for (file, handler, helper) in funnel {
        let src = source(file);
        let body = fn_body(&src, handler);
        let reaches = match helper {
            None => body.contains("plan_and_execute("),
            Some(h) => {
                body.contains(&format!("{h}(")) && fn_body(&src, h).contains("plan_and_execute(")
            }
        };
        assert!(
            reaches,
            "{file}::{handler} no longer reaches plan_and_execute (via {helper:?}) — \
             every git write must flow through the shared planner (ADR 0016)"
        );
    }
}

/// The production composition itself: [`plan_and_execute`]'s body must call
/// `build_plan`, `validate`, `enforce_fresh` and `execute`, in that order.
/// The pipeline tests above drive the same stages with injected tokens (the
/// process-global selection is set-once per process, owned by `state`'s own
/// test); this pin guarantees the entry point requests actually take composes
/// exactly the stages those tests prove.
/// The guard's position is pinned too, and it is load-bearing (#60) — but the
/// *other way round* from the obvious guess: the plan is built BEFORE the guard
/// so a queued duplicate carries the pre-mutation generation into
/// `enforce_fresh` and is refused there. Guarding the observation as well would
/// let a double-clicked Commit observe fresh state and commit twice. See
/// `plan_and_execute_in`'s own docs and ADR 0019.
#[test]
fn the_production_entry_point_composes_the_tested_stages_in_order() {
    let src = source("src/planner.rs");
    let body = fn_body(&src, "plan_and_execute_in");
    let mut from = 0;
    for stage in [
        "build_plan(",
        "coordinator::lock(",
        "coordinator::refuse_if_git_busy(",
        "validate(",
        "enforce_fresh(",
        "execute(",
    ] {
        match body[from..].find(stage) {
            Some(at) => from += at + stage.len(),
            None => panic!(
                "plan_and_execute_in no longer calls {stage} after the previous stage — \
                 the guard → build → validate → enforce_fresh → execute composition is broken"
            ),
        }
    }
}

/// The outer entry point still applies the write gate and delegates, now
/// through the lifecycle layer: the handlers' single funnel is unchanged by the
/// #60 split or the #61 one.
///
/// Both hops are pinned because both are load-bearing. The gate and the
/// idempotency-key requirement have to sit on the *outermost* entry point —
/// that is what makes them impossible for a new handler to forget — while the
/// guarded pipeline has to stay reachable underneath, or the tracked path would
/// silently stop taking the repository guard.
#[test]
fn the_global_entry_point_delegates_through_the_lifecycle_to_the_pipeline() {
    let src = source("src/planner.rs");

    let outer = fn_body(&src, "plan_and_execute");
    assert!(
        outer.contains("reject_if_read_only()"),
        "the write gate must stay on the global entry point"
    );
    assert!(
        outer.contains("operations::current_key()"),
        "every mutation must require the client's idempotency key at the funnel (#61)"
    );
    assert!(
        outer.contains("plan_and_execute_tracked("),
        "the global entry point must delegate through the lifecycle layer"
    );

    let tracked = fn_body(&src, "plan_and_execute_tracked");
    for required in [
        "operations::admit(",
        "tokio::spawn(",
        "plan_and_execute_in(",
    ] {
        assert!(
            tracked.contains(required),
            "the lifecycle layer no longer calls {required} — an operation must be \
             admitted, run detached (so a disconnect can't cancel git), and reach \
             the guarded pipeline"
        );
    }
    assert!(
        tracked.contains("wait_terminal()"),
        "the request must await the recorded terminal result, so a retry replays it"
    );
}

// ---------------------------------------------------------------------------
// Layer 3 — the gates don't just refuse, they protect
// ---------------------------------------------------------------------------

/// #145's race, end-to-end: the repository moves after the plan is built, and
/// the full pipeline both refuses (409) **and leaves the target untouched**.
#[tokio::test]
async fn a_raced_plan_is_refused_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]);
    let (plan, observed) = build_plan(
        &repo,
        GitOperation::DeleteBranch {
            branch: branch("side"),
        },
        tokens(),
    )
    .await;
    // The race: a ref appears between build and execution.
    run(&repo, &["branch", "raced"]);
    let (status, why) = run_prebuilt(&repo, plan, observed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("changed while this plan was pending"), "{why}");
    assert_ne!(
        out(&repo, &["branch", "--list", "side"]),
        "",
        "the refused delete must not have removed the branch"
    );
}

/// #145's tamper, end-to-end: an operation swapped under its hash is refused
/// at `validate` and the substituted operation never executes.
#[tokio::test]
async fn a_tampered_plan_is_refused_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]);
    let (mut plan, observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    plan.operation = GitOperation::ForceDeleteBranch {
        branch: branch("side"),
    };
    let (status, why) = run_prebuilt(&repo, plan, observed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("doesn't match"), "{why}");
    assert_ne!(
        out(&repo, &["branch", "--list", "side"]),
        "",
        "the smuggled force-delete must not have run"
    );
}

/// #145's expiry, end-to-end: a plan past its TTL is refused and the staged
/// commit it approved is never written.
#[tokio::test]
async fn an_expired_plan_is_refused_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    let (mut plan, observed) = build_plan(
        &repo,
        GitOperation::CommitOnHead {
            message: message("too late"),
            allow_empty: false,
        },
        tokens(),
    )
    .await;
    plan.expires_at = UnixSeconds(crate::activity::now_secs() - 1);
    let (status, why) = run_prebuilt(&repo, plan, observed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("expired"), "{why}");
    assert_eq!(
        tip(&repo, "HEAD"),
        before,
        "the expired commit must not land"
    );
}

/// #145's *precondition* race, end-to-end: a precondition that held at build
/// breaks without any ref moving (the push remote disappears — invisible to
/// the generation check), and the full pipeline still refuses before the
/// executor runs. The other flavor of drift the staleness gate must catch.
#[tokio::test]
async fn a_broken_precondition_is_refused_end_to_end() {
    let (dir, repo) = seeded_repo();
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    let (plan, observed) = build_plan(
        &repo,
        GitOperation::PushBranch {
            branch: branch("main"),
            remote: RemoteName::new("origin").unwrap(),
        },
        tokens(),
    )
    .await;
    // The race: the remote is deconfigured between build and execution.
    // `git remote remove` also drops remote-tracking config but no local
    // branch ref moves, so only the precondition re-check can catch it.
    run(&repo, &["remote", "remove", "origin"]);
    let (status, why) = run_prebuilt(&repo, plan, observed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("no longer configured"), "{why}");
    assert_eq!(
        out(&remote, &["for-each-ref", "refs/heads"]),
        "",
        "nothing may have been pushed to the deconfigured remote"
    );
}

// --- #222 (M2.19a): typed AmendCommit contract, execution not yet wired ---

/// [`GitOperation::AmendCommit`] proves its *shape* end-to-end through the
/// real pipeline (build → validate → enforce_fresh), but M2.19a ships no
/// execution — #223's to add. So unlike every other pipeline test in this
/// file, this one asserts the operation is refused with `NOT_IMPLEMENTED`
/// and, more importantly, that the repository is completely untouched: HEAD
/// is exactly the commit it was before. A test that only checked the status
/// code would pass just as well if the stub silently mutated the repo and
/// then reported failure anyway — the tip assertion is what actually proves
/// "no execution happened," and it is what forces #223 to touch this exact
/// test when real execution replaces the stub.
#[tokio::test]
async fn amend_commit_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("amended message"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert_eq!(
        tip(&repo, "HEAD"),
        before,
        "the stub must never move HEAD — M2.19a ships no execution"
    );
    assert_eq!(
        out(&repo, &["log", "-1", "--format=%s"]),
        "seed",
        "the stub must never create a new commit"
    );
}
