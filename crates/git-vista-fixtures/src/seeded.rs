//! The baseline: an ordinary, entirely unbroken repository.
//!
//! ## What is "wrong" with it
//!
//! Nothing. That is the point, and it is why this is the most-used shape in
//! the catalogue. Most tests are not about a broken repository at all — they
//! are about what git-vista does with a *working* one, and they need a
//! repository that exists, has a branch, has a commit, and has a file, so that
//! "no conflicts", "one commit in the history", "a clean status" mean
//! something rather than being vacuously true of an empty directory.
//!
//! ## What git actually put on disk
//!
//! `git init -b main`, one file staged, one commit. Concretely: `.git/HEAD`
//! contains `ref: refs/heads/main`, `refs/heads/main` names a commit whose tree
//! holds exactly one blob, and there is no second commit, no second branch, no
//! remote, no stash, no reflog beyond the one entry the commit wrote.
//!
//! ## Why it matters
//!
//! Twenty separate copies of this fixture had grown across the server's test
//! suites before #448, and they had already drifted — one seeds `a.txt`, one
//! seeds `a.txt` *and* `b.txt`, one writes `seed\n` where the others write
//! `a\n`, one calls the commit `base` instead of `seed`. None of those
//! differences were deliberate, but at least one suite was quietly depending
//! on each. The parameterised builders below exist so that a suite needing a
//! different shape has to *name* the difference rather than fork the fixture.
//!
//! ## A note on the issue text
//!
//! Issue #448 describes this shape as "three commits, one file". No
//! implementation in the tree ever made three commits — all twenty made
//! exactly one. [`seeded`] reproduces what the code actually had, because this
//! is a consolidation: making it three would change what twenty suites see.

use crate::git;
use std::path::PathBuf;
use tempfile::TempDir;

/// A repository, and the temporary directory whose lifetime owns it.
///
/// The `TempDir` must be held for as long as the path is used — dropping it
/// deletes the repository. Callers conventionally bind it as `_dir`.
pub type Fixture = (TempDir, PathBuf);

/// The canonical seeded repository: one commit, one file, branch `main`.
///
/// `a.txt` contains `a\n`; the commit message is `seed`. This is the exact
/// shape sixteen of the twenty replaced `seeded_repo()` implementations built.
pub fn seeded() -> Fixture {
    seeded_files(&[("a.txt", "a\n")], "seed")
}

/// [`seeded`] with the files and commit message named by the caller.
///
/// This is the escape hatch for the suites whose seed genuinely differs — a
/// second file, different content, a different message — without either
/// bending the canonical shape or reintroducing a hand-rolled copy. Every
/// file is written, then `git add -A`, then one commit.
pub fn seeded_files(files: &[(&str, &str)], message: &str) -> Fixture {
    let (dir, repo) = empty();
    for (name, content) in files {
        git::write(&repo, name, content.as_bytes());
    }
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", message]);
    (dir, repo)
}

/// The date every `*_dated` shape pins its commits to.
///
/// Any fixed instant would do; this one is simply the value the contract suite
/// already used, kept so its recorded oids do not move.
pub const PINNED_DATE: &str = "2026-01-02T03:04:05Z";

/// [`seeded`] with the seed commit's author and committer dates pinned.
///
/// Commit oids hash their timestamps, so two repositories built from identical
/// instructions a second apart have different tip oids. Pinning the date makes
/// a pair of these true twins — same tree, same tip oid, same generation
/// inputs — which is what the staleness and contract suites compare against.
pub fn seeded_dated() -> Fixture {
    let (dir, repo) = empty();
    git::write(&repo, "a.txt", b"a\n");
    git::run(&repo, &["add", "-A"]);
    git::run_dated(&repo, &["commit", "-q", "-m", "seed"], PINNED_DATE);
    (dir, repo)
}

