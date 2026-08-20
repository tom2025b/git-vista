//! The #145 staleness contract: a plan built against generation N is
//! refused once anything moves (a new commit, or even just the working
//! tree picking up an untracked file), tamper detection on the admission
//! hash (#249), plan expiry, the precondition-drift race, and — behind it
//! all — the `Observed`/`enforce_fresh` machinery's own honesty about what
//! it could and could not read (D5, #66 Task 19).

use super::*;
use std::path::PathBuf;

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

/// `git rev-parse HEAD` in `repo`, trimmed — for tests that need a real
/// oid to build a compare-and-swap `GitOperation` against (#222).
async fn git_rev_parse_head(repo: &Path) -> String {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A fresh repository on branch `main` with one committed file and a
/// clean working tree.
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

/// #249: the admission hash for a **submitted** plan is derived from its
/// operation, never read off the plan's own `operation_hash` field.
///
/// Found by adversarial review before this shipped. A submitted plan comes
/// from outside — today from an LLM's raw tool-call argument through the
/// MCP bridge — so `plan.operation_hash` is unverified client data at the
/// moment `admit()` needs a hash. And `admit()` runs *before* `validate()`,
/// which is the check that would catch a mismatch.
///
/// Trusting the field is exploitable two ways. A plan whose declared hash
/// collides with an already-admitted key takes `Admission::Existing` and
/// replays the **first** operation's terminal result — the second operation
/// never validated, never executed, caller told it succeeded. And a first
/// submission carrying a mismatched hash poisons that key: every later,
/// correctly-hashed resubmission gets `Admission::Conflict` forever.
///
/// This pins the fix at the seam where it matters. `validate()` rejecting a
/// mismatched plan is a different guarantee and does not cover this one —
/// it happens after admission has already committed.
#[tokio::test]
async fn a_submitted_plans_admission_hash_ignores_the_hash_the_plan_declares() {
    let (_dir, repo) = seeded_repo();
    let (plan, _observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    let honest = operation_hash(&plan.operation);

    // A plan that lies about its own operation — the shape a hostile or
    // simply buggy caller can put on the wire.
    let mut tampered = plan.clone();
    tampered.operation_hash = OperationHash::new("0".repeat(64)).unwrap();
    assert_ne!(
        tampered.operation_hash.as_str(),
        honest.as_str(),
        "the tampered fixture must actually differ, or this test proves nothing"
    );

    let admitted = PlanSource::Submit(Box::new(tampered)).hash();
    assert_eq!(
        admitted.as_str(),
        honest.as_str(),
        "admission must key on the hash derived from the operation, not the one \
             the plan declares — otherwise a colliding declared hash replays a \
             different operation's result"
    );
}

/// #145 acceptance 1 + 4 (the race): a plan built against generation N is
/// refused once anything moves — a new commit, or even just the working
/// tree picking up an untracked file — and a fresh plan passes.
#[tokio::test]
async fn a_generation_move_refuses_execution() {
    let (_dir, repo) = seeded_repo();
    let (plan, observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;

    // Fresh plan against an untouched repository: allowed.
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

    // Worktree-only drift (no ref moved): still a generation move.
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("changed while this plan was pending"), "{why}");

    // Ref drift (a new commit) on a *fresh* plan built after the file
    // appeared: build, then commit, then try to execute.
    let (plan, observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    run(&repo, &["add", "b.txt"]);
    run(&repo, &["commit", "-q", "-m", "moved"]);
    let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("changed while this plan was pending"), "{why}");
}

/// #145 acceptance 2: a plan whose operation no longer matches its
/// declared hash is refused (tamper detection).
#[tokio::test]
async fn a_tampered_operation_is_refused() {
    let (_dir, repo) = seeded_repo();
    let (mut plan, _observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    plan.operation = GitOperation::UnstageAll; // no longer what the hash approves
    let (status, why) = validate(&plan).unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("doesn't match"), "{why}");
}

/// #145 acceptance 3: an expired plan is refused with a reason the client
/// can show.
#[tokio::test]
async fn an_expired_plan_is_refused() {
    let (_dir, repo) = seeded_repo();
    let (mut plan, _observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    plan.expires_at = UnixSeconds(crate::activity::now_secs() - 1);
    let (status, why) = validate(&plan).unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("expired"), "{why}");
}

/// #145 acceptance 4, precondition flavor: a precondition that *held* at
/// build time and broke before execution refuses — here the push remote
/// disappears, which moves no ref and so slips past the generation check.
#[tokio::test]
async fn a_broken_precondition_refuses_execution() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["remote", "add", "origin", "/nowhere/upstream.git"]);
    let op = GitOperation::PushBranch {
        branch: BranchName::new("main").unwrap(),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream: false,
        force: ForcePublish::None,
    };
    let (plan, observed) = build_plan(&repo, op, tokens()).await;
    assert!(
        observed.held_at_build.iter().any(|&h| h),
        "remote precondition should hold"
    );
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

    run(&repo, &["remote", "remove", "origin"]);
    let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("no longer configured"), "{why}");
}

