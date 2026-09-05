//! M11.02 (#547): the checkout-collision precondition, driven **without the
//! UI** — because that is the acceptance criterion.
//!
//! `docs/superpowers/specs/m3.23-worktrees.md` §2 states the rule this suite
//! pins: the precondition is a fact about the repository, not about the UI's
//! mood, so a client that offers the button anyway must be *refused* rather
//! than obeyed. Every test here submits a `GitOperation` straight into the
//! planner pipeline. Nothing in this file renders anything.
//!
//! # The two halves, and why both are here
//!
//! * The **decision** — given a census and a branch, is the branch free? —
//!   lives in `git_vista_protocol::branch_holder` and is host-tested beside
//!   the census types. Nothing in this file re-derives it.
//! * The **enforcement** — that a real repository with a real linked worktree
//!   produces a real 409 naming a real directory — needs a live git, and is
//!   what this file is for.
//!
//! A green suite here says the pipeline refuses. It says nothing about
//! whether the button is offered, which is `features::dialogs::core`'s
//! host-tested job in the frontend crate.

use super::*;
use git_vista_fixtures::seeded as seeded_repo;
use git_vista_protocol::{Serviceable, WorktreeSibling};

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

fn run(repo: &Path, args: &[&str]) {
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
}

/// A repository with a linked worktree named `desk` holding branch `branch`.
///
/// Returns `(repo_dir, worktree_dir, repo_path)`; both `TempDir`s must stay
/// alive for the duration of the test, and the linked worktree lives in its
/// own so that `git worktree add` gets a path that does not exist yet.
fn repo_with_a_sibling_on(
    branch_name: &str,
    desk: &str,
) -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    run(&repo, &["branch", branch_name]);
    let desks = tempfile::tempdir().unwrap();
    let desk_path = desks.path().join(desk);
    run(
        &repo,
        &["worktree", "add", desk_path.to_str().unwrap(), branch_name],
    );
    (dir, desks, repo)
}

async fn pipeline(repo: &Path, op: GitOperation) -> (StatusCode, String) {
    plan_and_execute_in(repo, None, tokens(), op, crate::planner::DropProof::Nothing).await
}

fn checkout(branch_name: &str) -> GitOperation {
    GitOperation::CheckoutBranch {
        branch: BranchName::new(branch_name).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// The plan says the rule out loud
// ---------------------------------------------------------------------------

/// Acceptance 1: the planner attaches the precondition. Without this the
/// server has nothing to enforce and the UI has nothing to read, so every
/// other test in this file would be pinning an accident.
#[tokio::test]
async fn the_plan_for_a_checkout_states_the_collision_precondition() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "feature/x"]);
    let (plan, _observed) = build_plan(&repo, checkout("feature/x"), tokens()).await;
    assert!(
        plan.preconditions
            .iter()
            .any(|p| matches!(
                p,
                Precondition::BranchFreeInEveryOtherWorktree { branch } if branch.as_str() == "feature/x"
            )),
        "a checkout plan must state that no other worktree holds the branch, got {:?}",
        plan.preconditions
    );
}

// ---------------------------------------------------------------------------
// The server refuses — with no UI anywhere in the call stack
// ---------------------------------------------------------------------------

/// Acceptance 2: a client that offers the button anyway is refused.
///
/// This drives `plan_and_execute_in` directly — the same entry point
/// `POST /api/execute-plan` reaches — so nothing about the frontend's
/// behaviour is load-bearing here. The status is the assertion; the wording
/// is the next test's.
#[tokio::test]
async fn the_server_refuses_a_checkout_of_a_branch_another_worktree_holds() {
    let (_dir, _desks, repo) = repo_with_a_sibling_on("feature/x", "desk-two");
    let (status, body) = pipeline(&repo, checkout("feature/x")).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a checkout git would certainly refuse must be refused here first: {body}"
    );
    // And it must be *this* refusal, not git's own relayed from the executor.
    assert!(
        !body.contains("fatal:"),
        "the executor was reached — the gate let a certainly-failing checkout \
         through and the user got git's raw words: {body}"
    );
}

