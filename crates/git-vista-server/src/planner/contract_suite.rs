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
//!     variant refuses to compile until it gets a pipeline test. (Two
//!     exceptions today: `FetchRemote` and `PullBranch` (M2.20a #227) ship no
//!     execution — their pipeline tests assert the *stubs'* refusal and that
//!     the repository stayed byte-identical, the honest version of this
//!     layer's claim until #229/#230 wire real execution in. `AmendCommit`
//!     was staged the same way by #222 and graduated when #223 wired
//!     `exec_amend_commit`; all four M2.21a (#235) tag operations graduated
//!     the same way — `CreateTag`/`DeleteLocalTag` when M2.21d (#238, ADR
//!     0048) wired theirs, `DeleteRemoteTag`/`PushTag` when M2.21f (#240)
//!     wired theirs — and every one of those inertness stubs was **replaced**
//!     by a real execution test rather than kept alongside, since an
//!     inertness assertion that survives the wiring it was guarding is a
//!     test asserting the opposite of the contract.)

//!     exceptions today: none remain among the tag operations — all four
//!     M2.21a (#235) tag variants now execute for real, most recently
//!     `DeleteRemoteTag`/`PushTag` when M2.21f (#240) wired theirs. Their
//!     pipeline tests used to assert the *stubs'* refusal and that the
//!     repository stayed byte-identical; each was **replaced** by a real
//!     execution test on graduation, the honest version of this layer's
//!     claim, not kept alongside it. `AmendCommit` was staged the same way
//!     by #222 and graduated to a real execution test when #223 wired
//!     `exec_amend_commit`; `FetchRemote` graduated the same way when M2.20c
//!     #229 wired `planner::fetch`, and `PullBranch` when M2.20d #230 wired
//!     `planner::pull`. Their heavier behavioural coverage — live progress, a
//!     cancel that kills the child, the dropped-connection replay, redaction
//!     on the streaming path, the merge-vs-rebase history difference, the
//!     conflict abort — lives in the siblings [`super::fetch_suite`] and
//!     [`super::pull_suite`].)
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

/// [`pipeline`] driven inside a tracked operation's progress scope — the shape
/// production runs in, since `plan_and_execute_tracked` wraps the pipeline in
/// `operations::with_progress`.
///
/// The difference that matters here is `planner::pin_recovery`: it names the
/// recovery ref after the operation's id, so it is a no-op under the plain
/// [`pipeline`] above (no id to name) and writes the pin under this one. A test
/// about what the pin *does* therefore has to drive this, or it would be
/// pinning by hand and proving nothing about production's ordering.
async fn tracked_pipeline(repo: &Path, op: GitOperation, key: &str) -> (StatusCode, String) {
    let hash = operation_hash(&op);
    let (repository, worktree) = tokens();
    let k = IdempotencyKey::new(format!("contract-{key}")).unwrap();
    let (handle, record) =
        match crate::operations::admit(&k, &op, &hash, repository, worktree, None) {
            crate::operations::Admission::Fresh(handle, record) => (handle, record),
            _ => panic!("‘{key}’ must be a fresh idempotency key in this binary"),
        };
    let out = crate::operations::with_progress(
        record,
        plan_and_execute_in(repo, None, tokens(), op.clone()),
    )
    .await;
    handle.finish(out.0, out.1.clone(), None);
    out
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
        GitOperation::PushStash { .. } => "push_stash_executes_through_the_pipeline",
        GitOperation::ApplyStash { .. } => "apply_stash_executes_through_the_pipeline",
        GitOperation::PopStash { .. } => "pop_stash_refuses_to_report_complete_while_conflicted",
        GitOperation::BranchFromStash { .. } => {
            "branch_from_stash_lands_a_stash_that_would_not_pop"
        }
        GitOperation::DropStash { .. } => "drop_stash_refuses_a_moved_selector",
        GitOperation::ResolveConflict { .. } => "resolve_conflict_executes_through_the_pipeline",
        GitOperation::ResolveConflictContent { .. } => {
            "resolve_conflict_content_executes_through_the_pipeline"
        }
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
        GitOperation::RevertMerge { .. } => "reverting_a_merge_needs_a_mainline_and_says_why",
        GitOperation::CherryPick { .. } => "a_cherry_pick_lands_a_commit_from_another_branch",
        GitOperation::SequenceContinue => "a_resolved_conflict_lets_the_sequence_continue",
        GitOperation::SequenceSkip => "skipping_drops_one_commit_and_keeps_going",
        GitOperation::SequenceAbort => "aborting_with_no_sequence_in_progress_is_refused",
        GitOperation::CherryPickMerge { .. } => "cherry_picking_a_merge_needs_a_mainline",
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
        GitOperation::PushStash { .. }
        | GitOperation::ApplyStash { .. }
        | GitOperation::PopStash { .. }
        | GitOperation::BranchFromStash { .. }
        | GitOperation::DropStash { .. }
        | GitOperation::ResolveConflict { .. }
        | GitOperation::ResolveConflictContent { .. }
        | GitOperation::CreateBranch { .. }
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
        | GitOperation::RevertMerge { .. }
        | GitOperation::CherryPick { .. }
        | GitOperation::CherryPickMerge { .. }
        | GitOperation::SequenceContinue
        | GitOperation::SequenceSkip
        | GitOperation::SequenceAbort
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
        GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
        },
        GitOperation::ResolveConflictContent {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            expected_stages: [Some(oid(&zeros)), Some(oid(&zeros)), Some(oid(&zeros))],
            expected_source: GenerationToken::new("conflict-v1:census").unwrap(),
            content: "resolved\n".to_string(),
        },
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

/// M3.24 (#77): a stash push rides the full pipeline and the drawer gains an
/// entry.
///
/// MUTATION: drop `--include-untracked` from the executor's argv and this goes
/// red — `new.txt` is untracked, so without the flag it stays in the tree and
/// the working directory is not clean afterwards.
#[tokio::test]
async fn push_stash_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();

    let (status, body) = pipeline(
        &repo,
        GitOperation::PushStash {
            message: Some(git_vista_protocol::StashMessage::new("wip").unwrap()),
            keep_index: false,
            include_untracked: true,
        },
    )
    .await;
    assert_ok(status, &body);

    let listed = out(&repo, &["stash", "list"]);
    assert!(
        listed.contains("wip"),
        "the drawer must hold the entry: {listed}"
    );
    assert_eq!(
        out(&repo, &["status", "--porcelain"]),
        "",
        "an --include-untracked push leaves a clean tree"
    );
}

/// M3.24 (#77): apply restores the changes and KEEPS the entry — the property
/// that distinguishes it from pop, and the reason pop is not in this slice.
///
/// MUTATION: make the executor run `stash pop` instead of `stash apply` and
/// this goes red on the still-listed assertion.
#[tokio::test]
async fn apply_stash_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "wip"]);
    let oid = out(&repo, &["rev-parse", "stash@{0}"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::ApplyStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(oid.clone()).unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);

    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "a changed\n",
        "the stash's changes are back in the tree"
    );
    assert!(
        out(&repo, &["stash", "list"]).contains("wip"),
        "apply KEEPS the entry — that is what makes it not a pop"
    );
}

/// M3.24 (#77): **the safety property of the whole write path.** A selector is
/// an index into a reflog, and every drop renumbers it. This drives the exact
/// race: plan against `stash@{0}`, let the drawer move underneath, then submit.
/// The compare-and-swap must refuse rather than drop a stash the user never
/// chose.
///
/// MUTATION: delete the `stash_entry_still_at` call from `exec_drop_stash` and
/// this goes red — the operation succeeds and destroys the wrong entry.
#[tokio::test]
async fn drop_stash_refuses_a_moved_selector() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "first\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "target"]);
    let target_oid = out(&repo, &["rev-parse", "stash@{0}"]);

    // Someone stashes again: "target" is now stash@{1}, and stash@{0} is a
    // different entry entirely.
    std::fs::write(repo.join("a.txt"), "second\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "innocent"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::DropStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(target_oid).unwrap(),
        },
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "a moved selector must refuse, not drop whatever now sits there: {body}"
    );
    let listed = out(&repo, &["stash", "list"]);
    assert!(
        listed.contains("innocent"),
        "the entry that moved into the slot must survive: {listed}"
    );
    assert!(
        listed.contains("target"),
        "and so must the intended one: {listed}"
    );
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

// --- #308: git 2.43 has no `revert --allow-empty` — the three diff-empty
// triggers, plus the failure-cleanup subtlety the two-step fix introduces --

/// #308, trigger 1: reverting a commit whose own diff was already empty (a
/// bare `git commit --allow-empty`). On this box's git 2.43 (verified via
/// `git --version` == 2.43.0), the single `git revert --no-edit <commit>`
/// `exec_revert` currently runs (planner.rs:3362-3395) fails outright —
/// `revert` gained `--allow-empty` only in 2.45 — so "undo" for a no-op
/// commit is undoable in name only. Empirically reproduced in a disposable
/// scratch repo before writing this: `git revert --no-edit <noop-sha>`
/// exits 1 with "nothing to commit, working tree clean". The fix is the
/// two-step `revert --no-commit` + `commit --allow-empty --no-edit`; this
/// asserts the whole pipeline, not just the git invocation, ends in a real
/// inverse commit.
#[tokio::test]
async fn revert_of_an_empty_commit_succeeds_with_an_inverse_empty_commit() {
    let (_dir, repo) = seeded_repo();
    run(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "noop change"],
    );
    let noop = tip(&repo, "HEAD");
    let before_count: u32 = out(&repo, &["rev-list", "--count", "HEAD"])
        .parse()
        .unwrap();

    let (status, body) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&noop) }).await;
    assert_ok(status, &body);

    assert_ne!(
        tip(&repo, "HEAD"),
        noop,
        "a successful revert must land a new commit, not silently no-op"
    );
    assert!(
        out(&repo, &["log", "-1", "--format=%s"]).starts_with("Revert"),
        "the new commit must be the revert, not something else"
    );
    assert_eq!(
        out(&repo, &["rev-list", "--count", "HEAD"]),
        (before_count + 1).to_string(),
        "exactly one new commit — the inverse — must land"
    );
    assert_eq!(
        out(&repo, &["status", "--porcelain"]),
        "",
        "the working tree must stay clean"
    );
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
}

/// #308, trigger 2: reverting a commit that is not in HEAD's own history at
/// all — the live repro (84570fe, an orphan lineage). `X`'s forward diff is
/// real, but it was never merged into the checked-out branch, so reversing
/// it against main's current tree is a no-op: the pre-change content the
/// reverse patch would produce is already what's on disk. Same git-2.43
/// failure as trigger 1, reached a different way — proving the bug isn't
/// specific to `--allow-empty` commits, it's specific to an empty DIFF
/// against HEAD however that's reached. Confirmed no fixture leak by
/// asserting X is genuinely not an ancestor of main (a raw `merge-base
/// --is-ancestor` check, not the `out()` helper, which asserts success and
/// would panic on the expected-nonzero exit).
#[tokio::test]
async fn revert_of_a_commit_not_in_head_succeeds_when_its_reverse_diff_is_already_a_no_op() {
    let (_dir, repo) = seeded_repo();
    // main sits at the seed (a.txt = "a\n"). A sibling branch changes a.txt
    // and is never merged — X is reachable only via `other`, not via main's
    // history, matching the "not in HEAD" trigger.
    run(&repo, &["checkout", "-q", "-b", "other"]);
    std::fs::write(repo.join("a.txt"), "z\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "change a to z on other"]);
    let x = tip(&repo, "other");
    run(&repo, &["checkout", "-q", "main"]);
    let before = tip(&repo, "main");

    let x_is_ancestor_of_main = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &x, "main"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success();
    assert!(
        !x_is_ancestor_of_main,
        "fixture error: X must NOT be reachable from main, or this isn't \
         actually the not-in-HEAD trigger"
    );

    let (status, body) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&x) }).await;
    assert_ok(status, &body);
    assert_ne!(
        tip(&repo, "HEAD"),
        before,
        "a new revert commit must land on main"
    );
    assert!(out(&repo, &["log", "-1", "--format=%s"]).starts_with("Revert"));
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// #308, trigger 3: reverting the SAME commit a second time. The first
/// revert has a real, non-empty diff and already works on today's code
/// (proven inline, not assumed); the second is what breaks — by the time it
/// runs, the tree already matches what the reverse patch would produce, so
/// the diff against HEAD is empty again, the same class of failure as
/// triggers 1 and 2 reached a third way.
#[tokio::test]
async fn reverting_an_already_reverted_commit_succeeds_again() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "b\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "change a to b"]);
    let c = tip(&repo, "HEAD");

    // First revert: non-empty diff, already works on today's code — not the
    // regression under test, just fixture setup for the second one.
    let (status1, body1) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&c) }).await;
    assert_ok(status1, &body1);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    let after_first = tip(&repo, "HEAD");
    let count_after_first: u32 = out(&repo, &["rev-list", "--count", "HEAD"])
        .parse()
        .unwrap();

    // Second revert of the SAME commit: the diff against HEAD is now empty.
    let (status2, body2) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&c) }).await;
    assert_ok(status2, &body2);
    assert_ne!(
        tip(&repo, "HEAD"),
        after_first,
        "the second revert must land its own new commit"
    );
    assert_eq!(
        out(&repo, &["rev-list", "--count", "HEAD"]),
        (count_after_first + 1).to_string()
    );
    assert!(out(&repo, &["log", "-1", "--format=%s"]).starts_with("Revert"));
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(out(&repo, &["status", "--porcelain"]), "");
}

