//! #327 defect B: `git revert`'s failure classification — the conflict
//! marker matching the owner's own session log verbatim, and the mirror
//! case where a dirty-tree refusal must stay unclassified, forwarding
//! git's own words as-is.

use super::*;
use std::path::PathBuf;

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

/// #327 defect B: the classifier must match **the exact string from the
/// owner's own session log** — not a paraphrase, not a shape resembling
/// it. This is the literal text this fix exists to handle.
#[test]
fn revert_conflict_marker_matches_the_owners_real_repro() {
    let real = "error: could not revert f993ba6... LangChain - Company \
                     Research Agent\n  hint: after resolving the conflicts, \
                     mark the corrected paths";
    assert!(
        looks_like_revert_conflict(real),
        "must classify the owner's own repro text as a conflict"
    );
}

/// The two-hint form `git revert --no-commit` actually prints on this
/// server's git (2.43.0) — captured verbatim from a real conflicting
/// revert in a scratch repo, not retyped from memory.
#[test]
fn revert_conflict_marker_matches_git_2_43s_no_commit_stderr() {
    let real = "error: could not revert e0754a0... add line2\n\
                     hint: after resolving the conflicts, mark the corrected paths\n\
                     hint: with 'git add <paths>' or 'git rm <paths>'";
    assert!(looks_like_revert_conflict(real));
}

/// The negative leg pull's own `looks_like_conflict` test insists on
/// (ADR 0044): a real, differently-shaped refusal that never touched the
/// working tree must not be tagged a conflict, or the marker set could
/// be deleted entirely and this test suite would not notice. Captured
/// verbatim from `git revert --no-commit` against a dirty working tree
/// on this server's git.
#[test]
fn revert_conflict_marker_ignores_a_dirty_tree_refusal() {
    let dirty = "error: Your local changes to the following files would \
                      be overwritten by merge:\n\tf.txt\nPlease commit your \
                      changes or stash them before you merge.\nAborting\n\
                      fatal: revert failed";
    assert!(
        !looks_like_revert_conflict(dirty),
        "a refusal that never touched the tree must not read as a conflict"
    );
}

/// #327 defect B, end to end: a real conflicting revert — later history
/// depends on what the reverted commit changed, the same shape as the
/// owner's repro — must come back `409` with a sentence a browser-only
/// user can act on, and the abort promise this function's doc comment
/// makes (nothing changed) must actually hold.
///
/// Mutation this proves: replace `revert_step1_failure_response`'s body
/// with `(StatusCode::BAD_REQUEST, git_said.to_string())` (i.e. delete
/// the classification entirely) and this goes red on the status
/// assertion — the 409 is load-bearing, not decoration.
#[tokio::test]
async fn a_conflicting_revert_is_reported_as_a_classified_conflict() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "add b"]);
    let to_revert = CommitOid::new(git_rev_parse_head(&repo).await).unwrap();

    std::fs::write(repo.join("a.txt"), "a\nb\nc\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "add c, needs b"]);

    let observed = observe_live(&repo).await;
    let (status, message) =
        exec_revert(&repo, NetworkNeed::Local, &to_revert, None, &observed).await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a conflicting revert must be reported as a classified conflict, \
             not a generic 400: {message}"
    );
    assert!(
        message.to_ascii_lowercase().contains("conflict"),
        "the response must say in words that this is a conflict: {message}"
    );
    assert!(
        message.contains("Nothing was applied"),
        "the response must say the repository is unchanged: {message}"
    );

    // The abort promise: the working tree must be exactly as clean as it
    // was before the attempt, and no revert must be mid-flight.
    let status_out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        status_out.stdout.is_empty(),
        "a rejected conflicting revert must leave a clean working tree"
    );
    let revert_head = repo.join(".git").join("REVERT_HEAD");
    assert!(
        !revert_head.exists(),
        "a rejected conflicting revert must not leave a revert in progress"
    );
}

/// The mirror case: a revert that fails for a reason that is **not** a
/// conflict (a dirty working tree) must keep exactly the old behavior —
/// `400`, git's own words forwarded verbatim, no invented sentence.
/// Proves the classifier's `false` arm is wired through, not just its
/// `true` arm.
#[tokio::test]
async fn a_non_conflict_revert_failure_keeps_forwarding_gits_words_verbatim() {
    let (_dir, repo) = seeded_repo();
    std::fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    run(&repo, &["add", "a.txt"]);
    run(&repo, &["commit", "-q", "-m", "add b"]);
    let to_revert = CommitOid::new(git_rev_parse_head(&repo).await).unwrap();

    // Leave the working tree dirty: `git revert` refuses before it ever
    // attempts the merge, so this is never classified a conflict.
    std::fs::write(repo.join("a.txt"), "a\nb\ndirty, not committed\n").unwrap();

    let observed = observe_live(&repo).await;
    let (status, message) =
        exec_revert(&repo, NetworkNeed::Local, &to_revert, None, &observed).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !message.contains("Nothing was applied"),
        "a non-conflict refusal must not get the conflict sentence: {message}"
    );
}
