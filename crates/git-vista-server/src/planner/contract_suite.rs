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
//!     variant refuses to compile until it gets a pipeline test. (Four
//!     exceptions today: the four tag operations (M2.21a #235) ship no
//!     execution — their pipeline tests assert the *stubs'* refusal and that
//!     the repository stayed byte-identical, the honest version of this
//!     layer's claim until the later M2.21 slices of #74 wire real execution
//!     in. `AmendCommit` was staged the same way by #222 and graduated to a
//!     real execution test when #223 wired `exec_amend_commit`; `FetchRemote`
//!     graduated the same way when M2.20c #229 wired `planner::fetch`, and
//!     `PullBranch` when M2.20d #230 wired `planner::pull`. Their heavier
//!     behavioural coverage — live progress, a cancel that kills the child,
//!     the dropped-connection replay, redaction on the streaming path, the
//!     merge-vs-rebase history difference, the conflict abort — lives in the
//!     siblings [`super::fetch_suite`] and [`super::pull_suite`].)
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
        GitOperation::FetchRemote { .. } => "fetch_remote_executes_through_the_pipeline",
        GitOperation::PullBranch { .. } => "pull_branch_executes_through_the_pipeline",
        GitOperation::CreateTag { .. } => "create_tag_executes_through_the_pipeline",
        GitOperation::DeleteLocalTag { .. } => "delete_local_tag_executes_through_the_pipeline",
        GitOperation::DeleteRemoteTag { .. } => "delete_remote_tag_executes_through_the_pipeline",
        GitOperation::PushTag { .. } => "push_tag_executes_through_the_pipeline",
    }
}

/// [`covered_by`]'s split-path sibling (M2.23c, #247): every variant names
/// the live test proving the `build_plan_only → submit_plan` staging covers
/// it too. **No wildcard arm, on purpose** — a new `GitOperation` variant
/// fails to compile here as well as in [`covered_by`], so it cannot land
/// covered on only one of the two paths.
///
/// Unlike [`covered_by`], the mapping is *not* injective: the split path
/// shares every executor with the single-shot path (same `execute`, same
/// argv, same texts), so what is new per variant is only *equivalence* — and
/// one sweep, [`the_split_path_is_byte_identical_to_the_single_shot_path`],
/// proves it for the whole [`samples`] census at once. A future variant whose
/// split behaviour genuinely diverges (none may, today) would get its own arm
/// pointing at its own test.
fn covered_on_split_path(op: &GitOperation) -> &'static str {
    match op {
        GitOperation::CreateBranch { .. }
        | GitOperation::CommitOnHead { .. }
        | GitOperation::EmptyCommitOnBranch { .. }
        | GitOperation::StageAll
        | GitOperation::UnstageAll
        | GitOperation::CheckoutBranch { .. }
        | GitOperation::MergeBranch { .. }
        | GitOperation::PushBranch { .. }
        | GitOperation::DeleteBranch { .. }
        | GitOperation::ForceDeleteBranch { .. }
        | GitOperation::RebaseOntoBase { .. }
        | GitOperation::RestoreBranch { .. }
        | GitOperation::ResetBranch { .. }
        | GitOperation::RevertCommit { .. }
        | GitOperation::ResetTestRepo
        | GitOperation::StageSelection { .. }
        | GitOperation::DiscardTrackedPaths { .. }
        | GitOperation::DeleteUntrackedPaths { .. }
        | GitOperation::AmendCommit { .. }
        | GitOperation::FetchRemote { .. }
        | GitOperation::PullBranch { .. }
        | GitOperation::CreateTag { .. }
        | GitOperation::DeleteLocalTag { .. }
        | GitOperation::DeleteRemoteTag { .. }
        | GitOperation::PushTag { .. } => {
            "the_split_path_is_byte_identical_to_the_single_shot_path"
        }
    }
}

/// One canonical sample per [`GitOperation`] variant — the census input for
/// the single-shot coverage test below and, since M2.23c (#247), for the
/// split-path census and the byte-identity sweep
/// ([`the_split_path_is_byte_identical_to_the_single_shot_path`]): one list,
/// so the two paths can never quietly census different vocabularies.
fn samples() -> Vec<GitOperation> {
    let zeros = "0".repeat(40);
    vec![
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
            set_upstream: false,
            force: ForcePublish::None,
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
        GitOperation::FetchRemote {
            remote: RemoteName::new("origin").unwrap(),
        },
        GitOperation::PullBranch {
            remote: RemoteName::new("origin").unwrap(),
            branch: branch("b"),
            strategy: git_vista_protocol::MergeStrategy::Merge,
        },
        GitOperation::CreateTag {
            name: TagName::new("v1").unwrap(),
            target: oid(&zeros),
            annotation: None,
        },
        GitOperation::DeleteLocalTag {
            name: TagName::new("v1").unwrap(),
        },
        GitOperation::DeleteRemoteTag {
            name: TagName::new("v1").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        },
        GitOperation::PushTag {
            name: TagName::new("v1").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        },
    ]
}