/// #308's own "critical subtlety": once the fix is a two-step `revert
/// --no-commit` + `commit --allow-empty --no-edit`, a failure can now
/// happen at the SECOND step (the commit) — not only the first, which is
/// all today's single-step code can ever fail at. A rejecting hook only
/// ever runs on `git commit`, so it can only fire here once the fix's
/// second step exists at all. Proven with a hook that both writes a marker
/// (so we know it genuinely ran) and rejects (so the commit fails): the
/// marker's presence is exactly what a naive revert back to the single-step
/// call cannot produce, because git bails out on "nothing to commit" before
/// any hook ever runs for an empty-diff commit (empirically confirmed on
/// this box's git 2.43: same hook script installed, single-step revert on
/// an empty-diff commit leaves the marker absent). Cleanup must still leave
/// the repository exactly as if nothing had been attempted — proving `git
/// revert --abort` is correct cleanup after a failed STEP-2 commit, not
/// only after a failed step-1 compute (empirically confirmed separately:
/// REVERT_HEAD is cleared by git only on a successful commit, never a
/// failed one, so a failed step-2 commit leaves the identical sequencer
/// state a failed step-1 --no-commit would).
#[tokio::test]
async fn a_hook_rejected_commit_step_is_cleaned_up_after_the_hook_actually_ran() {
    let (_dir, repo) = seeded_repo();
    run(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "noop change"],
    );
    let noop = tip(&repo, "HEAD");
    let commit_count = out(&repo, &["rev-list", "--count", "HEAD"]);

    let marker = repo.join(".git/hook-ran-marker");
    std::fs::write(
        repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran-marker\"\nexit 1\n",
    )
    .unwrap();
    make_executable(&repo.join(".git/hooks/pre-commit"));
    assert!(!marker.exists(), "the marker must start absent");

    let (status, body) = pipeline(&repo, GitOperation::RevertCommit { commit: oid(&noop) }).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        marker.exists(),
        "the pre-commit hook must actually have run — if it did not, the \
         revert never reached a second `git commit` step at all, which is \
         exactly the pre-fix single-`git revert --no-edit` behaviour (it \
         bails out on \"nothing to commit\" before any hook runs)"
    );

    let revert_head_present = std::process::Command::new("git")
        .args(["rev-parse", "-q", "--verify", "REVERT_HEAD"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success();
    assert!(
        !revert_head_present,
        "REVERT_HEAD must be cleared — a dangling sequencer state after a \
         failed step-2 commit means `git revert --abort` was skipped for \
         that failure arm"
    );
    assert_eq!(
        tip(&repo, "HEAD"),
        noop,
        "a failed revert must not move HEAD"
    );
    assert_eq!(
        out(&repo, &["rev-list", "--count", "HEAD"]),
        commit_count,
        "a failed revert must not create any commit, partial or otherwise"
    );
    assert_eq!(
        out(&repo, &["status", "--porcelain"]),
        "",
        "the working tree must be left clean, not mid-revert"
    );
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
///
/// #71 audit item 4a adds the forbidden-word half, mirroring
/// [`delete_untracked_paths_text_never_sounds_recoverable`]'s grep but with
/// its own word list: that test greps for words implying recoverability at
/// ALL (wrong for delete, which has none); this greps for words that would
/// overclaim BEYOND the narrow, qualified claim discard is actually allowed
/// to make (wrong here even though "recoverable" itself is fine) — a future
/// edit that dropped the "only … staged … only until … gc" qualifiers in
/// favour of something unconditional-sounding fails loudly here.
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

    // No-overclaim grep (item 4a): none of these words may appear anywhere
    // in the response or the journal — each one would turn the narrow,
    // qualified claim above into an unconditional one that isn't true.
    let body_lower = body.to_lowercase();
    let summary_lower = entry.summary.to_lowercase();
    for forbidden in [
        "guaranteed",
        "always recoverable",
        "fully recoverable",
        "completely recoverable",
        "automatically recover",
        "safely undo",
    ] {
        assert!(
            !body_lower.contains(forbidden),
            "response text must not overclaim recovery (found {forbidden:?}): {body}"
        );
        assert!(
            !summary_lower.contains(forbidden),
            "journal text must not overclaim recovery (found {forbidden:?}): {}",
            entry.summary
        );
    }
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

/// THE BUG (2026-08-17 audit, #71 close-out): on a non-zero exit from `git
/// checkout HEAD -- <paths>` the activity feed used to get ZERO entry —
/// silence, not an honest record. Reproduced here exactly as found: a
/// multi-path discard where `git checkout` reverts an EARLIER path
/// (`a.txt`, which sorts first) before hitting a real permission error on a
/// LATER one (`sub/b.txt`, whose parent directory is unwritable) and exits
/// non-zero. The fix this pins: the journal must name the TRUE partial
/// state — what actually reverted and what did not — never nothing at all.
#[tokio::test]
async fn discard_tracked_paths_journals_honestly_on_a_partial_multi_path_failure() {
    let (_dir, repo) = seeded_repo();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/b.txt"), "b\n").unwrap();
    run(&repo, &["add", "sub/b.txt"]);
    run(&repo, &["commit", "-q", "-m", "add sub/b.txt"]);

    std::fs::write(repo.join("a.txt"), "edited-a\n").unwrap();
    std::fs::write(repo.join("sub/b.txt"), "edited-b\n").unwrap();

    // Make `sub/` read-only (r-x, no write) so it can still be resolved —
    // the symlink-containment guard's `canonicalize` only needs traversal —
    // but `git checkout` cannot unlink/replace the file inside it. Reverts
    // `a.txt` (sorts first in the index) but hits a real permission error
    // on `sub/b.txt`, not a mocked failure — the same shape as the
    // empirically found bug.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(repo.join("sub"), std::fs::Permissions::from_mode(0o555)).unwrap();

    let result = exec_discard_tracked_paths(
        &repo,
        NetworkNeed::Local,
        &[wpath("a.txt"), wpath("sub/b.txt")],
    )
    .await;

    // Restore permissions immediately, before any assertion can panic and
    // leave a mode-000 directory behind for the tempdir cleanup to choke on.
    std::fs::set_permissions(repo.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let (status, body) = result;
    assert_ne!(status, StatusCode::OK, "{body}");

    // The reproduced bug, stated plainly: `a.txt` really did revert...
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    // ...but `sub/b.txt` did not.
    assert_eq!(
        std::fs::read_to_string(repo.join("sub/b.txt")).unwrap(),
        "edited-b\n"
    );

    // The fix under test: a journal entry must exist, and must name the
    // true partial state rather than staying silent.
    let journaled = crate::journal::read_all(&repo);
    let entry = journaled
        .last()
        .expect("a partial discard failure must still journal what happened");
    assert!(entry.summary.contains("a.txt"), "{}", entry.summary);
    assert!(entry.summary.contains("sub/b.txt"), "{}", entry.summary);
    // The response body must be equally honest, not just the journal.
    assert!(body.contains("a.txt"), "{body}");
    assert!(body.contains("sub/b.txt"), "{body}");
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
    // discarded. #71 audit item 3 additionally names the path, so the
    // wording gained a colon and the filename — the substance this test
    // pins (the count reflects reality, not the raw request) is unchanged.
    assert!(
        body.contains("1 tracked path: a.txt.") && !body.contains('2'),
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
        // M2.16 (#69): the four explicit DiffSpec diff modes. A POST, and
        // emphatically **not** a git write — it spawns a read-only `git diff`
        // through `git_stdout_capped`, constructs no plan, and leaves the
        // repository byte-for-byte unchanged. It has no funnel row below for
        // the same reason `/api/staging/preview` does not.
        //
        // It is a POST only because `DiffSpec` is an internally-tagged enum
        // whose variants carry different fields; a query string could carry it
        // only by flattening it into loose optional parameters, which is the
        // un-explicit shape the type exists to remove. `/api/plan` sits in this
        // table for the same reason — a read wearing a write's verb because the
        // CSRF gate keys on the method.
        ("/api/diff/spec", "spec_diff"),
        ("/api/unstage", "unstage_all"),
        ("/api/undo", "activity::undo"),
        ("/api/merge", "merge_branch"),
        ("/api/push", "push_branch"),
        // M2.20c (#229): fetch — a git write, funnel row below.
        ("/api/fetch", "fetch_remote"),
        // M2.20d (#230): pull — a git write, funnel row below.
        ("/api/pull", "pull_branch"),
        ("/api/delete-branch", "delete_branch"),
        // The stash drawer (M3.24, #77). All three are git writes and all
        // three go through the planner — push moves worktree state into
        // refs/stash, apply moves it back, and drop destroys an entry. Listed
        // here so the census sees three considered rows rather than three
        // routes nothing checked.
        ("/api/stash/push", "handlers::stash::push_stash"),
        ("/api/stash/apply", "handlers::stash::apply_stash"),
        ("/api/stash/drop", "handlers::stash::drop_stash"),
        ("/api/stash/branch", "handlers::stash::branch_from_stash"),
        // M2.21d (#238): the two local tag writes — git writes, funnel rows
        // below. M2.21f (#240) added the two remote ones right after. The
        // tag *listing* is a GET and so never reaches this table.
        ("/api/tag", "handlers::tags::create_tag"),
        ("/api/delete-tag", "handlers::tags::delete_tag"),
        ("/api/push-tag", "handlers::tags::push_tag"),
        (
            "/api/delete-remote-tag",
            "handlers::tags::delete_remote_tag",
        ),
        ("/api/checkout", "checkout_branch"),
        ("/api/force-delete-branch", "force_delete_branch"),
        ("/api/rebase", "rebase"),
        ("/api/reset-test-repo", "reset_test_repo"),
        // #219 (M2.18a): discard/delete of working-tree paths.
        ("/api/discard-tracked-paths", "discard_tracked_paths"),
        ("/api/delete-untracked-paths", "delete_untracked_paths"),
        // M4.31b (#429): resolving one conflicted path by taking a whole
        // side, or removing the file. A git write — it runs `checkout --ours`
        // / `--theirs` / `rm -f` — so it goes through the planner like every
        // other mutation, and appears in the funnel below.
        ("/api/resolve-conflict", "resolve_conflict"),
        ("/api/resolve-conflict-content", "resolve_conflict_content"),
        // M2.20c (#229): cancelling a running operation. A POST, and a write
        // in the "changes what the server is doing" sense — it kills a child
        // process — but **not** a git write: it constructs no argv and mints
        // no plan, so it has no funnel row below. It is classified here, on
        // purpose, rather than being allowed to slip past the tally.
        (
            "/api/operations/{id}/cancel",
            "handlers::operations::cancel_operation",
        ),
        // M2.23d (#248, ADR 0046): build a reviewable Plan and hand it back.
        // Deliberately NOT a funnel row below — it must never reach
        // `plan_and_execute`. The `build_only` block after the funnel loop
        // states the inverse requirement and checks it.
        ("/api/plan", "plan_operation"),
        // M2.23e (#249, ADR 0046 continued): submit a plan for execution. Not
        // a funnel row either — it reaches the planner through
        // `submit_plan_tracked`, the submit path's own tracked entry, never
        // `plan_and_execute` (which would rebuild the operation instead of
        // executing the plan that was actually approved). The
        // `submit_execute` block right after the `build_only` one below
        // checks this route's own chain.
        ("/api/execute-plan", "execute_plan"),
        // M3.25 (#78): executing one past operation's recovery. A git write —
        // it reaches the planner — but not a funnel row below, because it
        // enters through `plan_and_execute_recovery` rather than
        // `plan_and_execute` (it carries the `recovers` link the plain entry
        // point has no parameter for). The `recovery_chain` block after the
        // funnel loop checks its own chain, the same way `/api/execute-plan`
        // gets `submit_execute`.
        (
            "/api/operations/{id}/recover",
            "recovery_center::recover_operation",
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
        // M2.21d (#238): both tag write handlers build their operation and
        // call the planner directly — no `git tag` argv exists in that file.
        ("src/handlers/tags.rs", "create_tag", None),
        ("src/handlers/tags.rs", "delete_tag", None),
        // M2.21f (#240): the two remote tag write handlers, same shape —
        // build the operation, call the planner directly.
        ("src/handlers/tags.rs", "push_tag", None),
        ("src/handlers/tags.rs", "delete_remote_tag", None),
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

    // The recovery chain (M3.25, #78): `recover_operation` is the third way
    // into the funnel, alongside `plan_and_execute` and `execute_plan`.
    //
    // It must reach `plan_and_execute_recovery`, which delegates into the one
    // gated block (pinned by
    // `the_global_entry_point_delegates_through_the_lifecycle_to_the_pipeline`
    // above), so a recovery gets the same read-only gate, idempotency-key
    // requirement, admission, repository guard, staleness gate and durable
    // terminal record as every other write. It must NOT call
    // `plan_and_execute(` directly: that admits a row with no `recovers` link,
    // silently losing the one fact that ties a recovery to what it recovered.
    //
    // And it must still contain the equality gate. That comparison is what the
    // design names as this feature's single highest-risk point — the moment a
    // refactor treats the request body as authoritative ("the UI already
    // validated it"), a stale or hand-crafted `UndoAction` executes against a
    // world that has moved past `Offered`, and the type-level guarantee this
    // whole module exists to provide becomes decorative. The exact spelling is
    // pinned on purpose; if the variables are renamed, update this line
    // deliberately rather than dropping the assertion.
    let recovery_src = crate::argv_boundary::code_only(&source("src/recovery_center.rs"));
    let recover_body = fn_body(&recovery_src, "recover_operation");
    assert!(
        recover_body.contains("plan_and_execute_recovery("),
        "src/recovery_center.rs::recover_operation no longer reaches \
         plan_and_execute_recovery — every git write must flow through the \
         shared planner (ADR 0016)"
    );
    assert!(
        !recover_body.contains("plan_and_execute("),
        "src/recovery_center.rs::recover_operation calls plan_and_execute \
         directly — the recovery entry point exists so the admitted row records \
         which operation it recovers"
    );
    assert!(
        recover_body.contains("classify_recovery("),
        "src/recovery_center.rs::recover_operation must re-derive the \
         classification live, on this request — never trust a class the client \
         cached from an earlier page load"
    );
    assert!(
        recover_body.contains("undo != claimed"),
        "src/recovery_center.rs::recover_operation lost its equality gate — the \
         server's own re-derived UndoAction must equal the client's claim, or \
         the request is refused (409)"
    );

    // The build-only rows (M2.23d, #248): the inverse of the funnel above.
    // `/api/plan` exists to hand a reviewable plan back *unexecuted*, so its
    // handler must reach `build_plan_only` and must NOT reach any execution
    // entry point. Stated as a required-name plus a forbidden-name set so
    // both failure directions are caught: wiring the plan endpoint to the
    // executor (it would execute) and quietly dropping the `build_plan_only`
    // call (it would stop building) each fail here.
    //
    // **Scanned through `argv_boundary::code_only`**, which blanks comments
    // and string contents. Two reasons, one of them found by mutating this
    // very check: `fn_body`'s slice for one function runs up to the *next*
    // `fn` keyword and therefore swallows that next function's doc comment,
    // and these doc comments legitimately discuss `plan_and_execute_in` in
    // prose — so a raw text scan would false-positive.
    //
    // The forbidden names carry **no trailing `(`**, also learned by
    // mutation: `plan_and_execute_in(` does not contain the substring
    // `plan_and_execute(`, so a paren-anchored needle let a handler that
    // called the composed pipeline sail straight through this guard.
    let plan_src = crate::argv_boundary::code_only(&source("src/handlers/plan.rs"));
    for handler in ["plan_operation", "plan_only_in"] {
        let body = fn_body(&plan_src, handler);
        for forbidden in ["plan_and_execute", "submit_plan", "planner::execute"] {
            assert!(
                !body.contains(forbidden),
                "src/handlers/plan.rs::{handler} calls {forbidden} — POST /api/plan \
                 is build-only (#248); executing an approved plan is #249's own \
                 endpoint"
            );
        }
    }
    assert!(
        fn_body(&plan_src, "plan_only_in").contains("build_plan_only("),
        "src/handlers/plan.rs::plan_only_in no longer calls build_plan_only — \
         POST /api/plan must build its plan through the planner's own build \
         stage, not a parallel derivation"
    );
    assert!(
        fn_body(&plan_src, "plan_operation").contains("plan_only_in("),
        "src/handlers/plan.rs::plan_operation no longer goes through \
         plan_only_in — the seam the guard-held build test drives must be the \
         one the route actually uses"
    );
    // The blanking above must not have blanked away what is being looked for:
    // if `code_only` ever stopped preserving code, every `!contains` above
    // would pass vacuously. The two positive `contains` assertions just made
    // are that proof — they read real call sites out of the same blanked
    // string the negatives are checked against.

    // The submit-execute chain (M2.23e, #249): `execute_plan` is the second
    // funnel entry point, alongside `plan_and_execute` — it must reach
    // `submit_plan_tracked`, the submit path's own tracked entry, and that
    // entry must itself reach `plan_and_execute_tracked`, the shared
    // admit/spawn/terminalise layer every write funnels through (ADR 0016),
    // rather than growing a second undischarged copy of that machinery. It
    // must NOT reach `plan_and_execute` or a bare `submit_plan(` directly:
    // the first would rebuild the operation instead of executing the plan
    // that was actually approved, and the second would skip the
    // idempotency/lifecycle layer entirely — exactly the two mistakes the
    // `build_only` block above rules out for `/api/plan`.
    let execute_body = fn_body(&plan_src, "execute_plan");
    for forbidden in ["plan_and_execute(", "submit_plan("] {
        assert!(
            !execute_body.contains(forbidden),
            "src/handlers/plan.rs::execute_plan calls {forbidden} — it must reach \
             submit_plan_tracked, not the composed path or a bare submit_plan"
        );
    }
    assert!(
        execute_body.contains("submit_plan_tracked("),
        "src/handlers/plan.rs::execute_plan no longer calls submit_plan_tracked — \
         POST /api/execute-plan must reach the submit path's own tracked entry"
    );
    let planner_src = crate::argv_boundary::code_only(&source("src/planner.rs"));
    assert!(
        fn_body(&planner_src, "submit_plan_tracked").contains("plan_and_execute_tracked("),
        "planner::submit_plan_tracked no longer calls plan_and_execute_tracked — the \
         submit path must share the composed path's admit/spawn/terminalise layer \
         (ADR 0016), not duplicate it"
    );
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

/// **The recovery pin is composed inside the guard, before execution** — in
/// *both* compositions, and nowhere after them.
///
/// This is the shape half of the guarantee `lifecycle_suite`'s
/// `the_recovery_pin_exists_before_the_tag_it_saves_is_deleted` proves
/// behaviourally, and it is here because the behavioural test can only observe
/// the ordering it happens to race; this one cannot be satisfied by a lucky
/// schedule.
///
/// The ordering is load-bearing for `DeleteLocalTag` specifically.
/// `refs/git-vista/recovery/<id>` is the only thing keeping a deleted annotated
/// tag's now-dangling tag object — and, when no branch reaches it, the commit
/// under it — alive against `git gc`. `git tag -d` removes the last other ref
/// to that object, so a pin written afterwards is a pin written during a window
/// in which the object it names can already have been pruned. Written after
/// `plan_and_execute_in` *returned* — where it lived until this test existed —
/// it was also after `_guard` dropped, so the next queued mutation of the same
/// repository (and any `gc --auto` it fires) was free to run inside that gap.
///
/// The final assertion is the anti-regression one: the tracked wrapper must not
/// write the ref at all. Restoring the old call there while leaving the new one
/// in place would look harmless and would silently re-open nothing — but
/// *moving* it back is exactly the regression, and only "it is not in the
/// wrapper" catches that.
#[test]
fn the_recovery_pin_is_composed_inside_the_guard_before_execution() {
    let src = source("src/planner.rs");
    for composition in ["plan_and_execute_in", "submit_plan"] {
        let body = fn_body(&src, composition);
        let mut from = 0;
        for stage in [
            "coordinator::lock(",
            "enforce_fresh(",
            "pin_recovery(",
            "execute(",
        ] {
            match body[from..].find(stage) {
                Some(at) => from += at + stage.len(),
                None => panic!(
                    "{composition} no longer calls {stage} after the previous stage — the \
                     recovery pin must be written while the mutation guard is held, after \
                     the gates (so a refused plan leaves no ref) and before execute (so a \
                     destructive command cannot outrun the pin that makes it recoverable)"
                ),
            }
        }
    }

    let pin = fn_body(&src, "pin_recovery");
    assert!(
        pin.contains("write_recovery_ref("),
        "pin_recovery must actually write the ref"
    );

    let tracked = fn_body(&src, "plan_and_execute_tracked");
    assert!(
        !tracked.contains("write_recovery_ref("),
        "the recovery ref must NOT be written from the lifecycle wrapper: that runs \
         after plan_and_execute_in returned, which is after the destructive command \
         ran AND after the per-repository guard dropped — the exact gc window the pin \
         exists to close"
    );
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
///
/// M3.25 (#78) added a second public entry point,
/// `plan_and_execute_recovery`, so "the outermost entry point" is now the one
/// block both of them delegate into: `plan_and_execute_maybe_recovery`. The
/// three gate assertions moved there with the code, and this test additionally
/// pins **both** public entries to delegating into it — which is strictly more
/// than it checked before, and is the assertion that would catch the tempting
/// wrong fix (copying the gate block into the recovery entry point, where it
/// can then drift).
#[test]
fn the_global_entry_point_delegates_through_the_lifecycle_to_the_pipeline() {
    let src = source("src/planner.rs");

    for entry in ["plan_and_execute", "plan_and_execute_recovery"] {
        assert!(
            fn_body(&src, entry).contains("plan_and_execute_maybe_recovery("),
            "‘{entry}’ must delegate into the one gated block, never carry its \
             own copy of the write gate"
        );
    }

    let outer = fn_body(&src, "plan_and_execute_maybe_recovery");
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
/// `{FetchRemote, PullBranch, PushBranch}` (M2.20c #229, widened by M2.20d #230
/// and M2.20e #231).
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
        3,
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
    assert!(
        cancellable
            .iter()
            .any(|op| matches!(op, GitOperation::PushBranch { .. })),
        "PushBranch is cancellable during its transfer (#231), got {cancellable:?}"
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

    let push_src = source("src/planner/push.rs");
    assert!(
        push_src.contains("crate::operations::cancel_signal()"),
        "planner::push must take the operation's cancellation latch — \
         honours_cancellation(PushBranch) promises it does (#231)"
    );
    assert!(
        push_src.contains("git_streamed_for("),
        "planner::push must run its git through the streaming, cancellable \
         runner — the collecting `run_git` cannot be interrupted, and a push \
         is the one operation whose effect is on someone else's machine"
    );
}

/// **The force-construction tripwire.** `planner::push::push_argv` is the only
/// place in this server that builds a push command line, and nothing in its
/// production half can produce an unguarded force.
///
/// Two halves, and neither is redundant:
///
///  * `push::tests::no_push_argv_can_carry_a_bare_force` proves the *builder*
///    cannot emit one, over the whole `ForcePublish` × `set_upstream` × name
///    space. What it cannot prove is that some other module builds a push argv
///    of its own — a function's own tests never see its siblings.
///  * This test closes that: `src/planner.rs`, which built `&["push", …]`
///    inline until M2.20e moved it, must no longer name `push` as a git
///    subcommand at all, and `planner/push.rs`'s production half must contain
///    the leased flag and none of the unguarded spellings.
///
/// The source scan stops at `#[cfg(test)]`, on purpose: `push.rs`'s own tests
/// contain the literal `"--force"` precisely because they assert it never
/// appears in an argv, and a scan that could not tell the two apart would have
/// to be weakened until it proved nothing.
#[test]
fn only_planner_push_builds_a_push_argv_and_it_can_only_build_a_leased_force() {
    let planner = source("src/planner.rs");
    assert!(
        !planner.contains("\"push\""),
        "src/planner.rs names `push` as a git subcommand again — every push \
         argv must be built by planner::push::push_argv, which is the one \
         function whose `match` over ForcePublish cannot reach an unguarded \
         force (#231, ADR 0045 D1)"
    );

    let src = source("src/planner/push.rs");
    let split = src
        .find("#[cfg(test)]")
        .expect("planner/push.rs has a test module");
    let production = &src[..split];
    assert!(
        production.contains("--force-with-lease="),
        "the leased flag must be built here, or nothing offers the capability \
         at all"
    );
    for forbidden in [
        "\"--force\"",
        "\"-f\"",
        "\"--force-if-includes\"",
        "--force=",
        "'--force'",
    ] {
        assert!(
            !production.contains(forbidden),
            "planner::push's production half contains {forbidden} — the only \
             force this server may ever build is `--force-with-lease=`, and it \
             is the one thing standing between a user and another party's \
             commits"
        );
    }
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

/// **No `PushBranch` combination is a stub any more** (M2.20e, #231): every one
/// of the four reaches a real executor, and the two that were `501` until this
/// slice are the two that matter.
///
/// The predecessor of this test asserted the opposite — that the widened
/// combinations were refused `NOT_IMPLEMENTED` without touching the remote —
/// and it earned its keep: `PushBranch` had a *live* executor sitting next to
/// the stub, so an arm that ignored the new fields would have run a perfectly
/// ordinary push and reported success for an operation nobody approved. What
/// replaces it has to keep that guarantee while the stub is gone, so it asserts
/// the *positive* half: a `501` for any push combination now means the executor
/// lost an arm.
///
/// Deliberately **not** a behavioural push: these run against a filesystem-path
/// remote, which the sandbox refuses for a push by construction (receive-pack's
/// quarantine migration is a cross-directory rename and the shim withholds
/// `LANDLOCK_ACCESS_FS_REFER`). So the assertion here is exactly what this
/// fixture can honestly support — the plan reached execution and git ran — and
/// the *behaviour* of each combination against a real remote is
/// [`super::push_suite`]'s, over `git daemon`.
#[tokio::test]
async fn every_push_combination_reaches_a_real_executor() {
    for force in [
        ForcePublish::None,
        ForcePublish::WithLease {
            expected_remote_tip: oid(&"0".repeat(40)),
        },
    ] {
        for set_upstream in [true, false] {
            let (dir, repo) = seeded_repo();
            let remote = dir.path().join("remote.git");
            std::fs::create_dir_all(&remote).unwrap();
            run(&remote, &["init", "-q", "--bare", "-b", "main"]);
            run(
                &repo,
                &["remote", "add", "origin", &remote.display().to_string()],
            );

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
            assert_ne!(
                status,
                StatusCode::NOT_IMPLEMENTED,
                "every push combination is wired for execution since #231; a \
                 501 means an arm went missing (set_upstream={set_upstream} \
                 force={force:?}): {body}"
            );
            assert!(
                !body.contains("not yet wired"),
                "…and the stub's wording must be gone with it: {body}"
            );
            // The lease combinations are refused by the pre-flight — the
            // fixture's tracking ref does not exist, let alone hold forty
            // zeroes — and the fast-forward ones die in git's sandboxed
            // receive-pack. Either way nothing may have landed.
            assert_eq!(
                out(&remote, &["for-each-ref", "refs/heads"]),
                "",
                "no push may have landed on this path remote \
                 (set_upstream={set_upstream} force={force:?}): {body}"
            );
            if matches!(force, ForcePublish::WithLease { .. }) {
                assert_eq!(
                    status,
                    StatusCode::CONFLICT,
                    "a lease whose tip does not match the tracking ref must be \
                     refused before git is spawned (set_upstream={set_upstream}): \
                     {body}"
                );
            }
        }
    }
}

// --- #235 (M2.21a) vocabulary; #238 (M2.21d) local execution ----------------

fn tname(s: &str) -> TagName {
    TagName::new(s).unwrap()
}

fn annotation(message: &str, sign: bool) -> git_vista_protocol::TagAnnotation {
    git_vista_protocol::TagAnnotation {
        message: git_vista_protocol::TagMessage::new(message).unwrap(),
        sign,
    }
}

/// `git tag -l --format=…` for one tag, so assertions read git's own answer
/// rather than anything git-vista computed: the ref's unpeeled value, the
/// object type it names, the peeled commit, and the annotation body.
fn tag_facts(repo: &Path, name: &str) -> String {
    out(
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname) %(objecttype) %(*objectname) %(contents)",
            &format!("refs/tags/{name}"),
        ],
    )
}

/// [`GitOperation::CreateTag`] end to end through the real pipeline, for
/// **both kinds** — M2.21d (#238) replaced M2.21a's inertness stub with this.
///
/// The status code is the weakest possible claim here, so nothing rests on
/// it: every assertion below reads the repository back with plain `git`. In
/// particular the *kind* is checked by object type (`git cat-file -t`), not by
/// the tag's mere existence — the failure this catches is an executor that
/// drops `-a` and answers 200 having created a lightweight tag where the
/// reviewer approved an annotated one, which no "did a tag appear?" assertion
/// can see.
///
/// Both kinds are driven for the same reason `pull_branch` drives both
/// strategies: an executor that handled one and quietly mishandled the other
/// would be invisible to a single-shape test. The plan's shape is pinned per
/// kind too — lightweight promises the ref lands exactly at `target`,
/// annotated honestly says `Computed` (the ref will point at a tag object
/// that does not exist yet), and the annotated leg proves the ref really did
/// land somewhere `Computed` was the only honest answer for.
#[tokio::test]
async fn create_tag_executes_through_the_pipeline() {
    for annotated in [false, true] {
        let (_dir, repo) = seeded_repo();
        let target = tip(&repo, "HEAD");
        let ann = annotated.then(|| annotation("v1.0.0 — notes", false));

        // The shape half: risk, CAS-style absence precondition, per-kind
        // after-state, and the delete-created recovery.
        let op = GitOperation::CreateTag {
            name: tname("v1.0.0"),
            target: oid(&target),
            annotation: ann.clone(),
        };
        let (plan, _observed) = build_plan(&repo, op.clone(), tokens()).await;
        assert_eq!(plan.risk, RiskLevel::Reversible, "annotated={annotated}");
        assert!(
            plan.preconditions.contains(&Precondition::RefAbsent {
                ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
            }),
            "creating a tag must be guarded on the tag not already existing"
        );
        let expected_after = match &ann {
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
            "annotated={annotated}"
        );
        assert_eq!(
            plan.recovery,
            RecoveryStrategy::DeleteCreatedTag {
                name: tname("v1.0.0"),
            }
        );

        // The execution half — asserted against git, never against the status.
        assert_eq!(
            out(&repo, &["tag", "-l"]),
            "",
            "the repository must start with no tags, or 'the tag appeared' is vacuous"
        );
        let (status, body) = pipeline(&repo, op).await;
        assert_ok(status, &body);
        assert_eq!(
            out(&repo, &["tag", "-l"]),
            "v1.0.0",
            "annotated={annotated}: git must list the tag it was asked to create"
        );
        // The peeled commit is the reviewed target either way.
        assert_eq!(
            tip(&repo, "refs/tags/v1.0.0^{commit}"),
            target,
            "annotated={annotated}: the tag must speak for the reviewed commit"
        );
        if annotated {
            assert_eq!(
                out(&repo, &["cat-file", "-t", "refs/tags/v1.0.0"]),
                "tag",
                "an annotated tag's ref must name a tag OBJECT — an executor \
                 that dropped -a would answer 200 with a lightweight tag here"
            );
            assert_ne!(
                tip(&repo, "refs/tags/v1.0.0"),
                target,
                "the annotated ref must point at the new tag object, not at \
                 the commit — this is what the plan's RefState::Computed said"
            );
            assert!(
                out(&repo, &["cat-file", "tag", "v1.0.0"]).contains("v1.0.0 — notes"),
                "the reviewed message must be in the tag object git wrote"
            );
        } else {
            assert_eq!(
                out(&repo, &["cat-file", "-t", "refs/tags/v1.0.0"]),
                "commit",
                "a lightweight tag's ref must name the commit directly — an \
                 executor that added -a would write a tag object here"
            );
            assert_eq!(
                tip(&repo, "refs/tags/v1.0.0"),
                target,
                "lightweight: the ref lands exactly where the plan promised"
            );
        }
    }
}

/// The `RefAbsent` precondition, proven by the refusal it causes: creating a
/// tag whose name is taken is refused, and the tag that was already there is
/// **not** moved.
///
/// The second assertion is the one worth having. `git tag` without `-f`
/// refuses a duplicate on its own, so a 400 alone would still pass if the
/// precondition had been dropped entirely; what it would not survive is the
/// existing tag being silently repointed, which is exactly what an executor
/// that "helpfully" added `-f` would do.
#[tokio::test]
async fn create_tag_refuses_a_name_that_already_exists() {
    let (_dir, repo) = seeded_repo();
    let first = tip(&repo, "HEAD");
    run(&repo, &["tag", "-a", "-m", "the original", "v1.0.0"]);
    let original = tag_facts(&repo, "v1.0.0");
    run(&repo, &["commit", "-q", "--allow-empty", "-m", "later"]);
    let second = tip(&repo, "HEAD");
    assert_ne!(first, second);

    let (status, body) = pipeline(
        &repo,
        GitOperation::CreateTag {
            name: tname("v1.0.0"),
            target: oid(&second),
            annotation: Some(annotation("a replacement", false)),
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        tag_facts(&repo, "v1.0.0"),
        original,
        "the existing tag must be untouched — not repointed at the new target"
    );
    assert_eq!(tip(&repo, "refs/tags/v1.0.0^{commit}"), first);
}

/// Asking for a **signed** tag runs a real, sandboxed `git tag -s` (M2.21e,
/// #239) and — as this server's sandbox is built today — that fails, with a
/// typed [`SignTagError`] and nothing created. This used to be a `501`
/// answered *before* any argv was built at all; the whole point of #239 is
/// that a client asking for a signed tag now gets a real attempt and a
/// specific, actionable reason instead, never a raw gpg stderr dump and never
/// a hang past the bound `run_signed_tag`'s own doc comment argues for.
///
/// Inertness is checked with [`repo_fingerprint`] rather than `git tag -l`:
/// `git tag -s` on this host fails *after* writing nothing, but a
/// hypothetical executor that stripped the flag and ran `git tag -a` would
/// leave a perfectly valid unsigned tag behind — and the fingerprint sees the
/// object store too, so even a written-then-deleted tag object would show.
///
/// Wrapped in an outer bound well past [`SIGN_TIMEOUT`]: a regression to an
/// actual hang must fail this test loudly rather than wedging the suite.
#[tokio::test]
async fn create_tag_signing_fails_fast_with_a_typed_reason_and_touches_nothing() {
    let (_dir, repo) = seeded_repo();
    let target = tip(&repo, "HEAD");
    let before = repo_fingerprint(&repo);

    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        pipeline(
            &repo,
            GitOperation::CreateTag {
                name: tname("v1.0.0"),
                target: oid(&target),
                annotation: Some(annotation("signed, please", true)),
            },
        ),
    )
    .await
    .expect(
        "a signing attempt must return within 20s — its own bound is 10s; a hang here \
         is exactly the defect #239 exists to prevent",
    );
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a signing failure is a refusal, not a 5xx or a raw pass-through: {body}"
    );
    let parsed: SignTagError = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "signing failure body must be the typed SignTagError, not raw text: {e}\nbody={body}"
        )
    });
    // NOT pinned to a specific closed-set reason — see
    // `planner::tests::a_signing_attempt_with_no_usable_key_fails_fast_with_a_typed_reason`
    // for why. The specific bucket a keyless attempt lands in is genuinely
    // host-dependent (measured on three separate hosts, including this
    // project's own CI runner, which reaches `Other` here via a real
    // `[GNUPG:]`-line-free git wrapper message rather than a broken
    // classifier). What must hold everywhere is checked below: never
    // TimedOut, never raw gpg/git text in the message.
    assert_ne!(
        parsed.kind,
        SignTagFailureKind::TimedOut,
        "a keyless signing attempt must fail fast, not via the timeout backstop: {}",
        parsed.message
    );
    assert!(
        !parsed.message.contains("gpg:") && !parsed.message.contains("[GNUPG:]"),
        "the client-facing message must never carry raw gpg output: {}",
        parsed.message
    );
    assert_eq!(out(&repo, &["tag", "-l"]), "", "no tag may be created");
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "a failed signing attempt must leave the repository byte-identical"
    );
    // Paired positive: the same request without `sign` really does create a
    // tag here, so "nothing happened" above was capable of failing.
    let (status, body) = pipeline(
        &repo,
        GitOperation::CreateTag {
            name: tname("v1.0.0"),
            target: oid(&target),
            annotation: Some(annotation("signed, please", false)),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(out(&repo, &["tag", "-l"]), "v1.0.0");
}

/// **The no-editor guarantee** (ADR 0048), tested three ways because the
/// failure mode is a request that never returns.
///
/// `git tag -a` with no `-m` writes `.git/TAG_EDITMSG` and then launches
/// `core.editor`. On a headless server that is either an immediate death (on
/// whatever `$EDITOR` happens to be) or a process waiting forever for a human
/// who cannot reach it — and `git tag` has no `--no-edit` to switch it off
/// after the fact.
///
///  1. **The witness.** `.git/TAG_EDITMSG` exists if and only if git took the
///     editor path. Both kinds of create are driven and the file must not
///     appear. This is deterministic and needs no environment at all.
///  2. **The clock.** Every call runs under a timeout, so a genuine hang is a
///     test failure rather than a wedged CI job.
///  3. **The paired positive.** Plain `git tag -a` (no `-m`) is spawned in the
///     *same repository*, with a deliberately blocking editor set **on that
///     child's own environment** — no process-wide `set_var`, so nothing here
///     can race a parallel test. It must fail to finish inside the same
///     timeout and must leave the witness behind. Without this leg, both
///     assertions above would pass in a world where nothing could ever hang.
#[tokio::test]
async fn annotated_tag_creation_never_opens_an_editor() {
    use std::time::Duration;

    let (dir, repo) = seeded_repo();
    let target = tip(&repo, "HEAD");
    let editmsg = repo.join(".git/TAG_EDITMSG");
    assert!(
        !editmsg.exists(),
        "a fresh repository has no TAG_EDITMSG, or the witness proves nothing"
    );

    for (name, ann) in [
        ("lightweight", None),
        ("annotated", Some(annotation("v1.0.0 — notes", false))),
        // The message that spells a flag. It must be consumed as `-m`'s own
        // value — proven below by reading it back out of the tag object —
        // rather than re-entering git's option parser as `--edit`, which
        // would put the editor back in the path this test exists to close.
        ("option-shaped", Some(annotation("--edit", false))),
    ] {
        let op = GitOperation::CreateTag {
            name: tname(name),
            target: oid(&target),
            annotation: ann,
        };
        let (status, body) = tokio::time::timeout(Duration::from_secs(30), pipeline(&repo, op))
            .await
            .unwrap_or_else(|_| {
                panic!("creating the {name} tag did not finish — it is waiting on something")
            });
        assert_ok(status, &body);
        assert!(
            !editmsg.exists(),
            "creating the {name} tag wrote .git/TAG_EDITMSG — git took the \
             editor path, which on this server means a request that never ends"
        );
    }
    assert_eq!(
        out(&repo, &["cat-file", "tag", "option-shaped"])
            .lines()
            .last(),
        Some("--edit"),
        "an option-shaped message must end up as the tag's text, not back in \
         git's option parser"
    );

    // Leg 3: prove the witness fires and the editor path really does hang.
    let marker = dir.path().join("editor-ran");
    let editor = dir.path().join("blocking-editor.sh");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\ntouch {}\nsleep 3600\n",
            marker.to_str().expect("tempdir path is utf-8")
        ),
    )
    .unwrap();
    make_executable(&editor);

    let mut child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["tag", "-a", "v-probe", &target])
        // On this child only: `GIT_EDITOR` beats `core.editor` and every
        // ambient `EDITOR`/`VISUAL`, so the trap is armed no matter what the
        // developer's shell (or CI) has set — and setting it here rather than
        // on the process cannot disturb any concurrently running test.
        .env("GIT_EDITOR", &editor)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn git tag -a");
    let outcome = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let hung = outcome.is_err();
    let _ = child.kill().await;
    assert!(
        hung,
        "plain `git tag -a` with a blocking editor finished anyway — the trap \
         is not armed, so the two assertions above prove nothing"
    );
    assert!(
        marker.exists(),
        "the blocking editor never ran, so this leg did not exercise the \
         editor path it claims to"
    );
    assert!(
        editmsg.exists(),
        "the editor path must leave .git/TAG_EDITMSG — otherwise the witness \
         the assertions above rely on can never fire"
    );
}

/// [`GitOperation::DeleteLocalTag`] end to end — M2.21d (#238) replaced
/// M2.21a's inertness stub with this.
///
/// Driven against a *real annotated* tag, because that is the case where a
/// wrong answer is invisible: the ref is gone either way, so what the test
/// actually pins is the pair of facts the task turns on — the tagged **commit
/// survives** the delete, and the pre-delete unpeeled value is what the plan
/// carried forward for recovery (checked here as the journal's before-oid, so
/// the recovery information is proven to be *recorded*, not merely computed).
#[tokio::test]
async fn delete_local_tag_executes_through_the_pipeline() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1.0.0 — notes", "v1.0.0"]);
    let tag_object = tip(&repo, "refs/tags/v1.0.0");
    let tagged_commit = tip(&repo, "refs/tags/v1.0.0^{}");
    assert_ne!(
        tag_object, tagged_commit,
        "an annotated tag's ref value must differ from its commit, or this \
         test cannot tell a surviving commit from a surviving tag"
    );

    let (status, body) = pipeline(
        &repo,
        GitOperation::DeleteLocalTag {
            name: tname("v1.0.0"),
        },
    )
    .await;
    assert_ok(status, &body);
    assert_eq!(
        out(&repo, &["tag", "-l"]),
        "",
        "git must no longer list the deleted tag"
    );
    assert!(
        git_ok(&repo, &["rev-parse", "--verify", "refs/tags/v1.0.0"])
            .await
            .is_err(),
        "the ref itself must be gone, not merely hidden from the listing"
    );
    // The task's own requirement, and the reason this ranks Destructive
    // rather than Irrecoverable: the delete takes the ref, never the commit.
    assert_eq!(
        out(&repo, &["cat-file", "-t", &tagged_commit]),
        "commit",
        "deleting a tag must never destroy the commit it spoke for"
    );
    assert_eq!(tip(&repo, "HEAD"), tagged_commit);

    // The recovery datum was *recorded*, not just computed: the journal's
    // before-oid is the unpeeled tag object, which is the only value from
    // which the original tag can be restored (ADR 0048).
    let journaled = journal::read_all(&repo)
        .into_iter()
        .find(|e| e.ref_name.as_deref() == Some("refs/tags/v1.0.0"))
        .expect("the delete must be journaled against the tag's own ref");
    assert_eq!(
        journaled.old_oid.as_deref(),
        Some(tag_object.as_str()),
        "the journal must carry the UNPEELED pre-delete value; the peeled \
         commit would restore a lightweight look-alike, not the tag"
    );
    assert_eq!(journaled.new_oid, None, "a deleted ref has no new value");
    assert_eq!(
        journaled.source,
        git_vista_core::activity::ActivitySource::App,
        "the delete must be attributed to the app, not left to reflog inference"
    );
}

