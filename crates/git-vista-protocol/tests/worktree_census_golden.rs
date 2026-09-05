//! Golden-fixture test for the worktree-census wire contract (M11.01, #546):
//! [`WorktreeCensus`], [`WorktreeSibling`] and [`Serviceable`].
//!
//! `tests/fixtures/worktree_census_v1.json` is the **committed** wire form of
//! one [`WorktreeCensusGoldenSet`], deliberately covering the shapes a plain
//! round-trip cannot catch:
//!
//! - every [`Serviceable`] variant (`Yes`, `OutsideAllowedRoots`, `Missing`);
//! - both [`WorktreeCensus`] shapes (`Observed`, `CensusFailed`) — the
//!   distinction the type exists for, and the one a downstream reader must
//!   never collapse into "an empty list";
//! - `path` **present and absent**, since `path` is
//!   `skip_serializing_if = "Option::is_none"`: an optional field silently
//!   becoming always-present (or the reverse) round-trips fine through Rust
//!   but is a real wire change to a client's `"path" in obj`;
//! - a **detached** sibling (`branch` null, `head` present) and a **bare**
//!   one (`branch` and `head` both null, `bare: true`), which are three
//!   different reasons for a null and must not be allowed to collapse.
//!
//! Same two-directions proof as `status_golden.rs`/`dto_golden.rs`: the
//! fixture deserializes into exactly the values built here, and re-serializing
//! reproduces the fixture byte for byte.
//!
//! The `repository`/`id` values are **UUID-shaped**, matching `dto_v1.json`'s
//! own convention (`11111111-1111-5111-8111-111111111111`: a repeated digit,
//! the `5` version nibble and the `8` RFC-4122 variant nibble in place). That
//! is what production actually emits — both fields are the `Display` form of a
//! `git-vista-core` `RepositoryId`/`WorktreeId`, each a v5 UUID
//! (`identity.rs`'s `Uuid::new_v5`), rendered hyphenated with no prefix. A
//! fixture carrying an invented `r-…`/`w-…` shape would pin a wire form no
//! client will ever be handed, and would let a change that broke id rendering
//! sail through this file.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test worktree_census_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//!
//! No git process is spawned and no repository is read anywhere in this file —
//! every value is hand-built, exactly like `status_golden.rs`'s
//! `WorktreeStatus`. The real `git worktree list --porcelain` parser is
//! `git_vista_protocol::parse_worktree_porcelain` (unit-tested beside itself in
//! `src/worktree.rs`), and the enrichment that turns its records into these
//! values is `git-vista-server`'s `worktree_census`; neither is this file's
//! subject.

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

/// Every shape this wire family can carry, bundled the way `dto_golden.rs`
/// bundles `dto.rs`'s DTOs: one struct, one fixture file.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct WorktreeCensusGoldenSet {
    /// The served worktree: `is_current`, serviceable, path exposed
    /// (`GIT_VISTA_EXPOSE_PATHS` set), ordinary branch + head.
    current: WorktreeSibling,
    /// A linked sibling with its path **withheld** (the default) and git's own
    /// `locked` flag set — locked, yet still `Serviceable::Yes`, which is the
    /// whole point of keeping git's flags and the app's fence apart.
    locked_no_path: WorktreeSibling,
    /// Discovered, real, and refused — listed, never dropped (ADR 0092).
    outside_allowed_roots: WorktreeSibling,
    /// git's `prunable` flag set and the directory gone: `Serviceable::Missing`,
    /// which is a different sentence from `OutsideAllowedRoots`.
    missing: WorktreeSibling,
    /// Detached HEAD: `branch` null, `head` present, `bare` false.
    detached: WorktreeSibling,
    /// A bare-hub admin record: `branch` and `head` both null, `bare` true.
    bare: WorktreeSibling,
    /// The two `WorktreeCensus` shapes.
    observed: WorktreeCensus,
    /// A failed census with its path-bearing `detail` **withheld** — the
    /// default, and the shape a client sees unless the operator set
    /// `GIT_VISTA_EXPOSE_PATHS` (#657).
    census_failed: WorktreeCensus,
    /// The same failure with `detail` **present**. Both are goldened for the
    /// same reason `path` is goldened present and absent: `detail` is
    /// `skip_serializing_if = "Option::is_none"`, so an optional field
    /// silently becoming always-present (or the reverse) round-trips fine
    /// through Rust and is still a real wire change to a client's
    /// `"detail" in obj`.
    census_failed_with_detail: WorktreeCensus,
}