/// Acceptance 3: "already checked out somewhere" is explicitly not an
/// acceptable message. The refusal names the desk, and says what to do.
///
/// A separate test from the status above on purpose: the two failures are
/// different defects — one lets a doomed command run, the other runs the
/// right check and reports it uselessly — and a mutation that causes one
/// should not be able to hide behind the other.
///
/// # Naming the worktree is not, on its own, an assertion worth making
///
/// Found by the mutation proof rather than by reading it back. With the gate
/// weakened (`refuses_when_unmet_at_build` returning `false` for this
/// precondition), git's own words reach the client:
///
/// ```text
/// fatal: 'feature/x' is already used by worktree at '/tmp/…/desk-two'
/// ```
///
/// That string **contains both names**. A test asserting only
/// `body.contains("desk-two")` therefore passes on precisely the dead end
/// this feature exists to replace — green, and proving nothing. So it also
/// asserts the two things git's `fatal:` cannot carry: the rule stated in
/// words, and a next step. That is the difference between a name and an
/// answer.
#[tokio::test]
async fn the_refusal_names_the_worktree_and_says_what_to_do_about_it() {
    let (_dir, _desks, repo) = repo_with_a_sibling_on("feature/x", "desk-two");
    let (_status, body) = pipeline(&repo, checkout("feature/x")).await;
    assert!(
        body.contains("desk-two"),
        "the refusal must name the worktree holding the branch, got: {body}"
    );
    assert!(
        body.contains("feature/x"),
        "the refusal must name the branch it is about, got: {body}"
    );
    assert!(
        body.contains("only one worktree at a time"),
        "the refusal must state the rule rather than relay git's dead end, got: {body}"
    );
    assert!(
        body.contains("check out a different branch here")
            || body.contains("Open")
            || body.contains("prune"),
        "the refusal must offer a next step, got: {body}"
    );
}

/// The paired positive, and the reason the two tests above mean anything: a
/// gate that refused every checkout would satisfy both of them. This one
/// fails the moment the precondition starts refusing branches nobody holds.
#[tokio::test]
async fn a_branch_no_other_worktree_holds_still_checks_out() {
    let (_dir, _desks, repo) = repo_with_a_sibling_on("feature/x", "desk-two");
    run(&repo, &["branch", "feature/free"]);
    let (status, body) = pipeline(&repo, checkout("feature/free")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a branch no other worktree holds must still be checkable-out: {body}"
    );
}

/// A worktree that takes the branch *between* planning and executing is a
/// refused race, not a raw `fatal:`. This is the whole reason the check is
/// re-run by `enforce_fresh` rather than trusted from build time — and the
/// path through `verify_precondition` rather than `unmet_at_build`.
#[tokio::test]
async fn a_worktree_that_takes_the_branch_after_the_plan_is_built_is_a_refused_race() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "feature/x"]);

    // Built while the branch is free: the precondition holds here.
    let (plan, observed) = build_plan(&repo, checkout("feature/x"), tokens()).await;
    assert!(
        observed
            .held_at_build
            .iter()
            .zip(&plan.preconditions)
            .any(|(held, p)| *held
                && matches!(p, Precondition::BranchFreeInEveryOtherWorktree { .. })),
        "the premise of this test is gone: the precondition must hold at build time"
    );

    // Another desk opens on it before the plan is executed.
    let desks = tempfile::tempdir().unwrap();
    let desk_path = desks.path().join("late-desk");
    run(
        &repo,
        &["worktree", "add", desk_path.to_str().unwrap(), "feature/x"],
    );

    let refused = enforce_fresh(&repo, &plan, &observed)
        .await
        .expect_err("a worktree taking the branch mid-plan must refuse the execution");
    assert!(
        refused.1.contains("late-desk"),
        "the race refusal must name the worktree that took the branch, got: {}",
        refused.1
    );
}