/// Deleting a tag that isn't there is refused, and refused for the right
/// reason: nothing else in the repository moves.
///
/// The plan degrades honestly rather than inventing a CAS pin it cannot know
/// — with no observed value there is nothing to compare-and-swap against and
/// no restore point, so `RecoveryStrategy::NotNeeded` is the truthful answer
/// (there is nothing to recover) and git's own refusal is what the user sees.
#[tokio::test]
async fn delete_local_tag_refuses_a_tag_that_does_not_exist() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "kept", "v0.9.0"]);
    let before = repo_fingerprint(&repo);

    let op = GitOperation::DeleteLocalTag {
        name: tname("v1.0.0"),
    };
    let (plan, _observed) = build_plan(&repo, op.clone(), tokens()).await;
    assert_eq!(
        plan.recovery,
        RecoveryStrategy::NotNeeded,
        "no observed value means no restore point to promise"
    );
    assert!(
        plan.expected_ref_changes.is_empty(),
        "a plan must not claim a ref change it cannot describe"
    );

    let (status, body) = pipeline(&repo, op).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "a refused delete must leave the repository byte-identical — the \
         *other* tag in particular"
    );
    assert_eq!(out(&repo, &["tag", "-l"]), "v0.9.0");
}

/// **The recovery decision, proven end to end** (ADR 0048): a deleted tag is
/// restored *byte-identically* from the oid its plan carried, and the durable
/// recovery ref is what keeps that oid — and the commit under it — alive
/// against `git gc`.
///
/// The tag here is the **only** ref reaching its commit. That is deliberate
/// and it is the whole test: with the commit also on a branch, "the target
/// commit survives" would be true no matter what this code did, and the
/// assertion would be vacuous. Made unreachable, the claim has teeth — and
/// the paired negative leg proves it, by running the identical sequence
/// *without* the recovery pin and watching both objects vanish.
///
/// Two distinct things are being pinned:
///
///  * **Unpeeled, not peeled.** Restoring at the tag object's own oid gives
///    back the original tag object — same message, same tagger, same date,
///    same signature. Restoring at the peeled commit would produce a
///    *lightweight* tag: right name, right commit, and every annotation gone
///    forever. The last two assertions check the object *type* and the
///    message, which is the only way to tell those two outcomes apart.
///  * **The pin is load-bearing, not decorative.** `write_recovery_ref` is
///    what makes the dangling tag object reachable; the negative leg deletes
///    the same tag with no pin and shows `git gc` takes both objects.
///
/// The pinned leg drives [`tracked_pipeline`] so that the ref is written by
/// `planner::pin_recovery` — production's own call, in production's own place
/// (inside the mutation guard, before `execute`). It used to be written by hand
/// *before* calling the pipeline, which quietly made this a proof about an
/// ordering the shipped code did not use. That the two orderings differ at all
/// is the subject of `the_recovery_pin_is_composed_inside_the_guard_before_execution`
/// and of `lifecycle_suite`'s
/// `the_recovery_pin_exists_before_the_tag_it_saves_is_deleted`.
#[tokio::test]
async fn a_deleted_tag_is_restorable_byte_identically_and_the_pin_is_what_saves_it() {
    for pinned in [true, false] {
        let (_dir, repo) = seeded_repo();
        // A commit no branch reaches: the tag is its only anchor.
        run(&repo, &["checkout", "-q", "--detach"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "released"]);
        let released = tip(&repo, "HEAD");
        run(
            &repo,
            &["tag", "-a", "-m", "v1.0.0 — release notes", "v1.0.0"],
        );
        run(&repo, &["checkout", "-q", "main"]);
        let tag_object = tip(&repo, "refs/tags/v1.0.0");
        let original = out(&repo, &["cat-file", "tag", "v1.0.0"]);
        assert!(
            original.contains("v1.0.0 — release notes"),
            "the fixture's own annotation must be readable, or nothing below \
             can prove it came back"
        );

        let op = GitOperation::DeleteLocalTag {
            name: tname("v1.0.0"),
        };
        let (plan, _observed) = build_plan(&repo, op.clone(), tokens()).await;
        let RecoveryStrategy::RecreateTag { at, .. } = plan.recovery.clone() else {
            panic!("a delete with an observed value must promise RecreateTag");
        };
        assert_eq!(
            at.as_str(),
            tag_object,
            "the recovery oid is the unpeeled tag object"
        );

        // The pin is *production's*, not this test's: `pin_recovery` writes it
        // inside the mutation guard immediately before `execute`, so under
        // `tracked_pipeline` the ref is already there when `git tag -d` runs.
        // The unpinned leg drives the plain `pipeline`, which has no operation
        // id to name a ref after and so writes none — which is exactly the
        // world this test's negative half needs.
        let (status, body) = if pinned {
            tracked_pipeline(&repo, op, "tag-recovery-pin").await
        } else {
            pipeline(&repo, op).await
        };
        assert_ok(status, &body);
        assert!(
            git_ok(&repo, &["rev-parse", "--verify", "refs/tags/v1.0.0"])
                .await
                .is_err()
        );

        // Everything that could have kept the objects alive by accident, gone.
        run(&repo, &["reflog", "expire", "--expire=now", "--all"]);
        run(&repo, &["gc", "-q", "--prune=now"]);

        let tag_alive = git_ok(&repo, &["cat-file", "-e", &tag_object])
            .await
            .is_ok();
        let commit_alive = git_ok(&repo, &["cat-file", "-e", &released]).await.is_ok();
        if !pinned {
            // The negative leg: without the pin there is nothing to restore.
            assert!(
                !tag_alive && !commit_alive,
                "unpinned, git gc must take both objects — otherwise the \
                 pinned leg's survival proves nothing about the pin"
            );
            continue;
        }
        assert!(
            tag_alive,
            "the recovery ref must keep the dangling tag object reachable"
        );
        assert!(
            commit_alive,
            "and with it the commit the tag spoke for — this is what makes \
             the delete recoverable rather than a quiet history loss"
        );

        // The restoration itself, exactly what `RecreateTag` prescribes.
        run(&repo, &["update-ref", "refs/tags/v1.0.0", at.as_str()]);
        assert_eq!(
            out(&repo, &["cat-file", "-t", "refs/tags/v1.0.0"]),
            "tag",
            "restoring at the unpeeled oid gives back an ANNOTATED tag; the \
             peeled commit would have given a lightweight look-alike"
        );
        assert_eq!(
            out(&repo, &["cat-file", "tag", "v1.0.0"]),
            original,
            "the restored tag object must be byte-identical — message, \
             tagger and date included"
        );
        assert_eq!(tip(&repo, "refs/tags/v1.0.0^{}"), released);
    }
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

/// M2.21f (#240): `GitOperation::DeleteRemoteTag` now executes for real —
/// `git push <remote> --delete refs/tags/<name>` against a real, reachable
/// remote over the daemon fixture `push_branch_executes_through_the_pipeline`
/// above uses, for the same reason: a filesystem-path remote is dead under
/// the Network sandbox tier (receive-pack's quarantine migration is a
/// cross-directory rename the shim denies), so this test cannot use the
/// filesystem-path bare remote M2.21a's stub version of this test used.
#[tokio::test]
async fn delete_remote_tag_executes_through_the_pipeline() {
    let (dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1", "v1.0.0"]);
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);

    let _port_claim = crate::test_ports::PortClaim::acquire();
    let port = crate::test_ports::PortClaim::PORT;
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
    run(&repo, &["push", "-q", "origin", "main", "refs/tags/v1.0.0"]);
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
    assert_ok(status, &body);
    assert_eq!(
        out(&remote, &["for-each-ref", "refs/tags"]),
        "",
        "the remote's tag must really be gone after a real delete"
    );
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "deleting a REMOTE tag must not touch anything local — there is \
         no remote-tracking ref for a tag to move (D5, see \
         DeleteRemoteTag's doc in plan.rs)"
    );
}

