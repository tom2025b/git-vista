use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn repo() -> ForgeRepository {
    repository("https://github.com/octocat/Hello-World").unwrap()
}
const FIXTURE: &[u8] = include_bytes!("fixtures/open-pulls.json");

#[test]
fn production_shaped_fixture_maps_only_neutral_summary_fields() {
    let page = decode(FIXTURE, repo(), 1, Some(2), None).unwrap();
    assert_eq!(page.pulls.len(), 2);
    assert_eq!(page.pulls[0].title, "Improve the documentation — café");
    assert!(page.pulls[1].draft);
    assert_eq!(
        page.pulls[1].web_url,
        "https://github.com/octocat/Hello-World/pull/1348"
    );
    assert_eq!(page.next_page, Some(2));
    assert!(page.capabilities.checks && page.capabilities.reviews);
    let wire = serde_json::to_string(&page).unwrap();
    for omitted in [
        "malicious.invalid",
        "requested_reviewers",
        "markdown body",
        "private",
        "unknown_future_field",
    ] {
        assert!(!wire.contains(omitted));
    }
}

#[test]
fn credential_echo_is_absent_from_the_serialized_contract() {
    let token = format!("ghp_{}", "fixture".repeat(7));
    let body = serde_json::to_vec(&serde_json::json!([{"number":7,"state":"open","draft":false,"title":format!("before {token} after {token}"),"html_url":format!("https://example.invalid/{token}"),"body":token}])).unwrap();
    let wire =
        serde_json::to_string(&decode(&body, repo(), 1, None, Some(&token)).unwrap()).unwrap();
    assert!(
        !wire.contains(&token),
        "resolved credential reached the wire"
    );
    assert!(wire.contains("before [redacted] after [redacted]"));
}

#[test]
fn repository_mapping_refuses_foreign_hosts_and_path_injection() {
    for base in [
        "https://github.com.evil/o/r",
        "https://github.com/o/..",
        "https://github.com/o/r?token=x",
        "https://github.com/o/r/x",
        "https://github.com/o/%2e%2e",
        "http://github.com/o/r",
        "https://github.com/user@host/r",
    ] {
        assert!(repository(base).is_none(), "accepted {base}");
    }
    assert!(repository("https://github.com/a-b/r_1.2").is_some());
}

#[test]
fn pagination_accepts_only_the_next_page_on_the_same_endpoint() {
    let current = Url::parse("https://api.github.com/repos/o/r/pulls?page=1").unwrap();
    let valid = "<https://api.github.com/repos/o/r/pulls?per_page=30&page=2>; rel=\"next\", <https://api.github.com/repos/o/r/pulls?page=8>; rel=\"last\"";
    assert_eq!(next_page(Some(valid), &current, 1), Some(2));
    for link in [
        "<https://evil.invalid/repos/o/r/pulls?page=2>; rel=\"next\"",
        "<https://api.github.com/repos/o/other/pulls?page=2>; rel=\"next\"",
        "<https://api.github.com/repos/o/r/pulls?page=1>; rel=\"next\"",
        "<https://api.github.com/repos/o/r/pulls?page=2&page=3>; rel=\"next\"",
    ] {
        assert_eq!(next_page(Some(link), &current, 1), None);
    }
    assert_eq!(next_page(Some(valid), &current, MAX_PAGE), None);
    assert_eq!(next_page(None, &current, 1), None);
}

/// Real socket fixture: captures the actual request header and returns an
/// upstream response through the same adapter as production. No live credentials.
async fn upstream(
    status: u16,
    headers: &str,
    body: &[u8],
) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/repos/octocat/Hello-World/pulls?page=1",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let reply = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    let body = body.to_vec();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![];
        loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        stream.write_all(&reply).await.unwrap();
        stream.write_all(&body).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (url, task)
}
fn fixture_client() -> Client {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

async fn upstream_sequence<F>(replies_for: F) -> (Url, tokio::task::JoinHandle<Vec<String>>)
where
    F: FnOnce(&Url) -> Vec<(u16, String, Vec<u8>)>,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let replies = replies_for(&base);
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, headers, body) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            loop {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            requests.push(String::from_utf8(bytes).unwrap());
        }
        requests
    });
    (base, task)
}

