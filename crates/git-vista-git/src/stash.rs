//! Reading the stash drawer (M3.24, #77).
//!
//! # A stash list *is* a reflog, and that is not an implementation detail
//!
//! `git stash list` prints the reflog of a single ref, `refs/stash`. Entry
//! `stash@{0}` is that reflog's newest line, `stash@{1}` the one before it,
//! and so on. Everything awkward about stashes follows from this one fact:
//!
//! - **The position is an index into a log, not a name.** Dropping
//!   `stash@{0}` renumbers every entry below it — `stash@{1}` becomes
//!   `stash@{0}` — which is why the server compare-and-swaps a selector
//!   against its expected oid immediately before acting on it.
//! - **The oid is not an identity.** Two entries can reference the same
//!   commit at once (`git stash store` will happily do it), because a reflog
//!   line records a *movement to* a commit, not ownership of one. So "is this
//!   oid still in the list?" cannot answer "does the entry I meant still
//!   exist?".
//! - **The list can be empty by being absent.** A repository that has never
//!   stashed has no `refs/stash` at all — not an empty one. That is a real,
//!   readable observation of zero entries, and it must not be confused with
//!   failing to read the drawer.
//!
//! Read natively through `gix`, like every other read in this crate. There is
//! no subprocess here: one `gix::open_opts` maps the object database, and
//! each entry's commit is decoded straight out of it.
//!
//! # What this module does not decide
//!
//! It produces [`StashRecord`] — raw, git-shaped facts. It does not know the
//! wire DTO (`git-vista-protocol` is deliberately not a dependency of this
//! crate), so selector validation and message refitting live at the server's
//! mapping boundary. What this module owns is being honest about absence and
//! about failure, and keeping those two apart.

use std::path::Path;

use git_vista_core::model::Oid;

use crate::RepoError;

/// The most message bytes one stash entry contributes to a listing.
///
/// Matches `git_vista_protocol::plan::MAX_STASH_MESSAGE_LEN` (16 KiB) by
/// intent, not by import — this crate does not depend on the protocol crate.
/// The cap stops a repository with a hostile stash message from making a
/// listing response unbounded.
pub const MAX_STASH_MESSAGE_LEN: usize = 16 * 1024;

/// One entry in the stash drawer, as git records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashRecord {
    /// Position in the drawer: `0` is `stash@{0}`, the newest.
    ///
    /// This is the entry's *address*, and it is only valid for as long as the
    /// list does not change underneath it. Callers that act on an entry must
    /// pair it with [`Self::oid`] and re-resolve before mutating.
    pub index: usize,
    /// The stash commit. Identifies the recoverable *content* — never the
    /// entry, since two entries may carry the same oid.
    pub oid: Oid,
    /// The reflog line's message, exactly as git wrote it
    /// (`WIP on main: 1a2b3c4 subject`, or the user's `-m` text). Truncated at
    /// [`MAX_STASH_MESSAGE_LEN`].
    pub message: String,
    /// Unix seconds from the reflog entry's own signature.
    pub time: i64,
}

