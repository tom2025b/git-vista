//! Golden-fixture test for the [`PatchPlan`] wire contract (M2.17a, #212).
//!
//! `tests/fixtures/patch_plan_v1.json` is the **committed** wire form of
//! four plans that together exercise every [`SelectionShape`] variant, both
//! [`StageDirection`]s, and the mixed several-files-several-granularities
//! case #213's endpoints will actually receive. The test proves the contract
//! is lossless in both directions:
//!
//! 1. the fixture deserializes into exactly the plans built here in code, and
//! 2. re-serializing those plans reproduces the fixture **byte for byte**.
//!
//! So any accidental rename, retag, or field change breaks this test loudly —
//! a wire change must be deliberate: update the fixture by running
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test patch_plan_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).

use git_vista_protocol::{
    FileSelection, GenerationToken, HunkLines, HunkRef, PatchPlan, PatchPreview, RepositoryToken,
    SelectionShape, StageDirection, StagingDiff, WorktreeToken,
};
use serde::{Deserialize, Serialize};

const FIXTURE: &str = include_str!("fixtures/patch_plan_v1.json");
const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/patch_plan_v1.json"
);

/// One plan with the identity/staleness boilerplate filled in — the same
/// fixed tokens `plan_golden.rs` uses, so the two fixtures read alike.
fn patch_plan(direction: StageDirection, files: Vec<FileSelection>) -> PatchPlan {
    PatchPlan {
        repository: RepositoryToken::new("11111111-1111-5111-8111-111111111111").unwrap(),
        worktree: WorktreeToken::new("22222222-2222-5222-8222-222222222222").unwrap(),
        generation: GenerationToken::new("12345678901234567890").unwrap(),
        direction,
        files,
    }
}

fn hunk(index: u32, old_start: u32, new_start: u32) -> HunkRef {
    HunkRef {
        index,
        old_start,
        new_start,
    }
}

/// The golden set. Every plan also passes [`PatchPlan::validate`] — pinned
/// below — so the committed wire forms double as canonical-form examples.
fn golden_patch_plans() -> Vec<PatchPlan> {
    vec![
        // Whole-file staging — the only granularity binary / mode-only /
        // no-content-rename diffs have.
        patch_plan(
            StageDirection::Stage,
            vec![FileSelection {
                path: "assets/logo.png".to_string(),
                selection: SelectionShape::EntireFile,
            }],
        ),
        // Hunk-level staging (#213's execution scope): non-adjacent ordinals,
        // anchors repeating the pinned headers.
        patch_plan(
            StageDirection::Stage,
            vec![FileSelection {
                path: "src/lib.rs".to_string(),
                selection: SelectionShape::Hunks {
                    hunks: vec![hunk(0, 10, 10), hunk(2, 91, 94)],
                },
            }],
        ),
        // Line-level unstaging (#214's execution scope): sub-hunk indices
        // into `Hunk::lines`, context lines never selected.
        patch_plan(
            StageDirection::Unstage,
            vec![FileSelection {
                path: "src/net.rs".to_string(),
                selection: SelectionShape::Lines {
                    hunks: vec![
                        HunkLines {
                            hunk: hunk(1, 40, 42),
                            lines: vec![1, 2],
                        },
                        HunkLines {
                            hunk: hunk(3, 200, 205),
                            lines: vec![0, 4, 5],
                        },
                    ],
                },
            }],
        ),
        // The mixed case a real "stage these" gesture produces: several
        // files, different granularity each, diff order preserved.
        patch_plan(
            StageDirection::Stage,
            vec![
                FileSelection {
                    path: "Cargo.toml".to_string(),
                    selection: SelectionShape::EntireFile,
                },
                FileSelection {
                    path: "src/main.rs".to_string(),
                    selection: SelectionShape::Hunks {
                        hunks: vec![hunk(1, 55, 61)],
                    },
                },
                FileSelection {
                    path: "src/planner.rs".to_string(),
                    selection: SelectionShape::Lines {
                        hunks: vec![HunkLines {
                            hunk: hunk(0, 7, 7),
                            lines: vec![2],
                        }],
                    },
                },
            ],
        ),
    ]
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let plans = golden_patch_plans();

    // Deliberate-regeneration path (see module docs): rewrite the fixture
    // from the plans above, then fall through and verify against what was
    // written.
    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&plans).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    // 1. The committed wire form deserializes into exactly these plans…
    let parsed: Vec<PatchPlan> = serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, plans, "fixture and in-code golden plans diverged");

    // 2. …and re-serializing reproduces the committed bytes exactly, so no
    //    field is dropped, defaulted, renamed, or reordered in flight.
    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized patch plans no longer match the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_selection_shape_and_direction() {
    // Count the distinct `select` tags and `direction` strings on the wire.
    // A new SelectionShape variant without a golden selection fails here,
    // keeping fixture and vocabulary in lockstep — the same contract
    // `golden_set_covers_every_operation_variant` holds for `Plan`.
    let plans = golden_patch_plans();
    let mut shapes = std::collections::BTreeSet::new();
    let mut directions = std::collections::BTreeSet::new();
    for p in &plans {
        directions.insert(
            serde_json::to_value(p.direction)
                .unwrap()
                .as_str()
                .expect("direction serializes as a string")
                .to_string(),
        );
        for f in &p.files {
            shapes.insert(
                serde_json::to_value(&f.selection).unwrap()["select"]
                    .as_str()
                    .expect("every selection serializes with a select tag")
                    .to_string(),
            );
        }
    }
    let expected_shapes: std::collections::BTreeSet<String> =
        ["entire_file", "hunks", "lines"].map(String::from).into();
    let expected_directions: std::collections::BTreeSet<String> =
        ["stage", "unstage"].map(String::from).into();
    assert_eq!(shapes, expected_shapes, "selection wire tags changed");
    assert_eq!(
        directions, expected_directions,
        "direction wire strings changed"
    );
}

