use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn repo() -> ForgeRepository { repository("https://github.com/octocat/Hello-World").unwrap() }
const FIXTURE: &[u8] = include_bytes!("fixtures/open-pulls.json");

#[test]
fn production_shaped_fixture_maps_only_neutral_summary_fields() {
    let page = decode(FIXTURE, repo(), 1, Some(2), None).unwrap();
    assert_eq!(page.pulls.len(), 2);
    assert_eq!(page.pulls[0].title, "Improve the documentation — café");
    assert!(page.pulls[1].draft);
    assert_eq!(page.pulls[1].web_url, "https://github.com/octocat/Hello-World/pull/1348");
    assert_eq!(page.next_page, Some(2));
    assert!(!page.capabilities.checks && !page.capabilities.reviews);
    let wire = serde_json::to_string(&page).unwrap();
    for omitted in ["malicious.invalid", "requested_reviewers", "markdown body", "private", "unknown_future_field"] { assert!(!wire.contains(omitted)); }
}

#[test]
fn credential_echo_is_absent_from_the_serialized_contract() {
    let token = format!("ghp_{}", "fixture".repeat(7));
    let body = serde_json::to_vec(&serde_json::json!([{"number":7,"state":"open","draft":false,"title":format!("before {token} after {token}"),"html_url":format!("https://example.invalid/{token}"),"body":token}])).unwrap();
    let wire = serde_json::to_string(&decode(&body, repo(), 1, None, Some(&token)).unwrap()).unwrap();
    assert!(!wire.contains(&token), "resolved credential reached the wire");
    assert!(wire.contains("before [redacted] after [redacted]"));
}

#[test]
fn repository_mapping_refuses_foreign_hosts_and_path_injection() {
    for base in ["https://github.com.evil/o/r", "https://github.com/o/..", "https://github.com/o/r?token=x", "https://github.com/o/r/x", "https://github.com/o/%2e%2e", "http://github.com/o/r", "https://github.com/user@host/r"] {
        assert!(repository(base).is_none(), "accepted {base}");
    }
    assert!(repository("https://github.com/a-b/r_1.2").is_some());
}

#[test]
fn pagination_accepts_only_the_next_page_on_the_same_endpoint() {
    let current = Url::parse("https://api.github.com/repos/o/r/pulls?page=1").unwrap();
    let valid = "<https://api.github.com/repos/o/r/pulls?per_page=30&page=2>; rel=\"next\", <https://api.github.com/repos/o/r/pulls?page=8>; rel=\"last\"";
    assert_eq!(next_page(Some(valid), &current, 1), Some(2));
    for link in ["<https://evil.invalid/repos/o/r/pulls?page=2>; rel=\"next\"", "<https://api.github.com/repos/o/other/pulls?page=2>; rel=\"next\"", "<https://api.github.com/repos/o/r/pulls?page=1>; rel=\"next\"", "<https://api.github.com/repos/o/r/pulls?page=2&page=3>; rel=\"next\""] {
        assert_eq!(next_page(Some(link), &current, 1), None);
    }
    assert_eq!(next_page(Some(valid), &current, MAX_PAGE), None);
    assert_eq!(next_page(None, &current, 1), None);
}

/// Real socket fixture: captures the actual request header and returns an
/// upstream response through the same adapter as production. No live credentials.
async fn upstream(status: u16, headers: &str, body: &[u8]) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/repos/octocat/Hello-World/pulls?page=1", listener.local_addr().unwrap())).unwrap();
    let reply = format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()).into_bytes();
    let body = body.to_vec();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![];
        loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 { break; }
            bytes.extend_from_slice(&chunk[..n]);
            if bytes.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        }
        stream.write_all(&reply).await.unwrap();
        stream.write_all(&body).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (url, task)
}
fn fixture_client() -> Client {
    Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(2)).build().unwrap()
}

#[tokio::test]
async fn http_adapter_transmits_token_only_as_header_and_returns_no_store_summary() {
    let token = format!("ghp_{}", "canary".repeat(6));
    let (url, task) = upstream(200, "", FIXTURE).await;
    let page = fetch_page(&fixture_client(), url, repo(), 1, Some(&token), &ForgeService::new()).await;
    assert_eq!(page.availability, Availability::Ready);
    let response = response(page);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains(&token));
    let request = task.await.unwrap();
    assert!(request.contains(&format!("authorization: Bearer {token}\r\n")));
    assert!(!request.lines().next().unwrap().contains(&token));
}

#[tokio::test]
async fn rate_limit_and_ambiguous_access_failures_stay_distinct() {
    for (status, headers, expected) in [
        (403, "Retry-After: 120\r\n", Availability::RateLimited),
        (403, "X-RateLimit-Remaining: 0\r\n", Availability::RateLimited),
        (429, "", Availability::RateLimited),
        (403, "", Availability::AccessUncertain),
        (404, "", Availability::AccessUncertain),
        (401, "", Availability::AccessUncertain),
        (503, "", Availability::Unavailable),
        (302, "Location: http://127.0.0.1:1/steal\r\n", Availability::Unavailable),
    ] {
        let svc = ForgeService::new();
        let (url, task) = upstream(status, headers, b"raw secret error must never be returned").await;
        let page = fetch_page(&fixture_client(), url, repo(), 1, None, &svc).await;
        assert_eq!(page.availability, expected, "HTTP {status}");
        assert_eq!(svc.retry_after().is_some(), expected == Availability::RateLimited);
        assert!(!serde_json::to_string(&page).unwrap().contains("raw secret"));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_and_oversized_upstream_payloads_fail_closed() {
    for body in [b"{\"message\":\"not an array\"}".to_vec(), vec![b'x'; MAX_BODY + 1], b"[{\"number\":1,\"title\":\"closed\",\"draft\":false,\"state\":\"closed\"}]".to_vec()] {
        let (url, task) = upstream(200, "", &body).await;
        let result = fetch_page(&fixture_client(), url, repo(), 1, None, &ForgeService::new()).await;
        assert_eq!(result.availability, Availability::Unavailable);
        // A rejected oversized body may close before the fixture finishes writing.
        let _ = task.await;
    }
}

#[tokio::test]
async fn timed_out_resolver_cannot_accumulate_blocking_workers() {
    let svc = ForgeService::new();
    let (release, wait) = std::sync::mpsc::channel();
    let result = svc.token_with(move || { wait.recv().unwrap(); None }, Duration::from_millis(20)).await;
    assert_eq!(result, Err(Availability::Unavailable));
    assert_eq!(svc.workers.available_permits(), 0);
    assert_eq!(svc.token_with(|| panic!("second resolver must not start"), TOKEN_BUDGET).await, Err(Availability::Unavailable));
    release.send(()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn pending_provider_does_not_block_local_git() {
    let dir = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git").arg("init").arg(dir.path()).output().unwrap().status.success());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/repos/o/r/pulls", listener.local_addr().unwrap())).unwrap();
    let (accepted, observed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let remote = tokio::spawn(async move { fetch_page(&fixture_client(), url, repo(), 1, None, &ForgeService::new()).await });
    observed.await.unwrap();
    // The real local-Git read adapter, on a single-thread runtime while the
    // provider has accepted the socket but never answers.
    let local = tokio::time::timeout(Duration::from_secs(1), crate::git_cmd::git_output(dir.path(), &["status", "--porcelain"])).await;
    assert!(local.is_ok(), "provider blocked the local Git deadline");
    assert!(local.unwrap().unwrap().status.success());
    assert!(!remote.is_finished(), "provider must still be pending at the local assertion");
    server.abort();
    assert_eq!(remote.await.unwrap().availability, Availability::Unavailable);
}
