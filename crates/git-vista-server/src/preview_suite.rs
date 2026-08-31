//! M10.08 (#576) — the graph preview's own suite.
//!
//! Included from [`super`] with `#[path]`, so it is a **child** of
//! `crate::preview` rather than a sibling and can see the module's private
//! items (`ScratchStore`, `PreviewTarget`, `recipe` and the pure parsers). A
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
//! # Which fixtures come from the catalogue, and which are built here
//!
//! One rule decides it, and it is about the **oid tiebreak**, not about taste.
//!
//! `git_vista_fixtures::divergent` builds its commits with `git::run`, which
//! stamps them **now**. `stable_topo_order` emits ready commits from a max-heap
//! on `(time, Reverse(id))`, so when two commits are ready at once with equal
//! timestamps their order is decided by oid — and a hypothetical commit's oid
//! differs from the real one by construction. A test that compares *positions*
//! would then disagree about row 0 roughly half the time, for a reason that has
//! nothing to do with the preview.
//!
//! So:
//!
//! * **A test that compares a layout, and whose operation adds a commit, builds
//!   its shape here**, pinning every commit's date to [`LONG_AGO`] so the new
//!   commit is unambiguously newest. That is [`revert_shape`],
//!   [`cherry_pick_shape`], [`revert_shape_with_a_competitor_tip`],
//!   [`merge_shape_with_a_competitor_tip`], plus [`fast_forward_shape`] and
//!   [`sha256_shape`], which pin a `merge.ff` value and an object format
//!   respectively.
//! * **A test that compares an outcome, a tree, or a refusal takes the
//!   catalogue shape**, because no layout is compared and none of the above
//!   applies. That is `cherry_pick_conflict`, `merge_conflict`,
//!   `cherry_pick_clean`, `cherry_pick_already_applied` and
//!   `divergent_merge_ff_only`. Those shapes prove their own claims against a
//!   real `git` on a disposable clone, which a shape built here does not, so
//!   where both are usable the catalogue one is the stronger instrument.
//!
//! Two catalogue shapes *are* laid out, each for a stated reason rather than by
//! exception:
//!
//! * `merge_clean_two_branch` and `fast_forward_merge_ff_false` — at every step
//!   of their `after` window exactly **one** commit is ready, so there is never
//!   a tie for the oid to break.
//! * `fast_forward_merge_ff_unset` — a fast-forward adds **no** commit at all,
//!   so there is no hypothetical oid in the comparison and both sides can be
//!   required equal outright (see [`assert_identical_layout`]).
//!
//! `cherry_pick_shape` and `merge_shape_with_a_competitor_tip` therefore live
//! on beside catalogue twins that look interchangeable and are not: the twins
//! stamp *now*, and these two are the shapes the row-position tests need.
//!
//! # What three independent verifiers found green here, and what changed
//!
//! An earlier round of this suite was fully green while the preview could not
//! tell computing the right answer from computing nothing. Every item below was
//! *measured* by mutating the production code and watching the suite stay
//! green, not reasoned about:
//!
//! * A revert preview that reverted nothing, and a merge preview that merged
//!   nothing, passed every test — because nothing compared the hypothetical
//!   commit's **tree**. [`assert_tree_matches_the_real_run`] does, on all three
//!   legs, and each comparison is paired with an `assert_ne!` proving the
//!   oracle's own tree actually moved. An equality between two copies of HEAD's
//!   tree is not evidence.
//! * Dropping every edge from both graphs kept the whole binary green —
//!   `assert_parity` never looked at `edges`. It does now, and asserts the real
//!   run's edge set is non-empty first, because `[] == []` is the same
//!   vacuous pass in a different position.
//! * Refs (the branch badges) and `color` were likewise never compared.
//! * Row position was decidable on the cherry-pick leg only: the revert and
//!   merge `after` windows each had exactly one topologically-ready commit, so
//!   a hypothetical commit stamped at time 0 could not change the order.
//!   [`revert_shape_with_a_competitor_tip`] and [`merge_shape_with_a_competitor_tip`]
//!   add an independent branch tip dated [`LONG_AGO`], so "newest first" has
//!   something to be newest *than*.
//! * There was no conflicting-**merge** test at all; A3 covered cherry-pick
//!   alone, and the merge leg is where a wrong answer is most dangerous.
//!
//! # `git_dir_manifest`, and why it is stronger than an object count
//!
//! The A2 tests above count files under `<commondir>/objects`. That is the
//! acceptance criterion, but it cannot see a ref file rewritten, a `config`
//! edited, a `logs/HEAD` appended to, or a scratch directory that survived with
//! a different name. [`git_dir_manifest`] hashes **every byte of every file**
//! under the whole common directory, so "changed nothing" means changed
//! nothing. Keep it that way: softening it back into a count is how the weaker
//! version was arrived at the first time.
//!
//! # `merge.ff` — four values, four tests, and one deliberate divergence
//!
//! [`resolve_plumbing`]'s merge arm reads `merge.ff` through
//! [`fast_forward_policy`], because the executor
//! (`planner::branch_exec::exec_merge`) runs `["merge", "--no-edit"]`, which
//! obeys it. Measured in throwaway repositories on this host, 2026-08-30, and
//! reproduced as the oracle inside each test below:
//!
//! | `merge.ff` | What real `git merge --no-edit` does | Test |
//! |---|---|---|
//! | **unset** | fast-forwards; moves the ref to the branch's own oid, writes no commit | `merge_ff_unset_previews_the_fast_forward_git_actually_performs` |
//! | `false` | writes a **two-parent commit** (`git cat-file -p HEAD` shows two `parent` lines) | `merge_ff_false_must_preview_the_two_parent_commit_git_actually_writes` |
//! | `only`, divergent | exits **128**, "Not possible to fast-forward, aborting", moves nothing | `merge_ff_only_must_not_draw_a_merge_git_refuses_to_make` |
//! | `banana` | **ignores it** and keeps the default — git does not barf on values from future versions | `merge_ff_set_to_an_unparseable_value_refuses_instead_of_defaulting` |
//!
//! The unset row is the one that is easiest to leave untested and the one that
//! matters most, because it is what nearly every repository is in. Every other
//! fast-forwardable shape in this file writes `merge.ff = true` into its own
//! config, so before its test existed, inverting
//! [`fast_forward_policy`]'s key-absent arm from `Allow` to `Never` left the
//! whole binary green (measured, 2026-08-30) — reintroducing the defect the
//! `merge.ff` round was about, undetected.
//!
//! The last row is the one place this module is deliberately **stricter than
//! git**: an unparseable value refuses rather than defaulting, so the user sees
//! no picture instead of a picture drawn from a value neither party understood.
//! That is a posture, recorded in ADR 0099, and its test is what stops someone
//! "fixing" it to default with a green suite.
//!
//! # No test in this file is carried red, and none may be deleted to keep it so
//!
//! **Six** tests here were carried red as findings while the arms they pin
//! were wrong: the two `merge_ff_` tests above, the already-applied
//! cherry-pick, `a2_a_cancelled_preview_leaves_nothing_behind`, and — added
//! 2026-08-31, red before the guard they pin existed —
//! `a_detached_head_refuses_rather_than_colouring_a_commit_no_branch_claims`
//! and `the_refusal_says_detached_only_when_head_really_is_detached`. All six
//! now pass, and each carries its own measurement plus the mutations that must
//! turn it red again, in its own doc comment.
//!
//! The detached-HEAD pair is documented as a pair rather than two independent
//! twos: **three** mutations run between them, and each was applied and
//! measured (2026-08-31, by hand — `failure-atlas` clones `HEAD`, and this
//! work is deliberately uncommitted while the parent session owns the index).
//! Deleting the guard reddens both, at different assertions; dropping
//! `is_branch()` from `added_claimed_by_no_branch` in
//! `git_vista_core::preview` reddens the first one *earlier*, at the witness
//! assertion, plus the core test; making the "HEAD is detached" sentence
//! unconditional reddens only the second, which is the one place the
//! difference between the two sentences is visible.
//!
//! A red test here is a finding, not a defect in the test. None may be deleted,
//! narrowed or `#[ignore]`d to make the suite green — that is exactly what
//! happened to the byte-level A2 tests once already.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
/// a fast-forward — **when `merge.ff` allows it**, which is why this fixture
/// pins it.
///
/// # Why `merge.ff = true` is written into the fixture's own config
///
/// `git_vista_fixtures::git` builds a fixture with `GIT_CONFIG_GLOBAL` and
/// `GIT_CONFIG_SYSTEM` pointed at `/dev/null`, but neither the preview nor the
/// executor spawns that way: `sandbox::spawn` passes `$HOME` through and grants
/// it read-only, so a developer's own `~/.gitconfig` reaches every `git` the
/// server runs. `merge.ff = false` is a common setting, and under it `git merge
/// --no-edit` on this shape writes a **two-parent commit** rather than
/// fast-forwarding (measured on this host, 2026-08-30). A fast-forward test
/// that did not pin the setting would therefore assert the wrong thing on such
/// a machine and the right thing on CI. Pinned locally, the shape means what
/// its name says everywhere.
fn fast_forward_shape() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    git::run(&repo, &["config", "merge.ff", "true"]);
    git::write(&repo, "a.txt", b"one\n");
    commit_old(&repo, "add a");
    git::run(&repo, &["branch", "behind"]);
    git::write(&repo, "b.txt", b"two\n");
    commit_old(&repo, "add b");
    (dir, repo)
}

/// [`revert_shape`] plus an independent branch tip, so the `after` window has
/// **two** topologically-ready commits and row order is decidable.
///
/// # Why the competitor is needed at all
///
/// `stable_topo_order` emits ready commits newest-first. In `revert_shape`'s
/// `after` window the hypothetical revert is the only ready commit, so it takes
/// row 0 whatever its timestamp says — a preview that stamped it at the epoch
/// would still be laid out identically and no assertion in this suite could
/// see it. `side` branches off the first commit and is dated [`LONG_AGO`], so
/// it is ready from the start and is the thing the revert has to be newer than.
fn revert_shape_with_a_competitor_tip() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    git::write(&repo, "a.txt", b"one\n");
    commit_old(&repo, "add a");

    git::run(&repo, &["checkout", "-q", "-b", "side"]);
    git::write(&repo, "side.txt", b"side\n");
    commit_old(&repo, "side work");

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "b.txt", b"two\n");
    commit_old(&repo, "add b");
    git::write(&repo, "c.txt", b"three\n");
    commit_old(&repo, "add c");
    (dir, repo)
}

/// `main` and `feature` diverged from a shared base, **plus** an independent
/// `side` tip — all dated [`LONG_AGO`] — so the merge commit's row is decided
/// by its timestamp rather than by being the only candidate.
///
/// Built here rather than taken from `git_vista_fixtures::merge_clean_two_branch`
/// for the reason in the module doc: that shape's commits are stamped *now*, so
/// a competitor tip added to it would tie with the hypothetical commit on time
/// and the oid tiebreak — which differs between the preview and the real run by
/// construction — would decide row 0.
fn merge_shape_with_a_competitor_tip() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    git::write(&repo, "shared.txt", b"base\n");
    commit_old(&repo, "base");

    git::run(&repo, &["checkout", "-q", "-b", "feature"]);
    git::write(&repo, "feature-one.txt", b"one\n");
    commit_old(&repo, "feature: one");

    git::run(&repo, &["checkout", "-q", "-b", "side", "main"]);
    git::write(&repo, "side.txt", b"side\n");
    commit_old(&repo, "side work");

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-alpha.txt", b"alpha\n");
    commit_old(&repo, "main: alpha");
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

/// Assert that a repository's row 0 is decided by the newest commit's
/// **timestamp** rather than by topology alone.
///
/// The check is direct: lay the repository out, rewrite the row-0 commit's
/// `time` to the epoch, lay it out again through the same function, and
/// require a *different* commit in row 0. If the same one stays there, the
/// window has only one topologically-ready commit and a preview that stamped
/// its hypothetical commit at time 0 would be laid out identically — which is
/// exactly the hole the two `..._row_is_decided_by_its_timestamp` tests exist
/// to close, and it would close nothing without this.
fn assert_row_zero_is_decided_by_time(repo: &Path) {
    let commits =
        git_vista_git::walk_history(repo, PREVIEW_HISTORY_LIMIT).expect("walk the history");
    let refs = git_vista_git::read_refs(repo).expect("read the refs");
    let head_branch = git_vista_git::read_head_branch(repo);

    let as_built = layout_with_refs(commits.clone(), refs.clone(), head_branch.as_deref());
    let newest = as_built.rows[0].commit.id.clone();

    let restamped: Vec<CommitSummary> = commits
        .into_iter()
        .map(|mut c| {
            if c.id == newest {
                c.time = 0;
            }
            c
        })
        .collect();
    let with_epoch = layout_with_refs(restamped, refs, head_branch.as_deref());

    assert_ne!(
        with_epoch.rows[0].commit.id,
        newest,
        "row 0 held {} whether its time was the real one or the epoch, so this \
         shape cannot tell a correctly-stamped preview from one stamped at 0",
        newest.short()
    );
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

/// The repository path handed to [`a2_env_redirect_driver`]'s child process.
const A2_ENV_REPO_VAR: &str = "GV_A2_ENV_REDIRECT_REPO";
/// The managed root handed to the same child process.
///
/// The driver runs in a **fresh process** and knows only what it is told, so
/// it is handed the root rather than inventing one. A test that guessed a root
/// would be validating containment against a fact it made up.
const A2_ENV_ROOT_VAR: &str = "GV_A2_ENV_REDIRECT_ROOT";
/// The serialized [`Plan`] handed to the same child process.
const A2_ENV_PLAN_VAR: &str = "GV_A2_ENV_REDIRECT_PLAN";
/// The line the driver prints — only after the preview under the redirected
/// environment answered `Graph` — that the outer test requires verbatim.
/// Without it, a driver that ran nothing would hand the outer test a free
/// green.
const A2_ENV_SENTINEL: &str = "GV_A2_ENV_REDIRECT_OUTCOME=graph";

/// A byte-level manifest of **everything** under `commondir`: one sorted line
/// per entry, carrying the relative path, the file's length and a digest of its
/// contents.
///
/// # Why this and not the object count beside it
///
/// [`object_file_count`] answers A2's literal wording — "no new object under
/// `<commondir>/objects`" — and is blind to everything else in a `.git`: a ref
/// file rewritten in place at the same length, a `config` edited, a `logs/HEAD`
/// appended to, an `index` refreshed, or a scratch store that survived under a
/// name [`scratch_dirs`] does not recognise. Hashing every byte means "changed
/// nothing" is the claim being tested, rather than "did not change one
/// particular counter".
///
/// Directories are listed too (with a `/` suffix and no digest) so an empty
/// leftover directory is visible; an unreadable entry is recorded as such
/// rather than skipped, because a file this cannot read is exactly the kind of
/// thing that should show up in a diff instead of vanishing from both sides.
fn git_dir_manifest(commondir: &Path) -> Vec<String> {
    fn digest(bytes: &[u8]) -> u64 {
        use std::hash::{Hash, Hasher};
        // `DefaultHasher::new()` is documented as constructed with fixed keys,
        // so two manifests taken in one process are comparable. (`RandomState`,
        // which `HashMap` uses, is the seeded one — deliberately not that.)
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            out.push(format!(
                "{}/ <unreadable directory>",
                dir.strip_prefix(root).unwrap_or(dir).display()
            ));
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    out.push(format!("{rel}/"));
                    walk(root, &path, out);
                }
                Ok(t) if t.is_symlink() => {
                    let target = std::fs::read_link(&path)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|e| format!("<unreadable link: {e}>"));
                    out.push(format!("{rel} -> {target}"));
                }
                Ok(_) => match std::fs::read(&path) {
                    Ok(bytes) => out.push(format!(
                        "{rel} len={} hash={:016x}",
                        bytes.len(),
                        digest(&bytes)
                    )),
                    Err(e) => out.push(format!("{rel} <unreadable: {e}>")),
                },
                Err(e) => out.push(format!("{rel} <no file type: {e}>")),
            }
        }
    }
    let mut out = Vec::new();
    walk(commondir, commondir, &mut out);
    out.sort();
    out
}

/// The lines that differ between two [`git_dir_manifest`] snapshots, marked
/// `-` (gone) and `+` (arrived). Empty means byte-identical.
fn manifest_diff(before: &[String], after: &[String]) -> Vec<String> {
    let mut diff: Vec<String> = before
        .iter()
        .filter(|line| !after.contains(line))
        .map(|line| format!("- {line}"))
        .collect();
    diff.extend(
        after
            .iter()
            .filter(|line| !before.contains(line))
            .map(|line| format!("+ {line}")),
    );
    diff
}

