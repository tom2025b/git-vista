//! D2 (#66, Task 7): validated repository-metadata resolution — given a
//! repository path, resolve the git directory(ies) it actually uses and
//! REFUSE when that resolution lands outside the server's managed root.
//!
//! # Why this exists, and how it differs from `sandbox::worktree`
//!
//! `sandbox::worktree::linked_worktree_dirs` already resolves a **linked
//! worktree's** gitdir with a containment rule: the resolved gitdir must lie
//! strictly inside `<commondir>/worktrees/`. That rule proves the pointer
//! chain is *internally consistent* — the gitdir really is a registered
//! worktree of the commondir it claims — but it says nothing about where that
//! commondir itself sits on the host. A `.git` gitfile is repository-writable
//! (inside the sandbox's own RW grant), so a hostile hook can rewrite it to
//! point at a *self-consistent* linked-worktree geometry — a real
//! `commondir/worktrees/<id>/commondir` chain — built entirely outside any
//! directory the operator ever intended this server to touch, as long as the
//! attacker can write that chain somewhere. `worktree.rs`'s rule alone does
//! not refuse that: it only asks "is this a linked worktree of *some*
//! commondir", never "is that commondir somewhere this server manages".
//!
//! This module adds the second, missing check, and composes rather than
//! duplicates the first: [`resolve`] calls
//! [`worktree::linked_worktree_dirs`] for the gitfile-pointer case (a plain
//! `.git` *directory* is resolved directly, since there is no pointer to
//! chase), so the internal-consistency rule and the managed-root rule are
//! two independent gates a resolution must pass, not two implementations of
//! the same gate. A repository can fail either one on its own — an ordinary
//! repo cloned somewhere outside the managed root fails the second gate with
//! no linked-worktree geometry in sight at all — and a hostile pointer must
//! satisfy both simultaneously to be granted anything.
//!
//! # Fail-closed posture
//!
//! Every failure is a named [`RepoPathsError`], never a silent "resolves to
//! nothing" — the same posture `worktree.rs` documents and for the same
//! reason: "I could not prove this safe" must never collapse into "there is
//! nothing here", or a tamper attempt becomes a confusing downstream git
//! error instead of a refusal with a stated cause.

use std::path::{Path, PathBuf};

use super::worktree;

/// The resolved git directory(ies) `repo` actually uses, proven to lie
/// inside the managed root the caller checked against.
///
/// For a plain repository (`.git` is a directory) `gitdir == commondir`. For
/// a linked worktree they differ, mirroring
/// [`worktree::LinkedWorktreeDirs`] exactly — see that type's docs for why a
/// grant on `commondir` alone covers both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoPaths {
    pub(crate) gitdir: PathBuf,
    pub(crate) commondir: PathBuf,
}

/// Why a repository's git-directory resolution could not be produced or
/// validated. Every variant is a refusal, never a "nothing to grant".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RepoPathsError {
    /// `<repo>/.git` does not exist at all — not a repository this server can
    /// serve, mutation or read.
    #[error("{} has no `.git`", repo.display())]
    MissingGitFile { repo: PathBuf },
    /// `<repo>/.git` exists but could not even be `stat`ed (permissions, a
    /// racing unlink, …). Distinct from "missing" so an operator sees *why*.
    #[error("`{}/.git` is unreadable: {why}", repo.display())]
    UnreadableGitFile { repo: PathBuf, why: String },
    /// `<repo>/.git` exists but is neither a directory nor something
    /// `worktree::linked_worktree_dirs` could classify as a gitfile pointer
    /// (i.e. it stat'd as `Ok`, `is_dir() == false`, and the worktree module
    /// itself never got to render its own opinion — kept distinct from
    /// [`Self::WorktreeGeometry`] for the one geometry shape neither module
    /// owns: `.git` is a file but empty, or otherwise pre-empted before the
    /// pointer parse). In practice most malformed-pointer cases surface as
    /// `WorktreeGeometry` instead, since `resolve` delegates the file case to
    /// `worktree::linked_worktree_dirs` first.
    #[error("`{}/.git` is malformed: {why}", repo.display())]
    MalformedGitFile { repo: PathBuf, why: String },
    /// `<repo>/.git` is a gitfile whose pointer chain
    /// `worktree::linked_worktree_dirs` could not prove safe (dangling
    /// target, symlinked `.git`, gitdir not contained in `commondir`'s
    /// registered worktrees, no `commondir` file at all, …). Carries that
    /// module's own reason.
    #[error("cannot resolve the git directory of {}: {why}", repo.display())]
    WorktreeGeometry { repo: PathBuf, why: String },
    /// The resolution is internally consistent — `worktree.rs`'s containment
    /// rule (or the trivial plain-repo case) is satisfied — but it lands
    /// outside the server's managed root. Refused regardless of how
    /// convincing the pointer chain is: a repository this server was never
    /// configured to serve gets no sandbox grant, no matter what its own
    /// `.git` claims about itself.
    #[error(
        "the git directory of {} resolves to {} (commondir {}), which lies outside the \
         server's managed root — refusing rather than granting sandbox access to an \
         unmanaged location",
        repo.display(), gitdir.display(), commondir.display()
    )]
    OutsideManagedRoot {
        repo: PathBuf,
        gitdir: PathBuf,
        commondir: PathBuf,
    },
}