/// An initialised repository with no commit yet.
///
/// ## What is wrong with it
///
/// `HEAD` names `refs/heads/main`, but `refs/heads/main` does not exist. This
/// is git's "unborn branch" state, and it is the one every tool forgets: there
/// is a HEAD, `git symbolic-ref HEAD` answers happily, and yet `git rev-parse
/// HEAD` fails and there is no commit to diff against. Code that assumes "a
/// repository has a HEAD commit" fails here and nowhere else.
pub fn empty() -> Fixture {
    let dir = tempfile::tempdir().expect("create fixture tempdir");
    let repo = dir.path().join("repo");
    git::init(&repo);
    (dir, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape is asserted from git's own view of the repository, not by
    /// calling the builder's own helpers back — reading `git log` and
    /// `git ls-files` is how a suite would see it.
    #[test]
    fn seeded_is_one_commit_named_seed_holding_one_file() {
        let (_dir, repo) = seeded();

        let commits = git::out(&repo, &["rev-list", "--count", "HEAD"]);
        assert_eq!(commits, "1", "seeded() must be exactly one commit");

        let subject = git::out(&repo, &["log", "-1", "--format=%s"]);
        assert_eq!(subject, "seed");

        let tracked = git::out(&repo, &["ls-files"]);
        assert_eq!(tracked, "a.txt");

        let content = std::fs::read_to_string(repo.join("a.txt")).unwrap();
        assert_eq!(content, "a\n");
    }

    #[test]
    fn seeded_is_on_main_and_has_a_clean_worktree() {
        let (_dir, repo) = seeded();
        assert_eq!(
            git::out(&repo, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert_eq!(git::out(&repo, &["status", "--porcelain"]), "");
    }

    /// The identity is part of the contract: two suites assert on the literal
    /// author string, and it must not come from the developer's global config.
    #[test]
    fn the_seed_commit_is_authored_by_the_catalogue_identity() {
        let (_dir, repo) = seeded();
        let author = git::out(&repo, &["log", "-1", "--format=%an <%ae>"]);
        assert_eq!(author, "t <t@example.invalid>");
    }

    /// The local config is what lets a caller keep committing with a bare
    /// `git commit` of its own — the suites do exactly this, so a builder that
    /// only passed `-c` on its own invocations would break them.
    #[test]
    fn a_caller_can_commit_again_without_supplying_an_identity() {
        let (_dir, repo) = seeded();
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let ok = std::process::Command::new("git")
            .args(["commit", "-q", "-am", "follow-up"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success();
        assert!(ok, "a bare follow-up commit must find an identity");
    }

    #[test]
    fn seeded_files_writes_every_file_named() {
        let (_dir, repo) = seeded_files(&[("a.txt", "a\n"), ("b.txt", "b\n")], "seed");
        assert_eq!(git::out(&repo, &["ls-files"]), "a.txt\nb.txt");
        assert_eq!(git::out(&repo, &["log", "-1", "--format=%s"]), "seed");
    }

    /// The whole reason `seeded_dated` exists: two independent builds must be
    /// the same repository, down to the oid.
    #[test]
    fn two_dated_seeds_have_the_same_tip_oid() {
        let (_a, repo_a) = seeded_dated();
        let (_b, repo_b) = seeded_dated();
        assert_eq!(
            git::out(&repo_a, &["rev-parse", "HEAD"]),
            git::out(&repo_b, &["rev-parse", "HEAD"]),
        );
    }

    /// ...and the undated one must NOT be, or the pinning above would be
    /// proving nothing. Timestamps have one-second resolution, so the two
    /// builds are separated by a commit whose content differs instead.
    #[test]
    fn an_undated_seed_is_not_pinned_to_the_dated_one() {
        let (_a, repo_a) = seeded();
        let (_b, repo_b) = seeded_dated();
        assert_ne!(
            git::out(&repo_a, &["rev-parse", "HEAD"]),
            git::out(&repo_b, &["rev-parse", "HEAD"]),
        );
    }

    #[test]
    fn empty_has_a_head_that_names_a_branch_which_does_not_exist() {
        let (_dir, repo) = empty();
        assert_eq!(
            git::out(&repo, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(
            !git::try_run(&repo, &["rev-parse", "--verify", "HEAD"]),
            "an unborn branch must have no HEAD commit to resolve"
        );
    }
}
