//! Deriving stable identity and generations from a real repository via `gix`.
//!
//! The pure value types live in [`git_vista_core::identity`]; this module is the
//! native half that reads a filesystem repository and *produces* them. It is the
//! only place that knows the mapping from a path to an id — everything above the
//! backend deals in opaque [`RepositoryId`]/[`WorktreeId`] and never sees a path.
//!
//! - [`read_handle`] resolves a path to the [`RepositoryHandle`] (repository +
//!   worktree ids) the API should address it by.
//! - [`read_generation_inputs`] reads the observable state `gix` can see (HEAD,
//!   refs, and the index) into a [`GenerationInputs`] the caller finishes and
//!   folds into a [`RepositoryGeneration`]. The working-tree (unstaged) digest is
//!   layered on by the caller from its status read — see the note on that
//!   function — which is why it returns the builder rather than the finished
//!   generation.
//! - [`read_generation`] is the convenience that folds HEAD + refs + index alone.

use std::path::Path;

use gix::refs::Category;

use git_vista_core::identity::{
    GenerationInputs, ObjectId, RepositoryGeneration, RepositoryHandle, RepositoryId, WorktreeId,
};

use crate::RepoError;

/// Open a repository in isolated mode (no ambient config/env), matching how the
/// rest of this crate reads repositories.
fn open(path: &Path) -> Result<gix::Repository, RepoError> {
    gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// Canonicalise a git directory to an absolute, symlink-resolved string so the
/// same repository yields the same id regardless of how it was addressed. Falls
/// back to a lossy display of the original path if canonicalisation fails (a
/// path `gix` just opened should canonicalise, but we never want id derivation
/// to hard-fail on a filesystem quirk).
fn canonical_dir(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Resolve `path` to the opaque [`RepositoryHandle`] the API addresses it by.
///
/// The [`RepositoryId`] is derived from the repository's *common directory* —
/// shared by every worktree of the clone — and the [`WorktreeId`] from this
/// worktree's own *git directory*. For an ordinary single-worktree repository
/// the two directories coincide, but the ids still differ because they live in
/// separate namespaces (see [`git_vista_core::identity`]); for a linked worktree
/// the git dir is `…/worktrees/<name>` while the common dir is the main `.git`,
/// so linked worktrees share the repository id and carry distinct worktree ids.
pub fn read_handle(path: &Path) -> Result<RepositoryHandle, RepoError> {
    let repo = open(path)?;
    let repository = RepositoryId::from_common_dir(&canonical_dir(repo.common_dir()));
    let worktree = WorktreeId::from_git_dir(&canonical_dir(repo.git_dir()));
    Ok(RepositoryHandle::new(repository, worktree))
}

/// Read the observable state `gix` can see — HEAD, every ref, and the index —
/// into a [`GenerationInputs`] the caller finishes and folds into a generation.
///
/// This deliberately does **not** populate the working-tree (unstaged) slot:
/// reading the full working-tree status is the job of the status subsystem
/// (porcelain v2), and the request path already has that read in hand. The
/// intended use is:
///
/// ```no_run
/// # use std::path::Path;
/// # fn go(path: &Path, worktree_digest: &str) -> Result<(), git_vista_git::RepoError> {
/// let mut inputs = git_vista_git::read_generation_inputs(path)?;
/// inputs.worktree(worktree_digest); // digest of the porcelain-v2 status read
/// let generation = inputs.generation();
/// # let _ = generation;
/// # Ok(())
/// # }
/// ```
///
/// So the generation reflects HEAD, refs, and the index here, and the caller
/// layers the unstaged working tree on top. Staged changes are already visible
/// through the index digest, so `git add` advances the generation without the
/// worktree slot.
pub fn read_generation_inputs(path: &Path) -> Result<GenerationInputs, RepoError> {
    let repo = open(path)?;
    let mut inputs = GenerationInputs::new();

    // HEAD: its symbolic target (which branch, or None when detached) and the
    // commit it resolves to (None for an unborn HEAD on a fresh repo).
    let symbolic = repo
        .head_name()
        .map_err(|e| RepoError::Walk(format!("reading HEAD name: {e}")))?
        .map(|name| name.as_bstr().to_string());
    let resolved = repo.head_id().ok().map(|id| id.detach().to_string());
    let resolved_oid = resolved.as_deref().map(to_object_id).transpose()?;
    inputs.head(symbolic.as_deref(), resolved_oid.as_ref());

    // Every ref, peeled to the commit it resolves to, keyed by full name so a
    // rename or retarget changes the generation. Order is irrelevant: the
    // builder canonicalises by sorting.
    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    for reference in all {
        let mut reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while reading identity: {e}");
                continue;
            }
        };
        // Only branches, tags, and remote-tracking refs contribute; the HEAD
        // pseudo-ref is already recorded above, and notes/worktree-private refs
        // aren't observable repository state for staleness purposes.
        let full_name = match reference.name().category_and_short_name() {
            Some((Category::LocalBranch | Category::RemoteBranch | Category::Tag, _)) => {
                reference.name().as_bstr().to_string()
            }
            _ => continue,
        };
        match reference.peel_to_id() {
            Ok(id) => {
                let oid = to_object_id(&id.detach().to_string())?;
                inputs.reference(&full_name, &oid);
            }
            Err(e) => {
                eprintln!("git-vista: ref {full_name:?} won't resolve ({e}); not in generation")
            }
        }
    }

    // The index checksum is a compact digest of the whole staging area: any
    // stage/unstage rewrites the index and changes it. A repository with no
    // index yet (freshly `git init`ed, nothing staged) contributes nothing.
    if let Ok(index) = repo.open_index() {
        if let Some(checksum) = index.checksum() {
            inputs.index(&checksum.to_string());
        }
    }

    Ok(inputs)
}