/// How many `parent` lines `rev`'s raw commit object records — git's own view,
/// read the way `git_vista_fixtures::divergent` reads it, rather than inferred
/// from what a command was expected to do.
fn parent_count(repo: &Path, rev: &str) -> usize {
    git::out(repo, &["cat-file", "-p", rev])
        .lines()
        .take_while(|line| !line.is_empty())
        .filter(|line| line.starts_with("parent "))
        .count()
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
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();

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
    let outcome = preview(&target, &plan).await;

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
    let (dir, repo) = git_vista_fixtures::cherry_pick_conflict();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
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

    match preview(&target, &plan).await {
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

/// Take the two `git status` reads that *warm the index* out of the way before
/// a manifest is captured.
///
/// `git status` refreshes `.git/index` and rewrites it when a stat entry is
/// racy — which every file in a just-built fixture is, having been written in
/// the same second as the index. A manifest taken before that write and
/// compared with one taken after would report a changed `index` and blame the
/// preview for it. Two reads, because the first is the one that does the
/// rewriting and the second proves it has settled.
fn warm_the_index(repo: &Path) -> String {
    let first = git::out(repo, &["status", "--porcelain=v2", "--branch"]);
    let second = git::out(repo, &["status", "--porcelain=v2", "--branch"]);
    assert_eq!(first, second, "two consecutive status reads must agree");
    second
}

/// **A2, byte for byte.** A revert preview changes **no byte** anywhere under
/// the common git directory, and leaves the worktree where it found it.
///
/// # Why this exists beside the object count above
///
/// `a2_a_preview_writes_no_object_moves_no_ref_and_leaves_no_scratch_directory`
/// counts files under `<commondir>/objects` and snapshots the refs. That is A2's
/// literal wording and it is blind to a `config` edited, a `logs/HEAD`
/// appended to, a ref rewritten in place, or a leftover directory whose name
/// `scratch_dirs` does not match. [`git_dir_manifest`] hashes every byte of
/// every file, so the claim under test is the one the feature actually makes:
/// the served repository never learns the preview happened.
///
/// A stricter version of this test was written once, and deleted mid-session to
/// take the suite from red to green. It is back, and it is not to be softened
/// into a count.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `std::mem::forget` the `ScratchStore` (or
///    `TempDir::into_path`). The store's whole directory tree survives and the
///    diff names every file in it, dozens of `+` lines.
/// 2. **Weakens it** — drop the `objects/info/alternates` write in
///    `ScratchStore::new` and instead have the preview run `merge-tree` against
///    the real git dir. The scratch directory is still cleaned up, so mutation
///    1's `+` lines never appear; what goes red is the *changed* `objects/…`
///    entries, a `~`-shaped diff of pairs, in a different part of the message.
#[tokio::test]
async fn a2_a_revert_preview_changes_no_byte_under_the_git_directory() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;

    let worktree_before = warm_the_index(&repo);
    let before = git_dir_manifest(&commondir);
    assert!(
        before.len() > 20,
        "a real `.git` has far more than 20 entries; a manifest this small \
         means the walk found nothing and the diff below would be vacuous \
         (got {})",
        before.len()
    );

    let outcome = preview(&target, &plan).await;
    let after = git_dir_manifest(&commondir);
    let worktree_after = git::out(&repo, &["status", "--porcelain=v2", "--branch"]);

    // Unwrapped *after* the manifests are taken, so a preview that refused
    // still has its "nothing changed" checked — and so the reason is reported
    // before the byte diff, which would otherwise be the confusing failure.
    let (_graph, _changes) = expect_graph(outcome);
    let diff = manifest_diff(&before, &after);
    assert!(
        diff.is_empty(),
        "a revert preview changed bytes under the git directory:\n{}",
        diff.join("\n")
    );
    assert_eq!(
        worktree_before, worktree_after,
        "a preview must not touch the worktree either"
    );
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
}

/// **A2, byte for byte, merge.** The same claim on the merge leg, which creates
/// a two-parent commit in the scratch store and is the arm with the most git
/// steps.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `std::mem::forget` the store: the whole
///    scratch tree shows up as `+` lines.
/// 2. **Weakens it** — have `commit_tree` omit the `--git-dir=<scratch>` flag.
///    The commit is then written into the served repository's own object
///    database, no directory is left behind, and the diff is a handful of new
///    `objects/…` entries instead — the exact failure A2 exists to catch, in a
///    different shape from mutation 1.
#[tokio::test]
async fn a2_a_merge_preview_changes_no_byte_under_the_git_directory() {
    let (dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;

    warm_the_index(&repo);
    let before = git_dir_manifest(&commondir);
    let outcome = preview(&target, &plan).await;
    let after = git_dir_manifest(&commondir);

    let (_graph, _changes) = expect_graph(outcome);
    let diff = manifest_diff(&before, &after);
    assert!(
        diff.is_empty(),
        "a merge preview changed bytes under the git directory:\n{}",
        diff.join("\n")
    );
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
}

/// **A2, byte for byte, the conflict path.** A preview that ends in
/// `Conflict` returns early, after the store exists — the exit path most likely
/// to skip cleanup.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `std::mem::forget` the recipe (and so the
///    store) in `compute`'s `Conflict` arm. The scratch tree survives and the
///    diff names it.
/// 2. **Weakens it** — return the conflict from inside `merge_tree` before the
///    `Recipe` is dropped *and* leak the temp dir there. The store survives
///    too, but the outcome assertion above it still passes, so the failure
///    arrives at the diff rather than at the `match` — the same directory,
///    reached down a different path.
#[tokio::test]
async fn a2_a_conflicting_preview_changes_no_byte_under_the_git_directory() {
    let (dir, repo) = git_vista_fixtures::cherry_pick_conflict();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic).expect("a full hex oid"),
        },
    )
    .await;

    warm_the_index(&repo);
    let before = git_dir_manifest(&commondir);
    let outcome = preview(&target, &plan).await;
    let after = git_dir_manifest(&commondir);

    assert!(
        matches!(outcome, PreviewOutcome::Conflict { .. }),
        "expected Conflict, got {outcome:?}"
    );
    let diff = manifest_diff(&before, &after);
    assert!(
        diff.is_empty(),
        "a conflicting preview changed bytes under the git directory:\n{}",
        diff.join("\n")
    );
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
}

/// **A2, the environment.** An inherited `GIT_OBJECT_DIRECTORY` must not
/// redirect the preview's writes into the served repository's own object
/// database.
///
/// # Why this is a real inheritance and not an attack scenario
///
/// The sealed launcher deliberately passes the server's environment through
/// (`sandbox::spawn`), and git honours `GIT_OBJECT_DIRECTORY` as the primary
/// object database **regardless of `--git-dir`** — so every object-writing
/// step in this module (`merge-tree --write-tree`, `commit-tree`) lands its
/// objects wherever that variable points, not in the scratch store its argv
/// named. Nothing hostile is required: git itself exports exactly this
/// variable (with `GIT_ALTERNATE_OBJECT_DIRECTORIES`) into hooks during its
/// receive-pack quarantine, so a server launched from inside a hook inherits
/// the bypass by construction. Pointing it at the served repository's own
/// `objects/` is the shape an independent audit of this branch ran on
/// 2026-08-31: real object files went 2 → 3, the scratch store stayed at 0,
/// and the "hypothetical" commit was readable from the real ODB. No ref moved,
/// and A2 was violated anyway.
///
/// # Why the preview runs in a second process
///
/// The redirect must reach the preview's spawns through *inheritance* — that
/// is the defect — and the only way to inherit is to be in the environment of
/// the process that spawns. An earlier version of this test set the variable
/// process-wide (mutex-guarded, restore-on-drop, the `SSH_AUTH_SOCK_LOCK`
/// discipline) and it was not shippable: measured 2026-08-31 in the parallel
/// binary, sibling tests' *fixture builders* — raw unsandboxed `git`
/// commands — inherited the variable during the window, wrote **22 foreign
/// objects** into this test's ODB (turning its own assertion falsely red) and
/// broke three sibling tests whose repositories lost their objects to the
/// redirect. A lock only serializes the tests that take it.
///
/// So the redirected environment lives in a **child process**: this test
/// re-executes the test binary, running only [the ignored driver below] with
/// `GIT_OBJECT_DIRECTORY` set on that child's `Command` alone. The parent's
/// environment is never touched, so no sibling can inherit anything. The
/// driver runs the preview and prints [`A2_ENV_SENTINEL`] only if it answered
/// `Graph`; the sentinel is required here, so a driver that ran nothing (or
/// refused) cannot hand this test a vacuous green. Fixture, plan and every
/// snapshot stay in this process, outside the redirect.
///
/// # Two mutations that make this red, failing differently
///
/// The invariant — inherited repository-geometry environment cannot redirect a
/// sandboxed git — is pinned by this test *and* by
/// `sandbox::spawn`'s `the_launcher_scrubs_gits_repository_geometry_environment`,
/// and the two mutations split across the pair deliberately:
///
/// * **M1 — REMOVES the mechanism where it bites.** Delete
///   `"GIT_OBJECT_DIRECTORY"` from `SCRUBBED_GIT_GEOMETRY_ENV` in
///   `sandbox::spawn`. The preview writes its merge trees and hypothetical
///   commit into the served ODB again and **this** test goes red at the
///   object-count assertion, with the structural test red beside it.
/// * **M2 — WEAKENS the family.** Delete `"GIT_INDEX_FILE"` from the same
///   list. This test stays green — the preview never touches an index — and
///   only the structural test goes red, naming the missing variable. That
///   split is the point: a behavioural test alone would let the family erode
///   one unexercised variable at a time.
#[tokio::test]
async fn a2_an_inherited_git_object_directory_cannot_redirect_preview_writes() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;

    let objects_before = object_file_count(&commondir);
    let refs_before = refs_snapshot(&repo);
    assert!(
        objects_before > 0,
        "the fixture must actually have objects, or the count proves nothing"
    );

    let plan_json = serde_json::to_string(&plan).expect("a Plan serializes");
    // The re-exec `Command` lives in the fixtures crate, not here:
    // `argv_boundary`'s scan rightly refuses any non-`git` spawn in this
    // crate, and the fixture layer is the established unsandboxed test-support
    // trust level. See `git_vista_fixtures::reexec`'s module doc.
    let output = git_vista_fixtures::reexec::run_ignored_test(
        "preview::suite::a2_env_redirect_driver_runs_one_preview_under_the_variable",
        &[
            (
                "GIT_OBJECT_DIRECTORY",
                commondir.join("objects").as_os_str(),
            ),
            (A2_ENV_REPO_VAR, repo.as_os_str()),
            (A2_ENV_ROOT_VAR, dir.path().as_os_str()),
            (A2_ENV_PLAN_VAR, std::ffi::OsStr::new(&plan_json)),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the driver process failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(A2_ENV_SENTINEL),
        "the driver never reported a Graph outcome, so no preview ran under \
         the redirected environment and the assertions below would be \
         vacuous:\n{stdout}"
    );

    assert_eq!(
        object_file_count(&commondir),
        objects_before,
        "an inherited GIT_OBJECT_DIRECTORY redirected the preview's writes \
         into the served repository's own object database — A2's 'the real \
         repository is unchanged' does not say 'unless an environment \
         variable was set'"
    );
    assert_eq!(
        refs_snapshot(&repo),
        refs_before,
        "a preview must move no ref, environment notwithstanding"
    );
    assert_eq!(
        scratch_dirs(&commondir),
        Vec::<String>::new(),
        "the scratch store must be gone whatever the environment said"
    );
}

/// The driver half of
/// [`a2_an_inherited_git_object_directory_cannot_redirect_preview_writes`] —
/// not a test of its own, which is what the `#[ignore]` says. The outer test
/// re-executes this binary to run exactly this function in a process whose
/// environment carries `GIT_OBJECT_DIRECTORY`; here it only deserializes the
/// plan it was handed, runs the one preview, and prints [`A2_ENV_SENTINEL`]
/// if — and only if — the answer was `Graph`.
///
/// Under a bare `cargo test -- --include-ignored` there is no harness, so
/// this returns quietly instead of failing someone's full run. That is not a
/// vacuous-green hazard: the invariant lives in the outer test, and a run
/// that drove nothing prints no sentinel, which the outer test refuses.
#[tokio::test]
#[ignore = "driver for a2_an_inherited_git_object_directory_cannot_redirect_preview_writes; runs in its own process"]
async fn a2_env_redirect_driver_runs_one_preview_under_the_variable() {
    let Some(repo) = std::env::var_os(A2_ENV_REPO_VAR) else {
        eprintln!("{A2_ENV_REPO_VAR} unset; this driver only means something when the outer test spawns it");
        return;
    };
    let repo = PathBuf::from(repo);
    let root = PathBuf::from(
        std::env::var_os(A2_ENV_ROOT_VAR).expect("the outer test passes the managed root"),
    );
    let target = PreviewTarget::resolved_in(&repo, &root)
        .expect("the handed repository is inside the handed root");
    let plan_json = std::env::var(A2_ENV_PLAN_VAR).expect("the outer test passes the plan");
    let plan: Plan = serde_json::from_str(&plan_json).expect("the handed plan deserializes");
    assert!(
        std::env::var_os("GIT_OBJECT_DIRECTORY").is_some(),
        "the outer test sets the redirect on this process; without it this \
         driver would measure nothing"
    );

    match preview(&target, &plan).await {
        PreviewOutcome::Graph { .. } => println!("{A2_ENV_SENTINEL}"),
        other => panic!("expected Graph under the redirected environment, got {other:?}"),
    }
}