#[test]
fn every_golden_plan_is_canonically_valid() {
    // The fixture doubles as the canonical-form documentation, so a golden
    // plan that fails structural validation would be teaching the wrong
    // shape.
    for (i, p) in golden_patch_plans().iter().enumerate() {
        assert_eq!(p.validate(), Ok(()), "golden plan {i} is not canonical");
    }
}

// ---------------------------------------------------------------------------
// The staging response DTOs (#213) — same golden discipline, second fixture.
// ---------------------------------------------------------------------------

const RESPONSES_FIXTURE: &str = include_str!("fixtures/staging_responses_v1.json");
const RESPONSES_FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/staging_responses_v1.json"
);

/// One bundle holding each staging response DTO once, so the committed file
/// pins both wire shapes — the `DtoGoldenSet` pattern from `dto_golden.rs`.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StagingResponsesGoldenSet {
    staging_diff: StagingDiff,
    patch_preview: PatchPreview,
}

fn golden_responses() -> StagingResponsesGoldenSet {
    StagingResponsesGoldenSet {
        staging_diff: StagingDiff {
            generation: GenerationToken::new("diff-v1:12345678901234567890").unwrap(),
            patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n context\n+added\n"
                .to_string(),
            truncated: false,
        },
        patch_preview: PatchPreview {
            generation: GenerationToken::new("diff-v1:12345678901234567890").unwrap(),
            patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n context\n+added\n"
                .to_string(),
            whole_files: vec!["assets/logo.png".to_string()],
        },
    }
}

#[test]
fn staging_response_fixture_round_trips_losslessly() {
    let set = golden_responses();
    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&set).unwrap();
        pretty.push('\n');
        std::fs::write(RESPONSES_FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(RESPONSES_FIXTURE_PATH).unwrap()
    } else {
        RESPONSES_FIXTURE.to_string()
    };
    let parsed: StagingResponsesGoldenSet =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, set, "fixture and in-code golden responses diverged");
    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized staging responses no longer match the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}
