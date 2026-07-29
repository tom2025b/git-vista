//! The third impure corner of `sandbox`: resolving a linked worktree's real
//! git directory, so a policy can grant it.
//!
//! # Why this exists
//!
//! `policy_for_repo` grants read-write on the repository path it is handed and
//! nothing else outside the fixed system trees. For a plain repository that is
//! complete: `.git` is a directory inside the grant. For a **linked worktree**
//! (`git worktree add`) it is not — `<repo>/.git` is a one-line pointer file,
//! the worktree's own git state lives at `<main>/.git/worktrees/<id>`, and the
//! shared object/ref store lives at `<main>/.git` — both outside the worktree
//! path. Without the extra grant, every sandboxed git run in a linked worktree
//! dies with `fatal: not a git repository`, which Task 6's migration made
//! visible the moment `planner::run_git` started going through the launcher.
//!
//! # The threat model, and the containment rule that answers it
//!
//! The pointer file is **repository-writable**: the worktree is inside the
//! sandbox's RW grant, so a hostile hook can rewrite `.git` to say
//! `gitdir: /anywhere` before the next policy is built. Granting whatever the
//! pointer names would hand that hook an arbitrary read-write grant — the same
//! escalation the C10 audit refused for `.git/config`-derived trust. So a
//! resolution is honoured only when it proves itself with paths the attacker
//! would already have to control:
//!
//! 1. `<repo>/.git` must be a regular file (a symlink is refused) containing a
//!    `gitdir:` pointer; the target must exist and canonicalise.
//! 2. `<gitdir>/commondir` must exist, and its target must canonicalise.
//! 3. **The canonical gitdir must lie strictly inside
//!    `<canonical commondir>/worktrees/`.**
//!
//! Rule 3 is the security boundary. To make this function grant a victim
//! directory `V`, an attacker must produce a real, canonicalising directory
//! under `V/worktrees/` containing a `commondir` file that points back at `V`
//! — that is, they must already be able to write inside `V`. Symlinks buy
//! nothing: canonicalisation resolves them before the containment check, and a
//! target that does not exist fails to canonicalise and is refused. On top of
//! this, the shim's `--exclude` set still withholds secret paths from **every**
//! grant, so even a check bypass could not expose `~/.ssh` and friends.
//!
//! Every failure is an error, never a silent "no extra grant": a linked
//! worktree that resolves strangely gets a refused operation with a named
//! reason (fail-closed, same posture as INV-13), not a confusing downstream
//! git error. Geometries this deliberately does not support, today: submodule
//! gitdir pointers (no `commondir`) and `--separate-git-dir` repositories
//! (same) — both refuse rather than guess. D2 may later re-key this onto
//! validated catalog metadata; the containment rule must survive that move.

use std::path::{Path, PathBuf};

/// A pointer file (`.git` or `commondir`) is one short line; anything bigger
/// is not a geometry we recognise. Refusing early also keeps a hostile file
/// from making policy construction read something huge.
const POINTER_FILE_CAP: u64 = 4096;

/// The resolved directories of a linked worktree. `gitdir` is strictly inside
/// `commondir.join("worktrees")` — that containment is checked, not assumed —
/// so a policy needs to grant only `commondir` to cover both.
pub(crate) struct LinkedWorktreeDirs {
    /// `<main>/.git/worktrees/<id>` — the worktree's private index/HEAD/locks.
    pub gitdir: PathBuf,
    /// `<main>/.git` — the shared object and ref store.
    pub commondir: PathBuf,
}

/// Resolve `repo`'s linked-worktree directories, if it is a linked worktree.
///
/// - `Ok(None)` — not a linked worktree (`.git` is a directory, or absent):
///   the plain repository grant already covers everything.
/// - `Ok(Some(dirs))` — a linked worktree whose geometry passed the
///   containment rule above.
/// - `Err(why)` — a pointer geometry that could not be proven safe. The caller
///   must refuse to build a policy (fail-closed), never fall back to "no extra
///   grant" — that would convert a tamper attempt into a confusing git error
///   instead of a named refusal.
pub(crate) fn linked_worktree_dirs(repo: &Path) -> Result<Option<LinkedWorktreeDirs>, String> {
    let dotgit = repo.join(".git");
    let meta = match std::fs::symlink_metadata(&dotgit) {
        Ok(m) => m,
        // No `.git` at all: a bare repo path or not a repository. No extra
        // grant to compute; if it is not a repository, git itself will say so
        // from inside the sandbox.
        Err(_) => return Ok(None),
    };
    if meta.is_dir() {
        return Ok(None); // a plain repository
    }
    if meta.file_type().is_symlink() {
        return Err("`.git` is a symlink; a linked worktree's pointer must be a regular file".into());
    }
    if !meta.is_file() {
        return Err("`.git` is neither a directory nor a regular file".into());
    }

    let pointer = read_pointer_file(&dotgit, "`.git`")?;
    let Some(target) = pointer.strip_prefix("gitdir:") else {
        return Err("`.git` is a file but does not start with `gitdir:`".into());
    };
    let gitdir = canonical_join(repo, target.trim(), "gitdir target")?;

    // A linked worktree's gitdir always carries a `commondir` file. Its
    // absence means a geometry we do not grant (submodule or
    // --separate-git-dir pointer) — refuse rather than guess.
    let commondir_file = gitdir.join("commondir");
    let common_target = read_pointer_file(&commondir_file, "`commondir`")?;
    let commondir = canonical_join(&gitdir, common_target.trim(), "commondir target")?;

    // The containment rule (module doc, rule 3). `starts_with` on canonical
    // paths is component-wise, so `…/worktreesEvil` cannot pass as being
    // under `…/worktrees`.
    let base = commondir.join("worktrees");
    if !gitdir.starts_with(&base) || gitdir == base {
        return Err(format!(
            "gitdir {} is not strictly inside {} — the pointer does not describe \
             a linked worktree of that repository, so nothing is granted",
            gitdir.display(),
            base.display(),
        ));
    }

    Ok(Some(LinkedWorktreeDirs { gitdir, commondir }))
}

