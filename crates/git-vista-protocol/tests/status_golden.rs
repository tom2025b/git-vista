//! Golden-fixture test for the [`WorktreeStatus`] wire contract (M2.15, #68a).
//!
//! `tests/fixtures/status_v1.json` is the **committed** wire form of one
//! [`WorktreeStatus`], its `entries` covering every [`StatusEntry`] variant,
//! every [`ChangeSides`] shape, every [`ConflictKind`], and both a present and
//! an absent [`SubmoduleState`]. Same shape as `plan_golden.rs`: the fixture
//! deserializes into exactly the value built here, and re-serializing
//! reproduces the fixture byte for byte.
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test status_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).
//!
//! No git process is spawned and no repository is read anywhere in this file
//! — every value is hand-built, exactly like `plan_golden.rs`'s `Plan`
//! values. The real `git status --porcelain=v2 -z` parser that populates a
//! `WorktreeStatus` from an actual repository is #68b, not this task.

use git_vista_protocol::{
    ChangeKind, ChangeSides, ConflictKind, GenerationToken, StatusEntry, SubmoduleState,
    WorktreeStatus,
};

const FIXTURE: &str = include_str!("fixtures/status_v1.json");
const FIXTURE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status_v1.json");

fn generation(s: &str) -> GenerationToken {
    GenerationToken::new(s).unwrap()
}

fn dirty_submodule() -> SubmoduleState {
    SubmoduleState {
        commit_changed: false,
        has_tracked_changes: true,
        has_untracked_changes: true,
    }
}

/// One [`WorktreeStatus`], its `entries` deliberately covering every variant
/// and sub-shape this DTO can carry — the fixture this test pins.
fn golden_status() -> WorktreeStatus {
    WorktreeStatus {
        generation: generation("status-v1:12345678901234567890"),
        branch: Some("main".to_string()),
        upstream: Some("origin/main".to_string()),
        ahead: 2,
        behind: 1,
        entries: vec![
            // Changed, staged only.
            StatusEntry::Changed {
                path: "staged-only.rs".to_string(),
                sides: ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
                submodule: None,
                binary: false,
            },
            // Changed, unstaged only.
            StatusEntry::Changed {
                path: "unstaged-only.rs".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
                submodule: None,
                binary: false,
            },
            // Changed, both sides — the same path dirty twice over.
            StatusEntry::Changed {
                path: "both-sides.rs".to_string(),
                sides: ChangeSides::Both {
                    staged: ChangeKind::Added,
                    unstaged: ChangeKind::Modified,
                },
                submodule: None,
                binary: false,
            },
            // Changed, deleted, and binary — a blob change with no text diff.
            StatusEntry::Changed {
                path: "deleted.bin".to_string(),
                sides: ChangeSides::StagedOnly {
                    staged: ChangeKind::Deleted,
                },
                submodule: None,
                binary: true,
            },
            // A submodule, dirty without its recorded commit having changed.
            StatusEntry::Changed {
                path: "vendor/lib".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
                submodule: Some(dirty_submodule()),
                binary: false,
            },
            // Renamed, required origin_path, with a similarity score.
            StatusEntry::Renamed {
                path: "new/name.rs".to_string(),
                origin_path: "old/name.rs".to_string(),
                score: 100,
                sides: ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                },
                submodule: None,
                binary: false,
            },
            // Untracked, text.
            StatusEntry::Untracked {
                path: "scratch.txt".to_string(),
                binary: false,
            },
            // Untracked, binary.
            StatusEntry::Untracked {
                path: "scratch.bin".to_string(),
                binary: true,
            },
            // Ignored.
            StatusEntry::Ignored {
                path: "target/".to_string(),
            },
            // Every conflict kind, one entry each.
            StatusEntry::Conflicted {
                path: "both-deleted.rs".to_string(),
                kind: ConflictKind::BothDeleted,
                submodule: None,
            },
            StatusEntry::Conflicted {
                path: "added-by-us.rs".to_string(),
                kind: ConflictKind::AddedByUs,
                submodule: None,
            },
            StatusEntry::Conflicted {
                path: "deleted-by-them.rs".to_string(),
                kind: ConflictKind::DeletedByThem,
                submodule: None,
            },
            StatusEntry::Conflicted {
                path: "added-by-them.rs".to_string(),
                kind: ConflictKind::AddedByThem,
                submodule: None,
            },
            StatusEntry::Conflicted {
                path: "deleted-by-us.rs".to_string(),
                kind: ConflictKind::DeletedByUs,
                submodule: None,
            },
            StatusEntry::Conflicted {
                path: "both-added.rs".to_string(),
                kind: ConflictKind::BothAdded,
                submodule: None,
            },
            // Both-modified, and a submodule at the same time — a conflicted
            // submodule pointer is a real git state.
            StatusEntry::Conflicted {
                path: "both-modified-submodule".to_string(),
                kind: ConflictKind::BothModified,
                submodule: Some(dirty_submodule()),
            },
        ],
    }
}

