//! Golden-fixture tests for the v4 paged-history wire envelopes (M1.10, #63).
//!
//! `tests/fixtures/history_frame_v4.json` and `tests/fixtures/history_page_v4.json`
//! are the **committed** wire forms of one [`HistoryFrame`] and one
//! [`HistoryPage`], each instantiated with `git-vista-core`'s real nested types
//! (`GitRef`, `GraphRow`, `Edge`, `FrameStub`) — through this crate's
//! dev-dependency only, proving the generic envelopes carry those types
//! losslessly without this crate depending on core at build time.
//!
//! Same shape as `plan_golden.rs`: each fixture deserializes into exactly the
//! value built here, and re-serializing reproduces the fixture byte for byte.
//! `HistoryPage` includes `stubs`; `HistoryFrame` does not carry stubs at all
//! (there is no field to omit — see the module docs on `history.rs`).
//!
//! A wire change here is deliberate: regenerate with
//! `REGEN_GOLDEN=1 cargo test -p git-vista-protocol --test history_golden`,
//! review the diff, and record the protocol implications (M1.02 rules).

use git_vista_core::model::{CommitSummary, Edge, FrameStub, GitRef, GraphRow, Oid, RefKind};
use git_vista_protocol::plan::GenerationToken;
use git_vista_protocol::{HeadState, HistoryFrame, HistoryPage};

const FRAME_FIXTURE: &str = include_str!("fixtures/history_frame_v4.json");
const FRAME_FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/history_frame_v4.json"
);
const PAGE_FIXTURE: &str = include_str!("fixtures/history_page_v4.json");
const PAGE_FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/history_page_v4.json"
);

fn generation(digits: &str) -> GenerationToken {
    GenerationToken::new(digits).unwrap()
}

fn golden_frame() -> HistoryFrame<GitRef> {
    HistoryFrame {
        generation: generation("12345678901234567890"),
        refs: vec![
            GitRef {
                name: "HEAD".to_string(),
                kind: RefKind::Head,
                target: Oid("1111111111111111111111111111111111111111".to_string()),
            },
            GitRef {
                name: "main".to_string(),
                kind: RefKind::Branch,
                target: Oid("1111111111111111111111111111111111111111".to_string()),
            },
            GitRef {
                name: "origin/main".to_string(),
                kind: RefKind::RemoteBranch,
                target: Oid("2222222222222222222222222222222222222222".to_string()),
            },
            GitRef {
                name: "v1.0.0".to_string(),
                kind: RefKind::Tag,
                target: Oid("3333333333333333333333333333333333333333".to_string()),
            },
        ],
        head_branch: Some("main".to_string()),
        head_state: HeadState::OnBranch,
        branch_colors: vec![("main".to_string(), 0), ("origin/main".to_string(), 1)],
        repo_label: Some("git-vista-test".to_string()),
        repo_id: Some("repo-abc".to_string()),
        worktree_id: Some("worktree-def".to_string()),
        read_only: false,
        resettable: true,
        repo_url: Some("https://github.com/owner/repo".to_string()),
        remote_web_url: Some("https://github.com/owner/repo".to_string()),
    }
}

fn golden_page() -> HistoryPage<GraphRow, Edge, FrameStub> {
    HistoryPage {
        rows: vec![
            GraphRow {
                commit: CommitSummary {
                    id: Oid("1111111111111111111111111111111111111111".to_string()),
                    parents: vec![Oid("2222222222222222222222222222222222222222".to_string())],
                    summary: "feat: land the thing".to_string(),
                    author: "Ada Lovelace".to_string(),
                    time: 1_753_300_000,
                },
                row: 0,
                lane: 0,
                refs: vec![GitRef {
                    name: "HEAD".to_string(),
                    kind: RefKind::Head,
                    target: Oid("1111111111111111111111111111111111111111".to_string()),
                }],
                color: 0,
                on_remote: false,
            },
            GraphRow {
                commit: CommitSummary {
                    id: Oid("2222222222222222222222222222222222222222".to_string()),
                    parents: vec![],
                    summary: "chore: first commit".to_string(),
                    author: "Ada Lovelace".to_string(),
                    time: 1_753_200_000,
                },
                row: 1,
                lane: 0,
                refs: vec![],
                color: 0,
                on_remote: true,
            },
        ],
        edges: vec![Edge {
            from_row: 0,
            from_lane: 0,
            to_row: 1,
            to_lane: 0,
        }],
        stubs: vec![FrameStub {
            name: "feature/idea".to_string(),
            anchor_commit: Oid("2222222222222222222222222222222222222222".to_string()),
            lane_offset: 0,
            color: 1,
            depth: 0,
        }],
        lane_count: 1,
        cursor: Some("row:2".to_string()),
        generation: generation("12345678901234567890"),
    }
}

#[test]
fn history_frame_v4_golden() {
    let frame = golden_frame();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&frame).unwrap();
        pretty.push('\n');
        std::fs::write(FRAME_FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(FRAME_FIXTURE_PATH).unwrap()
    } else {
        FRAME_FIXTURE.to_string()
    };

    let parsed: HistoryFrame<GitRef> =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, frame, "fixture and in-code golden frame diverged");

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized frame no longer matches the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );

    // Frame carries no stubs field at all.
    let value: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    assert!(
        value.as_object().unwrap().get("stubs").is_none(),
        "HistoryFrame must never carry stubs"
    );
}

#[test]
fn history_page_v4_golden() {
    let page = golden_page();

    if std::env::var("REGEN_GOLDEN").is_ok() {
        let mut pretty = serde_json::to_string_pretty(&page).unwrap();
        pretty.push('\n');
        std::fs::write(PAGE_FIXTURE_PATH, &pretty).unwrap();
    }
    let fixture = if std::env::var("REGEN_GOLDEN").is_ok() {
        std::fs::read_to_string(PAGE_FIXTURE_PATH).unwrap()
    } else {
        PAGE_FIXTURE.to_string()
    };

    let parsed: HistoryPage<GraphRow, Edge, FrameStub> =
        serde_json::from_str(&fixture).expect("fixture must deserialize");
    assert_eq!(parsed, page, "fixture and in-code golden page diverged");

    let mut reserialized = serde_json::to_string_pretty(&parsed).unwrap();
    reserialized.push('\n');
    assert_eq!(
        reserialized, fixture,
        "re-serialized page no longer matches the committed fixture — \
         if this wire change is deliberate, regenerate with REGEN_GOLDEN=1 \
         and review the diff"
    );

    // Page must carry stubs.
    let value: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    assert!(
        value.as_object().unwrap().get("stubs").is_some(),
        "HistoryPage must carry stubs"
    );
}
