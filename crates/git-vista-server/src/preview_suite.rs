//! M10.08 (#576) — the graph preview's own suite.
//!
//! Included from [`super`] with `#[path]`, so it is a **child** of
//! `crate::preview` rather than a sibling and can see the module's private
//! items (`ScratchStore`, `commondir_of`, `recipe` and the pure parsers). A
//! sibling `mod preview_suite;` in `main.rs` could not, and the tests that
//! matter here are exactly the ones about private machinery.
//!
//! # What is proven, and what is only exercised
//!
//! * **A2** — the acceptance criterion that earns the feature. Counts objects
//!   under `<commondir>/objects`, captures every ref, and asserts both are
//!   unchanged and that no scratch directory survives.
//! * **A3** — a conflicting operation answers `Conflict { paths }`, a real
//!   answer, not an error and not a guessed graph.
//! * **A4** — an operation the plumbing cannot express answers `Unsupported`,
//!   from the **default** arm.
//! * **A5** — the predicted graph equals the graph produced by *actually
//!   running* the operation on a copy of the same repository.
//! * The four `Unavailable` reasons, each reached by its own route.
//! * The pure parsers, with literal expected values written out one per case.
//!
//! # Why a filesystem copy and not `git clone` for A5
//!
//! A clone carries `origin/*` refs the source repository does not have, and
//! `layout_with_refs` seeds branch colouring and lane reservation **from the
//! ref slice it is handed**. Laying out a clone would therefore differ from
//! laying out the preview for a reason that is not the mechanism — the exact
//! wrong-reason failure `git_vista_core::preview`'s own doc comment warns
//! about. A recursive copy of the repository directory is the same repository:
//! same refs, same oids, same worktree, no `origin`.
//!
//! # Why several fixtures are built here rather than taken from the catalogue
//!
//! `git_vista_fixtures::divergent` builds its commits with `git::run`, which
//! stamps them **now**. `stable_topo_order` emits ready commits from a max-heap
//! on `(time, Reverse(id))`, so a hypothetical commit stamped in the same
//! second as an unrelated branch tip has its row decided by the oid tiebreak —
//! and the hypothetical oid differs from the real one by construction. A5's
//! two halves would then disagree about row 0 roughly half the time, for a
//! reason that has nothing to do with the preview. The shapes built here pin
//! their commits' dates in the past so the new commit is unambiguously newest.
//! `merge_clean_two_branch` is used from the catalogue as-is because its merge
//! commit is the only ready commit in its `after` window and therefore has no
//! competitor to tie with; `cherry_pick_conflict` is used because a conflict
//! is never laid out at all.

use std::path::{Path, PathBuf};

use git_vista_core::layout::layout_with_refs;
use git_vista_core::model::{Graph, Oid};
use git_vista_fixtures::git;
use git_vista_protocol::preview::{PreviewOutcome, PreviewUnavailable};
use git_vista_protocol::{
    BranchName, CommitOid, GitOperation, Plan, RefName, RepositoryToken, WorktreeToken,
};
use tempfile::TempDir;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// Every commit in a suite-built fixture is stamped here — comfortably older
/// than the "now" both the preview's `commit-tree` and a real `git revert`
/// stamp their new commit with, so the new commit is unambiguously newest and
/// no oid tiebreak decides row 0. See the module doc.
const LONG_AGO: &str = "2020-01-01T00:00:00+0000";

/// Commit whatever is staged, dated [`LONG_AGO`].
fn commit_old(repo: &Path, message: &str) {
    git::run(repo, &["add", "-A"]);
    git::run_dated(repo, &["commit", "-q", "-m", message], LONG_AGO);
}

/// `main` with three commits, all dated in the past. The shape a revert needs:
/// a commit with exactly one parent, and history above it.
fn revert_shape() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    git::write(&repo, "a.txt", b"one\n");
    commit_old(&repo, "add a");
    git::write(&repo, "b.txt", b"two\n");
    commit_old(&repo, "add b");
    git::write(&repo, "c.txt", b"three\n");
    commit_old(&repo, "add c");
    (dir, repo)
}

/// `main` and `topic` from a shared base, editing distant regions of one file
/// so the pick is a real three-way merge that applies cleanly. All commits
/// dated in the past.
fn cherry_pick_shape() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    let base: String = (1..=12).map(|n| format!("line {n}\n")).collect();
    git::write(&repo, "target.txt", base.as_bytes());
    commit_old(&repo, "base");

    git::run(&repo, &["checkout", "-q", "-b", "topic"]);
    git::write(&repo, "topic-setup.txt", b"setup\n");
    commit_old(&repo, "topic: setup");
    git::write(
        &repo,
        "target.txt",
        base.replace("line 11\n", "line 11 by topic\n").as_bytes(),
    );
    commit_old(&repo, "topic: edit line 11");

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(
        &repo,
        "target.txt",
        base.replace("line 2\n", "line 2 by main\n").as_bytes(),
    );
    commit_old(&repo, "main: edit line 2");
    (dir, repo)
}

/// `main`, and a `behind` branch pointing at an ancestor of it. Merging
/// `behind` into `main` is already up to date; merging `main` into `behind` is
/// a fast-forward.
fn fast_forward_shape() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    git::write(&repo, "a.txt", b"one\n");
    commit_old(&repo, "add a");
    git::run(&repo, &["branch", "behind"]);
    git::write(&repo, "b.txt", b"two\n");
    commit_old(&repo, "add b");
    (dir, repo)
}

/// A **SHA-256** repository with two commits, dated in the past.
///
/// The only fixture in this suite whose object format is not `sha1`, and the
/// only thing that exercises `ScratchStore::new`'s
/// `rev-parse --show-object-format` read. Measured on this host on 2026-08-30:
/// a `--object-format=sha1` scratch store pointed at a sha256 repository
/// answers `fatal: Not a valid object name` for that repository's own HEAD,
/// while a `--object-format=sha256` store answers `commit`. Without this
/// shape, dropping the format read is a mutation the whole suite survives.
fn sha256_shape() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    // Not `git::init`, which hardcodes the default object format.
    std::fs::create_dir_all(&repo).expect("create the repository directory");
    git::run(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    git::run(&repo, &["config", "user.email", "suite@git-vista.invalid"]);
    git::run(&repo, &["config", "user.name", "preview-suite"]);
    git::write(&repo, "a.txt", b"one\n");
    commit_old(&repo, "add a");
    git::write(&repo, "b.txt", b"two\n");
    commit_old(&repo, "add b");
    assert_eq!(
        git::out(&repo, &["rev-parse", "--show-object-format"]),
        "sha256",
        "the fixture must really be sha256, or it proves nothing"
    );
    assert_eq!(
        git::out(&repo, &["rev-parse", "HEAD"]).len(),
        64,
        "a sha256 oid is 64 hex characters"
    );
    (dir, repo)
}

