//! Bisect's read and three write endpoints (#87, #708, ADRs 0131 and 0138).
//! The status read discovers git's own session; writes use the planner.

use axum::extract::{Json, Query};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use git_vista_protocol::dto::{BisectLogStep, BisectMarkRequest, BisectStartRequest, BisectStatus};
use git_vista_protocol::GitOperation;

use super::read::{resolve_repo, RepoQuery};
use crate::planner;
use crate::state::reject_if_read_only;

/// `POST /api/bisect/start`: [`GitOperation::BisectStart`].
pub(crate) async fn bisect_start(Json(req): Json<BisectStartRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectStart {
        bad: req.bad,
        good: req.good,
    })
    .await
}

/// `POST /api/bisect/mark`: [`GitOperation::BisectMark`].
pub(crate) async fn bisect_mark(Json(req): Json<BisectMarkRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectMark {
        verdict: req.verdict,
    })
    .await
}

/// `POST /api/bisect/reset`: [`GitOperation::BisectReset`]. No body — same
/// shape as `/api/stage`/`/api/unstage`.
pub(crate) async fn bisect_reset() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::BisectReset).await
}

/// Read-only on both listener profiles and in Visualize mode. Resolve the
/// optional opaque worktree selector once, just like the other live reads.
pub(crate) async fn bisect_status(Query(q): Query<RepoQuery>) -> Response {
    let (repo, _, _) = match resolve_repo(q.repo.as_deref()) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    let status = planner::bisect_exec::discover(&repo).await;
    let body = BisectStatus {
        in_progress: status.in_progress,
        current: status.current,
        started_from: status.started_from,
        bad: status.bad,
        good: status.good,
        skipped: status.skipped,
        history: status
            .history
            .into_iter()
            .map(|step| BisectLogStep {
                verb: step.verb,
                args: step.args,
            })
            .collect(),
        finished: status.finished,
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, Router};
    use git_vista_protocol::{RepoMode, PROTOCOL_HEADER, PROTOCOL_VERSION};
    use std::path::Path;
    use std::sync::Arc;
    use tower::ServiceExt;

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

    async fn assert_discoverable(repo: &Path) {
        assert!(
            repo.join(".git/BISECT_START").is_file(),
            "git must have written BISECT_START"
        );
        assert!(
            planner::bisect_exec::discover(repo).await.in_progress,
            "discover must observe the fixture's live bisect"
        );
    }

    // Fresh router + SessionManager emulate a restarted server, with no app
    // bisect cache to restore. Requests pass the production auth/route layers.
    async fn client(via_lan: bool) -> (Router, String, &'static str) {
        let manager = Arc::new(crate::session::SessionManager::new(None));
        let token = manager.current_bootstrap();
        let host = if via_lan {
            "192.168.1.42:8080"
        } else {
            "localhost:8080"
        };
        let hosts = if via_lan {
            crate::security::HostPolicy::lan("192.168.1.42".parse().unwrap(), crate::state::PORT)
        } else {
            crate::security::HostPolicy::loopback(crate::state::PORT)
        };
        let app = crate::api_router(
            crate::handlers::session::SessionState {
                manager,
                via_lan,
                rate_limiter: None,
            },
            hosts,
            !via_lan,
            Arc::new(crate::history::CursorCodec::with_key([0x28; 32])),
            Arc::new(crate::token_store::RequestTokenResolver::without_keyring()),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header(header::HOST, host)
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        55000,
                    ))))
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (app, cookie, host)
    }

    async fn get(client: &(Router, String, &str), suffix: &str) -> Response {
        client
            .0
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/bisect/status{suffix}"))
                    .header(header::HOST, client.2)
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::COOKIE, &client.1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn status(client: &(Router, String, &str), suffix: &str) -> BisectStatus {
        let response = get(client, suffix).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn bisect_status_route_reads_git_and_survives_a_new_session_on_both_profiles() {
        crate::state::with_isolated_test_current(async {
            let (_dir, repo) = git_vista_fixtures::seeded();
            let good = git(&repo, &["rev-parse", "HEAD"]);
            for n in 0..4 {
                git(
                    &repo,
                    &["commit", "--allow-empty", "-m", &format!("candidate {n}")],
                );
            }
            let bad = git(&repo, &["rev-parse", "HEAD"]);
            let handle = crate::state::set_current(&repo, RepoMode::Visualize).unwrap();
            let selector = format!("?repo={}", handle.worktree);
            let first = client(false).await;
            let inactive = status(&first, "").await;
            assert!(!inactive.in_progress);
            assert!(!inactive.finished);
            assert!(
                inactive.current.is_none()
                    && inactive.started_from.is_none()
                    && inactive.bad.is_none()
            );
            assert!(
                inactive.good.is_empty()
                    && inactive.skipped.is_empty()
                    && inactive.history.is_empty()
            );
            // Git starts this session independently of the app's write routes.
            git(&repo, &["bisect", "start", &bad, &good]);
            let skipped = git(&repo, &["rev-parse", "HEAD"]);
            git(&repo, &["bisect", "skip"]);
            let head = git(&repo, &["rev-parse", "HEAD"]);
            assert_discoverable(&repo).await;
            let log = std::fs::read(repo.join(".git/BISECT_LOG")).unwrap();
            let index = std::fs::read(repo.join(".git/index")).unwrap();
            let refs = git(
                &repo,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            );
            for via_lan in [false, true] {
                let restarted = client(via_lan).await;
                let actual = status(&restarted, &selector).await;
                assert!(actual.in_progress);
                assert!(!actual.finished);
                assert_eq!(actual.current.as_deref(), Some(head.as_str()));
                assert_eq!(actual.started_from.as_deref(), Some("main"));
                assert_eq!(actual.bad.as_deref(), Some(bad.as_str()));
                assert_eq!(actual.good, vec![good.clone()]);
                assert_eq!(actual.skipped, vec![skipped.clone()]);
                assert_eq!(
                    actual.history,
                    vec![
                        BisectLogStep {
                            verb: "start".into(),
                            args: vec![bad.clone(), good.clone()]
                        },
                        BisectLogStep {
                            verb: "skip".into(),
                            args: vec![skipped.clone()]
                        },
                    ]
                );
            }
            assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
            assert_eq!(std::fs::read(repo.join(".git/BISECT_LOG")).unwrap(), log);
            assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
            assert_eq!(
                git(
                    &repo,
                    &["for-each-ref", "--format=%(refname) %(objectname)"]
                ),
                refs
            );
            git(&repo, &["bisect", "reset"]);
            assert_eq!(status(&first, &selector).await, inactive);
            // A settled range is still an active session until reset.
            let parent = git(&repo, &["rev-parse", "HEAD^"]);
            git(&repo, &["bisect", "start", &bad, &parent]);
            let finished = status(&first, &selector).await;
            assert!(finished.in_progress && finished.finished);
            git(&repo, &["bisect", "reset"]);
        })
        .await;
    }

    #[tokio::test]
    async fn bisect_status_selector_is_scoped_and_fails_closed() {
        crate::state::with_isolated_test_current(async {
            let (_dir, repo) = git_vista_fixtures::seeded();
            let handle = crate::state::set_current(&repo, RepoMode::Visualize).unwrap();
            let good = git(&repo, &["rev-parse", "HEAD"]);
            git(&repo, &["commit", "--allow-empty", "-m", "bad"]);
            git(&repo, &["bisect", "start", "HEAD", &good]);
            assert_discoverable(&repo).await;
            let (_other_dir, other) = git_vista_fixtures::seeded();
            crate::state::set_current(&other, RepoMode::Visualize);
            let client = client(false).await;
            assert!(!status(&client, "").await.in_progress);
            assert!(
                status(&client, &format!("?repo={}", handle.worktree))
                    .await
                    .in_progress
            );
            assert_eq!(
                get(&client, "?repo=invalid").await.status(),
                StatusCode::BAD_REQUEST
            );
            let unknown = git_vista_core::identity::WorktreeId::from_git_dir("/missing/gv708/.git");
            assert_eq!(
                get(&client, &format!("?repo={unknown}")).await.status(),
                StatusCode::NOT_FOUND
            );
            git(&repo, &["bisect", "reset"]);
        })
        .await;
    }
}