/// One sample per variant, each mapped through [`covered_by`]: the mapping
/// stays total (compile-time) and injective (here) — no two variants may
/// share a pipeline test.
#[test]
fn every_operation_kind_names_a_distinct_pipeline_test() {
    let samples = samples();
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
            set_upstream: false,
            force: ForcePublish::None,
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

/// The argument list of every `.route(…)` call in `src`, whitespace-collapsed
/// so a registration rustfmt wrapped across lines reads exactly like a
/// one-liner.
///
/// Balanced parens rather than "up to the next `.route(`": a naive split
/// would fold each call's span into its successor's, so one route's handler
/// would satisfy a check about a different route's.
fn route_call_spans(src: &str) -> Vec<String> {
    let flat = src.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = ".route(";
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = flat[from..].find(needle) {
        let start = from + rel + needle.len();
        let mut depth = 1usize;
        let mut end = flat.len();
        for (offset, ch) in flat[start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(flat[start..end].to_string());
        from = end + 1;
    }
    out
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

    // Every POST route in the router. Repo-management writes
    // (clone/select/rescan/delete-clone) manage the catalog rather than
    // mutating the selected repository's git state; they are listed so a new
    // route *must* be classified here, on purpose, not silently.
    //
    // Extracted as balanced-paren `.route(` **spans**, not lines. rustfmt
    // wraps any call whose argument list exceeds `fn_call_width` (60 by
    // default), which several registrations here do — a per-line scan sees
    // only the `post(handler)` fragment of a wrapped one, never the route it
    // belongs to, so a route could be added in wrapped form and satisfy
    // nothing. `route_authz.rs` extracts the same way, for the same reason.
    let posts: Vec<String> = route_call_spans(&main_src)
        .into_iter()
        .filter(|span| span.contains("post("))
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
        // M2.19b (#223): amend — a git write, funnel row below.
        ("/api/amend-commit", "amend_commit"),
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
        // M2.20c (#229): fetch — a git write, funnel row below.
        ("/api/fetch", "fetch_remote"),
        // M2.20d (#230): pull — a git write, funnel row below.
        ("/api/pull", "pull_branch"),
        ("/api/delete-branch", "delete_branch"),
        ("/api/checkout", "checkout_branch"),
        ("/api/force-delete-branch", "force_delete_branch"),
        ("/api/rebase", "rebase"),
        ("/api/reset-test-repo", "reset_test_repo"),
        // #219 (M2.18a): discard/delete of working-tree paths.
        ("/api/discard-tracked-paths", "discard_tracked_paths"),
        ("/api/delete-untracked-paths", "delete_untracked_paths"),
        // M2.20c (#229): cancelling a running operation. A POST, and a write
        // in the "changes what the server is doing" sense — it kills a child
        // process — but **not** a git write: it constructs no argv and mints
        // no plan, so it has no funnel row below. It is classified here, on
        // purpose, rather than being allowed to slip past the tally.
        (
            "/api/operations/{id}/cancel",
            "handlers::operations::cancel_operation",
        ),
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
        // M2.19b (#223): the amend handler builds `AmendCommit` and calls
        // the planner directly.
        ("src/handlers/commit.rs", "amend_commit", None),
        ("src/handlers/commit.rs", "stage_all", None),
        ("src/handlers/commit.rs", "unstage_all", None),
        // M2.20c (#229) and M2.20d (#230): the two remote-reaching writes.
        // Fetch's row was missing until #230 added it — the POST table above
        // has said "funnel row below" for it since #229, and there was none,
        // so `fetch_remote` could have stopped calling the planner without
        // this test noticing. A census that names a row it does not have is
        // the same vacuity as a test that asserts nothing.
        ("src/handlers/fetch.rs", "fetch_remote", None),
        ("src/handlers/pull.rs", "pull_branch", None),
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
            set_upstream: false,
            force: ForcePublish::None,
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

// --- #223 (M2.19b): AmendCommit execution — CAS, published flag, ------------
// --- hook/signing classification, journal evidence. #222 staged the ---------
// --- typed contract; the inertness test that pinned its stub did its job ----
// --- and was deliberately replaced by the battery below. --------------------

/// The parsed 400 body every amend refusal carries
/// ([`git_vista_protocol::AmendCommitError`]): the typed `kind` plus git's
/// (or the hook's) own message.
fn amend_error(status: StatusCode, body: &str) -> git_vista_protocol::AmendCommitError {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    serde_json::from_str(body).unwrap_or_else(|e| {
        panic!("a 400 from the amend path must parse as AmendCommitError, got {e}: {body}")
    })
}

/// [`GitOperation::AmendCommit`] executes for real through the pipeline
/// (M2.19b, #223): the tip commit is rewritten in place — new message, new
/// staged content, **same commit count** — and the response is the
/// structured [`git_vista_protocol::AmendCommitSuccess`] body carrying the
/// old/new tips and the published-history flag (`Some(false)` here: no
/// remote is configured, and the walk genuinely ran — the paired positive
/// lives in `amending_a_published_commit_is_flagged_in_the_response`).
///
/// The commit-count assertion is the one that distinguishes a real amend
/// from the cheapest broken implementation (a plain `git commit` would move
/// HEAD and change the subject too — but it would leave two commits).
#[tokio::test]
async fn amend_commit_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    std::fs::write(repo.join("a.txt"), "amended content\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("amended message"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    assert_ok(status, &body);
    let after = tip(&repo, "HEAD");
    assert_ne!(
        after, before,
        "an amend rewrites the tip to a new commit id"
    );
    assert_eq!(out(&repo, &["log", "-1", "--format=%s"]), "amended message");
    assert_eq!(
        out(&repo, &["rev-list", "--count", "HEAD"]),
        "1",
        "amend must rewrite the tip in place, never add a commit on top"
    );
    let success: git_vista_protocol::AmendCommitSuccess =
        serde_json::from_str(&body).expect("a 200 amend body is AmendCommitSuccess JSON");
    assert_eq!(success.old_tip, before);
    assert_eq!(success.new_tip.as_deref(), Some(after.as_str()));
    assert_eq!(
        success.amended_published_commit,
        Some(false),
        "no remote is configured, so the walk ran and the honest answer is \
         `false` — not `None`, which is reserved for a walk that failed"
    );
}

/// The executor-level compare-and-swap: an `expected_tip` that was stale
/// **when the plan was built** (the client reviewed an old tip) is refused
/// with a 400 — kind `stale_tip`, per the endpoint's typed contract — and
/// the repository is untouched. This is the leg `enforce_fresh` deliberately
/// does not cover: its per-precondition re-check runs only for preconditions
/// that *held* at build time, so a stale-from-the-start `RefAt` flows
/// through to `exec_amend_commit`'s own guard (`planner.rs`; the
/// moved-after-build race is separately pinned as a 409 by
/// `amend_commit_refuses_when_the_tip_moved_after_the_plan_was_built` in
/// `planner`'s unit tests — 400 says "your request is wrong", 409 says "you
/// lost a race", and the two must not blur).
#[tokio::test]
async fn a_stale_expected_tip_is_refused_as_stale_tip_without_touching_the_repo() {
    let (_dir, repo) = seeded_repo();
    let stale = tip(&repo, "HEAD");
    // The tip moves before the request is even built: the client's picture
    // is out of date, not racing.
    std::fs::write(repo.join("a.txt"), "moved on\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "moved on"]);
    let now = tip(&repo, "HEAD");

    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("must not land"),
            expected_tip: oid(&stale),
            allow_empty: false,
        },
    )
    .await;
    let error = amend_error(status, &body);
    assert_eq!(error.kind, git_vista_protocol::AmendFailureKind::StaleTip);
    assert_eq!(tip(&repo, "HEAD"), now, "a refused CAS must not move HEAD");
    assert_eq!(
        out(&repo, &["log", "-1", "--format=%s"]),
        "moved on",
        "a refused CAS must not rewrite any commit"
    );
    assert_eq!(
        out(&repo, &["rev-list", "--count", "HEAD"]),
        "2",
        "a refused CAS must not create or drop commits"
    );
}

/// Repository hooks genuinely execute during an amend — through the same
/// single sealed spawn as the amend itself (there is no separate hook
/// runner to bypass; `argv_boundary` pins the spawn sites). The proof is a
/// passing `pre-commit` hook that writes a marker file: amend succeeds AND
/// the marker exists, so the hook demonstrably ran inside the pipeline's
/// one `git commit --amend` invocation. Without this, the rejection test
/// below could pass against an implementation that ran hooks through some
/// second, unsandboxed path — or a future `--no-verify` "fix" could
/// silently stop running hooks and every rejection test would just never
/// fire again.
#[tokio::test]
async fn the_amend_runs_repository_hooks_inside_the_pipelines_own_spawn() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    let marker = repo.join(".git/hook-ran-marker");
    std::fs::write(
        repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran-marker\"\nexit 0\n",
    )
    .unwrap();
    make_executable(&repo.join(".git/hooks/pre-commit"));
    assert!(!marker.exists(), "the marker must start absent");

    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("amended with hook"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    assert_ok(status, &body);
    assert!(
        marker.exists(),
        "the pre-commit hook must have executed during the amend — if it \
         did not, hooks are being bypassed (--no-verify, or a second spawn \
         path) and every hook-rejection classification is dead code"
    );
}

/// A rejecting hook classifies as `hook_rejected` — driven with the hard
/// case, a **silent** hook (exit 1, not a byte of output), because git
/// prints nothing of its own for a hook rejection (verified against git
/// 2.43), so an implementation that "classified" by matching some
/// hook-related stderr text would pass a chatty-hook test and misclassify
/// every quiet real-world hook. All three rejectable hook points this argv
/// has are driven — the same three `rejectable_hook_present` probes for
/// (`pre-commit`, `prepare-commit-msg`, `commit-msg`; `git commit --amend
/// -m` runs `prepare-commit-msg` even with `-m`, and a silent exit-1 there
/// fails the amend with the same empty-stderr signature) — so trimming the
/// planner's hook list or regressing any one point turns this red. The
/// repository must be untouched afterward.
#[tokio::test]
async fn a_hook_rejection_is_classified_as_hook_rejected() {
    for hook in ["pre-commit", "prepare-commit-msg", "commit-msg"] {
        let (_dir, repo) = seeded_repo();
        let before = tip(&repo, "HEAD");
        std::fs::write(
            repo.join(format!(".git/hooks/{hook}")),
            "#!/bin/sh\nexit 1\n",
        )
        .unwrap();
        make_executable(&repo.join(format!(".git/hooks/{hook}")));

        let (status, body) = pipeline(
            &repo,
            GitOperation::AmendCommit {
                message: message("must not land"),
                expected_tip: oid(&before),
                allow_empty: false,
            },
        )
        .await;
        let error = amend_error(status, &body);
        assert_eq!(
            error.kind,
            git_vista_protocol::AmendFailureKind::HookRejected,
            "{hook}: {body}"
        );
        assert_eq!(
            tip(&repo, "HEAD"),
            before,
            "{hook}: a rejected amend must not move HEAD"
        );
        assert_eq!(
            out(&repo, &["log", "-1", "--format=%s"]),
            "seed",
            "{hook}: a rejected amend must not rewrite the commit"
        );
    }
}

/// The paired negative for hook classification: a failure that happens
/// **while a rejectable hook is present** but is *not* the hook's doing —
/// git's own would-become-empty refusal — must classify as `other`, not
/// `hook_rejected`. This is what proves the classifier is not just "a hook
/// exists, so blame the hook": the hook here demonstrably passes (it writes
/// a marker), and the refusal comes from git afterward.
#[tokio::test]
async fn a_non_hook_failure_with_a_hook_present_is_not_blamed_on_the_hook() {
    let (_dir, repo) = seeded_repo();
    // A second commit whose staged reversal would make the amended commit
    // empty: `git commit --amend` (without allow_empty) refuses with its
    // "You asked to amend the most recent commit…" advice, exit 1, no
    // `fatal:` — the exact shape most easily confused with a silent hook.
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    run(&repo, &["commit", "-q", "-m", "add b"]);
    let before = tip(&repo, "HEAD");
    run(&repo, &["rm", "-q", "--cached", "b.txt"]);

    let marker = repo.join(".git/hook-ran-marker");
    std::fs::write(
        repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran-marker\"\nexit 0\n",
    )
    .unwrap();
    make_executable(&repo.join(".git/hooks/pre-commit"));

    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("would become empty"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    let error = amend_error(status, &body);
    assert!(
        marker.exists(),
        "the hook must actually have run and passed — otherwise this test \
         is not the negative it claims to be"
    );
    assert_eq!(
        error.kind,
        git_vista_protocol::AmendFailureKind::Other,
        "git's own empty-amend refusal must not be blamed on the passing hook: {body}"
    );
    assert_eq!(tip(&repo, "HEAD"), before, "the refusal must not move HEAD");
}

/// A signing failure classifies as `signing_failed`, for both signer
/// shapes: the gpg format (git's canonical `gpg failed to sign the data`
/// stderr, forced deterministically with `gpg.program=/bin/false`) and the
/// ssh format (an unloadable signing key — no canonical gpg line, which is
/// exactly why the classifier needs its config-probe leg). The repository
/// must be untouched afterward.
#[tokio::test]
async fn a_signing_failure_is_classified_as_signing_failed() {
    let cases: &[&[(&str, &str)]] = &[
        // gpg format: the signer program itself fails.
        &[("commit.gpgsign", "true"), ("gpg.program", "/bin/false")],
        // ssh format: the signing key cannot be loaded.
        &[
            ("commit.gpgsign", "true"),
            ("gpg.format", "ssh"),
            ("user.signingkey", "/nonexistent-signing-key"),
        ],
    ];
    for case in cases {
        let (_dir, repo) = seeded_repo();
        let before = tip(&repo, "HEAD");
        for (key, value) in *case {
            run(&repo, &["config", key, value]);
        }
        let (status, body) = pipeline(
            &repo,
            GitOperation::AmendCommit {
                message: message("must not land"),
                expected_tip: oid(&before),
                allow_empty: false,
            },
        )
        .await;
        let error = amend_error(status, &body);
        assert_eq!(
            error.kind,
            git_vista_protocol::AmendFailureKind::SigningFailed,
            "{case:?}: {body}"
        );
        assert_eq!(
            tip(&repo, "HEAD"),
            before,
            "{case:?}: a failed signing must not move HEAD"
        );
        assert_eq!(
            out(&repo, &["log", "-1", "--format=%s"]),
            "seed",
            "{case:?}: a failed signing must not rewrite the commit"
        );
    }
}

/// The published-history guard's positive: amending a commit that is
/// reachable from a remote-tracking ref answers `amended_published_commit:
/// Some(true)` — and the amend still **succeeds**, because the flag is
/// advisory by decision (ADR 0040), never blocking. The negative
/// (`Some(false)` with no remote) is pinned by
/// `amend_commit_executes_through_the_pipeline`; together they prove the
/// flag is computed, not constant.
///
/// The second leg is the adversarial depth case: the amended tip is *not*
/// the remote-tracking ref's own tip — the remote has moved past it — so a
/// naive "is HEAD the remote tip?" comparison would answer `false`. Only a
/// real reachability walk answers `true` here.
#[tokio::test]
async fn amending_a_published_commit_is_flagged_in_the_response() {
    for remote_moves_past in [false, true] {
        let (dir, repo) = seeded_repo();
        let remote = dir.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "--bare", "-b", "main"]);
        run(
            &repo,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        run(&repo, &["push", "-q", "origin", "main"]);
        let published_tip = tip(&repo, "HEAD");
        if remote_moves_past {
            // The remote gains a commit on top of the one being amended, so
            // the amended commit is published-but-buried.
            run(&repo, &["commit", "-q", "--allow-empty", "-m", "on top"]);
            run(&repo, &["push", "-q", "origin", "main"]);
            run(&repo, &["reset", "-q", "--hard", &published_tip]);
        }

        let (status, body) = pipeline(
            &repo,
            GitOperation::AmendCommit {
                message: message("amend published history"),
                expected_tip: oid(&published_tip),
                allow_empty: false,
            },
        )
        .await;
        assert_ok(status, &body);
        let success: git_vista_protocol::AmendCommitSuccess =
            serde_json::from_str(&body).expect("a 200 amend body is AmendCommitSuccess JSON");
        assert_eq!(
            success.amended_published_commit,
            Some(true),
            "remote_moves_past={remote_moves_past}: the amended-away commit \
             is reachable from refs/remotes/origin/main and must be flagged"
        );
        assert_eq!(
            out(&repo, &["log", "-1", "--format=%s"]),
            "amend published history",
            "remote_moves_past={remote_moves_past}: the flag is advisory — \
             the amend itself must still have run"
        );
    }
}

/// Amending moves only the checked-out branch. Another ref pointing at the
/// same commit under a different name (the issue's named adversarial case)
/// keeps the pre-amend commit exactly where it was — reachable, unmoved,
/// unrewritten.
#[tokio::test]
async fn a_sibling_ref_at_the_amended_commit_is_left_untouched() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    run(&repo, &["branch", "keeper", &before]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("amended message"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(
        tip(&repo, "keeper"),
        before,
        "the sibling branch must still point at the pre-amend commit"
    );
    assert_eq!(
        out(&repo, &["log", "-1", "--format=%s", "keeper"]),
        "seed",
        "the pre-amend commit itself must be unrewritten"
    );
    assert_ne!(tip(&repo, "HEAD"), before);
}

/// A detached HEAD refuses the amend (400, untouched repo): there is no
/// checked-out branch for the operation to target and — the load-bearing
/// half — no branch ref for the plan's `ResetRef` recovery to reset, so
/// running would mean rewriting history with no recovery story. The plan's
/// own `shape` degrades recovery to `NotNeeded` on detached HEAD, which is
/// only honest as long as nothing executes.
#[tokio::test]
async fn a_detached_head_refuses_the_amend() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    run(&repo, &["checkout", "-q", "--detach", &before]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("must not land"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    let error = amend_error(status, &body);
    assert_eq!(
        error.kind,
        git_vista_protocol::AmendFailureKind::Other,
        "{body}"
    );
    assert_eq!(
        tip(&repo, "HEAD"),
        before,
        "a refused detached-HEAD amend must not move HEAD"
    );
    assert_eq!(out(&repo, &["log", "-1", "--format=%s"]), "seed");
}

/// A successful amend is journaled as `ActivityKind::Amend` with the exact
/// old→new tip pair — the record that makes the amend show up in
/// `/api/activity` attributed to the app, and (because `old_oid` is the
/// pre-amend tip, still live in the object database) makes the feed's
/// reset-back undo hint work: `assemble_feed`'s `undo_hint` offers a
/// `ResetBranch { to: old_oid, expected_tip: new_oid }` for the newest
/// Amend event on a branch (`git_vista_core::activity`, its own tested
/// mapping). Asserting the journaled oids here is what proves the recovery
/// story starts from true values rather than from unread placeholders.
#[tokio::test]
async fn a_successful_amend_journals_the_old_and_new_tips_for_undo() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    let (status, body) = pipeline(
        &repo,
        GitOperation::AmendCommit {
            message: message("amended for the journal"),
            expected_tip: oid(&before),
            allow_empty: false,
        },
    )
    .await;
    assert_ok(status, &body);
    let after = tip(&repo, "HEAD");

    let events = journal::read_all(&repo);
    let amend = events
        .iter()
        .find(|e| e.kind == git_vista_core::activity::ActivityKind::Amend)
        .expect("a successful amend must append an Amend journal event");
    assert_eq!(amend.ref_name.as_deref(), Some("main"));
    assert_eq!(
        amend.old_oid.as_deref(),
        Some(before.as_str()),
        "the journaled old tip is the amended-away commit — the undo target"
    );
    assert_eq!(
        amend.new_oid.as_deref(),
        Some(after.as_str()),
        "the journaled new tip is the rewritten commit — the undo's CAS pin"
    );
    assert_eq!(
        amend.source,
        git_vista_core::activity::ActivitySource::App,
        "the amend must be attributed to the app, not left to reflog inference"
    );
}

/// Make a fixture hook executable (`chmod +x`).
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

// --- #227 (M2.20a): typed remote vocabulary, execution not yet wired -------

/// Everything a fetch, pull or push could change about a repository, in one
/// comparable string — the inertness assertion for the contract-only stubs
/// below.
///
/// Checking HEAD alone (which is all the `AmendCommit` stub above needed)
/// would be far too weak here: a fetch that ran would move refs under
/// `refs/remotes/`, write `FETCH_HEAD`, and add objects while leaving HEAD
/// exactly where it was, so a HEAD-only assertion would pass with the network
/// operation having fully executed. `--set-upstream` writes only config, and
/// would be invisible to every ref check. So this covers all five surfaces:
/// every ref, `FETCH_HEAD`, the object store, local config, and the
/// index/worktree.
///
/// [`repo_fingerprint_detects_every_change_it_claims_to_watch`] below proves
/// this is capable of failing on each of them, rather than being a constant
/// that makes its callers vacuously green.
fn repo_fingerprint(repo: &Path) -> String {
    let refs = out(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let head = out(repo, &["rev-parse", "HEAD"]);
    let status = out(repo, &["status", "--porcelain=v2", "--branch"]);
    let objects = out(repo, &["count-objects", "-v"]);
    let config = {
        let mut lines: Vec<String> = out(repo, &["config", "--local", "--list"])
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines.join("\n")
    };
    let fetch_head = repo.join(".git/FETCH_HEAD").exists();
    format!(
        "refs:\n{refs}\nhead:{head}\nstatus:\n{status}\nobjects:\n{objects}\n\
         config:\n{config}\nfetch_head:{fetch_head}"
    )
}

/// The anti-vacuity proof for [`repo_fingerprint`]: each mutation a fetch,
/// pull or push would make must change the fingerprint.
///
/// Without this, `fetch_remote_executes_through_the_pipeline` and
/// `pull_branch_executes_through_the_pipeline` below would be exactly the
/// kind of test this repository has been bitten by six times — asserting
/// "nothing changed" against a helper that could not have noticed if
/// everything had. Each case is driven with plain `git`, so the helper is
/// tested against real repository mutations rather than against itself.
/// One simulated mutation: what a real fetch/pull/push would do, and how to
/// reproduce it with plain `git`.
type FingerprintCase = (&'static str, &'static dyn Fn(&Path));

#[test]
fn repo_fingerprint_detects_every_change_it_claims_to_watch() {
    let cases: &[FingerprintCase] = &[
        ("a fetch moving a remote-tracking ref", &|repo: &Path| {
            run(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"])
        }),
        ("a fetch writing FETCH_HEAD", &|repo: &Path| {
            std::fs::write(repo.join(".git/FETCH_HEAD"), "").unwrap()
        }),
        (
            // Deliberately isolated to the object store: the source file is
            // written inside `.git`, which `git status` ignores, so this case
            // fails unless `count-objects` is genuinely part of the
            // fingerprint. A blob added under the worktree would have changed
            // the status line too and let an objects-blind fingerprint pass.
            "a fetch adding objects and nothing else",
            &|repo: &Path| {
                let src = repo.join(".git/fetched-blob-source");
                std::fs::write(&src, "fetched\n").unwrap();
                run(repo, &["hash-object", "-w", ".git/fetched-blob-source"]);
            },
        ),
        ("a pull moving the checked-out branch", &|repo: &Path| {
            run(repo, &["commit", "-q", "--allow-empty", "-m", "pulled"])
        }),
        ("--set-upstream writing branch config", &|repo: &Path| {
            run(repo, &["config", "branch.main.remote", "origin"])
        }),
        // M2.21a (#235): the two local tag mutations the tag stubs below
        // must be provably not making.
        ("a lightweight tag created (ref only)", &|repo: &Path| {
            run(repo, &["tag", "marker"])
        }),
        (
            "an annotated tag created (ref plus tag object)",
            &|repo: &Path| run(repo, &["tag", "-a", "-m", "v1", "v1"]),
        ),
    ];
    for (what, mutate) in cases {
        let (_dir, repo) = seeded_repo();
        let before = repo_fingerprint(&repo);
        mutate(&repo);
        assert_ne!(
            before,
            repo_fingerprint(&repo),
            "repo_fingerprint must notice {what}; it did not, so every \
             inertness assertion built on it is vacuous"
        );
    }
}

/// A repository with `origin` pointing at a bare remote that is **one commit
/// ahead** of the local `refs/remotes/origin/*`, so a fetch that really runs
/// has something to move and a fetch that does not is visibly inert.
///
/// # Why the remote lives *inside* the repository
///
/// The sandbox (#66 Task 6) grants the served repository's tree and the system
/// trees, and nothing else — a bare remote in a sibling tempdir is denied
/// outright, and the fetch fails with git's "does not appear to be a git
/// repository" for a reason that has nothing to do with fetching. A remote
/// under the repository's own granted tree is readable, and `upload-pack` runs
/// there read-only, so a *fetch* works where the push fixture's equivalent
/// cannot (see `push_branch_executes_through_the_pipeline`: receive-pack's
/// quarantine migration is a cross-directory rename and the shim withholds
/// `LANDLOCK_ACCESS_FS_REFER`).
///
/// This is a **local transport**, so it does not exercise a socket. That is
/// deliberate here: this test's subject is the pipeline and the reported
/// outcome, and the real-socket half of the Network tier already has its own
/// coverage (`sandbox::network_exec`'s `https_suite`, and the push fixture's
/// `git daemon` on the arbitrated port). Classification is by *typed
/// operation*, not by URL (#66's D3), so this still runs through the Network
/// tier, #228's forced `-c core.askpass=`, and the redaction chokepoint.
///
/// Returns `(tempdir, repo, the oid the remote is at)`.
fn repo_behind_its_remote() -> (tempfile::TempDir, PathBuf, String) {
    let (dir, repo) = seeded_repo();
    let remote = repo.join("upstream.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    run(&repo, &["push", "-q", "origin", "main"]);
    run(&repo, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
    run(&repo, &["push", "-q", "origin", "main"]);
    let ahead = tip(&repo, "HEAD");
    // Rewind the local remote-tracking ref so the fetch has work to do, and
    // rewind HEAD too so the *local* history is genuinely behind.
    run(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
    run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    (dir, repo, ahead)
}

/// [`GitOperation::FetchRemote`] runs for real through the whole pipeline
/// (M2.20c, #229): build → validate → enforce_fresh → execute, against a
/// configured remote holding a commit this repository does not have.
///
/// Three assertions, and the *second* is the one that matters:
///
/// 1. The response is a `200` carrying a parseable [`FetchSuccess`].
/// 2. `refs/remotes/origin/main` actually points at the remote's tip
///    afterwards — read out of the repository, not out of the response. A
///    handler that returned a well-formed success body without fetching
///    anything passes (1) and fails this.
/// 3. The reported `updated_refs` matches what the repository shows, so the
///    wire answer and the observed answer cannot drift apart.
#[tokio::test]
async fn fetch_remote_executes_through_the_pipeline() {
    let (_dir, repo, ahead) = repo_behind_its_remote();
    assert!(
        !std::path::Path::new(&repo)
            .join(".git/refs/remotes/origin/main")
            .exists(),
        "the fixture must start with no remote-tracking ref, or 'the fetch \
         created it' proves nothing"
    );

    let (status, body) = pipeline(
        &repo,
        GitOperation::FetchRemote {
            remote: RemoteName::new("origin").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let observed = out(&repo, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(
        observed, ahead,
        "the fetch must have moved the remote-tracking ref to the remote's tip"
    );

    let success: git_vista_protocol::FetchSuccess =
        serde_json::from_str(&body).expect("a 200 from /api/fetch is a FetchSuccess");
    assert_eq!(success.remote, "origin");
    assert_eq!(
        success.updated_refs,
        vec![git_vista_protocol::RemoteRefUpdate {
            ref_name: "refs/remotes/origin/main".to_string(),
            old_oid: None,
            new_oid: Some(ahead),
        }],
        "the reported update must be the one the repository actually shows"
    );
}

/// The paired no-op leg: a second fetch, with nothing new on the remote,
/// still succeeds and reports **no** updates.
///
/// This is what stops `updated_refs` from being a rubber stamp. A
/// implementation that reported every remote-tracking ref it could see —
/// rather than the before/after difference — would pass the test above and
/// fail here, and a client would show "1 ref updated" every time a user
/// pressed Fetch on an up-to-date repository.
#[tokio::test]
async fn a_fetch_with_nothing_new_succeeds_and_reports_no_updates() {
    let (_dir, repo, _ahead) = repo_behind_its_remote();
    let op = || GitOperation::FetchRemote {
        remote: RemoteName::new("origin").unwrap(),
    };

    let (status, body) = pipeline(&repo, op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let before = repo_fingerprint(&repo);
    let (status, body) = pipeline(&repo, op()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let success: git_vista_protocol::FetchSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        success.updated_refs.is_empty(),
        "an up-to-date fetch must report nothing moved, got {:?}",
        success.updated_refs
    );
    assert!(
        success.message.contains("already up to date"),
        "{}",
        success.message
    );
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "a no-op fetch must leave the repository byte-identical"
    );
}

/// A fetch from a remote whose URL points at nothing fails with the typed
/// taxonomy rather than a bare 500 or an opaque message, and leaves the
/// repository untouched.
///
/// The remote is *configured* (so the plan's `RemoteConfigured` precondition
/// holds and execution is really reached) but its URL names a directory that
/// does not exist, which is a genuine transport failure git reports in its
/// own words.
#[tokio::test]
async fn a_fetch_from_a_broken_remote_is_classified_and_changes_nothing() {
    let (_dir, repo) = seeded_repo();
    let nowhere = repo.join("no-such-remote.git");
    run(
        &repo,
        &["remote", "add", "origin", &nowhere.display().to_string()],
    );

    let before = repo_fingerprint(&repo);
    let (status, body) = pipeline(
        &repo,
        GitOperation::FetchRemote {
            remote: RemoteName::new("origin").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let error: git_vista_protocol::FetchError =
        serde_json::from_str(&body).expect("a failed /api/fetch is a FetchError");
    assert_eq!(
        error.kind,
        git_vista_protocol::FetchFailureKind::RemoteRejected,
        "git's own words were: {}",
        error.message
    );
    assert!(
        error.updated_refs.is_empty(),
        "a fetch that never reached a remote cannot have moved a ref"
    );
    assert!(
        !error.message.is_empty(),
        "git's own explanation must be forwarded, whatever the tag says"
    );
    assert_eq!(
        without_fetch_head(&repo_fingerprint(&repo)),
        without_fetch_head(&before),
        "a failed fetch must move no ref and add no object"
    );
    assert_eq!(
        out(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/"]
        ),
        "",
        "a fetch that never reached a remote must have created no \
         remote-tracking ref"
    );
}

/// [`repo_fingerprint`] with its `fetch_head:` line dropped.
///
/// Measured against git 2.43.0: a `git fetch` that **fails** — the remote is
/// unreadable, nothing is negotiated, no object arrives — still creates
/// `.git/FETCH_HEAD` on its way to discovering that. That file names no ref,
/// holds no object, and nothing in this server reads it, so treating its
/// appearance as "the repository was mutated" would fail the inertness
/// assertion for a reason that has nothing to do with the repository's
/// contents.
///
/// Dropped only here, and only for the *failure* case. Every other use of the
/// fingerprint keeps the line — `repo_fingerprint_detects_every_change_it_
/// claims_to_watch` has a dedicated case proving it is load-bearing there —
/// and this test compensates by additionally asserting, directly, that no
/// remote-tracking ref exists afterwards.
fn without_fetch_head(fingerprint: &str) -> String {
    fingerprint
        .lines()
        .filter(|l| !l.starts_with("fetch_head:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The census of operations that claim to be cancellable is exactly
/// `{FetchRemote, PullBranch}` (M2.20c #229, widened by M2.20d #230).
///
/// `planner::honours_cancellation` is answered to an operator by
/// `POST /api/operations/{id}/cancel`: a `true` there is a promise that the
/// executor watches the latch. The match itself is exhaustive (a new variant
/// will not compile without an arm), but nothing in the compiler stops
/// someone from putting a new variant on the `true` side without an executor
/// to back it — so the `true` set is pinned here, and widening it is a
/// deliberate edit to this list.
///
/// The second half is the anti-vacuity leg: the executors those two dispatch
/// to must actually take a cancel signal. A `honours_cancellation` that
/// answered `true` for an executor which ignored the latch would be the exact
/// "tested but does nothing" shape this suite exists to catch.
///
/// Pull's promise is narrower than fetch's and the source check says so: its
/// fetch half runs through `planner::fetch`'s cancellable spawn, and
/// `planner::pull` reads the latch itself once more between the halves.
///
/// The *behavioural* proof that a cancelled pull does not integrate is
/// `pull_suite::a_cancelled_pull_does_not_integrate`, which drives the real
/// endpoint and then checks the repository. What the source assertion below
/// adds is narrower and worth being precise about: it pins that the
/// between-halves read exists at all. That read is defense in depth and is
/// **not** covered behaviourally — reaching it needs a cancel inside the
/// window between `git fetch` exiting and `git merge` spawning, and every way
/// to arrange that is a timing race. Deleting it leaves this whole suite
/// green; that is stated in `planner::pull` and in ADR 0044 rather than left
/// for a reader to discover.
#[test]
fn only_operations_with_a_real_cancellation_point_claim_to_be_cancellable() {
    let samples = samples();
    let cancellable: Vec<&GitOperation> = samples
        .iter()
        .filter(|op| super::honours_cancellation(op))
        .collect();
    assert_eq!(
        cancellable.len(),
        2,
        "the cancellable census changed — every `true` arm in \
         planner::honours_cancellation promises an executor that watches the \
         cancellation latch, so adding one means adding that executor too: \
         {cancellable:?}"
    );
    assert!(
        cancellable
            .iter()
            .any(|op| matches!(op, GitOperation::FetchRemote { .. })),
        "FetchRemote must stay cancellable, got {cancellable:?}"
    );
    assert!(
        cancellable
            .iter()
            .any(|op| matches!(op, GitOperation::PullBranch { .. })),
        "PullBranch is cancellable during its fetch half (#230), got {cancellable:?}"
    );

    let src = source("src/planner/fetch.rs");
    assert!(
        src.contains("crate::operations::cancel_signal()"),
        "planner::fetch must take the operation's cancellation latch — \
         honours_cancellation(FetchRemote) promises it does"
    );
    assert!(
        src.contains("git_streamed_for("),
        "planner::fetch must run its git through the streaming, cancellable \
         runner — the collecting `run_git` cannot be interrupted"
    );

    let pull_src = source("src/planner/pull.rs");
    assert!(
        pull_src.contains("run_fetch("),
        "planner::pull must reach the remote through planner::fetch's own \
         cancellable spawn, not a second one of its own (#230, ADR 0044)"
    );
    assert!(
        pull_src.contains("crate::operations::cancel_signal()"),
        "planner::pull must re-read the cancellation latch between the fetch \
         and the integration — honours_cancellation(PullBranch) promises a \
         cancel stops the local mutation, not merely the transfer"
    );
    assert!(
        !pull_src.contains("git_streamed_for(") && !pull_src.contains("\"fetch\""),
        "planner::pull must not spawn a fetch of its own — one `git fetch` in \
         this server (ADR 0044 D1), or the askpass hardening and redaction of \
         ADR 0036 have two places to drift apart in"
    );
}

/// [`GitOperation::PullBranch`] executes end-to-end through the pipeline
/// (M2.20d, #230): the fetch lands the remote's commits and the integration
/// moves the checked-out branch onto them.
///
/// Both strategies are driven, because the whole reason `MergeStrategy` is
/// mandatory is that the two do different things to history — an executor
/// that ran one arm for both inputs would satisfy a single-strategy test. What
/// they do *differently* is the sibling suite's
/// `merge_and_rebase_pulls_of_one_diverged_history_produce_different_histories`;
/// this test's job is the pipeline leg: build → validate → enforce_fresh →
/// execute, with the repository as referee.
#[tokio::test]
async fn pull_branch_executes_through_the_pipeline() {
    for strategy in [
        git_vista_protocol::MergeStrategy::Merge,
        git_vista_protocol::MergeStrategy::Rebase,
    ] {
        // The bare remote lives *inside* the served tree: #66 Task 6 grants the
        // served repository and the system trees and nothing else, so a remote
        // in a sibling tempdir is denied by the sandbox and every fetch fails
        // for a reason that has nothing to do with what is under test. Same
        // fixture shape as `planner::fetch_suite`.
        let (_dir, repo) = seeded_repo();
        let remote = repo.join("upstream.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "--bare", "-b", "main"]);
        run(
            &repo,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        run(&repo, &["push", "-q", "origin", "main"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
        run(&repo, &["push", "-q", "origin", "main"]);
        run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
        run(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);

        let behind = tip(&repo, "HEAD");
        let wanted = out(&remote, &["rev-parse", "main"]);
        assert_ne!(
            behind, wanted,
            "the fixture must actually be behind, or a pull that did nothing \
             would pass"
        );

        let (status, body) = pipeline(
            &repo,
            GitOperation::PullBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: branch("main"),
                strategy,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{strategy:?}: {body}");

        // The repository is the referee, not the response body.
        assert_eq!(
            tip(&repo, "HEAD"),
            wanted,
            "{strategy:?}: the pull must move the checked-out branch onto what \
             the remote had"
        );
        assert_eq!(
            tip(&repo, "refs/remotes/origin/main"),
            wanted,
            "{strategy:?}: the fetch half must have created the tracking ref"
        );

        let success: git_vista_protocol::PullSuccess = serde_json::from_str(&body).unwrap();
        assert_eq!(success.strategy, strategy, "the response echoes what ran");
        assert!(success.advanced, "{strategy:?}: {body}");
    }
}

/// The widened `PushBranch` combinations that M2.20a does **not** execute are
/// refused, and refused inertly — nothing reaches the remote.
///
/// This is the case that most needed writing down, because unlike fetch and
/// pull, `PushBranch` has a *live* executor sitting right next to the stub.
/// An arm that ignored the new fields would have run a perfectly ordinary
/// push — succeeding, mutating the remote, and reporting success for an
/// operation nobody approved. The remote's ref listing being empty afterwards
/// is what rules that out; the status code alone could not.
#[tokio::test]
async fn the_unwired_push_combinations_are_refused_without_touching_the_remote() {
    for force in [
        ForcePublish::None,
        ForcePublish::WithLease {
            expected_remote_tip: oid(&"0".repeat(40)),
        },
    ] {
        for set_upstream in [true, false] {
            if !set_upstream && force == ForcePublish::None {
                // The one combination that *does* execute — covered by
                // `push_branch_executes_through_the_pipeline` above, which
                // asserts the push really reaches the remote.
                continue;
            }
            let (dir, repo) = seeded_repo();
            let remote = dir.path().join("remote.git");
            std::fs::create_dir_all(&remote).unwrap();
            run(&remote, &["init", "-q", "--bare", "-b", "main"]);
            run(
                &repo,
                &["remote", "add", "origin", &remote.display().to_string()],
            );

            let before = repo_fingerprint(&repo);
            let (status, body) = pipeline(
                &repo,
                GitOperation::PushBranch {
                    branch: branch("main"),
                    remote: RemoteName::new("origin").unwrap(),
                    set_upstream,
                    force: force.clone(),
                },
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_IMPLEMENTED,
                "set_upstream={set_upstream} force={force:?}: {body}"
            );
            assert_eq!(
                out(&remote, &["for-each-ref", "refs/heads"]),
                "",
                "nothing may reach the remote for an unwired push combination \
                 (set_upstream={set_upstream} force={force:?})"
            );
            assert_eq!(
                repo_fingerprint(&repo),
                before,
                "the refusal must also leave the local repository untouched \
                 (set_upstream={set_upstream} force={force:?})"
            );
        }
    }
}

// --- #235 (M2.21a): typed tag vocabulary, execution not yet wired ----------

fn tname(s: &str) -> TagName {
    TagName::new(s).unwrap()
}

/// [`GitOperation::CreateTag`] proves its shape end-to-end through the real
/// pipeline, for **both kinds**, but M2.21a ships no execution — the later
/// M2.21 slices of #74 own it.
///
/// Both kinds are driven for the same reason `pull_branch` drives both
/// strategies: a stub that refused the annotated form and quietly executed
/// the lightweight one (or vice versa) would be invisible to a single-shape
/// test. The plan's shape is also pinned per kind — lightweight promises the
/// ref lands exactly at `target`, annotated honestly says `Computed` (the
/// ref will point at a tag object that does not exist yet).
#[tokio::test]
async fn create_tag_executes_through_the_pipeline() {
    for annotation in [
        None,
        Some(git_vista_protocol::TagAnnotation {
            message: git_vista_protocol::TagMessage::new("v1.0.0 — notes").unwrap(),
            sign: false,
        }),
    ] {
        let (_dir, repo) = seeded_repo();
        let target = tip(&repo, "HEAD");

        // The shape half: risk, CAS-style absence precondition, per-kind
        // after-state, and the delete-created recovery.
        let op = GitOperation::CreateTag {
            name: tname("v1.0.0"),
            target: oid(&target),
            annotation: annotation.clone(),
        };
        let (plan, _observed) = build_plan(&repo, op.clone(), tokens()).await;
        assert_eq!(plan.risk, RiskLevel::Reversible, "{annotation:?}");
        assert!(
            plan.preconditions.contains(&Precondition::RefAbsent {
                ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
            }),
            "creating a tag must be guarded on the tag not already existing"
        );
        let expected_after = match &annotation {
            None => RefState::At(oid(&target)),
            Some(_) => RefState::Computed,
        };
        assert_eq!(
            plan.expected_ref_changes,
            vec![RefChange {
                ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
                before: RefState::Absent,
                after: expected_after,
            }],
            "{annotation:?}"
        );
        assert_eq!(
            plan.recovery,
            RecoveryStrategy::DeleteCreatedTag {
                name: tname("v1.0.0"),
            }
        );

        // The stub half: refused, and provably inert.
        let before = repo_fingerprint(&repo);
        let (status, body) = pipeline(&repo, op).await;
        assert_eq!(
            status,
            StatusCode::NOT_IMPLEMENTED,
            "{annotation:?}: {body}"
        );
        assert_eq!(
            repo_fingerprint(&repo),
            before,
            "the {annotation:?} stub must leave the repository byte-identical — \
             M2.21a ships no tag-create execution (#74)"
        );
        // The paired positive for the inertness claim: the very mutation the
        // stub must not make *does* change the fingerprint when plain git
        // makes it, so the assertion above was capable of failing.
        run(&repo, &["tag", "v1.0.0", &target]);
        assert_ne!(
            repo_fingerprint(&repo),
            before,
            "creating the tag for real must move the fingerprint, or the \
             inertness assertion above is vacuous"
        );
    }
}

/// [`GitOperation::DeleteLocalTag`], same contract-only staging: the plan's
/// shape is proven against a *real* annotated tag, execution is refused, and
/// the tag demonstrably survives.
#[tokio::test]
async fn delete_local_tag_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1.0.0 — notes", "v1.0.0"]);

    let before = repo_fingerprint(&repo);
    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteLocalTag {
            name: tname("v1.0.0"),
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "the stub must leave the repository byte-identical — M2.21a ships no \
         tag-delete execution (#74)"
    );
    // Paired positive: really deleting the tag moves the fingerprint.
    run(&repo, &["tag", "-d", "v1.0.0"]);
    assert_ne!(
        repo_fingerprint(&repo),
        before,
        "deleting the tag for real must move the fingerprint, or the \
         inertness assertion above is vacuous"
    );
}

/// The decision #235 was told not to make by reflex, pinned against a real
/// repository: a `DeleteLocalTag` plan's CAS precondition and `RecreateTag`
/// recovery both carry the **unpeeled** ref value — for an annotated tag,
/// the tag *object's* oid — and not the peeled commit.
///
/// The negative half is the test: the two oids of a real annotated tag
/// genuinely differ, so asserting "recovery == tag object" here cannot pass
/// while the observation secretly peels. Recovery at the unpeeled oid is
/// what restores the original tag byte-identically (message, tagger,
/// signature); recovery at the peeled commit would silently demote an
/// annotated tag to a lightweight one — see `RecreateTag`'s doc in plan.rs.
#[tokio::test]
async fn delete_local_tag_recovery_carries_the_unpeeled_tag_object() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1.0.0 — notes", "v1.0.0"]);
    let tag_object = tip(&repo, "refs/tags/v1.0.0");
    let peeled_commit = tip(&repo, "refs/tags/v1.0.0^{}");
    assert_ne!(
        tag_object, peeled_commit,
        "an annotated tag's ref value must differ from its peeled commit, or \
         this test cannot tell the two apart and proves nothing"
    );

    let (plan, _observed) = build_plan(
        &repo,
        GitOperation::DeleteLocalTag {
            name: tname("v1.0.0"),
        },
        tokens(),
    )
    .await;
    assert_eq!(plan.risk, RiskLevel::Destructive);
    assert_eq!(
        plan.recovery,
        RecoveryStrategy::RecreateTag {
            name: tname("v1.0.0"),
            at: oid(&tag_object),
        },
        "recovery must carry the tag object (unpeeled), not the tagged commit"
    );
    assert_eq!(
        plan.preconditions,
        vec![Precondition::RefAt {
            ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
            oid: oid(&tag_object),
        }],
        "the CAS pin is the same unpeeled value the recovery restores"
    );
    assert_eq!(
        plan.expected_ref_changes,
        vec![RefChange {
            ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
            before: RefState::At(oid(&tag_object)),
            after: RefState::Absent,
        }]
    );
}

/// [`GitOperation::DeleteRemoteTag`], contract-only like the fetch/pull stubs
/// above and with the same reason to prove inertness hard: the remote here is
/// real, reachable, and holds the tag — a stub that answered `501` *after*
/// pushing the deletion would pass a status-only assertion while having
/// destroyed the remote's ref.
#[tokio::test]
async fn delete_remote_tag_executes_through_the_pipeline() {
    let (dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1", "v1.0.0"]);
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    run(&repo, &["push", "-q", "origin", "main", "v1.0.0"]);
    let remote_tags_before = out(&remote, &["for-each-ref", "refs/tags"]);
    assert!(
        remote_tags_before.contains("v1.0.0"),
        "the remote must really hold the tag, or 'nothing was deleted' is vacuous"
    );

    let before = repo_fingerprint(&repo);
    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteRemoteTag {
            name: tname("v1.0.0"),
            remote: RemoteName::new("origin").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert_eq!(
        out(&remote, &["for-each-ref", "refs/tags"]),
        remote_tags_before,
        "the remote's tag must survive the stub — M2.21a ships no execution (#74)"
    );
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "the stub must leave the local repository byte-identical too"
    );
}

/// [`GitOperation::PushTag`], contract-only: the remote is real and reachable
/// and does *not* have the tag, so a stub that pushed before refusing would
/// demonstrably leave `refs/tags/v1.0.0` on it.
#[tokio::test]
async fn push_tag_executes_through_the_pipeline() {
    let (dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1", "v1.0.0"]);
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    run(&repo, &["push", "-q", "origin", "main"]);
    assert_eq!(
        out(&remote, &["for-each-ref", "refs/tags"]),
        "",
        "the remote must start without the tag, or 'nothing was pushed' is vacuous"
    );

    let before = repo_fingerprint(&repo);
    let (status, body) = pipeline(
        &repo,
        GitOperation::PushTag {
            name: tname("v1.0.0"),
            remote: RemoteName::new("origin").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert_eq!(
        out(&remote, &["for-each-ref", "refs/tags"]),
        "",
        "no tag may reach the remote — M2.21a ships no push-tag execution (#74)"
    );
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "the stub must leave the local repository byte-identical too"
    );
}

// ---------------------------------------------------------------------------
// --- #247 (M2.23c): the build-only / submit-approved-plan seam. -------------
// --- `build_plan_only` must touch neither the guard nor `execute`; ----------
// --- `submit_plan` must take the same guard and refuse tampered/expired/ ----
// --- stale plans with the single-shot path's exact words; and the two -------
// --- paths must be byte-identical for every operation kind. -----------------
// ---------------------------------------------------------------------------

/// `git <args…>` with author/committer dates pinned, so two identically
/// seeded repositories get **identical commit oids** — what lets
/// [`the_split_path_is_byte_identical_to_the_single_shot_path`] compare
/// response bytes (some refusals embed the seed tip) across twin repos.
fn run_dated(repo: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
            .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed in {repo:?}"
    );
}

/// [`seeded_repo`] with the seed commit's dates pinned via [`run_dated`]:
/// every call yields a repository in the byte-identical state — same tree,
/// same tip oid, same generation inputs — so a pair of them are true twins.
fn seeded_repo_dated() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run_dated(&repo, &["commit", "-q", "-m", "seed"]);
    (dir, repo)
}

/// The split-path census (M2.23c, #247): every [`GitOperation`] variant maps
/// through [`covered_on_split_path`] to a **live** `#[tokio::test]`, and the
/// sweep that table vouches for must itself iterate [`samples`] — the same
/// census list — or the table would point at a test that quietly stopped
/// sweeping. Compile-time totality (no wildcard arm in the table) plus these
/// two source checks are what "a new variant cannot land covered on only one
/// path" means mechanically.
#[test]
fn every_operation_kind_is_covered_on_the_split_path() {
    let src = source("src/planner/contract_suite.rs");
    for op in samples() {
        let name = covered_on_split_path(&op);
        assert!(
            src.contains(&format!("#[tokio::test]\nasync fn {name}(")),
            "covered_on_split_path names ‘{name}’ but no live #[tokio::test] with \
             that name exists"
        );
    }
    let sweep = fn_body(
        &src,
        "the_split_path_is_byte_identical_to_the_single_shot_path",
    );
    assert!(
        sweep.contains("samples()"),
        "the split-path sweep no longer iterates the samples() census — \
         covered_on_split_path's per-variant claim just went vacuous"
    );
}

/// [`the_production_entry_point_composes_the_tested_stages_in_order`]'s
/// sibling for the submit stage: `submit_plan`'s body must re-observe through
/// the shared eyes, then compose guard → busy-check → validate →
/// enforce_fresh → execute — the same stage functions, in the same order, as
/// `plan_and_execute_in`. The existing pin test forces the composed path to
/// keep its stages inline, so the two compositions are necessarily separate
/// function bodies; this pin is what keeps them from drifting apart.
#[test]
fn the_submit_stage_composes_the_same_guarded_stages_in_order() {
    let src = source("src/planner.rs");
    let body = fn_body(&src, "submit_plan");
    let mut from = 0;
    for stage in [
        "observe_for_submission(",
        "coordinator::lock(",
        "coordinator::refuse_if_git_busy(",
        "validate(",
        "enforce_fresh(",
        "execute(",
    ] {
        match body[from..].find(stage) {
            Some(at) => from += at + stage.len(),
            None => panic!(
                "submit_plan no longer calls {stage} after the previous stage — \
                 the re-observe → guard → validate → enforce_fresh → execute \
                 composition is broken"
            ),
        }
    }
}

/// #247 acceptance 1, both halves, with the vacuity trap closed on each:
///
/// - **Build takes no guard.** Proven by *holding* the pipeline's own
///   mutation guard for the entire `build_plan_only` call: if building ever
///   acquires it, the call blocks against our held guard and the timeout
///   fails the test. And building mutates nothing — the full
///   [`repo_fingerprint`] (refs, HEAD, status, object count, config) and the
///   live generation token are unchanged after the call.
/// - **The held guard is the real one.** A test that holds an unrelated lock
///   would pass vacuously, so the same test proves `submit_plan` *does* queue
///   on exactly this guard: polled for two full seconds it stays pending and
///   the branch it would create stays absent (an unguarded submit finishes in
///   well under that), and the moment the guard drops it completes and the
///   branch exists.
#[tokio::test]
async fn building_a_plan_takes_no_guard_and_submitting_takes_the_real_one() {
    let (_dir, repo) = seeded_repo();
    let at = tip(&repo, "HEAD");
    let op = GitOperation::CreateBranch {
        name: branch("seam"),
        at: oid(&at),
    };

    let fingerprint_before = repo_fingerprint(&repo);
    let generation_before = generation_token(&repo, &observe_live(&repo).await).await;

    // Hold the exact guard the pipeline serializes on (repo_id None ⇒ the
    // Unregistered bucket, the one every injected-token suite drive uses).
    let held = crate::coordinator::lock(None).await;

    let plan = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        build_plan_only(&repo, op, tokens()),
    )
    .await
    .expect("build_plan_only blocked on the mutation guard — building must never lock");

    assert_eq!(
        repo_fingerprint(&repo),
        fingerprint_before,
        "build_plan_only must leave the repository byte-identical"
    );
    let generation_after = generation_token(&repo, &observe_live(&repo).await).await;
    assert_eq!(
        generation_after.as_str(),
        generation_before.as_str(),
        "build_plan_only must not move the repository's generation"
    );

    // The paired positive: submitting the same plan queues on the guard we
    // hold. Two seconds is ~20× an unguarded submit's runtime here, so a
    // submit that stopped taking the guard would finish (and create the
    // branch) well inside the window and fail both assertions.
    let submit = submit_plan(&repo, None, tokens(), plan);
    tokio::pin!(submit);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), submit.as_mut())
            .await
            .is_err(),
        "submit_plan completed while the mutation guard was held elsewhere — \
         it no longer takes the guard"
    );
    assert_eq!(
        out(&repo, &["branch", "--list", "seam"]),
        "",
        "nothing may execute while the guard is held elsewhere"
    );

    drop(held);
    let (status, body) = submit.await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "seam"), at, "the released submit must execute");
}

/// #247 acceptance 2: for **every** operation kind, `build_plan_only` then
/// `submit_plan` produces output byte-identical to the single-shot
/// `plan_and_execute_in` — same status, same body — proven against twin
/// repositories seeded into byte-identical states ([`seeded_repo_dated`], so
/// even refusals that embed the seed tip compare equal). This sweep is the
/// test [`covered_on_split_path`] vouches with; it iterates [`samples`], the
/// same census the single-shot coverage test uses.
#[tokio::test]
async fn the_split_path_is_byte_identical_to_the_single_shot_path() {
    for op in samples() {
        let (_dir_single, repo_single) = seeded_repo_dated();
        let (_dir_split, repo_split) = seeded_repo_dated();

        let single_shot = pipeline(&repo_single, op.clone()).await;

        let plan = build_plan_only(&repo_split, op.clone(), tokens()).await;
        let split = submit_plan(&repo_split, None, tokens(), plan).await;

        assert_eq!(
            single_shot, split,
            "the split path diverged from the single-shot path for {op:?}"
        );
    }
}

/// #247 acceptance 3, the staleness leg — the property that makes a client
/// review roundtrip safe at all: the repository moves between build and
/// submit (the review window), and `submit_plan` refuses with
/// `enforce_fresh`'s exact single-shot words and mutates nothing. The paired
/// positive: a plan rebuilt against the moved repository submits and
/// executes.
#[tokio::test]
async fn a_stale_plan_is_refused_at_submit_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    let at = tip(&repo, "HEAD");
    let plan = build_plan_only(
        &repo,
        GitOperation::CreateBranch {
            name: branch("late"),
            at: oid(&at),
        },
        tokens(),
    )
    .await;

    // The review window: the repository moves while the plan is out for
    // approval.
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    run(&repo, &["commit", "-q", "-m", "moved during review"]);

    let (status, why) = submit_plan(&repo, None, tokens(), plan).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("changed while this plan was pending"), "{why}");
    assert_eq!(
        out(&repo, &["branch", "--list", "late"]),
        "",
        "the refused stale plan must not have created the branch"
    );

    // Rebuilt against the live state, the same intent goes through.
    let fresh_at = tip(&repo, "HEAD");
    let plan = build_plan_only(
        &repo,
        GitOperation::CreateBranch {
            name: branch("fresh"),
            at: oid(&fresh_at),
        },
        tokens(),
    )
    .await;
    let (status, body) = submit_plan(&repo, None, tokens(), plan).await;
    assert_ok(status, &body);
    assert_eq!(tip(&repo, "fresh"), fresh_at);
}

/// #247 acceptance 3, the tamper leg: an operation swapped under its hash
/// after `build_plan_only` is refused by `submit_plan` at `validate`, with
/// the single-shot path's exact words, and the smuggled operation never runs.
#[tokio::test]
async fn a_tampered_plan_is_refused_at_submit_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "side"]);
    let mut plan = build_plan_only(&repo, GitOperation::StageAll, tokens()).await;
    plan.operation = GitOperation::ForceDeleteBranch {
        branch: branch("side"),
    };
    let (status, why) = submit_plan(&repo, None, tokens(), plan).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("doesn't match"), "{why}");
    assert_ne!(
        out(&repo, &["branch", "--list", "side"]),
        "",
        "the smuggled force-delete must not have run"
    );
}

/// #247 acceptance 3, the expiry leg: a plan past `PLAN_TTL_SECS` is refused
/// by `submit_plan` with the single-shot path's exact words and the approved
/// commit is never written.
#[tokio::test]
async fn an_expired_plan_is_refused_at_submit_and_mutates_nothing() {
    let (_dir, repo) = seeded_repo();
    let before = tip(&repo, "HEAD");
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    run(&repo, &["add", "b.txt"]);
    let mut plan = build_plan_only(
        &repo,
        GitOperation::CommitOnHead {
            message: message("too late"),
            allow_empty: false,
        },
        tokens(),
    )
    .await;
    plan.expires_at = UnixSeconds(crate::activity::now_secs() - 1);
    let (status, why) = submit_plan(&repo, None, tokens(), plan).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("expired"), "{why}");
    assert_eq!(
        tip(&repo, "HEAD"),
        before,
        "the expired commit must not land"
    );
}

/// The cross-selection guard, with its vacuity trap sprung on purpose: a plan
/// built for one selection may not submit against another, **and the
/// generation token provably cannot be the thing that stops it** — twin
/// repositories in byte-identical states share a generation (it digests
/// HEAD/refs/status, nothing identifying the repository), so the control leg
/// shows the same foreign plan *executing* once the tokens are made to
/// match. The token check is load-bearing, not decorative.
#[tokio::test]
async fn a_plan_built_for_another_selection_is_refused_at_submit() {
    let (_dir_a, repo_a) = seeded_repo_dated();
    let (_dir_b, repo_b) = seeded_repo_dated();
    let at = tip(&repo_a, "HEAD");
    assert_eq!(at, tip(&repo_b, "HEAD"), "twins must share their seed tip");

    let plan = build_plan_only(
        &repo_a,
        GitOperation::CreateBranch {
            name: branch("crossed"),
            at: oid(&at),
        },
        tokens(),
    )
    .await;

    // Submitted against repo_b under a *different* selection: refused before
    // anything is observed or locked.
    let other = (
        RepositoryToken::new("other-repo").unwrap(),
        WorktreeToken::new("other-worktree").unwrap(),
    );
    let (status, why) = submit_plan(&repo_b, None, other, plan.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{why}");
    assert!(why.contains("different repository or worktree"), "{why}");
    assert_eq!(
        out(&repo_b, &["branch", "--list", "crossed"]),
        "",
        "the cross-selection submit must not have executed"
    );

    // The control: with the tokens matching, the same foreign plan sails
    // through `enforce_fresh` against the twin — its generation matches —
    // and executes. This is the leg that proves the refusal above came from
    // the token check and could not have come from the generation.
    let (status, body) = submit_plan(&repo_b, None, tokens(), plan).await;
    assert_ok(status, &body);
    assert_eq!(
        tip(&repo_b, "crossed"),
        at,
        "with matching tokens the twin's generation admits the foreign plan — \
         the token check above is the only thing standing between selections"
    );
}

/// `observe_for_submission`'s `held_at_build` re-derivation is load-bearing,
/// not decorative — the mutation `observed.held_at_build = Vec::new()` after
/// the re-observe passed this entire suite before this test existed. The
/// re-derived census is what arms `enforce_fresh`'s per-precondition live
/// recheck on the split path, and the one window where that recheck is the
/// *only* defence is a **generation-invisible** break (`RemoteConfigured`:
/// remotes live in config, which no generation input digests) landing between
/// `submit_plan`'s pre-guard observation and its post-guard gate.
///
/// The test makes that window deterministic the same way
/// [`building_a_plan_takes_no_guard_and_submitting_takes_the_real_one`] does:
/// hold the real mutation guard, start the submit (it observes — remote still
/// configured, so the re-derived census reads *held* — then queues on our
/// guard), break the precondition while it queues, release the guard. The
/// gate's live recheck must refuse with `verify_precondition`'s own 409 —
/// specifically the "no longer configured" wording, which is what
/// distinguishes *this* path from the never-held one.
///
/// If the re-derivation is ever dropped or emptied, `held_at_build` reads
/// false, `enforce_fresh` skips the live recheck, and the refusal becomes
/// `unmet_at_build`'s "not configured" instead (ADR 0044 — before it, the
/// push reached `exec_push` and git answered a 400). Either way the wording
/// assertion below fails, which is the property that matters: the mutation
/// is still caught, and now it is caught without a git process ever running.
#[tokio::test]
async fn a_generation_invisible_break_while_queued_is_refused_by_the_gates_live_recheck() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["remote", "add", "origin", "/nowhere/upstream.git"]);
    let op = GitOperation::PushBranch {
        branch: branch("main"),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream: false,
        force: ForcePublish::None,
    };
    let plan = build_plan_only(&repo, op, tokens()).await;
    // Sanity: the shape still pins the remote — without this precondition the
    // scenario below would silently stop testing the recheck at all.
    assert!(
        plan.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::RemoteConfigured { .. })),
        "PushBranch no longer carries RemoteConfigured — this test's premise is gone"
    );

    let held = crate::coordinator::lock(None).await;
    let submit = submit_plan(&repo, None, tokens(), plan);
    tokio::pin!(submit);
    // Two seconds is ~20× an unguarded submit's runtime (see the guard test
    // above): by the time this times out, `observe_for_submission` has read
    // the still-configured remote and the future is queued on the held guard.
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), submit.as_mut())
            .await
            .is_err(),
        "submit_plan completed while the mutation guard was held elsewhere"
    );

    // The break lands inside the guarded window, and it is generation-
    // invisible: removing a never-fetched remote touches only .git/config —
    // no ref, no HEAD, no status input moves.
    run(&repo, &["remote", "remove", "origin"]);

    drop(held);
    let (status, why) = submit.await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the gate's live recheck must refuse the vanished remote, not let the \
         executor stumble over it: {why}"
    );
    assert!(
        why.contains("no longer configured"),
        "expected verify_precondition's own refusal, got: {why}"
    );
}