/// **A2, cancellation.** A preview whose future is *dropped* part-way through
/// leaves nothing behind either.
///
/// # What "leaves nothing behind" means here, and why it is not "within 150 ms"
///
/// The defect this test was written for is fixed: `ScratchStore::new` used to
/// await `git init --bare <scratch>` through a spawn that detached its child on
/// drop, so cancelling ran `TempDir::drop`, removed the directory, and let the
/// unsignalled orphan write the whole store straight back inside the served
/// `.git`, where it survived until `sweep_stale` found it an hour later.
/// `preview` now runs its work in a detached task and bails at the first
/// checkpoint *after* an awaited spawn, so nothing is removed while a `git` is
/// still writing into it and nothing is left once the step returns.
///
/// **The bound this test used to assert was the wrong contract, and it made the
/// test flaky rather than strict.** It slept a fixed 150 ms after each
/// cancellation and called anything still on disk a leak. But the residue
/// window is not a constant — it is the length of the *spawn that was in
/// flight*, and `preview.rs`'s own doc records individual `git init --bare`
/// calls on this host at **128 ms** and **1.16 s**. Measured 2026-08-30, this
/// file: run alone the test passed 5 times out of 5, and run inside the full
/// 1078-test binary it failed 3 times out of 3 — every failure the same shape,
/// a store holding `HEAD`, `config`, `refs/heads/` and `refs/tags/` and
/// **no `objects/`**, which is `git init` part-way through its own work (it
/// creates the ref directories, `HEAD` and `config` before `objects/`), not a
/// store anybody abandoned. With the wait below in place the same full-suite
/// run is green — 4 runs of 4 — and the slowest cancellation in each cleared in
/// **350.8 ms, 155.5 ms, 461.7 ms and 174.3 ms**. Every one of those is over
/// the old fixed bound, and every one is a factor of twenty inside the
/// ceiling; the run this test prints its own figure on each time, so the margin
/// is visible rather than asserted.
///
/// So the test now waits for the repository to settle
/// ([`wait_for_the_repository_to_settle`], floor [`SETTLE_FLOOR`], ceiling
/// [`SETTLE_CEILING`]) and asserts what its name says: **nothing survives**.
/// That is strictly the A2 criterion. It is not a weakening — every way of
/// breaking the cleanup produces residue that never clears, so the ceiling is
/// reached and the assertion fires; what the fixed sleep added was a race with
/// the machine's load, which is not a property of `preview.rs`.
///
/// # The case this test does NOT cover, named rather than implied
///
/// A partially-built store exists inside the served `.git` for the life of the
/// in-flight spawn, and if the tokio **runtime itself** is torn down mid-task
/// the task is dropped where it stands and that store survives. Neither is
/// covered here: this test drives cancellation, not shutdown. ADR 0099 records
/// the teardown case as an open consequence in the same class as `SIGKILL` and
/// power loss, and records the transient window as a stated cost of the design.
///
/// *Corrected 2026-08-31 (audit findings 2/3):* this comment used to add
/// "`ScratchStore::sweep_stale` is what covers it". That is no longer true and
/// it contradicted `preview.rs`'s own statement at the `preview` entry point.
/// The sweep now reclaims only directories carrying `STORE_MARKER`, and the
/// teardown residue has none — `TempDir::drop` removed the store and an
/// unsignalled orphan `git init` wrote it back, and git does not write our
/// marker. `SIGKILL` and power loss are still covered, because those leave the
/// marker on disk with the whole store; teardown-during-a-spawn is the one
/// member of that class that is not.
///
/// **The window is a few milliseconds wide, and the sweep is aimed at it
/// deliberately.** A geometric ladder over the whole call (200 µs, 500 µs,
/// 1 ms, … 256 ms) — which is how this was first written — reproduces
/// **nothing**: every rung lands either before the store exists or after the
/// preview has cleaned up after itself. That version of this test was green
/// while the defect was present, which is the exact failure this suite exists
/// to stop making. A 2 ms sweep hit it on one rung of thirty. So the sampler
/// below measures when a store first appears and the sweep steps through that
/// region in **0.5 ms** increments, starting well before it.
///
/// # Two instruments, both proved non-inert before they are trusted
///
/// The verdict is "nothing was left behind", and a detector that could not see
/// a leftover store would return that verdict for free. So a store is
/// **planted** first and both `scratch_dirs` and `git_dir_manifest` are
/// required to name it, then it is removed. And the sampler establishes that a
/// store is genuinely observable mid-flight — otherwise a cancellation that
/// never got that far would leave nothing behind for the trivial reason that
/// nothing was ever there. The sampler writes what it saw into shared state
/// rather than returning it, because an aborted `JoinHandle` yields a
/// `JoinError` and anything carried in its return value is lost. (The earlier
/// draft of this test did return it, and would have failed on its own sampler
/// assertion whatever the code under test did.)
///
/// # Two mutations, both run and both caught (2026-08-30, in a scratch clone)
///
/// 1. **Removes the detachment** — replace `preview`'s
///    `tokio::spawn(...)` + `task.await` with a direct `compute(...).await`, so
///    the caller's future owns the work again. That is the original defect
///    exactly: dropping it runs `TempDir::drop` mid-`git init` and the
///    unsignalled orphan writes the store straight back. Caught at the first
///    rung, 35.6 ms, with a `gv-preview-*` directory still present after
///    [`SETTLE_CEILING`].
/// 2. **Removes the removal** — return the store from `ScratchStore::new` as a
///    `PathBuf` via `TempDir::into_path`, so nothing ever deletes it. No orphan
///    and no signal is involved; the directory simply never goes away. Also
///    caught at the first rung, and with **two** surviving directories rather
///    than one, because every completed preview leaks as well.
///
/// The two break different mechanisms — a removal that races a live child, and
/// a removal that never happens — and only the second is visible to a completed
/// call. They land on the same assertion because this test has one verdict; the
/// instruments that could fail separately (the planted detector, the sampler)
/// are checked before it and are what stop that verdict being free.
///
/// # One mutation that SURVIVES, and why that is not a hole in this test
///
/// Swapping `preview_git` for `git_cmd::git_output_bounded` — kill-on-drop, the
/// change that suggests itself as *the fix* — leaves this test green: measured
/// 2026-08-30, 3 of 3 runs alone and the whole 1080-test binary green with it
/// in place. That is correct, and it is worth stating rather than papering
/// over. `preview` detaches its task, so a cancelled caller never drops a
/// child and `kill_on_drop` has nothing to fire on *in this path*. What it does
/// fire on is runtime teardown, which this test does not drive.
///
/// **The consequence is a correction to a citation, not to this test.**
/// `preview_git`'s own doc comment carries a table — `git_output_bounded` "0 of
/// 5 runs green" against `git_output`'s "12 of 13" — offered as evidence that
/// kill-on-drop is strictly worse here. Those numbers were taken against this
/// test's *old* fixed 150 ms settle, and under a wait that lets the in-flight
/// spawn finish they do not reproduce: both arities leave nothing behind. The
/// argument for `git_output` still stands on the teardown case; the measurement
/// quoted for it no longer does.
#[tokio::test]
async fn a2_a_cancelled_preview_leaves_nothing_behind() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;
    warm_the_index(&repo);
    let before = git_dir_manifest(&commondir);

    // ---- the detector, proved non-inert before it is trusted --------------
    //
    // This test's whole verdict is "nothing was left behind". A detector that
    // could not see a leftover store would return that verdict for free. So
    // one is planted, both instruments are required to name it, and it is
    // removed again.
    let planted = commondir.join(format!("{SCRATCH_PREFIX}planted-detector-check"));
    std::fs::create_dir_all(planted.join("objects")).expect("plant a store");
    std::fs::write(planted.join("HEAD"), b"ref: refs/heads/main\n").expect("plant a file");
    assert_eq!(
        scratch_dirs(&commondir),
        vec![format!("{SCRATCH_PREFIX}planted-detector-check")],
        "`scratch_dirs` must see a planted store, or its silence below means \
         nothing"
    );
    assert!(
        !manifest_diff(&before, &git_dir_manifest(&commondir)).is_empty(),
        "the manifest must see a planted store, or its silence below means \
         nothing"
    );
    std::fs::remove_dir_all(&planted).expect("remove the planted store");
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
    assert_eq!(
        manifest_diff(&before, &git_dir_manifest(&commondir)),
        Vec::<String>::new()
    );

    // ---- when, within a preview, is a store on disk? ----------------------
    let seen: Arc<Mutex<Vec<(u128, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let started = std::time::Instant::now();
    {
        let seen = Arc::clone(&seen);
        let watched = commondir.clone();
        let sampler = tokio::spawn(async move {
            loop {
                for dir in scratch_dirs(&watched) {
                    let mut seen = seen.lock().expect("the sampler's lock");
                    if !seen.iter().any(|(_, d)| *d == dir) {
                        seen.push((started.elapsed().as_micros(), dir));
                    }
                }
                tokio::time::sleep(Duration::from_micros(50)).await;
            }
        });
        let _ = preview(&target, &plan).await;
        sampler.abort();
        let _ = sampler.await;
    }
    let whole_preview = started.elapsed();
    let observed = seen.lock().expect("the sampler's lock").clone();
    println!("a whole preview took {whole_preview:?}; the sampler saw {observed:?}");
    assert!(
        !observed.is_empty(),
        "the sampler never saw a scratch store mid-flight, so the cancellation \
         sweep below would be cancelling nothing that could leak one"
    );
    let first_seen =
        Duration::from_micros(u64::try_from(observed[0].0).expect("a sane elapsed time"));

    // ---- cancel across the window where a store exists --------------------
    //
    // The leak needs the cancellation to land while `git init` is the
    // outstanding await, and that window is only a few milliseconds wide:
    // measured on this host, a sweep in 2 ms steps hit it on **one** of its 30
    // rungs, and the geometric ladder this test was first written with
    // (200 µs, 500 µs, 1 ms, … 256 ms) hit it on none at all and reported the
    // repository clean while the defect was present. So the steps are 0.5 ms
    // and the band straddles the moment the sampler first saw a store.
    let mut timeouts: Vec<Duration> = Vec::new();
    let base = first_seen.saturating_sub(Duration::from_millis(14));
    for step in 0..60u32 {
        timeouts.push(base + Duration::from_micros(u64::from(step) * 500));
    }
    // A coarse safety net for a leak that would need a much earlier or much
    // later cancellation than the `git init` window. Kept deliberately, and
    // stated honestly: **these four do not fire on this host.** They are here
    // so a different failure shape is not missed by a sweep tuned to one, not
    // as evidence of anything today.
    for micros in [200u64, 2_000, 16_000, 512_000] {
        timeouts.push(Duration::from_micros(micros));
    }

    let mut leaked: Vec<(Duration, Vec<String>, Vec<String>)> = Vec::new();
    let mut slowest_clear = Duration::ZERO;
    for limit in timeouts {
        let completed = tokio::time::timeout(limit, preview(&target, &plan))
            .await
            .is_ok();
        let (settled_after, dirs, diff) =
            wait_for_the_repository_to_settle(&commondir, &before).await;
        slowest_clear = slowest_clear.max(settled_after);
        println!(
            "cancelled at {limit:?}: completed={completed} settled_after={settled_after:?} \
             dirs={} diff={}",
            dirs.len(),
            diff.len()
        );
        // Sweep the residue away before the next iteration, so an entry in
        // `leaked` names the cancellation that actually produced it rather
        // than inheriting the previous one's leftovers. This is the test
        // tidying up after the code under test; it is not a fix.
        for dir in &dirs {
            let _ = std::fs::remove_dir_all(commondir.join(dir));
        }
        if !dirs.is_empty() || !diff.is_empty() {
            leaked.push((limit, dirs, diff));
            // One reproduction is the whole verdict, and stopping here keeps a
            // failing run short. A run that finds nothing does the full sweep.
            break;
        }
    }
    println!(
        "the slowest cancellation took {slowest_clear:?} to leave the repository clean \
         (floor {SETTLE_FLOOR:?}, ceiling {SETTLE_CEILING:?})"
    );
    assert!(
        leaked.is_empty(),
        "a cancelled preview left residue inside the served repository after \
         {SETTLE_CEILING:?} (timeout, surviving directories, byte diff):\n{leaked:#?}"
    );
}

/// How long [`wait_for_the_repository_to_settle`] waits before it first looks.
///
/// A floor, not a guess. Cancelling at 200 µs drops the caller's future *before*
/// the detached task has created a store at all, so a check taken immediately
/// would find the repository clean for the trivial reason that nothing had
/// happened yet — and would then miss a store created 5 ms later. 150 ms is the
/// value this test used as its whole settle period before it learned to wait,
/// and it is kept as the floor for exactly the coverage it was giving.
const SETTLE_FLOOR: Duration = Duration::from_millis(150);

/// How long [`wait_for_the_repository_to_settle`] keeps waiting after the floor
/// before it calls what it sees a leak.
///
/// Sized off the spawn, because the residue window *is* the spawn's length:
/// `preview.rs`'s own doc records individual `git init --bare` calls taking
/// **128 ms** and **1.16 s** on this host, and this suite runs 4-way parallel
/// with 1077 other tests. Ten seconds is roughly eight times the slowest
/// measured init and still finite, so a store that is genuinely abandoned —
/// which is what every mutation of the cleanup mechanism produces — is reported
/// rather than waited on forever.
const SETTLE_CEILING: Duration = Duration::from_secs(10);

/// Wait for a cancelled preview's residue to disappear, and report how long
/// that took.
///
/// Returns `(elapsed, surviving directories, byte diff)`. An empty pair means
/// the served repository is byte-identical to `before` again; a non-empty one
/// means it still was not after [`SETTLE_CEILING`], which is the leak.
///
/// # Why this waits instead of sleeping a fixed 150 ms
///
/// Because "150 ms" is not the contract and never was — see
/// [`a2_a_cancelled_preview_leaves_nothing_behind`]'s doc comment for the
/// measurement that made that concrete.
async fn wait_for_the_repository_to_settle(
    commondir: &Path,
    before: &[String],
) -> (Duration, Vec<String>, Vec<String>) {
    let started = std::time::Instant::now();
    tokio::time::sleep(SETTLE_FLOOR).await;
    loop {
        let dirs = scratch_dirs(commondir);
        let diff = manifest_diff(before, &git_dir_manifest(commondir));
        if dirs.is_empty() && diff.is_empty() {
            return (started.elapsed(), dirs, diff);
        }
        if started.elapsed() >= SETTLE_CEILING {
            return (started.elapsed(), dirs, diff);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// **A2, linked worktree.** A preview driven from a linked worktree changes no
/// byte of the **main** git directory.
///
/// Nobody else raised this case, and it is the one where the scratch store is
/// furthest from the repository the caller handed in: a linked worktree's
/// validated commondir is the *main* `.git`, so the store — and therefore the
/// cleanup — happens somewhere the caller never named.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `std::mem::forget` the store: the leftover
///    tree appears in the main `.git`, which is the worst place for it, and the
///    diff and the `scratch_dirs` assertion both name it.
/// 2. **Weakens it** — resolve the store's home with a second
///    `rev-parse --git-dir` instead of `sandbox::repo_paths::resolve`. That
///    answers `<main>/.git/worktrees/wt`, not the commondir, so the store is
///    created outside the read-write grant the policy built and `git init`
///    fails; the outcome stops being a `Graph` and `expect_graph` names the
///    reason, leaving the byte diff untouched.
#[tokio::test]
async fn a2_a_preview_in_a_linked_worktree_touches_the_main_git_directory_not_at_all() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let main_commondir = target.commondir().to_path_buf();

    let worktree = repo
        .parent()
        .expect("the fixture repository has a parent directory")
        .join("wt");
    git::run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wtbranch",
            worktree.to_str().expect("a utf-8 path"),
        ],
    );
    // The worktree is created at `<root>/wt`, a sibling of the repository and
    // still inside the fixture root, so it passes the same containment check
    // production applies.
    let worktree_target = PreviewTarget::resolved_in(&worktree, dir.path())
        .expect("the linked worktree is inside the fixture root");
    let worktree_commondir = worktree_target.commondir().to_path_buf();
    assert_eq!(
        worktree_commondir, main_commondir,
        "a linked worktree must resolve to the MAIN common directory, or this \
         test is watching a directory the preview was never going to use"
    );

    let head = git::out(&worktree, &["rev-parse", "HEAD"]);
    let plan = plan_for(
        &worktree,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;

    warm_the_index(&worktree);
    let before = git_dir_manifest(&main_commondir);
    let outcome = preview(&worktree_target, &plan).await;
    let after = git_dir_manifest(&main_commondir);

    let (_graph, _changes) = expect_graph(outcome);
    let diff = manifest_diff(&before, &after);
    assert!(
        diff.is_empty(),
        "a preview run from a linked worktree changed bytes in the MAIN git \
         directory:\n{}",
        diff.join("\n")
    );
    assert_eq!(
        scratch_dirs(&main_commondir),
        Vec::<String>::new(),
        "no store may be left in the main git directory"
    );
}

/// The two instruments the A2 byte tests rest on both see a leftover scratch
/// store — proved here once, permanently, rather than assumed by four tests
/// whose verdict is "nothing changed".
///
/// A detector that could not see a leak returns that verdict for free, and a
/// suite of such tests is green for the same reason a correct one is. So a
/// store is planted with the name and the shape `ScratchStore::new` produces,
/// both instruments are required to name it, and it is removed again.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — have [`git_dir_manifest`] return
///    `Vec::new()`. The manifest assertion goes red; `scratch_dirs` still
///    passes, so the two are not one instrument wearing two hats.
/// 2. **Weakens it** — have `git_dir_manifest` record only the relative path
///    and drop the length and digest. The planted *directory* is still seen, so
///    this test stays green — and `a2_a_revert_preview_changes_no_byte_…` would
///    stop noticing a file rewritten in place, which is why the digest is in
///    the manifest and why this pair is written as "remove" and "narrow"
///    rather than as two removals.
#[test]
fn the_manifest_and_the_scratch_sweep_both_notice_a_planted_store() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let before = git_dir_manifest(&commondir);
    assert!(
        before.len() > 20,
        "a real `.git` has far more than 20 entries; got {}",
        before.len()
    );

    let planted = commondir.join(format!("{SCRATCH_PREFIX}planted"));
    std::fs::create_dir_all(planted.join("objects").join("info")).expect("plant a store");
    std::fs::write(planted.join("HEAD"), b"ref: refs/heads/main\n").expect("plant a file");

    assert_eq!(
        scratch_dirs(&commondir),
        vec![format!("{SCRATCH_PREFIX}planted")],
        "`scratch_dirs` must name a directory carrying the prefix the module \
         itself uses"
    );
    let diff = manifest_diff(&before, &git_dir_manifest(&commondir));
    assert!(
        diff.contains(&format!("+ {SCRATCH_PREFIX}planted/")),
        "the manifest must name the planted directory itself: {diff:?}"
    );
    assert!(
        diff.iter()
            .any(|line| line.starts_with(&format!("+ {SCRATCH_PREFIX}planted/HEAD len=21 hash="))),
        "the manifest must carry each file's length and a digest of its \
         contents, or a file rewritten in place at the same length is \
         invisible to it: {diff:?}"
    );

    std::fs::remove_dir_all(&planted).expect("remove the planted store");
    assert_eq!(scratch_dirs(&commondir), Vec::<String>::new());
    assert_eq!(
        manifest_diff(&before, &git_dir_manifest(&commondir)),
        Vec::<String>::new(),
        "with the plant removed the manifest must be byte-identical again, or \
         it is reporting noise and every A2 diff above is unreadable"
    );
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
    let (dir, repo) = git_vista_fixtures::cherry_pick_conflict();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic).expect("a full hex oid"),
        },
    )
    .await;

    match preview(&target, &plan).await {
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
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
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
        match preview(&target, &plan).await {
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
    let (dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
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
    match preview(&target, &plan).await {
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
        // The badges beside the row. The hypothetical commit's own refs name
        // the hypothetical oid, so the targets go through `map` exactly as the
        // parent list does; everything else — the name, the kind, the order —
        // must match outright, because those are what the user reads.
        let predicted_refs: Vec<GitRef> = predicted
            .refs
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r.target = map(&r.target);
                r
            })
            .collect();
        assert_eq!(
            predicted_refs, actual.refs,
            "{what}: row {} carries different ref badges",
            actual.row
        );
        assert_eq!(
            predicted.color,
            actual.color,
            "{what}: commit {} is painted with a different palette slot",
            actual.commit.id.short()
        );
    }
    assert_eq!(
        after.lane_count, real.lane_count,
        "{what}: the gutter widths differ"
    );

    // ---- the two non-trivial facts the comparisons above rest on ----------
    //
    // Both are asserted about the ORACLE (`real`), never about the preview: an
    // equality between two empty vectors passes while the mechanism it claims
    // to check is gone, which is exactly how "dropping every edge from both
    // graphs" survived this function before.
    assert!(
        real.rows.len() > 1,
        "{what}: a one-row layout cannot show a placement mistake — the \
         fixture is too small to prove anything"
    );
    assert!(
        !real.edges.is_empty(),
        "{what}: the real layout drew no edges at all, so comparing edge sets \
         would compare two empty vectors and pass whatever the preview did"
    );
    assert!(
        real.rows.iter().any(|r| !r.refs.is_empty()),
        "{what}: no row in the real layout carries a ref badge, so the badge \
         comparison above is vacuous"
    );
    assert_eq!(
        after.edges, real.edges,
        "{what}: the lines drawn between rows differ — edges are (row, lane) \
         pairs on both sides, so they compare directly and a missing or \
         misrouted connector shows up here"
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
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&target, &plan).await);

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
    let (dir, repo) = cherry_pick_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::CherryPick {
            commit: CommitOid::new(topic.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&target, &plan).await);

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
    let (dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&target, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["merge", "-q", "--no-edit", "feature"]);
    let real = layout_of(&copy);

    assert_parity(&graph.after, &real, &before_layout, "merge");
}

