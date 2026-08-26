//! Golden-fixture test for the worktree census wire contract (M11, #546):
//! [`WorktreeCensus`], [`WorktreeSibling`], and [`Serviceable`].
//!
//! `tests/fixtures/worktree_census_v1.json` is the **committed** wire form of
//! a [`WorktreeCensusGoldenSet`] covering every [`Serviceable`] variant, both
//! the `Observed` and `CensusFailed` shapes of [`WorktreeCensus`], a sibling
//! with `path` present and one with it omitted (`GIT_VISTA_EXPOSE_PATHS`),
//! and a bare-repository sibling (`branch`/`head` both absent) alongside an
//! ordinary one. Same two-directions proof as `status_golden.rs`: the
//! fixture deserializes into exactly the value built here, and re-serializing
//! reproduces the fixture byte for byte.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test worktree_census_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//!
//! No git process is spawned and no repository is read anywhere in this file
//! — every value is hand-built, exactly like `status_golden.rs`'s
//! `WorktreeStatus` values. The real `git worktree list --porcelain -z`
//! parser and the server-side enrichment that populate these from a live
//! repository are `git-vista-protocol::parse_worktree_list_porcelain_z` and
//! `git-vista-server::handlers::worktrees`, not this task.

use git_vista_protocol::{BranchName, CommitOid, Serviceable, WorktreeCensus, WorktreeSibling};
use serde::{Deserialize, Serialize};

const FIXTURE: &str = include_str!("fixtures/worktree_census_v1.json");
const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/worktree_census_v1.json"
);

fn branch(s: &str) -> BranchName {
    BranchName::new(s).unwrap()
}

fn oid(s: &str) -> CommitOid {
    CommitOid::new(s).unwrap()
}

fn worktree_token(s: &str) -> git_vista_protocol::WorktreeToken {
    git_vista_protocol::WorktreeToken::new(s).unwrap()
}

/// Every public shape this family can carry, bundled the way `dto_golden.rs`
/// bundles `dto.rs`'s DTOs: one struct, one fixture file.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct WorktreeCensusGoldenSet {
    // The current worktree: serviceable, path exposed, ordinary branch/head.
    current: WorktreeSibling,
    // A linked sibling with a path withheld (the default,
    // `GIT_VISTA_EXPOSE_PATHS` unset) and its lock flag set.
    locked_no_path: WorktreeSibling,
    // A sibling outside the allowed roots — discovered and refused, not
    // dropped (see `Serviceable::OutsideAllowedRoots`'s own doc comment).
    outside_allowed_roots: WorktreeSibling,
    // A prunable sibling whose directory is gone.
    missing: WorktreeSibling,
    // A detached-HEAD sibling: `branch` absent, `head` present.
    detached: WorktreeSibling,
    // A bare-repository sibling: both `branch` and `head` absent.
    bare: WorktreeSibling,
    // The two `WorktreeCensus` shapes.
    observed: WorktreeCensus,
    census_failed: WorktreeCensus,
}

