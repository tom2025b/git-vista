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

fn current_of(siblings: &[WorktreeSibling]) -> Vec<&WorktreeSibling> {
    siblings.iter().filter(|s| s.is_current).collect()
}

/// `git worktree add <side>/<name>` on a fresh branch of the same name, and
/// the id `gix` gives that worktree **while it still exists** — the
/// independent witness the correlation tests below anchor on (the same trick
/// `a_missing_siblings_id_survives_its_own_deletion` uses, hoisted so more
/// than one worktree can be witnessed per fixture).
fn add_linked(repo: &Path, side: &Path, name: &str) -> (PathBuf, String) {
    let path = side.join(name);
    fx::run(repo, &["branch", name]);
    fx::run(repo, &["worktree", "add", path.to_str().unwrap(), name]);
    let id = git_vista_git::read_handle(&path)
        .unwrap()
        .worktree
        .to_string();
    (path, id)
}

/// The `<repo>/.git/worktrees/<name>` administrative directory git creates for
/// a linked worktree — the directory whose `gitdir` file
/// `correlate_missing_admin_dir` reads, and the one that survives deletion of
/// the working tree.
fn admin_dir(repo: &Path, name: &str) -> PathBuf {
    let dir = repo.join(".git").join("worktrees").join(name);
    assert!(
        dir.is_dir(),
        "fixture is wrong: no admin directory at {}",
        dir.display()
    );
    dir
}

fn observed(census: WorktreeCensus) -> Vec<WorktreeSibling> {
    match census {
        WorktreeCensus::Observed { siblings } => siblings,
        WorktreeCensus::CensusFailed { reason, .. } => {
            panic!("expected Observed, got CensusFailed({reason})")
        }
    }
}

/// The client-safe half of a failed census — what every route serializes.
fn failed(census: WorktreeCensus) -> String {
    failed_pair(census).0
}

/// Both halves, for the tests that are about the split itself (#657).
fn failed_pair(census: WorktreeCensus) -> (String, Option<String>) {
    match census {
        WorktreeCensus::Observed { siblings } => {
            panic!(
                "expected CensusFailed, got Observed({} siblings)",
                siblings.len()
            )
        }
        WorktreeCensus::CensusFailed { reason, detail } => (reason, detail),
    }
}

// ---------------------------------------------------------------------------
// Acceptance: the typed census exists and is produced from a real repo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repo_with_no_linked_worktrees_reports_itself_as_the_only_sibling() {
    let (_dir, repo) = seeded();
    let tip = head_tip(&repo);

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);

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

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);

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

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
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

    let siblings = observed(worktree_census(&repo, CensusPaths::from_flag(false), &fence).await);

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

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
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
    let siblings_again =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    let missing_again = siblings_again.iter().find(|s| !s.is_current).unwrap();
    assert_eq!(
        missing.id, missing_again.id,
        "the derived id must be stable"
    );
}

/// The id a `Serviceable::Missing` row carries is the **right** id, checked
/// against a value derived a completely different way.
///
/// `prunable_with_a_gone_directory_reads_missing_and_keeps_a_stable_id` above
/// only proves two consecutive censuses agree with each other — two reads of
/// one derivation, which would stay green if that derivation were wrong. This
/// one anchors on an independent witness: `git_vista_git::read_handle` opens
/// the worktree with `gix` *while it still exists* and hashes the git dir gix
/// itself resolved, whereas the census (after the deletion, with gix unable to
/// open anything) reaches the same directory through
/// `correlate_missing_admin_dir` — reading every `<common>/worktrees/*/gitdir`
/// file and matching its recorded path against the porcelain's. Two unrelated
/// routes to the same admin directory; if the correlation picked the wrong
/// entry, guessed a name, or silently fell back to the served worktree's own
/// id, this fails and the stability test would not.
#[tokio::test]
async fn a_missing_siblings_id_survives_its_own_deletion() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let linked = side.path().join("linked");
    fx::run(&repo, &["branch", "feature"]);
    fx::run(
        &repo,
        &["worktree", "add", linked.to_str().unwrap(), "feature"],
    );

    // The independent witness, captured while the directory is still there.
    let id_before_deletion = git_vista_git::read_handle(&linked).unwrap().worktree;
    // And a negative control: it must not merely equal the served worktree's
    // own id, which is what a lazy fallback would produce.
    let current_id = git_vista_git::read_handle(&repo).unwrap().worktree;
    assert_ne!(
        id_before_deletion, current_id,
        "fixture is wrong: a linked worktree must not share the main one's id"
    );

    std::fs::remove_dir_all(&linked).unwrap();

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    let missing = siblings.iter().find(|s| !s.is_current).unwrap();
    assert_eq!(missing.serviceable, Serviceable::Missing);
    assert_eq!(
        missing.id,
        id_before_deletion.to_string(),
        "a Missing row must carry the id the worktree had while it existed"
    );
}

