//! Two branches that have diverged from a common base and have **not yet**
//! been merged or cherry-picked onto one another — the fixtures the graph
//! preview (#576 / M10.08) needs.
//!
//! # Why these are not conflict fixtures
//!
//! [`crate::conflict`] builds repositories *stopped mid-operation*: `git
//! merge` or `git revert` has already run, failed, and left `MERGE_HEAD` or
//! `REVERT_HEAD` and a half-resolved index on disk. That is the right shape
//! for testing what a tool does once a user is already stuck.
//!
//! The graph preview needs the opposite moment: a repository *before* the
//! operation runs, so a `preview(repo, plan)` function can be handed a `Plan`
//! and asked what merging or cherry-picking *would* produce, without git ever
//! being told to do it for real — that is the whole feature (see
//! `docs/superpowers/specs/2026-08-29-graph-preview-design.md` §1). A shape in
//! this module is never merged, never cherry-picked, never conflicted, on the
//! repository it hands back. Its claim — "this merges cleanly", "this pick
//! conflicts" — is proved once, at build time, on a disposable clone, and then
//! the clone is thrown away.
//!
//! # Why every builder proves its own claim on a clone, never on itself
//!
//! [`verify`] and [`verify_conflict`] clone the fixture into a throwaway
//! directory, check out the target branch *there*, and run the real git
//! command — `merge` or `cherry-pick` — for real. The clone is dropped when
//! the function returns; the repository handed back to the caller is
//! untouched: still two diverged branches, no merge, no pick, no
//! `MERGE_HEAD`, no `CHERRY_PICK_HEAD`. Every builder below asserts that
//! explicitly before returning.
//!
//! This mirrors the constraint the preview feature itself must satisfy — the
//! design's **A2**, "the real repository is unchanged" — for a plain reason:
//! a fixture claiming to test "does this stay unchanged" cannot itself be
//! built by leaving a mess on the one repository it hands out.
//!
//! # Why the topology has more than one commit per side
//!
//! A branch that is a single commit away from its base can be merged or
//! cherry-picked by an implementation that gets lane assignment completely
//! wrong and still *look* right, because there is only one row on each side
//! to get right. A wrong parent order or a swapped lane is invisible against
//! a one-commit stub. Every shape below gives each branch at least two
//! commits of its own before the operation, so a lane swap, an off-by-one
//! row, or a merge/pick attached under the wrong parent produces a graph that
//! is visibly — not just technically — wrong. This is what the design's §5
//! (the A5 parity test) asks for: fixtures where a wrong answer looks
//! different from a right one.
//!
//! **One shape below is a deliberate, stated exception.**
//! [`fast_forward_merge_ff_false`] needs `main` to be a plain ancestor of
//! `feature` — that *is* "fast-forwardable" — so `main` cannot also have
//! commits of its own past the branch point without stopping being that
//! shape. Its own doc comment explains where the depth goes instead.
//!
//! # Why some shapes also pin `merge.ff` in the repository's own local config
//!
//! [`fast_forward_merge_ff_false`] and [`divergent_merge_ff_only`] exist
//! because a preview or a model that decides "will this fast-forward" from
//! ancestry alone is answering a question git itself does not always answer
//! that way: `merge.ff` changes what `git merge` actually does on the same
//! topology. [`crate::git`]'s module doc explains why every builder here
//! pins `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` to `/dev/null` — so a
//! fixture's *default* behaviour cannot depend on a developer's own
//! `~/.gitconfig`. Local, per-repository config is a different layer: it is
//! measured below, not assumed, to survive that override and to be read both
//! by an ordinary `git -C <repo> merge` and by `git-vista-server`'s own
//! preview spawn (`git_cmd::sandboxed` builds its child with `-C <repo>`,
//! never with a config override of its own). A fixture whose shape depends on
//! it must therefore set it on itself, in its own `.git/config` — which is
//! also why [`clone_onto_with_config`] exists: a plain `git clone` does
//! **not** copy the source repository's local config (measured, 2026-08-30 —
//! a `merge.ff` value visible on the fixture is simply absent from
//! `git config --local --get merge.ff` in a fresh clone), so a builder that
//! wants its own claim proved on a disposable clone, the same way every other
//! builder here does, has to carry the value across explicitly.

use crate::conflict::{base_commit, stages_of};
use crate::git;
use crate::seeded::{empty, Fixture};

/// Clone `repo` into a throwaway directory and check out `onto` there.
///
/// `git clone` only ever creates a **local** branch for the branch `HEAD`
/// pointed at; every other branch in the source repository lands as a
/// remote-tracking ref (`origin/<name>`) with no local branch of the same
/// name. That is invisible to `git cherry-pick <oid>`, which names a commit
/// directly and does not care what, if anything, points at it — but it
/// breaks `git merge <name>` outright, because `<name>` is exactly the
/// argument that must resolve. So every remote branch gets a matching local
/// one before this returns, mirroring what a person cloning the repository
/// and typing `git checkout <branch>` would end up with.
///
/// Returns the scratch `TempDir` (drop it to delete the clone) and the
/// clone's path.
fn clone_onto(repo: &std::path::Path, onto: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let scratch = tempfile::tempdir().expect("create verification tempdir");
    let clone = scratch.path().join("clone");
    let repo_str = repo.to_str().expect("fixture path must be valid UTF-8");
    let clone_str = clone.to_str().expect("clone path must be valid UTF-8");
    git::run(scratch.path(), &["clone", "-q", repo_str, clone_str]);
    git::run(&clone, &["checkout", "-q", onto]);

    let remotes = git::out(
        &clone,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/remotes/origin",
        ],
    );
    for remote in remotes.lines() {
        let Some(short) = remote.strip_prefix("origin/") else {
            continue;
        };
        if short == "HEAD" || short == onto {
            // `onto` already has a local branch from the checkout above;
            // recreating it would only print a harmless "already exists".
            continue;
        }
        git::run(&clone, &["branch", short, remote]);
    }

    (scratch, clone)
}

/// Set `key = value` in `repo`'s own **local** `.git/config` — see the module
/// doc's "Why some shapes also pin `merge.ff`" section for why this is the
/// one config layer a fixture built through [`crate::git`] can both reach and
/// rely on.
fn set_local_config(repo: &std::path::Path, key: &str, value: &str) {
    git::run(repo, &["config", "--local", key, value]);
}