#[test]
fn missing_or_empty_credentials_are_an_explicit_access_state() {
    assert_eq!(required_token(None), Err(Availability::AccessUncertain));
    assert_eq!(
        required_token(Some(String::new())),
        Err(Availability::AccessUncertain)
    );
    assert_eq!(
        required_token(Some("configured".into())).unwrap(),
        "configured"
    );
}

#[tokio::test]
async fn detail_adapter_projects_checks_and_latest_reviews_without_raw_provider_data() {
    let token = format!("ghp_{}", "detail-canary".repeat(3));
    let sha = "a".repeat(40);
    let pull = serde_json::json!({
        "number": 1347,
        "state": "open",
        "head": { "sha": sha },
        "body": format!("must not cross the boundary: {token}"),
        "html_url": "https://malicious.invalid/provider-link"
    });
    let checks = serde_json::json!({
        "total_count": 2,
        "check_runs": [
            { "name": "build", "status": "completed", "conclusion": "success", "output": { "text": token } },
            { "name": "browser", "status": "completed", "conclusion": "failure", "details_url": "https://malicious.invalid/check" }
        ]
    });
    let reviews = serde_json::json!([
        { "id": 1, "state": "COMMENTED", "body": token, "user": { "login": "octocat" } },
        { "id": 2, "state": "APPROVED", "body": token, "user": { "login": "octocat" } },
        { "id": 3, "state": "CHANGES_REQUESTED", "body": token, "user": { "login": "hubot" } }
    ]);
    let (base, task) = upstream_sequence(|_| {
        vec![
            (200, String::new(), serde_json::to_vec(&pull).unwrap()),
            (200, String::new(), serde_json::to_vec(&checks).unwrap()),
            (200, String::new(), serde_json::to_vec(&reviews).unwrap()),
        ]
    })
    .await;
    let details = fetch_details(
        &fixture_client(),
        &base,
        &repo(),
        1347,
        &token,
        &ForgeService::new(),
    )
    .await;
    assert_eq!(details.availability, Availability::Ready);
    assert_eq!(details.check_rollup, CheckRollup::Failing);
    assert_eq!(details.review_rollup, ReviewRollup::ChangesRequested);
    assert_eq!(
        details.checks,
        vec![
            CheckSummary {
                name: "build".into(),
                state: CheckState::Success,
            },
            CheckSummary {
                name: "browser".into(),
                state: CheckState::Failure,
            },
        ]
    );
    assert_eq!(
        details.reviews,
        vec![
            ReviewSummary {
                reviewer: "hubot".into(),
                state: ReviewState::ChangesRequested,
            },
            ReviewSummary {
                reviewer: "octocat".into(),
                state: ReviewState::Approved,
            },
        ]
    );
    let wire = serde_json::to_string(&details).unwrap();
    assert!(!wire.contains(&token));
    assert!(!wire.contains("malicious.invalid"));
    let response = details_response(details);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /repos/octocat/Hello-World/pulls/1347 "));
    assert!(requests[1].starts_with(&format!(
        "GET /repos/octocat/Hello-World/commits/{sha}/check-runs?"
    )));
    assert!(requests[2].starts_with("GET /repos/octocat/Hello-World/pulls/1347/reviews?"));
    for request in requests {
        assert!(request.contains(&format!("authorization: Bearer {token}\r\n")));
        assert!(!request.lines().next().unwrap().contains(&token));
    }
}