fn golden_set() -> WorktreeCensusGoldenSet {
    let current = WorktreeSibling {
        id: worktree_token("11111111-1111-1111-1111-111111111111"),
        path: Some("/home/user/git-vista".to_string()),
        branch: Some(branch("main")),
        head: Some(oid("b7a947f8011f10fa6362e0ec96d9d766ca1f92a6")),
        is_current: true,
        locked: false,
        prunable: false,
        serviceable: Serviceable::Yes,
    };
    let locked_no_path = WorktreeSibling {
        id: worktree_token("22222222-2222-2222-2222-222222222222"),
        path: None,
        branch: Some(branch("feature/m11-worktrees")),
        head: Some(oid("cccccccccccccccccccccccccccccccccccccccc")),
        is_current: false,
        locked: true,
        prunable: false,
        serviceable: Serviceable::Yes,
    };
    let outside_allowed_roots = WorktreeSibling {
        id: worktree_token("33333333-3333-3333-3333-333333333333"),
        path: Some("/home/user/gv/variants/git-vista-codex".to_string()),
        branch: Some(branch("codex/65-sheet-wiring")),
        head: Some(oid("dddddddddddddddddddddddddddddddddddddddd")),
        is_current: false,
        locked: false,
        prunable: false,
        serviceable: Serviceable::OutsideAllowedRoots,
    };
    let missing = WorktreeSibling {
        id: worktree_token("44444444-4444-4444-4444-444444444444"),
        path: Some("/home/user/projects/git-vista-testbed-8081".to_string()),
        branch: Some(branch("testbed/main-20260817-2219")),
        head: Some(oid("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")),
        is_current: false,
        locked: false,
        prunable: true,
        serviceable: Serviceable::Missing,
    };
    let detached = WorktreeSibling {
        id: worktree_token("55555555-5555-5555-5555-555555555555"),
        path: Some("/home/user/projects/git-vista-detached".to_string()),
        branch: None,
        head: Some(oid("ffffffffffffffffffffffffffffffffffffffff")),
        is_current: false,
        locked: false,
        prunable: false,
        serviceable: Serviceable::Yes,
    };
    let bare = WorktreeSibling {
        id: worktree_token("66666666-6666-6666-6666-666666666666"),
        path: Some("/home/user/projects/git-vista.git".to_string()),
        branch: None,
        head: None,
        is_current: false,
        locked: false,
        prunable: false,
        serviceable: Serviceable::Yes,
    };
    let observed = WorktreeCensus::Observed {
        siblings: vec![current.clone(), locked_no_path.clone()],
    };
    let census_failed = WorktreeCensus::CensusFailed {
        reason: "git worktree list --porcelain -z exited non-zero: not a git repository"
            .to_string(),
    };
    WorktreeCensusGoldenSet {
        current,
        locked_no_path,
        outside_allowed_roots,
        missing,
        detached,
        bare,
        observed,
        census_failed,
    }
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let set = golden_set();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&set).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    let parsed: WorktreeCensusGoldenSet =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, set, "fixture and in-code golden set diverged");

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized census no longer matches the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_serviceable_variant() {
    let set = golden_set();
    let variants: std::collections::BTreeSet<String> = [
        &set.current,
        &set.locked_no_path,
        &set.outside_allowed_roots,
        &set.missing,
        &set.detached,
        &set.bare,
    ]
    .iter()
    .map(|s| {
        serde_json::to_value(s.serviceable).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_string()
    })
    .collect();
    let expected: std::collections::BTreeSet<String> = ["yes", "outside_allowed_roots", "missing"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        variants, expected,
        "a Serviceable variant is missing from (or extra in) the golden set"
    );
}

#[test]
fn golden_set_covers_both_worktree_census_shapes() {
    let set = golden_set();
    assert_eq!(
        serde_json::to_value(&set.observed).unwrap()["status"],
        "observed"
    );
    assert_eq!(
        serde_json::to_value(&set.census_failed).unwrap()["status"],
        "census_failed"
    );
    // `Observed([])` must never be confused with `CensusFailed` — the whole
    // point of the type (module doc, "The census itself can fail").
    assert_ne!(set.observed, WorktreeCensus::Observed { siblings: vec![] });
}

#[test]
fn golden_set_covers_path_present_absent_and_bare_branch_head() {
    let set = golden_set();
    assert!(set.current.path.is_some(), "path-exposed case missing");
    assert!(
        set.locked_no_path.path.is_none(),
        "path-withheld case missing"
    );
    assert!(
        set.bare.branch.is_none() && set.bare.head.is_none(),
        "bare-repository sibling (no branch, no head) missing from the golden set"
    );
    assert!(
        set.detached.branch.is_none() && set.detached.head.is_some(),
        "detached-HEAD sibling (no branch, real head) missing from the golden set"
    );
    assert!(
        set.current.branch.is_some() && set.current.head.is_some(),
        "an ordinary branch+head sibling must also be in the golden set"
    );
}