/// Convenience over [`read_generation_inputs`]: the generation of HEAD + refs +
/// index alone (no unstaged working-tree component). Use this where only the
/// committed/staged state matters, or in tests.
pub fn read_generation(path: &Path) -> Result<RepositoryGeneration, RepoError> {
    Ok(read_generation_inputs(path)?.generation())
}

/// Validate a hex string `gix` produced into a core [`ObjectId`]. `gix` only
/// emits well-formed hashes, so a failure here means an internal contract broke
/// rather than bad user input — surface it as a walk error.
fn to_object_id(hex: &str) -> Result<ObjectId, RepoError> {
    ObjectId::parse(hex)
        .map_err(|e| RepoError::Walk(format!("git produced an invalid object id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{commit, fixture, git};

    #[test]
    fn handle_is_stable_across_reads() {
        let dir = fixture();
        let a = read_handle(dir.path()).unwrap();
        let b = read_handle(dir.path()).unwrap();
        assert_eq!(a, b, "the same repository must yield the same handle");
    }

    #[test]
    fn different_repos_get_different_ids() {
        let one = fixture();
        let two = fixture();
        let a = read_handle(one.path()).unwrap();
        let b = read_handle(two.path()).unwrap();
        assert_ne!(a.repository, b.repository);
        assert_ne!(a.worktree, b.worktree);
    }

    #[test]
    fn linked_worktree_shares_repo_id_but_has_its_own_worktree_id() {
        let dir = fixture();
        let main = dir.path();
        // Create a linked worktree checked out on the `feature` branch.
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("linked");
        git(
            main,
            &["worktree", "add", wt_path.to_str().unwrap(), "feature"],
        );

        let main_handle = read_handle(main).unwrap();
        let linked_handle = read_handle(&wt_path).unwrap();

        assert_eq!(
            main_handle.repository, linked_handle.repository,
            "a linked worktree shares the shared-repository id"
        );
        assert_ne!(
            main_handle.worktree, linked_handle.worktree,
            "a linked worktree has its own worktree id"
        );
    }

    #[test]
    fn generation_is_stable_when_nothing_changes() {
        let dir = fixture();
        let a = read_generation(dir.path()).unwrap();
        let b = read_generation(dir.path()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn generation_advances_when_head_moves() {
        let dir = fixture();
        let before = read_generation(dir.path()).unwrap();
        // Move HEAD back one commit (detaches, but HEAD's resolved oid changes).
        git(dir.path(), &["checkout", "-q", "HEAD~1"]);
        let after = read_generation(dir.path()).unwrap();
        assert_ne!(before, after, "moving HEAD must advance the generation");
    }

    #[test]
    fn generation_advances_when_a_branch_is_created() {
        let dir = fixture();
        let before = read_generation(dir.path()).unwrap();
        git(dir.path(), &["branch", "new-branch"]);
        let after = read_generation(dir.path()).unwrap();
        assert_ne!(before, after, "a new ref must advance the generation");
    }

    #[test]
    fn generation_advances_when_a_commit_is_added() {
        let dir = fixture();
        let before = read_generation(dir.path()).unwrap();
        commit(dir.path(), "F another", 7);
        let after = read_generation(dir.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn generation_advances_when_the_index_changes() {
        let dir = fixture();
        let before = read_generation(dir.path()).unwrap();
        // Stage a new file: the index checksum changes even though HEAD/refs
        // don't, so the generation must advance.
        std::fs::write(dir.path().join("staged.txt"), "hello").unwrap();
        git(dir.path(), &["add", "staged.txt"]);
        let after = read_generation(dir.path()).unwrap();
        assert_ne!(before, after, "staging must advance the generation");
    }
}
