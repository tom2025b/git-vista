//! M2.20d (#230): the pull slice's *behavioural* tests — the ones that spawn
//! real git against a real remote, because the claims this slice makes cannot
//! be proved any other way.
//!
//! `handlers::pull`'s own tests cover the wire gate (the missing-strategy
//! `400`, with paired positives) and `planner::pull`'s inline tests cover the
//! pure functions. This file covers what only a repository can answer:
//!
//! * **merge and rebase produce genuinely different, correct histories** from
//!   one diverged fixture — asserted on the commit graph (parent counts,
//!   reachability of the pre-pull tip, the rewritten commit's identity), not
//!   on "both returned 200";
//! * **a conflicted pull is aborted and the repository is restored**, with the
//!   response saying so in a typed field, and a paired negative proving the
//!   conflict tag is not simply what every failure gets;
//! * **a cancel between the halves stops the integration**, which is the
//!   promise `honours_cancellation(PullBranch) == true` makes beyond what it
//!   inherits from fetch;
//! * **the journal records one `Pull` entry naming the strategy** — not the
//!   `Merge`/`Rebase` entry the standalone executors write, which would
//!   describe an operation the user never submitted.
//!
//! # The fixture shape, and why it looks odd
//!
//! Every remote here is a bare repository **inside the served repository's own
//! tree**, for the reason [`super::fetch_suite`] documents at length: #66 Task
//! 6 grants the served repository and the system trees and nothing else, so a
//! bare remote in a sibling tempdir is denied outright and every fetch fails
//! for a reason that has nothing to do with what is under test.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};
use git_vista_protocol::{
    BranchName, GitOperation, MergeStrategy, PullError, PullFailureKind, PullSuccess, RemoteName,
    RepositoryToken, WorktreeToken,
};

