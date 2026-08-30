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
}
