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

/// How a repository relates to git's on-disk layout, classified from the
/// directories `gix` resolves. The server catalog (M1.03) needs this to handle
/// bare repositories and linked worktrees explicitly rather than assuming one
/// working tree per clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// A bare repository — a git directory with no working tree.
    Bare,
    /// The main working tree (its git dir *is* the common dir).
    Main,
    /// A linked worktree (`git worktree add`) whose git dir lives under
    /// `…/worktrees/<name>` while its common dir is the main `.git`.
    Linked,
}

/// Everything the server catalog needs to admit a repository by an opaque id,
/// resolved from a real path via `gix` — the one place that maps a path to
/// identity. Nothing above the backend ever sees the [`root`](Self::root) path.
#[derive(Debug, Clone)]
pub struct RepoFacts {
    /// The opaque repository + worktree ids this path is addressed by.
    pub handle: RepositoryHandle,
    /// Bare / main / linked classification.
    pub kind: WorktreeKind,
    /// The canonical (symlink-resolved) directory used for the allowed-root
    /// check: the working tree for a normal or linked worktree, the git dir for
    /// a bare repository. Canonical so a symlink escaping an allowed root fails
    /// the containment check closed.
    pub root: std::path::PathBuf,
    /// A short, non-path display label: the base name of [`root`](Self::root).
    pub name: String,
}

/// Canonicalise a directory to an absolute, symlink-resolved [`PathBuf`], or
/// return the path unchanged if canonicalisation fails (missing, permissions).
/// The catalog treats a non-canonicalisable path as outside every allowed root,
/// so a failure here fails closed rather than admitting an unresolved path.
fn canonical_pathbuf(dir: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Resolve `path` to the [`RepoFacts`] the server catalog admits it by: its
/// opaque handle, its bare/main/linked classification, and the canonical root
/// directory the allowed-root check is performed against.
///
/// A bare repository is recognised by having no working tree; a linked worktree
/// by its git dir differing from its common dir (see [`WorktreeKind`]). The
/// [`root`](RepoFacts::root) is canonicalised so that a symlink pointing outside
/// an allowed root resolves — and is then rejected — rather than slipping through.
pub fn read_repo_facts(path: &Path) -> Result<RepoFacts, RepoError> {
    let repo = open(path)?;
    let handle = RepositoryHandle::new(
        RepositoryId::from_common_dir(&canonical_dir(repo.common_dir())),
        WorktreeId::from_git_dir(&canonical_dir(repo.git_dir())),
    );

    // Bare iff there is no working tree. Otherwise it's linked when this
    // worktree's git dir differs from the shared common dir.
    let (kind, root) = match repo.workdir() {
        None => (WorktreeKind::Bare, canonical_pathbuf(repo.git_dir())),
        Some(work_dir) => {
            let linked = canonical_pathbuf(repo.git_dir()) != canonical_pathbuf(repo.common_dir());
            let kind = if linked {
                WorktreeKind::Linked
            } else {
                WorktreeKind::Main
            };
            (kind, canonical_pathbuf(work_dir))
        }
    };

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());

    Ok(RepoFacts {
        handle,
        kind,
        root,
        name,
    })
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
    fn facts_classify_a_normal_repo_as_the_main_worktree() {
        let dir = fixture();
        let facts = read_repo_facts(dir.path()).unwrap();
        assert_eq!(facts.kind, WorktreeKind::Main);
        // The root is the working tree, and the handle matches read_handle.
        assert_eq!(facts.handle, read_handle(dir.path()).unwrap());
        // Canonical root, so the name is the working tree's directory base name.
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(facts.root, expected);
    }

    #[test]
    fn facts_classify_a_linked_worktree_explicitly() {
        let dir = fixture();
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("linked");
        git(
            dir.path(),
            &["worktree", "add", wt_path.to_str().unwrap(), "feature"],
        );

        let main = read_repo_facts(dir.path()).unwrap();
        let linked = read_repo_facts(&wt_path).unwrap();
        assert_eq!(main.kind, WorktreeKind::Main);
        assert_eq!(linked.kind, WorktreeKind::Linked);
        // Shared repository, distinct worktrees.
        assert_eq!(main.handle.repository, linked.handle.repository);
        assert_ne!(main.handle.worktree, linked.handle.worktree);
    }

    #[test]
    fn facts_classify_a_bare_repo_explicitly() {
        let dir = fixture();
        let bare_parent = tempfile::tempdir().unwrap();
        let bare = bare_parent.path().join("mirror.git");
        git(
            dir.path(),
            &["clone", "--bare", ".", bare.to_str().unwrap()],
        );

        let facts = read_repo_facts(&bare).unwrap();
        assert_eq!(facts.kind, WorktreeKind::Bare);
        // A bare repo's root is its git directory, canonicalised.
        assert_eq!(facts.root, std::fs::canonicalize(&bare).unwrap());
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