use super::operation_hash;
use crate::operations::{Admission, Record};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn run(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed in {dir:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `git <args…>`, trimmed stdout, asserting success.
fn out(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {dir:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("pull-suite-repo").unwrap(),
        WorktreeToken::new("pull-suite-worktree").unwrap(),
    )
}

fn pull_op(strategy: MergeStrategy) -> GitOperation {
    GitOperation::PullBranch {
        remote: RemoteName::new("origin").unwrap(),
        branch: BranchName::new("main").unwrap(),
        strategy,
    }
}

/// A repository whose `origin/main` has moved on **and** whose own `main` has
/// a local-only commit: the diverged history that makes merge and rebase
/// visibly different operations.
///
/// The remote-side commits are authored in a scratch clone outside the served
/// tree and pushed from there — the same reason [`super::fetch_suite`] does
/// it: commits authored *in* the served repository and then rewound leave
/// every object already present, git negotiates an almost-empty pack, and the
/// fetch half of the test passes over nothing.
///
/// `local` names the file the local-only commit touches; picking a different
/// name from the remote's is what makes the histories merge cleanly, and
/// picking the *same* name (see [`conflicting_repo`]) is what makes them
/// conflict.
fn diverged_repo(local_file: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@example.invalid"]);
    run(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    run(&repo, &["add", "seed.txt"]);
    run(&repo, &["commit", "-q", "-m", "seed"]);

    let remote = repo.join("upstream.git");
    std::fs::create_dir_all(&remote).unwrap();
    run(&remote, &["init", "-q", "--bare", "-b", "main"]);
    // The bare remote sits inside the served tree (see the module docs), so
    // git would otherwise report it as an untracked directory and every
    // "the working tree is clean" assertion below would be measuring the
    // fixture instead of the pull. `.git/info/exclude` rather than a
    // committed `.gitignore`, so no commit in these fixtures exists for the
    // sake of the harness.
    std::fs::write(repo.join(".git/info/exclude"), "upstream.git/\n").unwrap();
    run(
        &repo,
        &["remote", "add", "origin", &remote.display().to_string()],
    );
    run(&repo, &["push", "-q", "origin", "main"]);
    run(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);

    // Remote-side history, authored elsewhere.
    let authoring = dir.path().join("authoring");
    run(
        dir.path(),
        &[
            "clone",
            "-q",
            &remote.display().to_string(),
            &authoring.display().to_string(),
        ],
    );
    run(&authoring, &["config", "user.email", "u@example.invalid"]);
    run(&authoring, &["config", "user.name", "u"]);
    std::fs::write(authoring.join("shared.txt"), "from upstream\n").unwrap();
    run(&authoring, &["add", "shared.txt"]);
    run(&authoring, &["commit", "-q", "-m", "upstream work"]);
    run(&authoring, &["push", "-q", "origin", "main"]);

    // Local-only history.
    std::fs::write(repo.join(local_file), "from me\n").unwrap();
    run(&repo, &["add", local_file]);
    run(&repo, &["commit", "-q", "-m", "local work"]);

    (dir, repo)
}

/// The same divergence, except both sides edited the *same* file at the same
/// place — so the integration cannot be automatic, whichever strategy runs.
fn conflicting_repo() -> (tempfile::TempDir, PathBuf) {
    let (dir, repo) = diverged_repo("mine.txt");
    // Rewrite the local-only commit so it touches `shared.txt` with different
    // content than upstream's.
    run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.join("shared.txt"), "from me, incompatibly\n").unwrap();
    run(&repo, &["add", "shared.txt"]);
    run(&repo, &["commit", "-q", "-m", "local work (conflicting)"]);
    (dir, repo)
}

/// Admit one pull operation into the registry, exactly as
/// `plan_and_execute_tracked` would.
fn admit_pull(
    name: &str,
    strategy: MergeStrategy,
) -> (crate::operations::OperationHandle, std::sync::Arc<Record>) {
    let key = git_vista_protocol::IdempotencyKey::new(format!("pull-suite-{name}")).unwrap();
    let op = pull_op(strategy);
    let hash = operation_hash(&op);
    let (repository, worktree) = tokens();
    match crate::operations::admit(&key, &op, &hash, repository, worktree, None) {
        Admission::Fresh(handle, record) => (handle, record),
        _ => panic!("a fresh key must be admitted"),
    }
}

/// Run the guarded pipeline under `record`, so the executor sees the
/// operation's progress sink and cancellation latch — the scope
/// `plan_and_execute_tracked`'s detached task establishes in production.
async fn run_tracked(
    repo: &Path,
    record: std::sync::Arc<Record>,
    op: GitOperation,
) -> (StatusCode, String) {
    let repo = repo.to_path_buf();
    crate::operations::with_progress(record, async move {
        super::plan_and_execute_in(&repo, None, tokens(), op).await
    })
    .await
}

/// The pipeline with no operation record around it — for the tests that do not
/// touch progress or cancellation.
async fn pipeline(repo: &Path, op: GitOperation) -> (StatusCode, String) {
    super::plan_and_execute_in(repo, None, tokens(), op).await
}

/// The pipeline's future stays small enough to poll on an ordinary 2 MiB
/// thread stack.
///
/// **This test exists because #230 crashed on it.** Adding one `.await` frame
/// to the fetch path — `exec_fetch` awaiting `run_fetch`, so that
/// `planner::pull` could reuse the fetch step instead of writing a second one
/// — turned a passing suite into `fatal runtime error: stack overflow` in
/// *fetch*'s own tests. The cause was measurable and had been latent since
/// #229: `git_cmd::git_streamed_for`'s future is ~66 KiB, and every caller
/// that awaits it inline carries a copy in its own frame, so the whole
/// `plan_and_execute_in` state machine was **68,104 bytes**. One `Box::pin`
/// in `planner::fetch` took it to under 4 KiB.
///
/// A size assertion rather than a comment, because the failure mode is
/// invisible until it is a SIGABRT in an unrelated test: nothing about
/// `.await`ing a large future looks wrong at the call site, and the cost lands
/// on whoever is unlucky enough to be deepest on the stack. 16 KiB is a
/// deliberately loose ceiling — four times today's value, so ordinary growth
/// never trips it, while the 68 KiB regression this caught could not slip
/// through.
#[test]
fn the_planner_pipelines_future_stays_small_enough_for_an_ordinary_stack() {
    /// Four times the measured size at the time of writing (~3.8 KiB).
    const BUDGET: usize = 16 * 1024;

    let repo = std::path::PathBuf::from("/nonexistent");
    // Never polled — `size_of_val` is a compile-time property of the future's
    // type, so constructing one is enough and the path is never touched.
    for (name, size) in [
        (
            "plan_and_execute_in (pull)",
            std::mem::size_of_val(&super::plan_and_execute_in(
                &repo,
                None,
                tokens(),
                pull_op(MergeStrategy::Merge),
            )),
        ),
        (
            "exec_fetch",
            std::mem::size_of_val(&super::fetch::exec_fetch(
                &repo,
                crate::sandbox::NetworkNeed::Remote,
                &RemoteName::new("origin").unwrap(),
            )),
        ),
        (
            "run_fetch",
            std::mem::size_of_val(&super::fetch::run_fetch(
                &repo,
                crate::sandbox::NetworkNeed::Remote,
                &RemoteName::new("origin").unwrap(),
                "/api/pull",
            )),
        ),
    ] {
        assert!(
            size <= BUDGET,
            "{name}'s future is {size} bytes, over the {BUDGET}-byte budget. \
             Something now awaits a very large future inline — most likely \
             `git_cmd::git_streamed_for` (~66 KiB), whose `Box::pin` in \
             planner::fetch is what keeps this whole pipeline pollable on a \
             2 MiB stack. Box the new await rather than raising this number."
        );
    }
}

/// How many parents a commit has.
fn parents(repo: &Path, rev: &str) -> usize {
    out(repo, &["rev-list", "--parents", "-n", "1", rev])
        .split_whitespace()
        .count()
        - 1
}

/// Whether `ancestor` is reachable from `descendant`.
///
/// **Both commits must exist in `repo` first, and that is checked**, because
/// `git merge-base --is-ancestor` exits non-zero for "no" *and* for "I have
/// never heard of that object" — so a caller asking about a commit this
/// repository does not have gets a confident `false` that means nothing. That
/// is not hypothetical: the first draft of
/// `merge_and_rebase_pulls_of_one_diverged_history_produce_different_histories`
/// asserted the fixture was diverged by asking whether the *upstream* tip was
/// reachable before the fetch had brought it over, and passed on git's
/// `fatal: Not a valid commit name`.
fn reachable(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    for rev in [ancestor, descendant] {
        assert!(
            std::process::Command::new("git")
                .args(["cat-file", "-e", &format!("{rev}^{{commit}}")])
                .current_dir(repo)
                .status()
                .unwrap()
                .success(),
            "{repo:?} has no commit {rev}, so `is {ancestor} an ancestor of \
             {descendant}?` cannot be answered — git would say `no` for a \
             reason that is not an answer"
        );
    }
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

// ---------------------------------------------------------------------------
// The issue's headline: merge and rebase are not the same operation
// ---------------------------------------------------------------------------

/// **The golden test the issue asks for by name**: a `merge`-strategy pull and
/// a `rebase`-strategy pull of the *same* diverged history produce different,
/// individually correct histories — not merely "both succeed".
///
/// Four properties per strategy, and each one is what distinguishes it from
/// the other:
///
/// | | merge | rebase |
/// |---|---|---|
/// | new tip's parents | 2 (a merge commit) | 1 (linear) |
/// | pre-pull tip reachable from the new tip | yes — it is a parent | **no** — it was rewritten |
/// | upstream tip reachable | yes | yes |
/// | the local commit's own id | unchanged | **changed** |
///
/// The last row is the one that makes this test hard to fake: a rebase that
/// silently fast-forwarded, or a merge arm that ran for both inputs, would
/// satisfy "both moved HEAD" and even "both contain everything", but cannot
/// satisfy both the parent count *and* the identity change.
///
/// Both runs start from a fresh, identical fixture, so the only independent
/// variable is `strategy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_and_rebase_pulls_of_one_diverged_history_produce_different_histories() {
    // --- merge ------------------------------------------------------------
    let (_m_dir, merged) = diverged_repo("mine.txt");
    let local_before = out(&merged, &["rev-parse", "HEAD"]);
    let upstream_tip = out(&merged.join("upstream.git"), &["rev-parse", "main"]);
    // The fixture really is diverged: both sides sit one commit on top of the
    // *same* seed, and those two commits are different. Asserted from objects
    // each repository actually holds — the served repository has not fetched
    // yet, so asking it about `upstream_tip` at this point would be asking
    // about an object it has never seen (see `reachable`'s doc).
    assert_ne!(
        local_before, upstream_tip,
        "the two sides must have different tips"
    );
    assert_eq!(
        out(&merged, &["rev-parse", "HEAD~1"]),
        out(&merged.join("upstream.git"), &["rev-parse", "main~1"]),
        "…and must share the seed as their common parent, or this is not a \
         divergence at all"
    );

    let (status, body) = pipeline(&merged, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let merge_tip = out(&merged, &["rev-parse", "HEAD"]);

    assert_eq!(
        parents(&merged, &merge_tip),
        2,
        "a merge-strategy pull of a diverged history must land a merge commit"
    );
    assert!(
        reachable(&merged, &local_before, &merge_tip),
        "the pre-pull tip must survive as a parent of the merge"
    );
    assert!(
        reachable(&merged, &upstream_tip, &merge_tip),
        "the upstream tip must be reachable after the merge"
    );
    assert_eq!(
        out(&merged, &["rev-list", "--count", "HEAD"]),
        "4",
        "seed + upstream + local + merge"
    );

    // --- rebase -----------------------------------------------------------
    let (_r_dir, rebased) = diverged_repo("mine.txt");
    let r_local_before = out(&rebased, &["rev-parse", "HEAD"]);
    let r_upstream_tip = out(&rebased.join("upstream.git"), &["rev-parse", "main"]);

    let (status, body) = pipeline(&rebased, pull_op(MergeStrategy::Rebase)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rebase_tip = out(&rebased, &["rev-parse", "HEAD"]);

    assert_eq!(
        parents(&rebased, &rebase_tip),
        1,
        "a rebase-strategy pull must leave linear history — a merge commit \
         here means the strategy was ignored"
    );
    assert!(
        !reachable(&rebased, &r_local_before, &rebase_tip),
        "a rebase rewrites the local commit, so the pre-pull tip must NOT be \
         reachable from the new one. If it is, this ran a merge (or a \
         fast-forward) under a rebase label."
    );
    assert!(
        reachable(&rebased, &r_upstream_tip, &rebase_tip),
        "the upstream tip must be reachable after the rebase"
    );
    assert_ne!(
        rebase_tip, r_local_before,
        "the replayed commit must have a new identity"
    );
    assert_eq!(
        out(&rebased, &["rev-list", "--count", "HEAD"]),
        "3",
        "seed + upstream + replayed local — a rebase adds no commit"
    );

    // --- and the two outcomes are not the same one ------------------------
    assert_ne!(
        parents(&merged, &merge_tip),
        parents(&rebased, &rebase_tip),
        "the two strategies must have produced observably different shapes"
    );
    // Both kept every commit's *content*, which is what makes each correct
    // rather than merely different.
    for (repo, file) in [(&merged, "shared.txt"), (&rebased, "shared.txt")] {
        assert_eq!(
            std::fs::read_to_string(repo.join(file)).unwrap(),
            "from upstream\n",
            "upstream's content must be in the working tree either way"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("mine.txt")).unwrap(),
            "from me\n",
            "the local commit's content must survive either way"
        );
    }

    // Each response echoed the strategy that actually ran — a body that echoed
    // the other one would be a bug this field exists to make visible.
    for (repo, strategy) in [
        (&merged, MergeStrategy::Merge),
        (&rebased, MergeStrategy::Rebase),
    ] {
        let (status, body) = pipeline(repo, pull_op(strategy)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let parsed: PullSuccess = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.strategy, strategy);
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.remote, "origin");
    }
}

/// An up-to-date pull is a success that says nothing moved — and says it from
/// an observation, not from a constant.
///
/// This is the paired negative for `advanced`: without it, an executor that
/// always reported `advanced: true` would pass every test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_with_nothing_to_integrate_reports_that_nothing_moved() {
    let (_dir, repo) = diverged_repo("mine.txt");
    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first: PullSuccess = serde_json::from_str(&body).unwrap();
    assert!(first.advanced, "the first pull had something to do: {body}");
    let tip_after_first = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second: PullSuccess = serde_json::from_str(&body).unwrap();
    assert!(
        !second.advanced,
        "a second pull of the same remote has nothing to integrate: {body}"
    );
    assert!(
        second.updated_refs.is_empty(),
        "…and nothing to fetch either: {body}"
    );
    assert_eq!(
        out(&repo, &["rev-parse", "HEAD"]),
        tip_after_first,
        "the repository must agree that nothing moved"
    );
}

/// **`advanced` is read off the branch tip, not off the fetch's ref list —
/// direction one: the fetch moved nothing and the branch moved anyway.**
///
/// Every other test in this file runs a fixture where "the fetch had something
/// to bring" and "the checked-out branch had something to integrate" are the
/// same fact, so all of them pass equally for
/// `advanced = !head_after.same_observation(&head_before)` (what the code says)
/// and for `advanced = !updated_refs.is_empty()` (a conflation of the two
/// halves). Verified by mutation before this test existed: the whole 12-test
/// suite plus the contract suite's pull case stayed green with the conflated
/// version in place.
///
/// Here the objects are fetched **first**, by the harness, so the pull's own
/// fetch half has nothing left to move — and the integration still advances the
/// branch by two commits' worth of history. `updated_refs` is empty and
/// `advanced` must be `true`, which the conflated version cannot produce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_that_had_nothing_left_to_fetch_still_reports_the_branch_advanced() {
    let (_dir, repo) = diverged_repo("mine.txt");
    // The harness does the fetch, outside the server, so the pull's fetch half
    // is a genuine no-op rather than a mocked one.
    run(&repo, &["fetch", "-q", "origin"]);
    assert_eq!(
        out(&repo, &["rev-parse", "refs/remotes/origin/main"]),
        out(&repo.join("upstream.git"), &["rev-parse", "main"]),
        "the pre-fetch must have left nothing for the pull's fetch half to do"
    );
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parsed: PullSuccess = serde_json::from_str(&body).unwrap();

    assert!(
        parsed.updated_refs.is_empty(),
        "the objects were already local, so the fetch half moved no \
         remote-tracking ref: {body}"
    );
    // The referee: git says the branch moved.
    assert_ne!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "the integration must really have advanced the branch, or this test \
         is asserting nothing"
    );
    assert!(
        parsed.advanced,
        "`advanced` must come from the checked-out branch's tip before and \
         after, not from whether the fetch half moved a ref — here the fetch \
         moved nothing and the branch moved: {body}"
    );
    assert!(
        parsed.message.contains("Pulled"),
        "…and the message must agree with the field: {}",
        parsed.message
    );
}