// ---------------------------------------------------------------------------
// The three census outcomes, in the one place they become English
// ---------------------------------------------------------------------------

fn sibling(name: &str, serviceable: Serviceable) -> WorktreeSibling {
    WorktreeSibling {
        repository: "repo-1".to_string(),
        id: format!("worktree-{name}"),
        name: name.to_string(),
        path: None,
        branch: Some(BranchName::new("feature/x").unwrap()),
        head: None,
        is_current: false,
        locked: false,
        prunable: false,
        bare: false,
        serviceable,
    }
}

fn observed_holding(serviceable: Serviceable) -> WorktreeCensus {
    WorktreeCensus::Observed {
        siblings: vec![sibling("desk-two", serviceable)],
    }
}

/// A sibling this application may open gets the offer the spec asks for:
/// select that worktree instead.
#[test]
fn a_serviceable_holder_is_offered_as_the_place_to_go() {
    let (status, body) = collision_refusal(
        &BranchName::new("feature/x").unwrap(),
        &observed_holding(Serviceable::Yes),
        CollisionMoment::AlreadySo,
    );
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("desk-two"), "{body}");
    assert!(
        body.contains("Open"),
        "a worktree the app can open must be offered, not merely named: {body}"
    );
}

/// A holder outside the allowed roots still blocks the checkout — git's
/// refusal does not consult this application's fence — but it must NOT be
/// offered as somewhere to go, because selecting it is refused. Visibility
/// for a collision check must never widen the boundary.
#[test]
fn a_holder_outside_the_allowed_roots_blocks_without_being_offered() {
    let (status, body) = collision_refusal(
        &BranchName::new("feature/x").unwrap(),
        &observed_holding(Serviceable::OutsideAllowedRoots),
        CollisionMoment::AlreadySo,
    );
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("desk-two"), "{body}");
    assert!(
        body.contains("outside"),
        "the refusal must say why this one cannot be opened: {body}"
    );
    assert!(
        !body.contains("Open ‘desk-two’ instead"),
        "a worktree the app refuses to open must not be offered as a destination: {body}"
    );
}

/// A prunable holder whose directory is gone still blocks, and the way out is
/// `git worktree prune` rather than a button.
#[test]
fn a_missing_holder_blocks_and_says_how_to_release_the_branch() {
    let (_status, body) = collision_refusal(
        &BranchName::new("feature/x").unwrap(),
        &observed_holding(Serviceable::Missing),
        CollisionMoment::AlreadySo,
    );
    assert!(body.contains("desk-two"), "{body}");
    assert!(body.contains("prune"), "{body}");
}

/// The fail-open this precondition exists to close: an unread census must
/// refuse, and must NOT name a worktree nobody observed.
#[test]
fn an_unread_census_refuses_without_inventing_a_worktree() {
    let census = WorktreeCensus::CensusFailed {
        reason: "`git worktree list --porcelain` failed: no such file".to_string(),
        detail: None,
    };
    let (status, body) = collision_refusal(
        &BranchName::new("feature/x").unwrap(),
        &census,
        CollisionMoment::AlreadySo,
    );
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a check that could not run is this server's failure, not the repository's: {body}"
    );
    assert!(
        body.contains("couldn't check"),
        "the refusal must say the check could not run: {body}"
    );
    assert!(
        !body.contains("is already checked out"),
        "an unread census must never claim another worktree has the branch: {body}"
    );
}

/// The placeholder census an operation that never needed one carries is a
/// *failure*, not an empty observation — so a future operation that acquires
/// this precondition without acquiring its census refuses rather than passes.
#[test]
fn the_no_census_placeholder_is_not_an_empty_observation() {
    let placeholder = no_census_taken();
    let holder =
        git_vista_protocol::branch_holder(&placeholder, &BranchName::new("feature/x").unwrap());
    assert!(
        !holder.permits_checkout(),
        "an operation that took no census must not be told the branch is free"
    );
}