/// [`clone_onto`], then copy each `(key, value)` into the clone's own local
/// config — because `git clone` does not do that for you. See the module
/// doc's "Why some shapes also pin `merge.ff`" section for the measurement
/// this relies on.
fn clone_onto_with_config(
    repo: &std::path::Path,
    onto: &str,
    config: &[(&str, &str)],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let (scratch, clone) = clone_onto(repo, onto);
    for (key, value) in config {
        set_local_config(&clone, key, value);
    }
    (scratch, clone)
}

/// Clone `repo`, check out `onto` there, and run the real git command `args`
/// against the clone — never against the fixture itself. Returns whether it
/// succeeded.
///
/// This is how each builder in this module proves its shape true: by asking
/// real git to actually perform the merge or cherry-pick, on a disposable
/// copy, exactly as the eventual preview code (and the existing
/// `merge --no-edit` / cherry-pick server handlers) will later do on a real
/// one. The clone is gone by the time this function returns; the fixture
/// handed back to the caller was never touched.
fn verify(repo: &std::path::Path, onto: &str, args: &[&str]) -> bool {
    let (_scratch, clone) = clone_onto(repo, onto);
    git::try_run(&clone, args)
}

/// Like [`verify`], but for a cherry-pick expected to *conflict*: clones
/// `repo`, checks out `onto`, cherry-picks `commit` there, and reports
/// whether it succeeded, whether `CHERRY_PICK_HEAD` was left behind, and
/// which index stages `path` ended up at — all read from the clone before it
/// is dropped, using [`stages_of`], the same reader `crate::conflict` trusts
/// for its own shapes.
fn verify_conflict(
    repo: &std::path::Path,
    onto: &str,
    commit: &str,
    path: &str,
) -> (bool, bool, Vec<u8>) {
    let (_scratch, clone) = clone_onto(repo, onto);
    let ok = git::try_run(&clone, &["cherry-pick", "--quiet", commit]);
    let head_exists = clone.join(".git/CHERRY_PICK_HEAD").exists();
    let stages = stages_of(&clone, path);
    (ok, head_exists, stages)
}

/// The number of `parent` lines a commit's raw object records — i.e. how many
/// parents it has. Read from `git cat-file -p`, git's own view, not derived
/// from how the builder thinks it built the commit.
fn parent_count(repo: &std::path::Path, commit: &str) -> usize {
    git::out(repo, &["cat-file", "-p", commit])
        .lines()
        .filter(|line| line.starts_with("parent "))
        .count()
}

/// Two branches, `main` and `feature`, diverged from one shared base commit,
/// each two commits deep, touching entirely disjoint files — merging
/// `feature` into `main` succeeds with no conflict.
///
/// ## What git actually put on disk
///
/// Five commits total: one base (`shared.txt`), two on `main`
/// (`main-alpha.txt`, `main-beta.txt`), two on `feature` (`feature-one.txt`,
/// `feature-two.txt`). `HEAD` is `main`. Neither branch's commits touch a
/// path the other branch touches, so a real 3-way merge has nothing to
/// reconcile — it unions the two trees onto the shared base.
///
/// ## Why it matters
///
/// This is the shape **A5** needs for `GitOperation::MergeBranch`: a graph
/// with real width *before* the merge (two independent two-commit chains, not
/// two single commits) and a real join at the merge commit, whose two
/// parents must be `main`'s tip and `feature`'s tip **in that order**. A
/// preview that transposes the parent order, or lays either chain out in the
/// wrong lane, produces a graph a person can tell apart from the real one at
/// a glance — see the module doc for why that width is deliberate.
pub fn merge_clean_two_branch() -> Fixture {
    let (dir, repo) = empty();
    base_commit(&repo, &[("shared.txt", b"shared\n")]);

    git::run(&repo, &["checkout", "-q", "-b", "feature"]);
    git::write(&repo, "feature-one.txt", b"feature work one\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "feature: add one.txt"]);
    git::write(&repo, "feature-two.txt", b"feature work two\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "feature: add two.txt"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-alpha.txt", b"main work alpha\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: add alpha.txt"]);
    git::write(&repo, "main-beta.txt", b"main work beta\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: add beta.txt"]);

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "3",
        "main must be base + two commits, or the graph has no width to get wrong"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "feature"]),
        "3",
        "feature must be base + two commits, for the same reason"
    );
    assert_eq!(
        git::out(&repo, &["merge-base", "main", "feature"]),
        git::out(&repo, &["rev-parse", "main~2"]),
        "the two branches must share exactly the base commit as their ancestor"
    );

    assert!(
        verify(&repo, "main", &["merge", "-q", "feature"]),
        "the two branches touch disjoint files and must merge without a conflict"
    );
    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "the fixture handed back must stay pre-merge: verify() runs on a clone, not here"
    );
    (dir, repo)
}