/// A recursive `std::fs` copy — the whole repository, `.git` included.
///
/// Constructs no `Command`: a raw spawn in this file would fail
/// `argv_boundary`'s source scan until someone edited `ALLOWED_SPAWN_SITES`,
/// and that allowlist is not this issue's to widen.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy destination");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("read dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copy file");
        }
    }
}

/// Copy `repo` into a throwaway directory so a real git operation can be run
/// against it without touching the fixture.
fn copy_of(repo: &Path) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let copy = dir.path().join("copy");
    copy_tree(repo, &copy);
    (dir, copy)
}

/// Lay a real repository out the way the preview's `before`/`after` halves are
/// laid out — [`layout_with_refs`] and nothing else, so the two are compared
/// through the same pipeline.
fn layout_of(repo: &Path) -> Graph {
    let commits =
        git_vista_git::walk_history(repo, PREVIEW_HISTORY_LIMIT).expect("walk the history");
    let refs = git_vista_git::read_refs(repo).expect("read the refs");
    let head_branch = git_vista_git::read_head_branch(repo);
    layout_with_refs(commits, refs, head_branch.as_deref())
}

/// Build the real reviewable [`Plan`] the production caller would hand
/// [`preview`], rather than a hand-assembled one that could drift from it.
///
/// The tokens are literals rather than `planner::selection_tokens()`, which
/// reads the process-global current selection and panics under `cargo test`
/// where no server ever set one. `preview` reads only `plan.operation` — the
/// repository is resolved by the caller, per ADR 0003 — so the token values
/// are inert here, and taking them from a literal keeps this suite free of a
/// global other tests are concurrently writing.
fn tokens() -> (RepositoryToken, WorktreeToken) {
    (
        RepositoryToken::new("preview-suite-repo").expect("a non-empty token"),
        WorktreeToken::new("preview-suite-worktree").expect("a non-empty token"),
    )
}

async fn plan_for(repo: &Path, op: GitOperation) -> Plan {
    crate::planner::build_plan_only(repo, op, tokens()).await
}

/// Every ref in the repository, as `git show-ref` would list them — read
/// through `git_vista_git` so no `Command` is constructed here.
fn refs_snapshot(repo: &Path) -> Vec<(String, String)> {
    let mut refs: Vec<(String, String)> = git_vista_git::read_refs(repo)
        .expect("read the refs")
        .into_iter()
        .map(|r| (r.name, r.target.0))
        .collect();
    refs.sort();
    refs
}

/// How many files live under `<commondir>/objects`.
///
/// **Under `objects`, not under `commondir`.** The scratch store is a real
/// directory created inside `commondir`, so a count taken one level up would
/// include the store's own objects and go red for exactly the reason the
/// design works. A2 is "no new object in the repository", not "nothing written
/// under `.git`".
fn object_file_count(commondir: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&entry.path(), count),
                Ok(_) => *count += 1,
                Err(_) => {}
            }
        }
    }
    let mut count = 0;
    walk(&commondir.join("objects"), &mut count);
    count
}

/// The `gv-preview-*` directories currently sitting in `commondir`.
fn scratch_dirs(commondir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(commondir) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name.starts_with(SCRATCH_PREFIX))
        .collect();
    found.sort();
    found
}

/// The two halves of a `Graph` outcome, re-materialised as core [`Graph`]s so
/// they can be compared against a real repository's own layout with one
/// function.
struct Halves {
    before: Graph,
    after: Graph,
}

/// The wire envelope back into a core [`Graph`]. The four fields the preview
/// carries are the four a comparison needs; the rest of `Graph` is backend
/// decoration (`repo_url`, `remote_commits`, …) that neither half ever sets.
fn rehydrate(g: PreviewGraph<GraphRow, Edge, BranchStub>) -> Graph {
    Graph {
        rows: g.rows,
        edges: g.edges,
        lane_count: g.lane_count,
        stubs: g.stubs,
        ..Graph::default()
    }
}