#[test]
fn golden_fixture_round_trips_losslessly() {
    let status = golden_status();

    // Deliberate-regeneration path (see module docs): rewrite the fixture
    // from the value above, then fall through and verify against what was
    // written.
    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&status).unwrap();
        pretty.push('\n');
        std::fs::write(FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FIXTURE_PATH).unwrap()
    } else {
        FIXTURE.to_string()
    };

    // 1. The committed wire form deserializes into exactly this value…
    let parsed: WorktreeStatus = serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, status, "fixture and in-code golden status diverged");

    // 2. …and re-serializing reproduces the committed bytes exactly, so no
    //    field is dropped, defaulted, renamed, or reordered in flight.
    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized status no longer matches the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );
}

#[test]
fn golden_set_covers_every_entry_kind_and_conflict_kind() {
    let status = golden_status();

    let entry_kinds: std::collections::BTreeSet<String> = status
        .entries
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["entry_kind"]
                .as_str()
                .expect("every entry serializes with an entry_kind tag")
                .to_string()
        })
        .collect();
    let expected_entry_kinds: std::collections::BTreeSet<String> =
        ["changed", "renamed", "untracked", "ignored", "conflicted"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(
        entry_kinds, expected_entry_kinds,
        "an entry_kind variant is missing from (or extra in) the golden set"
    );

    let conflict_kinds: std::collections::BTreeSet<String> = status
        .entries
        .iter()
        .filter_map(|e| match e {
            StatusEntry::Conflicted { kind, .. } => Some(
                serde_json::to_value(kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            _ => None,
        })
        .collect();
    let expected_conflict_kinds: std::collections::BTreeSet<String> = [
        "both_deleted",
        "added_by_us",
        "deleted_by_them",
        "added_by_them",
        "deleted_by_us",
        "both_added",
        "both_modified",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        conflict_kinds, expected_conflict_kinds,
        "a ConflictKind variant is missing from (or extra in) the golden set"
    );

    // The three ChangeSides shapes and both a present and an absent
    // SubmoduleState are exercised too — pinned structurally, not by tag
    // string, since ChangeSides is internally tagged on "side" the same way.
    let sides_shapes: std::collections::BTreeSet<String> = status
        .entries
        .iter()
        .filter_map(|e| match e {
            StatusEntry::Changed { sides, .. } | StatusEntry::Renamed { sides, .. } => Some(
                serde_json::to_value(sides).unwrap()["side"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        sides_shapes,
        ["staged_only", "unstaged_only", "both"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<String>>(),
        "a ChangeSides shape is missing from the golden set"
    );
    assert!(
        status.entries.iter().any(|e| matches!(
            e,
            StatusEntry::Changed {
                submodule: Some(_),
                ..
            } | StatusEntry::Conflicted {
                submodule: Some(_),
                ..
            }
        )),
        "no entry with a present SubmoduleState in the golden set"
    );
    assert!(
        status.entries.iter().any(|e| matches!(
            e,
            StatusEntry::Changed {
                submodule: None,
                ..
            } | StatusEntry::Renamed {
                submodule: None,
                ..
            }
        )),
        "no entry with an absent SubmoduleState in the golden set"
    );
}