/// The tree `merge-tree` computes for `op`, through the **same**
/// `previewable` → `resolve_plumbing` → `merge_tree` path [`compute`] takes.
///
/// Recomputed here rather than read out of the returned outcome because the
/// scratch store is dropped by the time `preview()` returns and
/// `PreviewOutcome` carries no tree oid. That limit is real and is stated in
/// the module doc: this pins the recipe, not the wiring between the recipe and
/// `commit_tree`.
async fn predicted_tree(target: &PreviewTarget, op: &GitOperation) -> String {
    let repo = target.repo();
    let head = git::out(repo, &["rev-parse", "HEAD"]);
    let previewable = previewable(op).expect("the operation must be previewable");
    let plumbing = resolve_plumbing(target, &previewable, &head)
        .await
        .expect("resolve the plumbing");
    let Plumbing::Synthesize(recipe) = plumbing else {
        panic!("this operation must synthesize a commit for its tree to exist");
    };
    match merge_tree(repo, &recipe).await.expect("merge-tree ran") {
        MergeTreeAnswer::Clean { tree } => tree,
        other => panic!("expected a clean merge, got {other:?}"),
    }
}

/// The previewed tree is the tree the real command writes — and the real
/// command's tree is not HEAD's.
///
/// # Why the second half is the load-bearing one
///
/// `assert_eq!(predicted, real)` is satisfied when **both** are simply HEAD's
/// own tree, which is exactly what "a revert that reverts nothing" and "a merge
/// that merges nothing" produce. Verified by mutation: with that pairing
/// missing, a preview computing no merge at all passed every test in this file.
/// So the oracle is checked for non-triviality first, against HEAD's tree read
/// before anything ran.
///
/// A tree is the one value comparable across the two runs: it hashes content,
/// while the commit that wraps it hashes the time it was written.
async fn assert_tree_matches_the_real_run(
    target: &PreviewTarget,
    op: &GitOperation,
    real_command: &[&str],
    what: &str,
) {
    let repo = target.repo();
    let head_tree_before = git::out(repo, &["rev-parse", "HEAD^{tree}"]);
    let predicted = predicted_tree(target, op).await;

    let (_scratch, copy) = copy_of(repo);
    git::run(&copy, real_command);
    let real_tree = git::out(&copy, &["rev-parse", "HEAD^{tree}"]);

    assert_ne!(
        real_tree,
        head_tree_before,
        "{what}: `git {}` left HEAD's tree untouched, so comparing a predicted \
         tree against it would pass for a preview that computed nothing — this \
         fixture cannot prove what the test claims",
        real_command.join(" ")
    );
    assert_eq!(
        predicted,
        real_tree,
        "{what}: the previewed tree must be the tree `git {}` actually writes",
        real_command.join(" ")
    );
}

/// **A5, revert, content.** The previewed revert really removes what the
/// reverted commit added.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — set the revert recipe's `theirs` to `head`
///    instead of the target's parent. The three-way merge then has nothing to
///    apply, `merge-tree` answers HEAD's own tree, and the `assert_eq!` goes
///    red naming two different tree oids.
/// 2. **Weakens it** — set `merge_base: None` so git computes its own base.
///    A tree is still produced and the graph shape is unchanged, but it is the
///    tree of a merge rather than of a revert, so the same equality goes red
///    with a *different* pair of oids — and the shape tests all stay green,
///    which is the point of having this test at all.
#[tokio::test]
async fn a5_a_previewed_revert_writes_the_tree_a_real_revert_writes() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let op = GitOperation::RevertCommit {
        commit: CommitOid::new(head.clone()).expect("a full hex oid"),
    };
    assert_tree_matches_the_real_run(&target, &op, &["revert", "--no-edit", &head], "revert").await;
}

/// The cherry-pick really carries the picked commit's *content* across, not
/// just its shape.
///
/// The shape comparison above cannot see this: a preview that used the picked
/// commit as its own merge base would produce a commit with the right parents
/// in the right lane whose tree was simply HEAD's.
///
/// # Why the catalogue's `cherry_pick_clean` and not this file's own shape
///
/// This test compares **trees**, never a layout, so the dating hazard that
/// makes the suite build its own shapes for the parity tests (see the module
/// doc) does not apply — and the catalogue shape is the better instrument
/// here. It proves on a disposable clone that a real `git cherry-pick` applies,
/// and it is built so its merged tree is provably *not* `main`'s own tree,
/// which is exactly the non-triviality
/// [`assert_tree_matches_the_real_run`] checks. It is also the half of a
/// deliberate pair: [`git_vista_fixtures::cherry_pick_already_applied`], used
/// by `a_cherry_pick_that_is_already_applied_must_not_be_drawn_as_a_clean_commit`
/// below, is the other, and a tree comparison asserted against only one of the
/// two would still pass an implementation that always answered "different".
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — use the picked commit itself as
///    `merge_base`. The pick contributes nothing, the predicted tree is HEAD's,
///    and the equality goes red.
/// 2. **Weakens it** — swap `ours` and `theirs`. A real three-way merge still
///    runs and still answers a tree, but it is the reverse merge's tree, so the
///    equality goes red on a different oid than mutation 1 produces.
#[tokio::test]
async fn a5_cherry_pick_actually_moves_the_content() {
    let (dir, repo) = git_vista_fixtures::cherry_pick_clean();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let op = GitOperation::CherryPick {
        commit: CommitOid::new(topic.clone()).expect("a full hex oid"),
    };
    assert_tree_matches_the_real_run(&target, &op, &["cherry-pick", &topic], "cherry-pick").await;
}

/// **A5, merge, content.** The previewed merge really unions the two branches'
/// trees.
///
/// This is the leg the verifiers found most dangerous: with no tree comparison,
/// a merge preview whose `theirs` equalled `ours` passed all thirty tests — and
/// since a branch can never conflict with itself, **every** conflicting merge
/// would have been drawn as a clean graph under that break.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — set the merge recipe's `theirs` to `head`.
///    `merge-tree` merges HEAD with itself, answers HEAD's tree, and the
///    equality goes red.
/// 2. **Weakens it** — set `merge_base: Some(head)`. git then treats HEAD as
///    the base, so the merged tree becomes the *other* branch's tree rather
///    than the union of both — a different wrong oid, from a different cause.
#[tokio::test]
async fn a5_a_previewed_merge_writes_the_tree_a_real_merge_writes() {
    let (dir, repo) = git_vista_fixtures::merge_clean_two_branch();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let op = GitOperation::MergeBranch {
        branch: BranchName::new("feature").expect("a valid branch name"),
    };
    assert_tree_matches_the_real_run(
        &target,
        &op,
        &["merge", "-q", "--no-edit", "feature"],
        "merge",
    )
    .await;
}

/// **A5, revert, row order.** The same parity as
/// `a5_a_previewed_revert_matches_a_real_revert`, on a shape where the
/// hypothetical commit's **timestamp** is what puts it in row 0.
///
/// `revert_shape`'s `after` window has exactly one topologically-ready commit,
/// so its row order is forced by topology alone and a bogus timestamp is
/// invisible there. Here `side` is ready from the start and dated
/// [`LONG_AGO`], so "newest first" has to actually decide.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — build the added `CommitSummary` with
///    `time: 0` instead of the value `read_back` read out of the store. The
///    hypothetical commit sorts below `side work` and the row assertion names
///    the swap.
/// 2. **Weakens it** — read the **author** time (`%at`) rather than the
///    committer time (`%ct`) in `read_commit_record`'s format string. On this
///    fixture both are "now", so row 0 is still right and this test stays
///    green — while `parse_commit_record_reads_six_fields_and_the_committer_time`
///    goes red on the literal `1_788_127_876`. Different mechanism, different
///    test, which is why the pair is written this way rather than as two
///    timestamp breaks that land here.
#[tokio::test]
async fn a5_the_previewed_reverts_row_is_decided_by_its_timestamp() {
    let (dir, repo) = revert_shape_with_a_competitor_tip();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let before_layout = layout_of(&repo);
    assert!(
        before_layout
            .rows
            .iter()
            .filter(|r| r.commit.parents.len() <= 1)
            .count()
            >= 2,
        "the fixture must offer a competitor tip, or row order is forced by \
         topology and this test proves nothing about time"
    );

    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head.clone()).expect("a full hex oid"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&target, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["revert", "--no-edit", &head]);
    let real = layout_of(&copy);

    assert_eq!(
        real.rows[0].commit.summary, "Revert \"add c\"",
        "the oracle must really put the new commit in row 0, or the parity \
         below could hold with the timestamp ignored"
    );
    assert_row_zero_is_decided_by_time(&copy);
    assert_parity(
        &graph.after,
        &real,
        &before_layout,
        "revert with competitor",
    );
}

/// **A5, merge, row order.** As above, for the merge leg.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — `time: 0` for the added commit, as in the
///    revert case: the merge commit sorts below `side work` and row 0 changes.
/// 2. **Weakens it** — transpose the merge recipe's `parents` to `[tip, head]`.
///    The timestamp is untouched so row 0 is still the merge commit, but the
///    parent-topology assertion and the lane placement of the two chains go
///    red instead.
#[tokio::test]
async fn a5_the_previewed_merges_row_is_decided_by_its_timestamp() {
    let (dir, repo) = merge_shape_with_a_competitor_tip();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let before_layout = layout_of(&repo);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, _) = expect_graph(preview(&target, &plan).await);

    let (_scratch, copy) = copy_of(&repo);
    git::run(&copy, &["merge", "-q", "--no-edit", "feature"]);
    let real = layout_of(&copy);

    assert_eq!(
        parent_count(&copy, "HEAD"),
        2,
        "the oracle must really be a two-parent merge commit"
    );
    assert_eq!(
        real.rows[0].commit.summary, "Merge branch 'feature'",
        "the oracle must really put the merge commit in row 0, ahead of the \
         competitor tip, or the parity below could hold with time ignored"
    );
    assert_row_zero_is_decided_by_time(&copy);
    assert_parity(&graph.after, &real, &before_layout, "merge with competitor");
}

/// **A3, merge.** A merge that would conflict answers `Conflict { paths }`,
/// naming the file — there was no such test at all before, and the merge arm is
/// where a wrongly-clean answer does the most damage.
///
/// The oracle runs first: real `git merge --no-edit` on a copy must actually
/// fail and leave the path conflicted. Without that, a preview reporting a
/// conflict on a shape that merges cleanly would look correct.
///
/// # Why the catalogue's `merge_conflict` and not this file's own shape
///
/// A `Conflict` outcome carries no layout, so the dating hazard that makes this
/// file build its own shapes for the parity tests (see the module doc) does not
/// apply, and the catalogue shape is strictly the better one: it is two commits
/// deep on each side rather than one, and it proves on a disposable clone that
/// a real `git merge` conflicts and leaves all three index stages behind — a
/// claim any caller gets for free, rather than one this file has to restate.
/// ADR 0099 names it as the fixture built for exactly this test.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — classify `Some(1)` as
///    `MergeTreeAnswer::Clean` in `merge_tree`. The preview draws a graph for a
///    merge git refuses to make, and the `match` here names it.
/// 2. **Weakens it** — set the merge recipe's `theirs` to `head`. HEAD merged
///    with itself never conflicts, so `merge-tree` exits 0, the preview draws a
///    clean merge commit and the same `match` goes red — but from a
///    *plausible* graph rather than a misclassification, which is the failure
///    the whole feature exists to prevent.
#[tokio::test]
async fn a3_a_conflicting_merge_answers_conflict_naming_the_file() {
    let (dir, repo) = git_vista_fixtures::merge_conflict();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");

    let (_scratch, copy) = copy_of(&repo);
    assert!(
        !git::try_run(&copy, &["merge", "--no-edit", "incoming"]),
        "the fixture must really conflict under real git, or this test's \
         expectation is not git's"
    );
    assert!(
        git::out(&copy, &["diff", "--name-only", "--diff-filter=U"])
            .lines()
            .any(|line| line == "shared.txt"),
        "real git must leave `shared.txt` unmerged"
    );

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("incoming").expect("a valid branch name"),
        },
    )
    .await;
    match preview(&target, &plan).await {
        PreviewOutcome::Conflict { paths } => assert_eq!(
            paths,
            vec!["shared.txt".to_string()],
            "the conflicted path must be the file itself, not git's prose"
        ),
        other => panic!("expected Conflict for a conflicting merge, got {other:?}"),
    }
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
    let (dir, repo) = fast_forward_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("behind").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, changes) = expect_graph(preview(&target, &plan).await);

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
    let (dir, repo) = fast_forward_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
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
    let (_graph, changes) = expect_graph(preview(&target, &plan).await);

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