/// A precondition that already failed at build time is *not* enforced
/// here: it flows to the executor's legacy guard, so refusal texts stay
/// exactly what they always were.
///
/// # Why the example is `RefAbsent` and no longer `RemoteConfigured`
///
/// This test used to make the same point with a never-configured push
/// remote, and it passed for a reason it did not check: it asserted the
/// gate steps aside without ever asserting anything catches the operation
/// afterwards. For `RemoteConfigured` nothing does — `git push`/`git
/// fetch` reinterpret an unknown remote as a transport target rather than
/// refusing it — so the test was pinning the mechanism of a real hole
/// (`planner::remote_boundary_suite`, ADR 0047) as if it were a
/// guarantee.
///
/// The rule it states is still the rule, so it is kept and made
/// load-bearing instead of deleted: the example moves to `RefAbsent`,
/// where the executor's guard is real, and the second leg **proves** that
/// guard fires rather than assuming it. The exception is asserted
/// directly in the third leg, so the two halves of the policy are visible
/// in one place.
#[tokio::test]
async fn a_precondition_unmet_at_build_time_is_left_to_the_executor() {
    let (_dir, repo) = seeded_repo();
    run(&repo, &["branch", "dup"]);
    let op = GitOperation::CreateBranch {
        name: BranchName::new("dup").unwrap(), // already exists
        at: CommitOid::new(git_rev_parse_head(&repo).await).unwrap(),
    };
    let (plan, observed) = build_plan(&repo, op.clone(), tokens()).await;
    assert!(
        plan.preconditions
            .iter()
            .any(|p| matches!(p, Precondition::RefAbsent { .. })),
        "CreateBranch no longer carries RefAbsent — this test's premise is gone"
    );
    assert!(
        !observed.held_at_build.iter().any(|&h| h),
        "the precondition must be unmet at build time, or the gate below \
             is being asked the wrong question"
    );

    // Leg 1: the gate steps aside.
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

    // Leg 2 — the leg this test was missing: something downstream really
    // does refuse, in git's own words. Without it, "the gate steps aside"
    // is only half a claim.
    let (status, why) = plan_and_execute_in(&repo, None, tokens(), op).await;
    assert!(
        !status.is_success(),
        "the executor's own guard must refuse the duplicate branch: {why}"
    );
    assert!(
        why.contains("already exists"),
        "expected git's own wording, got: {why}"
    );

    // Leg 3: the exception. `RemoteConfigured` has no such guard, so the
    // gate must refuse it itself rather than stepping aside.
    let push = GitOperation::PushBranch {
        branch: BranchName::new("main").unwrap(),
        remote: RemoteName::new("origin").unwrap(), // never configured
        set_upstream: false,
        force: ForcePublish::None,
    };
    let (plan, observed) = build_plan(&repo, push, tokens()).await;
    let (status, why) = enforce_fresh(&repo, &plan, &observed)
        .await
        .expect_err("an unconfigured remote must not reach the executor");
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("not configured"), "{why}");
}

// -----------------------------------------------------------------------
// D5 (#66, Task 19): ExecUnavailable propagates as its own value.
//
// Every test below drives a *real* unrunnable repository
// (`git_cmd::unrunnable_repo` — a `.git` whose geometry the sandbox policy
// refuses, so no git is ever spawned). Nothing here is stubbed, and none
// of it would pass if `rev_parse` had simply been made infallible.
// -----------------------------------------------------------------------

/// An `Observed` with no unreadable fields, for the precondition checks
/// that only consult `live.head_branch` / `live.status`.
fn live_observed() -> Observed {
    Observed {
        head_branch: Some("main".to_string()),
        head_tip: Obs::Known("0".repeat(40)),
        branch_tip: Obs::Absent,
        status: Obs::Known(String::new()),
        held_at_build: Vec::new(),
    }
}

fn ref_name(s: &str) -> RefName {
    RefName::new(s).expect("valid ref name")
}

