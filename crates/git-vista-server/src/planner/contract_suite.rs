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
//!     [`covered_by`] matches exhaustively over the enum, so adding a
//!     sixteenth variant refuses to compile until it gets a pipeline test.
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
struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
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
    let daemon = std::process::Command::new("git")
        .args([
            "daemon",
            "--reuseaddr",
            "--listen=127.0.0.1",
            "--port=9418",
            "--export-all",
            "--enable=receive-pack",
            &format!("--base-path={}", dir.path().display()),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("git daemon spawns");
    let _daemon = DaemonGuard(daemon);
    let ready = (0..50).any(|_| {
        std::net::TcpStream::connect(("127.0.0.1", 9418))
            .map(|_| true)
            .unwrap_or_else(|_| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            })
    });
    assert!(ready, "git daemon never came up on 127.0.0.1:9418");

    run(
        &repo,
        &["remote", "add", "origin", "git://127.0.0.1:9418/remote.git"],
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
        ("/api/unstage", "unstage_all"),
        ("/api/undo", "activity::undo"),
        ("/api/merge", "merge_branch"),
        ("/api/push", "push_branch"),
        ("/api/delete-branch", "delete_branch"),
        ("/api/checkout", "checkout_branch"),
        ("/api/force-delete-branch", "force_delete_branch"),
        ("/api/rebase", "rebase"),
        ("/api/reset-test-repo", "reset_test_repo"),
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