/// Unwrap a `Graph` outcome, printing the whole answer when it is not one —
/// an `Unavailable` reason is the thing a reader needs to see.
fn expect_graph(outcome: PreviewResponse) -> (Halves, Vec<PreviewChange>) {
    match outcome {
        PreviewOutcome::Graph {
            before,
            after,
            changes,
        } => (
            Halves {
                before: rehydrate(before),
                after: rehydrate(after),
            },
            changes,
        ),
        other => panic!("expected a Graph outcome, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A2 — the acceptance criterion
// ---------------------------------------------------------------------------

/// **A2.** A preview of a revert leaves the repository byte-identical where it
/// counts: no new object under `<commondir>/objects`, every ref exactly where
/// it was, and no scratch directory left behind.
///
/// This is the test the whole design exists to pass, so it asserts all three
/// facts about one real preview rather than trusting any of them separately.
///
/// # Two mutations that make it red, failing differently
///
/// 1. **Removes the isolation** — delete the `objects/info/alternates` write in
///    `ScratchStore::new`. The scratch store can then no longer see HEAD, so
///    `merge-tree` exits 128 and the preview answers
///    `Unavailable { CheckFailed }`. The **`expect_graph` assertion** fires
///    first, naming a reason, and the object/ref counts never get compared.
/// 2. **Weakens it** — `std::mem::forget(store)` instead of dropping it (or
///    swap `TempDir` for `TempDir::into_path`). Objects and refs are still
///    untouched and the outcome is still a `Graph`, so the first two assertions
///    pass; only the **`scratch_dirs` assertion** goes red, naming the
///    surviving directory. Different assertion, different message, different
///    stage of the test.
#[tokio::test]
async fn a2_a_preview_writes_no_object_moves_no_ref_and_leaves_no_scratch_directory() {
    let (_dir, repo) = revert_shape();
    let commondir = commondir_of(&repo).expect("resolve the commondir");

    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let objects_before = object_file_count(&commondir);
    let refs_before = refs_snapshot(&repo);
    assert!(
        objects_before > 0,
        "the fixture must actually have objects, or the count proves nothing"
    );

    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let outcome = preview(&repo, &plan).await;

    let (_graph, _changes) = expect_graph(outcome);

    assert_eq!(
        object_file_count(&commondir),
        objects_before,
        "a preview must add no object to <commondir>/objects — the whole \
         safety argument is that the hypothetical commit lives only in the \
         scratch store"
    );
    assert_eq!(
        refs_snapshot(&repo),
        refs_before,
        "a preview must move no ref"
    );
    assert_eq!(
        scratch_dirs(&commondir),
        Vec::<String>::new(),
        "the scratch store must be gone by the time preview() returns"
    );
}

/// **A2, the error path.** The scratch store is removed even when the git work
/// fails partway through.
///
/// Reverting a *merge* commit is refused after the store would have been
/// needed — but the honest version of this is the `?` path: a bogus commit id
/// makes `read_commit_record` fail, and a conflicting merge makes `merge_tree`
/// return `Conflict` after the store exists. The conflict route is the one
/// that actually creates a store and then returns early, so it is the one that
/// proves cleanup on a non-happy path.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `std::mem::forget` the store in the
///    `Conflict` arm of `compute`: the directory survives and this test names
///    it.
/// 2. **Weakens it** — make `ScratchStore::sweep_stale` the only cleanup by
///    replacing `TempDir` with a plain `PathBuf`: the directory survives this
///    test too, but `a2_…_leaves_no_scratch_directory` above also goes red,
///    which is a different failure surface than the one this test alone names.
#[tokio::test]
async fn a2_the_scratch_store_is_removed_on_the_conflict_path_too() {
    let (_dir, repo) = git_vista_fixtures::cherry_pick_conflict();
    let commondir = commondir_of(&repo).expect("resolve the commondir");
    let objects_before = object_file_count(&commondir);
    let refs_before = refs_snapshot(&repo);

    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic).expect("a full hex oid"),
        },
    )
    .await;

    match preview(&repo, &plan).await {
        PreviewOutcome::Conflict { .. } => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(
        scratch_dirs(&commondir),
        Vec::<String>::new(),
        "the scratch store must be gone after a conflict too — the early \
         return is an exit path like any other"
    );
    assert_eq!(object_file_count(&commondir), objects_before);
    assert_eq!(refs_snapshot(&repo), refs_before);
}

// ---------------------------------------------------------------------------
// A3 — a conflict is an answer
// ---------------------------------------------------------------------------

/// **A3.** A cherry-pick that would conflict answers `Conflict { paths }`,
/// naming the file, rather than erroring or drawing a graph.
///
/// The path is asserted as a **literal** — `git_vista_fixtures`'
/// `cherry_pick_conflict` builds `target.txt` as the file both sides edit
/// — so a parser that returned git's prose (`Auto-merging`) instead of a path
/// fails here rather than passing on a non-empty vector.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — classify `Some(1)` as
///    `MergeTreeAnswer::Clean` in `merge_tree`. The preview then draws a graph
///    for a merge that does not apply, and this test's `match` names it.
/// 2. **Weakens it** — drop the `break` on the first empty record in
///    `parse_merge_tree_conflicts`, so the informational block is read as
///    paths. The outcome is still `Conflict`, but `paths` gains
///    `Auto-merging`-shaped entries and the literal equality goes red with a
///    different message.
#[tokio::test]
async fn a3_a_conflicting_cherry_pick_answers_conflict_naming_the_file() {
    let (_dir, repo) = git_vista_fixtures::cherry_pick_conflict();
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic).expect("a full hex oid"),
        },
    )
    .await;

    match preview(&repo, &plan).await {
        PreviewOutcome::Conflict { paths } => {
            assert_eq!(
                paths,
                vec!["target.txt".to_string()],
                "the conflicted path must be the file itself, not git's prose"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A4 — Unsupported is the default arm
// ---------------------------------------------------------------------------

/// **A4.** Every operation outside the three this slice supports answers
/// `Unsupported`, and the name it reports is the operation's own wire tag.
///
/// Written as literal `(operation, expected name)` pairs, one per case, so the
/// mapping is asserted rather than re-derived by calling the function that
/// defines it.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — change `previewable`'s `_ => None` to
///    `_ => Some(Previewable::Merge { branch: "main".into() })`. Every case
///    here stops being `Unsupported` and the `match` panics.
/// 2. **Weakens it** — have `operation_name` return a constant
///    `"unsupported"`. Every case is still `Unsupported`, so the shape check
///    passes; the literal name comparison is what goes red, once per case.
#[tokio::test]
async fn a4_operations_the_plumbing_cannot_express_answer_unsupported() {
    let (_dir, repo) = revert_shape();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);

    let cases: Vec<(GitOperation, &str)> = vec![
        (
            GitOperation::RebaseOntoBase {
                base: RefName::new("main").expect("a valid ref name"),
            },
            "rebase_onto_base",
        ),
        (
            GitOperation::ResetBranch {
                branch: BranchName::new("main").expect("a valid branch name"),
                to: CommitOid::new(head.clone()).expect("a full hex oid"),
                expected_tip: CommitOid::new(head.clone()).expect("a full hex oid"),
            },
            "reset_branch",
        ),
        (
            GitOperation::CherryPickMerge {
                commit: CommitOid::new(head.clone()).expect("a full hex oid"),
                mainline: std::num::NonZero::new(1).expect("1 is non-zero"),
            },
            "cherry_pick_merge",
        ),
        (
            GitOperation::RevertMerge {
                commit: CommitOid::new(head.clone()).expect("a full hex oid"),
                mainline: std::num::NonZero::new(1).expect("1 is non-zero"),
            },
            "revert_merge",
        ),
        (
            GitOperation::CheckoutBranch {
                branch: BranchName::new("main").expect("a valid branch name"),
            },
            "checkout_branch",
        ),
    ];

    for (op, expected) in cases {
        let plan = plan_for(&repo, op.clone()).await;
        match preview(&repo, &plan).await {
            PreviewOutcome::Unsupported { operation } => assert_eq!(
                operation, expected,
                "the reported name must be this operation's own wire tag"
            ),
            other => panic!("expected Unsupported for {op:?}, got {other:?}"),
        }
    }
}

/// **A4, the instance-level refusal.** Reverting a *merge* commit is one of
/// the three supported names and still cannot be expressed: `merge-tree` needs
/// a sole parent as `theirs`, and a merge commit has two.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — make `sole_parent` return
///    `target.parents.first()`. The preview then reverts against an arbitrary
///    parent and draws a graph; this test's `match` names it.
/// 2. **Weakens it** — return `Unavailable { CheckFailed }` instead of
///    `Unsupported`. The preview still refuses, so nothing is drawn, but the
///    caller is told to retry something permanent — and the `match` goes red
///    on a different arm.
#[tokio::test]
async fn a4_reverting_a_merge_commit_is_unsupported_not_a_guessed_graph() {
    let (_dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    git::run(&repo, &["merge", "-q", "--no-edit", "feature"]);
    let merge_commit = git::out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git::out(&repo, &["rev-list", "-1", "--merges", "HEAD"]),
        merge_commit,
        "the fixture must really be sitting on a merge commit"
    );

    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(merge_commit).expect("a full hex oid"),
        },
    )
    .await;
    match preview(&repo, &plan).await {
        PreviewOutcome::Unsupported { operation } => assert_eq!(operation, "revert_commit"),
        other => panic!("expected Unsupported for a merge revert, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A5 — the predicted graph equals the real one
// ---------------------------------------------------------------------------

/// Compare a preview's `after` half against a real run's layout.
///
/// The hypothetical commit's oid is **not** compared: it cannot be, because
/// `commit-tree` stamps its own committer date and `git_cmd` exposes no arity
/// that would let this file pin `GIT_COMMITTER_DATE`. So the one id present in
/// `after` but not in `before` is mapped onto the one id present in the real
/// layout but not in `before`, by position, and everything else — parent
/// topology, lane, row — is compared for equality.
fn assert_parity(after: &Graph, real: &Graph, before: &Graph, what: &str) {
    let known: Vec<Oid> = before.rows.iter().map(|r| r.commit.id.clone()).collect();
    let novel = |g: &Graph| -> Vec<Oid> {
        g.rows
            .iter()
            .map(|r| r.commit.id.clone())
            .filter(|id| !known.contains(id))
            .collect()
    };
    let predicted_new = novel(after);
    let real_new = novel(real);
    assert_eq!(
        predicted_new.len(),
        1,
        "{what}: the preview must add exactly one commit, got {predicted_new:?}"
    );
    assert_eq!(
        real_new.len(),
        1,
        "{what}: the real run must add exactly one commit, got {real_new:?}"
    );
    let map = |oid: &Oid| -> Oid {
        if *oid == predicted_new[0] {
            real_new[0].clone()
        } else {
            oid.clone()
        }
    };

    assert_eq!(
        after.rows.len(),
        real.rows.len(),
        "{what}: row counts differ"
    );
    for (predicted, actual) in after.rows.iter().zip(real.rows.iter()) {
        assert_eq!(
            map(&predicted.commit.id),
            actual.commit.id,
            "{what}: row {} holds a different commit",
            actual.row
        );
        assert_eq!(
            predicted.commit.parents.iter().map(map).collect::<Vec<_>>(),
            actual.commit.parents,
            "{what}: row {} has different parent topology",
            actual.row
        );
        assert_eq!(
            (predicted.row, predicted.lane),
            (actual.row, actual.lane),
            "{what}: commit {} is placed differently",
            actual.commit.id.short()
        );
    }
    assert_eq!(
        after.lane_count, real.lane_count,
        "{what}: the gutter widths differ"
    );
}

/// **A5, revert.** The predicted graph equals the graph a real `git revert`
/// produces on a copy of the same repository.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — pass `parents: vec![]` in the revert recipe.
///    `commit-tree` then writes a root commit, the parent-topology assertion
///    goes red naming row 0's parents.
/// 2. **Weakens it** — hand `lay_out_preview` an empty `ref_moves`. The commit
///    is still correct and its parents are still right, but the hypothetical
///    commit lands in lane 1 with a synthetic colour, so the **placement**
///    assertion goes red instead. (In the shipped code that case is refused
///    before it can be returned; the mutation removes that refusal too.)
#[tokio::test]
async fn a5_a_previewed_revert_matches_a_real_revert() {
    let (_dir, repo) = revert_shape();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&repo, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["revert", "--no-edit", &head]);
    let real = layout_of(&copy);

    assert_parity(&graph.after, &real, &before_layout, "revert");
}

/// **A5, cherry-pick.** Same, for `git cherry-pick`.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — swap `ours` and `theirs` in the cherry-pick
///    recipe. git then computes the reverse merge, the resulting tree differs
///    and (on this fixture) so does the commit's content; the parity assertion
///    goes red on the parent topology once the tree change cascades.
/// 2. **Weakens it** — use the picked commit itself as `merge_base` instead of
///    its parent. The pick then contributes nothing, `commit-tree` still
///    succeeds and the graph shape is *identical* — so this mutation is caught
///    by `a5_cherry_pick_actually_moves_the_content` below rather than by the
///    shape comparison, which is why both tests exist.
#[tokio::test]
async fn a5_a_previewed_cherry_pick_matches_a_real_cherry_pick() {
    let (_dir, repo) = cherry_pick_shape();
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&repo, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["cherry-pick", &topic]);
    let real = layout_of(&copy);

    assert_parity(&graph.after, &real, &before_layout, "cherry-pick");
}

/// **A5, merge.** Same, for a real two-parent merge, over the catalogue's own
/// `merge_clean_two_branch` — the shape built for exactly this.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — drop the second parent from the merge
///    recipe's `parents`. The predicted commit has one parent where the real
///    one has two, and the parent-topology assertion names it.
/// 2. **Weakens it** — transpose `parents` to `[tip, head]`. Both parents are
///    still present and the row/lane placement can survive it, but the ordered
///    parent-topology comparison goes red — a different assertion from the
///    first mutation's, on the same row.
#[tokio::test]
async fn a5_a_previewed_merge_matches_a_real_merge() {
    let (_dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&repo, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["merge", "-q", "--no-edit", "feature"]);
    let real = layout_of(&copy);

    assert_parity(&graph.after, &real, &before_layout, "merge");
}

/// The cherry-pick really carries the picked commit's *content* across, not
/// just its shape.
///
/// The shape comparison above cannot see this: a preview that used the picked
/// commit as its own merge base would produce a commit with the right parents
/// in the right lane whose tree was simply HEAD's. So the hypothetical
/// commit's tree is compared against the real cherry-pick's tree — the one
/// value that is identical across the two runs even though the commit oids are
/// not, because a tree hashes content and not time.
#[tokio::test]
async fn a5_cherry_pick_actually_moves_the_content() {
    let (_dir, repo) = cherry_pick_shape();
    let topic = git::out(&repo, &["rev-parse", "topic"]);

    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic.clone()).expect("a full hex oid"),
        },
    )
    .await;
    // Recompute the tree through the same recipe the preview used, in a store
    // of this test's own, so the tree oid can be read before the store is
    // dropped.
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let op = previewable(&plan.operation).expect("cherry-pick is previewable");
    let plumbing = resolve_plumbing(&repo, &op, &head)
        .await
        .expect("resolve the plumbing");
    let Plumbing::Synthesize(recipe) = plumbing else {
        panic!("a clean cherry-pick must synthesize a commit");
    };
    let predicted_tree = match merge_tree(&repo, &recipe).await.expect("merge-tree ran") {
        MergeTreeAnswer::Clean { tree } => tree,
        other => panic!("expected a clean merge, got {other:?}"),
    };

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["cherry-pick", &topic]);
    let real_tree = git::out(&copy, &["rev-parse", "HEAD^{tree}"]);

    assert_eq!(
        predicted_tree, real_tree,
        "the previewed tree must be the tree a real cherry-pick writes — a \
         tree hashes content, so this is comparable across two runs whose \
         commit oids can never be"
    );
}

