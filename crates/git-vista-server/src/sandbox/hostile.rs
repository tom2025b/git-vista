//! D2 (#66, Task 7): the hostile-geometry battery for
//! `sandbox::repo_paths`. Not to be confused with `escape_suite.rs` — that
//! battery proves the running *sandbox* (Landlock/seccomp/namespaces) holds
//! against a process already inside it; this one proves the *repository
//! metadata resolution that decides what to grant in the first place* holds
//! against a `.git` an attacker (a hostile hook, running with RW on the
//! repository) can rewrite before the next policy is built.
//!
//! Mirrors the rigor of `worktree.rs`'s own test module — every attack has a
//! scaffold that would, if the containment/managed-root checks were skipped
//! or weakened, resolve to a real, existing, wrong directory. A test that
//! only tries a dangling or nonexistent target proves nothing, because
//! `canonicalize()` already refuses those on its own; every attack fixture
//! here builds a real target so the refusal is provably about the rule, not
//! about a missing file.

use std::path::{Path, PathBuf};

use super::repo_paths::{resolve, resolve_and_validate, RepoPathsError};

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init failed at {}", dir.display());
}

/// A real linked-worktree layout, mirroring `worktree.rs`'s own `scaffold`
/// helper (kept independent rather than shared — this module tests through
/// the `repo_paths` seam, and duplicating a dozen lines of fixture setup is
/// cheaper than reaching into a sibling module's private test helpers).
fn linked_worktree_scaffold(base: &Path) -> (PathBuf, PathBuf) {
    let main = base.join("main-repo");
    let gitdir = main.join(".git/worktrees/linked");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    let linked = base.join("linked-worktree");
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )
    .unwrap();
    (main, linked)
}

// --- 1. a gitfile pointing outside the managed root --------------------

/// A *self-consistent* linked worktree — `worktree::linked_worktree_dirs`'s
/// own containment rule is satisfied, gitdir really is registered under
/// commondir's `worktrees/` — but the whole main repository sits outside the
/// managed root. This is exactly the gap `repo_paths.rs`'s module doc
/// describes: `worktree.rs` alone would grant it.
#[test]
fn a_self_consistent_linked_worktree_outside_the_managed_root_is_refused() {
    let managed = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (_main, linked) = linked_worktree_scaffold(elsewhere.path());

    // Sanity: the plain `worktree` containment rule alone has nothing to
    // object to here — the geometry really is a legitimate linked worktree.
    assert!(super::worktree::linked_worktree_dirs(&linked)
        .expect("geometry itself is valid")
        .is_some());

    let err = resolve_and_validate(&linked, managed.path()).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::OutsideManagedRoot { .. }),
        "got: {err}"
    );
}

/// The direct, non-worktree case: an ordinary repository whose `.git` is a
/// plain directory, entirely outside the managed root. No pointer chain
/// involved at all — the simplest instance of the threat.
#[test]
fn a_plain_directory_repo_outside_the_managed_root_is_refused() {
    let managed = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let repo = elsewhere.path().join("repo");
    init_repo(&repo);

    let err = resolve_and_validate(&repo, managed.path()).unwrap_err();
    assert!(matches!(err, RepoPathsError::OutsideManagedRoot { .. }));
}

/// A linked worktree whose gitdir/commondir *are* inside the managed root
/// must still resolve — this is the compose-not-duplicate property from the
/// module doc: passing the managed-root check does not depend on skipping
/// the worktree containment check, and a legitimate worktree must not
/// collect a spurious refusal from either gate.
#[test]
fn a_self_consistent_linked_worktree_inside_the_managed_root_resolves() {
    let managed = tempfile::tempdir().unwrap();
    let (main, linked) = linked_worktree_scaffold(managed.path());

    let paths = resolve_and_validate(&linked, managed.path()).expect("inside the root");
    assert_eq!(
        paths.gitdir,
        main.join(".git/worktrees/linked").canonicalize().unwrap()
    );
    assert_eq!(paths.commondir, main.join(".git").canonicalize().unwrap());
}

// --- 2. a gitfile pointing at a symlink escape --------------------------

