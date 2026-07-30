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
use git_vista_protocol::{HookPolicy, RepositoryDescriptor, RepositoryKind};

use crate::sandbox::hook_policy::hook_policy_for_repo;
use crate::sandbox::probe::ProbeVerdict;

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

    /// Whether `path` (exact canonical-path equality — the same spelling
    /// `register` stored) is a registered entry's read-only flag, or `None`
    /// when nothing in the catalog was registered at that exact path.
    ///
    /// D2 (#66, Task 7): the lookup behind `sandbox::policy_for`'s read-only
    /// grant decision, and independent of `Catalog::resolve` — that one is
    /// keyed by opaque id (what a *request* addresses a repository by), this
    /// one by path (what a *sandbox policy* is built for). A linear scan
    /// over `entries`, deliberately: the catalog holds at most a handful of
    /// repositories, this runs once per git spawn, and a path→entry reverse
    /// index would be one more piece of state that could drift from the
    /// primary map for no measurable benefit at this scale.
    pub(crate) fn read_only_for_path(&self, path: &Path) -> Option<bool> {
        self.entries
            .values()
            .find(|e| e.path == path)
            .map(|e| e.read_only)
    }

    /// Drop the entry for `worktree`, returning it (`None` when not held). The
    /// allowed root it lived under stays — other clones share it.
    pub(crate) fn remove(&mut self, worktree: WorktreeId) -> Option<RepoEntry> {
        self.entries.remove(&worktree)
    }

    /// Scan `root`'s DIRECT children (ADR 0009: one deliberate root, no
    /// recursion) and register every valid git repository, allowing `root`
    /// first. `read_only` marks every registered child as a URL clone (the
    /// clones-root scan) or a normal repo (the configured repo root). Junk
    /// children are skipped and logged; a missing/unreadable root is a
    /// warning and an empty scan — the server stays healthy rather than
    /// failing startup over a config typo. Returns (registered, skipped dirs).
    pub(crate) fn scan_direct_children(&mut self, root: &Path, read_only: bool) -> (usize, usize) {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("git-vista: repo root {} not scanned: {e}", root.display());
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
            match self.register(&child, read_only) {
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
    ///
    /// `verdict` is the boot sandbox measurement
    /// ([`crate::sandbox::probe::boot_verdict`]) — see
    /// [`disclosed_hook_policy`] for what each entry does with it, and why it
    /// is a parameter rather than a global read from inside here.
    pub(crate) fn descriptors(
        &self,
        expose_paths: bool,
        verdict: Option<&ProbeVerdict>,
    ) -> Vec<RepositoryDescriptor> {
        let mut out: Vec<RepositoryDescriptor> = self
            .entries
            .values()
            .map(|e| Self::descriptor(e, expose_paths, verdict))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.worktree.cmp(&b.worktree)));
        out
    }

    /// The descriptor for one entry, or `None` when the catalog doesn't hold
    /// the id — the same capability view as [`descriptors`](Self::descriptors),
    /// for the single entry a fresh clone just registered (ADR 0008).
    pub(crate) fn descriptor_of(
        &self,
        worktree: WorktreeId,
        expose_paths: bool,
        verdict: Option<&ProbeVerdict>,
    ) -> Option<RepositoryDescriptor> {
        self.entries
            .get(&worktree)
            .map(|e| Self::descriptor(e, expose_paths, verdict))
    }

    /// One entry's wire form — shared by the list and single-entry views.
    fn descriptor(
        e: &RepoEntry,
        expose_paths: bool,
        verdict: Option<&ProbeVerdict>,
    ) -> RepositoryDescriptor {
        Self::descriptor_with_policy(e, expose_paths, disclosed_hook_policy(&e.path, verdict))
    }

    /// [`descriptor`](Self::descriptor) with the policy lookup hoisted out, so
    /// the wire-shape half is a pure function of its inputs.
    ///
    /// The split is the same one `sandbox::hook_policy` makes for the same
    /// reason: the trusted branch of the disclosure cannot be exercised without
    /// writing a marker into the operator's **real** `~/.local/state`
    /// directory, which this crate's test conventions forbid (see
    /// `sandbox::trust`'s test module for the parallel-test race that rule
    /// exists to prevent). Hoisting lets a test feed the mapping's own answer
    /// for a trusted repository in and check the descriptor carries it
    /// unchanged.
    fn descriptor_with_policy(
        e: &RepoEntry,
        expose_paths: bool,
        hook_policy: Option<HookPolicy>,
    ) -> RepositoryDescriptor {
        RepositoryDescriptor {
            repository: e.handle.repository.to_string(),
            worktree: e.handle.worktree.to_string(),
            name: e.name.clone(),
            kind: kind_to_protocol(e.kind),
            read_only: e.read_only,
            path: expose_paths.then(|| e.path.display().to_string()),
            remote_web_url: e.remote_web_url.clone(),
            hook_policy,
        }
    }
}