/// **A cherry-pick whose change is already applied.** Real `git cherry-pick`
/// refuses and strands the repository mid-sequence, so there is no clean added
/// commit to draw.
///
/// # The defect this was carried red for, and what closed it
///
/// The cherry-pick arm used to compare nothing: `merge-tree` answered HEAD's
/// own tree, `commit-tree` wrapped it in an **empty** commit and the preview
/// drew it as an ordinary addition. What the user was shown was a tidy new row;
/// what they got on pressing the button was exit 1, a `CHERRY_PICK_HEAD` and a
/// repository they had to `--abort` out of. `compute` now carries the recipe's
/// `no_op` and refuses when the merged tree equals HEAD's.
///
/// The signal needed to refuse was always in hand, and it is still asserted
/// below: the tree `merge_tree` returns **equals** HEAD's tree, which is
/// precisely the "this pick contributes nothing" fact. That assertion is not
/// decoration — it is the evidence that the refusal costs one comparison rather
/// than a new spawn, and it is what makes the outcome assertion below mean
/// something rather than passing on a fixture that never reached the arm.
///
/// # What this asserts, and what it deliberately does not
///
/// Only that the outcome is **not a `Graph`**. The shipped answer is
/// `Unavailable { CheckFailed }`, and this test does not pin that: which
/// refusal is right — that, `Unsupported`, or a fifth outcome meaning "this
/// would do nothing and then stop" — is a contract question, and pinning it
/// from here would make a deliberate contract change look like a regression.
/// Asserting "no `Added` change" instead would be *weaker*, not narrower: it is
/// satisfied by routing this to a `Graph` with `changes: []`, which still tells
/// the user the pick is a harmless no-op when it is in fact an error.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — drop the tree-versus-HEAD comparison: the
///    preview draws the empty commit again and this assertion names the
///    `Graph`.
/// 2. **Weakens it** — compare the merged tree against the *picked commit's*
///    tree instead of HEAD's. That is a different question with a different
///    answer, so the empty pick is not caught and the same assertion goes red
///    — while `a5_cherry_pick_actually_moves_the_content` stays green, showing
///    the two comparisons are not interchangeable.
#[tokio::test]
async fn a_cherry_pick_that_is_already_applied_must_not_be_drawn_as_a_clean_commit() {
    let (dir, repo) = git_vista_fixtures::cherry_pick_already_applied();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let topic = git::out(&repo, &["rev-parse", "topic"]);
    let head_tree = git::out(&repo, &["rev-parse", "HEAD^{tree}"]);

    // The oracle: real git, on a copy.
    let (_scratch, copy) = copy_of(&repo);
    let head_before = git::out(&copy, &["rev-parse", "HEAD"]);
    assert!(
        !git::try_run(&copy, &["cherry-pick", &topic]),
        "a pick whose change is already applied must fail under real git — if \
         it succeeded, git's behaviour is not what this test was written \
         against"
    );
    assert_eq!(
        git::out(&copy, &["rev-parse", "HEAD"]),
        head_before,
        "the refused pick must leave HEAD where it was"
    );
    assert!(
        copy.join(".git").join("CHERRY_PICK_HEAD").exists(),
        "real git leaves the repository mid-sequence, which is the state a \
         preview must not describe as a clean new commit"
    );

    let op = GitOperation::CherryPick {
        commit: CommitOid::new(topic).expect("a full hex oid"),
    };
    assert_eq!(
        predicted_tree(&target, &op).await,
        head_tree,
        "the fact needed to refuse is already in hand: `merge-tree` answers \
         HEAD's own tree, which is what 'this pick contributes nothing' looks \
         like"
    );

    let plan = plan_for(&repo, op).await;
    let outcome = preview(&target, &plan).await;
    assert!(
        !matches!(outcome, PreviewOutcome::Graph { .. }),
        "a pick that real git refuses, leaving CHERRY_PICK_HEAD behind, must \
         not be drawn as a graph with a clean new commit; got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// `merge.ff` — what the executor will really do, and what the preview asks
// ---------------------------------------------------------------------------

/// Compare a preview's `after` half against a real run's layout when the
/// operation creates **no commit**, so every id on both sides is one that
/// already existed and nothing has to be mapped.
///
/// [`assert_parity`] cannot be used for these: it *requires* exactly one novel
/// commit on each side, because a hypothetical `commit-tree` oid can never
/// equal a real one. A fast-forward has none, which is precisely the claim, and
/// that makes the comparison strictly stronger here — the two graphs must be
/// equal outright, oids included.
///
/// The three non-triviality guards are asserted about the **oracle**, never
/// about the preview, for the reason [`assert_parity`] states: an equality
/// between two empty or single-row layouts passes while the mechanism it claims
/// to check is gone.
fn assert_identical_layout(after: &Graph, real: &Graph, before: &Graph, what: &str) {
    assert!(
        real.rows.len() > 1,
        "{what}: a one-row layout cannot show a placement mistake — the \
         fixture is too small to prove anything"
    );
    assert!(
        !real.edges.is_empty(),
        "{what}: the real layout drew no edges at all, so comparing edge sets \
         would compare two empty vectors and pass whatever the preview did"
    );
    assert_ne!(
        real.rows, before.rows,
        "{what}: the real run left the layout exactly as it found it, so \
         `after == real` would also hold for a preview that reported nothing \
         happening at all"
    );

    assert_eq!(
        after.rows, real.rows,
        "{what}: the rows differ — commit, row, lane, ref badges and colour are \
         all compared here, because all of them are what the user reads"
    );
    assert_eq!(
        after.edges, real.edges,
        "{what}: the lines drawn between rows differ"
    );
    assert_eq!(
        after.lane_count, real.lane_count,
        "{what}: the gutter widths differ"
    );
    assert_eq!(after.stubs, real.stubs, "{what}: the branch stubs differ");
}

/// **`merge.ff` unset — the configuration nearly every repository is in.** Real
/// `git merge --no-edit` fast-forwards; the preview must report that ref move
/// and add no commit.
///
/// # Why this test exists at all, when two `merge.ff` tests already did
///
/// Because both of those pin a *set* value, and every other fast-forwardable
/// shape in this file writes `merge.ff = true` into its own config. The
/// **unset** arm of [`fast_forward_policy`] — git's documented default — was
/// therefore pinned by nothing: measured on this host 2026-08-30, changing that
/// arm from `Allow` to `Never` left the whole binary green, which reintroduces
/// the exact "confidently wrong picture" defect the `merge.ff` round was about.
///
/// # Why the premise is checked twice, through two different launchers
///
/// The oracle below runs through `git_vista_fixtures::git`, which pins
/// `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` to `/dev/null`. The preview does
/// **not**: `sandbox::spawn` passes `$HOME` through and grants it read-only, so
/// a developer's own `~/.gitconfig merge.ff` reaches every spawn the server
/// makes (`fast_forward_policy`'s own doc says so). On such a host the two
/// sides would be answering different questions, and this test would report the
/// preview wrong when it was right. So the fixture asserts its own emptiness
/// with its launcher, and this asserts it again with [`preview_git`] — the
/// server's own path, the only config visibility that matters here. Removing
/// either check makes the oracle unsound rather than merely less thorough.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — return `FastForward::Never` from
///    [`fast_forward_policy`]'s `Some(1)` (key absent) arm. The preview
///    synthesises a two-parent merge commit git would not write, so `changes`
///    carries an `Added` and the first assertion goes red.
/// 2. **Weakens it** — keep `Allow` but hand `lay_out` an empty `ref_moves`.
///    Still no commit, so the `Added` assertion passes and so does the row
///    comparison's commit list — the **`RefMoved`** assertions go red instead,
///    naming a ref the user is told will move and is not, and
///    [`assert_identical_layout`] goes red on the badges.
#[tokio::test]
async fn merge_ff_unset_previews_the_fast_forward_git_actually_performs() {
    let (dir, repo) = git_vista_fixtures::fast_forward_merge_ff_unset();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");

    // The premise, read through the launcher the preview itself uses.
    let probe = preview_git(&repo, &["config", "--get", "merge.ff"])
        .await
        .expect("run git config through the preview's own launcher");
    assert_eq!(
        probe.status.code(),
        Some(1),
        "`git config --get merge.ff` must exit 1 (key absent) for the spawn the \
         SERVER makes, not merely for the fixture's own /dev/null-config \
         launcher — this host appears to set merge.ff somewhere the preview can \
         see it ({:?}), and with that set this test's oracle and its subject are \
         answering different questions",
        String::from_utf8_lossy(&probe.stdout)
    );

    let before_layout = layout_of(&repo);
    let head_before = git::out(&repo, &["rev-parse", "HEAD"]);
    let feature_tip = git::out(&repo, &["rev-parse", "feature"]);

    // The oracle: the executor's own argv, on a copy.
    let (_scratch, copy) = copy_of(&repo);
    let commits_before = git::out(&copy, &["rev-list", "--count", "--all"]);
    git::run(&copy, &["merge", "--no-edit", "feature"]);
    assert_eq!(
        git::out(&copy, &["rev-parse", "HEAD"]),
        feature_tip,
        "with merge.ff unset a real merge moves HEAD to feature's OWN oid — if \
         this fails, git's default is not what this test was written against"
    );
    assert_eq!(
        git::out(&copy, &["rev-list", "--count", "--all"]),
        commits_before,
        "a fast-forward writes no commit"
    );
    let real = layout_of(&copy);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, changes) = expect_graph(preview(&target, &plan).await);

    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, PreviewChange::Added { .. })),
        "git fast-forwards here, creating no commit, so nothing may be reported \
         as added: {changes:?}"
    );
    assert!(
        changes.contains(&PreviewChange::RefMoved {
            ref_name: "main".to_string(),
            from: Oid(head_before.clone()),
            to: Oid(feature_tip.clone()),
        }),
        "the checked-out branch must be reported as moving to feature's tip: {changes:?}"
    );
    assert!(
        changes.contains(&PreviewChange::RefMoved {
            ref_name: "HEAD".to_string(),
            from: Oid(head_before),
            to: Oid(feature_tip),
        }),
        "HEAD moves with the branch it is attached to: {changes:?}"
    );
    assert_identical_layout(&graph.after, &real, &before_layout, "merge.ff unset");
}

/// **`merge.ff = banana`.** git ignores a value it cannot parse and keeps its
/// default; this preview **refuses** instead, and that divergence is deliberate.
///
/// # The measurement, and why the divergence is the right way round
///
/// Measured on this host, 2026-08-30, in a throwaway repository built like this
/// one: `git config --get merge.ff` prints `banana` and exits 0;
/// `git config --type=bool --get merge.ff` exits **128** with `fatal: bad
/// boolean config value 'banana' for 'merge.ff'`; and `git merge --no-edit
/// feature` **fast-forwards normally** — `builtin/merge.c` deliberately does
/// not barf on values from future versions of git. The oracle below reproduces
/// that, so this test states what git does rather than assuming it.
///
/// [`fast_forward_policy`] answers `Unavailable { CheckFailed }` here. That is
/// stricter than git, in the only direction that is safe: the user sees no
/// picture rather than a picture drawn from a value neither party understood,
/// and it is the case a future git could give a *meaning* to, at which point
/// silently defaulting would become silently wrong.
///
/// Nothing pinned that choice before this test, so "fixing" it to default
/// quietly would have been a green change. It is a posture, not an accident,
/// and it is recorded in ADR 0099 under "Where it still refuses rather than
/// guesses".
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — make the `--type=bool` failure arm return
///    `Ok(FastForward::Allow)` instead of an error, i.e. default the way git
///    does. The outcome becomes a `Graph` and the match arm goes red naming it.
/// 2. **Weakens it** — keep the refusal but drop `{raw:?}` from the message.
///    The outcome is still `CheckFailed`, so the match arm passes, and so does
///    the "names the value" assertion — git's own stderr, which the message
///    also carries, happens to contain `banana` too. The **quoted-literal**
///    assertion is the one that goes red, which is exactly why it is written
///    separately: this module's own rendering of the value is the part it
///    controls, and it is what keeps `merge.ff = " only"` or an empty value
///    from being invisible.
#[tokio::test]
async fn merge_ff_set_to_an_unparseable_value_refuses_instead_of_defaulting() {
    let (dir, repo) = git_vista_fixtures::fast_forward_merge_ff_unset();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    git::run(&repo, &["config", "--local", "merge.ff", "banana"]);

    // The oracle: real git ignores the value and keeps its default.
    let feature_tip = git::out(&repo, &["rev-parse", "feature"]);
    let (_scratch, copy) = copy_of(&repo);
    assert_eq!(
        git::out(&copy, &["config", "--get", "merge.ff"]),
        "banana",
        "the copy must carry the setting, or the oracle below is not the case \
         this test is about"
    );
    git::run(&copy, &["merge", "--no-edit", "feature"]);
    assert_eq!(
        git::out(&copy, &["rev-parse", "HEAD"]),
        feature_tip,
        "git must IGNORE an unparseable merge.ff and fast-forward anyway — the \
         whole point of this test is that the preview is deliberately stricter \
         than that, so if git started refusing, the divergence would be gone \
         and this test would be pinning nothing"
    );

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    match preview(&target, &plan).await {
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::CheckFailed { detail },
        } => {
            assert!(
                detail.contains("banana"),
                "the refusal must name the value it choked on, or the user \
                 cannot act on it: {detail}"
            );
            assert!(
                detail.contains("\"banana\""),
                "the refusal must name the value as a QUOTED literal, and this \
                 one names it only in git's own stderr. The quoting is what \
                 makes `merge.ff = \" only\"` — or an empty value — visible at \
                 all, and git's stderr is a fallback this module does not \
                 control: {detail}"
            );
            assert!(
                detail.contains("merge.ff"),
                "the refusal must name the setting: {detail}"
            );
        }
        other => panic!(
            "an unparseable merge.ff must refuse — a value neither git-vista \
             nor the reader understands must not produce a picture; got {other:?}"
        ),
    }
}

/// **`merge.ff = false`, fast-forwardable.** Real `git merge --no-edit` writes
/// a **two-parent commit**; the preview must draw that commit.
///
/// # The defect this was carried red for, and what closed it
///
/// `resolve_plumbing`'s `Previewable::Merge` arm used to decide between
/// `AlreadyUpToDate`, `FastForward` and `Synthesize` from `merge-base` alone,
/// reading no git config at any point, while
/// `planner::branch_exec::exec_merge` runs `["merge", "--no-edit"]`, which
/// obeys `merge.ff`. Measured in a throwaway repository on this host,
/// 2026-08-30: with `merge.ff=false` on a fast-forwardable branch git printed
/// "Merge made by the 'ort' strategy" and `git cat-file -p HEAD` showed two
/// `parent` lines, while the preview took the `FastForward` arm and drew a
/// linear history with nothing added. The arm now asks
/// [`fast_forward_policy`], and this fixture — the catalogue's
/// `fast_forward_merge_ff_false`, which writes the setting into its own local
/// config and proves on a disposable clone that a real merge there is a
/// two-parent commit — is what holds it to that.
///
/// That is the confidently-wrong picture ADR 0099 exists to make impossible,
/// and `merge.ff = false` is a common setting that `sandbox::spawn` carries
/// into every repository through `$HOME`.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — drop the `merge.ff` read entirely and go back
///    to deciding on `merge-base` alone. The preview adds no commit and
///    `assert_parity`'s "the preview must add exactly one commit" fires with an
///    empty list.
/// 2. **Weakens it** — read the config but treat any value other than `only` as
///    permitting a fast-forward (i.e. handle `only` and ignore `false`). A
///    commit is still not created here, so the failure lands on the same
///    assertion — which is why that is deliberately **not** the second
///    mutation. Take instead: read `merge.ff=false` and answer `Synthesize`
///    with `parents: vec![head]`, a one-parent commit. One commit *is* added,
///    so the count passes and the **parent-topology** assertion goes red
///    instead, naming one parent where git wrote two.
#[tokio::test]
async fn merge_ff_false_must_preview_the_two_parent_commit_git_actually_writes() {
    let (dir, repo) = git_vista_fixtures::fast_forward_merge_ff_false();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let before_layout = layout_of(&repo);

    // The oracle: the executor's own argv, on a copy.
    let (_scratch, copy) = copy_of(&repo);
    assert_eq!(
        git::out(&copy, &["config", "--get", "merge.ff"]),
        "false",
        "the copy must carry the setting, or the oracle below is not the case \
         this test is about"
    );
    git::run(&copy, &["merge", "--no-edit", "feature"]);
    assert_eq!(
        parent_count(&copy, "HEAD"),
        2,
        "with merge.ff=false a real merge writes a two-parent commit — if this \
         fails, git's behaviour is not what this test was written against"
    );
    let real = layout_of(&copy);

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("feature").expect("a valid branch name"),
        },
    )
    .await;
    let (graph, changes) = expect_graph(preview(&target, &plan).await);

    assert_eq!(
        changes
            .iter()
            .filter(|c| matches!(c, PreviewChange::Added { .. }))
            .count(),
        1,
        "with merge.ff=false the merge creates a commit, so exactly one must \
         be reported as added: {changes:?}"
    );
    assert_parity(&graph.after, &real, &before_layout, "merge.ff=false");
}

