//! The #145 staleness contract: a plan built against generation N is
//! refused once anything moves (a new commit, or even just the working
//! tree picking up an untracked file), tamper detection on the admission
//! hash (#249), plan expiry, the precondition-drift race, and — behind it
//! all — the `Observed`/`enforce_fresh` machinery's own honesty about what
//! it could and could not read (D5, #66 Task 19).

use super::*;
use git_vista_fixtures::seeded as seeded_repo;
use git_vista_protocol::{OperationId, StashSelector};

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
    let (status, why) = plan_and_execute_in(
        &repo,
        None,
        tokens(),
        op,
        crate::planner::DropProof::Nothing,
    )
    .await;
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

/// A stage entry moving, with the working tree untouched, moves the
/// generation (M4.31c groundwork, #432).
///
/// # Why this test exists
///
/// #432 wants to carry a user-composed conflict resolution in the plan, and the
/// staleness argument for doing that safely rests on a claim nobody had
/// checked: that git's `status --porcelain=v2` `u` lines carry the three stage
/// OIDs, so any stage moving between build and execute is visible to
/// `enforce_fresh` for free.
///
/// That claim was asserted from git's documented format, not from this
/// repository. The precedent for why that is not good enough is a few hundred
/// lines up in `generation_token` itself: refs/stash had to be added as its own
/// digest field because `read_refs` keeps "only branches and tags", and until
/// #77 did so, **no stash write moved the generation at all**. Its comment
/// records how that was found — "Caught by a test written for #77's
/// 'generation updates are correct' criterion, not by inspection."
///
/// Same shape, same crate, so it gets the same treatment before a design leans
/// on it.
///
/// # What makes it a real test
///
/// The working tree file is written ONCE and never touched again. Only the
/// index changes, via `update-index --cacheinfo` at an explicit stage. If the
/// generation moved because the worktree moved, this would prove nothing.
#[tokio::test]
async fn a_stage_entry_moving_with_the_worktree_untouched_moves_the_generation() {
    let (_dir, repo) = seeded_repo();

    // Two blobs to point a stage entry at. Written through git so the objects
    // exist without any working-tree file ever holding them.
    let blob_one = String::from_utf8(
        std::process::Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"stage content one\n")?;
                c.wait_with_output()
            })
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let blob_two = String::from_utf8(
        std::process::Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"stage content two\n")?;
                c.wait_with_output()
            })
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_ne!(blob_one, blob_two, "fixture: two distinct blobs");

    // The working tree gets its bytes now, and never again.
    std::fs::write(repo.join("staged.txt"), "worktree bytes\n").unwrap();
    let worktree_before = std::fs::read(repo.join("staged.txt")).unwrap();

    run(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob_one},staged.txt"),
        ],
    );
    let before = generation_token(&repo, &observe_live(&repo).await)
        .await
        .as_str()
        .to_string();

    // Only the index moves: the same path, a different blob, no file write.
    run(
        &repo,
        &[
            "update-index",
            "--cacheinfo",
            &format!("100644,{blob_two},staged.txt"),
        ],
    );
    let after = generation_token(&repo, &observe_live(&repo).await)
        .await
        .as_str()
        .to_string();

    assert_eq!(
        std::fs::read(repo.join("staged.txt")).unwrap(),
        worktree_before,
        "fixture: the working tree must not have changed, or this proves nothing"
    );
    assert_ne!(
        before, after,
        "a stage entry moving must move the generation — otherwise a plan \
         approved against one staging state executes against another, and \
         enforce_fresh cannot see the difference"
    );
}