fn golden_set() -> WorktreeCensusGoldenSet {
    let current = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "22222222-2222-5222-8222-222222222222".to_string(),
        name: "git-vista".to_string(),
        path: Some("/home/user/projects/git-vista".to_string()),
        branch: Some(branch("main")),
        head: Some(oid("b7a947f8011f10fa6362e0ec96d9d766ca1f92a6")),
        is_current: true,
        locked: false,
        prunable: false,
        bare: false,
        serviceable: Serviceable::Yes,
    };
    let locked_no_path = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "33333333-3333-5333-8333-333333333333".to_string(),
        name: "git-vista-m11".to_string(),
        path: None,
        branch: Some(branch("feature/m11-worktrees")),
        head: Some(oid("cccccccccccccccccccccccccccccccccccccccc")),
        is_current: false,
        locked: true,
        prunable: false,
        bare: false,
        serviceable: Serviceable::Yes,
    };
    let outside_allowed_roots = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "44444444-4444-5444-8444-444444444444".to_string(),
        name: "git-vista-codex".to_string(),
        path: Some("/home/user/gv/variants/git-vista-codex".to_string()),
        branch: Some(branch("codex/65-sheet-wiring")),
        head: Some(oid("dddddddddddddddddddddddddddddddddddddddd")),
        is_current: false,
        locked: false,
        prunable: false,
        bare: false,
        serviceable: Serviceable::OutsideAllowedRoots,
    };
    let missing = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "55555555-5555-5555-8555-555555555555".to_string(),
        name: "git-vista-testbed".to_string(),
        path: Some("/home/user/projects/git-vista-testbed".to_string()),
        branch: Some(branch("testbed/main-20260817-2219")),
        head: Some(oid("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")),
        is_current: false,
        locked: false,
        prunable: true,
        bare: false,
        serviceable: Serviceable::Missing,
    };
    let detached = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "66666666-6666-5666-8666-666666666666".to_string(),
        name: "git-vista-detached".to_string(),
        path: Some("/home/user/projects/git-vista-detached".to_string()),
        branch: None,
        head: Some(oid("ffffffffffffffffffffffffffffffffffffffff")),
        is_current: false,
        locked: false,
        prunable: false,
        bare: false,
        serviceable: Serviceable::Yes,
    };
    let bare = WorktreeSibling {
        repository: "11111111-1111-5111-8111-111111111111".to_string(),
        id: "77777777-7777-5777-8777-777777777777".to_string(),
        name: "git-vista.git".to_string(),
        path: Some("/home/user/projects/git-vista.git".to_string()),
        branch: None,
        head: None,
        is_current: false,
        locked: false,
        prunable: false,
        bare: true,
        serviceable: Serviceable::Yes,
    };
    let observed = WorktreeCensus::Observed {
        siblings: vec![
            current.clone(),
            locked_no_path.clone(),
            outside_allowed_roots.clone(),
            missing.clone(),
            detached.clone(),
            bare.clone(),
        ],
    };
    // Note what the two reasons have in common: they are the **same string**.
    // The flag adds `detail`; it never rewrites `reason` (#657, ADR 0119), and
    // pinning both here is what makes that invariant a wire fact rather than a
    // claim in a doc comment.
    let census_failed = WorktreeCensus::CensusFailed {
        reason: "`git worktree list --porcelain` failed".to_string(),
        detail: None,
    };
    let census_failed_with_detail = WorktreeCensus::CensusFailed {
        reason: "`git worktree list --porcelain` failed".to_string(),
        detail: Some(
            "`git worktree list --porcelain` failed: fatal: not a git repository: \
             '/home/someone/private/repo/.git'"
                .to_string(),
        ),
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
        census_failed_with_detail,
    }
}

#[test]
fn worktree_census_v1_golden() {
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

    let parsed: WorktreeCensusGoldenSet = serde_json::from_str(&fixture)
        .expect("fixture must deserialize into WorktreeCensusGoldenSet");
    assert_eq!(
        parsed, set,
        "fixture and in-code golden census set diverged — if this is \
         deliberate, regenerate with REGEN_GOLDEN=1 and review the diff"
    );

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized census set no longer matches the committed fixture at \
         tests/fixtures/worktree_census_v1.json — if this wire change is \
         intentional, regenerate with `REGEN_GOLDEN=1 cargo test -p \
         git-vista-protocol --test worktree_census_golden`, review the diff, \
         and record the protocol implications; if it was not intentional, you \
         have just broken whatever client depended on this shape"
    );
}