/// **Direction two: the fetch moved a ref and the branch did not.**
///
/// The mirror of the test above, and the leg that catches the conflation the
/// other way round. The remote gains a commit on a branch nobody is pulling,
/// so the pull's fetch half really does update `refs/remotes/origin/other` —
/// while `origin/main`, the ref the integration runs against, is already
/// merged. `updated_refs` is non-empty and `advanced` must be `false`.
///
/// Together the two tests make `advanced` and `!updated_refs.is_empty()`
/// independently observable: no implementation can satisfy both by deriving
/// one from the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_whose_fetch_moved_an_unrelated_ref_does_not_claim_the_branch_advanced() {
    let (dir, repo) = diverged_repo("mine.txt");
    // Get `main` fully integrated first, so the second pull's integration has
    // nothing to do.
    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let settled = out(&repo, &["rev-parse", "HEAD"]);

    // A commit on a branch this pull does not name.
    let authoring = dir.path().join("authoring");
    run(&authoring, &["checkout", "-q", "-b", "other"]);
    std::fs::write(authoring.join("elsewhere.txt"), "not yours\n").unwrap();
    run(&authoring, &["add", "elsewhere.txt"]);
    run(&authoring, &["commit", "-q", "-m", "unrelated work"]);
    run(&authoring, &["push", "-q", "origin", "other"]);

    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parsed: PullSuccess = serde_json::from_str(&body).unwrap();

    assert!(
        parsed
            .updated_refs
            .iter()
            .any(|u| u.ref_name.ends_with("origin/other")),
        "the fetch half must have brought the unrelated branch over, or this \
         test's premise is gone: {body}"
    );
    // The referee: git says the branch did not move.
    assert_eq!(
        out(&repo, &["rev-parse", "HEAD"]),
        settled,
        "`origin/main` was already integrated, so nothing may have moved the \
         checked-out branch"
    );
    assert!(
        !parsed.advanced,
        "`advanced` describes the checked-out branch, not the fetch: a ref \
         moved for a branch nobody pulled, and the pulled branch stood still: \
         {body}"
    );
    assert!(
        parsed.message.contains("Already up to date"),
        "…and the message must agree with the field: {}",
        parsed.message
    );
}