/// The review-window half of the same corner, and the exact claim
/// `submit_plan`'s doc and ADR 0042 §3 make in prose: a `RemoteConfigured`
/// precondition that held at build and silently broke **before** submit is
/// re-derived as never-held (the census is re-read at submission) — "from the
/// submitter's seat the two cases are genuinely indistinguishable, and both
/// fail closed".
///
/// What both cases fail closed *with* changed in ADR 0044. This test used to
/// document them as flowing to the executor's legacy refusal; for
/// `RemoteConfigured` there is no such refusal (git reinterprets an unknown
/// remote as a transport target rather than rejecting it), so
/// `enforce_fresh` now refuses it directly via
/// `planner::refuses_when_unmet_at_build`. The assertion this test actually
/// makes — that the two paths are byte-identical and both refuse — is
/// unchanged and still holds, which is why it is the assertion and not the
/// prose that was load-bearing. Proven, not asserted: twin repositories, one running the
/// single-shot path with the remote *never* configured, one running the split
/// path with the remote removed during the review window, must refuse with
/// byte-identical status and body — and refuse, full stop.
#[tokio::test]
async fn review_window_remote_drift_fails_closed_with_the_never_configured_refusal() {
    let push = || GitOperation::PushBranch {
        branch: branch("main"),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream: false,
        force: ForcePublish::None,
    };

    // Twin A: the single-shot path with the precondition never held — the
    // executor's legacy refusal in its own words.
    let (_dir_a, repo_a) = seeded_repo_dated();
    let single_shot = pipeline(&repo_a, push()).await;
    assert!(
        !single_shot.0.is_success(),
        "a push with no remote configured must fail: {}",
        single_shot.1
    );

    // Twin B: the split path, precondition held at build, broken during the
    // review window (generation-invisible — config only, see the guarded-
    // window test above).
    let (_dir_b, repo_b) = seeded_repo_dated();
    run(
        &repo_b,
        &["remote", "add", "origin", "/nowhere/upstream.git"],
    );
    let plan = build_plan_only(&repo_b, push(), tokens()).await;
    assert!(
        plan.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::RemoteConfigured { .. })),
        "PushBranch no longer carries RemoteConfigured — this test's premise is gone"
    );
    run(&repo_b, &["remote", "remove", "origin"]);
    let fingerprint = repo_fingerprint(&repo_b);

    let split = submit_plan(&repo_b, None, tokens(), plan).await;
    assert!(
        !split.0.is_success(),
        "review-window remote drift must fail closed on the split path: {}",
        split.1
    );
    assert_eq!(
        single_shot, split,
        "drift during the review window must be indistinguishable from a \
         precondition that never held — same status, same words"
    );
    assert_eq!(
        repo_fingerprint(&repo_b),
        fingerprint,
        "the refused push must leave the repository byte-identical"
    );
}