/// M2.21f (#240): `GitOperation::PushTag` now executes for real —
/// `git push <remote> refs/tags/<name>` against the same real daemon
/// fixture the delete test above uses, over the shared `PortClaim` (the
/// three daemon-needing tests in this binary run it sequentially, never
/// concurrently — see `test_ports`'s own doc).
#[tokio::test]
async fn push_tag_executes_through_the_pipeline() {
    let (dir, repo) = seeded_repo();
    run(&repo, &["tag", "-a", "-m", "v1", "v1.0.0"]);
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);

    let _port_claim = crate::test_ports::PortClaim::acquire();
    let port = crate::test_ports::PortClaim::PORT;
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
    assert_ok(status, &body);
    let remote_tags = out(&remote, &["for-each-ref", "refs/tags"]);
    assert!(
        remote_tags.contains("v1.0.0"),
        "the tag must really reach the remote: {remote_tags}"
    );
    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "pushing a tag must not touch anything local — there is no \
         remote-tracking ref for a tag to move (D5, see PushTag's doc in \
         plan.rs)"
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

/// #248's build-only proof, at the seam `POST /api/plan` actually uses and
/// for **every** operation kind — the census version of the single-operation
/// test above.
///
/// [`crate::handlers::plan::plan_only_in`] is the exact function
/// `plan_operation` (the route handler) calls once it has resolved the
/// repository; `every_git_write_route_reaches_the_planner` pins that, so this
/// is not a lookalike seam. Driving it here, rather than the route, is
/// deliberate: the route reads the process-global `CURRENT` selection, which
/// is set-once per process and owned by `state`'s own test in this binary.
///
/// Two claims, both across the whole [`samples`] census:
///
///  - **The plan endpoint takes no guard.** The pipeline's real mutation
///    guard is held for the entire call; if building ever acquired it the
///    call would block and the timeout would fail the test. This is what
///    makes a client-review roundtrip safe — an agent can ask "what would
///    this do?" for any operation while an unrelated mutation is running,
///    and neither blocks the other.
///  - **It executes nothing.** The full [`repo_fingerprint`] (refs, HEAD,
///    status, object count, config, FETCH_HEAD) is unchanged after all 25
///    calls, against a repository whose fingerprint is itself proven
///    change-detecting by
///    [`repo_fingerprint_detects_every_change_it_claims_to_watch`].
///
/// The anti-vacuity leg is the surrounding test above: it proves this exact
/// guard is the one `submit_plan` queues on, so "held" here is not a lock
/// nothing cares about.
#[tokio::test]
async fn every_plan_tool_operation_builds_while_the_mutation_guard_is_held() {
    let (_dir, repo) = seeded_repo();
    let before = repo_fingerprint(&repo);

    let held = crate::coordinator::lock(None).await;
    for op in samples() {
        let label = serde_json::to_value(&op).unwrap()["op"]
            .as_str()
            .unwrap()
            .to_string();
        let plan = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            crate::handlers::plan::plan_only_in(&repo, tokens(), op.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "POST /api/plan's seam blocked on the mutation guard for ‘{label}’ — \
                 building must never lock"
            )
        });
        // The plan describes the operation asked for, not some other one:
        // a seam that silently substituted an operation would still return
        // a Plan and still leave the repository untouched.
        assert_eq!(plan.operation, op, "‘{label}’ built a plan for another op");
    }
    drop(held);

    assert_eq!(
        repo_fingerprint(&repo),
        before,
        "POST /api/plan's seam mutated the repository — it must build only"
    );
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
/// `unmet_at_build`'s "not configured" instead (ADR 0047 — before it, the
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
/// What both cases fail closed *with* changed in ADR 0047. This test used to
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