// ---------------------------------------------------------------------------
// The two merge cases that create no commit
// ---------------------------------------------------------------------------

/// Merging a branch that is already an ancestor of HEAD adds nothing and moves
/// nothing. Both halves of the graph are identical and `changes` is empty —
/// which here is the *claim*, not an absence.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `base == tip` arm in
///    `resolve_plumbing`. A commit is then synthesised for a merge git would
///    refuse to make, and the `changes` assertion goes red with an `Added`
///    entry.
/// 2. **Weakens it** — return `FastForward { to: tip }` instead. No commit is
///    invented, so `changes` still has no `Added` — but the refs are reported
///    as moving backwards to the ancestor, and the before/after equality
///    assertion goes red instead.
#[tokio::test]
async fn a_merge_that_is_already_up_to_date_adds_nothing_and_moves_no_ref() {
    let (_dir, repo) = fast_forward_shape();
    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("behind").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, changes) = expect_graph(preview(&repo, &plan).await);

    assert_eq!(
        changes,
        Vec::new(),
        "an already-up-to-date merge changes nothing at all"
    );
    assert_eq!(
        graph.before.rows, graph.after.rows,
        "the two halves must be the same graph"
    );
}

/// A fast-forward merge moves the refs and creates no commit.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `base == head` arm. A merge
///    commit is invented that git would never write, and the "no `Added`"
///    assertion goes red.
/// 2. **Weakens it** — keep the arm but pass `Vec::new()` as the ref moves.
///    Still no commit, so the first assertion holds; the `RefMoved` assertion
///    goes red because nothing is reported as having moved at all.
#[tokio::test]
async fn a_fast_forward_merge_moves_the_refs_and_adds_no_commit() {
    let (_dir, repo) = fast_forward_shape();
    let tip = git::out(&repo, &["rev-parse", "main"]);
    let behind = git::out(&repo, &["rev-parse", "behind"]);
    git::run(&repo, &["checkout", "-q", "behind"]);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("main").expect("a valid branch name"),
        },
    )
    .await;
    let (_graph, changes) = expect_graph(preview(&repo, &plan).await);

    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, PreviewChange::Added { .. })),
        "a fast-forward creates no commit, so nothing may be reported as added: {changes:?}"
    );
    assert!(
        changes.contains(&PreviewChange::RefMoved {
            ref_name: "behind".to_string(),
            from: Oid(behind.clone()),
            to: Oid(tip.clone()),
        }),
        "the checked-out branch must be reported as moving to the merged tip: {changes:?}"
    );
    assert!(
        changes.contains(&PreviewChange::RefMoved {
            ref_name: "HEAD".to_string(),
            from: Oid(behind),
            to: Oid(tip),
        }),
        "HEAD moves with the branch it is attached to: {changes:?}"
    );
}

