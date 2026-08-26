//! Tests for [`super::worktree_census`] and its porcelain parser (M11.01,
//! #546).
//!
//! Every test that runs real git goes through a fresh [`tempfile::tempdir`]
//! fixture (via `git_vista_fixtures`), never the process-global catalog/state
//! — see the module doc's "Why the allowed-roots check… are parameters"
//! section for why `path_is_allowed` always arrives here as a local closure
//! rather than `crate::state::path_is_allowed`.

use super::*;
use git_vista_fixtures::git as fx;
use git_vista_fixtures::{empty, seeded};

/// Always-allow fence — the common case, used by every test that isn't
/// specifically exercising the fence.
fn allow_all(_: &Path) -> bool {
    true
}

fn head_tip(repo: &Path) -> String {
    fx::out(repo, &["rev-parse", "HEAD"])
}

fn current_of<'a>(siblings: &'a [WorktreeSibling]) -> Vec<&'a WorktreeSibling> {
    siblings.iter().filter(|s| s.is_current).collect()
}

fn observed(census: WorktreeCensus) -> Vec<WorktreeSibling> {
    match census {
        WorktreeCensus::Observed { siblings } => siblings,
        WorktreeCensus::CensusFailed { reason } => {
            panic!("expected Observed, got CensusFailed({reason})")
        }
    }
}

fn failed(census: WorktreeCensus) -> String {
    match census {
        WorktreeCensus::Observed { siblings } => {
            panic!(
                "expected CensusFailed, got Observed({} siblings)",
                siblings.len()
            )
        }
        WorktreeCensus::CensusFailed { reason } => reason,
    }
}

// ---------------------------------------------------------------------------
// Acceptance: the typed census exists and is produced from a real repo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repo_with_no_linked_worktrees_reports_itself_as_the_only_sibling() {
    let (_dir, repo) = seeded();
    let tip = head_tip(&repo);

    let siblings = observed(worktree_census(&repo, false, &allow_all).await);

    assert_eq!(siblings.len(), 1);
    let s = &siblings[0];
    assert!(s.is_current);
    assert_eq!(s.branch.as_ref().map(|b| b.as_str()), Some("main"));
    assert_eq!(s.head.as_ref().map(|h| h.as_str()), Some(tip.as_str()));
    assert!(!s.locked);
    assert!(!s.prunable);
    assert!(!s.bare);
    assert_eq!(s.serviceable, Serviceable::Yes);
    // Acceptance criterion 5, taken literally: exactly one, not merely any.
    assert_eq!(current_of(&siblings).len(), 1);
}

#[tokio::test]
async fn a_linked_worktree_gets_its_own_id_but_shares_the_repository_id() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&repo, &["branch", "feature"]);
    fx::run(
        &repo,
        &["worktree", "add", linked.to_str().unwrap(), "feature"],
    );

    let siblings = observed(worktree_census(&repo, false, &allow_all).await);

    assert_eq!(siblings.len(), 2);
    assert_eq!(current_of(&siblings).len(), 1);

    let main = siblings.iter().find(|s| s.is_current).unwrap();
    let side_sibling = siblings.iter().find(|s| !s.is_current).unwrap();

    assert_ne!(main.id, side_sibling.id, "distinct worktrees, distinct ids");
    assert_eq!(
        main.repository, side_sibling.repository,
        "one repository shared by every worktree"
    );
    assert_eq!(
        side_sibling.branch.as_ref().map(|b| b.as_str()),
        Some("feature")
    );
    assert!(!side_sibling.bare);
    assert_eq!(side_sibling.serviceable, Serviceable::Yes);
}

/// Acceptance criterion 2: `locked` is read straight from git, and does not
/// leak into `serviceable` — the app's fence has nothing to do with git's own
/// lock.
#[tokio::test]
async fn locked_reads_true_from_gits_own_flag_and_leaves_serviceable_alone() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&repo, &["branch", "feature"]);
    fx::run(
        &repo,
        &["worktree", "add", linked.to_str().unwrap(), "feature"],
    );
    fx::run(
        &repo,
        &[
            "worktree",
            "lock",
            linked.to_str().unwrap(),
            "--reason",
            "testing",
        ],
    );

    let siblings = observed(worktree_census(&repo, false, &allow_all).await);
    let side_sibling = siblings.iter().find(|s| !s.is_current).unwrap();

    assert!(side_sibling.locked, "git's own lock flag must read true");
    assert!(!side_sibling.prunable);
    assert_eq!(
        side_sibling.serviceable,
        Serviceable::Yes,
        "locked is git's fact; it must not be folded into the app's fence"
    );
}