/// `main`, two commits deep, with `feature` branched off its tip and two
/// commits **strictly ahead** of it — the shape `git merge` would ordinarily
/// resolve as a fast-forward, just moving `main`'s ref — except this
/// repository's own local `merge.ff` is `false`, so a real merge refuses to
/// fast-forward and writes a genuine two-parent commit instead.
///
/// ## Why this is not `merge_clean_two_branch`
///
/// [`merge_clean_two_branch`] is two branches that have *each* moved past
/// their shared base — neither is an ancestor of the other, so **no**
/// `merge.ff` setting changes what a real merge does there: a merge commit
/// was already the only possible outcome. This fixture is the opposite
/// topology, the one a `merge.ff` setting can actually change the outcome
/// of: `main` never moves past the branch point, so `feature` is strictly
/// ahead and a default-configured `git merge` would just move `main`'s ref.
///
/// ## Why `main` has no commits of its own after the branch point
///
/// The module doc's "give both sides real depth" rule cannot hold here: a
/// fast-forwardable pair means `main` is an ancestor of `feature`, which by
/// definition means `main` has **zero** commits past the point `feature`
/// diverged from — a `main` with commits of its own there would not be an
/// ancestor of `feature` any more, and this would quietly become
/// `merge_clean_two_branch` again. The depth the module doc asks for is
/// given **before** the branch point instead: `main` is two commits deep
/// (`root`, `main: second commit`) when `feature` branches off it and adds
/// two more of its own, so the graph still has real width above the join —
/// it is only `main`'s *branch-local* depth, past the point that matters
/// here, that is necessarily zero.
///
/// ## What git actually put on disk
///
/// Four commits: `root`, `main: second commit` (both on `main`, which never
/// moves again after this), then `feature` branches off and adds
/// `feature: add one.txt` and `feature: add two.txt`. `merge.ff=false` is
/// written into this repository's own local `.git/config` — see the module
/// doc's "Why some shapes also pin `merge.ff`" section for why that is the
/// layer that survives this crate's `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`
/// overrides.
///
/// ## Why it matters
///
/// A preview or a model that decides "fast-forward vs. real merge" purely
/// from ancestry — `merge-base(head, tip) == head` — is *confidently wrong*
/// on exactly this repository. Measured, 2026-08-30, on a throwaway
/// repository built this same way: with `merge.ff=false` set locally, a real
/// `git merge --no-edit feature` from `main` printed "Merge made by the
/// 'ort' strategy", added one commit to `main`, and gave that new tip **two**
/// parents — not the zero-commit ref move an ancestry-only classifier would
/// draw.
pub fn fast_forward_merge_ff_false() -> Fixture {
    let (dir, repo) = empty();
    git::write(&repo, "root.txt", b"root\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "root"]);
    git::write(&repo, "main-second.txt", b"main second commit\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: second commit"]);

    git::run(&repo, &["checkout", "-q", "-b", "feature"]);
    git::write(&repo, "feature-one.txt", b"feature work one\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "feature: add one.txt"]);
    git::write(&repo, "feature-two.txt", b"feature work two\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "feature: add two.txt"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    set_local_config(&repo, "merge.ff", "false");

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "2",
        "main must be root + one commit, and must never move again, or this is not \
         a fast-forwardable pair"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "feature"]),
        "4",
        "feature must be main's two commits plus two more of its own, or there is \
         no width above the join"
    );
    assert_eq!(
        git::out(&repo, &["merge-base", "main", "feature"]),
        git::out(&repo, &["rev-parse", "main"]),
        "main must be an ancestor of feature, or this is not fast-forwardable at all"
    );

    // Read back the value actually on disk, rather than hard-coding "false"
    // a second time here: a mutation that changed what `set_local_config`
    // above wrote must be caught by this readback, not silently bypassed by
    // a verification step that assumes its own answer.
    let configured_ff = git::out(&repo, &["config", "--local", "--get", "merge.ff"]);
    assert_eq!(
        configured_ff, "false",
        "merge.ff must be set to false on the fixture itself, or its whole claim is unproven"
    );

    let feature_tip = git::out(&repo, &["rev-parse", "feature"]);
    let (_scratch, clone) = clone_onto_with_config(&repo, "main", &[("merge.ff", &configured_ff)]);
    assert!(
        git::try_run(&clone, &["merge", "--no-edit", "feature"]),
        "a fast-forwardable merge must still succeed with merge.ff=false — \
         it is refused as a fast-forward, not blocked outright"
    );
    let merged = git::out(&clone, &["rev-parse", "main"]);
    assert_ne!(
        merged, feature_tip,
        "merge.ff=false must produce a NEW commit rather than moving main to \
         feature's own oid — that is the whole discriminator this fixture exists to prove"
    );
    assert_eq!(
        parent_count(&clone, "main"),
        2,
        "a refused fast-forward is a real two-parent merge commit"
    );

    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "the fixture handed back must stay pre-merge: the merge above ran on a clone"
    );
    (dir, repo)
}

/// Two branches, `main` and `rival`, each two commits past a shared base,
/// touching disjoint files — a real 3-way merge would succeed cleanly under
/// git's defaults — except this repository's own local `merge.ff` is `only`,
/// so a real merge **refuses outright** rather than writing that commit.
///
/// ## Why this is not a config twist on `merge_clean_two_branch`
///
/// [`merge_clean_two_branch`]'s own contract — a real merge of this topology
/// succeeds — is used elsewhere for the plain clean-merge case and must not
/// start depending on which config value happens to be set when a caller
/// reaches for it. This is a separate builder, with its own files and its
/// own commits, that proves the opposite claim on the same *kind* of
/// topology: divergent, disjoint files, would merge cleanly under git's
/// defaults, and still refuses once `merge.ff=only` is set, because neither
/// branch is an ancestor of the other and `only` accepts nothing but a
/// fast-forward.
///
/// ## What git actually put on disk
///
/// Five commits: one base (`shared-b.txt`), two on `main`
/// (`main-alpha-b.txt`, `main-beta-b.txt`), two on `rival` (`rival-one.txt`,
/// `rival-two.txt`). `merge.ff=only` is written into this repository's own
/// local `.git/config`.
///
/// ## Why it matters
///
/// A preview or a model that decides "will this merge succeed" purely from
/// "the files don't overlap" is *confidently wrong* here. Measured,
/// 2026-08-30, on a throwaway repository built this same way: with
/// `merge.ff=only` set locally, a real `git merge --no-edit rival` from
/// `main` exited `128` with "Not possible to fast-forward, aborting" and
/// moved nothing — not the clean merge commit a content-only classifier
/// would draw.
pub fn divergent_merge_ff_only() -> Fixture {
    let (dir, repo) = empty();
    base_commit(&repo, &[("shared-b.txt", b"shared\n")]);

    git::run(&repo, &["checkout", "-q", "-b", "rival"]);
    git::write(&repo, "rival-one.txt", b"rival work one\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "rival: add one.txt"]);
    git::write(&repo, "rival-two.txt", b"rival work two\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "rival: add two.txt"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-alpha-b.txt", b"main work alpha\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: add alpha.txt"]);
    git::write(&repo, "main-beta-b.txt", b"main work beta\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: add beta.txt"]);
    set_local_config(&repo, "merge.ff", "only");

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "3",
        "main must be base + two commits, or the graph has no width to get wrong"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "rival"]),
        "3",
        "rival must be base + two commits, for the same reason"
    );
    let merge_base = git::out(&repo, &["merge-base", "main", "rival"]);
    assert_ne!(
        merge_base,
        git::out(&repo, &["rev-parse", "main"]),
        "main must not be an ancestor of rival, or merge.ff=only would accept the merge"
    );
    assert_ne!(
        merge_base,
        git::out(&repo, &["rev-parse", "rival"]),
        "rival must not be an ancestor of main either — neither side may be a fast-forward"
    );

    // Read back the value actually on disk, rather than hard-coding "only" a
    // second time here — see the identical note in
    // `fast_forward_merge_ff_false` for why.
    let configured_ff = git::out(&repo, &["config", "--local", "--get", "merge.ff"]);
    assert_eq!(
        configured_ff, "only",
        "merge.ff must be set to only on the fixture itself, or its whole claim is unproven"
    );

    let (_scratch, clone) = clone_onto_with_config(&repo, "main", &[("merge.ff", &configured_ff)]);
    let main_before = git::out(&clone, &["rev-parse", "main"]);
    assert!(
        !git::try_run(&clone, &["merge", "--no-edit", "rival"]),
        "merge.ff=only must refuse a merge that is not a fast-forward"
    );
    assert_eq!(
        git::out(&clone, &["rev-parse", "main"]),
        main_before,
        "a refused merge must move nothing"
    );
    assert!(
        !clone.join(".git/MERGE_HEAD").exists(),
        "merge.ff=only refuses before ever starting a merge — there is no MERGE_HEAD to abort"
    );

    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "the fixture handed back must stay pre-merge: the attempt above ran on a clone"
    );
    (dir, repo)
}