// ---------------------------------------------------------------------------
// The four Unavailable reasons
// ---------------------------------------------------------------------------

/// A repository open in Visualize mode has no read-write grant, so no scratch
/// store can live in it. The answer names *that*, not `Unsupported`.
///
/// The mode is set through `state::set_current`, which also registers the path
/// in the catalog as read-only — so `read_only_for_path` answers `true` from
/// the catalog even if a concurrent test moves the current selection.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `read_only_for_path` check. The
///    preview then tries to create a store in a repository the sandbox will
///    not grant write access to, and answers `ScratchStore` (or worse, a
///    graph); either way the `match` here names it.
/// 2. **Weakens it** — return `Unsupported { operation }` for the read-only
///    case. The preview still refuses, so nothing wrong is drawn, but the user
///    is told the operation can never be previewed instead of "reopen in
///    Active mode" — a different arm, a different message.
#[tokio::test]
async fn a_read_only_repository_answers_repository_read_only() {
    let (_dir, repo) = revert_shape();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;

    // `with_isolated_test_current` scopes the selection to this task, so the
    // mode this test sets cannot leak into a concurrently running one — and
    // cannot be clobbered by one either.
    let outcome = crate::state::with_isolated_test_current(async {
        crate::state::set_current(&repo, git_vista_protocol::RepoMode::Visualize);
        preview(&repo, &plan).await
    })
    .await;

    match outcome {
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::RepositoryReadOnly,
        } => {}
        other => panic!("expected Unavailable{{RepositoryReadOnly}}, got {other:?}"),
    }
}

/// An unborn HEAD is not a fact about the operation and not a conflict: it is
/// "the check could not run", which is `CheckFailed`.
#[tokio::test]
async fn an_unborn_head_answers_check_failed_rather_than_a_graph() {
    let (_dir, repo) = git_vista_fixtures::empty();
    // `empty()` has no commits, so there is no oid to name; any well-formed
    // one will do — the point is that HEAD does not resolve, which is checked
    // before the commit is ever looked up.
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new("0".repeat(40)).expect("a full hex oid"),
        },
    )
    .await;
    match preview(&repo, &plan).await {
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::CheckFailed { detail },
        } => assert!(
            detail.contains("HEAD"),
            "the detail must name what could not be established: {detail}"
        ),
        other => panic!("expected Unavailable{{CheckFailed}}, got {other:?}"),
    }
}

/// A directory that is not a repository has no commondir, so there is nowhere
/// for a scratch store to live — `ScratchStore`, not `CheckFailed`. The two
/// are different facts: one says the computation failed, the other says it
/// never had anywhere to happen.
#[test]
fn a_directory_with_no_git_answers_scratch_store() {
    let dir = TempDir::new().expect("tempdir");
    match commondir_of(dir.path()) {
        Err(PreviewUnavailable::ScratchStore { detail }) => assert!(
            !detail.is_empty(),
            "the reason must carry git's own account of the failure"
        ),
        other => panic!("expected ScratchStore, got {other:?}"),
    }
}

/// The version gate's polarity, with literal versions on both sides of the
/// floor.
///
/// Written as `(input, expected)` literals rather than by re-deriving the
/// comparison, so an inverted `>=` fails here instead of passing whichever way
/// it runs.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `return None` unconditionally. The four
///    below-floor cases go red.
/// 2. **Weakens it** — invert the comparison to `<=`. `2.38.0` and `2.43.0`
///    then report `GitTooOld` and the two above-floor cases go red instead.
#[test]
fn the_version_gate_refuses_below_2_38_and_allows_2_38_itself() {
    assert!(version_gate((2, 38, 0)).is_none(), "2.38.0 is the floor");
    assert!(version_gate((2, 43, 0)).is_none(), "2.43.0 is above it");
    assert!(version_gate((3, 0, 0)).is_none(), "3.0.0 is above it");

    assert_eq!(
        version_gate((2, 37, 3)),
        Some(PreviewUnavailable::GitTooOld {
            found: "2.37.3".to_string(),
            minimum: "2.38".to_string(),
        })
    );
    assert_eq!(
        version_gate((2, 32, 0)),
        Some(PreviewUnavailable::GitTooOld {
            found: "2.32.0".to_string(),
            minimum: "2.38".to_string(),
        }),
        "the product floor is a supported host that simply does not get this feature"
    );
    assert_eq!(
        version_gate((1, 99, 99)),
        Some(PreviewUnavailable::GitTooOld {
            found: "1.99.99".to_string(),
            minimum: "2.38".to_string(),
        }),
        "a lower major is below the floor whatever its minor says"
    );
}

