//! The M1.08 lifecycle suite (#61): proof that a write has an identity, that a
//! retry replays instead of re-running, and that a vanished client neither
//! cancels the git command nor loses its outcome.
//!
//! These drive [`plan_and_execute_tracked`] — the real lifecycle layer, with
//! the selection injected — against a throwaway repository, so what is under
//! test is the production composition and not a copy of it. The layer below
//! (`plan_and_execute_in`: guard, staleness gate, execution) is the
//! `coordination_suite`'s and `contract_suite`'s subject and is unchanged here.
//!
//! The load-bearing test is [`a_retry_under_the_same_key_runs_git_once`]: it is
//! the one that would catch a regression turning a retry back into a second
//! commit, which is the failure this whole milestone exists to remove.

use super::*;
use std::path::PathBuf;
use std::time::Duration;

use git_vista_core::identity::RepositoryId;
use git_vista_protocol::OperationState;

use crate::operations::{self, Admission};

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

/// A fresh repository on `main` with one commit and a clean working tree.
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

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
    )
}

fn repo_id(repo: &Path) -> RepositoryId {
    git_vista_git::read_repo_facts(repo)
        .expect("a seeded repo classifies")
        .handle
        .repository
}

fn commit_count(repo: &Path, rev: &str) -> usize {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", rev])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn commit(message: &str) -> GitOperation {
    GitOperation::CommitOnHead {
        message: CommitMessage::new(message).unwrap(),
        allow_empty: true,
    }
}

/// The registry is process-global and shared with every other test in this
/// binary, so each test names its own key.
fn key(name: &str) -> IdempotencyKey {
    IdempotencyKey::new(format!("lifecycle-{name}")).unwrap()
}

/// Drive the production lifecycle layer against `repo`.
async fn tracked(
    key: IdempotencyKey,
    repo: &Path,
    op: GitOperation,
) -> (axum::http::StatusCode, String) {
    let id = repo_id(repo);
    plan_and_execute_tracked(key, repo.to_path_buf(), Some(id), tokens(), op).await
}

/// The record admitted under `key`, found the way a second request finds it.
/// Panics if the key was never admitted — every caller has already run one.
fn record_for(key: &IdempotencyKey, op: &GitOperation) -> std::sync::Arc<operations::Record> {
    let hash = operation_hash(op);
    let (repository, worktree) = tokens();
    match operations::admit(key, op, &hash, repository, worktree) {
        Admission::Existing(record) => record,
        Admission::Fresh(..) => panic!("the key should already have been admitted"),
        Admission::Conflict => panic!("the key should name this very operation"),
    }
}

// ---------------------------------------------------------------------------
// Identity and lifecycle
// ---------------------------------------------------------------------------

/// A tracked write gets an id, a recorded terminal state, and the two fields a
/// reconnecting client reconciles with: the post-execution generation and the
/// plan's typed recovery strategy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tracked_operation_records_its_whole_outcome() {
    let (_dir, repo) = seeded_repo();
    let k = key("records-outcome");
    let op = commit("tracked");

    let (status, body) = tracked(k.clone(), &repo, op.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");

    let snapshot = record_for(&k, &op).status();
    assert_eq!(snapshot.state, OperationState::Succeeded);
    assert_eq!(snapshot.stage, OperationStage::Finished);
    assert_eq!(snapshot.status, Some(200));
    assert_eq!(snapshot.message.as_deref(), Some(body.as_str()));
    assert_eq!(snapshot.operation, op);
    assert!(snapshot.ended_at.is_some(), "a terminal record has an end");
    assert!(
        snapshot.generation.is_some(),
        "the post-execution generation is what tells a client its cache is stale"
    );
    assert!(
        snapshot.recovery.is_some(),
        "the plan's recovery strategy must be recorded, so a client can undo"
    );
}

/// A refusal is an outcome, not a lost request: the record is `Failed` and
/// carries the refusal's own status and text, replayable like any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_operation_is_recorded_as_a_failure_with_its_reason() {
    let (_dir, repo) = seeded_repo();
    let k = key("records-refusal");
    // A commit with nothing staged and `allow_empty: false` — git's own refusal.
    let op = GitOperation::CommitOnHead {
        message: CommitMessage::new("nothing to commit").unwrap(),
        allow_empty: false,
    };

    let (status, body) = tracked(k.clone(), &repo, op.clone()).await;
    assert!(!status.is_success(), "expected a refusal, got {status}");

    let snapshot = record_for(&k, &op).status();
    assert_eq!(snapshot.state, OperationState::Failed);
    assert_eq!(snapshot.status, Some(status.as_u16()));
    assert_eq!(snapshot.message.as_deref(), Some(body.as_str()));
}