/// The correlation in `correlate_missing_admin_dir` is by **`gitdir` content**,
/// and its doc comment says so in as many words ("exact, not a naming guess").
/// Every fixture above has exactly one linked worktree, so "read each admin
/// entry's `gitdir` and match it against the porcelain path" and "take the
/// first admin entry `read_dir` hands back" are indistinguishable in all of
/// them — the claim is defended by the fixture's arity, not by a test.
///
/// Here there are **two** linked worktrees and only one is deleted, so the
/// admin root holds two entries while exactly one row is `Missing`. The
/// `Missing` row must carry the *deleted* worktree's id — captured by `gix`
/// before the deletion, an independent witness — and must not carry the
/// survivor's.
///
/// Deterministic-failure note, kept honest: `read_dir` order is the
/// filesystem's business, so a first-entry-wins mutation would still guess
/// right here roughly half the time.
/// `two_missing_rows_each_keep_their_own_admin_entrys_id` below is the leg
/// that cannot be got right by luck — read the two together.
#[tokio::test]
async fn a_missing_rows_id_comes_from_its_own_gitdir_not_a_surviving_siblings() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (gone, gone_id) = add_linked(&repo, side.path(), "gone");
    let (_survivor, survivor_id) = add_linked(&repo, side.path(), "survivor");
    assert_ne!(
        gone_id, survivor_id,
        "fixture is wrong: two linked worktrees must have distinct ids"
    );

    std::fs::remove_dir_all(&gone).unwrap();

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    assert_eq!(siblings.len(), 3, "main, the survivor, and the gone one");

    let missing: Vec<&WorktreeSibling> = siblings
        .iter()
        .filter(|s| s.serviceable == Serviceable::Missing)
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "only the deleted worktree is Missing; the survivor is still there"
    );
    let missing = missing[0];
    assert_eq!(missing.name, "gone");
    assert_eq!(
        missing.id, gone_id,
        "the Missing row must carry the id of the worktree that vanished"
    );
    assert_ne!(
        missing.id, survivor_id,
        "and must never be handed the surviving sibling's admin entry instead"
    );

    // The survivor is untouched: still live, still its own id.
    let live = siblings
        .iter()
        .find(|s| s.name == "survivor")
        .expect("the surviving linked worktree");
    assert_eq!(live.serviceable, Serviceable::Yes);
    assert_eq!(live.id, survivor_id);
}

/// The same claim, in the arrangement no `read_dir` ordering can pass by luck:
/// **two** linked worktrees, **both** deleted, so the census resolves two
/// `Missing` rows against two surviving admin entries in one call.
///
/// "Take the first admin entry" hands the *same* directory to both rows, so
/// the two ids come out equal — and at most one of them could be right. Only
/// a per-row `gitdir` match can give each row the id its own worktree had
/// while it existed, which is what the two `gix` witnesses captured before the
/// deletions pin.
#[tokio::test]
async fn two_missing_rows_each_keep_their_own_admin_entrys_id() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (alpha, alpha_id) = add_linked(&repo, side.path(), "alpha");
    let (beta, beta_id) = add_linked(&repo, side.path(), "beta");
    assert_ne!(alpha_id, beta_id, "fixture is wrong: ids must differ");

    std::fs::remove_dir_all(&alpha).unwrap();
    std::fs::remove_dir_all(&beta).unwrap();

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    let missing: Vec<&WorktreeSibling> = siblings
        .iter()
        .filter(|s| s.serviceable == Serviceable::Missing)
        .collect();
    assert_eq!(missing.len(), 2, "both deleted worktrees are Missing rows");

    let alpha_row = missing
        .iter()
        .find(|s| s.name == "alpha")
        .expect("a row for alpha");
    let beta_row = missing
        .iter()
        .find(|s| s.name == "beta")
        .expect("a row for beta");

    assert_ne!(
        alpha_row.id, beta_row.id,
        "two Missing rows sharing one id means the correlation stopped \
         correlating and started taking whatever `read_dir` returned first"
    );
    assert_eq!(
        alpha_row.id, alpha_id,
        "alpha's row must carry alpha's own pre-deletion id"
    );
    assert_eq!(
        beta_row.id, beta_id,
        "beta's row must carry beta's own pre-deletion id"
    );
}

