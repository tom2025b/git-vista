//! Repositories that are broken *structurally* — not by a conflict, but by the
//! `.git` directory itself being in a state no ordinary git command produces.
//!
//! These are the shapes you cannot reach by using git normally, which is
//! exactly why they are worth having: they are what a half-finished operation,
//! a killed process, a full disk, or a hostile checkout leaves behind, and they
//! are the states real users arrive in and cannot get out of.

use crate::git;
use crate::seeded::{empty, Fixture};

/// A directory git genuinely cannot be run against.
///
/// ## What is wrong
///
/// `.git` is a **regular file** whose contents are not a `gitdir:` pointer.
///
/// ## What git put on disk
///
/// Nothing — git never made this. A `.git` file is legal (that is how linked
/// worktrees and submodules work: the file reads `gitdir: /path/to/real/git`),
/// but this one holds arbitrary text, so the geometry cannot be classified at
/// all. git-vista's worktree resolution refuses to guess, which means no
/// sandbox policy can be chosen, which means **no git process is ever
/// spawned**.
///
/// ## Why it matters
///
/// It is the honest test for "what happens when git cannot run?". The
/// tempting alternative — putting a fake `git` on `PATH` — mutates
/// process-wide state and races every other test in the binary. This shape
/// needs no mocking and no stubbing: it exercises a real production failure
/// path (a corrupt or hostile `.git`) the way production reaches it.
pub fn unrunnable() -> Fixture {
    let dir = tempfile::tempdir().expect("create fixture tempdir");
    let repo = dir.path().join("hostile");
    std::fs::create_dir_all(&repo).expect("create fixture dir");
    std::fs::write(repo.join(".git"), "this is not a gitdir: pointer\n").expect("write .git");
    (dir, repo)
}

/// A repository whose `HEAD` resolves to nothing.
///
/// ## What is wrong
///
/// `.git/HEAD` holds a **well-formed object id with no object behind it** —
/// forty zeroes. Not a symbolic ref, not a typo: a syntactically perfect
/// pointer into an empty space.
///
/// ## What git put on disk
///
/// Again, git did not do this; the builder writes the file by hand, because no
/// ordinary command produces this state. It is what a truncated write, an
/// interrupted `git checkout`, or a botched recovery leaves behind. The rest of
/// the repository is *fine*: `refs/heads/main` still names a real commit and
/// the object database is intact, so the readable half of the repository
/// survives.
///
/// ## Why it matters
///
/// This is the failure that looks like a bug in your tool rather than a broken
/// repository, and it has a trap in it worth knowing about.
///
/// `git status`, `git log`, `git symbolic-ref HEAD` and `git cat-file -t HEAD`
/// all fail here with `fatal: bad object HEAD`. But **`git rev-parse --verify
/// HEAD` succeeds**, printing the forty zeroes straight back: `--verify` checks
/// that the argument names exactly one revision in well-formed syntax, not that
/// an object exists behind it. Only `HEAD^{commit}`, which forces a peel to a
/// real commit object, actually fails.
///
/// So the cheapest liveness probe a tool can write — "does `rev-parse HEAD`
/// work?" — reports this repository as healthy, and every command after it
/// fails anyway. The repository is *not* empty and *not* unborn, so the two
/// cases most code does handle both fall through to an unexplained error. The
/// user sees a blank screen or a stack trace, with no hint that the cause is
/// one corrupt forty-byte file they could fix in a second if anyone told them.
///
/// It had been hand-built twice in two days before it earned a name here.
pub fn broken_head() -> Fixture {
    let (dir, repo) = empty();

    git::write(&repo, "a.txt", b"a\n");
    git::run(&repo, &["add", "-A"]);
    git::run(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "seed: one real commit, so the branch still reads",
        ],
    );

    std::fs::write(repo.join(".git/HEAD"), format!("{}\n", "0".repeat(40)))
        .expect("overwrite .git/HEAD");

    (dir, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture must actually be unrunnable, or every test built on it is
    /// asserting against a repository that quietly works.
    #[test]
    fn unrunnable_really_cannot_run_git() {
        let (_d, repo) = unrunnable();
        assert!(
            !git::try_run(&repo, &["status", "--porcelain"]),
            "git must fail against the hostile .git file"
        );
    }

    /// The `.git` entry is a file, not a directory — that is the whole shape.
    #[test]
    fn unrunnable_has_a_dot_git_that_is_a_regular_file() {
        let (_d, repo) = unrunnable();
        let meta = std::fs::metadata(repo.join(".git")).unwrap();
        assert!(meta.is_file());
    }

    /// The claim that makes `broken_head` distinct from an empty repository:
    /// HEAD does not name a usable commit, yet the branch and its commit are
    /// still perfectly readable.
    #[test]
    fn broken_head_cannot_peel_head_but_the_branch_still_reads() {
        let (_d, repo) = broken_head();

        assert!(
            !git::try_run(&repo, &["rev-parse", "--verify", "HEAD^{commit}"]),
            "HEAD must not peel to a real commit"
        );
        let branch = git::out(&repo, &["rev-parse", "--verify", "refs/heads/main"]);
        assert_eq!(branch.len(), 40, "main must still name a real commit");
    }

    /// The trap the documentation warns about, pinned here so it cannot rot:
    /// the obvious health probe passes on a repository where nothing else
    /// works. If a future git changes this, the doc above must change with it.
    #[test]
    fn the_cheap_liveness_probe_wrongly_reports_this_repository_as_healthy() {
        let (_d, repo) = broken_head();

        assert!(
            git::try_run(&repo, &["rev-parse", "--verify", "HEAD"]),
            "rev-parse --verify HEAD checks syntax, not existence — it passes"
        );
        for argv in [
            ["status", "--porcelain"],
            ["log", "--oneline"],
            ["symbolic-ref", "HEAD"],
        ] {
            assert!(
                !git::try_run(&repo, &argv),
                "git {argv:?} must fail on a HEAD with no object behind it"
            );
        }
    }

    /// It must not be mistakable for the unborn-branch case: there, HEAD is a
    /// symbolic ref to a missing branch; here, HEAD is a raw oid.
    #[test]
    fn broken_head_is_not_the_unborn_branch_state() {
        let (_d, repo) = broken_head();
        let head = std::fs::read_to_string(repo.join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), "0".repeat(40));
        assert!(!head.starts_with("ref:"));
    }
}