/// Resolve `repo`'s actual git directory(ies), with no containment check
/// against any root — the pure geometry half of this module, used directly
/// by tests and by [`resolve_and_validate`].
///
/// Delegates the gitfile-pointer case to [`worktree::linked_worktree_dirs`]
/// rather than re-parsing `gitdir:` pointers here — seeing the module doc
/// comment above for why the two checks compose instead of duplicating.
pub(crate) fn resolve(repo: &Path) -> Result<RepoPaths, RepoPathsError> {
    match worktree::linked_worktree_dirs(repo) {
        Ok(Some(dirs)) => {
            return Ok(RepoPaths {
                gitdir: dirs.gitdir,
                commondir: dirs.commondir,
            })
        }
        // `Ok(None)` covers two different real states — `.git` is a plain
        // directory, or `.git` is entirely absent — which `worktree.rs`
        // deliberately treats alike (neither needs an extra worktree grant).
        // This module *does* need to tell them apart, so it falls through to
        // its own stat below rather than trusting a collapsed `None`.
        Ok(None) => {}
        Err(why) => {
            return Err(RepoPathsError::WorktreeGeometry {
                repo: repo.to_path_buf(),
                why,
            })
        }
    }

    let dotgit = repo.join(".git");
    match std::fs::symlink_metadata(&dotgit) {
        Ok(meta) if meta.is_dir() => {
            let canonical = dotgit.canonicalize().map_err(|e| RepoPathsError::UnreadableGitFile {
                repo: repo.to_path_buf(),
                why: e.to_string(),
            })?;
            Ok(RepoPaths {
                gitdir: canonical.clone(),
                commondir: canonical,
            })
        }
        // `worktree::linked_worktree_dirs` already returned `Ok(None)` above,
        // which only happens for a directory or a missing path — so a
        // non-directory `Ok` here would mean the two stats disagreed (a
        // concurrent tamper between the two calls). Fail closed rather than
        // guess which stat was right.
        Ok(_) => Err(RepoPathsError::MalformedGitFile {
            repo: repo.to_path_buf(),
            why: "`.git` changed shape between two resolution passes".to_string(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(RepoPathsError::MissingGitFile {
                repo: repo.to_path_buf(),
            })
        }
        Err(e) => Err(RepoPathsError::UnreadableGitFile {
            repo: repo.to_path_buf(),
            why: e.to_string(),
        }),
    }
}

/// Resolve `repo`'s git directory(ies) and refuse unless both `gitdir` and
/// `commondir` lie within `managed_root` (the root itself, or a descendant of
/// it — canonical, component-wise containment, the same shape
/// `catalog::AllowedRoots::contains` uses).
///
/// Single-root by design: this is the pure, directly testable primitive
/// ([`hostile`](super::hostile) drives it against throwaway roots). Production
/// policy construction (`sandbox::policy_for`) is multi-root aware — the
/// catalog can hold more than one allowed root (the configured repo root, the
/// clones root, ad-hoc roots a trusted launch allowed) — so it calls
/// [`resolve`] directly and checks containment against the catalog's full set
/// via `state::path_is_allowed` instead of calling this single-root wrapper.
/// Kept as its own function anyway rather than folded away, because a single
/// fixed root is the right shape for a hostile-geometry test and for any
/// future caller that only ever has one root to check against.
pub(crate) fn resolve_and_validate(
    repo: &Path,
    managed_root: &Path,
) -> Result<RepoPaths, RepoPathsError> {
    let paths = resolve(repo)?;
    let managed_root = managed_root
        .canonicalize()
        .unwrap_or_else(|_| managed_root.to_path_buf());
    let contained = |p: &Path| *p == managed_root || p.starts_with(&managed_root);
    if !contained(&paths.gitdir) || !contained(&paths.commondir) {
        return Err(RepoPathsError::OutsideManagedRoot {
            repo: repo.to_path_buf(),
            gitdir: paths.gitdir,
            commondir: paths.commondir,
        });
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init should run");
        assert!(status.success());
    }

    #[test]
    fn a_plain_repo_inside_the_managed_root_resolves() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        init_repo(&repo);
        let paths = resolve_and_validate(&repo, root.path()).expect("inside the root");
        assert_eq!(paths.gitdir, paths.commondir);
        assert_eq!(paths.gitdir, repo.join(".git").canonicalize().unwrap());
    }

    #[test]
    fn a_plain_repo_outside_the_managed_root_is_refused() {
        let managed = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let repo = elsewhere.path().join("repo");
        init_repo(&repo);
        let err = resolve_and_validate(&repo, managed.path()).unwrap_err();
        assert!(matches!(err, RepoPathsError::OutsideManagedRoot { .. }));
    }

    #[test]
    fn a_missing_dot_git_is_refused_not_silently_empty() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("not-a-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let err = resolve_and_validate(&repo, root.path()).unwrap_err();
        assert!(matches!(err, RepoPathsError::MissingGitFile { .. }));
    }

    #[test]
    fn resolve_alone_does_no_containment_check() {
        let elsewhere = tempfile::tempdir().unwrap();
        let repo = elsewhere.path().join("repo");
        init_repo(&repo);
        // No managed root involved at all — `resolve` just describes the
        // geometry; only `resolve_and_validate` (or a caller composing its
        // own containment check, as `policy_for` does) refuses on location.
        assert!(resolve(&repo).is_ok());
    }
}
