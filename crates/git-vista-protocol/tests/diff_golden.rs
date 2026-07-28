//! Golden-fixture test for the [`ParsedPatch`] wire contract (M2.16, #69a).
//!
//! `tests/fixtures/diff_v1.json` is the **committed** wire form of one
//! [`ParsedPatch`], its `files` covering every [`FileDiff`] shape. Same
//! pattern as `status_golden.rs`: the fixture deserializes into exactly the
//! value built here, and re-serializing reproduces the fixture byte for
//! byte.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test diff_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//!
//! No git process is spawned anywhere in this file — every value is
//! hand-built, exactly like `plan_golden.rs`'s `Plan` values. The real
//! `parse_unified_diff` parser that produces a `ParsedPatch` from real
//! `git show --patch` text is exercised by `diff.rs`'s own unit tests
//! against real captured git output; this file only pins the wire shape.

use git_vista_protocol::{DiffLine, FileDiff, Hunk, LineKind, ParsedPatch};

const FIXTURE: &str = include_str!("fixtures/diff_v1.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/diff_v1.json");

fn line(kind: LineKind, text: &str) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_string(),
        no_newline_at_eof: false,
    }
}

/// One [`ParsedPatch`], its `files` deliberately covering every [`FileDiff`]
/// variant — the fixture this test pins.
fn golden_patch() -> ParsedPatch {
    ParsedPatch {
        files: vec![
            // An ordinary edit, with a no-newline-at-eof line.
            FileDiff::Hunks {
                old_path: Some("a.txt".to_string()),
                new_path: Some("a.txt".to_string()),
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 3,
                    new_start: 1,
                    new_len: 3,
                    section_heading: "fn main() {".to_string(),
                    lines: vec![
                        line(LineKind::Context, "one"),
                        line(LineKind::Removed, "two"),
                        DiffLine {
                            kind: LineKind::Added,
                            text: "TWO".to_string(),
                            no_newline_at_eof: true,
                        },
                    ],
                }],
            },
            // A new file with content.
            FileDiff::Hunks {
                old_path: None,
                new_path: Some("new.txt".to_string()),
                hunks: vec![Hunk {
                    old_start: 0,
                    old_len: 0,
                    new_start: 1,
                    new_len: 1,
                    section_heading: String::new(),
                    lines: vec![line(LineKind::Added, "hello")],
                }],
            },
            // A deleted file.
            FileDiff::Hunks {
                old_path: Some("gone.txt".to_string()),
                new_path: None,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 0,
                    new_len: 0,
                    section_heading: String::new(),
                    lines: vec![line(LineKind::Removed, "bye")],
                }],
            },
            // A new, empty file — Hunks with no hunks at all.
            FileDiff::Hunks {
                old_path: None,
                new_path: Some("empty.txt".to_string()),
                hunks: vec![],
            },
            FileDiff::ModeChangeOnly {
                path: "script.sh".to_string(),
                old_mode: "100644".to_string(),
                new_mode: "100755".to_string(),
            },
            FileDiff::Binary {
                old_path: Some("image.png".to_string()),
                new_path: Some("image.png".to_string()),
            },
            FileDiff::Binary {
                old_path: None,
                new_path: Some("new-image.png".to_string()),
            },
            FileDiff::Renamed {
                old_path: "old-name.rs".to_string(),
                new_path: "new-name.rs".to_string(),
                similarity: 100,
                is_copy: false,
            },
            FileDiff::Renamed {
                old_path: "template.rs".to_string(),
                new_path: "template-copy.rs".to_string(),
                similarity: 95,
                is_copy: true,
            },
            FileDiff::Combined {
                path: "conflict.txt".to_string(),
                raw: "diff --combined conflict.txt\nindex 111,222..333\n--- a/conflict.txt\n+++ b/conflict.txt\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n+ b\n".to_string(),
            },
        ],
    }
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let patch = golden_patch();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&patch).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    let parsed: ParsedPatch = serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, patch, "fixture and in-code golden patch diverged");

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized patch no longer matches the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_file_diff_shape() {
    let patch = golden_patch();
    let shapes: std::collections::BTreeSet<String> = patch
        .files
        .iter()
        .map(|f| {
            serde_json::to_value(f).unwrap()["shape"]
                .as_str()
                .expect("every FileDiff serializes with a shape tag")
                .to_string()
        })
        .collect();
    let expected: std::collections::BTreeSet<String> =
        ["hunks", "mode_change_only", "binary", "renamed", "combined"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(
        shapes, expected,
        "a FileDiff shape is missing from (or extra in) the golden set"
    );

    // Both a real rename and a real copy are covered, not just "renamed"
    // appearing once — is_copy is the field that distinguishes them.
    let has_rename = patch
        .files
        .iter()
        .any(|f| matches!(f, FileDiff::Renamed { is_copy: false, .. }));
    let has_copy = patch
        .files
        .iter()
        .any(|f| matches!(f, FileDiff::Renamed { is_copy: true, .. }));
    assert!(
        has_rename && has_copy,
        "both a rename and a copy must be in the golden set"
    );

    // Both a present and an absent old_path/new_path (new file, deleted
    // file) are covered on the Hunks variant.
    let has_no_old_path = patch
        .files
        .iter()
        .any(|f| matches!(f, FileDiff::Hunks { old_path: None, .. }));
    let has_no_new_path = patch
        .files
        .iter()
        .any(|f| matches!(f, FileDiff::Hunks { new_path: None, .. }));
    let has_empty_hunks = patch
        .files
        .iter()
        .any(|f| matches!(f, FileDiff::Hunks { hunks, .. } if hunks.is_empty()));
    assert!(
        has_no_old_path,
        "no new-file (old_path: None) case in the golden set"
    );
    assert!(
        has_no_new_path,
        "no deleted-file (new_path: None) case in the golden set"
    );
    assert!(has_empty_hunks, "no empty-hunks case in the golden set");
}