// ---------------------------------------------------------------------------
// Conflict: an outcome, aborted, observed
// ---------------------------------------------------------------------------

/// A conflicted pull, either strategy: `409` with a typed `Conflict`, the
/// integration aborted, and the repository **observably** back at its pre-pull
/// tip with nothing unmerged.
///
/// The repository is the referee for every claim: the tip is compared against
/// the one recorded before the pull, `git ls-files --unmerged` is listed
/// directly, and `git status --porcelain` is checked for conflict markers.
/// Asserting only `worktree_restored == true` would prove nothing — that is
/// the field under test.
///
/// The fetched objects staying put is asserted too, because it is what makes
/// the advice in the message true: retrying with the other strategy must not
/// need the network again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicted_pull_is_aborted_and_the_repository_is_restored() {
    for strategy in [MergeStrategy::Merge, MergeStrategy::Rebase] {
        let (_dir, repo) = conflicting_repo();
        let before = out(&repo, &["rev-parse", "HEAD"]);
        let before_tree = out(&repo, &["status", "--porcelain"]);
        assert_eq!(before_tree, "", "the fixture must start clean");

        let (status, body) = pipeline(&repo, pull_op(strategy)).await;

        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{strategy:?}: a conflict is a 409, never a 500 — the server did \
             not break, the histories disagree: {body}"
        );
        let error: PullError =
            serde_json::from_str(&body).expect("every /api/pull failure is a PullError");
        assert_eq!(
            error.kind,
            PullFailureKind::Conflict,
            "{strategy:?}: {body}"
        );
        assert!(error.worktree_restored, "{strategy:?}: {body}");
        assert!(
            error.message.contains("aborted"),
            "{strategy:?}: the terminal record must say plainly that it was \
             aborted: {}",
            error.message
        );

        // The referee.
        assert_eq!(
            out(&repo, &["rev-parse", "HEAD"]),
            before,
            "{strategy:?}: the checked-out branch must be back at its pre-pull tip"
        );
        assert_eq!(
            out(&repo, &["ls-files", "--unmerged"]),
            "",
            "{strategy:?}: nothing may be left unmerged after the abort"
        );
        assert_eq!(
            out(&repo, &["status", "--porcelain"]),
            "",
            "{strategy:?}: the working tree must be clean again"
        );

        // The fetch half's work survives — that is what makes "retry with the
        // other strategy" free.
        assert_eq!(
            out(&repo, &["rev-parse", "refs/remotes/origin/main"]),
            out(&repo.join("upstream.git"), &["rev-parse", "main"]),
            "{strategy:?}: the fetched tracking ref must survive the aborted \
             integration"
        );
        assert!(
            !error.updated_refs.is_empty(),
            "{strategy:?}: the response must report what the fetch half landed: {body}"
        );
    }
}