#[tokio::test]
async fn resolve_conflict_executes_through_the_pipeline() {
    // M4.31 (#84). A REAL merge conflict, resolved through the full production
    // path — plan build, mutation guard, staleness gate, executor — not a
    // direct call to the exec function. That is the whole point of this suite:
    // a variant that works when called directly and breaks somewhere in the
    // funnel is exactly what the census exists to catch.
    let (_dir, repo) = seeded_repo();

    run(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("a.txt"), "theirs\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "theirs"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "ours\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "ours"]);
    // Expected to fail — that is what produces the conflict under test.
    let _ = std::process::Command::new("git")
        .args(["merge", "theirs"])
        .current_dir(&repo)
        .status();
    assert!(
        out(&repo, &["ls-files", "-u", "--", "a.txt"]).contains("a.txt"),
        "the fixture must actually be conflicted before the pipeline runs"
    );

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
        },
    )
    .await;
    assert_ok(status, &body);

    // Three separate facts, because any one of them alone could hold while the
    // resolution was still wrong.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "ours\n",
        "the working tree must hold our side, with no conflict markers"
    );
    assert_eq!(
        out(&repo, &["ls-files", "-u", "--", "a.txt"]),
        "",
        "the stage entries must be cleared — a checkout alone leaves them"
    );
    // Stage 0 is git's "normal, resolved" slot. Deliberately NOT
    // `git diff --cached`: taking OUR side produces content identical to HEAD,
    // so a cached diff is legitimately empty and asserting on it would fail on
    // a correct resolution. The index stage is the fact that actually means
    // resolved.
    let staged = out(&repo, &["ls-files", "-s", "--", "a.txt"]);
    assert!(
        staged.starts_with("100644") && staged.contains(" 0\t"),
        "the resolved file must sit at stage 0, got: {staged}"
    );
}

