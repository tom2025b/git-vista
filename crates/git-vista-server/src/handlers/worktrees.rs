//! `GET /api/worktrees` (M11, #546): the worktree census — every linked
//! worktree sibling of a repository, read fresh from
//! `git worktree list --porcelain -z` on every request (no caching, same
//! posture as `/api/status`: another process can add or remove a worktree at
//! any moment).
//!
//! # Where the id for each sibling comes from
//!
//! `git-vista-core::WorktreeId` is a pure hash of a worktree's *canonical git
//! directory* (`git-vista-git::read_handle` derives it by opening the
//! worktree with `gix` and hashing `repo.git_dir()`). This module reproduces
//! that same input **without gix**, because it must also produce an id for a
//! [`Serviceable::Missing`] sibling whose directory — and therefore whose
//! `.git` pointer — no longer exists:
//!
//! - For a sibling whose directory still exists, [`sibling_gitdir`] resolves
//!   its git directory exactly the way [`crate::sandbox::worktree`] already
//!   does for sandbox policy grants: read `<path>/.git` (a plain directory
//!   for the main worktree, a `gitdir:` pointer file for a linked one) and,
//!   for a bare record, `<path>` itself.
//! - For a sibling whose directory is gone, there is nothing at `<path>/.git`
//!   left to read — but the **administrative** directory the pointer used to
//!   name, `<commondir>/worktrees/<name>`, survives on its own (that is what
//!   makes the worktree `prunable` rather than simply absent from git's
//!   knowledge). [`missing_sibling_gitdir`] finds the right `<name>` by
//!   reading every admin directory's own `gitdir` file — which records the
//!   worktree's original path — and matching it against the porcelain
//!   record's path, then hashes the admin directory itself.
//!
//! Either way the string handed to `WorktreeId::from_git_dir` is a
//! `std::fs::canonicalize`d path, matching `git-vista-git::canonical_dir`'s
//! own construction — so a sibling this server has already registered gets
//! *the same id* here as in the catalog. `self_worktree_id_matches_catalog`
//! below proves it against a real `git worktree add`.
//!
//! # No new sandbox tier or grant (acceptance criterion 6)
//!
//! `git worktree list --porcelain -z` is a plain local read, declared
//! `NetworkNeed::Local` exactly like `worktree_status`/`rev_parse`/the tag
//! listing — it runs under whatever tier `policy_for` already computes for
//! `(repo, Local, trust)`. The plain filesystem reads this module does on top
//! (`sibling_gitdir`, `missing_sibling_gitdir`, the allowed-roots check) are
//! the same *unsandboxed, server-trusted* class of read
//! `crate::sandbox::worktree::linked_worktree_dirs` already performs to
//! compute a policy grant in the first place — nothing here reaches into a
//! sandboxed child process, so there is no new grant to add.

use std::path::{Path, PathBuf};

use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::{
    parse_worktree_list_porcelain_z, BranchName, CommitOid, Serviceable, WorktreeCensus,
    WorktreeListRecord, WorktreeSibling, WorktreeToken,
};

use crate::git_cmd::git_stdout_capped;
use crate::handlers::read::{resolve_repo, RepoQuery};
use crate::sandbox::worktree::linked_worktree_dirs;
use crate::state::{expose_paths, path_is_allowed};

/// Upper bound on `git worktree list --porcelain -z`'s stdout. Same ceiling as
/// [`crate::handlers::read`]'s status-v2 cap (8 MiB) for the same reason: a
/// truncated stream can cut a record in the middle, and there is no honest way
/// to parse a partial one, so a cap hit is refused rather than best-effort
/// parsed.
const WORKTREE_LIST_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// `GET /api/worktrees`: the [`WorktreeCensus`] of the repository `?repo=`
/// selects (or the current default selection). Always `200` — a failed
/// enumeration is reported *as data* (`WorktreeCensus::CensusFailed`), not as
/// an HTTP error, because every consumer of this shape (this listing today;
/// a future collision-aware checkout) must handle "the read failed" the same
/// way it handles "the read succeeded", never by inferring it from a status
/// code a caller might not check.
pub(crate) async fn worktree_list(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let census = worktree_census(&repo).await;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(census)))
}