/// **The paired negative for the conflict tag**: an integration that fails for
/// a reason that is *not* a conflict is not reported as one.
///
/// Without this, `looks_like_conflict` could return `true` unconditionally —
/// or the classifier could be absent entirely, with every failed integration
/// tagged `Conflict` — and the test above would still pass. The fixture is a
/// dirty working tree whose uncommitted edit the merge would overwrite: git
/// refuses before touching anything, with words that carry no conflict marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_integration_that_fails_for_another_reason_is_not_tagged_a_conflict() {
    let (_dir, repo) = diverged_repo("mine.txt");
    // An uncommitted edit to the file upstream also changed: `git merge`
    // refuses outright ("Your local changes … would be overwritten").
    std::fs::write(repo.join("shared.txt"), "uncommitted local edit\n").unwrap();
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error: PullError = serde_json::from_str(&body).unwrap();
    assert_eq!(
        error.kind,
        PullFailureKind::Other,
        "git refused before merging anything, with no conflict in sight — \
         tagging this `Conflict` would send the user looking for conflict \
         markers that do not exist: {body}"
    );
    assert!(
        error.worktree_restored,
        "nothing was integrated, so the repository is where it was: {body}"
    );
    assert_eq!(out(&repo, &["rev-parse", "HEAD"]), before);
    assert_eq!(
        std::fs::read_to_string(repo.join("shared.txt")).unwrap(),
        "uncommitted local edit\n",
        "the uncommitted work git refused to overwrite must still be there"
    );
}

/// A pull naming a branch the remote does not have is refused **before** any
/// integration runs, from an observation of the ref rather than from git's
/// error prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_of_a_branch_the_remote_lacks_is_refused_without_integrating() {
    let (_dir, repo) = diverged_repo("mine.txt");
    let before = out(&repo, &["rev-parse", "HEAD"]);

    let (status, body) = pipeline(
        &repo,
        GitOperation::PullBranch {
            remote: RemoteName::new("origin").unwrap(),
            branch: BranchName::new("no-such-branch").unwrap(),
            strategy: MergeStrategy::Merge,
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error: PullError = serde_json::from_str(&body).unwrap();
    assert_eq!(error.kind, PullFailureKind::NoSuchRemoteBranch, "{body}");
    assert!(error.worktree_restored, "{body}");
    assert_eq!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "nothing may be integrated when there is nothing to integrate from"
    );
    // The fetch half still ran and still landed what the remote *does* have —
    // this refusal is about the branch asked for, not about the fetch.
    assert!(
        !error.updated_refs.is_empty(),
        "the fetch half ran and must report what it moved: {body}"
    );
}

// ---------------------------------------------------------------------------
// Cancellation between the halves
// ---------------------------------------------------------------------------

