//! Golden-fixture test for the [`DiffSpec`] wire contract (M2.16, #69b).
//!
//! `tests/fixtures/diff_spec_v1.json` is the **committed** wire form of all
//! four [`DiffSpec`] modes. Same pattern as `diff_golden.rs`/`status_golden.rs`:
//! the fixture deserializes into exactly the value built here, and
//! re-serializing reproduces the fixture byte for byte.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test diff_spec_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//!
//! No git process is spawned anywhere in this file — every value is
//! hand-built. `diff_spec_argv`'s own unit tests (in `diff.rs`) pin the argv
//! mapping; this file only pins the wire shape.

use git_vista_protocol::plan::{CommitOid, RefName};
use git_vista_protocol::DiffSpec;

const FIXTURE: &str = include_str!("fixtures/diff_spec_v1.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/diff_spec_v1.json");

fn commit(hex_prefix: char) -> CommitOid {
    CommitOid::new(hex_prefix.to_string().repeat(40)).unwrap()
}

/// One of each [`DiffSpec`] mode — the fixture this test pins.
fn golden_specs() -> Vec<DiffSpec> {
    vec![
        DiffSpec::WorktreeVsIndex,
        DiffSpec::IndexVsCommit {
            commit: commit('a'),
        },
        DiffSpec::CommitVsCommit {
            base: commit('a'),
            target: commit('b'),
        },
        DiffSpec::RefVsRef {
            base: RefName::new("main").unwrap(),
            target: RefName::new("feature/x").unwrap(),
        },
    ]
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let specs = golden_specs();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&specs).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    let parsed: Vec<DiffSpec> =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, specs, "fixture and in-code golden specs diverged");

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized specs no longer match the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_mode() {
    let specs = golden_specs();
    let modes: std::collections::BTreeSet<String> = specs
        .iter()
        .map(|s| {
            serde_json::to_value(s).unwrap()["mode"]
                .as_str()
                .expect("every DiffSpec serializes with a mode tag")
                .to_string()
        })
        .collect();
    let expected: std::collections::BTreeSet<String> = [
        "worktree_vs_index",
        "index_vs_commit",
        "commit_vs_commit",
        "ref_vs_ref",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(modes, expected, "a DiffSpec mode is missing from (or extra in) the golden set");
}