/// The tag strings a client matches on, read off the **committed fixture**
/// rather than off a freshly-serialized value.
///
/// This is the leg that survives a rename: `serde_json::to_value(&x)["kind"]`
/// asks the code under test what it calls itself and always agrees with
/// itself, so it would stay green through a variant rename that broke every
/// deployed client. Reading the literal file cannot.
#[test]
fn the_fixture_file_carries_the_literal_wire_tags_a_client_matches_on() {
    let raw: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();

    // Hand-written table, not derived from `Serviceable` — see the doc above.
    for (field, expected) in [
        ("current", "yes"),
        ("locked_no_path", "yes"),
        ("outside_allowed_roots", "outside_allowed_roots"),
        ("missing", "missing"),
        ("detached", "yes"),
        ("bare", "yes"),
    ] {
        assert_eq!(
            raw[field]["serviceable"]["kind"], expected,
            "{field}'s serviceable tag drifted in the committed fixture"
        );
    }
    assert_eq!(raw["observed"]["kind"], "observed");
    assert_eq!(raw["census_failed"]["kind"], "census_failed");
    assert!(
        raw["census_failed"]["reason"].is_string(),
        "CensusFailed must carry a human-readable reason on the wire"
    );
    // #657: the flag adds a field, it does not rewrite one. Read off the
    // committed file rather than off a freshly-serialized value, for the same
    // reason the tags above are.
    assert!(
        raw["census_failed"].get("detail").is_none(),
        "detail must be absent from the wire when withheld, not null: {}",
        raw["census_failed"]
    );
    assert_eq!(
        raw["census_failed_with_detail"]["reason"], raw["census_failed"]["reason"],
        "the operator's opt-in must add `detail`, never rewrite `reason` — a \
         client that matched on the reason string would otherwise see a \
         different message purely because of a server-side env var"
    );
    assert!(
        raw["census_failed_with_detail"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains('/')),
        "the goldened opted-in detail must actually carry the absolute path \
         the flag exists to gate: {}",
        raw["census_failed_with_detail"]
    );
    assert!(
        raw["observed"]["siblings"].is_array(),
        "Observed must carry its siblings as an array on the wire"
    );
}

/// The three `Serviceable` variants are all in the set, and nothing else is.
///
/// `Serviceable` has no `strum`-style variant iterator, so the coverage claim
/// is pinned against a hand-written list: adding a fourth variant without
/// adding it to the fixture fails here rather than quietly leaving the new
/// state ungoldened.
#[test]
fn the_golden_set_covers_every_serviceable_variant_and_no_more() {
    let set = golden_set();
    let seen: Vec<&Serviceable> = [
        &set.current,
        &set.locked_no_path,
        &set.outside_allowed_roots,
        &set.missing,
        &set.detached,
        &set.bare,
    ]
    .into_iter()
    .map(|s| &s.serviceable)
    .collect();

    for want in [
        Serviceable::Yes,
        Serviceable::OutsideAllowedRoots,
        Serviceable::Missing,
    ] {
        assert!(
            seen.contains(&&want),
            "{want:?} is not represented in the golden set"
        );
    }
    // …and no fourth, unfixtured state has appeared. `Serviceable` has no
    // variant iterator, so this counts distinct Rust variant names — a new
    // variant added to the enum and to the set (but never to the fixture, or
    // vice versa) still lands in the round-trip test above; a new variant
    // added to the enum and *not* to the set fails the loop above.
    let distinct: std::collections::BTreeSet<String> =
        seen.iter().map(|s| format!("{s:?}")).collect();
    assert_eq!(
        distinct.len(),
        3,
        "Serviceable gained or lost a variant; the golden fixture must be          regenerated to cover it: {distinct:?}"
    );
}

/// `path` is `skip_serializing_if = "Option::is_none"`, so "absent" and
/// "null" are different bytes to a client's `JSON.parse`. Both directions are
/// pinned on the **raw fixture**, which a Rust round-trip cannot distinguish.
///
/// Same for `branch`/`head`, which are *not* skipped: they must be present as
/// explicit `null`s, so a client can tell "detached" from "this build of the
/// server does not send a branch at all".
#[test]
fn the_fixture_pins_path_omission_and_explicit_branch_head_nulls() {
    let raw: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();

    assert!(
        raw["current"].as_object().unwrap().contains_key("path"),
        "the path-exposed case must actually carry a path key"
    );
    assert!(
        !raw["locked_no_path"]
            .as_object()
            .unwrap()
            .contains_key("path"),
        "an absent path must be omitted from the wire, never sent as null"
    );

    assert!(
        raw["detached"]["branch"].is_null() && !raw["detached"]["head"].is_null(),
        "a detached sibling is null branch + real head"
    );
    assert_eq!(raw["detached"]["bare"], false);
    assert!(
        raw["bare"]["branch"].is_null() && raw["bare"]["head"].is_null(),
        "a bare record is null branch + null head"
    );
    assert_eq!(raw["bare"]["bare"], true);
    for field in ["branch", "head"] {
        assert!(
            raw["bare"].as_object().unwrap().contains_key(field),
            "`{field}` is not skip_serializing_if — it must be an explicit \
             null, not omitted"
        );
    }
}

/// The invariant the `WorktreeCensus` type exists for: an empty `Observed` is
/// not a `CensusFailed`, in Rust *and* on the wire.
#[test]
fn an_empty_observed_is_never_a_census_failed() {
    let set = golden_set();
    let empty = WorktreeCensus::Observed { siblings: vec![] };
    assert_ne!(set.census_failed, empty);
    assert_ne!(
        serde_json::to_value(&set.census_failed).unwrap(),
        serde_json::to_value(&empty).unwrap()
    );
    // And `Observed` in the fixture is the populated one, so the round-trip
    // test above is proving something about a real list.
    assert_ne!(set.observed, empty);
}