// ---------------------------------------------------------------------------
// The scratch store
// ---------------------------------------------------------------------------

/// The store is created inside the repository's own `commondir`, under the
/// named prefix the sweep looks for, with an `alternates` file naming the real
/// object directory — and it is gone once dropped.
///
/// The prefix is asserted from the **name the code produced**, which is what
/// makes `sweep_stale` non-inert: a `tempfile` default of `.tmpXXXXXX` would
/// pass a hand-written "a stale directory is removed" test while never
/// matching anything the module actually creates.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `tempdir_in(std::env::temp_dir())` instead
///    of `tempdir_in(&commondir)`. The containment assertion goes red.
/// 2. **Weakens it** — drop the `.prefix(SCRATCH_PREFIX)` call. The store is
///    still inside `commondir` and still readable, so the containment and
///    alternates assertions pass; only the prefix assertion goes red, and with
///    it the sweep silently stops matching anything.
#[tokio::test]
async fn the_scratch_store_lives_under_commondir_under_the_swept_prefix() {
    let (_dir, repo) = revert_shape();
    let commondir = commondir_of(&repo).expect("resolve the commondir");

    let path = {
        let store = ScratchStore::new(&repo).await.expect("create the store");
        let path = store.dir.path().to_path_buf();
        assert!(
            path.starts_with(&commondir),
            "the store must sit inside the read-write grant the policy already \
             builds: {path:?} is not under {commondir:?}"
        );
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a utf-8 directory name");
        assert!(
            name.starts_with(SCRATCH_PREFIX),
            "the store's own name must carry the prefix `sweep_stale` looks \
             for, or the sweep is inert: {name}"
        );
        let alternates =
            std::fs::read_to_string(path.join("objects").join("info").join("alternates"))
                .expect("the alternates file must exist");
        assert_eq!(
            alternates.trim(),
            commondir.join("objects").display().to_string(),
            "the alternates file must name the served repository's own object \
             directory — that is the whole read path"
        );
        assert!(
            !path.join("hooks").exists(),
            "`-c init.templateDir=` must leave no hooks directory behind"
        );
        assert!(
            store.git_dir_flag().starts_with("--git-dir="),
            "the flag must be the one git resolves before the subcommand"
        );
        path
    };
    assert!(
        !path.exists(),
        "the store must be gone the moment it is dropped"
    );
}

/// The scratch store inherits the served repository's **object format**, so
/// the git half of a preview works on a SHA-256 repository.
///
/// # Why this test exists at all
///
/// Without it, deleting `"--object-format", &format` from `ScratchStore::new`'s
/// init argv is a mutation the entire rest of this suite **survives** — every
/// other fixture is SHA-1, where the default format happens to be right. That
/// was verified by running it, not assumed. The failure mode being pinned is
/// the worst kind: a code path that simply never works on such a repository
/// while every test stays green.
///
/// # What this test does NOT claim, and the finding behind that
///
/// It drives the git half — the store, `merge-tree`, `commit-tree`, the
/// read-back — and stops there. It deliberately does not call `preview()`,
/// because **`preview()` cannot succeed on a SHA-256 repository today, for a
/// reason that has nothing to do with this module**: `git_vista_git` opens
/// every repository with `gix`, and `gix` refuses this one. Measured here on
/// 2026-08-30, from `read_refs`:
///
/// ```text
/// Open { path: "…/repo", message: "Failed to load the git configuration" }
/// ```
///
/// So `preview()` answers `Unavailable { CheckFailed }` on such a repository,
/// which is the honest answer, and the object-format inheritance below is
/// correct-but-not-yet-reachable end to end. That whole-product limitation is
/// `git_vista_git`'s to fix, not this module's, and it is recorded in ADR 0099
/// rather than papered over. Asserting the current end-to-end failure here
/// would pin a bug in another crate as though it were a contract.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete `"--object-format", &format` from the
///    init argv. The store is SHA-1, cannot read the repository's objects
///    across the alternates boundary, and `merge_tree` fails; the
///    `MergeTreeAnswer::Clean` match panics.
/// 2. **Weakens it** — hardcode `"sha256"` in place of the probed `format`.
///    The flag is still passed, so this test still passes; the *probe* has
///    stopped tracking the repository, and **seven SHA-1 tests** go red
///    instead (A2 both ways, A3, and all four A5 legs). Verified by running
///    it. That is the different failure surface the pair needs: mutation 1
///    proves the flag is required, mutation 2 proves it must carry the value
///    `rev-parse --show-object-format` actually returned.
///
///    (Hardcoding `"sha1"` instead would fail on the *same* assertion as
///    mutation 1 — same line, same message — so it is deliberately not the
///    second mutation. Two breaks that land identically prove one thing, not
///    two.)
#[tokio::test]
async fn the_scratch_store_reads_a_sha256_repository_because_it_inherits_its_format() {
    let (_dir, repo) = sha256_shape();
    let commondir = commondir_of(&repo).expect("resolve the commondir");
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let objects_before = object_file_count(&commondir);
    let refs_before_raw = git::out(&repo, &["show-ref"]);

    let op = previewable(&GitOperation::RevertCommit {
        commit: CommitOid::new(head.clone()).expect("a 64-character hex oid"),
    })
    .expect("a revert is previewable");
    let plumbing = resolve_plumbing(&repo, &op, &head)
        .await
        .expect("the plumbing resolves");
    let Plumbing::Synthesize(recipe) = plumbing else {
        panic!("reverting an ordinary commit must synthesize one");
    };

    let tree = match merge_tree(&repo, &recipe).await.expect("merge-tree ran") {
        MergeTreeAnswer::Clean { tree } => tree,
        other => panic!("the revert applies cleanly; got {other:?}"),
    };
    assert_eq!(tree.len(), 64, "a sha256 tree oid is 64 hex characters");

    let parents: Vec<&str> = recipe.parents.iter().map(String::as_str).collect();
    let oid = commit_tree(&repo, &recipe.store, &tree, &parents, &recipe.message)
        .await
        .expect("commit-tree ran");
    let added = read_back(&repo, &recipe.store, &oid)
        .await
        .expect("the store can read its own object back");
    assert_eq!(added.id.0.len(), 64, "a sha256 commit oid is 64 characters");
    assert_eq!(
        added.parents,
        vec![Oid(head)],
        "the hypothetical revert sits on HEAD"
    );

    drop(recipe);
    assert_eq!(
        object_file_count(&commondir),
        objects_before,
        "A2 holds on a sha256 repository too"
    );
    assert_eq!(git::out(&repo, &["show-ref"]), refs_before_raw);
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
}