/// Read a small pointer file, refusing anything over [`POINTER_FILE_CAP`].
fn read_pointer_file(path: &Path, what: &str) -> Result<String, String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("{what} at {} is unreadable: {e}", path.display()))?;
    if meta.len() > POINTER_FILE_CAP {
        return Err(format!(
            "{what} at {} is {} bytes; a pointer file is one short line",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|e| format!("{what} at {} is not readable UTF-8: {e}", path.display()))
}

/// Join a possibly-relative pointer target onto `base` and canonicalise. A
/// target that does not exist fails here, which is what makes symlink games
/// and dangling pointers refusals instead of grants.
fn canonical_join(base: &Path, target: &str, what: &str) -> Result<PathBuf, String> {
    if target.is_empty() {
        return Err(format!("{what} is empty"));
    }
    let joined = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        base.join(target)
    };
    joined
        .canonicalize()
        .map_err(|e| format!("{what} {} does not canonicalise: {e}", joined.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a real linked-worktree layout with plain filesystem calls — no
    /// git spawn, so this file never enters the argv-boundary allowlists.
    /// Returns (tempdir, main repo, linked worktree).
    fn scaffold() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().expect("tempdir");
        let main = t.path().join("repo");
        let gitdir = main.join(".git/worktrees/linked");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let linked = t.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        (t, main, linked)
    }

    #[test]
    fn a_plain_repository_needs_no_extra_grant() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(matches!(linked_worktree_dirs(&repo), Ok(None)));
    }

    #[test]
    fn a_missing_dot_git_is_no_worktree_and_no_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(matches!(linked_worktree_dirs(t.path()), Ok(None)));
    }

    #[test]
    fn a_linked_worktree_resolves_to_its_gitdir_and_commondir() {
        let (_t, main, linked) = scaffold();
        let dirs = linked_worktree_dirs(&linked)
            .expect("a well-formed linked worktree resolves")
            .expect("and is recognised as linked");
        assert_eq!(
            dirs.gitdir,
            main.join(".git/worktrees/linked").canonicalize().unwrap()
        );
        assert_eq!(dirs.commondir, main.join(".git").canonicalize().unwrap());
        // The property the policy grant relies on: gitdir is inside commondir,
        // so granting commondir alone covers both.
        assert!(dirs.gitdir.starts_with(&dirs.commondir));
    }

    /// The attack this module exists to refuse: the pointer chain names a
    /// directory that is real and canonicalises, but is not a worktree
    /// registered under the commondir it claims. Nothing may be granted.
    #[test]
    fn a_gitdir_outside_the_commondirs_worktrees_is_refused() {
        let (t, _main, linked) = scaffold();
        // A hostile hook rewrites `.git` to point at a directory it controls,
        // whose `commondir` file names a victim directory elsewhere.
        let evil = t.path().join("evil");
        std::fs::create_dir_all(&evil).unwrap();
        let victim = t.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(evil.join("commondir"), victim.display().to_string()).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", evil.display()),
        )
        .unwrap();
        let err = linked_worktree_dirs(&linked).unwrap_err();
        assert!(
            err.contains("not strictly inside"),
            "the containment rule must be what refuses this, got: {err}"
        );
    }

    #[test]
    fn a_symlinked_dot_git_is_refused() {
        let (t, main, _linked) = scaffold();
        let sneaky = t.path().join("sneaky");
        std::fs::create_dir_all(&sneaky).unwrap();
        std::os::unix::fs::symlink(main.join(".git"), sneaky.join(".git")).unwrap();
        let err = linked_worktree_dirs(&sneaky).unwrap_err();
        assert!(err.contains("symlink"), "got: {err}");
    }

    /// Submodule and `--separate-git-dir` pointers have no `commondir`; they
    /// refuse rather than guess at a grant.
    #[test]
    fn a_pointer_without_a_commondir_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let standalone = t.path().join("standalone-gitdir");
        std::fs::create_dir_all(&standalone).unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".git"),
            format!("gitdir: {}\n", standalone.display()),
        )
        .unwrap();
        let err = linked_worktree_dirs(&repo).unwrap_err();
        assert!(err.contains("commondir"), "got: {err}");
    }

    #[test]
    fn a_dangling_pointer_is_refused_not_granted() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: /nonexistent/worktrees/x\n").unwrap();
        let err = linked_worktree_dirs(&repo).unwrap_err();
        assert!(err.contains("does not canonicalise"), "got: {err}");
    }
}
