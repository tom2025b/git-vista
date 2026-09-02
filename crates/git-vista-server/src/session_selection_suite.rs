//! #588: the selected repository must belong to the **session**, not the process.
//!
//! # The defect this suite pins
//!
//! `state::CURRENT` is one `OnceLock<RwLock<Current>>` for the whole server, so
//! the selection is process-global. Two consequences, both observable from
//! outside the server:
//!
//! - a fresh session inherits whatever repository the previous one had picked;
//! - two live sessions overwrite each other's selection while both are open —
//!   and the bootstrap token explicitly supports a second device bootstrapping.
//!
//! # Why these tests drive the real router
//!
//! Asserting on `state::` internals would risk proving the mapping by calling
//! the function that defines it. Here two fixture repositories carry different
//! seed commit messages (`alpha seed` / `beta seed`), so "session A never sees
//! session B's repository" is decided by the bytes a read handler actually
//! returns to that session's cookie.

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::Router;
use git_vista_fixtures::seeded_files;
use git_vista_protocol::{RepoMode, PROTOCOL_HEADER, PROTOCOL_VERSION};
use tower::ServiceExt;

use crate::history::CursorCodec;
use crate::session::SessionManager;
use crate::state;

const HOST: &str = "localhost:8080";

/// A deterministic cursor codec, so nothing here depends on the per-process
/// random key. Same shape as `handlers::read::routing_suite`'s.
fn codec() -> CursorCodec {
    CursorCodec::with_key([0x27; 32])
}

fn router_and_manager() -> (Router, Arc<SessionManager>) {
    let sessions = Arc::new(SessionManager::new(None));
    let session_state = crate::handlers::session::SessionState {
        manager: Arc::clone(&sessions),
        via_lan: false,
        rate_limiter: None,
    };
    let hosts = crate::security::HostPolicy::loopback(state::PORT);
    let router = crate::api_router(session_state, hosts, true, Arc::new(codec()));
    (router, sessions)
}

/// One live session: its cookie header value and its CSRF token.
struct Client {
    cookie: String,
    csrf: String,
}

