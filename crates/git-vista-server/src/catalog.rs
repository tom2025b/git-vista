//! The server-owned repository catalog (M1.03).
//!
//! # Why this exists
//!
//! Before this module the server addressed "the repository" by a filesystem
//! path. That is the wrong identity for a browser-facing API: a path leaks the
//! server's filesystem, and — the real hazard — the moment a request could carry
//! a path, a traversal (`../../etc`) or a symlink could point the server's git
//! commands at something the operator never meant to expose. M1.01 introduced
//! opaque, path-independent [`RepositoryId`]/[`WorktreeId`] handles; this module
//! is the **only** thing that maps such a handle back to a path, and it does so
//! from a set the server itself registered, never from client input.
//!
//! # The guarantee
//!
//! - A request selects a repository by an opaque [`WorktreeId`], never a path.
//! - [`Catalog::resolve`] returns a path *only* for an id the catalog holds;
//!   anything else is `None` — a request for an unknown id fails closed.
//! - [`Catalog::register`] admits a path only when its **canonical** (symlink-
//!   resolved) root lies within an allowed root. A `../` traversal or a symlink
//!   escaping the root canonicalises to its real location and is then rejected,
//!   so both fail closed.
//! - Bare repositories and linked worktrees are classified explicitly (via
//!   [`git_vista_git::read_repo_facts`]) rather than assumed away.
//! - [`Catalog::descriptors`] reports capabilities by id and omits absolute
//!   paths unless the operator opts in — the catalog never leaks the layout of
//!   the machine to the browser by default.
//!
//! The catalog owns registration and resolution; the process-global wiring that
//! holds one lives in [`crate::state`].
//!
//! [`RepositoryId`]: git_vista_core::identity::RepositoryId
//! [`WorktreeId`]: git_vista_core::identity::WorktreeId

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_git::{read_repo_facts, WorktreeKind};
use git_vista_protocol::{RepositoryDescriptor, RepositoryKind};

/// Why a path could not be admitted to the catalog. Surfaced today only through
/// [`Display`](std::fmt::Display) — the trusted-launch path logs it and drops to
/// degraded mode. The request-driven mutation paths that turn these into HTTP
/// statuses arrive with the typed operations of M1.06.
#[derive(Debug)]
pub(crate) enum CatalogError {
    /// The path wouldn't open/classify as a git repository (`gix` couldn't read
    /// it). Carries git's own reason.
    NotARepository(git_vista_git::RepoError),
    /// The repository's canonical root is not within any allowed root — a
    /// traversal or symlink escape, or simply a repo the operator didn't allow.
    OutsideAllowedRoots,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::NotARepository(e) => write!(f, "not a git repository: {e}"),
            CatalogError::OutsideAllowedRoots => write!(f, "outside the allowed roots"),
        }
    }
}

/// The set of directories under which repositories may be opened. Stored
/// canonicalised, so containment is checked against real, symlink-resolved paths.
#[derive(Debug, Default)]
struct AllowedRoots {
    roots: Vec<PathBuf>,
}

impl AllowedRoots {
    /// Add `dir` as an allowed root. Canonicalised first (symlinks resolved); a
    /// directory that doesn't canonicalise — it must exist to be a real root — is
    /// skipped with a warning rather than stored unresolved, since an unresolved
    /// root could never soundly contain a canonical path.
    fn allow(&mut self, dir: &Path) {
        match std::fs::canonicalize(dir) {
            Ok(canonical) => {
                if !self.roots.contains(&canonical) {
                    self.roots.push(canonical);
                }
            }
            Err(e) => eprintln!(
                "git-vista: ignoring allowed root {} (can't canonicalise: {e})",
                dir.display()
            ),
        }
    }

    /// Whether `canonical` (which the caller must already have canonicalised) is
    /// one of the roots or a descendant of one. Uses component-wise
    /// [`Path::starts_with`], so `/srv/repos-secret` is *not* considered within
    /// `/srv/repos` — a string-prefix check would wrongly admit it.
    fn contains(&self, canonical: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| canonical == root || canonical.starts_with(root))
    }
}

/// One admitted repository: its opaque handle, its canonical path (the one thing
/// never sent to the client by default), and how it's classified.
#[derive(Debug, Clone)]
pub(crate) struct RepoEntry {
    /// The repository + worktree ids this entry is addressed by.
    pub(crate) handle: RepositoryHandle,
    /// The canonical root directory — the working tree, or the git dir for a bare
    /// repo. The catalog is the only holder of this path on the request path.
    pub(crate) path: PathBuf,
    /// Short, non-path display label (the directory base name).
    pub(crate) name: String,
    /// Bare / main / linked classification.
    pub(crate) kind: WorktreeKind,
    /// View-only (a clone opened from a URL): every mutation is refused.
    pub(crate) read_only: bool,
    /// Normalized web base of the repo's origin remote (ADR 0010), read once at
    /// registration. `None` = no usable remote.
    pub(crate) remote_web_url: Option<String>,
}