/// INV-15's per-repository disclosure, in the one place a
/// [`RepositoryDescriptor`] is built — this is what makes
/// `sandbox::hook_policy::hook_policy_for_repo` a *production* function rather
/// than a computation with tests and no callers.
///
/// # Why the verdict is passed in
///
/// `boot_verdict()` is process-global and set by the boot gate. Reading it from
/// inside here would make every catalog unit test's expected output depend on
/// whether some *other* test in the same binary had already driven
/// `probe::run_at_startup` — a genuinely order-dependent assertion, which is
/// the sort of test that passes for the wrong reason. `crate::state` supplies
/// the real value at the two production call sites; tests supply theirs.
///
/// # The three `None`s, and why none of them invents a policy
///
/// * **No verdict yet** — the probe has not run in this process. Unknown.
/// * **A refusal** ([`crate::sandbox::hook_policy::HookPolicyRefused`]) — the
///   host cannot supply the tier this repository's operations require, so those
///   operations refuse to run (INV-13 / ADR 0029). There is no [`HookPolicy`]
///   that honestly says that, and mapping it to
///   [`HookPolicy::Blocked`] would be exactly the degrade-and-block-hooks
///   posture ADR 0029 rejects by name — arriving through the descriptor instead
///   of through `hook_policy_for_repo`, but the same wrong claim. So: nothing is
///   disclosed, and `RepositoryDescriptor::hook_policy_requires_banner` folds
///   the absence to "fly the banner".
/// * Both are unreachable in a live server (the boot gate exits on anything but
///   `Contained`), which is why the refusal is *logged* rather than silently
///   dropped: if it ever does happen, it is evidence of something the gate was
///   supposed to have caught.
fn disclosed_hook_policy(path: &Path, verdict: Option<&ProbeVerdict>) -> Option<HookPolicy> {
    let verdict = verdict?;
    match hook_policy_for_repo(path, verdict) {
        Ok(policy) => Some(policy),
        Err(refused) => {
            eprintln!(
                "git-vista: no hook policy disclosed for {} — {refused}",
                path.display()
            );
            None
        }
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
        let (registered, skipped) = catalog.scan_direct_children(root.path(), false);
        assert_eq!(registered, 2);
        assert_eq!(skipped, 1, "the non-repo dir is skipped; files don't count");
        let names: Vec<String> = catalog
            .descriptors(false, None)
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["repo-a", "repo-b"]);
    }

    #[test]
    fn scan_of_a_missing_root_is_a_soft_zero_not_a_panic() {
        let mut catalog = Catalog::new();
        let (registered, skipped) = catalog.scan_direct_children(Path::new("/no/such/dir"), false);
        assert_eq!((registered, skipped), (0, 0));
    }

    #[test]
    fn a_clone_survives_a_simulated_restart_scan() {
        // ADR 0008: a fresh process re-scans the clones root and re-registers
        // surviving clones, keeping the clone marker (`read_only`) the picker
        // uses to offer Delete.
        let clones = tempfile::tempdir().unwrap();
        init_repo(&clones.path().join("octocat"));

        // "Restart" = a brand-new catalog scanning the same directory.
        let mut catalog = Catalog::new();
        let (registered, skipped) = catalog.scan_direct_children(clones.path(), true);
        assert_eq!((registered, skipped), (1, 0));
        let d = catalog.descriptors(false, None);
        assert_eq!(d[0].name, "octocat");
        assert!(d[0].read_only, "re-registered clones keep the clone marker");
    }

    #[test]
    fn descriptor_of_reports_one_entry_and_fails_closed_on_unknown_ids() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, true).unwrap();

        let d = catalog
            .descriptor_of(handle.worktree, false, None)
            .expect("known id");
        assert_eq!(d, catalog.descriptors(false, None)[0]);
        assert!(d.read_only);

        let stranger = WorktreeId::from_git_dir("/nowhere/.git/worktrees/ghost");
        assert!(catalog.descriptor_of(stranger, false, None).is_none());
    }

    #[test]
    fn remove_drops_the_entry_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, true).unwrap();

        assert!(catalog.remove(handle.worktree).is_some());
        assert!(
            catalog.resolve(handle.worktree).is_none(),
            "gone after remove"
        );
        assert!(
            catalog.remove(handle.worktree).is_none(),
            "second remove is a no-op"
        );
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

        let hidden = catalog.descriptors(false, None);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].name, "project");
        assert!(hidden[0].path.is_none(), "no path unless opted in");

        let shown = catalog.descriptors(true, None);
        assert_eq!(
            shown[0].path.as_deref(),
            Some(std::fs::canonicalize(&repo).unwrap().to_str().unwrap())
        );
    }

    // --- INV-15: the per-repository hook policy on the descriptor (#202) ----

    /// The production path, end to end: a real, untrusted repository on a
    /// contained host discloses `strict` on its descriptor.
    ///
    /// Nothing here fabricates the trust answer — the repository is a fresh
    /// temp directory with no marker in the operator's trust store, so
    /// `hook_policy_for_repo` reaches the real `trust::is_trusted` and gets its
    /// fail-closed `false`. The expected value is read from the dispatch
    /// (`tier_for`) rather than written as a literal, so a change to what a
    /// local operation on an untrusted repository runs under fails this test
    /// instead of silently re-baselining it.
    #[test]
    fn a_descriptor_discloses_the_tier_an_untrusted_repository_actually_runs_in() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, false).unwrap();

        let expected = crate::sandbox::hook_policy::hook_policy_for_tier(crate::sandbox::tier_for(
            crate::sandbox::NetworkNeed::Local,
            false,
        ));
        assert_eq!(expected, HookPolicy::Strict, "fixture invariant");

        let d = catalog.descriptors(false, Some(&ProbeVerdict::Contained));
        assert_eq!(d[0].hook_policy, Some(expected));
        assert!(
            !d[0].hook_policy_requires_banner(),
            "strict is the one tier that earns a silent banner"
        );
        // The single-entry view must agree with the list view — they are two
        // routes to the same disclosure and a client sees both.
        assert_eq!(
            catalog
                .descriptor_of(handle.worktree, false, Some(&ProbeVerdict::Contained))
                .unwrap(),
            d[0]
        );
    }

    /// The two ways a descriptor legitimately discloses nothing, and the
    /// property both must share: the field is *absent*, and the banner flies.
    ///
    /// A `Blocked` here would be the degrade-and-block-hooks posture ADR 0029
    /// rejects by name, re-entering through the descriptor instead of through
    /// `hook_policy_for_repo`; asserting `!= Some(Blocked)` as well as `== None`
    /// is what makes that non-vacuous, since `None` alone would still pass if a
    /// later edit produced some *other* fabricated policy.
    #[test]
    fn a_refusing_or_unmeasured_host_discloses_no_policy_and_never_blocked() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        catalog.register(&repo, false).unwrap();

        let absent = ProbeVerdict::CapabilityAbsent {
            missing: vec!["bwrap"],
        };
        let fail_open = ProbeVerdict::FailOpen {
            failed_checks: vec!["fs_write_outside=OPEN want=DENIED".to_string()],
        };

        for (label, verdict) in [
            ("no boot verdict yet", None),
            ("the host cannot supply the tier (ADR 0029)", Some(&absent)),
            ("the sandbox self-test found a hole", Some(&fail_open)),
        ] {
            let d = catalog.descriptors(false, verdict);
            assert_eq!(d[0].hook_policy, None, "{label}: nothing is disclosed");
            assert_ne!(
                d[0].hook_policy,
                Some(HookPolicy::Blocked),
                "{label}: 'run it but block hooks' is the posture ADR 0029 \
                 rejects — a refusal must not be re-disclosed as a policy"
            );
            assert!(
                d[0].hook_policy_requires_banner(),
                "{label}: an undisclosed policy must fly the banner"
            );
        }
    }

    /// The descriptor carries the policy it is given, unchanged, for a trusted
    /// repository as well as an untrusted one.
    ///
    /// **What this does and does not prove.** The two policies come from the
    /// real dispatch (`tier_for(_, trusted)`), so the *values* are not made up;
    /// what is not exercised here is the trust *lookup* itself, because
    /// granting operator trust writes a marker into the operator's real
    /// `~/.local/state` directory and this crate's test conventions forbid a
    /// test doing that (see `sandbox::trust`'s test module for the
    /// parallel-test race behind that rule). The lookup's fail-closed behaviour
    /// is proved in `sandbox::trust`, and its wiring into the disclosure in
    /// `sandbox::hook_policy`. This covers the remaining link — that the
    /// descriptor neither drops nor rewrites the answer, and in particular does
    /// not flatten a trusted repository's banner-flying `unsandboxed` into the
    /// silent `strict`.
    #[test]
    fn a_descriptor_carries_the_trusted_and_untrusted_policies_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("project");
        init_repo(&repo);
        let mut catalog = Catalog::new();
        catalog.allow_root(root.path());
        let handle = catalog.register(&repo, false).unwrap();
        let entry = catalog.resolve(handle.worktree).unwrap();

        for (trusted, want, banner) in [
            (false, HookPolicy::Strict, false),
            (true, HookPolicy::Unsandboxed, true),
        ] {
            let policy = crate::sandbox::hook_policy::hook_policy_for_tier(
                crate::sandbox::tier_for(crate::sandbox::NetworkNeed::Local, trusted),
            );
            assert_eq!(policy, want, "fixture invariant (trusted={trusted})");

            let d = Catalog::descriptor_with_policy(entry, false, Some(policy));
            assert_eq!(d.hook_policy, Some(want), "trusted={trusted}");
            assert_eq!(
                d.hook_policy_requires_banner(),
                banner,
                "trusted={trusted}: a trusted repository must not be able to go \
                 silent, and an untrusted one on a contained host must not \
                 needlessly warn"
            );
        }
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

        let descriptors = catalog.descriptors(false, None);
        assert_eq!(descriptors.len(), 1, "same worktree is one entry");
        assert!(descriptors[0].read_only, "the entry was updated in place");
    }
}