#[tokio::test]
async fn check_and_review_pages_are_reconstructed_and_aggregated() {
    let sha = "b".repeat(40);
    let pull = serde_json::to_vec(&serde_json::json!({
        "number": 9, "state": "open", "head": { "sha": sha.clone() }
    }))
    .unwrap();
    let check_one = serde_json::to_vec(&serde_json::json!({
        "check_runs": [{ "name": "queued", "status": "queued", "conclusion": null }]
    }))
    .unwrap();
    let check_two = serde_json::to_vec(&serde_json::json!({
        "check_runs": [{ "name": "built", "status": "completed", "conclusion": "success" }]
    }))
    .unwrap();
    let review_one = serde_json::to_vec(&serde_json::json!([
        { "id": 1, "state": "COMMENTED", "user": { "login": "octocat" } }
    ]))
    .unwrap();
    let review_two = serde_json::to_vec(&serde_json::json!([
        { "id": 2, "state": "APPROVED", "user": { "login": "octocat" } }
    ]))
    .unwrap();
    let (base, task) = upstream_sequence(|base| {
        let checks_next = format!(
            "Link: <{}repos/octocat/Hello-World/commits/{sha}/check-runs?filter=latest&per_page=100&page=2>; rel=\"next\"\r\n",
            base.as_str()
        );
        let reviews_next = format!(
            "Link: <{}repos/octocat/Hello-World/pulls/9/reviews?per_page=100&page=2>; rel=\"next\"\r\n",
            base.as_str()
        );
        vec![
            (200, String::new(), pull),
            (200, checks_next, check_one),
            (200, String::new(), check_two),
            (200, reviews_next, review_one),
            (200, String::new(), review_two),
        ]
    })
    .await;
    let details = fetch_details(
        &fixture_client(),
        &base,
        &repo(),
        9,
        "token",
        &ForgeService::new(),
    )
    .await;
    assert_eq!(details.availability, Availability::Ready);
    assert_eq!(details.checks.len(), 2);
    assert_eq!(details.check_rollup, CheckRollup::Pending);
    assert_eq!(
        details.reviews,
        vec![ReviewSummary {
            reviewer: "octocat".into(),
            state: ReviewState::Approved,
        }]
    );
    assert_eq!(details.review_rollup, ReviewRollup::Approved);
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[2].starts_with(&format!(
        "GET /repos/octocat/Hello-World/commits/{sha}/check-runs?filter=latest&per_page=100&page=2 "
    )));
    assert!(requests[4]
        .starts_with("GET /repos/octocat/Hello-World/pulls/9/reviews?per_page=100&page=2 "));
}

#[tokio::test]
async fn http_adapter_transmits_token_only_as_header_and_returns_no_store_summary() {
    let token = format!("ghp_{}", "canary".repeat(6));
    let (url, task) = upstream(200, "", FIXTURE).await;
    let page = fetch_page(
        &fixture_client(),
        url,
        repo(),
        1,
        Some(&token),
        &ForgeService::new(),
    )
    .await;
    assert_eq!(page.availability, Availability::Ready);
    let response = response(page);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), MAX_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains(&token));
    let request = task.await.unwrap();
    assert!(request.contains(&format!("authorization: Bearer {token}\r\n")));
    assert!(!request.lines().next().unwrap().contains(&token));
    assert!(request.contains("x-github-api-version: 2026-03-10\r\n"));
}