/// A cancel that lands before execution stops the pull entirely — and, the
/// part that is pull's own promise rather than fetch's, **the integration
/// never runs**.
///
/// The repository is checked directly: the checked-out branch is where it was,
/// and no merge commit exists. A `409` alone would not distinguish "we
/// stopped" from "we integrated and then said we stopped".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_pull_does_not_integrate() {
    let (_dir, repo) = diverged_repo("mine.txt");
    let before = out(&repo, &["rev-parse", "HEAD"]);
    let (handle, record) = admit_pull("cancel-early", MergeStrategy::Merge);
    assert!(
        record.request_cancel(),
        "a live record must accept a cancel"
    );

    let (status, body) = run_tracked(&repo, record.clone(), pull_op(MergeStrategy::Merge)).await;
    handle.finish(status, body.clone(), None);

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error: PullError = serde_json::from_str(&body).unwrap();
    assert_eq!(error.kind, PullFailureKind::Cancelled, "{body}");
    assert!(error.worktree_restored, "{body}");

    assert_eq!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "a cancelled pull must not have integrated anything"
    );
    assert_eq!(
        parents(&repo, "HEAD"),
        1,
        "a merge commit here would mean the cancel was ignored by the \
         integration half"
    );
}

/// The paired positive: the *same* fixture, the *same* record plumbing, with
/// no cancel — the pull runs and integrates.
///
/// Without this leg, `a_cancelled_pull_does_not_integrate` would pass for an
/// executor that never integrated at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncancelled_pull_under_the_same_plumbing_does_integrate() {
    let (_dir, repo) = diverged_repo("mine.txt");
    let before = out(&repo, &["rev-parse", "HEAD"]);
    let (handle, record) = admit_pull("cancel-paired-positive", MergeStrategy::Merge);

    let (status, body) = run_tracked(&repo, record, pull_op(MergeStrategy::Merge)).await;
    handle.finish(status, body.clone(), None);

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(
        out(&repo, &["rev-parse", "HEAD"]),
        before,
        "the uncancelled pull must actually integrate"
    );
    assert_eq!(parents(&repo, "HEAD"), 2, "…as a merge, in this case");
}

// ---------------------------------------------------------------------------
// The sandbox tier the two halves run in
// ---------------------------------------------------------------------------

/// The network namespace this test process is in, as
/// `readlink /proc/self/ns/net` reports it (`net:[<inode>]`).
///
/// The probe both hook tests below compare against. It is a *host* fact, so a
/// spawn that reports this string ran with the host's network — no bwrap, no
/// `--unshare-net` — and one that reports anything else did not.
fn host_netns() -> String {
    std::fs::read_link("/proc/self/ns/net")
        .expect("Linux: /proc/self/ns/net")
        .to_string_lossy()
        .into_owned()
}

/// Install an executable hook that records its own network namespace, one line
/// per invocation, in `<repo>/.git/<sink>`.
///
/// Inside `.git` rather than the working tree so no `git status` in this file
/// starts measuring the probe. `cat >/dev/null` drains the stdin git hands a
/// `reference-transaction` hook — a hook that leaves it unread can make git
/// see a write error on a large transaction — and the explicit `exit 0` keeps
/// a `reference-transaction` hook from aborting the ref update it is watching.
fn install_netns_hook(repo: &Path, hook: &str, sink: &str) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join(hook);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat >/dev/null 2>&1\nreadlink /proc/self/ns/net >> \"$(git rev-parse --git-dir)/{sink}\"\nexit 0\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Every line a [`install_netns_hook`] sink collected.
fn netns_lines(repo: &Path, sink: &str) -> Vec<String> {
    match std::fs::read_to_string(repo.join(".git").join(sink)) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// **A pull's integration half runs in the same sandbox tier as a direct
/// merge** — so a repository hook cannot reach the network through
/// `POST /api/pull` when the byte-identical command through `POST /api/merge`
/// denies it.
///
/// # Why this is a real hole and not a theoretical one
///
/// `need` is not a label, it *chooses the tier*: `tier_for(Remote, untrusted)`
/// is `Tier::Network` (no bwrap, `AF_INET` permitted, `DEFAULT_GIT_PORTS`
/// reachable) and `tier_for(Local, untrusted)` is `Tier::Strict` (bwrap
/// `--unshare-net`, `--net-deny`). `policy_for` sets `HookMode::Run` in both.
/// A pull is classified `Remote` because its *fetch* half opens a socket; if
/// that need is threaded into `exec_merge`/`exec_rebase` as well, then a
/// `post-merge` hook in a hostile clone gets outbound TCP for free, on the one
/// operation such a clone is most likely to be the target of. Nothing else in
/// the suite would notice: the pull would still fetch, still integrate, still
/// journal.
///
/// # What is asserted, and why it cannot pass vacuously
///
/// The probe is `readlink /proc/self/ns/net`, run from real git hooks:
///
/// * the `post-merge` hook fires inside the **integration** half. Its
///   namespace must **differ** from this process's — i.e. bwrap unshared it,
///   i.e. `Tier::Strict`.
/// * the `reference-transaction` hook fires during the **fetch** half too, as
///   `refs/remotes/origin/main` is written. At least one of its lines must
///   **equal** this process's namespace — the fetch genuinely is `Remote` and
///   genuinely does run in `Tier::Network`.
///
/// That second leg is what makes the first non-vacuous. Without it, a probe
/// that could never report the host namespace (a test runner already inside a
/// netns, a hook that silently never ran) would satisfy the first assertion for
/// a reason that has nothing to do with the tier. With it, the same probe is
/// shown reporting both answers within one pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_in_a_pulls_integration_half_runs_without_the_network_the_fetch_half_has() {
    let (_dir, repo) = diverged_repo("mine.txt");
    install_netns_hook(&repo, "post-merge", "gv_netns_post_merge");
    install_netns_hook(&repo, "reference-transaction", "gv_netns_ref_tx");

    let host = host_netns();
    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The fetch half: Network tier, so its hook sees the host's namespace.
    // This is the leg that proves the probe can say "host".
    let fetch_side = netns_lines(&repo, "gv_netns_ref_tx");
    assert!(
        !fetch_side.is_empty(),
        "the reference-transaction hook never ran, so this test's probe \
         measured nothing. Hooks must be enabled (HookMode::Run) and the \
         fetch must have updated a ref for this fixture to say anything."
    );
    assert!(
        fetch_side.contains(&host),
        "a pull's fetch half is NetworkNeed::Remote and must run in \
         Tier::Network — no bwrap, so its hooks share this process's network \
         namespace. Saw {fetch_side:?}, this process is in {host}. If this \
         leg fails the probe below proves nothing."
    );

    // The integration half: Strict tier, so its hook is in a namespace of its
    // own — bwrap's `--unshare-net`, and with it `--net-deny` and the seccomp
    // filter that refuses `connect` outright.
    let merge_side = netns_lines(&repo, "gv_netns_post_merge");
    assert_eq!(
        merge_side.len(),
        1,
        "the post-merge hook must have run exactly once — a pull of a diverged \
         history really does merge. Saw {merge_side:?}"
    );
    assert_ne!(
        merge_side[0], host,
        "a pull's integration half must run in Tier::Strict, exactly as \
         POST /api/merge does: its hooks must NOT share this process's network \
         namespace. Seeing {host} here means `exec_merge` was spawned under the \
         pull's operation-level NetworkNeed::Remote, which hands every \
         post-merge hook in an untrusted repository outbound TCP on \
         DEFAULT_GIT_PORTS that the same command through POST /api/merge denies."
    );
}

