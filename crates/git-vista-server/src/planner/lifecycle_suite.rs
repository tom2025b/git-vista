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
use std::time::Duration;

use git_vista_core::identity::RepositoryId;
use git_vista_protocol::{OperationId, OperationState, OperationStatus};

use crate::operations::{self, Admission};
use git_vista_fixtures::seeded as seeded_repo;

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
    plan_and_execute_tracked(
        key,
        repo.to_path_buf(),
        Some(id),
        tokens(),
        PlanSource::Build(op),
        None,
        crate::planner::DropProof::Nothing,
    )
    .await
}

/// The record admitted under `key`, found the way a second request finds it.
/// Panics if the key was never admitted — every caller has already run one.
fn record_for(key: &IdempotencyKey, op: &GitOperation) -> std::sync::Arc<operations::Record> {
    let hash = operation_hash(op);
    let (repository, worktree) = tokens();
    match operations::admit(key, op, &hash, repository, worktree, None) {
        Admission::Existing(record) => record,
        Admission::Fresh(..) => panic!("the key should already have been admitted"),
        Admission::Conflict => panic!("the key should name this very operation"),
        Admission::IncompatibleKey { .. } => {
            panic!("no test key can name an incompatible journal row")
        }
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

// ---------------------------------------------------------------------------
// M1.09 — the durable journal, end to end across the module boundary
// ---------------------------------------------------------------------------

/// A tracked write is durable by the time its own request has its answer: the
/// journal row exists and matches the in-memory record, no polling or delay
/// needed. Exercises [`crate::durable::recover`] directly, rather than through
/// its own unit tests against a scratch connection, so this pins the
/// cross-module contract [`crate::operations`] relies on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_operation_is_durable_by_the_time_the_request_returns() {
    let (_dir, repo) = seeded_repo();
    let k = key("durable-by-return");
    let op = commit("journaled");

    let (status, _) = tracked(k.clone(), &repo, op.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let id = record_for(&k, &op).id();

    let journaled = crate::durable::recover().await;
    let row = journaled
        .records
        .iter()
        .find(|(_, s)| s.id == id)
        .expect("the finished operation must already be in the journal");
    assert_eq!(row.0, k);
    assert_eq!(row.1.state, OperationState::Succeeded);
    assert_eq!(row.1.operation, op);
}

/// **The crash-recovery integration path.** A row the durable layer would see
/// as `Running` — the shape a killed process leaves behind — is closed out as
/// `Failed` by [`crate::durable::recover`], and rehydrating it makes it
/// fetchable again through the ordinary registry lookup, exactly as if the
/// server had never restarted.
///
/// Runs against [`crate::durable::open_private`], not the shared journal
/// (issue #158): this test's whole point is to seed a fake orphaned row and
/// prove `recover`'s close-out sweep, but that sweep has no way to tell a
/// genuine orphan from another concurrently-running test's operation that is
/// simply still executing — every test in this binary shares one journal.
/// Calling the real, shared-journal `recover` here would risk marking some
/// other, real, in-flight test's row `Failed` too, which is exactly what made
/// `a_finished_operation_is_durable_by_the_time_the_request_returns` flaky. A
/// private connection makes that collision structurally impossible without
/// weakening what's under test — the assertions below only look at the row
/// this test itself seeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_left_running_recovers_as_failed_and_is_rehydrated_into_the_registry() {
    let (_dir, repo) = seeded_repo();
    let conn = crate::durable::open_private();
    let k = key("crash-recovery");
    let id = OperationId::new("crash-recovery-op").unwrap();
    let hash = operation_hash(&commit("never finished"));

    let mid_flight = OperationStatus {
        id: id.clone(),
        state: OperationState::Running,
        stage: OperationStage::Executing,
        operation: commit("never finished"),
        operation_hash: hash,
        repository: tokens().0,
        worktree: tokens().1,
        accepted_at: UnixSeconds(1),
        ended_at: None,
        status: None,
        message: None,
        generation: None,
        recovery: None,
        // M3.25 (#78): this row is an ordinary write, not the recovery of
        // another operation.
        recovers: None,
        // M2.20c (#229): never persisted, so a rehydrated row always reads
        // `None` here regardless of what the crashed process was reporting.
        progress: None,
    };
    crate::durable::persist_to(conn, k.clone(), mid_flight).await;

    let recovered = crate::durable::recover_from(conn).await;
    let (recovered_key, recovered_status) = recovered
        .records
        .into_iter()
        .find(|(_, s)| s.id == id)
        .expect("the seeded row must come back from recover()");
    assert_eq!(recovered_key, k);
    assert_eq!(recovered_status.state, OperationState::Failed);
    assert!(recovered_status
        .message
        .as_deref()
        .unwrap_or_default()
        .contains("restarted"));

    operations::rehydrate(vec![(recovered_key, recovered_status)], Vec::new());
    let found = operations::lookup(&id).expect("rehydrate must make it fetchable again");
    assert_eq!(found.status().state, OperationState::Failed);
    assert!(found.status().is_terminal());

    let _ = repo; // seeded but unused directly — the row is entirely synthetic
}

// ---------------------------------------------------------------------------
// M2.21d — the recovery pin's ordering against the command it protects
// ---------------------------------------------------------------------------

/// Whether the loose ref file for `name` exists. Two `stat` calls and no
/// `.await`, on purpose: the observer below has to sample the repository at an
/// instant, and any await point would let the thing it is looking for happen
/// while it was suspended.
fn loose_ref_exists(repo: &Path, name: &str) -> bool {
    repo.join(".git").join(name).exists()
}

/// Whether *any* recovery pin has been written yet. The observer cannot know
/// the operation's minted id (it is minted inside `plan_and_execute_tracked`
/// and the observer is racing that very call), and it does not need to: this
/// repository is a throwaway with exactly one operation run against it.
fn any_recovery_pin(repo: &Path) -> bool {
    std::fs::read_dir(repo.join(".git/refs/git-vista/recovery"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn object_exists(repo: &Path, oid: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", oid])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

/// **The gc race the recovery pin exists to lose, run against the production
/// lifecycle path** (#238, ADR 0048).
///
/// `DeleteLocalTag` is ranked `Destructive` rather than `Irreversible` on one
/// claim: `refs/git-vista/recovery/<id>` keeps the deleted annotated tag's
/// now-dangling tag object — and, when no branch reaches it, the commit under
/// it — alive through `git gc`, so `update-ref` at the plan's recovery oid puts
/// the tag back byte-identically. That claim is only true if the pin exists
/// **before** `git tag -d` removes the last other reference to the object.
///
/// Until this test, it did not. The pin was written by
/// `plan_and_execute_tracked` after `plan_and_execute_in` returned — after
/// `execute` ran the delete, and after the per-repository mutation guard
/// dropped — with a live-generation observation and a sqlite write in between.
/// Any other git process touching this repository in that gap (this server's
/// own next queued mutation, a read endpoint, a terminal, anything that honours
/// `gc.auto`, which nothing here disables) could prune both objects
/// permanently, leaving the journal's `recovery` field naming an oid that no
/// longer exists — a recovery record that cannot recover.
///
/// The observer models that process. It polls for the exact instant
/// `refs/tags/v1.0.0` disappears (loose-file `stat`, sub-millisecond, no await
/// between the two samples) and records whether a pin existed at that instant;
/// then it prunes, the way `gc --auto` would. Two assertions follow, and they
/// fail for different reasons: the sample says the pin was late, the prune says
/// what being late costs.
///
/// Three anti-vacuity guards, because each of them would otherwise let this
/// pass while the mechanism was broken:
///
///  * The tagged commit is reachable from **nothing else** — no branch, and the
///    reflogs are expired before pruning. With it on a branch, "the commit
///    survived" would be true whatever this code did.
///  * The repository is asserted to have **no** recovery pin before the
///    operation starts, so "a pin existed when the tag vanished" cannot be
///    satisfied by leftover state.
///  * The observer must actually **witness the transition**. A delete that
///    never happened (a refusal, a wrong fixture) would otherwise leave every
///    assertion below trivially true; instead it times out and says so.
///
/// The paired negative — the same delete with no pin at all, and `git gc`
/// taking both objects — is `contract_suite`'s
/// `a_deleted_tag_is_restorable_byte_identically_and_the_pin_is_what_saves_it`,
/// whose unpinned leg proves these objects are genuinely prunable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recovery_pin_exists_before_the_tag_it_saves_is_deleted() {
    let (_dir, repo) = seeded_repo();
    // A commit no branch reaches: the annotated tag is its only anchor.
    run(&repo, &["checkout", "-q", "--detach"]);
    run(&repo, &["commit", "-q", "--allow-empty", "-m", "released"]);
    let released = git_stdout(&repo, &["rev-parse", "HEAD"]);
    run(
        &repo,
        &["tag", "-a", "-m", "v1.0.0 — release notes", "v1.0.0"],
    );
    run(&repo, &["checkout", "-q", "main"]);
    let tag_object = git_stdout(&repo, &["rev-parse", "refs/tags/v1.0.0"]);
    assert_ne!(
        tag_object, released,
        "an annotated tag's ref value must differ from its commit, or the pin \
         under test is not pinning a tag object at all"
    );
    assert!(
        loose_ref_exists(&repo, "refs/tags/v1.0.0"),
        "the observer below detects the delete by this file's disappearance"
    );
    assert!(
        !any_recovery_pin(&repo),
        "no pin may pre-exist, or ‘a pin was there when the tag vanished’ \
         proves nothing about this operation"
    );

    // The concurrent git process. It is *not* holding the mutation guard, and
    // that is the faithful model: `gc --auto` fires from any git invocation,
    // including this server's read paths, which never take that guard.
    let watched = repo.clone();
    let observer = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            // The sample: two stats, in this order, with nothing between them.
            if !loose_ref_exists(&watched, "refs/tags/v1.0.0") {
                let pinned_at_delete = any_recovery_pin(&watched);
                // Now do what a `gc --auto` would have done in this window.
                run(&watched, &["reflog", "expire", "--expire=now", "--all"]);
                run(&watched, &["gc", "-q", "--prune=now"]);
                return Some(pinned_at_delete);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
    });

    let k = key("tag-pin-ordering");
    let op = GitOperation::DeleteLocalTag {
        name: TagName::new("v1.0.0").unwrap(),
    };
    let (status, body) = tracked(k.clone(), &repo, op.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");

    let pinned_at_delete = observer
        .await
        .expect("the observer task must not panic")
        .expect(
            "the observer never saw refs/tags/v1.0.0 disappear — the delete this \
             test is about did not happen, so nothing below would have meant anything",
        );

    assert!(
        pinned_at_delete,
        "the recovery pin was still unwritten at the instant `git tag -d` removed \
         the tag: for the whole of that window the tag object and the commit under \
         it are unreachable and any concurrent git may prune them"
    );
    assert!(
        object_exists(&repo, &tag_object),
        "a prune racing the delete took the tag object — the pin was not in place \
         in time, so the plan's recovery oid now names nothing"
    );
    assert!(
        object_exists(&repo, &released),
        "and with it the commit the tag spoke for: this is the history loss the \
         Destructive (not Irreversible) rank promises cannot happen"
    );

    // The pin production wrote is the one the journal row names, at the plan's
    // recovery oid — restoring there is what gives back an *annotated* tag.
    let id = record_for(&k, &op).id();
    assert_eq!(
        git_stdout(
            &repo,
            &[
                "rev-parse",
                &format!("refs/git-vista/recovery/{}", id.as_str())
            ]
        ),
        tag_object,
        "the surviving pin must be the operation's own, at the unpeeled tag object"
    );
}