/// A fresh, commit-less repository's own (main) worktree has an unborn
/// branch: git reports `HEAD 000…0`, its null-oid sentinel, which must not be
/// passed through as though it named a real commit (`history::HeadState`'s
/// own precedent for the same fact about the *current* worktree's HEAD).
#[tokio::test]
async fn an_unborn_branch_reports_no_head_not_the_null_oid() {
    let (_dir, repo) = empty();

    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);

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

    let siblings =
        observed(worktree_census(&linked, CensusPaths::from_flag(false), &allow_all).await);

    assert_eq!(siblings.len(), 2);
    let bare_row = siblings.iter().find(|s| s.bare).expect("a bare row");
    assert_eq!(bare_row.branch, None);
    assert_eq!(bare_row.head, None);
    assert!(!bare_row.is_current);

    let current = siblings.iter().find(|s| s.is_current).unwrap();
    assert!(!current.bare);
    assert_eq!(current.branch.as_ref().map(|b| b.as_str()), Some("main"));
}

/// A truncated `git worktree list --porcelain` is **refused**, never parsed —
/// the same posture `handlers::read::worktree_status_v2_for_repo` takes on a
/// `STATUS_V2_STDOUT_CAP` hit, and the reason the read went through
/// `git_stdout_capped` instead of the uncapped `git_output` at all.
///
/// The cap is passed in (production uses `WORKTREE_LIST_STDOUT_CAP`, 8 MiB) so
/// this can hit it with a real repository instead of fabricating megabytes of
/// porcelain. One byte is below the shortest possible record, so git's own
/// output is guaranteed to exceed it.
///
/// The paired positive matters as much as the refusal: the *same* repository,
/// read with a cap that is not hit, is a healthy `Observed`. Without that leg
/// this test would still pass if the census had simply become unable to read
/// anything at all.
#[tokio::test]
async fn a_truncated_worktree_list_is_refused_not_parsed_into_a_short_census() {
    let (_dir, repo) = seeded();

    let reason =
        failed(worktree_census_capped(&repo, CensusPaths::from_flag(false), &allow_all, 1).await);
    assert!(
        reason.contains("truncated"),
        "a cap hit must say so, not masquerade as some other failure: {reason}"
    );

    let siblings = observed(
        worktree_census_capped(&repo, CensusPaths::from_flag(false), &allow_all, 8 * 1024).await,
    );
    assert_eq!(
        siblings.len(),
        1,
        "the same repository under an unhit cap must still census normally"
    );
}