/// The paired reference point: the *same* hook, the *same* probe, driven
/// through `POST /api/merge`'s own operation.
///
/// The test above says the pull's integration half is not in the host's
/// network namespace. This says what it must match instead — the direct merge
/// route, which has declared `NetworkNeed::Local` since long before pull
/// existed. Without this leg "not the host namespace" is only half a claim;
/// with it, the two routes into `exec_merge` are shown to confine a hook the
/// same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_in_a_direct_merge_is_confined_the_same_way() {
    let (_dir, repo) = diverged_repo("mine.txt");
    // Bring the upstream commit over without going through the server, then
    // give it a local name — `MergeBranch` names a local branch.
    run(&repo, &["fetch", "-q", "origin"]);
    run(
        &repo,
        &["branch", "upstream-copy", "refs/remotes/origin/main"],
    );
    install_netns_hook(&repo, "post-merge", "gv_netns_post_merge");

    let host = host_netns();
    let (status, body) = pipeline(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("upstream-copy").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let seen = netns_lines(&repo, "gv_netns_post_merge");
    assert_eq!(
        seen.len(),
        1,
        "the post-merge hook must have run exactly once: {seen:?}"
    );
    assert_ne!(
        seen[0], host,
        "POST /api/merge is NetworkNeed::Local -> Tier::Strict, so its hooks \
         run with an unshared network namespace. If this ever stops being \
         true, the pull test above is comparing against nothing."
    );
}

// ---------------------------------------------------------------------------
// The journal — what the activity feed is told a pull did
// ---------------------------------------------------------------------------

/// Every entry this repository's journal holds, read back through the same
/// parser `/api/activity` uses.
fn journaled(repo: &Path) -> Vec<ActivityEvent> {
    crate::journal::read_all(repo)
}

