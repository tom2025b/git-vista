//! Golden-fixture test for the graph-preview wire contract (M10.08, #576).
//!
//! `tests/fixtures/preview_v1.json` is the **committed** wire form of one
//! value per [`PreviewOutcome`] arm — the `graph` arm with all three
//! [`PreviewChange`] variants and a fully-populated [`PreviewGraph`] on each
//! side, `conflict` with a lossily-decoded path, `unsupported`, and one
//! `unavailable` per [`PreviewUnavailable`] reason. Same shape as
//! `plan_golden.rs` and `status_golden.rs`: the fixture deserializes into
//! exactly the value built here, and re-serializing reproduces the fixture
//! byte for byte.
//!
//! # Why this file exists, when `protocol/src/preview.rs` already has tests
//!
//! Because those tests round-trip **the same live Rust type** on both sides,
//! and an independent audit of the #576 branch proved what that cannot catch:
//! renaming `Conflict.paths` to serialize as `conflicted_paths` — and,
//! separately, renaming `PreviewGraph.stubs` — left all 177 protocol unit
//! tests and every integration test green, while a v9 server and a v9 client
//! stopped understanding each other with no version gate in the way. Both
//! halves of a round trip move together; only committed literal text pins the
//! key names an actual peer will see. No fixture anywhere contained a literal
//! `paths` key until this one.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test preview_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//! [`the_fixture_spells_every_load_bearing_key_and_tag`] has **no** regen
//! path, on purpose — a blind regeneration after a rename stays red there
//! until a human re-reads the spellings.
//!
//! No git process is spawned and no repository is read anywhere in this file.

use std::collections::BTreeMap;

use git_vista_core::model::{BranchStub, CommitSummary, Edge, GitRef, GraphRow, Oid, RefKind};
use git_vista_core::preview::PreviewChange;
use git_vista_protocol::preview::{PreviewGraph, PreviewOutcome, PreviewUnavailable};

const FIXTURE: &str = include_str!("fixtures/preview_v1.json");
const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/preview_v1.json"
);

/// The alias the server and the frontend each declare for themselves.
type Outcome = PreviewOutcome<GraphRow, Edge, BranchStub, PreviewChange>;

fn oid(digit: char) -> Oid {
    Oid((0..40).map(|_| digit).collect())
}

/// The `before` half: two rows on one lane, `main` and a tag on the tip,
/// HEAD badge present, one stub — every field of every graph type populated.
fn before_half() -> PreviewGraph<GraphRow, Edge, BranchStub> {
    PreviewGraph {
        rows: vec![
            GraphRow {
                commit: CommitSummary {
                    id: oid('2'),
                    parents: vec![oid('1')],
                    summary: "add thing".into(),
                    author: "Ada".into(),
                    time: 400,
                },
                row: 0,
                lane: 0,
                refs: vec![
                    GitRef {
                        name: "HEAD".into(),
                        kind: RefKind::Head,
                        target: oid('2'),
                    },
                    GitRef {
                        name: "main".into(),
                        kind: RefKind::Branch,
                        target: oid('2'),
                    },
                    GitRef {
                        name: "v1.0.0".into(),
                        kind: RefKind::Tag,
                        target: oid('2'),
                    },
                ],
                color: 0,
                on_remote: true,
            },
            GraphRow {
                commit: CommitSummary {
                    id: oid('1'),
                    parents: vec![],
                    summary: "root".into(),
                    author: "Ada".into(),
                    time: 300,
                },
                row: 1,
                lane: 0,
                refs: vec![],
                color: 0,
                on_remote: false,
            },
        ],
        edges: vec![Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 0,
        }],
        stubs: vec![BranchStub {
            name: "spike".into(),
            anchor_row: 1,
            anchor_lane: 0,
            lane: 2,
            color: 5,
            depth: 0,
        }],
        lane_count: 3,
    }
}

/// The `after` half: the hypothetical revert on top, `main`/HEAD moved onto
/// it, the old tip shifted down a row. Deliberately different from `before`
/// so the fixture never contains two identical halves a diff could confuse.
fn after_half() -> PreviewGraph<GraphRow, Edge, BranchStub> {
    let mut half = before_half();
    for row in &mut half.rows {
        row.row += 1;
        row.refs.retain(|r| r.kind == RefKind::Tag);
    }
    half.rows.insert(
        0,
        GraphRow {
            commit: CommitSummary {
                id: oid('9'),
                parents: vec![oid('2')],
                summary: "Revert \"add thing\"".into(),
                author: "git-vista".into(),
                time: 500,
            },
            row: 0,
            lane: 0,
            refs: vec![
                GitRef {
                    name: "HEAD".into(),
                    kind: RefKind::Head,
                    target: oid('9'),
                },
                GitRef {
                    name: "main".into(),
                    kind: RefKind::Branch,
                    target: oid('9'),
                },
            ],
            color: 0,
            on_remote: false,
        },
    );
    half.edges = vec![
        Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 0,
        },
        Edge {
            from_row: 1,
            from_lane: 0,
            to_row: 2,
            to_lane: 0,
        },
    ];
    for stub in &mut half.stubs {
        stub.anchor_row += 1;
    }
    half
}