/// Build the [`WorktreeCensus`] for `repo`: shell out to
/// `git worktree list --porcelain -z`, parse it strictly, and enrich every
/// record with this server's identity and allowed-roots fence. Infallible at
/// the type level — every failure this function can hit becomes
/// [`WorktreeCensus::CensusFailed`], never a panic or a silently-empty list.
pub(crate) async fn worktree_census(repo: &Path) -> WorktreeCensus {
    let (bytes, truncated) = match git_stdout_capped(
        repo,
        &[
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
            "-z".to_string(),
        ],
        "/api/worktrees",
        WORKTREE_LIST_STDOUT_CAP,
    )
    .await
    {
        Ok(out) => out,
        Err((_status, message)) => return failed(message),
    };
    if truncated {
        return failed("worktree list exceeded the read cap".to_string());
    }

    let records = match parse_worktree_list_porcelain_z(&bytes) {
        Ok(records) => records,
        Err(e) => return failed(e.to_string()),
    };

    let commondir = match common_dir(repo) {
        Ok(dir) => dir,
        Err(reason) => return failed(reason),
    };
    let canonical_repo = match std::fs::canonicalize(repo) {
        Ok(p) => p,
        Err(e) => return failed(format!("{} does not canonicalise: {e}", repo.display())),
    };

    let mut siblings = Vec::with_capacity(records.len());
    for record in records {
        match enrich(&record, &commondir, &canonical_repo) {
            Ok(sibling) => siblings.push(sibling),
            Err(reason) => return failed(reason),
        }
    }
    WorktreeCensus::Observed { siblings }
}

fn failed(reason: String) -> WorktreeCensus {
    WorktreeCensus::CensusFailed { reason }
}

/// Turn one raw porcelain record into a wire [`WorktreeSibling`]: resolve its
/// identity, decide [`Serviceable`], and validate its branch/head shapes.
fn enrich(
    record: &WorktreeListRecord,
    commondir: &Path,
    canonical_repo: &Path,
) -> Result<WorktreeSibling, String> {
    let path = Path::new(&record.path);
    let exists = match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("{}: {e}", record.path)),
    };

    let (gitdir, serviceable, is_current) = if exists {
        let gitdir = sibling_gitdir(path, record.bare)?;
        let canonical_path = std::fs::canonicalize(path)
            .map_err(|e| format!("{} does not canonicalise: {e}", record.path))?;
        let serviceable = if path_is_allowed(&canonical_path) {
            Serviceable::Yes
        } else {
            Serviceable::OutsideAllowedRoots
        };
        let is_current = canonical_path == canonical_repo;
        (gitdir, serviceable, is_current)
    } else {
        let gitdir = missing_sibling_gitdir(commondir, &record.path)?;
        (gitdir, Serviceable::Missing, false)
    };

    let id = WorktreeId::from_git_dir(&gitdir.to_string_lossy());
    let branch = record
        .branch
        .as_deref()
        .map(BranchName::new)
        .transpose()
        .map_err(|e| format!("{}: branch: {e}", record.path))?;
    let head = record
        .head
        .as_deref()
        .map(CommitOid::new)
        .transpose()
        .map_err(|e| format!("{}: HEAD: {e}", record.path))?;

    Ok(WorktreeSibling {
        id: WorktreeToken::new(id.to_string())
            .expect("a formatted uuid is never empty or option-shaped"),
        path: expose_paths().then(|| record.path.clone()),
        branch,
        head,
        is_current,
        locked: record.locked,
        prunable: record.prunable,
        serviceable,
    })
}

/// This repository's own shared common git directory, canonicalised —
/// `<repo>/.git` for a plain worktree, the shared dir a linked worktree's
/// `.git` pointer resolves to, or `repo` itself for a bare repository. Every
/// sibling `git worktree list` reports for `repo` shares this same directory
/// by definition, which is what makes it the right anchor for
/// [`missing_sibling_gitdir`]'s admin-directory scan.
fn common_dir(repo: &Path) -> Result<PathBuf, String> {
    if let Some(dirs) = linked_worktree_dirs(repo)? {
        return Ok(dirs.commondir);
    }
    let dotgit = repo.join(".git");
    if dotgit.is_dir() {
        return std::fs::canonicalize(&dotgit)
            .map_err(|e| format!("{} does not canonicalise: {e}", dotgit.display()));
    }
    // No `.git` at all: `repo` must be a bare repository (the catalog already
    // validated `repo` is a real repository before this is ever called).
    std::fs::canonicalize(repo)
        .map_err(|e| format!("{} does not canonicalise: {e}", repo.display()))
}