/// A modify/modify conflict with real base/ours/theirs stages, plus the
/// [`ConflictSource`](git_vista_protocol::ConflictSource) a content resolution
/// needs to build a valid request against — the OID triple and the
/// `conflict-v1:` token, both read the way a real client would: from the live
/// repository, not invented.
async fn content_conflict_fixture() -> (
    tempfile::TempDir,
    PathBuf,
    [Option<CommitOid>; 3],
    GenerationToken,
) {
    let (dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("a.txt"), "theirs\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "theirs"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "ours\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "ours"]);
    let _ = std::process::Command::new("git")
        .args(["merge", "theirs"])
        .current_dir(&repo)
        .status();
    assert!(
        out(&repo, &["ls-files", "-u", "--", "a.txt"]).contains("a.txt"),
        "the fixture must actually be conflicted"
    );

    let stages = [
        oid(&out(&repo, &["rev-parse", ":1:a.txt"])),
        oid(&out(&repo, &["rev-parse", ":2:a.txt"])),
        oid(&out(&repo, &["rev-parse", ":3:a.txt"])),
    ]
    .map(Some);

    let marker = std::fs::read(repo.join("a.txt")).unwrap();
    let source = crate::conflicts::conflict_source_token(&repo, "a.txt", &marker)
        .await
        .unwrap();

    (dir, repo, stages, source)
}

#[tokio::test]
async fn resolve_conflict_content_executes_through_the_pipeline() {
    // M4.31c (#432), ADR 0069. Same discipline as
    // `resolve_conflict_executes_through_the_pipeline`: the full production
    // path, not a direct call to the exec function.
    let (_dir, repo, expected_stages, expected_source) = content_conflict_fixture().await;

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflictContent {
            path: WorktreePath::new("a.txt").unwrap(),
            expected_stages,
            expected_source,
            content: "resolved by hand\n".to_string(),
        },
    )
    .await;
    assert_ok(status, &body);

    // Three separate facts, same reasoning as the whole-side test: any one
    // alone could hold while the resolution was still wrong.
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "resolved by hand\n",
        "the working tree must hold exactly the submitted content, no markers"
    );
    assert_eq!(
        out(&repo, &["ls-files", "-u", "--", "a.txt"]),
        "",
        "the stage entries must be cleared"
    );
    let staged = out(&repo, &["ls-files", "-s", "--", "a.txt"]);
    assert!(
        staged.starts_with("100644") && staged.contains(" 0\t"),
        "the resolved file must sit at stage 0, got: {staged}"
    );
}

#[tokio::test]
async fn resolve_conflict_content_refuses_when_a_stage_moved_since_it_was_built() {
    // ADR 0069's gate 3. MUTATION: drop the `live_stages != expected_stages`
    // comparison. A resolution composed against one picture would then apply
    // silently over a repository that has since moved — "the picture you
    // decided against has changed" is the invariant, not just the surviving
    // bytes.
    let (_dir, repo, expected_stages, expected_source) = content_conflict_fixture().await;

    // Rewrite ONLY the stage-2 (ours) index entry to a different blob, via the
    // plumbing command that changes a single stage without touching the
    // marker file or any other stage — isolating gate 3 from gate 4. A whole-
    // side resolve (`git checkout --ours`, then a fresh `add`) does exactly
    // this in production; a fabricated blob proves the same shape without
    // needing a resolution to already have happened.
    let scratch = repo.join(".moved-blob-scratch");
    std::fs::write(&scratch, "ours, moved\n").unwrap();
    let new_blob = out(&repo, &["hash-object", "-w", scratch.to_str().unwrap()]);
    std::fs::remove_file(&scratch).unwrap();
    // `--index-info` (not `--cacheinfo`, which cannot target a specific
    // stage): "<mode> SP <sha1> SP <stage> TAB <path>" replaces exactly the
    // stage-2 entry, leaving stage 1, stage 3, and the worktree file untouched.
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["update-index", "--index-info"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("100644 {new_blob} 2\ta.txt\n").as_bytes())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "update-index --index-info failed"
    );
    assert!(
        out(&repo, &["ls-files", "-u", "--", "a.txt"]).contains("a.txt"),
        "the path must still be conflicted after moving one stage"
    );
    let marker_unchanged = std::fs::read_to_string(repo.join("a.txt")).unwrap();

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflictContent {
            path: WorktreePath::new("a.txt").unwrap(),
            expected_stages, // stale — names the old stage-2 blob
            expected_source,
            content: "resolved by hand\n".to_string(),
        },
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(
        body.contains("changed since you opened it"),
        "must be gate 3's sentence, not gate 4's: {body}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        marker_unchanged,
        "a refused resolution must never touch the working tree"
    );
}