/// The sweep removes an old `gv-preview-*` directory, leaves a young one, and
/// never touches a name it did not choose.
///
/// The three cases are asserted together because the danger is not "the sweep
/// does not run" — it is "the sweep runs and deletes something else". This
/// function runs inside a user's `.git`.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — return early from `sweep_stale`. The stale
///    directory survives and the first assertion goes red.
/// 2. **Weakens it** — drop the `age < STALE_SCRATCH_AGE` guard. The stale one
///    is still removed, so the first assertion passes; the *young* directory
///    disappears too and the second assertion goes red — which in production
///    is a concurrent preview having its store deleted underneath it.
#[test]
fn the_sweep_removes_only_old_directories_it_named_itself() {
    let dir = TempDir::new().expect("tempdir");
    let commondir = dir.path();

    let stale = commondir.join(format!("{SCRATCH_PREFIX}stale"));
    let young = commondir.join(format!("{SCRATCH_PREFIX}young"));
    let foreign = commondir.join("objects");
    for d in [&stale, &young, &foreign] {
        std::fs::create_dir_all(d).expect("create dir");
    }
    // Age `stale` past the bound by rewriting its mtime.
    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&stale, long_ago);

    ScratchStore::sweep_stale(commondir);

    assert!(
        !stale.exists(),
        "a `gv-preview-*` directory older than the bound must be swept"
    );
    assert!(
        young.exists(),
        "a young `gv-preview-*` directory may be a concurrent preview's own \
         store and must never be swept"
    );
    assert!(
        foreign.exists(),
        "the sweep must never delete a directory it did not name"
    );
}

/// Set a directory's modification time.
///
/// `std::fs` has no setter, and this crate has no `filetime` dependency, so
/// the file is rewritten through a handle whose times are set with
/// `utimensat` via `std::fs::File::set_times` (stable since Rust 1.75).
fn filetime_set(path: &Path, when: std::time::SystemTime) {
    let file = std::fs::File::open(path).expect("open the directory");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set the directory's mtime");
}

// ---------------------------------------------------------------------------
// The pure parsers
// ---------------------------------------------------------------------------