/// **`merge.ff = only`, divergent.** Real `git merge --no-edit` exits **128**
/// and does nothing, so there is no graph to draw.
///
/// # The defect this was carried red for, and what closed it
///
/// Measured on this host, 2026-08-30: on two divergent branches with
/// `merge.ff=only`, git printed "fatal: Not possible to fast-forward,
/// aborting.", exited 128, and left HEAD exactly where it was, while the
/// preview took the `Synthesize` arm and drew a clean merge commit — a picture
/// of an operation that was going to fail. The merge arm now asks
/// [`fast_forward_policy`] and refuses on `Only` when HEAD has commits the
/// branch does not. The fixture is the catalogue's `divergent_merge_ff_only`,
/// which proves on a disposable clone that a real merge there is refused with
/// no `MERGE_HEAD` to abort.
///
/// # What this asserts, and what it deliberately does not
///
/// Only that the outcome is **not a `Graph`**. The shipped answer is
/// `Unavailable { CheckFailed { detail } }`, and this test does not pin that:
/// which refusal is right — that, or `Unsupported { operation }` ("this can
/// never be previewed") — is a contract question, and pinning it from here
/// would make a deliberate contract change look like a regression.
///
/// It is deliberately **not** written as "no `Added` change": `AlreadyUpToDate`
/// already returns a `Graph` with empty `changes`, so routing this case there
/// would satisfy that weaker form while still telling the user the merge is a
/// no-op when it is in fact an error.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — drop the `merge.ff` read: the preview
///    synthesises a merge commit again and this assertion names the `Graph`.
/// 2. **Weakens it** — read the config but route `only` to
///    `Plumbing::AlreadyUpToDate` instead of a refusal. Nothing is added and no
///    ref moves, so every "no commit was invented" check passes; the outcome is
///    still a `Graph` and *this* assertion is the one that goes red.
#[tokio::test]
async fn merge_ff_only_must_not_draw_a_merge_git_refuses_to_make() {
    let (dir, repo) = git_vista_fixtures::divergent_merge_ff_only();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");

    // The oracle first: prove real git refuses and moves nothing.
    let (_scratch, copy) = copy_of(&repo);
    let head_before = git::out(&copy, &["rev-parse", "HEAD"]);
    assert_eq!(
        git::out(&copy, &["config", "--get", "merge.ff"]),
        "only",
        "the copy must carry the setting, or the oracle below is not the case \
         this test is about"
    );
    assert!(
        !git::try_run(&copy, &["merge", "--no-edit", "rival"]),
        "with merge.ff=only a divergent merge must fail — if it succeeded, \
         git's behaviour is not what this test was written against"
    );
    assert_eq!(
        git::out(&copy, &["rev-parse", "HEAD"]),
        head_before,
        "the refused merge must leave HEAD where it was"
    );

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("rival").expect("a valid branch name"),
        },
    )
    .await;
    let outcome = preview(&target, &plan).await;

    assert!(
        !matches!(outcome, PreviewOutcome::Graph { .. }),
        "with merge.ff=only the merge exits 128 and changes nothing, so any \
         graph at all is a picture of something that will not happen; got \
         {outcome:?}"
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
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
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
        preview(&target, &plan).await
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
    let (dir, repo) = git_vista_fixtures::empty();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
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
    match preview(&target, &plan).await {
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::CheckFailed { detail },
        } => assert!(
            detail.contains("HEAD"),
            "the detail must name what could not be established: {detail}"
        ),
        other => panic!("expected Unavailable{{CheckFailed}}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The detached HEAD — the third report, and the refusal that reads it
// ---------------------------------------------------------------------------

/// A hypothetical commit whose oid is a literal, for the tests that ask what
/// the layout would do with one. `digit` becomes the whole 40-hex oid, so
/// `Oid::short` — the key `assign_branch_colors`'s synthetic fallback hashes —
/// is distinct per digit and readable in a failure message.
///
/// `time` must be newer than **now**, not merely newer than [`LONG_AGO`]:
/// `stable_topo_order` is a max-heap on `(time, Reverse(id))`, a real run
/// stamps its commit "now", and the tests that compare against one need the
/// hypothetical row to be row 0 without an oid tiebreak deciding it.
/// `2_000_000_000` is 2033-05-18 — ahead of the 2020 fixture commits and of
/// today. A reader running this after that date must raise it, and will see
/// row 0 quietly change hands if they do not.
fn hypothetical(digit: char, parent: &str) -> CommitSummary {
    CommitSummary {
        id: Oid((0..40).map(|_| digit).collect()),
        parents: vec![Oid(parent.to_string())],
        summary: "hypothetical".to_string(),
        author: "Test".to_string(),
        time: 2_000_000_000,
    }
}

/// Lay a real repository's history out with `added` prepended, through the
/// pure half directly — the same inputs [`lay_out`] builds, without its
/// refusal. This is how a test can look at the graph the refusal exists to
/// stop being returned.
fn would_be_layout(repo: &Path, added: CommitSummary) -> PreviewLayout {
    let target = added.id.0.clone();
    lay_out_preview(PreviewInput {
        before: git_vista_git::walk_history(repo, PREVIEW_HISTORY_LIMIT).expect("walk the history"),
        refs: git_vista_git::read_refs(repo).expect("read the refs"),
        head_branch: git_vista_git::read_head_branch(repo),
        added: Some(added),
        ref_moves: ref_moves_to(repo, &target),
        history_limit: PREVIEW_HISTORY_LIMIT,
    })
}

/// **A detached HEAD refuses.** On a detached HEAD `ref_moves_to` moves
/// `"HEAD"` and nothing else — `read_head_branch` is `None`, so there is no
/// branch to move — and `assign_branch_colors` seeds only from `is_branch()`
/// refs, which `RefKind::Head` is not. The hypothetical commit is therefore
/// coloured by `stable_color_slot("~<its own short oid>")`: a hash of the one
/// value a preview may never be compared on, because the preview's oid and the
/// real run's differ by construction.
///
/// # The defect is measured here, on this repository, before the refusal is
/// checked
///
/// The first half runs the pure layout twice over the *real* history and refs
/// of a real detached repository, changing nothing but the hypothetical
/// commit's oid, and the row-0 colour moves. That is the whole finding, in one
/// assertion, on real git data — and it is what makes the refusal below mean
/// something rather than passing on any repository that happens to fail.
///
/// `stable_color_slot` is `1 + fnv1a(key) % 6`, so an arbitrary pair of oids
/// can collide onto one slot and make this arm green while the mechanism is
/// broken. The two digits here were measured apart on this fixture; the
/// `assert_ne!` is the guard that a later edit to either digit cannot make the
/// arm vacuous without going red.
///
/// # The message is asserted by its words, not only by its variant
///
/// `CheckFailed { detail }` already carried two other sentences before this
/// one, for two other states. A `detail` that reads "moved no ref" here would
/// tell the user to fix something they did not do wrong, so the two other
/// sentences are asserted **absent** as well as this one present.
///
/// # Two mutations that make this red, failing differently
///
/// 1. **Removes the mechanism** — delete the `added_claimed_by_no_branch`
///    guard from [`lay_out`]. The preview answers `Graph` again and the
///    `match` names it.
/// 2. **Weakens it** — drop `is_branch()` from the field's computation in
///    `lay_out_preview`, so the `"HEAD"` entry satisfies it. The flag goes
///    false, the guard never fires, and both the witness assertion and the
///    `match` go red — the original defect class, restored.
#[tokio::test]
async fn a_detached_head_refuses_rather_than_colouring_a_commit_no_branch_claims() {
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    git::run(&repo, &["checkout", "-q", "--detach", "HEAD"]);
    assert!(
        git_vista_git::read_head_branch(&repo).is_none(),
        "the fixture must really be detached, or every assertion below is about \
         some other repository state"
    );

    // ---- the defect, on real git data --------------------------------------
    let f = would_be_layout(&repo, hypothetical('f', &head));
    let e = would_be_layout(&repo, hypothetical('e', &head));
    assert_eq!(f.after.rows[0].commit.id.0, "f".repeat(40));
    assert_eq!(e.after.rows[0].commit.id.0, "e".repeat(40));
    assert_ne!(
        f.after.rows[0].color, e.after.rows[0].color,
        "the hypothetical row's colour moved with nothing but its oid, and a \
         real run's oid is not either of these — that is what may not be drawn"
    );

    // ---- and it is the third report that says so, with the other two clear --
    assert!(
        f.added_claimed_by_no_branch,
        "HEAD moved onto the hypothetical commit, but HEAD is RefKind::Head and \
         assign_branch_colors does not seed from it"
    );
    assert_eq!(
        f.unmatched_ref_moves,
        Vec::<String>::new(),
        "the \"HEAD\" entry matched a real ref: this caller made no naming \
         mistake"
    );
    assert!(
        !f.added_without_ref_moves,
        "ref_moves was not empty either — both of the older reports are clear \
         for a preview that is nonetheless not reproducible"
    );

    // ---- so the server must refuse, and say which state it found -----------
    let plan = plan_for(
        &repo,
        GitOperation::RevertCommit {
            commit: CommitOid::new(head).expect("a full hex oid"),
        },
    )
    .await;

    match preview(&target, &plan).await {
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::CheckFailed { detail },
        } => {
            assert!(
                detail.contains("HEAD is detached"),
                "the detail must name the state that was found, in words the \
                 user can act on: {detail}"
            );
            assert!(
                detail.contains("colour"),
                "and what about the picture would have been wrong: {detail}"
            );
            assert!(
                !detail.contains("moved no ref"),
                "that is the `added_without_ref_moves` sentence, which names a \
                 caller mistake this caller did not make: {detail}"
            );
            assert!(
                !detail.contains("does not have"),
                "that is the `unmatched_ref_moves` sentence, and every ref this \
                 preview moved exists: {detail}"
            );
        }
        other => {
            panic!("expected Unavailable{{CheckFailed}} naming the detached HEAD, got {other:?}")
        }
    }
}

/// **The refusal is bound to the state it found, not to one fixed sentence.**
///
/// `added_claimed_by_no_branch` is the general condition; a detached HEAD is
/// its one production cause. A caller that moves `"HEAD"` alone while HEAD is
/// **attached** meets the same condition — every entry matches a real ref, the
/// list is not empty, and still no branch claims the added commit — and must
/// not be told "HEAD is detached", because it is not.
///
/// [`lay_out`] is called directly: `ref_moves_to` cannot produce this list on
/// an attached HEAD, which is exactly why the sentence needs pinning here. A
/// single constant string would satisfy the test above and lie in this one.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the guard: `lay_out` returns `Ok` and
///    `expect_err` panics.
/// 2. **Weakens it** — emit the detached sentence unconditionally (drop the
///    `head_branch.is_none()` arm). The variant is still `CheckFailed` and the
///    test above still passes; this one goes red on the sentence, which is the
///    only place the difference is visible.
#[test]
fn the_refusal_says_detached_only_when_head_really_is_detached() {
    let (_dir, repo) = revert_shape();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_vista_git::read_head_branch(&repo).as_deref(),
        Some("main"),
        "this fixture's HEAD is attached, which is the whole point of the case"
    );

    let added = hypothetical('f', &head);
    let target = added.id.clone();
    let refused = lay_out(&repo, Some(added), vec![("HEAD".to_string(), target)])
        .expect_err("no branch claims the added commit, so there is no honest graph");

    match refused {
        PreviewUnavailable::CheckFailed { detail } => {
            assert!(
                detail.contains("no branch"),
                "the general condition is what was found, so it is what the \
                 detail must name: {detail}"
            );
            assert!(
                !detail.contains("HEAD is detached"),
                "HEAD is attached to `main` in this repository — a sentence that \
                 says otherwise is the message drifting free of the state: {detail}"
            );
        }
        other => panic!("expected CheckFailed, got {other:?}"),
    }
}

/// **The refusal must not swallow the operations it does not apply to.** A
/// fast-forward adds no commit, so there is no hypothetical row to colour and
/// nothing for `added_claimed_by_no_branch` to report — a detached HEAD
/// previews it exactly as an attached one does.
///
/// Green before the guard existed and green after: it is the guard against
/// over-refusal, and it goes red the moment the refusal is written as "HEAD is
/// detached" rather than "a commit was added that no branch claims".
#[tokio::test]
async fn a_detached_head_still_previews_a_fast_forward_because_it_adds_no_commit() {
    let (dir, repo) = fast_forward_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let tip = git::out(&repo, &["rev-parse", "main"]);
    let behind = git::out(&repo, &["rev-parse", "behind"]);
    git::run(&repo, &["checkout", "-q", "--detach", "behind"]);
    assert!(
        git_vista_git::read_head_branch(&repo).is_none(),
        "the fixture must really be detached"
    );

    let plan = plan_for(
        &repo,
        GitOperation::MergeBranch {
            branch: BranchName::new("main").expect("a valid branch name"),
        },
    )
    .await;
    let (_graph, changes) = expect_graph(preview(&target, &plan).await);

    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, PreviewChange::Added { .. })),
        "a fast-forward creates no commit: {changes:?}"
    );
    assert!(
        changes.contains(&PreviewChange::RefMoved {
            ref_name: "HEAD".to_string(),
            from: Oid(behind),
            to: Oid(tip),
        }),
        "a detached HEAD moves itself and no branch, and that is still a fact \
         the preview can state: {changes:?}"
    );
}

/// A directory that is not a repository has no commondir, so there is nowhere
/// for a scratch store to live — `ScratchStore`, not `CheckFailed`. The two
/// are different facts: one says the computation failed, the other says it
/// never had anywhere to happen.
#[test]
fn a_directory_with_no_git_answers_scratch_store() {
    let dir = TempDir::new().expect("tempdir");
    match PreviewTarget::resolved_in(dir.path(), dir.path()) {
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
    let (dir, repo) = revert_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();

    let path = {
        let store = ScratchStore::new(&target).await.expect("create the store");
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
    let (dir, repo) = sha256_shape();
    let target =
        PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the fixture root");
    let commondir = target.commondir().to_path_buf();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);
    let objects_before = object_file_count(&commondir);
    let refs_before_raw = git::out(&repo, &["show-ref"]);

    let op = previewable(&GitOperation::RevertCommit {
        commit: CommitOid::new(head.clone()).expect("a 64-character hex oid"),
    })
    .expect("a revert is previewable");
    let plumbing = resolve_plumbing(&target, &op, &head)
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
    // Finding 2's own example: a directory the USER made, sharing the public
    // prefix, holding something they care about. It is also exactly
    // `tempfile`'s name shape — `gv-preview-` plus six alphanumerics — so a
    // "validate the generated name" fix would let this through untouched.
    let decoy = commondir.join(format!("{SCRATCH_PREFIX}backup"));
    // A second decoy that *does* carry a file of the marker's name, holding
    // somebody else's bytes. Without it, "the marker must exist" would pass
    // this test while the magic comparison rotted away, and a lock file from
    // any other tool would be read as a licence to `remove_dir_all`.
    let impostor = commondir.join(format!("{SCRATCH_PREFIX}other1"));
    for d in [&stale, &young, &foreign, &decoy, &impostor] {
        std::fs::create_dir_all(d).expect("create dir");
    }
    let precious = decoy.join("precious.txt");
    std::fs::write(&precious, b"the user's own bytes\n").expect("write the decoy's content");
    let impostor_marker = impostor.join(STORE_MARKER);
    std::fs::write(
        &impostor_marker,
        b"some other tool's lock file v9\nand more\n",
    )
    .expect("write the impostor's marker");
    // Mark the two directories this module would really have created, through
    // the production helper — never hand-rolled, or the magic could drift and
    // the sweep would silently stop matching what it writes.
    for d in [&stale, &young] {
        let lease = ScratchStore::claim(d).expect("claim the planted store");
        drop(lease); // abandoned: the owner is "gone"
    }
    // Age the three that must be old past the bound by rewriting their mtimes.
    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&stale, long_ago);
    filetime_set(&decoy, long_ago);
    filetime_set(&impostor, long_ago);

    ScratchStore::sweep_stale(commondir);

    assert!(
        !stale.exists(),
        "a marked, unleased `gv-preview-*` directory older than the bound must \
         be swept"
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
    assert!(
        decoy.exists(),
        "a `gv-preview-*` directory this module never created must survive \
         however old it is — a prefix is a PUBLIC string, not proof of \
         ownership, and `{}` is a user's own backup directory",
        decoy.display()
    );
    assert!(
        impostor.exists(),
        "a `gv-preview-*` directory holding a file merely NAMED like the \
         marker must survive: the magic is compared exactly, because \
         `{}`'s presence proves nothing about who wrote it",
        impostor_marker.display()
    );
    assert_eq!(
        std::fs::read(&impostor_marker).ok().as_deref(),
        Some(b"some other tool's lock file v9\nand more\n".as_slice()),
        "the impostor's own file was rewritten or removed"
    );
    assert_eq!(
        std::fs::read(&precious).ok().as_deref(),
        Some(b"the user's own bytes\n".as_slice()),
        "the sweep recursively deleted a foreign directory's contents: \
         `remove_dir_all` inside a user's `.git`, keyed on a name anyone can \
         write"
    );
}

/// A named pipe wearing the marker's name must not wedge the sweep.
///
/// [`ScratchStore::abandoned_store_lease`] opens the marker to read its
/// magic, and `File::open` on a FIFO with no writer **blocks for ever**. So
/// the four-gate refusal this module documents — "marker missing or
/// unreadable or not a regular file … is a `continue`" — is never reached on
/// this input: the decision hangs before it can be made, because the
/// `is_file()` guard that would refuse a FIFO runs *after* the open that
/// hangs.
///
/// The cost is not one leaked directory. `sweep_stale` runs from
/// [`ScratchStore::new`], which `preview` reaches on a `tokio::spawn`ed
/// task, so a single `mkfifo` inside `<commondir>` parks a runtime worker
/// permanently and takes every later preview against that repository with
/// it. The reach needed to plant one is write access to the scratch store's
/// own directory — the same reach finding 2 was about. This time the
/// mechanism that closed finding 2 is what opened it, which is why it is
/// pinned here rather than left to the marker's own doc comment.
///
/// The sweep runs on its own thread and must answer inside a generous
/// bound. A regression therefore costs one red test rather than a hung CI
/// run — a test that reproduced this by hanging would be unrunnable.
#[test]
fn a_named_pipe_wearing_the_markers_name_cannot_wedge_the_sweep() {
    let dir = TempDir::new().expect("tempdir");
    let commondir = dir.path().to_path_buf();

    // The attack: a `gv-preview-*` directory whose marker is a FIFO that
    // nobody will ever open for writing.
    let trap = commondir.join(format!("{SCRATCH_PREFIX}fifo"));
    std::fs::create_dir_all(&trap).expect("create the trap directory");
    let fifo = trap.join(STORE_MARKER);
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
        .expect("a temp path with no interior NUL");
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo failed, so this test would prove nothing: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        !fifo
            .symlink_metadata()
            .expect("stat the planted marker")
            .is_file(),
        "the planted marker must really be a FIFO, or the hazard is absent \
         and this test is inert"
    );

    // The control: a genuine abandoned store beside it. Without this, a
    // sweep that had simply stopped working would pass.
    let control = commondir.join(format!("{SCRATCH_PREFIX}control"));
    std::fs::create_dir_all(&control).expect("create the control store");
    drop(ScratchStore::claim(&control).expect("claim the control store"));

    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&trap, long_ago);
    filetime_set(&control, long_ago);

    let (tx, rx) = std::sync::mpsc::channel();
    let sweep_dir = commondir.clone();
    std::thread::spawn(move || {
        ScratchStore::sweep_stale(&sweep_dir);
        let _ = tx.send(());
    });

    if rx.recv_timeout(std::time::Duration::from_secs(20)).is_err() {
        panic!(
            "`sweep_stale` never returned: the named pipe at `{}` wedged it. \
             `File::open` on a FIFO with no writer blocks for ever, so the \
             `is_file()` refusal one line later is unreachable — and because \
             the sweep runs on a spawned task from `ScratchStore::new`, this \
             parks a runtime worker and every later preview against this \
             repository with it",
            fifo.display()
        );
    }

    assert!(
        trap.exists(),
        "a FIFO is not this module's marker: the trap directory must survive"
    );
    assert!(
        !control.exists(),
        "the sweep answered but reclaimed nothing — this test would pass \
         against a sweep that had stopped working entirely"
    );
}