/// The canonical git directory of a sibling whose own directory still exists,
/// mirroring exactly what `gix`/`git rev-parse --git-dir` would resolve for
/// it: a linked worktree's `.git` pointer (via
/// [`crate::sandbox::worktree::linked_worktree_dirs`]), a plain `.git`
/// directory for the main worktree, or `path` itself for a bare record.
fn sibling_gitdir(path: &Path, bare: bool) -> Result<PathBuf, String> {
    if let Some(dirs) = linked_worktree_dirs(path)? {
        return Ok(dirs.gitdir);
    }
    let dotgit = path.join(".git");
    if dotgit.is_dir() {
        return std::fs::canonicalize(&dotgit)
            .map_err(|e| format!("{} does not canonicalise: {e}", dotgit.display()));
    }
    if bare {
        return std::fs::canonicalize(path)
            .map_err(|e| format!("{} does not canonicalise: {e}", path.display()));
    }
    Err(format!(
        "{}: porcelain reports a non-bare worktree but `.git` is neither a directory nor a linked pointer",
        path.display()
    ))
}

/// The canonical *administrative* git directory (`<commondir>/worktrees/<name>`)
/// of a sibling whose working directory is gone (`Serviceable::Missing`).
///
/// The admin directory survives its worktree's own deletion — that is
/// precisely what makes git call it `prunable` instead of forgetting it — so
/// this reads every admin directory's `gitdir` file (which records the
/// worktree's original path, written once at `git worktree add` time and
/// never updated) and matches it against `reported_path` by exact string
/// comparison. Both sides come from git's own bookkeeping for the same
/// worktree, so they agree byte-for-byte; canonicalising `reported_path`
/// itself is not an option, since the directory it names no longer exists.
fn missing_sibling_gitdir(commondir: &Path, reported_path: &str) -> Result<PathBuf, String> {
    let worktrees_dir = commondir.join("worktrees");
    let entries = std::fs::read_dir(&worktrees_dir)
        .map_err(|e| format!("{}: {e}", worktrees_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", worktrees_dir.display()))?;
        let admin_dir = entry.path();
        let gitdir_file = admin_dir.join("gitdir");
        let Ok(target) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let target = target.trim();
        let recorded_path = target.strip_suffix("/.git").unwrap_or(target);
        if recorded_path == reported_path {
            return std::fs::canonicalize(&admin_dir)
                .map_err(|e| format!("{} does not canonicalise: {e}", admin_dir.display()));
        }
    }
    Err(format!(
        "{reported_path}: prunable but no admin directory under {} records this path",
        worktrees_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::Serviceable;
    use std::process::Command;

    // Every test below drives `common_dir`/`enrich` directly rather than the
    // async `worktree_census`/`worktree_list` — deliberately. Those two
    // functions' only jobs beyond parsing are (1) shell `git worktree list`
    // through this server's sandboxed spawn path and (2) fold each record
    // through `enrich`; (1) is exactly the same `git_stdout_capped` chokepoint
    // `worktree_status_v2` already uses, so it is proven code, not new code,
    // and this dev container cannot exercise *any* sandboxed spawn at all (no
    // Landlock ABI 6, confirmed by pre-existing, unrelated tests —
    // `handlers::read::status_suite` — failing identically here). Calling
    // `git` directly with `std::process::Command` (never through
    // `crate::sandboxed`) keeps every one of these tests exercising the
    // logic this module actually adds: identity derivation, `Serviceable`,
    // `is_current`, and strict field validation.

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
        // The repository itself must be inside an allowed root — a real
        // server always registers the repo it serves before anything asks
        // for its census. Linked worktrees created *under* `dir` inherit
        // this (they're descendants); the `outside`-roots test below
        // deliberately uses a *different* tempdir precisely so it is not.
        crate::state::allow_repo_root(dir.path());
        dir
    }

    fn add_worktree(main: &Path, at: &Path, branch: &str) {
        git(
            main,
            &["worktree", "add", "-q", at.to_str().unwrap(), "-b", branch],
        );
    }

    /// Real `git worktree list --porcelain -z` output for `repo`, parsed —
    /// run directly via `std::process::Command`, never through this crate's
    /// sandbox (see the module note above for why).
    fn list_records(repo: &Path) -> Vec<WorktreeListRecord> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain", "-z"])
            .current_dir(repo)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git worktree list failed");
        parse_worktree_list_porcelain_z(&output.stdout).expect("well-formed porcelain")
    }

    /// [`list_records`] plus [`common_dir`]/[`enrich`] — the whole pipeline
    /// [`worktree_census`] itself runs, minus the sandboxed spawn.
    fn census(repo: &Path) -> Vec<WorktreeSibling> {
        let records = list_records(repo);
        let commondir = common_dir(repo).expect("common_dir");
        let canonical_repo = std::fs::canonicalize(repo).unwrap();
        records
            .iter()
            .map(|r| enrich(r, &commondir, &canonical_repo).expect("enrich"))
            .collect()
    }

    #[test]
    fn a_repo_with_no_linked_worktrees_observes_exactly_itself() {
        let dir = init_repo();
        let siblings = census(dir.path());
        assert_eq!(siblings.len(), 1);
        assert!(siblings[0].is_current);
        assert_eq!(siblings[0].serviceable, Serviceable::Yes);
    }

    #[test]
    fn exactly_one_sibling_is_current() {
        let dir = init_repo();
        let linked = tempfile::tempdir().unwrap();
        add_worktree(dir.path(), &linked.path().join("linked"), "feature");

        let siblings = census(dir.path());
        assert_eq!(siblings.len(), 2);
        assert_eq!(siblings.iter().filter(|s| s.is_current).count(), 1);
    }

    #[test]
    fn a_locked_sibling_reads_locked_and_serviceable() {
        let dir = init_repo();
        let linked = tempfile::tempdir().unwrap();
        crate::state::allow_repo_root(linked.path());
        let wt_path = linked.path().join("linked");
        add_worktree(dir.path(), &wt_path, "feature");
        git(
            dir.path(),
            &[
                "worktree",
                "lock",
                wt_path.to_str().unwrap(),
                "--reason",
                "editing",
            ],
        );

        let siblings = census(dir.path());
        let sibling = siblings.iter().find(|s| !s.is_current).unwrap();
        assert!(sibling.locked);
        assert!(!sibling.prunable);
        assert_eq!(sibling.serviceable, Serviceable::Yes);
    }

    /// The mutation-kill case the task doc names explicitly: a sibling whose
    /// directory was deleted must still appear in the list, marked `Missing`
    /// — never silently dropped — and with a real, resolvable id.
    #[test]
    fn a_prunable_sibling_with_a_deleted_directory_reads_missing_not_dropped() {
        let dir = init_repo();
        let linked = tempfile::tempdir().unwrap();
        let wt_path = linked.path().join("linked");
        add_worktree(dir.path(), &wt_path, "feature");
        std::fs::remove_dir_all(&wt_path).unwrap();

        let siblings = census(dir.path());
        assert_eq!(
            siblings.len(),
            2,
            "the missing sibling must still be listed"
        );
        let missing = siblings.iter().find(|s| !s.is_current).unwrap();
        assert_eq!(missing.serviceable, Serviceable::Missing);
        assert!(missing.prunable);
        assert!(!missing.id.as_str().is_empty());
    }

    /// A sibling outside the allowed roots still counts as observed (never
    /// dropped) and is marked `OutsideAllowedRoots`, not folded into `Yes` or
    /// hidden — the mutation-kill case the task doc calls out by name, and
    /// this test is mutation-proven two different ways by hand:
    ///
    /// 1. **Fold the arm.** In [`enrich`], `let serviceable = Serviceable::Yes;`
    ///    unconditionally in place of the `if path_is_allowed(..) { .. } else
    ///    { .. }`. Red at `assert_eq!(sibling.serviceable,
    ///    Serviceable::OutsideAllowedRoots)` below (`left: OutsideAllowedRoots,
    ///    right: Yes`).
    /// 2. **Drop the sibling.** In this test's own `census()` helper above,
    ///    `.filter(|s| s.serviceable != Serviceable::OutsideAllowedRoots)`
    ///    appended to the iterator chain. Red at a *different* assertion,
    ///    `assert_eq!(siblings.len(), 2)` below (`left: 1, right: 2`) — proving
    ///    the test would also catch the silent-omission failure mode even if
    ///    `Serviceable::OutsideAllowedRoots` itself stopped being folded.
    ///
    /// Both mutations were reverted afterwards; `diff -q` against the
    /// pre-mutation file reported no difference before this test was
    /// confirmed green again.
    #[test]
    fn a_sibling_outside_the_allowed_roots_is_listed_and_refused() {
        let dir = init_repo();
        // No `allow_repo_root` call for this tempdir: it is guaranteed
        // outside whatever roots this process's catalog has configured, and
        // `AllowedRoots::contains` is a component-wise `starts_with`, so a
        // sibling tempdir under the same `/tmp` prefix is never mistaken for
        // being inside another test's allowed root.
        let outside = tempfile::tempdir().unwrap();
        add_worktree(dir.path(), &outside.path().join("linked"), "feature");

        let siblings = census(dir.path());
        assert_eq!(siblings.len(), 2);
        let sibling = siblings.iter().find(|s| !s.is_current).unwrap();
        assert_eq!(sibling.serviceable, Serviceable::OutsideAllowedRoots);
    }

    /// The id this module derives for the repository's own worktree must
    /// match `git-vista-git`'s production identity derivation exactly, since
    /// that is the id already stored in the catalog for this same worktree —
    /// a mismatch would mean the census names a sibling the catalog itself
    /// cannot resolve.
    #[test]
    fn self_worktree_id_matches_catalog_identity() {
        let dir = init_repo();
        let expected = git_vista_git::read_handle(dir.path()).unwrap().worktree;

        let siblings = census(dir.path());
        let me = siblings.iter().find(|s| s.is_current).unwrap();
        assert_eq!(me.id.as_str(), expected.to_string());
    }

    /// Same identity check for a *linked* worktree, since that is the path
    /// [`sibling_gitdir`] takes through `linked_worktree_dirs` rather than
    /// the plain-directory branch.
    #[test]
    fn linked_sibling_id_matches_catalog_identity() {
        let dir = init_repo();
        let linked = tempfile::tempdir().unwrap();
        let wt_path = linked.path().join("linked");
        add_worktree(dir.path(), &wt_path, "feature");
        let expected = git_vista_git::read_handle(&wt_path).unwrap().worktree;

        let siblings = census(dir.path());
        let sibling = siblings.iter().find(|s| !s.is_current).unwrap();
        assert_eq!(sibling.id.as_str(), expected.to_string());
    }

    /// [`missing_sibling_gitdir`] must also agree with the catalog's identity
    /// for the *same worktree while it still existed* — i.e. deleting a
    /// worktree's directory must not change the id this server assigns it,
    /// or a client holding the pre-deletion id could never recognise the
    /// post-deletion `Missing` row as the same sibling.
    #[test]
    fn a_missing_siblings_id_survives_its_own_deletion() {
        let dir = init_repo();
        let linked = tempfile::tempdir().unwrap();
        let wt_path = linked.path().join("linked");
        add_worktree(dir.path(), &wt_path, "feature");
        let id_before_deletion = git_vista_git::read_handle(&wt_path).unwrap().worktree;
        std::fs::remove_dir_all(&wt_path).unwrap();

        let siblings = census(dir.path());
        let missing = siblings.iter().find(|s| !s.is_current).unwrap();
        assert_eq!(missing.serviceable, Serviceable::Missing);
        assert_eq!(missing.id.as_str(), id_before_deletion.to_string());
    }

    /// A non-existent, non-repository path can't even spawn `git worktree
    /// list` — the [`worktree_census`]/[`git_stdout_capped`] half this
    /// module's unit tests above deliberately bypass (see the module note).
    /// Exercised at least this much end to end, through the real async
    /// entry point: `git`'s own exit failure becomes `CensusFailed`, never a
    /// silently empty `Observed`.
    #[tokio::test]
    async fn a_command_error_produces_census_failed_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let census = worktree_census(dir.path()).await;
        match census {
            WorktreeCensus::CensusFailed { .. } => {}
            WorktreeCensus::Observed { siblings } => {
                panic!("expected CensusFailed, got Observed({siblings:?})")
            }
        }
    }
}
