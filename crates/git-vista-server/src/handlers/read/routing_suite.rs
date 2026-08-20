//! The read handlers' shared front door: the `?repo=` selector's two
//! fail-closed cases (a malformed id, and a well-formed id the catalog never
//! registered), plus route *registration* itself — that `/api/frame` and the
//! paged `/api/commits` are wired onto both listener profiles (loopback and
//! LAN, ADR 0005) while a representative write route is loopback-only, and
//! that the pathological default-page fixture stays under its response
//! budget. Grouped together because both halves drive a real `Router` with
//! `tower::ServiceExt::oneshot` rather than calling a handler function
//! directly — registration and selector-validation are what's under test.

use super::*;
use axum::routing::get;
use axum::Router;
use git_vista_core::model::CommitSummary;
use git_vista_protocol::{PROTOCOL_HEADER, PROTOCOL_VERSION};
use tower::ServiceExt;

// --- duplicated cross-suite test helpers, verbatim from read.rs's original inline test module —
// private to their own modules and unreachable from here, same shape
// as the planner/*_suite.rs convention this crate already uses. ---

/// A deterministic cursor codec, so nothing here depends on the per-process
/// random key.
fn history_codec() -> CursorCodec {
    CursorCodec::with_key([0x27; 32])
}

async fn status_of(app: Router, uri: &str) -> StatusCode {
    let req = axum::http::Request::get(uri)
        .body(axum::body::Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn repo_selector_rejects_a_malformed_id_as_bad_request() {
    // A `?repo=` that isn't even a valid id never reaches path resolution.
    let app = Router::new().route("/api/head-branch", get(head_branch));
    let status = status_of(app, "/api/head-branch?repo=not-an-id").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn repo_selector_fails_closed_on_an_unknown_id() {
    // A well-formed id the catalog never registered resolves to nothing — the
    // request is refused with a 404 rather than falling back to any path.
    let unknown = WorktreeId::from_git_dir("/no/such/repo/.git").to_string();
    let app = Router::new().route("/api/head-branch", get(head_branch));
    let status = status_of(app, &format!("/api/head-branch?repo={unknown}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- shared router registration and the response budget (Step 9) --------

/// Establish a session against `router` (whichever host it expects) and
/// return just the `Cookie` header value. Duplicated from `main.rs`'s own
/// test helper of the same shape (private to that module, unreachable from
/// here) rather than exposed across the crate for one shared test helper.
async fn bootstrap_cookie_for(router: Router, host: &str, token: &str) -> String {
    let resp = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/session")
                .header(header::HOST, host)
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
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    set_cookie.split(';').next().unwrap().to_string()
}

/// Both `/api/frame` and the paged `/api/commits` are registered on *both*
/// listener profiles (loopback and LAN, ADR 0005), while a representative
/// write route (`/api/commit`, POST) exists only on loopback — proving the
/// two new reads were added to `api_router`'s always-registered section,
/// not inside the `full_routes` write block. Follows the shape of
/// `main::tests::the_lan_router_has_no_write_routes` /
/// `..._loopback_router_still_has_write_routes_registered`, driven at the
/// real route table (not `page_for_target` directly) because route
/// *registration* is exactly what's under test here.
#[tokio::test]
async fn history_routes_exist_on_loopback_and_lan_read_profile() {
    for (via_lan, host, full_routes) in [
        (false, "localhost:8080", true),
        (true, "192.168.1.42:8080", false),
    ] {
        let sessions = std::sync::Arc::new(crate::session::SessionManager::new(None));
        let token = sessions.current_bootstrap();
        let session_state = crate::handlers::session::SessionState {
            manager: sessions,
            via_lan,
            rate_limiter: None,
        };
        let hosts = if via_lan {
            crate::security::HostPolicy::lan("192.168.1.42".parse().unwrap(), crate::state::PORT)
        } else {
            crate::security::HostPolicy::loopback(crate::state::PORT)
        };
        let router =
            crate::api_router(session_state, hosts, full_routes, Arc::new(history_codec()));
        let cookie = bootstrap_cookie_for(router.clone(), host, &token).await;

        for (method, uri) in [("GET", "/api/frame"), ("GET", "/api/commits")] {
            let resp = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(uri)
                        .header(header::HOST, host)
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::COOKIE, cookie.clone())
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {uri} must be a registered route on the {} profile (it may still \
                     fail for other reasons, e.g. no repository selected)",
                if via_lan { "LAN" } else { "loopback" }
            );
        }

        // The representative write: registered POST-only on loopback,
        // never registered at all on LAN (ADR 0005). A GET reaches real
        // routing either way, so 404 (never built) is distinguishable
        // from 405 (built, wrong method).
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/commit")
                    .header(header::HOST, host)
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::COOKIE, cookie)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if via_lan {
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "the LAN profile must never register a write route"
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "the loopback profile keeps its write routes registered"
            );
        }
    }
}

/// A deliberately pathological, but not unrealistic, default-size Page: 250
/// rows (the un-overridden `?limit=`), long real-world author/summary
/// fields, a scatter of merges, several rows carrying refs, and a cascade
/// of stubs. This is a **fixture budget, not a universal metadata ceiling**
/// — a real repository with even longer commit messages or many more refs
/// on one page could exceed 512 KiB; this only proves today's realistic
/// worst case stays comfortably inside it.
#[test]
fn default_page_pathological_fixture_is_at_most_512_kib() {
    let long_author = "Alexandra Christodoulopoulou-Fitzgerald-Nakamura-Petrov \
             <alexandra.christodoulopoulou-fitzgerald-nakamura-petrov@\
             an-extremely-long-corporate-engineering-subdomain.example-enterprises.co.uk>";
    let long_summary = "refactor(auth,session): replace the legacy cookie-based session \
             token validation path with the new HMAC-SHA256-signed scheme, closing out the \
             follow-up work items from the January security review and satisfying checklist \
             item 7.3 (rotation, constant-time compare, and origin binding)";

    let hex = |n: u32| format!("{n:040x}");

    let mut rows = Vec::with_capacity(DEFAULT_PAGE_LIMIT);
    let mut edges = Vec::new();
    let mut stubs = Vec::new();
    let lanes = 6usize;

    for row in 0..DEFAULT_PAGE_LIMIT {
        let is_merge = row % 17 == 0 && row > 0;
        let lane = row % lanes;
        let mut parents = vec![Oid(hex(row as u32 + 1))];
        if is_merge {
            parents.push(Oid(hex(row as u32 + 1000)));
        }
        let mut refs = Vec::new();
        if row % 23 == 0 {
            refs.push(GitRef {
                name: format!("feature/a-very-descriptive-long-lived-branch-name-for-team-{row}"),
                kind: git_vista_core::model::RefKind::Branch,
                target: Oid(hex(row as u32)),
            });
            refs.push(GitRef {
                name: format!("origin/feature/a-very-descriptive-long-lived-branch-name-{row}"),
                kind: git_vista_core::model::RefKind::RemoteBranch,
                target: Oid(hex(row as u32)),
            });
        }
        rows.push(GraphRow {
            commit: CommitSummary {
                id: Oid(hex(row as u32)),
                parents,
                summary: long_summary.to_string(),
                author: long_author.to_string(),
                time: 1_700_000_000 + row as i64,
            },
            row,
            lane,
            refs,
            color: row % 8,
            on_remote: row % 3 == 0,
        });
        if row > 0 {
            edges.push(Edge {
                from_row: row - 1,
                from_lane: (row - 1) % lanes,
                to_row: row,
                to_lane: lane,
            });
        }
        if is_merge {
            edges.push(Edge {
                from_row: row - 1,
                from_lane: lanes - 1,
                to_row: row,
                to_lane: lane,
            });
        }
        if row % 31 == 0 {
            for depth in 0..3 {
                stubs.push(FrameStub {
                    name: format!(
                        "release/a-long-lived-release-branch-name-row-{row}-depth-{depth}"
                    ),
                    anchor_commit: Oid(hex(row as u32)),
                    lane_offset: lanes + depth,
                    color: (row + depth) % 8,
                    depth,
                });
            }
        }
    }

    let page = Page {
        rows,
        edges,
        stubs,
        lane_count: lanes,
        cursor: Some(
            "A".repeat(64) + "." + &"b".repeat(96), // a plausible signed-cursor shape
        ),
        generation: git_vista_protocol::GenerationToken::new(format!(
            "history-v1:{}",
            "f".repeat(64)
        ))
        .unwrap(),
    };

    let body = serde_json::to_vec(&page).expect("Page always serializes");
    assert!(
        body.len() <= 512 * 1024,
        "fixture budget exceeded ({} bytes > 512 KiB) — this is a fixture budget for \
             today's pathological-but-realistic default page, not a universal metadata \
             ceiling: a real repository could still produce a larger page than this",
        body.len()
    );
}