/// The registry mapping opaque [`WorktreeId`]s to the repositories the server may
/// serve. Keyed by worktree id because that is what a request addresses: the main
/// working tree and each linked worktree are distinct servable targets that share
/// one [`RepositoryId`](git_vista_core::identity::RepositoryId).
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    roots: AllowedRoots,
    entries: HashMap<WorktreeId, RepoEntry>,
}

impl Catalog {
    /// An empty catalog with no allowed roots and no entries.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Permit repositories under `dir` (canonicalised) to be registered.
    pub(crate) fn allow_root(&mut self, dir: &Path) {
        self.roots.allow(dir);
    }

    /// Whether `canonical` lies within an allowed root — the single check the
    /// clone handler uses to confirm a destination stays inside the clones root.
    pub(crate) fn contains_path(&self, canonical: &Path) -> bool {
        self.roots.contains(canonical)
    }

    /// Admit the repository at `path`, returning the opaque handle it is now
    /// addressed by. Fails closed unless the repository's canonical root is within
    /// an allowed root: a `../` traversal or a symlink escaping the allowlist
    /// resolves to its real location and is rejected here, never registered.
    ///
    /// `read_only` marks the entry view-only (a URL clone). Re-registering the
    /// same worktree updates its entry (e.g. its read-only flag), so this is
    /// idempotent on identity.
    pub(crate) fn register(
        &mut self,
        path: &Path,
        read_only: bool,
    ) -> Result<RepositoryHandle, CatalogError> {
        let facts = read_repo_facts(path).map_err(CatalogError::NotARepository)?;
        if !self.roots.contains(&facts.root) {
            return Err(CatalogError::OutsideAllowedRoots);
        }
        let handle = facts.handle;
        let remote_web_url = git_vista_git::remote_web_base(&facts.root);
        self.entries.insert(
            handle.worktree,
            RepoEntry {
                handle,
                path: facts.root,
                name: facts.name,
                kind: facts.kind,
                read_only,
                remote_web_url,
            },
        );
        Ok(handle)
    }

    /// Resolve an opaque worktree id to its entry, or `None` for any id the
    /// catalog does not hold — the fail-closed path a request for an unknown or
    /// forged id takes.
    pub(crate) fn resolve(&self, worktree: WorktreeId) -> Option<&RepoEntry> {
        self.entries.get(&worktree)
    }

    /// Scan `root`'s DIRECT children (ADR 0009: one deliberate root, no
    /// recursion) and register every valid git repository, allowing `root`
    /// first. Junk children are skipped and logged; a missing/unreadable root
    /// is a warning and an empty scan — the server stays healthy rather than
    /// failing startup over a config typo. Returns (registered, skipped dirs).
    pub(crate) fn scan_direct_children(&mut self, root: &Path) -> (usize, usize) {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "git-vista: repo root {} not scanned: {e}",
                    root.display()
                );
                return (0, 0);
            }
        };
        self.allow_root(root);
        let (mut registered, mut skipped) = (0, 0);
        let mut children: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        children.sort(); // stable scan/log order
        for child in children {
            match self.register(&child, false) {
                Ok(_) => registered += 1,
                Err(e) => {
                    skipped += 1;
                    eprintln!("git-vista: skipping {} ({e})", child.display());
                }
            }
        }
        (registered, skipped)
    }

    /// The capability view of the catalog: one [`RepositoryDescriptor`] per entry,
    /// addressed by id. Absolute paths are included only when `expose_paths` is
    /// set (the operator's opt-in); otherwise the descriptors carry no path.
    /// Sorted by display name so the report is stable across calls.
    pub(crate) fn descriptors(&self, expose_paths: bool) -> Vec<RepositoryDescriptor> {
        let mut out: Vec<RepositoryDescriptor> = self
            .entries
            .values()
            .map(|e| RepositoryDescriptor {
                repository: e.handle.repository.to_string(),
                worktree: e.handle.worktree.to_string(),
                name: e.name.clone(),
                kind: kind_to_protocol(e.kind),
                read_only: e.read_only,
                path: expose_paths.then(|| e.path.display().to_string()),
                remote_web_url: e.remote_web_url.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.worktree.cmp(&b.worktree)));
        out
    }
}