/// The **production** ceiling, exercised through the entry point that owns it.
///
/// `a_truncated_worktree_list_is_refused_not_parsed_into_a_short_census` above
/// supplies its own caps (1, then 8 KiB) to `worktree_census_capped`, so it
/// never observes `WORKTREE_LIST_STDOUT_CAP` at all: raising that constant to
/// `usize::MAX` — restoring precisely the unbounded read this module went
/// through `git_stdout_capped` to remove — leaves every other test in this
/// file green. This one calls `worktree_census`, which takes no cap, and makes
/// real git print more than the constant allows.
///
/// Doing that cheaply uses git's own porcelain: a locked worktree's **reason**
/// is printed verbatim on the record's `locked <reason>` line (verified by
/// hand against git 2.53 — a 100-byte reason printed 100 bytes, and a 9 MiB
/// one printed a 9,437,563-byte stream in 10 ms). The reason is stored in
/// `<common>/worktrees/<name>/locked`, which is the file
/// `git worktree lock --reason` writes, so growing that one file past the
/// ceiling produces a genuinely oversized stream **from real git**. The
/// alternative — enough linked worktrees to total 8 MiB of records — is
/// tens of thousands of checkouts, which is why the cap went untested here in
/// the first place.
///
/// Two legs, and both are load-bearing:
///   * the ceiling must be a finite, reachable byte count. A `usize::MAX`
///     "cap" is not a cap, and the test refuses to pretend it can allocate a
///     stream that exceeds it.
///   * the same repository, with the oversized reason removed, censuses
///     normally — which is what proves the refusal came from the size rather
///     than from a fixture git could not read at all.
#[tokio::test]
async fn the_production_census_refuses_a_stream_larger_than_its_own_ceiling() {
    let Some(over) = WORKTREE_LIST_STDOUT_CAP
        .checked_add(1)
        .filter(|n| *n <= 64 * 1024 * 1024)
    else {
        panic!(
            "WORKTREE_LIST_STDOUT_CAP is {WORKTREE_LIST_STDOUT_CAP} bytes — a \
             ceiling no `git worktree list --porcelain` stream could ever reach \
             is not a ceiling, and `worktree_census` would be reading git's \
             stdout unbounded"
        );
    };

    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (linked, _linked_id) = add_linked(&repo, side.path(), "linked");
    fx::run(
        &repo,
        &[
            "worktree",
            "lock",
            linked.to_str().unwrap(),
            "--reason",
            "placeholder",
        ],
    );

    // git wrote the lock reason here; grow it past the census's own ceiling.
    // Not via `--reason` itself: an argument that long is rejected before git
    // ever runs (measured — `execve` of `git worktree lock --reason <9 MiB>`
    // fails `E2BIG`, "Argument list too long"), so the reason is written to
    // git's own file, in git's own format, and git reads it back.
    let lock_file = admin_dir(&repo, "linked").join("locked");
    assert!(
        lock_file.is_file(),
        "`git worktree lock --reason` must have written {}",
        lock_file.display()
    );
    std::fs::write(&lock_file, "x".repeat(over)).unwrap();

    let reason = failed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        reason.contains("truncated"),
        "the production ceiling must refuse an oversized stream, and say why: {reason}"
    );

    // The paired positive: same repository, reason gone, healthy census.
    std::fs::remove_file(&lock_file).unwrap();
    let siblings =
        observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    assert_eq!(
        siblings.len(),
        2,
        "the same repository under the same production cap must census normally \
         once the oversized reason is gone"
    );
}

/// The exactly-one-current guard (`current_count != 1`), reached rather than
/// merely defended by construction — issue #546's acceptance asks for a test
/// that asserts *exactly* one, not "at least one", and a refusal branch no
/// test can enter is a branch nobody knows works.
///
/// No porcelain is fabricated here: the stream is real git's, produced from a
/// repository whose administrative bookkeeping has been corrupted the way a
/// half-finished `git worktree move`, a hand-edited `gitdir`, or a restored
/// backup can corrupt it. `<common>/worktrees/alpha/gitdir` is repointed at
/// **beta**'s working tree, and git then prints beta's path twice (verified by
/// hand against git 2.53: three records, two of them naming beta, neither
/// flagged `prunable`). Censused from beta, both of those records resolve
/// through `read_repo_facts` to beta's own worktree id, so two rows claim to
/// be the served worktree.
///
/// The healthy control runs first, on the same fixture before the corruption,
/// so a failure here cannot be the fixture simply refusing to census.
#[tokio::test]
async fn two_rows_resolving_to_the_served_worktree_is_refused_not_reported() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (_alpha, _alpha_id) = add_linked(&repo, side.path(), "alpha");
    let (beta, _beta_id) = add_linked(&repo, side.path(), "beta");

    // Control: censused from beta, this repository is healthy and has exactly
    // one current row.
    let healthy = observed(worktree_census(&beta, CensusPaths::from_flag(false), &allow_all).await);
    assert_eq!(healthy.len(), 3);
    assert_eq!(current_of(&healthy).len(), 1);

    // Corrupt alpha's admin entry so it names beta's working tree. git reads
    // this file to decide what path to print for that record, so porcelain now
    // lists beta twice.
    std::fs::write(
        admin_dir(&repo, "alpha").join("gitdir"),
        format!("{}/.git\n", beta.display()),
    )
    .unwrap();

    let reason = failed(worktree_census(&beta, CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        reason.contains("resolved 2 entries"),
        "the guard must name how many rows claimed to be current: {reason}"
    );
    assert!(
        reason.contains("exactly one is required"),
        "and must say that exactly one — not at least one — is the rule: {reason}"
    );
}