/// `.git` itself is a symlink (never a valid gitfile shape — `worktree.rs`
/// refuses this outright) rather than a regular file containing a `gitdir:`
/// line. Proves `repo_paths::resolve` inherits that refusal through its
/// delegation to `worktree::linked_worktree_dirs`, rather than falling
/// through to its own directory-or-missing stat and treating a symlink as
/// "no `.git`".
#[test]
fn a_symlinked_dot_git_is_refused() {
    let base = tempfile::tempdir().unwrap();
    let target = base.path().join("real-repo");
    init_repo(&target);
    let sneaky = base.path().join("sneaky");
    std::fs::create_dir_all(&sneaky).unwrap();
    std::os::unix::fs::symlink(target.join(".git"), sneaky.join(".git")).unwrap();

    let err = resolve(&sneaky).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::WorktreeGeometry { .. }),
        "got: {err}"
    );
    assert!(err.to_string().contains("symlink"), "got: {err}");
}

/// The pointer *target* (not `.git` itself) is a symlink that resolves to a
/// real, existing worktree geometry elsewhere on disk — canonicalisation
/// must resolve through it before either the worktree containment check or
/// the managed-root check runs, so the symlink cannot be used to make a
/// path *look* like it is inside the managed root while really landing
/// outside it.
#[test]
fn a_gitdir_reached_through_a_symlinked_target_is_still_checked_against_the_real_path() {
    let managed = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (_main, linked) = linked_worktree_scaffold(elsewhere.path());

    // A symlink living *inside* the managed root, but pointing at the real
    // (outside-the-root) gitdir target the linked worktree's `.git` names.
    let real_gitdir = std::fs::read_to_string(linked.join(".git"))
        .unwrap()
        .trim()
        .strip_prefix("gitdir:")
        .unwrap()
        .trim()
        .to_string();
    let looks_inside = managed.path().join("looks-inside-worktrees-link");
    std::os::unix::fs::symlink(&real_gitdir, &looks_inside).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", looks_inside.display()),
    )
    .unwrap();

    let err = resolve_and_validate(&linked, managed.path()).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::OutsideManagedRoot { .. }),
        "canonicalisation must resolve through the symlink to the real (outside-the-root) \
         target before the containment check runs — got: {err}"
    );
}

// --- 3. a missing / malformed gitfile -----------------------------------

#[test]
fn a_missing_dot_git_is_refused_by_name() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("not-a-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let err = resolve(&repo).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::MissingGitFile { .. }),
        "got: {err}"
    );
}

#[test]
fn a_gitfile_with_no_gitdir_prefix_is_refused() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(".git"), "not a gitdir pointer at all\n").unwrap();
    let err = resolve(&repo).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::WorktreeGeometry { .. }),
        "got: {err}"
    );
}

#[test]
fn a_dangling_gitdir_pointer_is_refused_not_silently_ungranted() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: /nonexistent/worktrees/x\n").unwrap();
    let err = resolve(&repo).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::WorktreeGeometry { .. }),
        "got: {err}"
    );
}

/// A `.git` pointer chain with no `commondir` (a submodule or
/// `--separate-git-dir` gitdir) — `worktree.rs` refuses this by design
/// rather than guess at a grant; `repo_paths` must inherit that refusal
/// rather than fall back to treating it as a plain repository.
#[test]
fn a_pointer_without_a_commondir_is_refused() {
    let base = tempfile::tempdir().unwrap();
    let standalone = base.path().join("standalone-gitdir");
    std::fs::create_dir_all(&standalone).unwrap();
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join(".git"),
        format!("gitdir: {}\n", standalone.display()),
    )
    .unwrap();
    let err = resolve(&repo).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::WorktreeGeometry { .. }),
        "got: {err}"
    );
}

/// A gitdir whose `commondir` file names a real, existing but unrelated
/// directory (not a `worktrees/<id>` registration of it) — the exact attack
/// `worktree.rs`'s own containment-rule test proves refused; repeated here
/// to prove `repo_paths::resolve` still surfaces it after the extra
/// directory-or-missing fallback this module adds.
#[test]
fn a_gitdir_not_registered_under_its_claimed_commondir_is_refused() {
    let base = tempfile::tempdir().unwrap();
    let (_main, linked) = linked_worktree_scaffold(base.path());
    let evil = base.path().join("evil");
    std::fs::create_dir_all(&evil).unwrap();
    let victim = base.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(evil.join("commondir"), victim.display().to_string()).unwrap();
    std::fs::write(linked.join(".git"), format!("gitdir: {}\n", evil.display())).unwrap();

    let err = resolve(&linked).unwrap_err();
    assert!(
        matches!(err, RepoPathsError::WorktreeGeometry { .. }),
        "got: {err}"
    );
    assert!(
        err.to_string().contains("not strictly inside"),
        "got: {err}"
    );
}