/// The same guarantee for an UNMERGED path: rewriting one of the three
/// conflict stages, with the working tree untouched, moves the generation
/// (M4.31c groundwork, #432).
///
/// # Why this is a separate test from the one above
///
/// The stage-0 test above passes through a different porcelain shape. A
/// non-conflicted staged path renders as a `1`/`2` line; only an unmerged path
/// renders as a `u` line, and it is the `u` line that carries the three stage
/// OIDs:
///
/// ```text
/// u UU N... 100644 100644 100644 100644 <stage1> <stage2> <stage3> a.txt
/// ```
///
/// #432's design leans on exactly that: "any stage moving between build and
/// execute is visible to `enforce_fresh` for free." Proving it for stage 0
/// would have looked like proving it and would not have been — the same
/// almost-right shape that let three M4 tests pass for months while pinning
/// nothing.
#[tokio::test]
async fn rewriting_one_conflict_stage_moves_the_generation() {
    let (_dir, repo) = seeded_repo();

    // A real conflict, so the path renders as a porcelain `u` line.
    run(&repo, &["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.join("a.txt"), "from side\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "side changes a"]);
    run(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "from main\n").unwrap();
    run(&repo, &["commit", "-q", "-am", "main changes a"]);
    let _ = std::process::Command::new("git")
        .args(["merge", "side"])
        .current_dir(&repo)
        .output();

    let unmerged = String::from_utf8(
        std::process::Command::new("git")
            .args(["ls-files", "-u", "--", "a.txt"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        unmerged.lines().count(),
        3,
        "fixture: a.txt must be unmerged with all three stages, got:\n{unmerged}"
    );

    // The conflict markers git wrote. Captured so the assertion at the end can
    // prove the working tree never moved.
    let worktree_before = std::fs::read(repo.join("a.txt")).unwrap();
    let before = generation_token(&repo, &observe_live(&repo).await)
        .await
        .as_str()
        .to_string();

    // Rewrite ONLY stage 3 (theirs), to a blob no file on disk holds.
    let replacement = String::from_utf8(
        std::process::Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"a third side, never on disk\n")?;
                c.wait_with_output()
            })
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let mut child = std::process::Command::new("git")
        .args(["update-index", "--index-info"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;

        writeln!(
            child.stdin.as_mut().unwrap(),
            "100644 {replacement} 3\ta.txt"
        )
        .unwrap();
    }
    assert!(child.wait().unwrap().success(), "update-index must succeed");

    let after = generation_token(&repo, &observe_live(&repo).await)
        .await
        .as_str()
        .to_string();

    assert_eq!(
        std::fs::read(repo.join("a.txt")).unwrap(),
        worktree_before,
        "fixture: the working tree must not have changed, or this proves nothing"
    );
    assert_ne!(
        before, after,
        "rewriting a conflict stage must move the generation — #432's staleness \
         story depends on a stage move being visible to enforce_fresh"
    );
}

// ---------------------------------------------------------------------------
// #514 — a drop that completes a pop must prove the tree still holds what the
// apply restored.
// ---------------------------------------------------------------------------

/// **The defect, driven end to end against real git.**
///
/// A composed pop is three unlinked requests. Between the apply and the drop,
/// another writer runs `git reset --hard` and throws the restored changes
/// away. The stash entry has not moved, so every check the drop used to make
/// still passes — and the entry is deleted over a tree that has lost the work.
///
/// The staleness gate cannot catch it: the plan is built *before* the guard is
/// taken, so interference arriving before plan-build reads as the valid
/// starting state rather than as drift. That is why the proof is a separate
/// check inside the guard rather than a tightening of `enforce_fresh`.
///
/// The two legs are the whole test. Leg 1 proves the guard actually refuses
/// the tampered case; leg 2 proves it is not refusing everything, which is the
/// half that would make leg 1 worthless on its own.
///
/// MUTATION 1 (remove the mechanism): make `proof_holds` return `Ok(())`
///   unconditionally — leg 1 goes red, the drop succeeds over a wiped tree.
/// MUTATION 2 (weaken it differently): compare the recorded generation against
///   the plan's own `observed` rather than a live read — leg 1 goes red again
///   but for the opposite reason, because the plan was built after the reset
///   and therefore agrees with it. That is the exact confusion the fix exists
///   to remove, so it is worth its own mutation.
#[tokio::test]
async fn a_pop_will_not_drop_a_stash_whose_applied_changes_were_wiped() {
    let (_dir, repo) = seeded_repo();

    // A stash to pop: modify a tracked file, stash it.
    std::fs::write(repo.join("a.txt"), "work worth keeping\n").unwrap();
    run(&repo, &["stash", "push", "-m", "the work"]);

    let entry = StashSelector::new("stash@{0}").unwrap();
    let oid = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "refs/stash"])
            .current_dir(&repo)
            .output()
            .unwrap();
        CommitOid::new(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
    };

    // Leg 1: a drop naming an apply that never happened cannot prove
    // anything, and must be refused rather than run.
    let phantom = OperationId::new("never-ran-this-one").unwrap();
    let (status, body) = plan_and_execute_in(
        &repo,
        None,
        tokens(),
        GitOperation::DropStash {
            entry: entry.clone(),
            expected_oid: oid.clone(),
        },
        crate::planner::DropProof::Completes(phantom),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a drop that cannot prove the tree must refuse, not run: {body}"
    );
    assert!(
        body.contains("left alone") || body.contains("NOT dropped"),
        "the refusal must say the entry was left alone: {body}"
    );

    // And the entry really is still there — the refusal is not just words.
    let after = std::process::Command::new("git")
        .args(["stash", "list"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("the work"),
        "the refused drop must leave the entry in the drawer"
    );

    // Leg 2: the same drop, with nothing to prove, still works. Without this
    // leg, a `proof_holds` that refused everything would pass leg 1.
    let (status, body) = plan_and_execute_in(
        &repo,
        None,
        tokens(),
        GitOperation::DropStash {
            entry,
            expected_oid: oid,
        },
        crate::planner::DropProof::Nothing,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a standalone drop proves nothing extra and must still run: {body}"
    );
}