/// A branch `topic`, two commits deep from a shared base, whose tip edits a
/// **different region of the same file** `main`'s own tip already edited —
/// a genuine three-way merge, not an add of an untouched path, and it applies
/// cleanly onto `main`.
///
/// ## Why this is not an add-only shape
///
/// An earlier version of this fixture had `topic`'s tip add a brand-new file
/// nothing else touched. That is the weakest possible "applies cleanly" case:
/// an add on an unused path cannot conflict under almost any three-way merge
/// implementation, correct or broken, so a preview computing the wrong merge
/// base would still happen to answer "clean" here — the fixture would not
/// have noticed. Editing the *same file* `main` also edited, far enough away
/// that the two hunks do not overlap, is a real three-way merge result: it
/// only stays clean because the merge base and the two diffs are the right
/// ones, which is the property worth pinning.
///
/// ## What git actually put on disk
///
/// `target.txt` starts at the base with ten lines, `line 1` through
/// `line 10`. `topic` has two commits past base: an unrelated
/// `topic-setup.txt`, then a rewrite of line 9. `main` has one commit past
/// base: a rewrite of line 2. The two edits are eight lines apart — well
/// outside git's default three-line diff context — so cherry-picking
/// `topic`'s tip onto `main` merges both edits into one file with no overlap.
///
/// `topic`'s tip is asserted to have exactly one parent: `git cherry-pick`
/// refuses a merge commit outright unless told which parent is the mainline
/// (`-m`), so this fixture is never accidentally that harder case.
///
/// ## Why it matters
///
/// **A5** needs `GitOperation::CherryPick` applied where both sides have real
/// depth *and* touch the same file — `topic` is not one commit from its base,
/// and neither is `main`, and both have opinions about `target.txt`. A
/// preview that anchors the picked commit under the wrong parent, computes
/// the wrong merge base, or lays either chain out in the wrong lane produces
/// a graph whose shape — or whose verdict — is visibly wrong, rather than
/// accidentally correct because the only path in play was one nothing else
/// could have touched.
///
/// It is also one half of a pair a **tree-comparing** A5 test needs, and
/// [`cherry_pick_already_applied`] is the other: here, the merged tree is a
/// real combination of two non-overlapping edits and is provably *not* equal
/// to `main`'s own tree; there, both sides make the identical edit and the
/// merged tree is provably *equal* to `main`'s own tree. A test that asserts
/// tree identity only against one of the two would still pass a preview that
/// always answers "different" (or always "same") — the pair is what makes a
/// wrong tree comparison visible in either direction.
pub fn cherry_pick_clean() -> Fixture {
    let (dir, repo) = empty();
    let ancestor: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    base_commit(&repo, &[("target.txt", ancestor.as_bytes())]);

    git::run(&repo, &["checkout", "-q", "-b", "topic"]);
    git::write(&repo, "topic-setup.txt", b"topic setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "topic: setup"]);
    let topic_edit = ancestor.replace("line 9\n", "line 9 edited by topic\n");
    git::write(&repo, "target.txt", topic_edit.as_bytes());
    git::run(&repo, &["commit", "-q", "-am", "topic: edit line nine"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    let main_edit = ancestor.replace("line 2\n", "line 2 edited by main\n");
    git::write(&repo, "target.txt", main_edit.as_bytes());
    git::run(&repo, &["commit", "-q", "-am", "main: edit line two"]);

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "topic"]),
        "3",
        "topic must be base + two commits, or there is no depth to place the pick under"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "2",
        "main must have diverged too, or cherry-picking onto it proves nothing about lane assignment"
    );
    assert_eq!(
        parent_count(&repo, "topic"),
        1,
        "topic's tip must be an ordinary single-parent commit, or git cherry-pick needs -m"
    );

    let pick = git::out(&repo, &["rev-parse", "topic"]);
    assert!(
        verify(&repo, "main", &["cherry-pick", "--quiet", &pick]),
        "topic's edit is eight lines from main's and must apply without a conflict"
    );
    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the fixture handed back must stay pre-pick: verify() runs on a clone, not here"
    );
    (dir, repo)
}