/// Bootstrap a session against `router`. The bootstrap token is single-use and
/// self-replacing, so calling this twice yields two independent sessions —
/// which is exactly the "a second device can bootstrap" path #588 is about.
async fn sign_in(router: &Router, sessions: &SessionManager) -> Client {
    let token = sessions.current_bootstrap();
    let resp = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/session")
                .header(header::HOST, HOST)
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                .header(header::CONTENT_TYPE, "application/json")
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    55000,
                ))))
                .body(axum::body::Body::from(format!(r#"{{"token":"{token}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let csrf = info["csrf"].as_str().expect("a csrf token").to_string();
    Client { cookie, csrf }
}

/// `POST /api/select` as `client`.
async fn select(router: &Router, client: &Client, worktree: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/select")
                .header(header::HOST, HOST)
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, client.cookie.clone())
                .header(git_vista_protocol::CSRF_HEADER, client.csrf.clone())
                .body(axum::body::Body::from(format!(
                    r#"{{"worktree":"{worktree}","mode":"active"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// `GET /api/commits` as `client`, returned as raw text so the assertion can
/// look for a seed commit's message without depending on the page's shape.
async fn commits_body(router: &Router, client: &Client) -> String {
    let resp = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/commits")
                .header(header::HOST, HOST)
                .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                .header(header::COOKIE, client.cookie.clone())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the paged history read should succeed for a signed-in session"
    );
    let body = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

/// **The invariant.** Two sessions, two different selected repositories,
/// neither sees the other's.
///
/// Both repositories are registered first, so the catalog holds each one and
/// the only thing under test is which of them a given *session* resolves to.
/// The second registration also leaves the process-global selection pointing
/// at `beta`, which is precisely the leftover a fresh session must not inherit.
#[tokio::test]
async fn two_sessions_hold_different_selected_repositories() {
    state::with_isolated_test_current(async {
        let (_dir_a, repo_a) = seeded_files(&[("a.txt", "a\n")], "alpha seed");
        let (_dir_b, repo_b) = seeded_files(&[("b.txt", "b\n")], "beta seed");

        let handle_a = state::set_current(&repo_a, RepoMode::Active).expect("alpha registers");
        let handle_b = state::set_current(&repo_b, RepoMode::Active).expect("beta registers");

        let (router, sessions) = router_and_manager();
        let alpha = sign_in(&router, &sessions).await;
        let beta = sign_in(&router, &sessions).await;

        assert_eq!(
            select(&router, &alpha, &handle_a.worktree.to_string()).await,
            StatusCode::OK
        );
        assert_eq!(
            select(&router, &beta, &handle_b.worktree.to_string()).await,
            StatusCode::OK
        );

        let alpha_sees = commits_body(&router, &alpha).await;
        let beta_sees = commits_body(&router, &beta).await;

        assert!(
            alpha_sees.contains("alpha seed"),
            "the alpha session must still be looking at its own repository:\n{alpha_sees}"
        );
        assert!(
            !alpha_sees.contains("beta seed"),
            "the alpha session is seeing the beta session's repository — the selection is \
             process-global:\n{alpha_sees}"
        );
        assert!(
            beta_sees.contains("beta seed"),
            "the beta session must be looking at its own repository:\n{beta_sees}"
        );
        assert!(
            !beta_sees.contains("alpha seed"),
            "the beta session is seeing the alpha session's repository:\n{beta_sees}"
        );
    })
    .await;
}

/// **Acceptance criterion 2.** A session that has chosen nothing resolves to
/// the *launch* repository — a defined place — not to whatever the previous
/// session left behind.
///
/// The setup is the leftover itself: `beta` is registered second, so the
/// process-wide default at sign-in time is beta. The alpha session then picks
/// alpha. A *third*, brand-new session must still land on the launch default
/// and must not inherit alpha's pick.
#[tokio::test]
async fn a_fresh_session_starts_at_the_launch_repository_not_the_previous_pick() {
    state::with_isolated_test_current(async {
        let (_dir_a, repo_a) = seeded_files(&[("a.txt", "a\n")], "alpha seed");
        let (_dir_l, repo_launch) = seeded_files(&[("l.txt", "l\n")], "launch seed");

        let handle_a = state::set_current(&repo_a, RepoMode::Active).expect("alpha registers");
        // Registered last, so this is the standing default every fresh session
        // seeds from — the stand-in for `main`'s startup selection.
        state::set_current(&repo_launch, RepoMode::Active).expect("launch registers");

        let (router, sessions) = router_and_manager();
        let first = sign_in(&router, &sessions).await;
        assert_eq!(
            select(&router, &first, &handle_a.worktree.to_string()).await,
            StatusCode::OK
        );
        assert!(commits_body(&router, &first).await.contains("alpha seed"));

        let newcomer = sign_in(&router, &sessions).await;
        let sees = commits_body(&router, &newcomer).await;
        assert!(
            sees.contains("launch seed"),
            "a fresh session must start at the launch repository:\n{sees}"
        );
        assert!(
            !sees.contains("alpha seed"),
            "a fresh session inherited the previous session's selection:\n{sees}"
        );
    })
    .await;
}

/// **Acceptance criterion 3.** Signing out leaves no selection behind.
///
/// The selection cell hangs off the session record, so `revoke` drops it. This
/// pins that: after a sign-out, a new session on the same server is back at the
/// launch repository rather than at the departed session's choice.
#[tokio::test]
async fn signing_out_leaves_no_selection_for_the_next_session() {
    state::with_isolated_test_current(async {
        let (_dir_a, repo_a) = seeded_files(&[("a.txt", "a\n")], "alpha seed");
        let (_dir_l, repo_launch) = seeded_files(&[("l.txt", "l\n")], "launch seed");

        let handle_a = state::set_current(&repo_a, RepoMode::Active).expect("alpha registers");
        state::set_current(&repo_launch, RepoMode::Active).expect("launch registers");

        let (router, sessions) = router_and_manager();
        let leaver = sign_in(&router, &sessions).await;
        assert_eq!(
            select(&router, &leaver, &handle_a.worktree.to_string()).await,
            StatusCode::OK
        );
        assert!(commits_body(&router, &leaver).await.contains("alpha seed"));

        let id = leaver
            .cookie
            .split_once('=')
            .expect("a cookie name=value")
            .1
            .to_string();
        assert!(sessions.revoke(&id), "the session was live before sign-out");

        let next = sign_in(&router, &sessions).await;
        let sees = commits_body(&router, &next).await;
        assert!(
            sees.contains("launch seed"),
            "after a sign-out the next session must start at the launch repository:\n{sees}"
        );
        assert!(
            !sees.contains("alpha seed"),
            "the departed session's selection outlived its session:\n{sees}"
        );
    })
    .await;
}

/// #614: a selection written inside a spawned task is visible to the session
/// that spawned it.
///
/// `inherit_selection` shares the cell's `Arc`. Snapshotting the *value* into a
/// fresh `Arc` leaves child *reads* working — which is why
/// `detached_tasks_inherit_their_session_selection` stayed green on that
/// mutation — while a child's *write* vanishes. Planner and preview only ever
/// read, so this invariant had no test until now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_selection_written_inside_a_spawned_task_is_visible_to_the_parent() {
    state::with_isolated_test_current(async {
        let (_dir_a, repo_a) = seeded_files(&[("a.txt", "a\n")], "alpha seed");
        let (_dir_b, repo_b) = seeded_files(&[("b.txt", "b\n")], "beta seed");

        let handle_a = state::set_current(&repo_a, RepoMode::Active).expect("alpha registers");
        let alpha_path = state::current().0;
        let _handle_b = state::set_current(&repo_b, RepoMode::Active).expect("beta registers");
        let beta_path = state::current().0;
        assert_ne!(
            alpha_path, beta_path,
            "the two fixtures must resolve to different paths or a vanished write is invisible"
        );

        let child_worktree = handle_a.worktree;
        tokio::spawn(state::inherit_selection(async move {
            assert!(
                state::select_registered(child_worktree, RepoMode::Active),
                "the child must be able to write a selection the catalog already holds"
            );
        }))
        .await
        .expect("child task completed");

        assert_eq!(
            state::current().0,
            alpha_path,
            "parent still sees {beta_path:?}; the child's write to {alpha_path:?} did not share the cell"
        );
    })
    .await;
}