#[tokio::test]
async fn a_path_that_is_not_a_git_repository_fails_the_census() {
    let dir = tempfile::tempdir().unwrap();
    let reason =
        failed(worktree_census(dir.path(), CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        reason.contains("identity"),
        "expected a message about the repo's own identity, got: {reason}"
    );
}

#[tokio::test]
async fn expose_paths_gates_the_path_field() {
    let (_dir, repo) = seeded();

    let hidden = observed(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    assert_eq!(hidden[0].path, None);

    let shown = observed(worktree_census(&repo, CensusPaths::from_flag(true), &allow_all).await);
    assert!(shown[0].path.is_some());
}

// ---------------------------------------------------------------------------
// #657: the flag holds on the FAILURE arm too
//
// Every test below drives a real failure inside a `tempfile::tempdir`, whose
// path is absolute and unpredictable — so "does this string name a path" is
// asserted against the actual directory this run used, not against a pattern
// that could match by accident.
// ---------------------------------------------------------------------------

/// The census's own identity read fails (a directory that is not a git
/// repository), and `gix`'s error names the directory.
///
/// This is the shape the whole issue is about: with the flag off, the string
/// a client receives must not contain the path — and the operator who opts in
/// must still be able to see it, or redaction has cost the diagnosability
/// `CensusFailed` exists to provide.
#[tokio::test]
async fn a_failure_reason_withholds_the_path_and_the_flag_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let here = dir.path().to_string_lossy().into_owned();

    let (reason, detail) =
        failed_pair(worktree_census(dir.path(), CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        !reason.contains(&here),
        "the client-safe half named the absolute path `{here}`: {reason}"
    );
    assert!(
        reason.contains("identity"),
        "and it must still say what failed: {reason}"
    );
    assert_eq!(
        detail, None,
        "the path-bearing half must be withheld entirely when the operator did \
         not opt in, not merely scrubbed"
    );

    let (opted_reason, opted_detail) =
        failed_pair(worktree_census(dir.path(), CensusPaths::from_flag(true), &allow_all).await);
    assert_eq!(
        opted_reason, reason,
        "the flag adds `detail`; it must never rewrite `reason`"
    );
    let opted_detail = opted_detail.expect("an opted-in operator gets the detail");
    assert!(
        opted_detail.contains(&here),
        "the detail is the whole point of opting in, and must carry the path: \
         {opted_detail}"
    );
}

/// The route that takes its census with row paths on for its own local use
/// (`handlers::select`, via [`CensusPaths::rows_for_local_use`]) must **not**
/// thereby publish the failure detail: that decision belongs to the operator's
/// flag alone.
///
/// This is the exact conflation #657 found — one boolean answering two
/// questions — so it gets its own test rather than being implied by the
/// constructor's name.
#[tokio::test]
async fn a_local_use_census_still_withholds_the_failure_detail() {
    let dir = tempfile::tempdir().unwrap();
    let here = dir.path().to_string_lossy().into_owned();

    let (reason, detail) = failed_pair(
        worktree_census(
            dir.path(),
            CensusPaths::rows_for_local_use(false),
            &allow_all,
        )
        .await,
    );
    assert_eq!(
        detail, None,
        "rows-for-local-use must not carry the failure detail to a client"
    );
    assert!(!reason.contains(&here), "{reason}");

    // Paired positive, so this cannot pass by the census having become unable
    // to produce a detail at all: the same constructor, operator opted in.
    let (_, detail) = failed_pair(
        worktree_census(
            dir.path(),
            CensusPaths::rows_for_local_use(true),
            &allow_all,
        )
        .await,
    );
    assert!(
        detail.is_some_and(|d| d.contains(&here)),
        "with the flag on, the same call must still produce the detail"
    );
}

/// The `current_count != 1` guard, whose reason used to end with
/// `(repository root: /abs/path)`. Its arithmetic stays client-safe; its path
/// moves.
///
/// The fixture is `two_rows_resolving_to_the_served_worktree_is_refused_not_reported`'s
/// — alpha's admin `gitdir` repointed at beta, so git prints beta twice.
#[tokio::test]
async fn the_exactly_one_current_guard_keeps_its_count_and_moves_its_path() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (_alpha, _alpha_id) = add_linked(&repo, side.path(), "alpha");
    let (beta, _beta_id) = add_linked(&repo, side.path(), "beta");
    std::fs::write(
        admin_dir(&repo, "alpha").join("gitdir"),
        format!("{}/.git\n", beta.display()),
    )
    .unwrap();
    let beta_path = beta.to_string_lossy().into_owned();

    let (reason, detail) =
        failed_pair(worktree_census(&beta, CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        reason.contains("resolved 2 entries") && reason.contains("exactly one is required"),
        "the count is this module's own arithmetic and stays: {reason}"
    );
    assert!(
        !reason.contains(&beta_path),
        "the repository root must not ride along in the client-safe half: {reason}"
    );
    assert_eq!(detail, None);

    let (_, detail) =
        failed_pair(worktree_census(&beta, CensusPaths::from_flag(true), &allow_all).await);
    assert!(
        detail.is_some_and(|d| d.contains("repository root:")),
        "the opted-in operator still gets the root that made the guard fire"
    );
}

/// A live sibling git lists but nothing can open: the row's failure names the
/// worktree by the same base name the success arm would have put in
/// `WorktreeSibling::name`, and its absolute path only in the detail.
///
/// Built by corrupting a linked worktree's `.git` pointer file rather than by
/// deleting the directory: git decides `prunable` by whether the path its
/// admin `gitdir` names still **exists**, so a `.git` file that is present but
/// unreadable keeps the row non-`prunable` (verified — deleting the directory
/// instead lands on the `prunable` arm and censuses fine as
/// `Serviceable::Missing`) while `read_repo_facts` fails on it. That is the
/// live-but-unreadable arm, and nothing else reaches it.
#[tokio::test]
async fn a_live_but_unreadable_sibling_is_named_by_base_name_not_by_path() {
    let (_dir, repo) = seeded();
    let side = tempfile::tempdir().unwrap();
    let (linked, _id) = add_linked(&repo, side.path(), "desk-two");

    std::fs::write(linked.join(".git"), "gitdir: /nonexistent/not/a/git/dir\n").unwrap();
    let linked_path = linked.to_string_lossy().into_owned();

    let (reason, detail) =
        failed_pair(worktree_census(&repo, CensusPaths::from_flag(false), &allow_all).await);
    assert!(
        reason.contains("desk-two"),
        "the base name is the non-path label the success arm already exposes, \
         and dropping it would make the refusal useless: {reason}"
    );
    assert!(
        !reason.contains(&linked_path),
        "but the absolute path must not be there: {reason}"
    );
    assert_eq!(detail, None);

    let (_, detail) =
        failed_pair(worktree_census(&repo, CensusPaths::from_flag(true), &allow_all).await);
    assert!(
        detail.is_some_and(|d| d.contains(&linked_path)),
        "the opted-in operator gets the path that could not be read"
    );
}

/// `safe_label` never degrades into the path itself — [`display_name`]'s
/// fallback for a path with no final component *is* the whole path, which is
/// precisely what must not reach a `reason`.
#[test]
fn safe_label_falls_back_to_a_placeholder_not_to_the_path() {
    assert_eq!(safe_label(Path::new("/home/someone/desk-two")), "desk-two");
    assert_eq!(safe_label(Path::new("/")), "an unnamed worktree");
    assert_eq!(display_name(Path::new("/")), "/");
}

// ---------------------------------------------------------------------------
// Enrichment unit tests
//
// The porcelain parser itself — `parse_worktree_porcelain`, its record type
// and its **eleven** unit tests — moved to `git-vista-protocol`'s `worktree`
// module, beside `status.rs`/`diff.rs`'s parsers and the DTOs it produces.
// Eleven, counted rather than remembered: `git show HEAD~1:` on this file
// lists `parses_a_well_formed_multi_record_stream`,
// `tolerates_a_missing_trailing_blank_line`,
// `a_bare_record_has_no_head_or_branch`,
// `an_attribute_before_any_worktree_line_is_an_error`,
// `a_second_worktree_line_without_a_blank_terminator_is_an_error`,
// `an_unrecognized_attribute_is_an_error_not_a_skip`,
// `branch_and_detached_together_is_an_error`,
// `a_non_bare_record_missing_head_is_an_error`,
// `a_record_naming_neither_branch_nor_detached_is_an_error`,
// `a_bare_record_carrying_head_is_an_error` and
// `empty_input_parses_to_no_records`, and every one of them is now in
// `git-vista-protocol/src/worktree.rs`'s own `mod tests`.
//
// What is left here is the half that needs the machine: the `HEAD 000…0`
// sentinel that must never be passed through as a `CommitOid`.
// ---------------------------------------------------------------------------

#[test]
fn is_null_oid_matches_exactly_40_or_64_zeros() {
    assert!(is_null_oid(&"0".repeat(40)));
    assert!(is_null_oid(&"0".repeat(64)));
    assert!(!is_null_oid(&"0".repeat(39)));
    assert!(!is_null_oid(&format!("{}1", "0".repeat(39))));
    assert!(!is_null_oid(&"a".repeat(40)));
}
