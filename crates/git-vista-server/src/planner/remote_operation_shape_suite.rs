//! M2.20a (#227) / M2.21a (#235) / M2.21f (#240): the plan-building shape of
//! every remote-reaching operation — `FetchRemote`, `PullBranch`, the
//! widened `PushBranch` (including the lease force-push pinning the
//! remote-tracking ref), and the two remote-reaching tag operations —
//! contract only, no execution.

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

// -----------------------------------------------------------------------
// M2.20a (#227): `FetchRemote` / `PullBranch` / the widened `PushBranch`
// in `shape` — contract only, no execution (see each variant's doc
// comment in `plan.rs` and `planner::execute`'s stub arms).
// -----------------------------------------------------------------------

/// A repository with a real, configured `origin` on disk — several shape
/// tests below need `RemoteConfigured` to actually hold, so that
/// `held_at_build` proving the preconditions are satisfiable means
/// something.
async fn seeded_repo_with_remote() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = seeded_repo();
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    (dir, repo)
}

/// Fetch is `Safe` with `NotNeeded` recovery, one `RemoteConfigured`
/// precondition, and **no** expected ref change.
///
/// The negative assertions are the point. `Safe`/`NotNeeded` is an
/// unusual pairing for a network operation, and the plausible wrong
/// answers are exactly the ones a later edit would reach for by reflex:
/// `RiskLevel::Remote` (because it talks to a remote) or
/// `RecoveryStrategy::Irrecoverable` (because push has it). Both are
/// pinned as *not* the answer, with the reasoning in the variant's doc
/// comment — a fetch cannot lose anything a user owns.
#[tokio::test]
async fn fetch_remote_shape_is_safe_with_nothing_to_recover() {
    let (_dir, repo) = seeded_repo_with_remote().await;
    let op = GitOperation::FetchRemote {
        remote: RemoteName::new("origin").unwrap(),
    };
    let (plan, observed) = build_plan(&repo, op, tokens()).await;

    assert_eq!(plan.risk, RiskLevel::Safe);
    assert_ne!(
        plan.risk,
        RiskLevel::Remote,
        "reach and risk are independent axes — see the variant's doc"
    );
    assert_eq!(
        plan.preconditions,
        vec![Precondition::RemoteConfigured {
            remote: RemoteName::new("origin").unwrap(),
        }]
    );
    assert!(
        plan.expected_ref_changes.is_empty(),
        "which refs/remotes/* move is unknowable before git speaks to the \
             remote; a guessed RefChange would be a claim shown to a reviewer"
    );
    assert_eq!(plan.recovery, RecoveryStrategy::NotNeeded);
    assert_ne!(plan.recovery, RecoveryStrategy::Irrecoverable);
    assert!(
        observed.held_at_build.iter().all(|&h| h),
        "the remote is configured, so the one precondition must hold — \
             otherwise this test would pass against an unsatisfiable shape"
    );
}