/// A marker that **serves the magic** and is not a regular file must still be
/// refused.
///
/// # Why this test exists: a `survived` verdict on the test above
///
/// `mutation_check` on 2026-08-31, run 303: deleting
///
/// ```text
/// if !f.metadata().ok()?.is_file() { return None; }
/// ```
///
/// left `a_named_pipe_wearing_the_markers_name_cannot_wedge_the_sweep` GREEN.
/// The guard was doing nothing that test could detect, because with
/// `O_NONBLOCK` a writerless FIFO fails at `read_exact` with `EAGAIN` and
/// `.ok()?` refuses it one line later. So the wedge test pins "the sweep
/// answers" and pins nothing at all about *type*.
///
/// That is the shape this repository keeps paying for — a guard that reads as
/// load-bearing, with no test that can fail when it is removed. One `caught`
/// on the flag would have let both claims ride on one experiment.
///
/// The case where `is_file()` is the only thing standing between a user and
/// `remove_dir_all` is narrow: a non-regular file whose *contents* satisfy
/// every later gate. A FIFO holding the magic does exactly that — the read
/// succeeds, the magic matches, and `flock` on a pipe is free. Without the
/// guard the directory is deleted.
///
/// The test holds the pipe open `O_RDWR` rather than spawning a writer:
/// opening a FIFO read-write never blocks, and it leaves the magic sitting in
/// the pipe buffer for the sweep's own reader to find. A writer thread would
/// have raced the sweep and made a green run mean nothing.
#[test]
fn a_marker_that_serves_the_magic_but_is_not_a_regular_file_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let commondir = dir.path().to_path_buf();

    let trap = commondir.join(format!("{SCRATCH_PREFIX}fedfifo"));
    std::fs::create_dir_all(&trap).expect("create the trap directory");
    let precious = trap.join("precious.txt");
    std::fs::write(&precious, b"the user's own bytes\n").expect("write the decoy's content");

    let fifo = trap.join(STORE_MARKER);
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
        .expect("a temp path with no interior NUL");
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo failed, so this test would prove nothing: {}",
        std::io::Error::last_os_error()
    );

    // `O_RDWR` on a FIFO never blocks and makes this process both ends, so
    // the magic can sit in the pipe buffer with no second thread to race.
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .expect("open the planted FIFO read-write");
    pipe.write_all(STORE_MARKER_MAGIC)
        .expect("feed the magic into the pipe");

    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&trap, long_ago);

    ScratchStore::sweep_stale(&commondir);

    assert!(
        trap.exists(),
        "a directory whose marker SERVES the magic but is not a regular file \
         was deleted. Every later gate passes on this input — the read \
         succeeds, the magic matches exactly, and `flock` on a pipe is free — \
         so `is_file()` on the open fd is the only thing refusing it"
    );
    assert_eq!(
        std::fs::read(&precious).ok().as_deref(),
        Some(b"the user's own bytes\n".as_slice()),
        "`remove_dir_all` ran inside a directory this module never created, \
         keyed on bytes anyone can feed through a pipe"
    );
}

/// The **production** constructor validates containment itself, and carries
/// the `commondir` rather than the `gitdir`.
///
/// Every other test in this file builds its target with
/// [`PreviewTarget::resolved_in`], the single-root constructor. That leaves
/// [`PreviewTarget::in_managed_catalog`] — the one the HTTP handler actually
/// calls — exercised by nothing, which is the same hole a `#[cfg(test)]`
/// bypass constructor would have opened: the suite would stop exercising the
/// shape production depends on. Verified by running it: with the
/// `path_is_allowed` guard deleted, every other test in this module still
/// passed.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `path_is_allowed` guard from
///    `in_managed_catalog`. The unregistered repository is accepted and the
///    first half goes red; the second half stays green, because an allowed
///    root was allowed either way.
/// 2. **Weakens it** — carry `paths.gitdir` instead of `paths.commondir`. The
///    guard still refuses the unregistered repository, so the first half stays
///    green; the linked worktree's target then names
///    `<main>/.git/worktrees/<id>` instead of `<main>/.git` and the second
///    half goes red. A *plain* repository cannot tell those apart —
///    `gitdir == commondir` there — which is why this test uses a worktree.
#[test]
fn the_catalog_constructor_refuses_an_unregistered_root_and_carries_the_commondir() {
    // Half one: a repository under a root the catalog was never told about.
    // The catalog is process-global and accumulates roots, but every root any
    // test allows is its own `TempDir`, so a freshly created one can never be
    // inside one of them.
    let stranger = TempDir::new().expect("tempdir");
    let outsider = stranger.path().join("repo");
    git::init(&outsider);
    match PreviewTarget::in_managed_catalog(&outsider) {
        Err(PreviewUnavailable::ScratchStore { detail }) => assert!(
            detail.contains("managed root"),
            "the refusal must say what was wrong: {detail}"
        ),
        other => panic!(
            "a repository under no allowed root must be refused before anything \
             can be deleted inside it, got {other:?}"
        ),
    }

    // Half two: an allowed root, and a linked worktree — the one geometry
    // where carrying the gitdir instead of the commondir is visible.
    let (dir, repo) = revert_shape();
    let worktree = dir.path().join("wt");
    git::run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "catalogbranch",
            worktree.to_str().expect("a utf-8 path"),
        ],
    );
    crate::state::allow_repo_root(dir.path());

    let target = PreviewTarget::in_managed_catalog(&worktree)
        .expect("a worktree inside an allowed root is served");
    let expected = repo
        .join(".git")
        .canonicalize()
        .expect("the main git directory exists");
    assert_eq!(
        target.commondir(),
        expected,
        "the production constructor must carry the COMMONDIR — the store lives \
         there and the sweep deletes there. A linked worktree's gitdir is \
         `<main>/.git/worktrees/<id>`, which is inside the grant but is not \
         where any of this happens."
    );
    assert_eq!(
        target.repo(),
        worktree,
        "the repository path every git spawn is built from must be the one the \
         caller asked about, unchanged"
    );
}

/// The instrument the two sweep tests rest on: a real store carries
/// [`STORE_MARKER`] with the exact magic in it, and holds that file's lease
/// for as long as it is alive.
///
/// The same role `the_manifest_and_the_scratch_sweep_both_notice_a_planted_store`
/// plays for the A2 detectors. Without it, "the decoy survived" and "the live
/// store survived" could both be true of a sweep that had simply stopped
/// working, and nothing in the suite would say so.
///
/// # Two mutations, and why they must fail differently
///
/// 1. **Removes half the mechanism** — have `ScratchStore::claim` skip
///    `try_lock`. The magic is still written, so the content assertions stay
///    green; only the `WouldBlock` assertion goes red.
/// 2. **Weakens the other half** — write a different magic string. The lease is
///    still taken, so the `WouldBlock` assertion stays green; only the content
///    assertion goes red.
///
/// The split is the point: ownership and liveness are two mechanisms, not one
/// wearing two hats, and a test that could not tell them apart would let either
/// rot while reporting the other.
#[tokio::test]
async fn the_scratch_store_carries_a_marker_and_holds_its_lease() {
    let (dir, repo) = revert_shape();
    let target = PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the root");

    let marker = {
        let store = ScratchStore::new(&target).await.expect("create the store");
        let marker = store.dir.path().join(STORE_MARKER);

        let meta = std::fs::metadata(&marker).expect("the store must carry its marker file");
        assert!(
            meta.is_file(),
            "the marker must be a regular file — the sweep `fstat`s it on the \
             open fd before believing anything it says"
        );

        let bytes = std::fs::read(&marker).expect("read the marker");
        assert!(
            bytes.starts_with(STORE_MARKER_MAGIC),
            "the marker's first bytes must be exactly the magic the sweep \
             compares against, or `sweep_stale` can never recognise a store \
             this module created: got {:?}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(64)])
        );
        assert!(
            bytes.len() > STORE_MARKER_MAGIC.len(),
            "the marker must also say, in words, what it is and when it is \
             safe to delete — a human who finds one after a crash has nothing \
             else to go on"
        );

        let probe = std::fs::File::open(&marker).expect("open the marker again");
        assert!(
            matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "a live store must hold its marker's lease: without it, `age` is \
             the only thing separating a running preview from an abandoned \
             one, and a two-hour preview is indistinguishable from a corpse"
        );
        marker
    };

    assert!(
        !marker.exists(),
        "the marker goes with the store — the whole directory is removed on \
         drop, while the lease is still held"
    );
}

/// A store a preview is **using right now** is never swept, however old the
/// directory looks.
///
/// This is the second half of finding 2 and it is not the same defect as the
/// decoy above. A directory mtime is a timestamp, not a lease: a preview whose
/// store was created more than [`STALE_SCRATCH_AGE`] ago is indistinguishable
/// from an abandoned one by age alone, so a second preview — in this process
/// or another — reaps a store that is in use. The store's own advisory lock on
/// its marker is what tells the two apart, and the kernel releases it exactly
/// when the owning process dies, which is the question the sweep is asking.
///
/// The mtime is forced past the bound deliberately rather than waiting an
/// hour: the point is that age must stop being the answer.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `try_lock` gate from
///    `abandoned_store_lease` so ownership alone decides. The store is marked
///    and old, so it is swept and every assertion below fails at the first.
/// 2. **Weakens it** — treat `Err(TryLockError::WouldBlock)` as "could not
///    tell, delete anyway". The lock is still consulted, so the code still
///    *looks* right; the live store is still deleted, and the failure arrives
///    from a sweep that examined the lease and drew the opposite conclusion.
#[tokio::test]
async fn a_live_store_is_never_swept_however_old_it_looks() {
    let (dir, repo) = revert_shape();
    let target = PreviewTarget::resolved_in(&repo, dir.path()).expect("a target inside the root");
    let commondir = target.commondir().to_path_buf();

    let store = ScratchStore::new(&target).await.expect("create the store");
    let path = store.dir.path().to_path_buf();
    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&path, long_ago);

    // A second preview starting up, in the same commondir, while the first is
    // still running.
    ScratchStore::sweep_stale(&commondir);

    assert!(
        path.exists(),
        "a live preview's store was swept out from under it: `{}` is held by \
         this test right now, and an mtime older than the bound is not \
         evidence that anybody abandoned it",
        path.display()
    );
    let alternates = std::fs::read_to_string(path.join("objects").join("info").join("alternates"))
        .expect(
            "the live store's alternates file must still be readable after a \
             concurrent sweep — without it the store cannot see the served \
             repository's objects at all",
        );
    assert_eq!(
        alternates.trim(),
        commondir.join("objects").display().to_string(),
        "the sweep left a store that no longer names the served object \
         directory"
    );
    assert!(
        store.git_dir_flag().starts_with("--git-dir="),
        "the store must still be usable as a git directory after the sweep"
    );

    // And once the owner really is gone, the same sweep does reclaim it — or
    // the assertions above would be satisfied by a sweep that never deletes
    // anything at all.
    drop(store);
    std::fs::create_dir_all(&path).expect("re-plant the abandoned store's directory");
    let lease = ScratchStore::claim(&path).expect("mark it the way production does");
    drop(lease);
    filetime_set(&path, long_ago);
    ScratchStore::sweep_stale(&commondir);
    assert!(
        !path.exists(),
        "an abandoned, marked, unleased store older than the bound must still \
         be reclaimed — otherwise this test is satisfied by a sweep that is \
         simply inert"
    );
}

#[tokio::test]
async fn the_store_lands_in_the_commondir_the_request_validated_not_a_re_resolved_one() {
    let (dir, repo) = revert_shape();
    let root = dir.path();

    let a = root.join("a");
    git::run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "abranch",
            a.to_str().expect("utf-8"),
        ],
    );

    let b = root.join("b");
    git::init(&b);
    git::write(&b, "x.txt", b"x\n");
    commit_old(&b, "b one");
    let bwt = root.join("bwt");
    git::run(
        &b,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "bbranch",
            bwt.to_str().expect("utf-8"),
        ],
    );

    // The request's target, validated once — this is the fact the whole fix
    // carries.
    let target = PreviewTarget::resolved_in(&a, root).expect("a is inside the fixture root");
    let validated = target.commondir().to_path_buf();
    let b_target = PreviewTarget::resolved_in(&bwt, root).expect("bwt is inside the fixture root");
    let b_commondir = b_target.commondir().to_path_buf();
    assert_ne!(validated, b_commondir, "the two geometries must differ");

    // A store in B that a sweep WOULD reclaim if it ever ran there: prefix,
    // marker, free lease, aged past the bound. Everything is affirmatively
    // true except that this request never validated B.
    let victim = b_commondir.join(format!("{SCRATCH_PREFIX}victim"));
    std::fs::create_dir_all(&victim).expect("plant the victim");
    let precious = victim.join("precious.txt");
    std::fs::write(&precious, b"another repository's bytes\n").expect("write");
    drop(ScratchStore::claim(&victim).expect("mark the victim the way production does"));
    let long_ago = std::time::SystemTime::now() - STALE_SCRATCH_AGE - Duration::from_secs(60);
    filetime_set(&victim, long_ago);

    // The concurrent tamper: A's `.git` pointer now names a self-consistent
    // linked-worktree geometry belonging to B, done with real git rather than
    // argued about.
    let stolen = std::fs::read(bwt.join(".git")).expect("read b's worktree pointer");
    std::fs::write(a.join(".git"), &stolen).expect("swap a's pointer");
    assert_eq!(
        PreviewTarget::resolved_in(&a, root)
            .expect("the tampered geometry still resolves")
            .commondir(),
        b_commondir,
        "the tamper must actually redirect a re-resolution, or this test proves nothing"
    );

    // A canary in the VALIDATED commondir, identical in every respect to the
    // victim. It proves the sweep actually ran: without it, an early return
    // anywhere in `ScratchStore::new` — a refused spawn under the tampered
    // geometry, say — would leave the victim intact and this test green for a
    // reason that has nothing to do with the fix.
    let canary = validated.join(format!("{SCRATCH_PREFIX}canary"));
    std::fs::create_dir_all(&canary).expect("plant the canary");
    drop(ScratchStore::claim(&canary).expect("mark the canary"));
    filetime_set(&canary, long_ago);

    // Whether the store can be built at all under a tampered geometry is not
    // the claim; where it is allowed to delete is.
    let _ = ScratchStore::new(&target).await;

    assert!(
        precious.exists(),
        "the store's sweep followed a `.git` pointer swapped AFTER the request \
         was validated and ran `remove_dir_all` in another repository"
    );
    assert_eq!(
        scratch_dirs(&b_commondir),
        vec![format!("{SCRATCH_PREFIX}victim")],
        "nothing may be created or removed under a commondir this request never validated"
    );
    // Checked last so the two assertions above get to name the defect first;
    // this one only answers "did the sweep run at all".
    assert!(
        !canary.exists(),
        "the sweep never ran, so this test would have proved nothing about \
         where it is allowed to run: `{}` is still there",
        canary.display()
    );
}