/// A branch `topic`, whose tip edits one line of a file — but `main`'s own
/// tip has **already made the identical edit**, independently, in its own
/// commit. Cherry-picking `topic`'s tip onto `main` is not "clean" the way
/// [`cherry_pick_clean`] is: `git cherry-pick` computes an empty patch,
/// refuses to create a commit, and leaves the repository **mid-sequence**.
///
/// ## Why this needs its own topology, not a flag on `cherry_pick_clean`
///
/// The discriminator here is not a git setting — it is a **fact about the
/// two trees**: does the tree a merge of this pick would write already equal
/// `main`'s current tree? [`cherry_pick_clean`] is built so that fact is
/// false (the two edits are eight lines apart, so they combine into
/// something neither side already has); this fixture is built so it is
/// true — both sides make the *exact same* edit — which is a different
/// commit graph, not a setting toggled on the same one.
///
/// ## What git actually put on disk
///
/// `target.txt` starts at the base with ten lines. `topic` has two commits
/// past base: an unrelated `topic-applied-setup.txt`, then a rewrite of
/// line 9. `main` has two commits past base: an unrelated
/// `main-applied-setup.txt`, then the **identical** rewrite of line 9 —
/// independently authored, not cherry-picked from `topic`.
///
/// Measured, 2026-08-30, against this exact shape: `git merge-tree
/// --write-tree --merge-base=<topic's parent> <main> <topic>` exits `0` (no
/// conflict) and writes a tree **byte-identical to `main`'s own current
/// tree** — the fact this builder asserts below, read from git's own
/// `rev-parse <tree>` output, never from how the builder thinks it built the
/// commits. A checker that reads "exit 0" alone as "clean, draw an added
/// commit" would draw a hypothetical commit that changes nothing, for a real
/// operation that refuses.
///
/// A real `git cherry-pick --quiet <topic>` on `main`, separately measured on
/// a clone: exits non-zero, prints "The previous cherry-pick is now empty,
/// possibly due to conflict resolution", leaves `.git/CHERRY_PICK_HEAD` on
/// disk, and leaves the working tree clean (`git status --porcelain` empty)
/// — a real mid-sequence state a user must resolve with `--skip`,
/// `--allow-empty`, or `--abort`, not the clean added row a tree-blind
/// checker would draw.
///
/// ## Why it matters
///
/// This is the fixture the design's §5 tree-parity discriminator needs and
/// did not have: something whose cherry-pick handling never compares the
/// merged tree against `HEAD`'s own tree passes every other cherry-pick
/// fixture in this catalogue and still has nothing here to catch it drawing
/// a confidently wrong "added" row for an operation that git itself refuses.
pub fn cherry_pick_already_applied() -> Fixture {
    let (dir, repo) = empty();
    let ancestor: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    base_commit(&repo, &[("target.txt", ancestor.as_bytes())]);
    let edited = ancestor.replace("line 9\n", "line 9 edited identically\n");

    git::run(&repo, &["checkout", "-q", "-b", "topic"]);
    git::write(&repo, "topic-applied-setup.txt", b"topic setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "topic: setup"]);
    git::write(&repo, "target.txt", edited.as_bytes());
    git::run(&repo, &["commit", "-q", "-am", "topic: edit line nine"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-applied-setup.txt", b"main setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: unrelated setup"]);
    git::write(&repo, "target.txt", edited.as_bytes());
    git::run(
        &repo,
        &[
            "commit",
            "-q",
            "-am",
            "main: independently made the identical edit",
        ],
    );

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "topic"]),
        "3",
        "topic must be base + two commits, matching the depth the other cherry-pick shapes carry"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "3",
        "main must have equal depth, for the same reason"
    );
    assert_eq!(
        parent_count(&repo, "topic"),
        1,
        "topic's tip must be an ordinary single-parent commit, or git cherry-pick needs -m"
    );

    let pick = git::out(&repo, &["rev-parse", "topic"]);
    let pick_parent = git::out(&repo, &["rev-parse", "topic^"]);
    let main_tip = git::out(&repo, &["rev-parse", "main"]);
    let merge_base_flag = format!("--merge-base={pick_parent}");
    let raw = git::out(
        &repo,
        &[
            "merge-tree",
            "-z",
            "--write-tree",
            &merge_base_flag,
            &main_tip,
            &pick,
        ],
    );
    // `-z`'s clean-case stdout is `<tree oid>\0` — one record, NUL-terminated,
    // exactly as `git_vista_server::preview::parse_merge_tree_tree` reads it.
    let tree = raw.split('\u{0}').next().unwrap_or_default().to_string();
    let main_tree = git::out(&repo, &["rev-parse", "main^{tree}"]);
    assert_eq!(
        tree, main_tree,
        "the whole discriminator: the tree a merge of this pick would write must already be \
         main's own current tree, or this fixture is just cherry_pick_clean again"
    );

    let (_scratch, clone) = clone_onto(&repo, "main");
    let ok = git::try_run(&clone, &["cherry-pick", "--quiet", &pick]);
    assert!(
        !ok,
        "an already-applied pick must refuse — git has nothing left to commit"
    );
    assert!(
        clone.join(".git/CHERRY_PICK_HEAD").exists(),
        "a refused already-applied pick still leaves the repository mid-sequence"
    );
    assert!(
        git::out(&clone, &["status", "--porcelain"]).is_empty(),
        "the working tree is clean — the patch really is empty, not merely unresolved"
    );

    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the fixture handed back must stay pre-pick: the attempt above ran on a clone"
    );
    (dir, repo)
}

/// A branch `topic`, two commits deep from a shared base, whose tip edits the
/// exact same line of the exact same file `main`'s own tip already edited —
/// cherry-picking it onto `main` conflicts.
///
/// ## What git actually put on disk
///
/// Both `main` and `topic` branch from one base commit holding `target.txt`
/// at three lines. Each side first makes an unrelated commit — so neither
/// branch is a single commit from its base, for the reason the module doc
/// gives — and then rewrites `target.txt`'s middle line to a different value.
/// A cherry-pick's three-way merge compares the picked commit's parent (the
/// shared base) against `main`'s current tip and the picked commit itself:
/// exactly the modify/modify shape [`crate::conflict::conflict_modify_modify`]
/// documents, reached here by `cherry-pick` instead of `merge` — same three
/// stages, `CHERRY_PICK_HEAD` in place of `MERGE_HEAD`.
///
/// ## Why it matters
///
/// The design's **A3**/mutation-`M2` contract is that a conflicting operation
/// must return `Conflict`, never a guessed graph. `crate::conflict` already
/// proves that arm reachable through a merge; this is the fixture that proves
/// it reachable through `GitOperation::CherryPick` specifically — a preview
/// whose `Conflict` detection is wired to `merge`'s exit code alone, and never
/// checked against a cherry-pick's, would pass every test built only on the
/// merge shape and still be wrong here.
pub fn cherry_pick_conflict() -> Fixture {
    let (dir, repo) = empty();
    base_commit(
        &repo,
        &[("target.txt", b"line one\nline two\nline three\n")],
    );

    git::run(&repo, &["checkout", "-q", "-b", "topic"]);
    git::write(&repo, "topic-setup.txt", b"topic setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "topic: unrelated setup"]);
    git::write(
        &repo,
        "target.txt",
        b"line one\nline two edited by topic\nline three\n",
    );
    git::run(&repo, &["commit", "-q", "-am", "topic: edit line two"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-setup.txt", b"main setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: unrelated setup"]);
    git::write(
        &repo,
        "target.txt",
        b"line one\nline two edited by main\nline three\n",
    );
    git::run(&repo, &["commit", "-q", "-am", "main: edit line two"]);

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "topic"]),
        "3",
        "topic must be base + two commits, matching the depth the other two shapes carry"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "3",
        "main must have equal depth, or this is a degenerate one-row conflict"
    );

    let pick = git::out(&repo, &["rev-parse", "topic"]);
    let (ok, cherry_pick_head_exists, stages) = verify_conflict(&repo, "main", &pick, "target.txt");
    assert!(!ok, "both sides edited the same line and must conflict");
    assert!(
        cherry_pick_head_exists,
        "a conflicted cherry-pick must leave CHERRY_PICK_HEAD on disk, in the clone"
    );
    assert_eq!(
        stages,
        vec![1, 2, 3],
        "a modify/modify conflict carries all three stages, same as the merge case"
    );

    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the fixture handed back must stay pre-pick: verify_conflict() runs on a clone, not here"
    );
    (dir, repo)
}