/// **The gate criterion.** `resolve_commit_oid` is an id-resolution gate,
/// and before D5 it answered the *same* 400 "not a valid object name" for
/// "git rejected this name" and for "git never ran". Those are now
/// different statuses, and the git-unavailable one must not be a 4xx: the
/// request was fine.
#[tokio::test]
async fn a_gate_distinguishes_git_unavailable_from_a_ref_that_is_absent() {
    let (_dir, repo) = seeded_repo();
    let (_hostile_dir, hostile) = crate::git_cmd::unrunnable_repo();

    // git ran and refused the name: the client's request is wrong.
    let (absent_status, absent_why) = resolve_commit_oid(&repo, "no-such-rev")
        .await
        .expect_err("a bogus rev must be refused");
    assert_eq!(absent_status, StatusCode::BAD_REQUEST);
    assert!(
        absent_why.contains("not a valid object name"),
        "git's own wording is preserved for the real refusal: {absent_why}"
    );

    // git never ran: nothing was refused, so nothing may be blamed on the
    // request.
    let (unavailable_status, unavailable_why) = resolve_commit_oid(&hostile, "no-such-rev")
        .await
        .expect_err("an unrunnable repository must be refused");
    assert_eq!(
        unavailable_status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "‘git could not run’ is a server fault, never a 400"
    );
    assert!(
        unavailable_why.contains("Couldn't run git"),
        "{unavailable_why}"
    );
    assert!(
        !unavailable_why.contains("not a valid object name"),
        "the old text asserted the user's input was bad on no evidence: \
             {unavailable_why}"
    );
    assert_ne!(
        absent_status, unavailable_status,
        "the two outcomes must be distinguishable by status alone"
    );
}

/// **The polarity criterion.** `RefAbsent` used to be *satisfied* by an
/// unreadable ref, while its two siblings refused on the identical input.
///
/// The first assertion reproduces the old expression verbatim against the
/// same fixture, so this is a regression pin and not merely a statement of
/// current behaviour: if `rev_parse` ever collapses back to a two-state
/// answer, that line is what the collapse would restore.
#[tokio::test]
async fn ref_absent_no_longer_treats_an_unreadable_ref_as_absent() {
    let (_hostile_dir, hostile) = crate::git_cmd::unrunnable_repo();
    let name = ref_name("refs/heads/feature");
    let live = live_observed();

    // The pre-D5 logic, written out: `rev_parse(...).await.is_none()`,
    // where `None` meant either "absent" or "git could not run".
    let pre_d5_said_absent = rev_parse(&hostile, name.as_str())
        .await
        .ok()
        .flatten()
        .is_none();
    assert!(
        pre_d5_said_absent,
        "the fixture must be one where the old expression answered \
             ‘absent’, or this test pins nothing"
    );

    let (status, why) = verify_precondition(
        &hostile,
        &Precondition::RefAbsent {
            ref_name: name.clone(),
        },
        &live,
    )
    .await
    .expect_err("an unreadable ref is not proof the ref is absent");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(why.contains("Couldn't run git"), "{why}");

    // And its two siblings, on the identical input, agree — the asymmetry
    // is gone rather than inverted.
    for precondition in [
        Precondition::RefExists {
            ref_name: name.clone(),
        },
        Precondition::RefAt {
            ref_name: name.clone(),
            oid: CommitOid::new("0".repeat(40)).unwrap(),
        },
    ] {
        let (status, _) = verify_precondition(&hostile, &precondition, &live)
            .await
            .expect_err("every ref precondition refuses on an unreadable ref");
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "and all three now use the *same* status for it"
        );
    }
}

/// The fix must not have been "refuse always": on a repository git can
/// run in, `RefAbsent` still passes for a branch that really is absent and
/// still refuses for one that exists. Without this, the test above would
/// pass against a `verify_precondition` that had been broken outright.
#[tokio::test]
async fn ref_absent_still_distinguishes_a_real_absence_from_a_real_ref() {
    let (_dir, repo) = seeded_repo();
    let live = live_observed();

    verify_precondition(
        &repo,
        &Precondition::RefAbsent {
            ref_name: ref_name("refs/heads/never-created"),
        },
        &live,
    )
    .await
    .expect("a branch that does not exist satisfies RefAbsent");

    let (status, why) = verify_precondition(
        &repo,
        &Precondition::RefAbsent {
            ref_name: ref_name("refs/heads/main"),
        },
        &live,
    )
    .await
    .expect_err("a branch that exists breaks RefAbsent");
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a ref that really is there is a 409 about the repository, \
             not a 500 about us"
    );
    assert!(
        why.contains("appeared while this plan was pending"),
        "{why}"
    );
}