/// A pull journals **one** `Pull` entry naming the strategy — not the
/// `Merge`/`Rebase` entry the standalone executors write.
///
/// Two claims, and both matter:
///
/// * the entry exists and says which strategy ran, which is the issue's
///   "distinguishing a pull-via-merge from a pull-via-rebase in the recorded
///   message" criterion; and
/// * there is **no** `Merge`/`Rebase` entry beside it. A feed showing
///   `Fetch` + `Merge` for one approved `PullBranch` describes an operation
///   nobody submitted, and its undo hint would offer to undo half of it.
///
/// The oids are checked against `git rev-parse`, not against the response
/// body: asserting the journal matches the response would only prove the two
/// came from the same variable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_journals_one_entry_naming_its_strategy() {
    for (strategy, word, forbidden) in [
        (MergeStrategy::Merge, "merge", ActivityKind::Merge),
        (MergeStrategy::Rebase, "rebase", ActivityKind::Rebase),
    ] {
        let (_dir, repo) = diverged_repo("mine.txt");
        assert!(
            journaled(&repo).is_empty(),
            "the fixture must start with an empty journal, or the counts below \
             measure the fixture"
        );
        let before = out(&repo, &["rev-parse", "HEAD"]);

        let (status, body) = pipeline(&repo, pull_op(strategy)).await;
        assert_eq!(status, StatusCode::OK, "{strategy:?}: {body}");
        let after = out(&repo, &["rev-parse", "HEAD"]);

        let all = journaled(&repo);
        let pulls: Vec<&ActivityEvent> = all
            .iter()
            .filter(|e| e.kind == ActivityKind::Pull)
            .collect();
        assert_eq!(
            pulls.len(),
            1,
            "{strategy:?}: a pull must journal exactly one Pull entry: {all:?}"
        );
        let entry = pulls[0];
        assert!(
            entry.summary.contains(word),
            "{strategy:?}: the summary must name the strategy that ran, so a \
             reader can tell a merge-pull from a rebase-pull: {}",
            entry.summary
        );
        assert!(
            entry.summary.contains("origin/main"),
            "{strategy:?}: …and what was pulled: {}",
            entry.summary
        );
        assert_eq!(
            entry.old_oid.as_deref(),
            Some(before.as_str()),
            "{strategy:?}: {entry:?}"
        );
        assert_eq!(
            entry.new_oid.as_deref(),
            Some(after.as_str()),
            "{strategy:?}: {entry:?}"
        );
        assert_eq!(entry.source, ActivitySource::App, "{entry:?}");

        assert!(
            !all.iter().any(|e| e.kind == forbidden),
            "{strategy:?}: a pull must not also journal a standalone {forbidden:?} \
             — the feed would describe an operation the user never submitted: \
             {all:?}"
        );
        // The fetch half's own per-ref entries are expected and correct: a
        // pull really does move remote-tracking refs, and that is a different
        // fact from the integration.
        assert!(
            all.iter().any(|e| e.kind == ActivityKind::Fetch),
            "{strategy:?}: the fetch half must still journal what it moved: {all:?}"
        );
    }
}

/// The paired negative: a pull that integrated nothing journals no `Pull`
/// entry.
///
/// Without this, `a_pull_journals_one_entry_naming_its_strategy` would pass for
/// an implementation that journaled on entry to `exec_pull` regardless of what
/// happened — a feed in which every pull looks like a change is exactly as
/// uninformative as one in which none do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_up_to_date_pull_journals_no_integration() {
    let (_dir, repo) = diverged_repo("mine.txt");
    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = journaled(&repo)
        .iter()
        .filter(|e| e.kind == ActivityKind::Pull)
        .count();
    assert_eq!(first, 1, "the first pull did integrate");

    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        journaled(&repo)
            .iter()
            .filter(|e| e.kind == ActivityKind::Pull)
            .count(),
        1,
        "an up-to-date pull moved nothing, so it must add no Pull entry: {:?}",
        journaled(&repo)
    );
}

/// A conflicted pull journals **no** integration entry: it was aborted, so
/// nothing happened to the branch, and a `Pull` event claiming otherwise would
/// offer an undo for a commit that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicted_pull_journals_no_integration() {
    let (_dir, repo) = conflicting_repo();
    let (status, body) = pipeline(&repo, pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let all = journaled(&repo);
    assert!(
        !all.iter()
            .any(|e| matches!(e.kind, ActivityKind::Pull | ActivityKind::Merge)),
        "an aborted pull changed nothing about the branch, so the feed must \
         not say it did: {all:?}"
    );
    // …but the fetch really did land objects, and the feed says so.
    assert!(
        all.iter().any(|e| e.kind == ActivityKind::Fetch),
        "the fetch half's refs did move, and hiding that would leave the feed \
         disagreeing with the repository: {all:?}"
    );
}

// ---------------------------------------------------------------------------
// Reuse, proved at the boundary rather than by reading the source
// ---------------------------------------------------------------------------

/// A pull's fetch half publishes transfer progress on the operation's own
/// record — which it can only do by going through `planner::fetch`'s streaming
/// spawn, since that is the only code in this server that parses git's
/// `--progress` records.
///
/// This is the *behavioural* half of the "pull reuses fetch" claim that the
/// contract suite pins at source level: a `planner::pull` that quietly grew
/// its own `git fetch` would still fetch, still integrate, and still pass
/// every other test in this file — but it would publish nothing here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pulls_fetch_half_publishes_transfer_progress() {
    let (dir, repo) = diverged_repo("mine.txt");
    // Five content commits on the remote: measured against git 2.43.0, a
    // single small commit prints no progress records at all (see
    // `fetch_suite::repo_with_remote_ahead`), so the fixture is fattened here
    // for the same reason.
    let authoring = dir.path().join("authoring");
    for n in 0..5 {
        let name = format!("f{n}.txt");
        std::fs::write(authoring.join(&name), format!("content {n}\n")).unwrap();
        run(&authoring, &["add", &name]);
        run(&authoring, &["commit", "-q", "-m", &format!("c{n}")]);
    }
    run(&authoring, &["push", "-q", "origin", "main"]);

    let (handle, record) = admit_pull("progress", MergeStrategy::Merge);
    let mut rx = record.subscribe();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow_and_update();
            if let Some(p) = snapshot.progress {
                seen.push(p);
            }
            if snapshot.is_terminal() {
                break;
            }
        }
        seen
    });

    let (status, body) = run_tracked(&repo, record.clone(), pull_op(MergeStrategy::Merge)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    handle.finish(status, body, None);

    let seen = collector.await.unwrap();
    assert!(
        !seen.is_empty(),
        "a pull whose fetch half transfers six commits must publish transfer \
         progress — it reaches the remote through planner::fetch's streaming \
         spawn, and nothing else in this server parses git's --progress \
         records. Nothing here means a second, unstreamed fetch grew somewhere."
    );
}
