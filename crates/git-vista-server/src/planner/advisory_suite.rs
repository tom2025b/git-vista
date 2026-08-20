//! M4.32 (#85): the advisories a force-with-lease push earns, and — more
//! importantly — the ones it does not.
//!
//! The interesting property here is not "a default-branch push is flagged".
//! It is that **three outcomes stay three outcomes**: the target *is* the
//! default branch, the target *is not* the default branch, and *we could not
//! tell*. Collapsing the third into the second is the failure this whole
//! estate keeps paying for — a check that did not run, reported as a check
//! that passed. A repository with no `refs/remotes/<remote>/HEAD` is the
//! common case (a manually added remote never sets it), so the wrong
//! behaviour here would be silent and permanent.
//!
//! Every test drives the real `build_plan_only` against a real repository on
//! disk. The advisories are read off the plan a reviewer would actually see.

use super::*;
use git_vista_protocol::{Advisory, BranchName, CommitOid, ForcePublish, RemoteName};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

/// A repository on `main` with one commit and an `origin` remote whose
/// remote-tracking refs exist. `default` decides whether
/// `refs/remotes/origin/HEAD` is set, which is the whole variable under test.
fn repo_with_origin(default: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    // A real bare remote, so `origin` is genuinely configured rather than a
    // config line pointing at nothing.
    let remote = dir.path().join("origin.git");
    run(dir.path(), &["init", "-q", "--bare", "origin.git"]);
    run(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&repo, &["push", "-q", "origin", "main"]);
    run(&repo, &["branch", "-q", "topic"]);
    run(&repo, &["push", "-q", "origin", "topic"]);

    if let Some(branch) = default {
        // What a real `git clone` records, set explicitly here because
        // `git remote add` never does.
        run(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
            ],
        );
    }
    (dir, repo)
}

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("advisory-suite-repo").unwrap(),
        WorktreeToken::new("advisory-suite-worktree").unwrap(),
    )
}

fn force_push_to(branch: &str) -> GitOperation {
    GitOperation::PushBranch {
        branch: BranchName::new(branch).unwrap(),
        remote: RemoteName::new("origin").unwrap(),
        set_upstream: false,
        force: ForcePublish::WithLease {
            expected_remote_tip: CommitOid::new("a".repeat(40)).unwrap(),
        },
    }
}

async fn advisories_of(repo: &Path, op: GitOperation) -> Vec<Advisory> {
    build_plan_only(repo, op, tokens()).await.advisories
}

// ---------------------------------------------------------------------------
// The three outcomes stay three
// ---------------------------------------------------------------------------

#[tokio::test]
async fn force_pushing_the_default_branch_is_flagged_as_such() {
    let (_dir, repo) = repo_with_origin(Some("main"));
    let advisories = advisories_of(&repo, force_push_to("main")).await;

    assert!(
        advisories.iter().any(|a| matches!(
            a,
            Advisory::DefaultBranchPush { branch, .. } if branch.as_str() == "main"
        )),
        "expected a DefaultBranchPush advisory naming main, got {advisories:?}"
    );
}

#[tokio::test]
async fn force_pushing_a_topic_branch_earns_no_default_branch_advisory() {
    // MUTATION: emit DefaultBranchPush unconditionally. Every force push would
    // then carry the warning, which is how a warning stops being read — the
    // same argument FetchRemote's docs make for refusing to overstate risk.
    let (_dir, repo) = repo_with_origin(Some("main"));
    let advisories = advisories_of(&repo, force_push_to("topic")).await;

    assert!(
        !advisories
            .iter()
            .any(|a| matches!(a, Advisory::DefaultBranchPush { .. })),
        "topic is not the default branch; expected no DefaultBranchPush, got {advisories:?}"
    );
    // ...and it must not have silently become "unknown" either: the check ran
    // and answered, so neither variant belongs.
    assert!(
        !advisories
            .iter()
            .any(|a| matches!(a, Advisory::DefaultBranchUnknown { .. })),
        "the default branch WAS readable here; reporting it as unknown hides a working check"
    );
}

#[tokio::test]
async fn an_unreadable_default_branch_is_unknown_and_never_silence() {
    // THE test in this file. MUTATION: treat a missing refs/remotes/origin/HEAD
    // as "not the default branch" and emit nothing. A force-push onto main in
    // any repository with a manually added remote would then go out with no
    // advisory, and nothing would ever say the check did not happen.
    let (_dir, repo) = repo_with_origin(None);
    let advisories = advisories_of(&repo, force_push_to("main")).await;

    let unknown = advisories
        .iter()
        .find_map(|a| match a {
            Advisory::DefaultBranchUnknown { reason } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("expected DefaultBranchUnknown when HEAD is absent, got {advisories:?}")
        });

    assert!(
        unknown.contains("main"),
        "the reason must name the branch it could not judge: {unknown}"
    );
    // And it must not ALSO claim the positive finding it could not make.
    assert!(
        !advisories
            .iter()
            .any(|a| matches!(a, Advisory::DefaultBranchPush { .. })),
        "an unknown default branch cannot also be a confirmed default-branch push"
    );
}

// ---------------------------------------------------------------------------
// Scope: only a force-with-lease earns advisories
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ordinary_push_earns_no_advisories_even_on_the_default_branch() {
    // MUTATION: drop the ForcePublish::WithLease guard from advisories_for.
    // A plain push cannot replace remote history, so warning on it trains the
    // user to click through the warnings that matter.
    let (_dir, repo) = repo_with_origin(Some("main"));
    let advisories = advisories_of(
        &repo,
        GitOperation::PushBranch {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::None,
        },
    )
    .await;

    assert!(
        advisories.is_empty(),
        "a non-force push earns no advisories, got {advisories:?}"
    );
}

#[tokio::test]
async fn a_force_with_lease_always_states_that_the_remote_cannot_be_undone() {
    // Acceptance criterion "recovery guidance never implies remote undo".
    // RecoveryStrategy describes what git-vista can restore locally; this
    // advisory states the part it cannot reach, and it must appear on EVERY
    // force-with-lease, including ones onto a topic branch.
    let (_dir, repo) = repo_with_origin(Some("main"));

    for branch in ["main", "topic"] {
        let advisories = advisories_of(&repo, force_push_to(branch)).await;
        assert!(
            advisories.iter().any(|a| matches!(
                a,
                Advisory::RemoteHistoryReplaced { branch: b, .. } if b.as_str() == branch
            )),
            "force-with-lease onto {branch} must state the remote cannot be undone, \
             got {advisories:?}"
        );
    }
}

#[tokio::test]
async fn a_non_push_operation_earns_no_advisories() {
    let (_dir, repo) = repo_with_origin(Some("main"));
    let advisories = advisories_of(
        &repo,
        GitOperation::DeleteBranch {
            branch: BranchName::new("topic").unwrap(),
        },
    )
    .await;
    assert!(
        advisories.is_empty(),
        "advisories are a push concern today, got {advisories:?}"
    );
}