/// Map the git crate's bare/main/linked classification to the protocol enum the
/// client sees.
fn kind_to_protocol(kind: WorktreeKind) -> RepositoryKind {
    match kind {
        WorktreeKind::Bare => RepositoryKind::Bare,
        WorktreeKind::Main => RepositoryKind::MainWorktree,
        WorktreeKind::Linked => RepositoryKind::LinkedWorktree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// `git init -q` a fresh repository at `dir` so `read_repo_facts` can classify
    /// it. No commit is needed — a working tree is enough to be the main worktree.
    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed at {}", dir.display());
    }

    // --- AllowedRoots containment ------------------------------------------

    #[test]
    fn allowed_roots_admit_the_root_and_its_descendants_only() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repos");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        // A sibling that shares a string prefix with the root but is not inside it.
        let sibling = base.path().join("repos-secret");
        std::fs::create_dir_all(&sibling).unwrap();

        let mut roots = AllowedRoots::default();
        roots.allow(&root);
        let c = |p: &Path| std::fs::canonicalize(p).unwrap();

        assert!(roots.contains(&c(&root)), "the root itself is contained");
        assert!(roots.contains(&c(&root.join("a/b"))), "a descendant is in");
        assert!(
            !roots.contains(&c(&sibling)),
            "a string-prefix sibling must NOT be admitted"
        );
        assert!(
            !roots.contains(&c(base.path())),
            "the parent of the root is not contained"
        );
    }

    // --- register / resolve fail-closed ------------------------------------

    #[test]
    fn register_admits_a_repo_under_an_allowed_root_and_resolves_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);

        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, false).unwrap();

        let entry = catalog.resolve(handle.worktree).expect("resolves by id");
        assert_eq!(entry.handle, handle);
        assert_eq!(entry.name, "project");
        assert_eq!(entry.kind, WorktreeKind::Main);
        assert!(!entry.read_only);
    }

    #[test]
    fn register_fails_closed_outside_the_allowed_roots() {
        let allowed = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let repo = elsewhere.path().join("project");
        init_repo(&repo);

        let mut catalog = Catalog::new();
        catalog.allow_root(allowed.path()); // does NOT cover `elsewhere`
        let err = catalog.register(&repo, false).unwrap_err();
        assert!(matches!(err, CatalogError::OutsideAllowedRoots));
    }

    #[test]
    #[cfg(unix)]
    fn register_fails_closed_on_a_symlink_escaping_the_allowed_root() {
        // A repository outside the allowed root, reached through a symlink that
        // lives *inside* it. The canonical root resolves outside → rejected.
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real_repo = outside.path().join("secret");
        init_repo(&real_repo);
        let link = allowed.path().join("looks-inside");
        std::os::unix::fs::symlink(&real_repo, &link).unwrap();

        let mut catalog = Catalog::new();
        catalog.allow_root(allowed.path());
        let err = catalog
            .register(&link, false)
            .expect_err("a symlink escaping the allowed root must fail closed");
        assert!(matches!(err, CatalogError::OutsideAllowedRoots));
    }

    #[test]
    fn register_fails_closed_when_not_a_git_repository() {
        let root = tempfile::tempdir().unwrap();
        let not_a_repo = root.path().join("plain");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let err = catalog.register(&not_a_repo, false).unwrap_err();
        assert!(matches!(err, CatalogError::NotARepository(_)));
    }

    #[test]
    fn resolve_returns_none_for_an_unknown_id() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        catalog.register(&repo, false).unwrap();

        // An id for a repository the catalog never registered resolves to nothing.
        let stranger = WorktreeId::from_git_dir("/nowhere/.git/worktrees/ghost");
        assert!(catalog.resolve(stranger).is_none());
    }

    // --- root scan (ADR 0009) ----------------------------------------------

    #[test]
    fn scan_registers_direct_child_repos_and_skips_junk() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("repo-a"));
        init_repo(&root.path().join("repo-b"));
        std::fs::create_dir_all(root.path().join("not-a-repo")).unwrap();
        std::fs::write(root.path().join("stray-file.txt"), "x").unwrap();
        // A repo one level deeper must NOT register (direct children only).
        init_repo(&root.path().join("not-a-repo/nested"));

        let mut catalog = Catalog::new();
        let (registered, skipped) = catalog.scan_direct_children(root.path());
        assert_eq!(registered, 2);
        assert_eq!(skipped, 1, "the non-repo dir is skipped; files don't count");
        let names: Vec<String> = catalog
            .descriptors(false)
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["repo-a", "repo-b"]);
    }

    #[test]
    fn scan_of_a_missing_root_is_a_soft_zero_not_a_panic() {
        let mut catalog = Catalog::new();
        let (registered, skipped) = catalog.scan_direct_children(Path::new("/no/such/dir"));
        assert_eq!((registered, skipped), (0, 0));
    }

    // --- descriptors: no path by default -----------------------------------

    #[test]
    fn descriptors_omit_absolute_paths_by_default_and_include_them_on_opt_in() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        catalog.register(&repo, false).unwrap();

        let hidden = catalog.descriptors(false);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].name, "project");
        assert!(hidden[0].path.is_none(), "no path unless opted in");

        let shown = catalog.descriptors(true);
        assert_eq!(
            shown[0].path.as_deref(),
            Some(std::fs::canonicalize(&repo).unwrap().to_str().unwrap())
        );
    }

    #[test]
    fn re_registering_the_same_worktree_updates_rather_than_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        catalog.register(&repo, false).unwrap();
        catalog.register(&repo, true).unwrap(); // same identity, now read-only

        let descriptors = catalog.descriptors(false);
        assert_eq!(descriptors.len(), 1, "same worktree is one entry");
        assert!(descriptors[0].read_only, "the entry was updated in place");
    }
}