#[tokio::test]
async fn resolve_conflict_content_refuses_when_the_served_document_moved() {
    // ADR 0069's gate 4 — the one no repository-level generation alone can
    // catch. The stage OIDs here are UNCHANGED; only the marker file's own
    // bytes moved, exactly the gap `conflict-v1:` exists to close.
    //
    // MUTATION: skip re-minting the token and only check `expected_stages`.
    // An edit landing between serve and submit — in the app or outside it —
    // would then be silently overwritten rather than refused.
    let (_dir, repo, expected_stages, stale_source) = content_conflict_fixture().await;

    // An edit to the marker file itself, stage entries untouched. Simulates
    // another tool (or another tab) writing the same file mid-resolution.
    std::fs::write(
        repo.join("a.txt"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> theirs\nan edit landed here\n",
    )
    .unwrap();

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflictContent {
            path: WorktreePath::new("a.txt").unwrap(),
            expected_stages,
            expected_source: stale_source,
            content: "resolved by hand\n".to_string(),
        },
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(body.contains("edited elsewhere"), "{body}");
    assert!(
        std::fs::read_to_string(repo.join("a.txt"))
            .unwrap()
            .contains("an edit landed here"),
        "a refused resolution must never overwrite the edit it just detected"
    );
}

#[tokio::test]
async fn resolve_conflict_content_refuses_a_binary_conflict() {
    // ADR 0069's gate 2, and #430's ResolutionSurface asks the identical
    // question client-side — this is the SAME rule
    // (`ConflictedFile::text_resolvable`), checked here at the layer that
    // actually writes.
    let (dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("bin.dat"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
    run(&repo, &["add", "bin.dat"]);
    run(&repo, &["commit", "-q", "-m", "theirs adds a binary"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("bin.dat"), [0x89u8, b'P', b'N', b'G', 0, 2]).unwrap();
    run(&repo, &["add", "bin.dat"]);
    run(
        &repo,
        &["commit", "-q", "-m", "ours adds a different binary"],
    );
    let _ = std::process::Command::new("git")
        .args(["merge", "theirs"])
        .current_dir(&repo)
        .status();
    assert!(
        out(&repo, &["ls-files", "-u", "--", "bin.dat"]).contains("bin.dat"),
        "the fixture must be conflicted"
    );

    let stages = [
        None,
        Some(oid(&out(&repo, &["rev-parse", ":2:bin.dat"]))),
        Some(oid(&out(&repo, &["rev-parse", ":3:bin.dat"]))),
    ];
    let source = GenerationToken::new("conflict-v1:irrelevant").unwrap();

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflictContent {
            path: WorktreePath::new("bin.dat").unwrap(),
            expected_stages: stages,
            expected_source: source,
            content: "cannot merge bytes as text".to_string(),
        },
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(body.contains("binary"), "{body}");
    drop(dir);
}

#[tokio::test]
async fn resolving_a_path_that_is_not_conflicted_is_refused_by_the_executor() {
    // `shape` records no Precondition for this operation, so the executor's
    // own re-read is the ONLY guard. MUTATION: drop that re-read and let the
    // checkout run — `git checkout --ours` on an unconflicted path fails with
    // a bare git error, and a caller would get an unexplained failure instead
    // of a refusal that says what happened.
    let (_dir, repo) = seeded_repo();
    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeTheirs,
        },
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(
        body.contains("not conflicted"),
        "the refusal must say why, got: {body}"
    );
}

#[tokio::test]
async fn taking_a_side_that_was_deleted_is_refused_rather_than_deleting_the_file() {
    // The gap a surviving mutation exposed: `ConflictedFile::refuses` was
    // tested at the protocol layer, but nothing checked the EXECUTOR acts on
    // it. Deleting the `refuses` call left every test green.
    //
    // MUTATION: drop the `refuses` check in `exec_resolve_conflict`. This test
    // goes red, because `git checkout --ours` on a path we deleted resolves to
    // *nothing* — the user asked to keep our side and would get the file
    // removed, having been told it succeeded.
    let (_dir, repo) = seeded_repo();

    run(&repo, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.join("a.txt"), "theirs changed it\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "theirs modifies"]);
    run(&repo, &["checkout", "-q", "main"]);
    run(&repo, &["rm", "-q", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "ours deletes"]);
    let _ = std::process::Command::new("git")
        .args(["merge", "theirs"])
        .current_dir(&repo)
        .status();

    // Sanity: this really is a delete/modify conflict with no "ours" stage.
    let unmerged = out(&repo, &["ls-files", "-u", "--", "a.txt"]);
    assert!(
        !unmerged.is_empty() && !unmerged.contains(" 2\t"),
        "fixture must be conflicted with no stage 2 (ours), got: {unmerged}"
    );

    let (status, body) = pipeline(
        &repo,
        GitOperation::ResolveConflict {
            path: git_vista_protocol::WorktreePath::new("a.txt").unwrap(),
            resolution: git_vista_protocol::conflict::Resolution::TakeOurs,
        },
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(
        body.contains("no ours side"),
        "the refusal must name the missing side and point at an explicit \
         deletion instead, got: {body}"
    );
    // And nothing may have been written on the way to refusing.
    assert!(
        !out(&repo, &["ls-files", "-u", "--", "a.txt"]).is_empty(),
        "a refused resolution must leave the conflict exactly as it was"
    );
}

#[tokio::test]
async fn pop_stash_removes_the_entry_on_a_clean_pop() {
    // The ordinary path, and the half `apply` cannot do: the entry is gone.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "wip"]);
    let oid = out(&repo, &["rev-parse", "stash@{0}"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::PopStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(oid).unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);

    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "a changed\n",
        "the stashed change must be back in the worktree"
    );
    assert_eq!(
        out(&repo, &["stash", "list"]),
        "",
        "a clean pop removes the entry — that is the whole difference from apply"
    );
}

#[tokio::test]
async fn pop_stash_refuses_to_report_complete_while_conflicted() {
    // THE acceptance criterion: "pop is not reported complete while conflicts
    // remain". Git already leaves the entry in place on a conflicting pop —
    // what this pins is that the RESPONSE says so, by name, rather than
    // returning a success whose only clue is a line of git stderr.
    //
    // MUTATION: report OK whenever `git stash pop` exits non-zero but the
    // scan is unavailable, or skip the conflict re-read entirely. Either way
    // a user is told their stash was popped while their worktree is full of
    // conflict markers and the entry is still in the drawer.
    let (_dir, repo) = seeded_repo();

    // Stash a change to a.txt, then commit a DIFFERENT change to the same
    // file so the stash cannot apply cleanly.
    std::fs::write(repo.join("a.txt"), "from the stash\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "wip"]);
    let oid = out(&repo, &["rev-parse", "stash@{0}"]);
    std::fs::write(repo.join("a.txt"), "from a commit\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "diverge"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::PopStash {
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(oid).unwrap(),
        },
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "a conflicted pop must not return OK — body: {body}"
    );
    assert!(
        body.contains("NOT complete"),
        "the response must say plainly that it did not finish: {body}"
    );
    assert!(
        body.contains("a.txt"),
        "the conflicted path must be named, not left for the user to hunt: {body}"
    );
    assert!(
        body.contains("not removed"),
        "the user must be told their stash survived: {body}"
    );

    // And the promise must be true, not just printed.
    assert!(
        out(&repo, &["stash", "list"]).contains("wip"),
        "git leaves the entry on a conflicting pop; the message says so, so it must hold"
    );
}

#[tokio::test]
async fn a_stash_write_moves_the_generation_so_an_older_plan_goes_stale() {
    // M3.24 (#77) criterion 5: "activity and generation updates are correct".
    //
    // The generation token is what `enforce_fresh` compares to refuse a plan
    // approved against a repository that has since moved. If a stash write did
    // NOT move it, a plan built before a drop would still look fresh
    // afterwards — and every stash selector in it would point at a different
    // entry, because dropping renumbers the list. That is the exact failure
    // `stash_entry_still_at` exists to catch at execution time; this asserts
    // the staleness gate catches it one stage earlier.
    //
    // Asserted rather than assumed: the ref read behind the generation digest
    // is gix's `all()`, and the test that pins it is named for "head, branches
    // and tags". Whether refs/stash rides along was worth checking, not
    // inferring.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "first"]);
    std::fs::write(repo.join("a.txt"), "a changed again\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "second"]);

    let before = build_plan_only(&repo, GitOperation::StageAll, tokens())
        .await
        .generation;

    // Drop the top entry. refs/stash now points somewhere else and every
    // selector below it has renumbered.
    run(&repo, &["stash", "drop", "-q", "stash@{0}"]);

    let after = build_plan_only(&repo, GitOperation::StageAll, tokens())
        .await
        .generation;

    assert_ne!(
        before, after,
        "dropping a stash must move the generation — otherwise a plan approved \
         before the drop still passes the staleness gate, while every selector \
         in it now addresses a different entry"
    );
}

#[tokio::test]
async fn a_stash_push_is_journaled_as_activity() {
    // The other half of criterion 5. A stash write that never reaches the
    // journal is invisible to the activity feed and to the recovery centre —
    // the user's own record of what happened to their work.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();

    let (status, body) = pipeline(
        &repo,
        GitOperation::PushStash {
            message: Some(git_vista_protocol::StashMessage::new("wip").unwrap()),
            keep_index: false,
            include_untracked: false,
        },
    )
    .await;
    assert_ok(status, &body);

    let journal =
        std::fs::read_to_string(repo.join(".git/git-vista/journal.jsonl")).unwrap_or_default();
    assert!(
        journal.contains("refs/stash"),
        "the stash write must be journaled against refs/stash; journal was: {journal}"
    );
}

#[tokio::test]
async fn branch_from_stash_lands_a_stash_that_would_not_pop() {
    // The point of this operation, demonstrated rather than asserted in prose:
    // the SAME stash that conflicts on pop goes in cleanly here, because git
    // creates the branch at the stash's original base and applies it there.
    //
    // The fixture is deliberately the one from the pop conflict test — stash a
    // change to a.txt, then commit a different change to a.txt — so the two
    // tests are the same scenario with different verbs, and the difference in
    // outcome is the whole justification for the variant existing.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "from the stash\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "wip"]);
    let oid = out(&repo, &["rev-parse", "stash@{0}"]);
    std::fs::write(repo.join("a.txt"), "from a commit\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "diverge"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::BranchFromStash {
            name: git_vista_protocol::BranchName::new("rescued").unwrap(),
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(oid).unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);

    // All three effects, checked separately — any one could hold while another
    // silently did not happen.
    assert_eq!(
        out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "rescued",
        "the new branch must be checked out"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "from the stash\n",
        "the stashed content must be present, with no conflict markers — this is \
         the same stash that conflicts on pop"
    );
    assert_eq!(
        out(&repo, &["stash", "list"]),
        "",
        "a successful branch-from-stash consumes the entry"
    );
}

#[tokio::test]
async fn branch_from_stash_refuses_a_name_that_already_exists() {
    // MUTATION: drop the RefAbsent precondition. Git would refuse anyway, but
    // AFTER approval — the caller would have committed to a plan that could
    // never run. Refusing at build time lets them pick another name having
    // consumed nothing.
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
    run(&repo, &["stash", "push", "-q", "-m", "wip"]);
    let oid = out(&repo, &["rev-parse", "stash@{0}"]);
    run(&repo, &["branch", "taken"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::BranchFromStash {
            name: git_vista_protocol::BranchName::new("taken").unwrap(),
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(oid).unwrap(),
        },
    )
    .await;

    assert_ne!(
        status,
        axum::http::StatusCode::OK,
        "an existing branch name must be refused, not reinterpreted: {body}"
    );
    assert!(
        out(&repo, &["stash", "list"]).contains("wip"),
        "a refused request must not have consumed the stash"
    );

    // The assertions above hold even WITHOUT the precondition, because git
    // refuses a taken branch name itself — at execution. That is precisely the
    // difference this operation's precondition exists to make, so it is
    // asserted directly: the refusal must be visible in the PLAN, before a
    // user approves anything.
    //
    // Found by mutation: removing the RefAbsent precondition left the checks
    // above green, which meant they were testing git's behaviour rather than
    // ours.
    let plan = build_plan_only(
        &repo,
        GitOperation::BranchFromStash {
            name: git_vista_protocol::BranchName::new("taken").unwrap(),
            entry: git_vista_protocol::StashSelector::new("stash@{0}").unwrap(),
            expected_oid: git_vista_protocol::CommitOid::new(out(
                &repo,
                &["rev-parse", "stash@{0}"],
            ))
            .unwrap(),
        },
        tokens(),
    )
    .await;
    assert!(
        plan.preconditions.iter().any(|p| matches!(
            p,
            git_vista_protocol::Precondition::RefAbsent { ref_name }
                if ref_name.as_str() == "refs/heads/taken"
        )),
        "the plan must carry the RefAbsent precondition, so the name clash is \
         visible before approval rather than only on execution: {:?}",
        plan.preconditions
    );
}

/// A repository whose HEAD is a real merge commit with two parents.
fn merged_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let (dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("side.txt"), "side\n").unwrap();
    run(&repo, &["add", "side.txt"]);
    run(&repo, &["commit", "-q", "-m", "side work"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("main.txt"), "main\n").unwrap();
    run(&repo, &["add", "main.txt"]);
    run(&repo, &["commit", "-q", "-m", "main work"]);
    run(&repo, &["merge", "--no-ff", "-m", "merge side", "side"]);
    (dir, repo)
}

#[tokio::test]
async fn reverting_a_merge_needs_a_mainline_and_says_why() {
    // M4.28 (#81). This is the defect that existed before RevertMerge: git
    // refuses `git revert <merge>` outright, and RevertCommit had nowhere to
    // carry the answer, so the attempt surfaced as a raw git error naming a
    // FLAG rather than the decision behind it.
    //
    // MUTATION: drop the parent-count check and let git refuse. The revert
    // still fails, but the user is told "no -m option was given" — which is
    // accurate and nearly useless to anyone who has not reverted a merge
    // before.
    let (_dir, repo) = merged_repo();
    let head = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::RevertCommit {
            commit: git_vista_protocol::CommitOid::new(head).unwrap(),
        },
    )
    .await;

    assert_ne!(status, axum::http::StatusCode::OK, "body: {body}");
    assert!(
        body.contains("which side of the merge"),
        "the refusal must name the DECISION, not a flag: {body}"
    );
    assert!(
        body.contains("parent 1"),
        "and it must say what the usual answer is: {body}"
    );
}

#[tokio::test]
async fn reverting_a_merge_with_a_mainline_succeeds() {
    // The capability that did not exist at all before this change.
    let (_dir, repo) = merged_repo();
    let head = out(&repo, &["rev-parse", "HEAD"]);
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::RevertMerge {
            commit: git_vista_protocol::CommitOid::new(head).unwrap(),
            mainline: std::num::NonZeroU8::new(1).unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);

    assert_ne!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "a revert adds a commit, so HEAD must have moved"
    );
    // Reverting the merge with mainline 1 undoes what the OTHER side brought.
    assert!(
        !repo.join("side.txt").exists(),
        "the side branch's file must be gone — that is what reverting the merge means"
    );
    assert!(
        repo.join("main.txt").exists(),
        "the mainline's own work must survive"
    );
}

#[tokio::test]
async fn a_mainline_on_an_ordinary_commit_is_refused() {
    // The other half of making the invalid state unrepresentable: the type
    // stops "merge without a choice", and this stops "choice without a merge".
    //
    // MUTATION: drop this arm and pass -m through. Git errors with "mainline
    // was specified but commit is not a merge", which is again a fact about
    // flags rather than about what the user asked for.
    let (_dir, repo) = seeded_repo();
    let head = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::RevertMerge {
            commit: git_vista_protocol::CommitOid::new(head).unwrap(),
            mainline: std::num::NonZeroU8::new(1).unwrap(),
        },
    )
    .await;

    assert_ne!(status, axum::http::StatusCode::OK, "body: {body}");
    assert!(
        body.contains("not a merge commit"),
        "the refusal must say the commit is not a merge: {body}"
    );
}

#[tokio::test]
async fn a_parent_that_does_not_exist_is_refused() {
    // A merge has two parents; asking for the third is a request that cannot
    // be satisfied, and saying so beats letting git say "commit ... does not
    // have parent 3".
    let (_dir, repo) = merged_repo();
    let head = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::RevertMerge {
            commit: git_vista_protocol::CommitOid::new(head).unwrap(),
            mainline: std::num::NonZeroU8::new(3).unwrap(),
        },
    )
    .await;

    assert_ne!(status, axum::http::StatusCode::OK, "body: {body}");
    assert!(
        body.contains("does not exist"),
        "the refusal must say the parent does not exist: {body}"
    );
}

#[tokio::test]
async fn a_cherry_pick_lands_a_commit_from_another_branch() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("picked.txt"), "from side\n").unwrap();
    run(&repo, &["add", "picked.txt"]);
    run(&repo, &["commit", "-q", "-m", "side work"]);
    let wanted = out(&repo, &["rev-parse", "HEAD"]);
    run(&repo, &["checkout", "-q", "main"]);

    // Main must have moved on, or the cherry-pick reproduces a BYTE-IDENTICAL
    // commit — same tree, same parent, same author, same message, same
    // timestamp, therefore the same oid. Correct git behaviour, and it makes
    // "a new commit was created" unobservable. Discovered by asserting the
    // opposite and being wrong.
    std::fs::write(repo.join("unrelated.txt"), "main moved on\n").unwrap();
    run(&repo, &["add", "unrelated.txt"]);
    run(&repo, &["commit", "-q", "-m", "main moves on"]);

    assert!(!repo.join("picked.txt").exists(), "fixture: not here yet");
    let tip_before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::CherryPick {
            commit: git_vista_protocol::CommitOid::new(wanted.clone()).unwrap(),
        },
    )
    .await;
    assert_ok(status, &body);

    assert!(
        repo.join("picked.txt").exists(),
        "the picked commit's file must be present on main"
    );

    // `assert_ne!(HEAD, wanted)` alone was INERT: main had already moved on in
    // the fixture above, so HEAD differed from `wanted` before the pipeline ran
    // and a cherry-pick that did nothing at all would still have passed. What
    // has to be pinned is that a NEW COMMIT WAS CREATED, on top of where main
    // actually was. (M4 test-integrity audit, 2026-08-22.)
    //
    // MUTATION: make exec_cherry_pick apply the change without committing (drop
    // to `--no-commit`). `picked.txt` still appears and HEAD still differs from
    // `wanted`; the three assertions below are what notice.
    let after = out(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(
        after, wanted,
        "a cherry-pick creates a NEW commit; it does not move the branch to the old one"
    );
    assert_ne!(
        after, tip_before,
        "a commit must actually have been created — HEAD must move"
    );
    assert_eq!(
        out(&repo, &["rev-parse", "HEAD^"]),
        tip_before,
        "and it must sit on top of where main was, not replace it"
    );
    assert_eq!(
        out(&repo, &["status", "--porcelain"]),
        "",
        "the pick must be committed, not left staged in the worktree"
    );
}

#[tokio::test]
async fn a_conflicting_cherry_pick_pauses_instead_of_aborting() {
    // The difference #81 depends on #84 for. The revert path next door
    // `--abort`s on conflict, which was correct when there was nowhere to send
    // a conflict. There is now — so the cherry-pick is left IN PROGRESS with
    // the paths named, because aborting would throw away work the user can
    // finish.
    //
    // MUTATION: `--abort` on failure, as revert does. The sequencer state
    // disappears and the user is told it failed, with the resolution they
    // could have completed silently discarded.
    let (_dir, repo) = seeded_repo();
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("a.txt"), "from side\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "side changes a"]);
    let wanted = out(&repo, &["rev-parse", "HEAD"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "main changes a"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::CherryPick {
            commit: git_vista_protocol::CommitOid::new(wanted).unwrap(),
        },
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(
        body.contains("NOT complete"),
        "the response must say plainly it did not finish: {body}"
    );
    assert!(
        body.contains("a.txt"),
        "the conflicted path must be named: {body}"
    );
    // The promise must be true: the cherry-pick is still in progress.
    assert!(
        repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the sequencer state must survive — this is what makes it a pause"
    );
}

#[tokio::test]
async fn cherry_picking_a_merge_needs_a_mainline() {
    // Same refusal as revert, one verb over — and it must read naturally for
    // THIS verb, which is why the shared helper takes the word.
    let (_dir, repo) = merged_repo();
    let head = out(&repo, &["rev-parse", "HEAD"]);
    run(&repo, &["checkout", "-q", "-b", "elsewhere", "HEAD~1"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::CherryPick {
            commit: git_vista_protocol::CommitOid::new(head.clone()).unwrap(),
        },
    )
    .await;

    assert_ne!(status, axum::http::StatusCode::OK, "body: {body}");
    assert!(
        body.contains("cherry-picking it needs one more answer"),
        "the refusal must use THIS verb, not revert's wording: {body}"
    );
    assert!(
        body.contains("which side of the merge"),
        "and must name the decision: {body}"
    );
}

/// A repository mid-cherry-pick, stopped on a conflict in `a.txt`.
fn conflicted_pick(repo: &std::path::Path) -> String {
    run(repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("a.txt"), "from side\n").unwrap();
    run(repo, &["commit", "-q", "-am", "side changes a"]);
    let wanted = out(repo, &["rev-parse", "HEAD"]);
    run(repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    run(repo, &["commit", "-q", "-am", "main changes a"]);
    // Expected to fail — that is the point.
    let _ = std::process::Command::new("git")
        .args(["cherry-pick", &wanted])
        .current_dir(repo)
        .status();
    wanted
}

/// A repository mid-cherry-pick of TWO commits, stopped on a conflict in the
/// FIRST. Returns `(first, second)`.
///
/// # Why two and not one
///
/// With a single-commit sequence, `--skip` and `--abort` are externally
/// IDENTICAL: both clear the sequencer and both leave `a.txt` at main's
/// version, because skipping the only commit leaves nothing to apply. Every
/// assertion either test could make would hold under the other verb, so a
/// mutation that ran the wrong flag stayed green. Found by the M4 test-integrity
/// audit, 2026-08-22.
///
/// With two, the verbs diverge on a fact neither can fake: after `--skip` the
/// sequence carries on and lands the SECOND commit (`b.txt` appears); after
/// `--abort` the whole thing unwinds and it does not.
fn conflicted_pick_of_two(repo: &std::path::Path) -> (String, String) {
    run(repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("a.txt"), "from side\n").unwrap();
    run(repo, &["commit", "-q", "-am", "side changes a"]);
    let first = out(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("b.txt"), "second of two\n").unwrap();
    run(repo, &["add", "b.txt"]);
    run(repo, &["commit", "-q", "-m", "side adds b"]);
    let second = out(repo, &["rev-parse", "HEAD"]);
    run(repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    run(repo, &["commit", "-q", "-am", "main changes a"]);
    // Expected to stop on the FIRST of the two — that is the point.
    let _ = std::process::Command::new("git")
        .args(["cherry-pick", &format!("{first}^..{second}")])
        .current_dir(repo)
        .status();
    (first, second)
}

#[tokio::test]
async fn a_resolved_conflict_lets_the_sequence_continue() {
    // The whole loop #81 depends on #84 for: a cherry-pick conflicts, the user
    // resolves, and the sequence carries on. Before the conflict model existed
    // there was nowhere to send the conflict, so this path did not exist.
    let (_dir, repo) = seeded_repo();
    conflicted_pick(&repo);
    assert!(
        repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "fixture: a cherry-pick must be in progress"
    );

    // Resolve by hand, exactly as a user would.
    std::fs::write(repo.join("a.txt"), "resolved by hand\n").unwrap();
    run(&repo, &["add", "a.txt"]);

    let (status, body) = pipeline(&repo, GitOperation::SequenceContinue).await;
    assert_ok(status, &body);

    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "a completed sequence must leave no marker behind"
    );
    assert!(
        body.contains("complete"),
        "and the response must say the sequence finished: {body}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "resolved by hand\n",
        "the user's resolution is what must be committed"
    );
}

#[tokio::test]
async fn continuing_while_still_conflicted_is_refused() {
    // MUTATION: skip the conflict re-read and report whatever git's exit code
    // said. `git cherry-pick --continue` on an unresolved tree fails, but the
    // user would be told only that a command failed — not WHICH file is still
    // in the way, and not that the sequence is still open.
    let (_dir, repo) = seeded_repo();
    conflicted_pick(&repo);

    // Deliberately do NOT resolve.
    let (status, body) = pipeline(&repo, GitOperation::SequenceContinue).await;

    assert_ne!(status, axum::http::StatusCode::OK, "body: {body}");
    assert!(
        repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "a refused continue must leave the sequence exactly as it was"
    );

    // The two assertions above hold WITHOUT the conflict re-read, because git's
    // own exit code already refuses. That makes them tests of git, not of us.
    // What the re-read actually buys is naming the file still in the way, so
    // that is what is asserted. Found by mutation: removing the re-read left
    // everything above green.
    assert!(
        body.contains("a.txt"),
        "the response must name the path still blocking the sequence, not just \
         report that a command failed: {body}"
    );
}

#[tokio::test]
async fn skipping_drops_one_commit_and_keeps_going() {
    // The name promises "and keeps going", and until 2026-08-22 nothing here
    // tested the second half. On a single-commit fixture `--skip` and `--abort`
    // are indistinguishable, so this passed under either verb. Two commits make
    // the difference observable: skip drops the conflicting one and LANDS THE
    // NEXT.
    //
    // MUTATION: make SequenceSkip's flag() return "--abort". `b.txt` then never
    // arrives, and the commit-count assertion below goes red.
    let (_dir, repo) = seeded_repo();
    conflicted_pick_of_two(&repo);
    // Captured AFTER the fixture: it commits "main changes a" itself, and a tip
    // read before that would count the fixture's own commit as one the sequence
    // landed. This is where the paused sequence actually left HEAD.
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(&repo, GitOperation::SequenceSkip).await;
    assert_ok(status, &body);

    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the sequence must be finished, not left open"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "from main\n",
        "the skipped commit's version of a.txt must NOT be applied"
    );

    // The half the old test could not see. An abort would leave neither of
    // these true.
    assert!(
        repo.join("b.txt").exists(),
        "skip must CONTINUE the sequence and land the second commit; an abort \
         would have unwound it"
    );
    let after = out(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(
        after, before,
        "one commit of the two must have landed, so HEAD must have moved"
    );
    let landed = out(
        &repo,
        &["rev-list", "--count", &format!("{before}..{after}")],
    );
    assert_eq!(
        landed, "1",
        "exactly one of the two commits may land — the conflicting one is \
         dropped, not merged in"
    );
}

/// A repository mid-revert, stopped on a conflict in `a.txt`.
///
/// The revert twin of [`conflicted_pick`]. Both markers cannot be produced at
/// once by git, so this is the only way to get a sequence whose verb is
/// `revert` rather than `cherry-pick`.
fn conflicted_revert(repo: &std::path::Path) -> String {
    std::fs::write(repo.join("a.txt"), "first\n").unwrap();
    run(repo, &["commit", "-q", "-am", "first change to a"]);
    let target = out(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.txt"), "second\n").unwrap();
    run(repo, &["commit", "-q", "-am", "second change to a"]);
    // Reverting the first change conflicts with the second. Expected to fail —
    // that is the point.
    let _ = std::process::Command::new("git")
        .args(["revert", "--no-edit", &target])
        .current_dir(repo)
        .status();
    target
}

#[tokio::test]
async fn a_sequence_resumes_after_a_reconnect() {
    // #81's acceptance criterion "sequences resume after reconnect", and the
    // only one of the five with no test before this.
    //
    // A reconnect means the client that continues a sequence shares NOTHING
    // with the one that started it — no cached plan, no remembered verb, no
    // in-memory sequencer state. `pipeline` builds a fresh plan against the
    // live repository on every call, so calling it here is exactly that: the
    // sequence below is started by raw git and resumed by a planner run that
    // never saw it begin.
    //
    // MUTATION-PROVEN, two ways, 2026-08-22 — and the two disagreed:
    //
    //   1. Report a revert as a cherry-pick
    //      (`("REVERT_HEAD", "revert")` -> `("REVERT_HEAD", "cherry-pick")`)
    //      -> SURVIVED. See the note on the subject assertion below.
    //   2. Drop REVERT_HEAD from the marker table entirely
    //      -> CAUGHT: 409 "no cherry-pick or revert in progress".
    //
    // So what this test actually pins is (2): a sequence started by one
    // connection must still be FOUND by a later one that shares no state with
    // it. That is the acceptance criterion. It does not — and provably cannot,
    // by this route — pin which verb drives the continue.
    let (_dir, repo) = seeded_repo();
    conflicted_revert(&repo);

    assert!(
        repo.join(".git/REVERT_HEAD").exists(),
        "fixture: a revert must be in progress"
    );
    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "fixture: and it must not look like a cherry-pick"
    );

    // Resolve by hand, exactly as a user would after reconnecting.
    std::fs::write(repo.join("a.txt"), "resolved after reconnect\n").unwrap();
    run(&repo, &["add", "a.txt"]);

    let (status, body) = pipeline(&repo, GitOperation::SequenceContinue).await;
    assert_ok(status, &body);

    assert!(
        !repo.join(".git/REVERT_HEAD").exists(),
        "a completed sequence must leave no marker behind"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "resolved after reconnect\n",
        "the resolution made after the reconnect is what must be committed"
    );

    // Records the commit shape, but is NOT a test of the verb, and must not be
    // read as one. Mutation 1 above swapped the verb and this still passed:
    // git keeps ONE sequencer per repository, `--continue` drives whichever
    // sequence is open regardless of the verb spelled at the command line, and
    // the "Revert" subject was already fixed by the original `git revert`.
    // Left in as a regression guard on the commit message, with its own limits
    // stated so no later reader mistakes it for verb coverage.
    let subject = out(&repo, &["log", "-1", "--pretty=%s"]);
    assert!(
        subject.starts_with("Revert"),
        "the resumed revert must still produce a Revert commit: got {subject:?}"
    );
}

#[tokio::test]
async fn aborting_with_no_sequence_in_progress_is_refused() {
    // THE reason this refusal exists. `git cherry-pick --abort` on a clean
    // repository can SUCCEED while doing nothing — so without this check a
    // caller would be told an abort worked when there was never anything to
    // abort. A success that means nothing is the failure this whole codebase
    // is organised against.
    //
    // MUTATION: drop the sequence_in_progress check and pass the call through.
    let (_dir, repo) = seeded_repo();
    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "fixture: nothing in progress"
    );

    let (status, body) = pipeline(&repo, GitOperation::SequenceAbort).await;

    assert_eq!(status, axum::http::StatusCode::CONFLICT, "body: {body}");
    assert!(
        body.contains("no cherry-pick or revert in progress"),
        "the refusal must say plainly there was nothing to abort: {body}"
    );
}

#[tokio::test]
async fn aborting_unwinds_the_sequence() {
    // Two commits, not one: on a single-commit sequence an abort and a skip
    // leave byte-identical state, so this test passed under either verb until
    // 2026-08-22. The `b.txt` assertion below is what actually separates them.
    //
    // MUTATION: make SequenceAbort's flag() return "--skip". The sequence then
    // carries on, `b.txt` lands, and this goes red.
    let (_dir, repo) = seeded_repo();
    conflicted_pick_of_two(&repo);
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(&repo, GitOperation::SequenceAbort).await;
    assert_ok(status, &body);

    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "abort must clear the sequencer"
    );
    assert_eq!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "abort returns to where the sequence started"
    );
    assert!(
        !repo.join("b.txt").exists(),
        "abort must unwind the WHOLE sequence — the second commit must not have \
         landed; a skip would have applied it"
    );
    assert!(
        body.contains("resolutions made during it are gone"),
        "the response must say what was discarded, not just that it worked: {body}"
    );
}