#[tokio::test]
async fn rate_limit_and_ambiguous_access_failures_stay_distinct() {
    for (status, headers, expected) in [
        (403, "Retry-After: 120\r\n", Availability::RateLimited),
        (
            403,
            "X-RateLimit-Remaining: 0\r\n",
            Availability::RateLimited,
        ),
        (429, "", Availability::RateLimited),
        (403, "", Availability::AccessUncertain),
        (404, "", Availability::AccessUncertain),
        (401, "", Availability::AccessUncertain),
        (503, "", Availability::Unavailable),
        (
            302,
            "Location: http://127.0.0.1:1/steal\r\n",
            Availability::Unavailable,
        ),
    ] {
        let svc = ForgeService::new();
        let (url, task) =
            upstream(status, headers, b"raw secret error must never be returned").await;
        let page = fetch_page(&fixture_client(), url, repo(), 1, None, &svc).await;
        assert_eq!(page.availability, expected, "HTTP {status}");
        assert_eq!(
            svc.retry_after().is_some(),
            expected == Availability::RateLimited
        );
        assert!(!serde_json::to_string(&page).unwrap().contains("raw secret"));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_and_oversized_upstream_payloads_fail_closed() {
    for body in [
        b"{\"message\":\"not an array\"}".to_vec(),
        vec![b'x'; MAX_BODY + 1],
        b"[{\"number\":1,\"title\":\"closed\",\"draft\":false,\"state\":\"closed\"}]".to_vec(),
    ] {
        let (url, task) = upstream(200, "", &body).await;
        let result = fetch_page(
            &fixture_client(),
            url,
            repo(),
            1,
            None,
            &ForgeService::new(),
        )
        .await;
        assert_eq!(result.availability, Availability::Unavailable);
        // A rejected oversized body may close before the fixture finishes writing.
        let _ = task.await;
    }
}

#[tokio::test]
async fn timed_out_resolver_cannot_accumulate_blocking_workers() {
    let svc = ForgeService::new();
    let (release, wait) = std::sync::mpsc::channel();
    let result = svc
        .token_with(
            move || {
                wait.recv().unwrap();
                None
            },
            Duration::from_millis(20),
        )
        .await;
    assert_eq!(result, Err(Availability::Unavailable));
    assert_eq!(svc.workers.available_permits(), 0);
    assert_eq!(
        svc.token_with(|| panic!("second resolver must not start"), TOKEN_BUDGET)
            .await,
        Err(Availability::Unavailable)
    );
    release.send(()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn pending_provider_does_not_block_local_git() {
    let dir = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg(dir.path())
        .output()
        .unwrap()
        .status
        .success());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/repos/o/r/pulls",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (accepted, observed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let remote = tokio::spawn(async move {
        fetch_page(
            &fixture_client(),
            url,
            repo(),
            1,
            None,
            &ForgeService::new(),
        )
        .await
    });
    observed.await.unwrap();
    // The real local-Git read adapter, on a single-thread runtime while the
    // provider has accepted the socket but never answers.
    let local = tokio::time::timeout(
        Duration::from_secs(1),
        crate::git_cmd::git_output(dir.path(), &["status", "--porcelain"]),
    )
    .await;
    assert!(local.is_ok(), "provider blocked the local Git deadline");
    assert!(local.unwrap().unwrap().status.success());
    assert!(
        !remote.is_finished(),
        "provider must still be pending at the local assertion"
    );
    server.abort();
    assert_eq!(
        remote.await.unwrap().availability,
        Availability::Unavailable
    );
}

#[test]
fn actual_origin_config_is_validated_before_lossy_display_normalization() {
    let dir = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg(dir.path())
        .output()
        .unwrap()
        .status
        .success());
    for (remote, accepted) in [
        ("https://github.com/octocat/Hello-World.git", true),
        ("git@github.com:octocat/Hello-World.git", true),
        ("ssh://git@github.com/octocat/Hello-World.git", true),
        ("https://github.com/octocat/Hello-World/extra", false),
        ("git@github.com:octocat/Hello-World/extra", false),
        ("https://github.com/other/../octocat/Hello-World", false),
        ("https://github.com/octocat/Hello-World?x=y", false),
        ("https://github.com/octocat/Hello-World#fragment", false),
        ("https://github.com:8443/octocat/Hello-World", false),
        ("https://user:secret@github.com/octocat/Hello-World", false),
    ] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", "remote.origin.url", remote])
            .output()
            .unwrap()
            .status
            .success());
        assert_eq!(repository_at(dir.path()).is_some(), accepted, "{remote}");
    }
}

#[tokio::test]
async fn successful_last_quota_response_stays_ready_and_starts_cooldown() {
    let svc = ForgeService::new();
    let (url, task) = upstream(
        200,
        "X-RateLimit-Remaining: 0\r\nRetry-After: 120\r\n",
        FIXTURE,
    )
    .await;
    let page = fetch_page(&fixture_client(), url, repo(), 1, None, &svc).await;
    assert_eq!(page.availability, Availability::Ready);
    assert_eq!(page.pulls.len(), 2);
    assert!(svc
        .retry_after()
        .is_some_and(|seconds| (119..=121).contains(&seconds)));
    task.await.unwrap();
}