/// `parse_git_version` against real and vendor-shaped lines, one literal
/// expectation per case.
#[test]
fn parse_git_version_reads_real_and_vendor_shaped_lines() {
    /// One `--version` line and the triple it must parse to. A named alias
    /// because clippy refuses the inline tuple type, and because naming it
    /// makes the table below read as data.
    type VersionCase = (&'static str, Option<(u32, u32, u32)>);

    let cases: &[VersionCase] = &[
        ("git version 2.43.0", Some((2, 43, 0))),
        ("git version 2.43.0\n", Some((2, 43, 0))),
        ("git version 2.39.5 (Apple Git-154)", Some((2, 39, 5))),
        ("git version 2.43.0.windows.1", Some((2, 43, 0))),
        ("git version 2.38", Some((2, 38, 0))),
        ("git version 2.37.3", Some((2, 37, 3))),
        // Not git's line: no fact, never a guess in either direction.
        ("gix version 0.66.0", None),
        ("2.43.0", None),
        ("git version banana", None),
        ("", None),
    ];
    for (line, expected) in cases {
        assert_eq!(parse_git_version(line), *expected, "for input {line:?}");
    }
}

/// `parse_merge_tree_conflicts` against the exact byte shape git 2.43.0
/// produced on this host, measured 2026-08-30.
///
/// The informational block after the empty record is the trap: it contains
/// records that look like paths (`c.txt`) and records that are prose
/// (`Auto-merging`). A parser that read past the empty record would report
/// `Auto-merging` as a conflicted file.
#[test]
fn parse_merge_tree_conflicts_stops_at_the_empty_record() {
    let mut stdout: Vec<u8> = Vec::new();
    let mut record = |s: &str| {
        stdout.extend_from_slice(s.as_bytes());
        stdout.push(0);
    };
    record("c789d9f2e2e2ed6733fca7da6bb531a1311c5aab");
    record("100644 df967b96a579e45a18b8251732d16804b2e56a55 1\tc.txt");
    record("100644 ba2906d0666cf726c7eaadd2cd3db615dedfdf3a 2\tc.txt");
    record("100644 0f62d67e76ce1255a098942495a846df0f8a2c11 3\tc.txt");
    record("100644 587be6b4c3f93f93c489c0111bba5596147a26cb 1\tsecond file.txt");
    record("100644 dd4623ffc23da603116a98a7cfc84b52a9c809a0 2\tsecond file.txt");
    record("100644 868e39663ec30a2ca8947b9fc8f26381d87c72ec 3\tsecond file.txt");
    record("");
    record("1");
    record("c.txt");
    record("Auto-merging");
    record("Auto-merging c.txt\n");
    record("1");
    record("c.txt");
    record("CONFLICT (contents)");
    record("CONFLICT (content): Merge conflict in c.txt\n");

    assert_eq!(
        parse_merge_tree_conflicts(&stdout),
        vec!["c.txt".to_string(), "second file.txt".to_string()],
        "each conflicted path once, in first-appearance order, and nothing \
         from the informational block"
    );
}

/// A clean `merge-tree -z` prints the tree oid and nothing else, so there are
/// no conflicted paths to find.
#[test]
fn parse_merge_tree_conflicts_finds_nothing_in_a_clean_result() {
    let stdout = b"18b95cf7cf7e8aa43f4a58556cb835f64adccd88\x00";
    assert_eq!(
        parse_merge_tree_conflicts(stdout),
        Vec::<String>::new(),
        "a clean merge names no conflicted path"
    );
    assert_eq!(
        parse_merge_tree_tree(stdout),
        Some("18b95cf7cf7e8aa43f4a58556cb835f64adccd88".to_string())
    );
}

/// A path containing a **real tab** — the one case that pins both the
/// first-tab split *and* the stop at the empty record.
///
/// # Why this case, and not the plain one it replaced
///
/// The first version of this suite asserted the stop-at-the-empty-record rule
/// against a fixture whose informational records were `1`, `c.txt`,
/// `Auto-merging`, `Auto-merging c.txt`. None of those contains a tab, so a
/// parser that read straight past the empty record found nothing to report and
/// the assertion passed either way. Replacing `break` with `continue` in
/// `parse_merge_tree_conflicts` **survived** it — the mechanism was removed and
/// the test stayed green. This is that hole, closed.
///
/// The bytes below are git 2.43.0's own output, measured on this host on
/// 2026-08-30, for a conflict on a file literally named `has<TAB>atab.txt`.
/// Every informational record then carries a tab, so a parser that read past
/// the empty record would also report `atab.txt` and `atab.txt\n`.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `break` → `continue` in
///    `parse_merge_tree_conflicts`. The result gains `"atab.txt"` and
///    `"atab.txt\n"`; this assertion goes red with two extra entries.
/// 2. **Weakens it** — split on the *last* tab instead of the first. Only one
///    entry is still produced, so the length is right; its value is
///    `atab.txt` instead of the whole path — a different wrong answer, in a
///    different position of the diff.
#[test]
fn parse_merge_tree_conflicts_keeps_a_tab_in_the_path_and_stops_before_the_prose() {
    let mut stdout: Vec<u8> = Vec::new();
    let mut record = |bytes: &[u8]| {
        stdout.extend_from_slice(bytes);
        stdout.push(0);
    };
    record(b"3d4287a22e231b3eab4a5f03e7852a4c629a32a8");
    record(b"100644 df967b96a579e45a18b8251732d16804b2e56a55 1\thas\tatab.txt");
    record(b"100644 ba2906d0666cf726c7eaadd2cd3db615dedfdf3a 2\thas\tatab.txt");
    record(b"100644 0f62d67e76ce1255a098942495a846df0f8a2c11 3\thas\tatab.txt");
    record(b"");
    record(b"1");
    record(b"has\tatab.txt");
    record(b"Auto-merging");
    record(b"Auto-merging has\tatab.txt\n");
    record(b"1");
    record(b"has\tatab.txt");
    record(b"CONFLICT (contents)");
    record(b"CONFLICT (content): Merge conflict in has\tatab.txt\n");
    record(b"");

    assert_eq!(
        parse_merge_tree_conflicts(&stdout),
        vec!["has\tatab.txt".to_string()],
        "the whole path, once: a tab inside a path belongs to the path, and \
         git's prose after the empty record is not a path at all"
    );
}

/// `parse_commit_record` reads the six NUL-separated fields, and reads the
/// **committer** time — the axis `walk_history` puts in `CommitSummary.time`
/// and the one `stable_topo_order` sorts on.
#[test]
fn parse_commit_record_reads_six_fields_and_the_committer_time() {
    let stdout = b"4f7672ab\x00aaaa bbbb\x001788127876\x00Ada\x00Merge branch 'x'\x00Merge branch 'x'\n\nlong body\n\x00\n";
    let record = parse_commit_record(stdout).expect("a readable record");
    assert_eq!(record.id, "4f7672ab");
    assert_eq!(record.parents, vec!["aaaa".to_string(), "bbbb".to_string()]);
    assert_eq!(record.time, 1_788_127_876);
    assert_eq!(record.author, "Ada");
    assert_eq!(record.subject, "Merge branch 'x'");
    assert_eq!(record.body, "Merge branch 'x'\n\nlong body\n");
}

/// A root commit's `%P` is empty, which is zero parents and not one empty one.
#[test]
fn parse_commit_record_reads_a_root_commit_as_having_no_parents() {
    let stdout = b"aaaa\x00\x001700000000\x00Ada\x00first\x00first\n\x00\n";
    let record = parse_commit_record(stdout).expect("a readable record");
    assert_eq!(record.parents, Vec::<String>::new());
    assert_eq!(
        sole_parent(&record),
        None,
        "a root commit has no sole parent"
    );
}

/// `sole_parent` is the fail-closed gate that keeps merge and root commits out
/// of the revert/cherry-pick recipes. Literal cases, one per arity.
#[test]
fn sole_parent_is_some_only_for_exactly_one_parent() {
    let with = |parents: Vec<&str>| CommitRecord {
        id: "aaaa".to_string(),
        parents: parents.into_iter().map(str::to_string).collect(),
        time: 0,
        author: String::new(),
        subject: String::new(),
        body: String::new(),
    };
    assert_eq!(sole_parent(&with(vec![])), None, "root");
    assert_eq!(sole_parent(&with(vec!["bbbb"])), Some("bbbb"), "ordinary");
    assert_eq!(sole_parent(&with(vec!["bbbb", "cccc"])), None, "merge");
    assert_eq!(
        sole_parent(&with(vec!["b", "c", "d"])),
        None,
        "octopus merge"
    );
}

/// The revert message is git's own default, word for word.
///
/// Bound to the state that produces it and asserted as a literal: a message
/// that stayed constant while the commit it names changed would show the user
/// a sentence about the wrong commit.
#[test]
fn the_revert_message_reproduces_gits_own_default_wording() {
    let record = CommitRecord {
        id: "c129f783ec832c7ad6c23eca509d9d70ad0c0d9b".to_string(),
        parents: vec!["aaaa".to_string()],
        time: 0,
        author: "Ada".to_string(),
        subject: "m1".to_string(),
        body: "m1\n".to_string(),
    };
    assert_eq!(
        revert_message(&record),
        "Revert \"m1\"\n\nThis reverts commit c129f783ec832c7ad6c23eca509d9d70ad0c0d9b.\n"
    );
}

/// The merge message names the branch that was merged.
#[test]
fn the_merge_message_names_the_branch_it_merged() {
    assert_eq!(merge_message("feature"), "Merge branch 'feature'\n");
    assert_eq!(merge_message("release/2.0"), "Merge branch 'release/2.0'\n");
}

/// `previewable` maps exactly three operations and nothing else, and
/// `operation_name` reports the wire tag. Literal pairs, one per case.
#[test]
fn previewable_maps_three_operations_and_defaults_to_none() {
    let oid = || CommitOid::new("a".repeat(40)).expect("a full hex oid");
    assert_eq!(
        previewable(&GitOperation::RevertCommit { commit: oid() }),
        Some(Previewable::Revert {
            commit: "a".repeat(40)
        })
    );
    assert_eq!(
        previewable(&GitOperation::CherryPick { commit: oid() }),
        Some(Previewable::CherryPick {
            commit: "a".repeat(40)
        })
    );
    assert_eq!(
        previewable(&GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name")
        }),
        Some(Previewable::Merge {
            branch: "feature".to_string()
        })
    );
    assert_eq!(
        previewable(&GitOperation::StageAll),
        None,
        "everything else falls through the default arm"
    );
    assert_eq!(
        operation_name(&GitOperation::StageAll),
        "stage_all",
        "the name is serde's own tag, so a variant added later is named \
         correctly here without anyone editing this file"
    );
}