/// [`review_window_remote_drift_fails_closed_with_the_never_configured_refusal`]'s
/// sibling for the *other* generation-invisible precondition, `SeedRecorded`
/// (seed files live under `.git/git-vista/`, outside every generation input).
/// The executor's independent re-read of the seed is the legacy guard the ADR
/// leans on; this pins that it actually refuses — and that the repository the
/// reset would have rewound stays untouched, which is the assertion with
/// teeth: the repo is deliberately drifted past its seed, so a reset that
/// wrongly ran would move `main` and delete the stray branch.
#[tokio::test]
async fn review_window_seed_drift_fails_closed_with_the_never_recorded_refusal() {
    // Twin A: single-shot, no seed ever recorded — the executor's 404.
    let (_dir_a, repo_a) = seeded_repo();
    let single_shot = pipeline(&repo_a, GitOperation::ResetTestRepo).await;
    assert_eq!(
        single_shot.0,
        StatusCode::NOT_FOUND,
        "a reset with no recorded seed must 404: {}",
        single_shot.1
    );

    // Twin B: seed recorded, repo drifted past it, plan built (SeedRecorded
    // holds), then the seed vanishes during the review window.
    let (_dir_b, repo_b) = seeded_repo();
    let seeded = tip(&repo_b, "HEAD");
    let state = repo_b.join(".git/git-vista");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("seed-refs"), format!("{seeded} main\n")).unwrap();
    std::fs::write(state.join("seed-head"), "main\n").unwrap();
    std::fs::write(repo_b.join("junk.txt"), "j\n").unwrap();
    run(&repo_b, &["add", "junk.txt"]);
    run(&repo_b, &["commit", "-q", "-m", "past the seed"]);
    run(&repo_b, &["branch", "stray"]);

    let plan = build_plan_only(&repo_b, GitOperation::ResetTestRepo, tokens()).await;
    assert!(
        plan.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::SeedRecorded)),
        "ResetTestRepo no longer carries SeedRecorded — this test's premise is gone"
    );
    let drifted = tip(&repo_b, "HEAD");
    std::fs::remove_file(state.join("seed-refs")).unwrap();
    std::fs::remove_file(state.join("seed-head")).unwrap();

    let split = submit_plan(&repo_b, None, tokens(), plan).await;
    assert_eq!(
        single_shot, split,
        "seed drift during the review window must be indistinguishable from a \
         seed that was never recorded — same status, same words"
    );
    assert_eq!(
        tip(&repo_b, "main"),
        drifted,
        "the refused reset must not have rewound the branch to its seed"
    );
    assert_ne!(
        out(&repo_b, &["branch", "--list", "stray"]),
        "",
        "the refused reset must not have deleted the stray branch"
    );
}