/// The deletion path resolves the repository's geometry **nowhere**: the only
/// resolver call in the whole module is the one that builds a
/// [`PreviewTarget`], and every consumer takes the answer it already carries.
///
/// A tripwire rather than a review convention, and the house pattern
/// (`sandbox::spawn`'s `the_sandboxed_command_exposes_no_way_to_change_what_runs`,
/// `sandbox::trust`'s `grant_is_unreachable_from_production_…`) is to assert it
/// against the source text. The property is structural, so it cannot be seen
/// from behaviour: re-introducing a second resolution is invisible until an
/// attacker swaps a pointer between the two.
///
/// # Why the expected count is two, not one
///
/// The *place* is `impl PreviewTarget` — a single block, which is what the
/// name means. It holds two constructors only because
/// `state::resolve_target` still discards the resolution it validated, so the
/// preview handler has to redo the multi-root check itself
/// (`in_managed_catalog`) while the suite uses the single-root one
/// (`resolved_in`). When `state.rs` grows a `ValidatedTarget` that carries
/// `repo_paths::RepoPaths`, `in_managed_catalog` becomes
/// `from_request(&ValidatedTarget)`, resolves nothing, and this expectation
/// drops to one. Both are written as literals below so that day is a
/// deliberate edit rather than a number someone bumped.
///
/// # Why the count is scoped, not global
///
/// `repo_paths::resolve` is a string *prefix* of `repo_paths::resolve_and_validate`,
/// and this module's doc comments name both in prose. A naive whole-file count
/// would move whenever someone writes a sentence, and a tripwire whose number
/// drifts gets "fixed" by bumping the number. So comment lines are excluded and
/// the surviving call is required to sit inside `impl PreviewTarget`.
///
/// # Two mutations
///
/// 1. **Removes the mechanism** — delete the `== 1` count assertion. A second
///    `repo_paths::resolve` planted anywhere in the module then passes.
/// 2. **Weakens it** — relax the count to `>= 1`. A re-introduced
///    `commondir_of` helper called from `ScratchStore::new` goes unnoticed,
///    which is precisely the code this test exists to keep deleted.
#[test]
fn preview_resolves_the_commondir_in_exactly_one_place() {
    const SRC: &str = include_str!("preview.rs");

    // Comment-aware: this module's own docs cite the deleted helper by name as
    // history, and a tripwire that fired on a doc sentence would be "fixed" by
    // deleting the history.
    let helper_calls: Vec<&str> = SRC
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("commondir_of"))
        .collect();
    assert!(
        helper_calls.is_empty(),
        "`commondir_of` is back in preview.rs. It re-resolved the repository's \
         geometry a second time, below the request boundary and with the \
         containment-free resolver, and its answer was handed straight to \
         `remove_dir_all`. The validated commondir is carried on \
         `PreviewTarget`; take it from there. Found: {helper_calls:?}"
    );

    let calls: Vec<(usize, &str)> = SRC
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .filter(|(_, l)| l.contains("repo_paths::"))
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();
    // Literal expectations, one per case, in source order — never a count
    // re-derived from whatever happens to be there. A third resolution fails
    // the length check; a resolution that *moved* fails the containment check
    // below; a resolution that changed shape fails these.
    assert_eq!(
        calls.len(),
        2,
        "preview.rs must resolve the repository's geometry in exactly the two \
         constructors below and nowhere else. Every extra resolution is \
         another chance to follow a `.git` pointer an attacker swapped after \
         the request was validated, and this module ends that chain in a bare \
         `remove_dir_all` with no sandbox in front of it. Found: {calls:?}"
    );
    assert!(
        calls[0].1.contains("repo_paths::resolve(repo)"),
        "the first resolution must be `in_managed_catalog`'s multi-root one: {:?}",
        calls[0]
    );
    assert!(
        calls[1]
            .1
            .contains("repo_paths::resolve_and_validate(repo, managed_root)"),
        "the second must be `resolved_in`'s single-root one, which validates \
         containment itself: {:?}",
        calls[1]
    );

    let start = SRC.find("impl PreviewTarget {").expect(
        "preview.rs no longer defines `impl PreviewTarget {` — if the \
                 constructor moved, move this tripwire with it",
    );
    let block = &SRC[start..];
    let end = block
        .find("\n}\n")
        .expect("unterminated `impl PreviewTarget` block");
    let block = &block[..end];
    let inside = block
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("repo_paths::"))
        .count();
    assert_eq!(
        inside,
        calls.len(),
        "every resolution must live in `impl PreviewTarget`, where a target is \
         built and validated — not anywhere the store or the sweep can reach \
         it. {} of {} are outside it.",
        calls.len() - inside,
        calls.len()
    );

    let store_start = SRC
        .find("impl ScratchStore {")
        .expect("preview.rs no longer defines `impl ScratchStore {`");
    let store = &SRC[store_start..];
    let store_end = store
        .find("\n}\n")
        .expect("unterminated `impl ScratchStore` block");
    let store = &store[..store_end];
    for (i, line) in store.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        assert!(
            !line.contains("repo_paths::"),
            "`impl ScratchStore` line {i} resolves the repository's geometry: \
             {line}\nThe store is created in, and the sweep deletes from, the \
             commondir the REQUEST validated. Resolving here re-opens the \
             window the carried target closed."
        );
    }
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

/// **`RefMoved.from` must report the moved BRANCH's old target, not a tag's.**
///
/// `read_refs` shortens `refs/heads/main` and `refs/tags/main` into the same
/// display name (`git-vista-git/src/refs.rs`'s `category_and_short_name`
/// match), and [`previous_targets`] searched that flat list by name alone. In a
/// repository holding both, which entry the search reached first decided the
/// oid the user was shown as the ref's *previous* position — an accident of
/// enumeration order, not a decision.
///
/// Both orderings are exercised deliberately. Only one of them is reachable
/// through `git_vista_git::read_refs` on this host (loose refs enumerate
/// `refs/heads/…` before `refs/tags/…`, so the branch happens to win), which is
/// precisely why the ordering must not be what decides: a packed-refs
/// repository, a different `gix`, or a future reader is free to hand the list
/// over the other way round and nothing here would notice.
///
/// # Two mutations that make this red, failing differently
///
/// * **M11a — REMOVES the mechanism.** Drop the kind filter from
///   [`previous_targets`]. The tag-first case reports the tag's `2222…`: red on
///   the first assertion, green on the second.
/// * **M11b — WEAKENS the mechanism.** Filter on `is_branch()` instead, which
///   excludes `RefKind::Head`. Both `main` cases stay green and the `"HEAD"`
///   entry stops matching anything at all, so it vanishes from the result: red
///   on the length assertions.
#[test]
fn ref_moved_from_reports_the_branch_old_target_not_a_same_named_tags() {
    use git_vista_core::model::RefKind;

    let git_ref = |name: &str, kind: RefKind, digit: char| GitRef {
        name: name.to_string(),
        kind,
        target: Oid((0..40).map(|_| digit).collect()),
    };
    let new_target = Oid("9".repeat(40));
    let moves = vec![
        ("main".to_string(), new_target.clone()),
        ("HEAD".to_string(), new_target.clone()),
    ];

    // Tag first — the discriminating order.
    let tag_first = vec![
        git_ref("main", RefKind::Tag, '2'),
        git_ref("HEAD", RefKind::Head, '3'),
        git_ref("main", RefKind::Branch, '3'),
    ];
    let got = previous_targets(&tag_first, &moves);
    assert_eq!(
        got,
        vec![
            ("main".to_string(), Oid("3".repeat(40))),
            ("HEAD".to_string(), Oid("3".repeat(40))),
        ],
        "`main` moved as a branch, so its previous position is the BRANCH's \
         old target — the tag on 2222… is a different ref that is not moving"
    );

    // Branch first — the order this host's reader happens to produce. Same
    // answer, which is the point: order must not be load-bearing.
    let branch_first = vec![
        git_ref("HEAD", RefKind::Head, '3'),
        git_ref("main", RefKind::Branch, '3'),
        git_ref("main", RefKind::Tag, '2'),
    ];
    assert_eq!(
        previous_targets(&branch_first, &moves),
        got,
        "the same repository read in a different ref order must give the same \
         previous targets"
    );
}

/// **A same-second tie refuses (#576 finding 6).**
///
/// `stable_topo_order` breaks a committer-second tie by comparing oid strings,
/// and the previewed commit's oid is not the one a real run writes —
/// [`commit_tree`] writes under a fixed `preview@git-vista.invalid` identity
/// and `git_cmd` exposes no arity that could pin `GIT_COMMITTER_DATE`. So when
/// the new commit ties with an independent tip already in view, which of the
/// two is drawn on top is a coin flip, and the rows, lanes and edge
/// coordinates below it all follow that flip.
///
/// # Why this is not driven through `preview()`
///
/// It cannot be, deterministically. Forcing a live tie means making a fixture
/// commit land in the same wall-clock second as the scratch `commit-tree`
/// write, and that second is exactly the value this module cannot pin. A test
/// that raced for it would be flaky in the direction that matters — silently
/// green. So the layout is built through the pure half from a *chosen* tie and
/// handed to the guard selector, which is the wiring under test.
///
/// The three earlier reports are asserted clear on the same layout, so this is
/// the tie firing and not one of them.
///
/// # Two mutations that make this red, failing differently
///
/// 1. **REMOVES the mechanism** — delete the fourth arm of [`refusal_for`].
///    `expect` panics on `None`.
/// 2. **WEAKENS the mechanism** — move the fourth arm above the third. The
///    detached/no-branch layout in `the_refusal_says_detached_only_when_head_
///    really_is_detached` keeps its own sentence (it has no tie), but a layout
///    that meets both conditions would now report the tie instead of the
///    narrower, actionable cause; the ordering assertion at the end of this
///    test goes red.
#[test]
fn a_same_second_tie_refuses_rather_than_guessing_which_row_is_on_top() {
    use git_vista_core::model::{CommitSummary, RefKind};

    let oid_of = |d: char| Oid((0..40).map(|_| d).collect::<String>());
    let commit = |d: char, time: i64, parents: &[char]| CommitSummary {
        id: oid_of(d),
        parents: parents.iter().copied().map(oid_of).collect(),
        summary: format!("commit {d}"),
        author: "Test".to_string(),
        time,
    };
    let git_ref = |name: &str, kind: RefKind, d: char| GitRef {
        name: name.to_string(),
        kind,
        target: oid_of(d),
    };

    // `4` is an independent tip stamped in the same second the new commit will
    // carry; `3` is the checked-out tip it is committed onto.
    let before = vec![
        commit('4', 400, &['2']),
        commit('3', 300, &['2']),
        commit('2', 200, &[]),
    ];
    let refs = vec![
        git_ref("HEAD", RefKind::Head, '3'),
        git_ref("main", RefKind::Branch, '3'),
        git_ref("side", RefKind::Branch, '4'),
    ];
    let input = |time: i64| PreviewInput {
        before: before.clone(),
        refs: refs.clone(),
        head_branch: Some("main".to_string()),
        added: Some(commit('9', time, &['3'])),
        ref_moves: vec![
            ("HEAD".to_string(), oid_of('9')),
            ("main".to_string(), oid_of('9')),
        ],
        history_limit: usize::MAX,
    };

    let tied = lay_out_preview(input(400));
    assert_eq!(tied.unmatched_ref_moves, Vec::<String>::new());
    assert!(!tied.added_without_ref_moves);
    assert!(
        !tied.added_claimed_by_no_branch,
        "`main` moved onto the new commit, so the three older reports are all \
         clear — whatever refuses below is the tie and not one of them"
    );

    match refusal_for(&tied, false).expect("a coin-flip row order is not a picture") {
        PreviewUnavailable::CheckFailed { detail } => {
            assert!(
                detail.contains("committer second"),
                "the detail must name the state that was found: {detail}"
            );
            assert!(
                detail.contains("re-run the preview once the seconds differ"),
                "and this one resolves itself a moment later, unlike the \
                 detached-HEAD refusal, so it must say so: {detail}"
            );
            assert!(
                !detail.contains("colour"),
                "that is the `added_claimed_by_no_branch` sentence, and this \
                 layout's colours are fine: {detail}"
            );
        }
        other => panic!("expected CheckFailed naming the tie, got {other:?}"),
    }

    // One second later, nothing shares the second and there is nothing to
    // refuse — the guard must not swallow ordinary previews.
    let clear = lay_out_preview(input(401));
    assert!(
        refusal_for(&clear, false).is_none(),
        "at 401 the new commit is unambiguously newest, so all four reports \
         are clear and the graph may be shown"
    );

    // The ordering the block has always had: a layout meeting the third
    // condition AND the tie must report the third, which is the one with a
    // cause the caller can act on.
    let mut both = lay_out_preview(input(400));
    both.added_claimed_by_no_branch = true;
    match refusal_for(&both, true).expect("still refused") {
        PreviewUnavailable::CheckFailed { detail } => assert!(
            detail.contains("HEAD is detached"),
            "the narrower, actionable cause must stay reachable: {detail}"
        ),
        other => panic!("expected CheckFailed, got {other:?}"),
    }
}

/// **The walk and the `after` cap read the same window.** #576 finding 7 was
/// those two numbers disagreeing: `lay_out` walked `PREVIEW_HISTORY_LIMIT`
/// commits and then handed the layout no cap, so prepending the hypothetical
/// row returned `PREVIEW_HISTORY_LIMIT + 1` rows out of a window the caller had
/// asked to be `PREVIEW_HISTORY_LIMIT` wide.
///
/// # Why this test exists even though the pure core is already covered
///
/// `the_after_graph_is_bounded_by_the_window_the_caller_read` pins the
/// mechanism inside [`lay_out_preview`], and it is a good test. It cannot see
/// this defect. It passes `history_limit` in by hand, so it proves the core
/// truncates when told to — not that the *server* tells it to, with the same
/// number it walked. Putting `history_limit: usize::MAX` back into
/// [`lay_out_within`] left that test, and every other test in this repository,
/// green: measured as `survived` by `mutation_check` id 314 during the round
/// that added the fix. This test is that gap closed, and it runs against
/// production [`lay_out_within`] rather than against a hand-built input.
///
/// # Reaching the bound without five hundred commits
///
/// The real constant is 500, and a fixture that large is slow enough that
/// nobody would keep it. Passing the window as a parameter — the reason
/// [`lay_out_within`] exists — lets a three-commit repository exercise exactly
/// the same two uses. That is a test-visibility argument, not a shortcut: the
/// production path reads one binding twice, and the number it happens to hold
/// is not what can go wrong.
///
/// # Two directions, so a constant answer cannot pass
///
/// At the cap the count must *stop* at the window; one under it, the added row
/// must *grow* the graph by one. A `lay_out_within` that always returned the
/// window would pass the first and fail the second, and one that never
/// truncated would pass the second and fail the first.
///
/// # Mutation-proved two ways
///
/// 1. **Removes the mechanism** — `history_limit: window` becomes
///    `history_limit: usize::MAX`, which is finding 7 restored exactly: the
///    at-cap half reads three rows where it demanded two.
/// 2. **Weakens it** — `walk_history(repo, window)` becomes
///    `walk_history(repo, window + 1)`, so the two uses drift by one rather
///    than by everything. The at-cap half still reads three rows against two,
///    and the under-cap half still holds, which is what makes the first half
///    the load-bearing assertion rather than the pair of them agreeing.
#[test]
fn the_walk_and_the_after_cap_read_the_same_window() {
    let (_dir, repo) = revert_shape();
    let head = git::out(&repo, &["rev-parse", "HEAD"]);

    // At the cap: the window is narrower than the history, so the added row
    // displaces the oldest one instead of being added to it.
    let added = hypothetical('f', &head);
    let target = added.id.clone();
    let moves = ref_moves_to(&repo, &target.0);
    let outcome = lay_out_within(&repo, Some(added), moves, 2)
        .expect("an attached HEAD with both refs moved has an honest graph");
    let PreviewOutcome::Graph { before, after, .. } = outcome else {
        panic!("expected a graph, got {outcome:?}");
    };
    assert_eq!(
        before.rows.len(),
        2,
        "the walk is bounded by the window the caller asked for"
    );
    assert_eq!(
        after.rows.len(),
        2,
        "the after graph is bounded by the SAME window: the hypothetical row \
         displaces the oldest commit rather than making a 2-commit window \
         return 3 rows. This is #576 finding 7"
    );

    // One under the cap: nothing is truncated, so the added row is a real
    // extra row. Without this half, an implementation that always returned
    // `window` rows would pass.
    let added = hypothetical('e', &head);
    let target = added.id.clone();
    let moves = ref_moves_to(&repo, &target.0);
    let outcome = lay_out_within(&repo, Some(added), moves, 9)
        .expect("an attached HEAD with both refs moved has an honest graph");
    let PreviewOutcome::Graph { before, after, .. } = outcome else {
        panic!("expected a graph, got {outcome:?}");
    };
    assert_eq!(before.rows.len(), 3, "the fixture has three commits");
    assert_eq!(
        after.rows.len(),
        4,
        "below the cap the added commit grows the graph, so the cap is a cap \
         and not a fixed size"
    );
}