/// A branch `incoming`, two commits deep from a shared base, whose tip edits
/// the exact same line of the exact same file `main`'s own tip already
/// edited — merging `incoming` into `main` conflicts. The `git merge` twin of
/// [`cherry_pick_conflict`].
///
/// ## What git actually put on disk
///
/// Both `main` and `incoming` branch from one base commit holding
/// `shared.txt` at three lines. Each side first makes an unrelated commit,
/// then rewrites `shared.txt`'s middle line to a different value — the same
/// modify/modify shape [`crate::conflict::conflict_modify_modify`] documents,
/// reached here by a pre-merge repository and a real `git merge` run against
/// a disposable clone, rather than by leaving the conflict on the fixture
/// itself.
///
/// ## Why it matters
///
/// Before this fixture there was no conflicting-merge shape in this
/// catalogue provable **pre-merge**: [`cherry_pick_conflict`] proves
/// `GitOperation::CherryPick` refuses correctly, and
/// [`crate::conflict::conflict_modify_modify`] proves what a conflict looks
/// like once git has already stopped mid-merge, but nothing proved what a
/// preview of `GitOperation::MergeBranch` must do when the merge it is asked
/// to draw would conflict: report `Conflict`, never a guessed clean graph. A
/// checker whose conflict detection is wired only to `merge-tree`'s exit code
/// on the revert/cherry-pick paths, and never exercised on a real
/// `git merge`, would pass every other fixture in the catalogue and still
/// draw a clean graph for this repository.
pub fn merge_conflict() -> Fixture {
    let (dir, repo) = empty();
    base_commit(
        &repo,
        &[("shared.txt", b"line one\nline two\nline three\n")],
    );

    git::run(&repo, &["checkout", "-q", "-b", "incoming"]);
    git::write(&repo, "incoming-setup.txt", b"incoming setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "incoming: unrelated setup"]);
    git::write(
        &repo,
        "shared.txt",
        b"line one\nline two edited by incoming\nline three\n",
    );
    git::run(&repo, &["commit", "-q", "-am", "incoming: edit line two"]);

    git::run(&repo, &["checkout", "-q", "main"]);
    git::write(&repo, "main-conflict-setup.txt", b"main setup\n");
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "main: unrelated setup"]);
    git::write(
        &repo,
        "shared.txt",
        b"line one\nline two edited by main\nline three\n",
    );
    git::run(&repo, &["commit", "-q", "-am", "main: edit line two"]);

    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "incoming"]),
        "3",
        "incoming must be base + two commits, matching the depth the other conflict shapes carry"
    );
    assert_eq!(
        git::out(&repo, &["rev-list", "--count", "main"]),
        "3",
        "main must have equal depth, or this is a degenerate one-row conflict"
    );

    let (_scratch, clone) = clone_onto(&repo, "main");
    let ok = git::try_run(&clone, &["merge", "--no-edit", "incoming"]);
    assert!(!ok, "both sides edited the same line and must conflict");
    assert!(
        clone.join(".git/MERGE_HEAD").exists(),
        "a conflicted merge must leave MERGE_HEAD on disk, in the clone"
    );
    assert_eq!(
        stages_of(&clone, "shared.txt"),
        vec![1, 2, 3],
        "a modify/modify merge conflict carries all three stages"
    );

    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "the fixture handed back must stay pre-merge: the attempt above ran on a clone"
    );
    (dir, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A note on where these mutations are actually caught, so the comment on
    // each test below claims only what is true.
    //
    // Every builder in this module (`merge_clean_two_branch`,
    // `cherry_pick_clean`, `cherry_pick_conflict`) asserts its own shape
    // internally, before returning — the same house style `crate::conflict`
    // uses. That is deliberate: it means *any* caller, not just this test
    // file, gets an immediate, specific panic the moment the fixture stops
    // being what its name claims, including a future A5 parity test in
    // another crate that just calls e.g. `merge_clean_two_branch()` directly.
    //
    // The consequence for mutation testing: because every test below starts
    // by calling the builder fresh, a mutation that breaks a builder-level
    // assertion panics *inside the builder*, before the test's own body ever
    // runs. Both mutations named in each comment below are still genuinely
    // `caught`, and still fail at different lines with different messages —
    // but the thing doing the catching is the builder's own assertion, not
    // an independent check inside the test. The split into multiple tests
    // per fixture exists so a maintainer scanning test *names* finds "stays
    // clean" and "has real width" as two separately documented properties,
    // not because the tests are independent tripwires — they are not, since
    // the builder already fires first. Removing a builder-level assertion to
    // make a test the sole tripwire would trade away that fail-fast-for-every-
    // caller property, which is worse.

    /// Pins the whole `merge_clean_two_branch` claim: the fixture handed back
    /// is pre-merge, and a real merge of `feature` into `main` on a
    /// disposable clone is clean.
    ///
    /// Two mutations that must turn this red, in different ways (see the note
    /// above for *where* — both are caught inside the builder itself):
    /// - **removes the mechanism**: make `feature`'s second commit touch
    ///   `main-beta.txt` (a path `main`'s own second commit also touches)
    ///   instead of `feature-two.txt`. The clone's merge starts conflicting,
    ///   so the builder's `verify(..)` returns `false` and its own
    ///   `assert!` panics with "must merge without a conflict".
    /// - **weakens it**: drop `feature`'s second commit, leaving each branch
    ///   one commit from base. The merge stays clean, but the builder's own
    ///   `rev-list --count feature == "3"` assertion panics first, with a
    ///   different message, before the merge is even attempted.
    #[test]
    fn merge_clean_two_branch_merges_cleanly_and_stays_pre_merge() {
        let (_dir, repo) = merge_clean_two_branch();
        assert_eq!(
            git::out(&repo, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(!repo.join(".git/MERGE_HEAD").exists());
        assert!(verify(&repo, "main", &["merge", "-q", "feature"]));
    }

    /// Names the width property on its own, for a reader scanning test names
    /// — see the mutation note on the module's `mod tests` doc comment above
    /// for which mutation this actually turns red on and where.
    #[test]
    fn merge_clean_two_branch_has_real_width() {
        let (_dir, repo) = merge_clean_two_branch();
        assert_eq!(git::out(&repo, &["rev-list", "--count", "main"]), "3");
        assert_eq!(git::out(&repo, &["rev-list", "--count", "feature"]), "3");
    }

    /// Pins `cherry_pick_clean`: the fixture is pre-pick, and cherry-picking
    /// `topic`'s tip onto `main` on a clone succeeds.
    ///
    /// Two mutations that must turn this red, in different ways (again,
    /// caught inside the builder — see the note above):
    /// - **removes the mechanism**: make `topic`'s edit land on the exact
    ///   same line `main`'s edit touches (line 2, instead of line 9) — the
    ///   clone's cherry-pick now conflicts, `verify` returns `false`, and
    ///   the builder's own `assert!` panics.
    /// - **weakens it**: drop `topic: setup`, leaving `topic` one commit from
    ///   base. The pick is still clean, but the builder's own
    ///   `rev-list --count topic == "3"` assertion panics first, with a
    ///   different message.
    #[test]
    fn cherry_pick_clean_applies_and_stays_pre_pick() {
        let (_dir, repo) = cherry_pick_clean();
        assert!(!repo.join(".git/CHERRY_PICK_HEAD").exists());
        let pick = git::out(&repo, &["rev-parse", "topic"]);
        assert!(verify(&repo, "main", &["cherry-pick", "--quiet", &pick]));
    }

    /// Names the depth property on its own — see the module's `mod tests`
    /// doc comment for which mutation this turns red on and where.
    #[test]
    fn cherry_pick_clean_has_real_depth_on_both_sides() {
        let (_dir, repo) = cherry_pick_clean();
        assert_eq!(git::out(&repo, &["rev-list", "--count", "topic"]), "3");
        assert_eq!(git::out(&repo, &["rev-list", "--count", "main"]), "2");
        assert_eq!(parent_count(&repo, "topic"), 1);
    }

    /// Pins `cherry_pick_conflict`: the fixture is pre-pick, and
    /// cherry-picking `topic`'s tip onto `main` on a clone genuinely
    /// conflicts — leaving `CHERRY_PICK_HEAD` and all three index stages,
    /// same as a merge conflict would.
    ///
    /// This test re-runs `verify_conflict` itself rather than only trusting
    /// the builder's own copy, but the builder's internal `assert!(!ok, ...)`
    /// and `assert_eq!(stages, vec![1, 2, 3], ...)` still fire first, inside
    /// `cherry_pick_conflict()`, before this test's body is reached — see the
    /// `mod tests` note above. What still differs between the two mutations
    /// is genuinely their message:
    /// - **removes the mechanism**: make `topic` edit a line `main` never
    ///   touched (e.g. append a fourth line instead of rewriting line two).
    ///   The clone's cherry-pick now succeeds, `ok` becomes `true`, and the
    ///   builder's `assert!(!ok, ...)` panics with "must conflict" — the
    ///   conflict this fixture exists to provide is gone.
    /// - **weakens it**: change the builder's own stage assertion from the
    ///   exact `vec![1, 2, 3]` to a loose `!stages.is_empty()`. A shape that
    ///   regressed to, say, an add/add conflict (stages `[2, 3]`, no common
    ///   ancestor — see `crate::conflict::conflict_add_add`) would still pass
    ///   a non-empty check; only the exact-vector assertion catches that
    ///   regression, and it panics with a different message ("assertion
    ///   `left == right` failed") at a different line than the first
    ///   mutation.
    #[test]
    fn cherry_pick_conflict_conflicts_for_real_and_stays_pre_pick() {
        let (_dir, repo) = cherry_pick_conflict();
        assert!(!repo.join(".git/CHERRY_PICK_HEAD").exists());

        let pick = git::out(&repo, &["rev-parse", "topic"]);
        let (ok, cherry_pick_head_exists, stages) =
            verify_conflict(&repo, "main", &pick, "target.txt");
        assert!(!ok, "expected the cherry-pick to fail with a conflict");
        assert!(cherry_pick_head_exists);
        assert_eq!(stages, vec![1, 2, 3]);
    }

    /// The distinction the design's `M2` mutation depends on: a merge
    /// conflict and a cherry-pick conflict must be told apart by which
    /// sequencer file is on disk, or `Conflict` cannot be attributed to the
    /// right operation.
    #[test]
    fn a_cherry_pick_conflict_writes_cherry_pick_head_not_merge_head() {
        let (_dir, repo) = cherry_pick_conflict();
        let pick = git::out(&repo, &["rev-parse", "topic"]);
        let (_scratch, clone) = clone_onto(&repo, "main");
        let _ = git::try_run(&clone, &["cherry-pick", "--quiet", &pick]);
        assert!(clone.join(".git/CHERRY_PICK_HEAD").exists());
        assert!(!clone.join(".git/MERGE_HEAD").exists());
    }

    /// Pins `fast_forward_merge_ff_false`: a real merge here writes a
    /// two-parent commit rather than moving `main`'s ref.
    ///
    /// Two mutations that must turn this red, in different ways (both caught
    /// inside the builder — see the note above):
    /// - **removes the mechanism**: drop the `set_local_config(&repo,
    ///   "merge.ff", "false")` line (or change `"false"` to `"true"`). A real
    ///   merge on the clone now fast-forwards: `merged` equals `feature_tip`,
    ///   and the builder's `assert_ne!(merged, feature_tip, ...)` panics with
    ///   "must produce a NEW commit".
    /// - **weakens it**: drop `main: second commit`, leaving `main` a single
    ///   commit. The fast-forward is still refused, but the builder's own
    ///   `rev-list --count main == "2"` assertion panics first, with a
    ///   different message, before the merge is ever attempted.
    #[test]
    fn fast_forward_merge_ff_false_refuses_the_fast_forward_and_stays_pre_merge() {
        let (_dir, repo) = fast_forward_merge_ff_false();
        assert!(!repo.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            git::out(&repo, &["config", "--local", "--get", "merge.ff"]),
            "false"
        );
        let main_before = git::out(&repo, &["rev-parse", "main"]);
        let feature_tip = git::out(&repo, &["rev-parse", "feature"]);
        let (_scratch, clone) = clone_onto_with_config(&repo, "main", &[("merge.ff", "false")]);
        assert!(git::try_run(&clone, &["merge", "--no-edit", "feature"]));
        let merged = git::out(&clone, &["rev-parse", "main"]);
        assert_ne!(merged, feature_tip, "must not have fast-forwarded");
        assert_ne!(
            merged, main_before,
            "must have moved from where main started"
        );
        assert_eq!(parent_count(&clone, "main"), 2);
    }

    /// Pins `divergent_merge_ff_only`: a real merge here is refused outright,
    /// and `main` does not move at all.
    ///
    /// Two mutations that must turn this red, in different ways (caught
    /// inside the builder):
    /// - **removes the mechanism**: change `"only"` to `"true"` (or drop the
    ///   `set_local_config` call). The clone's merge now succeeds, and the
    ///   builder's `assert!(!git::try_run(...))` panics with "must refuse a
    ///   merge that is not a fast-forward".
    /// - **weakens it**: drop `rival: add two.txt`, leaving `rival` one
    ///   commit from base. `merge.ff=only` still refuses the merge (the
    ///   topology is still divergent), but the builder's own
    ///   `rev-list --count rival == "3"` assertion panics first, with a
    ///   different message, before the merge is ever attempted.
    #[test]
    fn divergent_merge_ff_only_refuses_the_merge_and_stays_pre_merge() {
        let (_dir, repo) = divergent_merge_ff_only();
        assert!(!repo.join(".git/MERGE_HEAD").exists());
        assert_eq!(
            git::out(&repo, &["config", "--local", "--get", "merge.ff"]),
            "only"
        );
        let (_scratch, clone) = clone_onto_with_config(&repo, "main", &[("merge.ff", "only")]);
        let before = git::out(&clone, &["rev-parse", "main"]);
        assert!(!git::try_run(&clone, &["merge", "--no-edit", "rival"]));
        assert_eq!(git::out(&clone, &["rev-parse", "main"]), before);
        assert!(!clone.join(".git/MERGE_HEAD").exists());
    }

    /// Pins `cherry_pick_already_applied`: the merged tree already equals
    /// `main`'s own tree, and a real cherry-pick refuses with nothing to
    /// commit while still leaving `CHERRY_PICK_HEAD` behind.
    ///
    /// Three mutations were run, because the first one tried does not land
    /// where it looks like it should — recorded here so the discriminator's
    /// own line is not left unproven:
    /// - **removes the mechanism, and actually fires the discriminator**:
    ///   make `main`'s edit land on **line 2** instead of line 9 (`topic`
    ///   still edits line 9). The two edits are eight lines apart, so
    ///   `merge-tree` now exits `0` and writes a tree holding *both* edits —
    ///   which is no longer `main`'s own tree (that only has the line-2
    ///   edit). This is the mutation that actually panics on this builder's
    ///   `assert_eq!(tree, main_tree, ...)` line, with "the whole
    ///   discriminator". Measured, 2026-08-30.
    /// - **removes the mechanism a different way, but is caught earlier**:
    ///   make `main`'s edit differ from `topic`'s **on the same line** (e.g.
    ///   `"line 9 edited differently\n"`). Both sides now touch line 9
    ///   differently from a shared base, so `merge-tree` reports a
    ///   **conflict** (exit `1`) instead of writing a tree —
    ///   [`crate::git::out`]'s own generic assertion panics first, with
    ///   "git … failed", before this builder's tree-identity line is ever
    ///   reached. Kept here because it is still a real, caught mutation, and
    ///   because an earlier draft of this comment claimed it landed on the
    ///   tree-identity assertion — measured, and it does not; the line-2
    ///   variant above is the one that does.
    /// - **weakens it**: drop `topic: setup`, leaving `topic` one commit from
    ///   base. The tree-identity claim still holds, but the builder's own
    ///   `rev-list --count topic == "3"` assertion panics first, with a
    ///   different message, before the merge-tree call is ever made.
    #[test]
    fn cherry_pick_already_applied_computes_a_no_op_tree_and_stays_pre_pick() {
        let (_dir, repo) = cherry_pick_already_applied();
        assert!(!repo.join(".git/CHERRY_PICK_HEAD").exists());
        let pick = git::out(&repo, &["rev-parse", "topic"]);
        let pick_parent = git::out(&repo, &["rev-parse", "topic^"]);
        let main_tip = git::out(&repo, &["rev-parse", "main"]);
        let merge_base_flag = format!("--merge-base={pick_parent}");
        let raw = git::out(
            &repo,
            &[
                "merge-tree",
                "-z",
                "--write-tree",
                &merge_base_flag,
                &main_tip,
                &pick,
            ],
        );
        let tree = raw.split('\u{0}').next().unwrap_or_default().to_string();
        assert_eq!(tree, git::out(&repo, &["rev-parse", "main^{tree}"]));

        let (_scratch, clone) = clone_onto(&repo, "main");
        assert!(!git::try_run(&clone, &["cherry-pick", "--quiet", &pick]));
        assert!(clone.join(".git/CHERRY_PICK_HEAD").exists());
        assert!(git::out(&clone, &["status", "--porcelain"]).is_empty());
    }

    /// Pins `merge_conflict`: the fixture is pre-merge, and a real merge of
    /// `incoming` into `main` on a clone genuinely conflicts, leaving
    /// `MERGE_HEAD` and all three index stages — the `git merge` twin of the
    /// cherry-pick-conflict test above.
    ///
    /// Two mutations that must turn this red, in different ways (both caught
    /// inside the builder):
    /// - **removes the mechanism**: make `incoming` edit a line `main` never
    ///   touched (append a fourth line instead of rewriting line two). The
    ///   clone's merge now succeeds, and the builder's `assert!(!ok, ...)`
    ///   panics with "must conflict".
    /// - **weakens it**: drop `incoming: unrelated setup`, leaving `incoming`
    ///   one commit from base. The merge still conflicts, but the builder's
    ///   own `rev-list --count incoming == "3"` assertion panics first, with
    ///   a different message, before the merge is ever attempted.
    #[test]
    fn merge_conflict_conflicts_for_real_and_stays_pre_merge() {
        let (_dir, repo) = merge_conflict();
        assert!(!repo.join(".git/MERGE_HEAD").exists());

        let (_scratch, clone) = clone_onto(&repo, "main");
        let ok = git::try_run(&clone, &["merge", "--no-edit", "incoming"]);
        assert!(!ok, "expected the merge to fail with a conflict");
        assert!(clone.join(".git/MERGE_HEAD").exists());
        assert!(!clone.join(".git/CHERRY_PICK_HEAD").exists());
        assert_eq!(stages_of(&clone, "shared.txt"), vec![1, 2, 3]);
    }
}