/// **The freshness criterion.** Two `Unknown` observations must not
/// produce equal generation tokens, or the staleness gate compares two
/// non-observations, finds them "the same", and certifies as unchanged a
/// repository nobody read.
///
/// The control in the middle is what makes this non-vacuous: two identical
/// *real* observations must still compare equal, so the property being
/// pinned is "unknown is uncomparable", not "the token is random".
#[tokio::test]
async fn two_unknown_observations_never_compare_equal() {
    let (_dir, repo) = seeded_repo();

    let unknown = || Observed {
        head_branch: Some("main".to_string()),
        head_tip: Obs::Unknown,
        branch_tip: Obs::Absent,
        status: Obs::Known(String::new()),
        held_at_build: Vec::new(),
    };
    let known = || Observed {
        head_branch: Some("main".to_string()),
        head_tip: Obs::Known("abc123".to_string()),
        branch_tip: Obs::Absent,
        status: Obs::Known(String::new()),
        held_at_build: Vec::new(),
    };
    let absent = || Observed {
        head_branch: Some("main".to_string()),
        head_tip: Obs::Absent,
        branch_tip: Obs::Absent,
        status: Obs::Known(String::new()),
        held_at_build: Vec::new(),
    };

    // Control: two identical real observations DO compare equal. Without
    // this the whole freshness gate would be broken, not fixed.
    assert_eq!(
        generation_token(&repo, &known()).await.as_str(),
        generation_token(&repo, &known()).await.as_str(),
        "a real observation must be reproducible, or nothing is ever fresh"
    );
    assert_eq!(
        generation_token(&repo, &absent()).await.as_str(),
        generation_token(&repo, &absent()).await.as_str(),
    );

    // The criterion: two unknowns do not.
    assert_ne!(
        generation_token(&repo, &unknown()).await.as_str(),
        generation_token(&repo, &unknown()).await.as_str(),
        "two failed reads must not certify each other as unchanged"
    );

    // And unknown is distinguishable from both of the real answers.
    assert_ne!(
        generation_token(&repo, &unknown()).await.as_str(),
        generation_token(&repo, &absent()).await.as_str(),
    );
    assert_ne!(
        generation_token(&repo, &unknown()).await.as_str(),
        generation_token(&repo, &known()).await.as_str(),
    );
}

/// The digest tags are load-bearing on their own: an observed empty status
/// (a *clean* worktree) must not hash the same as one that could not be
/// read. Pre-D5 both went in as `""` via `unwrap_or_default`.
#[tokio::test]
async fn a_clean_worktree_does_not_hash_like_an_unreadable_one() {
    let (_dir, repo) = seeded_repo();
    let with = |status| Observed {
        head_branch: Some("main".to_string()),
        head_tip: Obs::Known("abc123".to_string()),
        branch_tip: Obs::Absent,
        status,
        held_at_build: Vec::new(),
    };
    assert_ne!(
        generation_token(&repo, &with(Obs::Known(String::new())))
            .await
            .as_str(),
        generation_token(&repo, &with(Obs::Absent)).await.as_str(),
        "‘clean’ and ‘not a working tree’ are different states"
    );
}

/// The gate is wired, not merely capable: a plan whose build-time
/// observation was `Unknown` is refused by `enforce_fresh` with the
/// git-unavailable status — and says so, rather than blaming the
/// repository for changing.
#[tokio::test]
async fn enforce_fresh_refuses_a_plan_built_on_an_unreadable_observation() {
    let (_dir, repo) = seeded_repo();
    let (plan, mut observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

    observed.head_tip = Obs::Unknown;
    let (status, why) = enforce_fresh(&repo, &plan, &observed)
        .await
        .expect_err("an unreadable observation cannot certify freshness");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(why.contains("Couldn't run git"), "{why}");
    assert!(
        !why.contains("changed while this plan was pending"),
        "we have no evidence the repository changed: {why}"
    );
}

/// The comparison behind “Already up to date”. `exec_merge` and
/// `exec_rebase` decide whether HEAD moved by calling
/// [`Obs::same_observation`]; two unreadable tips must not answer "it
/// didn't".
///
/// Note that `new == observed.head_tip` — what those two sites used to say
/// — no longer compiles at all: [`Obs`] deliberately has no `PartialEq`.
#[test]
fn two_unknown_tips_are_not_the_same_observation() {
    let unknown: Obs<String> = Obs::Unknown;
    assert!(
        !unknown.same_observation(&Obs::Unknown),
        "two reads that saw nothing are not evidence that nothing moved"
    );
    assert!(!unknown.same_observation(&Obs::Absent));
    assert!(!unknown.same_observation(&Obs::Known("a".into())));
    assert!(!Obs::Known("a".to_string()).same_observation(&Obs::Unknown));

    // The real answers still compare the way the callers need.
    assert!(Obs::Known("a".to_string()).same_observation(&Obs::Known("a".to_string())));
    assert!(!Obs::Known("a".to_string()).same_observation(&Obs::Known("b".to_string())));
    assert!(Obs::<String>::Absent.same_observation(&Obs::Absent));
}