/// Pull is `Reversible` with a CAS on the **local** branch and `ResetRef`
/// recovery back to the tip the plan observed — the same story merge and
/// rebase have, because a pull is a fetch plus one of those.
///
/// Two negatives carry the reasoning: it must not be `Irrecoverable`
/// (that is push's tag, for an effect that left the machine — a pull's
/// did not), and its `RefAt` must name `refs/heads/main`, not
/// `refs/remotes/origin/main`. Pinning the remote tip would refuse a pull
/// for the ordinary reason that the remote received a commit, i.e. for
/// the very thing being pulled.
#[tokio::test]
async fn pull_branch_shape_is_reversible_with_a_local_cas_and_reset_recovery() {
    let (_dir, repo) = seeded_repo_with_remote().await;
    let head_oid = CommitOid::new(git_rev_parse_head(&repo).await).unwrap();
    let main = RefName::new("refs/heads/main").unwrap();

    for strategy in [
        git_vista_protocol::MergeStrategy::Merge,
        git_vista_protocol::MergeStrategy::Rebase,
    ] {
        let op = GitOperation::PullBranch {
            remote: RemoteName::new("origin").unwrap(),
            branch: BranchName::new("main").unwrap(),
            strategy,
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;

        assert_eq!(plan.risk, RiskLevel::Reversible, "{strategy:?}");
        assert_eq!(
            plan.preconditions,
            vec![
                Precondition::BranchCheckedOut {
                    branch: BranchName::new("main").unwrap(),
                },
                Precondition::RemoteConfigured {
                    remote: RemoteName::new("origin").unwrap(),
                },
                Precondition::RefAt {
                    ref_name: main.clone(),
                    oid: head_oid.clone(),
                },
            ],
            "{strategy:?}"
        );
        assert!(
            !plan.preconditions.iter().any(|p| matches!(
                p,
                Precondition::RefAt { ref_name, .. }
                    if ref_name.as_str().starts_with("refs/remotes/")
            )),
            "{strategy:?}: a pull must not pin the remote tip — that would \
                 refuse the pull for the reason it exists"
        );
        assert_eq!(
            plan.expected_ref_changes,
            vec![RefChange {
                ref_name: main.clone(),
                before: RefState::At(head_oid.clone()),
                after: RefState::Computed,
            }],
            "{strategy:?}"
        );
        assert_eq!(
            plan.recovery,
            RecoveryStrategy::ResetRef {
                ref_name: main.clone(),
                to: head_oid.clone(),
            },
            "{strategy:?}"
        );
        assert_ne!(
            plan.recovery,
            RecoveryStrategy::Irrecoverable,
            "{strategy:?}: a pull's effect never left this machine"
        );
        assert!(observed.held_at_build.iter().all(|&h| h), "{strategy:?}");
    }
}

/// The lease is a compare-and-swap on the **remote-tracking** ref, and it
/// exists only when a lease was actually asked for.
///
/// Both halves run against the same repository, so the difference is
/// attributable to `force` and nothing else. Without the negative half a
/// `shape` that emitted the lease precondition unconditionally would pass
/// — and an unconditional precondition on `refs/remotes/origin/main`
/// would refuse ordinary pushes whenever the remote had moved, which is
/// most of the time.
#[tokio::test]
async fn only_a_lease_force_push_pins_the_remote_tracking_ref() {
    let (_dir, repo) = seeded_repo_with_remote().await;
    let tracking = RefName::new("refs/remotes/origin/main").unwrap();
    let lease_tip = CommitOid::new("4".repeat(40)).unwrap();

    let lease_precondition = |plan: &Plan| {
        plan.preconditions
            .iter()
            .find(|p| matches!(p, Precondition::RefAt { ref_name, .. } if *ref_name == tracking))
            .cloned()
    };

    let (plain, _) = build_plan(
        &repo,
        GitOperation::PushBranch {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::None,
        },
        tokens(),
    )
    .await;
    assert_eq!(plain.risk, RiskLevel::Remote);
    assert_eq!(
        lease_precondition(&plain),
        None,
        "a fast-forward push must not pin the remote tip"
    );

    let (leased, _) = build_plan(
        &repo,
        GitOperation::PushBranch {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::WithLease {
                expected_remote_tip: lease_tip.clone(),
            },
        },
        tokens(),
    )
    .await;
    assert_eq!(
        leased.risk,
        RiskLevel::Destructive,
        "a lease-force can leave remote commits referenced by nothing"
    );
    assert_eq!(
        lease_precondition(&leased),
        Some(Precondition::RefAt {
            ref_name: tracking,
            oid: lease_tip.clone(),
        }),
        "the lease must become a live compare-and-swap on the tracking ref"
    );
    // The oid must be the *reviewed* one, not one re-read from the repo.
    // A lease re-derived at plan time would assert only that the remote
    // has not moved since a millisecond ago, and would protect nobody.
    assert_ne!(
        lease_tip.as_str(),
        git_rev_parse_head(&repo).await,
        "the fixture's lease oid must differ from anything in the repo, or \
             this test could not tell a carried oid from a re-read one"
    );
    // Recovery is unchanged by the force mode: the effect left the machine
    // either way.
    assert_eq!(plain.recovery, RecoveryStrategy::Irrecoverable);
    assert_eq!(leased.recovery, RecoveryStrategy::Irrecoverable);
}

/// M2.21a (#235) classified the two remote-reaching tag operations ahead
/// of their execution; M2.21f (#240) wires the execution but must not
/// re-litigate the classification — this pins it as literal values on a
/// plan `build_plan` actually produced, the tag-shaped twin of
/// `only_a_lease_force_push_pins_the_remote_tracking_ref` above.
///
/// # The two risk values deliberately differ from each other
///
/// #240's own issue text asks for `RiskLevel::Remote` on **both**; the
/// shipped `shape()` arms disagree, and the `RiskLevel` enum's own
/// ranking rule (plan.rs) says the shipped arms are right: a remote ref
/// disappearing (`DeleteRemoteTag`) outranks a remote ref merely gaining
/// a tag (`PushTag`), so only the delete is `Destructive`. Asserting both
/// values in one test — rather than one value per test — is what a
/// mutation collapsing the two onto a single `RiskLevel` cannot survive:
/// asserting `Remote` alone would still pass against a classifier that
/// answered `Remote` for everything.
#[tokio::test]
async fn remote_tag_operations_are_classified_remote_and_destructive_never_the_same() {
    let (_dir, repo) = seeded_repo_with_remote().await;
    run(&repo, &["tag", "-a", "-m", "v1", "v1.0.0"]);

    let (deleted, _) = build_plan(
        &repo,
        GitOperation::DeleteRemoteTag {
            name: TagName::new("v1.0.0").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        },
        tokens(),
    )
    .await;
    assert_eq!(
        deleted.risk,
        RiskLevel::Destructive,
        "a remote ref disappearing outranks a remote ref merely gaining \
             a tag — see plan.rs's RiskLevel ranking rule"
    );
    assert_eq!(deleted.recovery, RecoveryStrategy::Irrecoverable);
    assert!(
        deleted.expected_ref_changes.is_empty(),
        "a remote tag has no local remote-tracking ref to show moving \
             (D5) — {:?}",
        deleted.expected_ref_changes
    );

    let (pushed, _) = build_plan(
        &repo,
        GitOperation::PushTag {
            name: TagName::new("v1.0.0").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
        },
        tokens(),
    )
    .await;
    assert_eq!(
        pushed.risk,
        RiskLevel::Remote,
        "publishing a tag is additive, like a fast-forward branch push"
    );
    assert_eq!(pushed.recovery, RecoveryStrategy::Irrecoverable);
    assert!(
        pushed.expected_ref_changes.is_empty(),
        "{:?}",
        pushed.expected_ref_changes
    );
    assert_eq!(
        pushed.preconditions,
        vec![
            Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            },
            Precondition::RefExists {
                ref_name: RefName::new("refs/tags/v1.0.0").unwrap(),
            },
        ],
        "PushTag's preconditions are richer than #240's issue text states \
             — both RemoteConfigured and RefExists must hold, and neither is \
             optional"
    );
}