/// Acceptance criterion 3, mutation-proven (see the PR body): a sibling
/// outside the allowed roots is listed — never dropped — and refused with
/// `Serviceable::OutsideAllowedRoots`, not silently treated as usable.
///
/// Two assertions, on purpose, so the two required mutations fail at
/// different places:
///   * `assert_eq!(siblings.len(), 2, …)` is what "drop the
///     `OutsideAllowedRoots` arm so a refused sibling vanishes from the list"
///     (the brief's own wording) would break.
///   * `assert_eq!(side_sibling.serviceable, Serviceable::OutsideAllowedRoots)`
///     is what silently widening the fence (folding refused into `Yes`)
///     would break, with the row still present.
#[tokio::test]
async fn outside_allowed_roots_sibling_is_listed_and_refused_never_dropped() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&repo, &["branch", "feature"]);
    fx::run(
        &repo,
        &["worktree", "add", linked.to_str().unwrap(), "feature"],
    );

    // A fence that admits only the main repo's own root — the linked sibling,
    // wherever it landed, is outside it. `canonicalize` matches what
    // `resolve_sibling` feeds the fence with.
    let allowed_root = std::fs::canonicalize(&repo).unwrap();
    let fence = move |candidate: &Path| candidate.starts_with(&allowed_root);

    let siblings = observed(worktree_census(&repo, false, &fence).await);

    assert_eq!(
        siblings.len(),
        2,
        "the refused sibling must still be present in the list"
    );
    let side_sibling = siblings.iter().find(|s| !s.is_current).unwrap();
    assert_eq!(
        side_sibling.serviceable,
        Serviceable::OutsideAllowedRoots,
        "a refused sibling must carry its own reason, not `Yes`"
    );
    // And the main worktree, which the fence does admit, is unaffected.
    let main = siblings.iter().find(|s| s.is_current).unwrap();
    assert_eq!(main.serviceable, Serviceable::Yes);
}

/// Acceptance criterion 4: a `prunable` sibling whose directory is gone reads
/// `Missing`, resolved through the surviving admin directory (spec §1,
/// option 1) rather than being dropped or misreported as refused-by-policy.
#[tokio::test]
async fn prunable_with_a_gone_directory_reads_missing_and_keeps_a_stable_id() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&repo, &["branch", "feature"]);
    fx::run(
        &repo,
        &["worktree", "add", linked.to_str().unwrap(), "feature"],
    );
    std::fs::remove_dir_all(&linked).unwrap();

    let siblings = observed(worktree_census(&repo, false, &allow_all).await);
    assert_eq!(siblings.len(), 2, "a gone worktree must still be listed");

    let missing = siblings.iter().find(|s| !s.is_current).unwrap();
    assert!(missing.prunable, "git's own flag must still read true");
    assert_eq!(
        missing.serviceable,
        Serviceable::Missing,
        "gone is a different sentence than refused-by-policy"
    );
    assert_eq!(
        missing.repository,
        siblings.iter().find(|s| s.is_current).unwrap().repository
    );

    // The id is derived, not fabricated, and stable across a second read.
    let siblings_again = observed(worktree_census(&repo, false, &allow_all).await);
    let missing_again = siblings_again.iter().find(|s| !s.is_current).unwrap();
    assert_eq!(
        missing.id, missing_again.id,
        "the derived id must be stable"
    );
}

/// A fresh, commit-less repository's own (main) worktree has an unborn
/// branch: git reports `HEAD 000…0`, its null-oid sentinel, which must not be
/// passed through as though it named a real commit (`history::HeadState`'s
/// own precedent for the same fact about the *current* worktree's HEAD).
#[tokio::test]
async fn an_unborn_branch_reports_no_head_not_the_null_oid() {
    let (_dir, repo) = empty();

    let siblings = observed(worktree_census(&repo, false, &allow_all).await);

    assert_eq!(siblings.len(), 1);
    let s = &siblings[0];
    assert!(s.is_current);
    assert_eq!(s.branch.as_ref().map(|b| b.as_str()), Some("main"));
    assert_eq!(
        s.head, None,
        "an unborn HEAD must read as None, never the null oid"
    );
}

