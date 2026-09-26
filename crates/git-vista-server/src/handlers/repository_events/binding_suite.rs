//! Real HTTP extraction and SSE bodies. Cases run in fresh processes because
//! stream admission is process-wide and other suites deliberately exhaust it.

use super::*;
use std::future::Future;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request};
use axum::routing::get;
use axum::Router;
use git_vista_core::identity::{RepositoryHandle, WorktreeId};
use git_vista_protocol::{
    CommitMessage, GitOperation, IdempotencyKey, OperationHash, RepoMode, RepositoryToken,
    WorktreeToken, PROTOCOL_HEADER, PROTOCOL_QUERY, PROTOCOL_VERSION,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[track_caller]
fn isolated(name: &str, case: impl Future<Output = ()>) {
    let full_name = format!("handlers::repository_events::binding_suite::{name}");
    if std::env::var("GV_BOUND_FEED_CASE").as_deref() == Ok(full_name.as_str()) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::state::with_isolated_test_current(case));
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([full_name.as_str(), "--exact", "--nocapture"])
        .env("GV_BOUND_FEED_CASE", &full_name)
        .output()
        .expect("run isolated handler test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "{full_name} must execute and pass, not match zero tests:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn repository() -> (tempfile::TempDir, RepositoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Feed Test"]);
    git(
        dir.path(),
        &["config", "user.email", "feed@example.invalid"],
    );
    git(dir.path(), &["commit", "-qm", "base", "--allow-empty"]);
    let handle = crate::state::set_current(dir.path(), RepoMode::Active)
        .expect("fixture must be registered");
    (dir, handle)
}

fn routes() -> Router {
    Router::new()
        .route("/api/repository/events", get(repository_events))
        .route(
            "/api/operations/{id}/events",
            get(crate::handlers::operations::operation_events),
        )
}

async fn request(uri: &str) -> Response {
    routes()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn feed_uri(handle: RepositoryHandle) -> String {
    format!("/api/repository/events?repo={}", handle.worktree)
}

async fn snapshot(body: &mut Body, within: Duration) -> Option<ChangeFeedSnapshot> {
    tokio::time::timeout(within, async {
        let mut pending = String::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.expect("SSE body error");
            let Ok(data) = frame.into_data() else {
                continue;
            };
            pending.push_str(std::str::from_utf8(&data).unwrap());
            while let Some(end) = pending.find("\n\n") {
                let event: String = pending.drain(..end + 2).collect();
                let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) else {
                    continue;
                };
                assert!(event.contains(SNAPSHOT_EVENT), "unchanged SSE event name");
                let snapshot: ChangeFeedSnapshot = serde_json::from_str(data).unwrap();
                if snapshot.generation.is_some() {
                    return Some(snapshot);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

async fn live_token(repo: &Path) -> git_vista_protocol::GenerationToken {
    let reading = crate::planner::live_reading(repo).await;
    assert!(
        reading.blind.is_none(),
        "fixture unreadable: {:?}",
        reading.blind
    );
    reading.token
}

fn held_permits(count: usize) -> Vec<operations::StreamPermit> {
    (0..count)
        .map(|_| operations::StreamPermit::acquire().expect("isolated pool has capacity"))
        .collect()
}

/// Enter the body without waiting for git. Leaking a permit inside the stream
/// generator is invisible to a test that only drops an unpolled response.
async fn poll_once(body: &mut Body) {
    std::future::poll_fn(|cx| {
        let _ = http_body::Body::poll_frame(std::pin::Pin::new(&mut *body), cx);
        std::task::Poll::Ready(())
    })
    .await;
}

fn operation_uri() -> String {
    let key = IdempotencyKey::new("bound-feed-permit-fixture").unwrap();
    let op = GitOperation::CommitOnHead {
        message: CommitMessage::new("permit fixture").unwrap(),
        allow_empty: true,
    };
    let operations::Admission::Fresh(handle, record) = operations::admit(
        &key,
        &op,
        &OperationHash::new("a".repeat(64)).unwrap(),
        RepositoryToken::new("test-repo").unwrap(),
        WorktreeToken::new("test-worktree").unwrap(),
        None,
    ) else {
        panic!("fresh operation fixture");
    };
    handle.finish(StatusCode::OK, "done".into(), None);
    format!("/api/operations/{}/events", record.id().as_str())
}

#[test]
fn explicit_feed_stays_on_x_and_does_not_publish_y() {
    isolated("explicit_feed_stays_on_x_and_does_not_publish_y", async {
        let (x, hx) = repository();
        let (y, hy) = repository();
        assert!(crate::state::select_registered(
            hx.worktree,
            RepoMode::Active
        ));
        let response = request(&feed_uri(hx)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let first = snapshot(&mut body, Duration::from_secs(10))
            .await
            .expect("initial X reading");
        assert_eq!(first.generation, Some(live_token(x.path()).await));
        assert!(crate::state::select_registered(
            hy.worktree,
            RepoMode::Active
        ));
        git(y.path(), &["tag", "y-only"]);
        let y_response = request(&feed_uri(hy)).await;
        assert_eq!(y_response.status(), StatusCode::OK);
        let mut y_body = y_response.into_body();
        let y_snapshot = snapshot(&mut y_body, Duration::from_secs(10))
            .await
            .expect("Y really published after selection");
        assert_ne!(first.generation, y_snapshot.generation);
        // Poll through a selection-check interval, including on a quiet X.
        if let Some(quiet) = snapshot(&mut body, SELECTION_CHECK * 2).await {
            assert_eq!(quiet.generation, first.generation, "X must not emit Y");
        }
        git(x.path(), &["tag", "x-after-selection"]);
        reconciliation::publish_after_write(x.path()).await;
        let expected = live_token(x.path()).await;
        let after = snapshot(&mut body, Duration::from_secs(10))
            .await
            .expect("bound X must stay open and publish X after session selects Y");
        assert_eq!(after.generation, Some(expected), "connection stays on X");
        assert_ne!(after.generation, y_snapshot.generation);
        drop(body);
        let reconnect = request(&feed_uri(hx)).await;
        assert_eq!(reconnect.status(), StatusCode::OK);
        let again = snapshot(&mut reconnect.into_body(), Duration::from_secs(10))
            .await
            .expect("bound reconnect reading");
        assert_eq!(
            again.generation, after.generation,
            "reconnect still resolves X"
        );
    });
}

#[test]
fn legacy_feed_captures_handler_scope_and_closes_on_selection_change() {
    isolated(
        "legacy_feed_captures_handler_scope_and_closes_on_selection_change",
        async {
            let (x, hx) = repository();
            let (_y, hy) = repository();
            let cell = crate::state::new_selection_cell();
            let response = crate::state::with_selection(Arc::clone(&cell), async {
                assert!(crate::state::select_registered(
                    hx.worktree,
                    RepoMode::Active
                ));
                request("/api/repository/events").await
            })
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            // Poll outside the handler's scope, where the ambient selection is Y.
            let mut body = response.into_body();
            let first = snapshot(&mut body, Duration::from_secs(10))
                .await
                .expect("legacy stream must retain the handler's selection cell");
            assert_eq!(first.generation, Some(live_token(x.path()).await));
            crate::state::with_selection(cell, async {
                assert!(crate::state::select_registered(
                    hy.worktree,
                    RepoMode::Active
                ));
            })
            .await;
            let ended = tokio::time::timeout(SELECTION_CHECK * 3, async {
                while let Some(frame) = body.frame().await {
                    frame.unwrap();
                }
            })
            .await;
            assert!(
                ended.is_ok(),
                "quiet legacy stream must close on its session change"
            );
        },
    );
}

#[test]
fn malformed_and_empty_selectors_fail_before_stream_admission() {
    isolated(
        "malformed_and_empty_selectors_fail_before_stream_admission",
        async {
            let (_repo, _) = repository();
            let _held = held_permits(operations::MAX_LIVE_STREAMS);
            for selector in ["", "not-an-id", "%2Ftmp%2Frepository"] {
                let response = request(&format!("/api/repository/events?repo={selector}")).await;
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "invalid explicit selector must fail before permit admission"
                );
            }
        },
    );
}

#[test]
fn unknown_selector_never_falls_back_including_on_reconnect() {
    isolated(
        "unknown_selector_never_falls_back_including_on_reconnect",
        async {
            let (_repo, _) = repository();
            let unknown = WorktreeId::from_git_dir("/unregistered-feed-fixture/.git");
            let uri = format!("/api/repository/events?repo={unknown}");
            for _ in 0..2 {
                let response = request(&uri).await;
                assert_eq!(
                    response.status(),
                    StatusCode::NOT_FOUND,
                    "unregistered selector must never reconnect onto the selected default"
                );
            }
        },
    );
}

#[test]
fn exhausted_pool_refuses_feed() {
    isolated("exhausted_pool_refuses_feed", async {
        let (_repo, handle) = repository();
        let _held = held_permits(operations::MAX_LIVE_STREAMS);
        let response = request(&feed_uri(handle)).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "feed endpoint must enforce the shared cap"
        );
    });
}

#[test]
fn exhausted_pool_refuses_operation_progress() {
    isolated("exhausted_pool_refuses_operation_progress", async {
        let uri = operation_uri();
        let _held = held_permits(operations::MAX_LIVE_STREAMS);
        let response = request(&uri).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "progress endpoint must enforce the shared cap"
        );
    });
}

#[test]
fn dropping_feed_restores_capacity_for_progress() {
    isolated("dropping_feed_restores_capacity_for_progress", async {
        let (_repo, handle) = repository();
        let uri = operation_uri();
        let _held = held_permits(operations::MAX_LIVE_STREAMS - 1);
        let feed = request(&feed_uri(handle)).await;
        assert_eq!(feed.status(), StatusCode::OK);
        let mut body = feed.into_body();
        poll_once(&mut body).await;
        assert_eq!(
            request(&uri).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(body);
        assert_eq!(
            request(&uri).await.status(),
            StatusCode::OK,
            "disconnecting a feed must restore shared capacity for progress"
        );
    });
}

#[test]
fn dropping_progress_restores_capacity_for_feed() {
    isolated("dropping_progress_restores_capacity_for_feed", async {
        let (_repo, handle) = repository();
        let uri = operation_uri();
        let _held = held_permits(operations::MAX_LIVE_STREAMS - 1);
        let progress = request(&uri).await;
        assert_eq!(progress.status(), StatusCode::OK);
        let mut body = progress.into_body();
        poll_once(&mut body).await;
        assert_eq!(
            request(&feed_uri(handle)).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(body);
        assert_eq!(
            request(&feed_uri(handle)).await.status(),
            StatusCode::OK,
            "disconnecting progress must restore shared capacity for a feed"
        );
    });
}

#[test]
fn dropping_unpolled_feed_also_restores_capacity() {
    isolated("dropping_unpolled_feed_also_restores_capacity", async {
        let (_repo, handle) = repository();
        let _held = held_permits(operations::MAX_LIVE_STREAMS - 1);
        let response = request(&feed_uri(handle)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(operations::StreamPermit::acquire().is_none());
        drop(response);
        assert!(
            operations::StreamPermit::acquire().is_some(),
            "disconnect before first poll must release the captured permit"
        );
    });
}

#[test]
fn registered_route_keeps_auth_and_eventsource_protocol_requirements() {
    isolated(
        "registered_route_keeps_auth_and_eventsource_protocol_requirements",
        async {
            let sessions = Arc::new(crate::session::SessionManager::new(None));
            let token = sessions.current_bootstrap();
            let app = crate::api_router(
                crate::handlers::session::SessionState {
                    manager: sessions,
                    via_lan: false,
                    rate_limiter: None,
                },
                crate::security::HostPolicy::loopback(crate::state::PORT),
                true,
                Arc::new(crate::history::CursorCodec::new()),
                Arc::new(crate::token_store::RequestTokenResolver::without_keyring()),
            );
            let request = |uri: &str| {
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "localhost:8080")
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        55000,
                    ))))
            };
            let uri =
                format!("/api/repository/events?repo=bad&{PROTOCOL_QUERY}={PROTOCOL_VERSION}");
            let unauthenticated = app
                .clone()
                .oneshot(request(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
            let session = app
                .clone()
                .oneshot(
                    request("/api/session")
                        .method("POST")
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(session.status(), StatusCode::OK);
            let cookie = session.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let missing_protocol = app
                .clone()
                .oneshot(
                    request("/api/repository/events?repo=bad")
                        .header(header::COOKIE, &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(missing_protocol.status(), StatusCode::UPGRADE_REQUIRED);
            let extracted = app
                .oneshot(
                    request(&uri)
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                extracted.status(),
                StatusCode::BAD_REQUEST,
                "authenticated EventSource query must reach fail-closed selector resolution"
            );
        },
    );
}