/// **Finding 9: `merge.ff` must move the generation.**
///
/// The preview reads `merge.ff` **live** on every call
/// (`preview::fast_forward_policy`), and the merge executor runs
/// `git merge --no-edit`, which obeys it live too. Between those two live
/// reads sits an approved plan whose freshness token — the one thing standing
/// between "the picture you approved" and "the operation that runs" — was
/// computed from HEAD, every ref, `refs/stash` and the worktree status, and
/// **not** from config.
///
/// So: preview a fast-forwardable merge with `merge.ff` unset and the graph
/// shows a ref-only fast-forward that writes no commit. Set `merge.ff=false`
/// before approving. No ref moved, no file changed, so the generation still
/// matched and `enforce_fresh` said yes — and `git merge --no-edit` then wrote
/// a two-parent commit that appears nowhere in the approved graph. Reversing
/// `false` to `true` gives the inverse mismatch.
///
/// ADR 0099's claim that the preview and the executor cannot see different
/// configs is what this refutes: they are two live reads with an unguarded
/// window between them.
///
/// # Two mutations that make this red, failing differently
///
/// 1. **REMOVES the mechanism** — drop the `merge_ff` field from
///    `generation_token`. The config write moves nothing, `enforce_fresh`
///    returns `Ok`, and `unwrap_err` panics.
/// 2. **WEAKENS the mechanism** — have `merge_ff_digest_input` fold the *key's
///    presence* rather than its value (`"known"` instead of
///    `"known\0<value>"`). Setting the key from unset to `false` still moves
///    the token, so the first half stays green; the second half, which flips
///    an already-set `false` to `true`, goes red.
#[tokio::test]
async fn a_merge_ff_change_between_build_and_execute_refuses_execution() {
    let (_dir, repo) = git_vista_fixtures::fast_forward_merge_ff_unset();
    let merge = || GitOperation::MergeBranch {
        branch: git_vista_protocol::BranchName::new("feature").expect("a valid branch name"),
    };

    // The fixture's own contract: `merge.ff` is unset here, which is the state
    // the preview would have drawn a fast-forward from.
    assert!(
        !std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["config", "--get", "merge.ff"])
            .status()
            .unwrap()
            .success(),
        "the fixture must start with merge.ff unset, or this test is about \
         some other transition"
    );

    let (plan, observed) = build_plan(&repo, merge(), tokens()).await;
    assert!(
        enforce_fresh(&repo, &plan, &observed).await.is_ok(),
        "an untouched repository must let its own fresh plan through, or the \
         refusal below could be about anything"
    );

    // Nothing a ref, the stash or the worktree can see: only config.
    run(&repo, &["config", "merge.ff", "false"]);
    let (status, why) = enforce_fresh(&repo, &plan, &observed)
        .await
        .expect_err("merge.ff decides whether this plan's own operation writes a commit");
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("changed while this plan was pending"), "{why}");

    // And the reverse direction: an already-set value changing is a change
    // too. A digest that only noticed "the key exists now" would pass here.
    let (plan, observed) = build_plan(&repo, merge(), tokens()).await;
    assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());
    run(&repo, &["config", "merge.ff", "true"]);
    let (status, why) = enforce_fresh(&repo, &plan, &observed)
        .await
        .expect_err("false -> true flips the answer back and must invalidate too");
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("changed while this plan was pending"), "{why}");
}
