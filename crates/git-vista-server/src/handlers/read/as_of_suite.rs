//! Real-repository evidence that captured state enters the existing layout
//! engine, never a live-ref substitute or a second graph implementation.
use super::*;
use crate::activity::{activity_page_for_target, ActivityParams};
use crate::{as_of, journal};
use git_vista_core::activity::{
    ActivityEvent, ActivityKind, ActivitySource, HeadAtEvent, RefsAtEvent,
};
use git_vista_protocol::AsOfAvailability;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn record(repo: &Path, name: &str) {
    journal::append(
        repo,
        &ActivityEvent {
            time: 4_000_000_000,
            kind: ActivityKind::Other,
            ref_name: None,
            summary: name.into(),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        },
    );
}

fn selector(target: &ResolvedHistoryTarget, codec: &CursorCodec, name: &str) -> String {
    let page = activity_page_for_target(
        &target.path,
        target.read_only,
        target.scope,
        &ActivityParams {
            limit: Some(100),
            cursor: None,
        },
        codec,
    )
    .unwrap();
    let row = page.events.iter().find(|e| e.summary == name).unwrap();
    match &row.as_of {
        AsOfAvailability::Available { token } => token.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

async fn body<T: serde::de::DeserializeOwned>(response: Response) -> T {
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn target(path: &Path, codec: &CursorCodec) -> ResolvedHistoryTarget {
    let path = path.canonicalize().unwrap();
    let scope = codec.scope_for_target(None, &path);
    ResolvedHistoryTarget {
        path,
        scope,
        read_only: false,
        handle: None,
    }
}

#[tokio::test]
async fn as_of_frame_and_every_page_equal_the_original_graph_after_live_refs_move() {
    let (_dir, repo) = git_vista_fixtures::seeded_files(&[("f", "base\n")], "base");
    let root = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "side tip"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "main tip"]);
    git(&repo, &["merge", "-q", "--no-ff", "side", "-m", "join"]);
    git(&repo, &["tag", "main", &root]); // same short name, different namespace
    git(&repo, &["update-ref", "refs/remotes/origin/main", &root]);
    let old_tip = git(&repo, &["rev-parse", "HEAD"]);
    let codec = CursorCodec::with_key([0x36; 32]);
    let target = target(&repo, &codec);
    let headers = HeaderMap::new();
    let walks = AtomicUsize::new(0);
    let expected_frame: Frame = body(frame_for_target(&target, &headers).await.unwrap()).await;
    let expected: Page = body(
        page_for_target(&target, None, 100, &codec, &headers, &walks)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(expected.rows.len(), 4);
    assert_eq!(expected.rows.iter().filter(|r| r.on_remote).count(), 1);
    record(&repo, "original observation");
    // Move every kind of live ref, including HEAD. Token is minted after the
    // moves, so any 409 here is the erroneous live drift check, not a stale fold.
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "new live tip"],
    );
    git(&repo, &["branch", "-D", "side"]);
    git(&repo, &["tag", "-d", "main"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&repo, &["checkout", "-q", "--detach", "HEAD"]);
    let token = selector(&target, &codec, "original observation");
    let snapshot = as_of::redeem(&repo, false, target.scope, &token, &codec).unwrap();
    let frame: Frame = body(
        frame_from_snapshot(&target, snapshot, &headers)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(frame.refs, expected_frame.refs);
    assert_eq!(frame.head_branch, expected_frame.head_branch);
    assert_eq!(frame.head_state, expected_frame.head_state);
    assert_eq!(frame.branch_colors, expected_frame.branch_colors);
    assert!(frame.read_only);
    assert!(!frame.resettable);
    assert_ne!(frame.generation, expected_frame.generation);
    let mut rows = Vec::new();
    let mut edges = Vec::new();
    let mut stubs = Vec::new();
    let mut cursor = None;
    for _ in 0..10 {
        let snapshot = as_of::redeem(&repo, false, target.scope, &token, &codec).unwrap();
        let page: Page = body(
            page_from_snapshot(
                &target,
                snapshot,
                cursor.as_deref(),
                1,
                &codec,
                &headers,
                &walks,
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(page.generation, frame.generation);
        rows.extend(page.rows);
        edges.extend(page.edges);
        stubs.extend(page.stubs);
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert_eq!(rows, expected.rows);
    // Edge and stub emission order follows page ownership; compare canonical
    // sets while preserving every field, not only endpoints/counts.
    edges.sort_by_key(|e| format!("{e:?}"));
    let mut expected_edges = expected.edges;
    expected_edges.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(edges, expected_edges);
    stubs.sort_by_key(|s| format!("{s:?}"));
    let mut expected_stubs = expected.stubs;
    expected_stubs.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(stubs, expected_stubs);
    assert_eq!(rows[0].commit.id.0, old_tip);
}

#[tokio::test]
async fn as_of_pages_cannot_spend_live_or_other_observation_cursors() {
    let (_dir, repo) = git_vista_fixtures::seeded_files(&[("f", "base\n")], "base");
    let codec = CursorCodec::with_key([0x36; 32]);
    let target = target(&repo, &codec);
    record(&repo, "first");
    record(&repo, "second");
    let first = selector(&target, &codec, "first");
    let second = selector(&target, &codec, "second");
    let headers = HeaderMap::new();
    let walks = AtomicUsize::new(0);
    let live: Page = body(
        page_for_target(&target, None, 1, &codec, &headers, &walks)
            .await
            .unwrap(),
    )
    .await;
    let first_page: Page = body(
        page_from_snapshot(
            &target,
            as_of::redeem(&repo, false, target.scope, &first, &codec).unwrap(),
            None,
            1,
            &codec,
            &headers,
            &walks,
        )
        .await
        .unwrap(),
    )
    .await;
    for cursor in [live.cursor.as_deref(), first_page.cursor.as_deref()] {
        let before = walks.load(Ordering::Relaxed);
        let error = page_from_snapshot(
            &target,
            as_of::redeem(&repo, false, target.scope, &second, &codec).unwrap(),
            cursor,
            1,
            &codec,
            &headers,
            &walks,
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            walks.load(Ordering::Relaxed),
            before,
            "foreign-generation cursor reached the walk"
        );
    }
    assert_eq!(
        page_for_target(
            &target,
            first_page.cursor.as_deref(),
            1,
            &codec,
            &headers,
            &walks
        )
        .await
        .unwrap_err()
        .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn as_of_lost_object_and_mid_read_fold_change_are_explicit_failures() {
    let (_dir, repo) = git_vista_fixtures::seeded_files(&[("f", "base\n")], "base");
    let dangling = git(
        &repo,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            "HEAD",
            "-m",
            "unreachable observation",
        ],
    );
    let mut capture = journal::capture_refs(&repo);
    if let RefsAtEvent::Captured { head, .. } = &mut capture {
        *head = Some(HeadAtEvent::Detached {
            oid: dangling.clone(),
        });
    }
    journal::append(
        &repo,
        &ActivityEvent {
            time: 4_000_000_000,
            kind: ActivityKind::Other,
            ref_name: None,
            summary: "dangling".into(),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: Some(capture),
        },
    );
    let codec = CursorCodec::with_key([0x36; 32]);
    let target = target(&repo, &codec);
    let token = selector(&target, &codec, "dangling");
    let snapshot = as_of::redeem(&repo, false, target.scope, &token, &codec).unwrap();
    record(&repo, "arrived during read");
    let error = frame_from_snapshot(&target, snapshot, &HeaderMap::new())
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::CONFLICT);
    let token = selector(&target, &codec, "dangling");
    std::fs::remove_file(
        repo.join(".git/objects")
            .join(&dangling[..2])
            .join(&dangling[2..]),
    )
    .unwrap();
    let error = as_of::redeem(&repo, false, target.scope, &token, &codec).unwrap_err();
    assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(error.1.contains("commit object"));
}