/// `bare` is git's own third boolean (see the protocol module's doc): a
/// bare-hub layout's admin directory shows up as its own porcelain record,
/// with no branch and no HEAD, and must be reported as `bare: true` rather
/// than dropped or misread as an ordinary detached worktree.
#[tokio::test]
async fn a_bare_hub_admin_entry_is_reported_as_bare_with_no_branch_or_head() {
    let outer = tempfile::tempdir().unwrap();
    let hub = outer.path().join("hub.git");
    fx::run(
        outer.path(),
        &["init", "-q", "--bare", hub.to_str().unwrap()],
    );
    // A bare repo has no working tree to commit from; give the hub a real
    // branch to check out by pushing one in from a normal clone.
    let (_seed_dir, seed_repo) = seeded();
    fx::run(&seed_repo, &["remote", "add", "hub", hub.to_str().unwrap()]);
    fx::run(&seed_repo, &["push", "-q", "hub", "main"]);

    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&hub, &["worktree", "add", linked.to_str().unwrap(), "main"]);

    let siblings = observed(worktree_census(&linked, false, &allow_all).await);

    assert_eq!(siblings.len(), 2);
    let bare_row = siblings.iter().find(|s| s.bare).expect("a bare row");
    assert_eq!(bare_row.branch, None);
    assert_eq!(bare_row.head, None);
    assert!(!bare_row.is_current);

    let current = siblings.iter().find(|s| s.is_current).unwrap();
    assert!(!current.bare);
    assert_eq!(current.branch.as_ref().map(|b| b.as_str()), Some("main"));
}

#[tokio::test]
async fn a_path_that_is_not_a_git_repository_fails_the_census() {
    let dir = tempfile::tempdir().unwrap();
    let reason = failed(worktree_census(dir.path(), false, &allow_all).await);
    assert!(
        reason.contains("identity"),
        "expected a message about the repo's own identity, got: {reason}"
    );
}

#[tokio::test]
async fn expose_paths_gates_the_path_field() {
    let (_dir, repo) = seeded();

    let hidden = observed(worktree_census(&repo, false, &allow_all).await);
    assert_eq!(hidden[0].path, None);

    let shown = observed(worktree_census(&repo, true, &allow_all).await);
    assert!(shown[0].path.is_some());
}

// ---------------------------------------------------------------------------
// Parser unit tests
// ---------------------------------------------------------------------------

#[test]
fn is_null_oid_matches_exactly_40_or_64_zeros() {
    assert!(is_null_oid(&"0".repeat(40)));
    assert!(is_null_oid(&"0".repeat(64)));
    assert!(!is_null_oid(&"0".repeat(39)));
    assert!(!is_null_oid(&format!("{}1", "0".repeat(39))));
    assert!(!is_null_oid(&"a".repeat(40)));
}

#[test]
fn parses_a_well_formed_multi_record_stream() {
    let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\n\nworktree /tmp/side\nHEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nbranch refs/heads/feature\nlocked reason with spaces\nprunable\n\n";
    let records = parse_worktree_porcelain(text).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].path, PathBuf::from("/tmp/main"));
    assert_eq!(records[0].branch_ref.as_deref(), Some("refs/heads/main"));
    assert!(!records[0].locked);
    assert!(records[1].locked);
    assert!(records[1].prunable);
}

#[test]
fn tolerates_a_missing_trailing_blank_line() {
    let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ndetached";
    let records = parse_worktree_porcelain(text).unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].detached);
}

#[test]
fn a_bare_record_has_no_head_or_branch() {
    let text = "worktree /tmp/hub.git\nbare\n";
    let records = parse_worktree_porcelain(text).unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].bare);
    assert_eq!(records[0].head_hex, None);
}

#[test]
fn an_attribute_before_any_worktree_line_is_an_error() {
    let text = "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn a_second_worktree_line_without_a_blank_terminator_is_an_error() {
    let text = "worktree /tmp/main\nworktree /tmp/side\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn an_unrecognized_attribute_is_an_error_not_a_skip() {
    let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\nsomething-new value\n";
    let err = parse_worktree_porcelain(text).unwrap_err();
    assert!(err.contains("something-new"));
}

#[test]
fn branch_and_detached_together_is_an_error() {
    let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\ndetached\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn a_non_bare_record_missing_head_is_an_error() {
    let text = "worktree /tmp/main\nbranch refs/heads/main\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn a_record_naming_neither_branch_nor_detached_is_an_error() {
    let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn a_bare_record_carrying_head_is_an_error() {
    let text = "worktree /tmp/hub.git\nbare\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
    assert!(parse_worktree_porcelain(text).is_err());
}

#[test]
fn empty_input_parses_to_no_records() {
    // The caller (`worktree_census`) is what turns zero records into a
    // `CensusFailed` — the parser itself just reports what it saw.
    assert_eq!(parse_worktree_porcelain("").unwrap().len(), 0);
}