/// One value per arm — and, through the `unavailable_*` cases, one per
/// [`PreviewUnavailable`] reason. A `BTreeMap` so the committed key order is
/// sorted and stable, never insertion-order luck.
fn golden_outcomes() -> BTreeMap<&'static str, Outcome> {
    let mut cases: BTreeMap<&'static str, Outcome> = BTreeMap::new();
    cases.insert(
        "graph",
        PreviewOutcome::Graph {
            before: before_half(),
            after: after_half(),
            changes: vec![
                PreviewChange::Added { commit: oid('9') },
                PreviewChange::RefMoved {
                    ref_name: "main".into(),
                    from: oid('2'),
                    to: oid('9'),
                },
                PreviewChange::LaneShifted {
                    commit: oid('2'),
                    from_lane: 0,
                    to_lane: 1,
                },
            ],
        },
    );
    cases.insert(
        "conflict",
        PreviewOutcome::Conflict {
            paths: vec![
                "src/main.rs".into(),
                "docs/na\u{fffd}me.md".into(),
                "with space/and'quote.txt".into(),
            ],
        },
    );
    cases.insert(
        "unsupported",
        PreviewOutcome::Unsupported {
            operation: "RebaseBranch".into(),
        },
    );
    cases.insert(
        "unavailable_repository_read_only",
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::RepositoryReadOnly,
        },
    );
    cases.insert(
        "unavailable_git_too_old",
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::GitTooOld {
                found: "2.34.1".into(),
                minimum: "2.38".into(),
            },
        },
    );
    cases.insert(
        "unavailable_scratch_store",
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::ScratchStore {
                detail: "mkdir: permission denied".into(),
            },
        },
    );
    cases.insert(
        "unavailable_check_failed",
        PreviewOutcome::Unavailable {
            reason: PreviewUnavailable::CheckFailed {
                detail: "merge-tree exited with signal 9".into(),
            },
        },
    );
    cases
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let outcomes = golden_outcomes();

    // Deliberate-regeneration path (see module docs): rewrite the fixture
    // from the values above, then fall through and verify against what was
    // written.
    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&outcomes).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    // 1. The committed wire form deserializes into exactly these values…
    let parsed: BTreeMap<String, Outcome> =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    let expected: BTreeMap<String, Outcome> = outcomes
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    assert_eq!(parsed, expected, "fixture and in-code golden set diverged");

    // 2. …and re-serializing reproduces the committed bytes exactly, so no
    //    field is dropped, defaulted, renamed, or reordered in flight.
    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized preview outcomes no longer match the committed fixture \
         — if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

/// Every load-bearing key and tag, spelled out as a literal and required in
/// the committed fixture text itself.
///
/// This is the half a regenerable byte-compare cannot carry: under
/// `REGEN_GOLDEN=1` the test above rewrites the fixture and then agrees with
/// what it wrote, so a rename plus a blind regen sails through it. These
/// literals have no regen path — after the audit's `paths` →
/// `conflicted_paths` rename this test stays red, whatever was regenerated,
/// until a human re-reads the wire contract and updates the spellings here
/// deliberately.
///
/// Quoted-with-colon needles (`"paths":` style, matched against the pretty
/// printer's `"paths": `) so a value that merely contains the word cannot
/// satisfy a key assertion.
#[test]
fn the_fixture_spells_every_load_bearing_key_and_tag() {
    // Keys: object member names, exactly as a peer will read them.
    for key in [
        // PreviewOutcome
        "outcome",
        "before",
        "after",
        "changes",
        "paths",
        "operation",
        "reason",
        // PreviewGraph
        "rows",
        "edges",
        "stubs",
        "lane_count",
        // GraphRow / CommitSummary / GitRef
        "commit",
        "row",
        "lane",
        "refs",
        "color",
        "on_remote",
        "id",
        "parents",
        "summary",
        "author",
        "time",
        "name",
        "kind",
        "target",
        // Edge
        "from_row",
        "from_lane",
        "to_row",
        "to_lane",
        // BranchStub
        "anchor_row",
        "anchor_lane",
        "depth",
        // PreviewChange
        "change",
        "ref_name",
        "from",
        "to",
        // PreviewUnavailable
        "unavailable",
        "found",
        "minimum",
        "detail",
    ] {
        let needle = format!("\"{key}\":");
        assert!(
            FIXTURE.contains(&needle),
            "the committed fixture never spells the key {needle} — either a \
             wire field was renamed (a protocol change wearing a refactor's \
             clothes) or the golden set stopped covering it"
        );
    }

    // Tags: the discriminant *values* a peer switches on.
    for tag in [
        "\"outcome\": \"graph\"",
        "\"outcome\": \"conflict\"",
        "\"outcome\": \"unsupported\"",
        "\"outcome\": \"unavailable\"",
        "\"unavailable\": \"repository_read_only\"",
        "\"unavailable\": \"git_too_old\"",
        "\"unavailable\": \"scratch_store\"",
        "\"unavailable\": \"check_failed\"",
        "\"change\": \"added\"",
        "\"change\": \"ref_moved\"",
        "\"change\": \"lane_shifted\"",
    ] {
        assert!(
            FIXTURE.contains(tag),
            "the committed fixture never carries {tag} — an arm or reason \
             fell out of the golden set, or its wire tag changed spelling"
        );
    }
}