/// Read the stash drawer, newest entry first.
///
/// An empty `Vec` means the drawer was **read and is empty** — including the
/// common case of a repository that has never stashed, which has no
/// `refs/stash` at all. A failure to read returns `Err`; the two are never
/// merged, because "no stashes" and "couldn't look" authorise very different
/// things upstream and this is the milestone that stopped collapsing them.
pub fn read_stashes(path: &Path) -> Result<Vec<StashRecord>, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    // A missing `refs/stash` is the never-stashed repository: zero entries,
    // observed. Any OTHER find_reference error is a real failure and is
    // reported as one — the distinction this whole milestone is about.
    let reference = match repo.find_reference("refs/stash") {
        Ok(r) => r,
        Err(gix::reference::find::existing::Error::NotFound { .. }) => return Ok(Vec::new()),
        Err(e) => {
            return Err(RepoError::Walk(format!("opening refs/stash: {e}")));
        }
    };

    let mut platform = reference.log_iter();
    let iter = match platform.rev() {
        Ok(Some(iter)) => iter,
        // The ref exists but carries no reflog. Git cannot produce this for a
        // real stash (the reflog IS the stash list), so it means someone
        // pointed `refs/stash` at a commit by hand. Zero entries is the
        // honest reading: there is no drawer here, whatever the ref says.
        Ok(None) => return Ok(Vec::new()),
        Err(e) => {
            return Err(RepoError::Walk(format!("reading the stash reflog: {e}")));
        }
    };

    let mut records = Vec::new();
    for (index, line) in iter.enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // One malformed line must not hide the rest of the drawer —
                // but it MUST NOT silently renumber the entries below it
                // either, because the index is the address the user will act
                // on. Stop at the first unreadable line and return what is
                // above it: a shorter list is honest, a shifted list is not.
                eprintln!(
                    "git-vista: stopping the stash listing at an unreadable entry \
                     (stash@{{{index}}}): {e}"
                );
                break;
            }
        };
        let mut message = line.message.to_string();
        if message.len() > MAX_STASH_MESSAGE_LEN {
            message.truncate(MAX_STASH_MESSAGE_LEN);
        }
        records.push(StashRecord {
            index,
            oid: Oid(line.new_oid.to_string()),
            message,
            time: line.signature.time.seconds,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A repo with one commit on `main`, ready to stash into.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    /// MUTATION: return `Err` for a missing `refs/stash` and this goes red.
    /// A repository that has never stashed is a readable observation of zero
    /// entries, not a failure.
    #[test]
    fn a_repo_that_never_stashed_reads_as_empty_not_as_an_error() {
        let dir = repo();
        let stashes = read_stashes(dir.path()).expect("a never-stashed repo is readable");
        assert!(stashes.is_empty());
    }

    /// MUTATION: drop the `.enumerate()` index or reverse the iteration and
    /// this goes red — stash@{0} must be the NEWEST entry.
    #[test]
    fn entries_are_newest_first_and_indexed_from_zero() {
        let dir = repo();
        std::fs::write(dir.path().join("f.txt"), "first change\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "older"]);
        std::fs::write(dir.path().join("f.txt"), "second change\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "newer"]);

        let stashes = read_stashes(dir.path()).unwrap();
        assert_eq!(stashes.len(), 2);
        assert_eq!(stashes[0].index, 0);
        assert_eq!(stashes[1].index, 1);
        assert!(
            stashes[0].message.contains("newer"),
            "stash@{{0}} must be the newest entry, got {:?}",
            stashes[0].message
        );
        assert!(stashes[1].message.contains("older"));
    }

    /// The fact that forced the protocol's selector/oid split, pinned here so
    /// nobody re-derives "the oid identifies the entry" from a comfortable
    /// assumption.
    ///
    /// MUTATION: none — this is a property of git, and the test exists to
    /// prove the property still holds on whatever git is installed.
    #[test]
    fn two_entries_can_carry_the_same_oid() {
        let dir = repo();
        std::fs::write(dir.path().join("f.txt"), "change\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "original"]);

        let first = read_stashes(dir.path()).unwrap();
        assert_eq!(first.len(), 1);
        let oid = first[0].oid.0.clone();

        // Re-store the SAME commit as a second entry, then push a third so the
        // duplicate is not merely the top entry twice.
        std::fs::write(dir.path().join("f.txt"), "other\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "unrelated"]);
        git(dir.path(), &["stash", "store", "-m", "restored", &oid]);

        let all = read_stashes(dir.path()).unwrap();
        let same: Vec<usize> = all
            .iter()
            .filter(|s| s.oid.0 == oid)
            .map(|s| s.index)
            .collect();
        assert_eq!(
            same.len(),
            2,
            "one commit must be able to occupy two stash slots — this is why a \
             selector, not an oid, addresses an entry"
        );
    }

    /// MUTATION: make the index survive a drop (e.g. cache it) and this goes
    /// red. Positions renumber, which is why the server re-resolves before
    /// every mutation instead of trusting a position it was handed.
    #[test]
    fn dropping_an_entry_renumbers_the_ones_below_it() {
        let dir = repo();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        git(dir.path(), &["stash", "push", "-q", "-m", "second"]);

        let before = read_stashes(dir.path()).unwrap();
        let first_oid = before[1].oid.0.clone(); // "first" sits at index 1

        git(dir.path(), &["stash", "drop", "-q", "stash@{0}"]);

        let after = read_stashes(dir.path()).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].index, 0, "the survivor moved from 1 to 0");
        assert_eq!(
            after[0].oid.0, first_oid,
            "and it is the same entry, at a different address"
        );
    }

    /// MUTATION: return `Ok(Vec::new())` for an unopenable repository and this
    /// goes red. "Couldn't look" must never read as "the drawer is empty".
    #[test]
    fn an_unreadable_repository_errors_rather_than_reporting_an_empty_drawer() {
        let dir = tempfile::tempdir().unwrap(); // no .git at all
        assert!(
            read_stashes(dir.path()).is_err(),
            "a path that is not a repository must be an error, not an empty list"
        );
    }
}