// ---------------------------------------------------------------------------
// The point of the milestone
// ---------------------------------------------------------------------------

/// **The load-bearing test.** Two requests carrying the same key for the same
/// operation produce exactly ONE commit and two byte-identical responses.
///
/// This is the case the staleness gate could only blunt: before M1.08 the
/// retry was a second intent, and the best the server could do was refuse it
/// with a 409 that didn't answer the user's actual question ("did my commit
/// land?"). Now the retry *is* the first request's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_under_the_same_key_runs_git_once() {
    let (_dir, repo) = seeded_repo();
    let before = commit_count(&repo, "HEAD");
    let k = key("retry-once");
    let op = commit("retried");

    let (first, second) = tokio::join!(
        tracked(k.clone(), &repo, op.clone()),
        tracked(k.clone(), &repo, op.clone()),
    );

    assert_eq!(first, second, "a retry must replay the original response");
    assert_eq!(first.0, axum::http::StatusCode::OK, "{}", first.1);
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before + 1,
        "the same key twice must produce exactly one commit"
    );
}

/// A retry that arrives *after* the first finished replays the recorded result
/// rather than planning a fresh operation — and still runs no git.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_retry_replays_the_recorded_result() {
    let (_dir, repo) = seeded_repo();
    let before = commit_count(&repo, "HEAD");
    let k = key("late-retry");
    let op = commit("replayed later");

    let first = tracked(k.clone(), &repo, op.clone()).await;
    let second = tracked(k.clone(), &repo, op.clone()).await;

    assert_eq!(first, second);
    assert_eq!(commit_count(&repo, "HEAD"), before + 1);
}

/// A key reused for a *different* operation is refused, never answered with a
/// result computed for something else — the invariant that makes an idempotency
/// key safe to trust at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_reused_for_a_different_operation_is_refused() {
    let (_dir, repo) = seeded_repo();
    let k = key("reused-key");

    let (ok, _) = tracked(k.clone(), &repo, commit("the original")).await;
    assert_eq!(ok, axum::http::StatusCode::OK);

    let before = commit_count(&repo, "HEAD");
    let (status, body) = tracked(k.clone(), &repo, commit("something else")).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{body}");
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before,
        "a conflicting key must not run git"
    );
}

/// **The disconnect.** Dropping the request future — what axum does when the
/// client's connection dies — must not cancel the git command, and the outcome
/// must still be recoverable afterwards.
///
/// Before M1.08 the pipeline ran *inside* the request future, so this dropped
/// the operation mid-flight. Now the future only waits; the work is detached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disconnected_client_neither_cancels_nor_loses_the_operation() {
    let (_dir, repo) = seeded_repo();
    let before = commit_count(&repo, "HEAD");
    let k = key("disconnect");
    let op = commit("survives the disconnect");

    // Start the operation, then abandon the request the way a dead tunnel does.
    {
        let repo = repo.clone();
        let (k, op) = (k.clone(), op.clone());
        let request = tokio::spawn(async move { tracked(k, &repo, op).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        request.abort();
    }

    // The operation was admitted before the abort — otherwise this test would
    // pass trivially by never having started anything — and it reaches a
    // terminal state with nobody waiting on it.
    let record = record_for(&k, &op);
    let (status, _) = record.wait_terminal().await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "the detached pipeline must finish even though its request is gone"
    );
    assert_eq!(record.status().state, OperationState::Succeeded);

    // And the client that comes back with the same key gets that outcome —
    // the real recovery path — with git having run exactly once.
    let (status, body) = tracked(k, &repo, op).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(
        commit_count(&repo, "HEAD"),
        before + 1,
        "the abandoned request's commit must have landed exactly once"
    );
}

/// The record is fetchable by the id the server minted, which is the whole
/// point of handing the id back in a response header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_record_is_fetchable_by_its_minted_id() {
    let (_dir, repo) = seeded_repo();
    let k = key("fetchable");
    let op = commit("fetch me");

    tracked(k.clone(), &repo, op.clone()).await;

    let id = record_for(&k, &op).id();
    let found = operations::lookup(&id).expect("the record must be fetchable by id");
    assert_eq!(found.status().id, id);
    assert!(found.status().is_terminal());
}
