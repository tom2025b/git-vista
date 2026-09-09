//! GitHub adapter at the handler boundary (#89). Only neutral DTOs leave here.
//! No local Git lock spans provider I/O; admission is fail-fast and independent.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::Query;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use git_vista_protocol::forge::{
    ForgeAvailability as Availability, ForgeCapabilities, ForgePage, ForgeRepository,
    PullRequestSummary,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

const PAGE_SIZE: usize = 30;
const MAX_BODY: usize = 1024 * 1024;
const MAX_PAGE: u32 = 10_000;
const DETAIL_PAGE_SIZE: usize = 100;
const MAX_DETAIL_PAGES: u32 = 10;
const REQUEST_BUDGET: Duration = Duration::from_secs(8);
const TOKEN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
pub(crate) struct PullQuery {
    repo: Option<String>,
    #[serde(default = "first_page")]
    page: u32,
    number: Option<u64>,
}
fn first_page() -> u32 {
    1
}

struct ForgeService {
    // A timed-out synchronous resolver keeps its permit until it actually exits.
    workers: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    // Only a deadline, never provider data or token hashes. Conservative across
    // all repos/tokens: changing a token does not bypass an outstanding cooldown.
    cooldown: Mutex<Option<Instant>>,
}
impl ForgeService {
    fn new() -> Self {
        Self {
            workers: Arc::new(Semaphore::new(1)),
            requests: Arc::new(Semaphore::new(2)),
            cooldown: Mutex::new(None),
        }
    }
    fn retry_after(&self) -> Option<u64> {
        self.cooldown
            .lock()
            .expect("forge cooldown")
            .and_then(|until| {
                until
                    .checked_duration_since(Instant::now())
                    .map(|d| d.as_secs().saturating_add(1))
            })
    }
    fn cool_down(&self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.clamp(1, 86400));
        let mut deadline = self.cooldown.lock().expect("forge cooldown");
        *deadline = Some(deadline.map_or(until, |old| old.max(until)));
    }
    async fn token_with<F>(
        &self,
        resolve: F,
        budget: Duration,
    ) -> Result<Option<String>, Availability>
    where
        F: FnOnce() -> Option<String> + Send + 'static,
    {
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| Availability::Unavailable)?;
        tokio::time::timeout(
            budget,
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                resolve()
            }),
        )
        .await
        .map_err(|_| Availability::Unavailable)?
        .map_err(|_| Availability::Unavailable)
    }
}
fn service() -> &'static ForgeService {
    static SERVICE: OnceLock<ForgeService> = OnceLock::new();
    SERVICE.get_or_init(ForgeService::new)
}

/// Validate the complete configured origin before constructing API/web links.
/// Browser input and upstream links never supply a credential destination.
fn repository_at(path: &std::path::Path) -> Option<ForgeRepository> {
    git_vista_git::origin_url(path)
        .as_deref()
        .and_then(repository_from_remote)
}

fn repository_from_remote(remote: &str) -> Option<ForgeRepository> {
    let path = if remote.contains("://") {
        let url = Url::parse(remote).ok()?;
        if url.host_str()? != "github.com"
            || url.query().is_some()
            || url.fragment().is_some()
            || url.password().is_some()
        {
            return None;
        }
        match url.scheme() {
            "https" if url.username().is_empty() && url.port_or_known_default() == Some(443) => {}
            "ssh" if matches!(url.username(), "" | "git") && url.port().is_none_or(|p| p == 22) => {
            }
            _ => return None,
        }
        // URL parsers normalize dot segments. Reject them in the original
        // spelling, rather than accepting their normalized different repo.
        if remote.split('/').any(|p| matches!(p, "." | "..")) || remote.contains('%') {
            return None;
        }
        url.path().strip_prefix('/')?.to_string()
    } else {
        let (host, path) = remote.split_once(':')?;
        if !matches!(
            host.to_ascii_lowercase().as_str(),
            "github.com" | "git@github.com"
        ) {
            return None;
        }
        path.to_string()
    };
    let path = path.strip_suffix('/').unwrap_or(&path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    repository(&format!("https://github.com/{path}"))
}

fn repository(base: &str) -> Option<ForgeRepository> {
    let name = base.strip_prefix("https://github.com/")?;
    let parts: Vec<_> = name.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 100
                || matches!(*part, "." | "..")
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        return None;
    }
    Some(ForgeRepository {
        name: name.to_string(),
        web_url: base.to_string(),
    })
}
fn empty(page: u32, availability: Availability) -> ForgePage {
    let supported = availability != Availability::Unsupported;
    ForgePage {
        repository: None,
        availability,
        capabilities: ForgeCapabilities {
            summaries: supported,
            checks: supported,
            reviews: supported,
        },
        pulls: vec![],
        page,
        next_page: None,
        retry_after_seconds: None,
    }
}
fn response(page: ForgePage) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckRollup {
    Unknown,
    NoChecks,
    Pending,
    Passing,
    Failing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReviewRollup {
    Unknown,
    NoReviews,
    Reviewed,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckState {
    Queued,
    InProgress,
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CheckSummary {
    name: String,
    state: CheckState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReviewSummary {
    reviewer: String,
    state: ReviewState,
}

/// Transient, provider-neutral details for one pull request. Raw provider
/// objects, review bodies, check output and credentials never cross this seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PullDetails {
    number: u64,
    availability: Availability,
    check_rollup: CheckRollup,
    review_rollup: ReviewRollup,
    checks: Vec<CheckSummary>,
    reviews: Vec<ReviewSummary>,
    retry_after_seconds: Option<u64>,
}

fn empty_details(number: u64, availability: Availability) -> PullDetails {
    PullDetails {
        number,
        availability,
        check_rollup: CheckRollup::Unknown,
        review_rollup: ReviewRollup::Unknown,
        checks: vec![],
        reviews: vec![],
        retry_after_seconds: None,
    }
}

fn details_response(details: PullDetails) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(details)).into_response()
}

fn unavailable_response(
    query: &PullQuery,
    availability: Availability,
    retry: Option<u64>,
) -> Response {
    match query.number {
        Some(number) => {
            let mut details = empty_details(number, availability);
            details.retry_after_seconds = retry;
            details_response(details)
        }
        None => {
            let mut page = empty(query.page, availability);
            page.retry_after_seconds = retry;
            response(page)
        }
    }
}

fn required_token(token: Option<String>) -> Result<String, Availability> {
    token
        .filter(|token| !token.is_empty())
        .ok_or(Availability::AccessUncertain)
}

pub(crate) async fn pulls(
    Query(query): Query<PullQuery>,
) -> Result<Response, (StatusCode, String)> {
    if !(1..=MAX_PAGE).contains(&query.page) {
        return Err((StatusCode::BAD_REQUEST, "Invalid pull request page.".into()));
    }
    if query.number == Some(0) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid pull request number.".into(),
        ));
    }
    let path = super::read::resolve_repo(query.repo.as_deref())?.0;
    let svc = service();
    let Ok(_request) = svc.requests.clone().try_acquire_owned() else {
        return Ok(unavailable_response(
            &query,
            Availability::Unavailable,
            None,
        ));
    };
    let mapped = tokio::task::spawn_blocking(move || repository_at(&path))
        .await
        .ok()
        .flatten();
    let Some(repo) = mapped else {
        return Ok(unavailable_response(
            &query,
            Availability::Unsupported,
            None,
        ));
    };
    if let Some(seconds) = svc.retry_after() {
        return Ok(unavailable_response(
            &query,
            Availability::RateLimited,
            Some(seconds),
        ));
    }
    let token = match svc
        .token_with(crate::state::credential_token, TOKEN_BUDGET)
        .await
    {
        Ok(token) => match required_token(token) {
            Ok(token) => token,
            Err(state) => return Ok(unavailable_response(&query, state, None)),
        },
        Err(state) => return Ok(unavailable_response(&query, state, None)),
    };
    // Even an accidentally credential-shaped repo name must not reflect the token.
    if repo.web_url.contains(&token) {
        return Ok(unavailable_response(
            &query,
            Availability::Unavailable,
            None,
        ));
    }
    if let Some(number) = query.number {
        let Ok(base) = Url::parse("https://api.github.com/") else {
            return Ok(details_response(empty_details(
                number,
                Availability::Unavailable,
            )));
        };
        let Ok(client) = client() else {
            return Ok(details_response(empty_details(
                number,
                Availability::Unavailable,
            )));
        };
        let details = match tokio::time::timeout(
            REQUEST_BUDGET,
            fetch_details(&client, &base, &repo, number, &token, svc),
        )
        .await
        {
            Ok(details) => details,
            Err(_) => empty_details(number, Availability::Unavailable),
        };
        return Ok(details_response(details));
    }
    let url = format!("https://api.github.com/repos/{}/pulls?state=open&sort=created&direction=desc&per_page={PAGE_SIZE}&page={}", repo.name, query.page);
    let page = match client().and_then(|c| {
        Url::parse(&url)
            .map(|u| (c, u))
            .map_err(|_| Availability::Unavailable)
    }) {
        Ok((client, url)) => fetch_page(&client, url, repo, query.page, Some(&token), svc).await,
        Err(state) => empty(query.page, state),
    };
    Ok(response(page))
}

fn client() -> Result<Client, Availability> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(REQUEST_BUDGET)
        .user_agent("git-vista-read-only-forge")
        .build()
        .map_err(|_| Availability::Unavailable)
}

#[derive(Deserialize)]
struct GithubPull {
    number: u64,
    title: String,
    draft: bool,
    state: String,
}
fn decode(
    body: &[u8],
    repo: ForgeRepository,
    page: u32,
    next_page: Option<u32>,
    token: Option<&str>,
) -> Result<ForgePage, Availability> {
    let pulls: Vec<GithubPull> =
        serde_json::from_slice(body).map_err(|_| Availability::Unavailable)?;
    if pulls.len() > PAGE_SIZE || pulls.iter().any(|p| p.number == 0 || p.state != "open") {
        return Err(Availability::Unavailable);
    }
    let pulls = pulls
        .into_iter()
        .map(|p| PullRequestSummary {
            number: p.number,
            title: match token.filter(|t| !t.is_empty()) {
                Some(t) => p.title.replace(t, "[redacted]"),
                None => p.title,
            },
            draft: p.draft,
            web_url: format!("{}/pull/{}", repo.web_url, p.number),
        })
        .collect();
    let mut result = empty(page, Availability::Ready);
    result.repository = Some(repo);
    result.pulls = pulls;
    result.next_page = next_page;
    Ok(result)
}

/// The Link header only tells us whether page+1 exists. Never follow its URL.
/// Validate the destination and paging shape before accepting that evidence.
fn next_page(link: Option<&str>, current: &Url, page: u32) -> Option<u32> {
    if page >= MAX_PAGE {
        return None;
    }
    for part in link?.split(',') {
        let mut sections = part.trim().split(';');
        let target = sections
            .next()?
            .trim()
            .strip_prefix('<')?
            .strip_suffix('>')?;
        if !sections.any(|s| {
            s.trim().strip_prefix("rel=").is_some_and(|r| {
                r.trim_matches('"')
                    .split_whitespace()
                    .any(|rel| rel == "next")
            })
        }) {
            continue;
        }
        let url = Url::parse(target).ok()?;
        if url.origin() != current.origin()
            || url.path() != current.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let pages: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "page")
            .map(|(_, v)| v.into_owned())
            .collect();
        if pages.len() == 1 && pages[0].parse::<u32>().ok() == Some(page + 1) {
            return Some(page + 1);
        }
    }
    None
}
fn header_number(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}
fn rate_delay(headers: &reqwest::header::HeaderMap) -> u64 {
    header_number(headers, "retry-after")
        .or_else(|| {
            header_number(headers, "x-ratelimit-reset").map(|reset| {
                reset.saturating_sub(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                )
            })
        })
        .unwrap_or(60)
        .clamp(1, 86400)
}
async fn fetch_page(
    client: &Client,
    url: Url,
    repo: ForgeRepository,
    page: u32,
    token: Option<&str>,
    svc: &ForgeService,
) -> ForgePage {
    let result = fetch_body(client, url.clone(), token, svc).await;
    let (headers, body) = match result {
        Ok(response) => response,
        Err(state) => {
            let mut page = empty(page, state);
            if state == Availability::RateLimited {
                page.retry_after_seconds = svc.retry_after();
            }
            return page;
        }
    };
    let next = next_page(
        headers.get("link").and_then(|s| s.to_str().ok()),
        &url,
        page,
    );
    decode(&body, repo, page, next, token).unwrap_or_else(|state| empty(page, state))
}

async fn fetch_body(
    client: &Client,
    url: Url,
    token: Option<&str>,
    svc: &ForgeService,
) -> Result<(reqwest::header::HeaderMap, Vec<u8>), Availability> {
    if svc.retry_after().is_some() {
        return Err(Availability::RateLimited);
    }
    let mut request = client
        .get(url.clone())
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2026-03-10");
    if let Some(token) = token {
        // Sensitive HeaderValue suppresses accidental debug exposure inside HTTP.
        let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        else {
            return Err(Availability::Unavailable);
        };
        value.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let Ok(mut upstream) = request.send().await else {
        return Err(Availability::Unavailable);
    };
    let status = upstream.status();
    let exhausted = header_number(upstream.headers(), "x-ratelimit-remaining") == Some(0);
    let limited = status.as_u16() == 429
        || (status.as_u16() == 403
            && (exhausted || upstream.headers().contains_key("retry-after")));
    if limited || exhausted {
        svc.cool_down(rate_delay(upstream.headers()));
    }
    if limited {
        return Err(Availability::RateLimited);
    }
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return Err(Availability::AccessUncertain);
    }
    if status.as_u16() != 200 {
        return Err(Availability::Unavailable);
    }
    let headers = upstream.headers().clone();
    if upstream
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(Availability::Unavailable);
    }
    let mut body = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= MAX_BODY => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return Err(Availability::Unavailable),
        }
    }
    Ok((headers, body))
}

#[derive(Deserialize)]
struct GithubPullDetails {
    number: u64,
    state: String,
    head: GithubHead,
}

#[derive(Deserialize)]
struct GithubHead {
    sha: String,
}

#[derive(Deserialize)]
struct GithubCheckPage {
    check_runs: Vec<GithubCheck>,
}

#[derive(Deserialize)]
struct GithubCheck {
    name: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct GithubReview {
    id: u64,
    user: Option<GithubUser>,
    state: String,
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

fn endpoint(base: &Url, repo: &ForgeRepository, suffix: &str) -> Result<Url, Availability> {
    base.join(&format!("repos/{}/{suffix}", repo.name))
        .map_err(|_| Availability::Unavailable)
}

fn safe_text(value: String, token: &str, max_chars: usize) -> Option<String> {
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.replace(token, "[redacted]"))
}

fn check_state(status: &str, conclusion: Option<&str>) -> CheckState {
    match (status, conclusion) {
        ("queued", _) => CheckState::Queued,
        ("in_progress", _) => CheckState::InProgress,
        ("completed", Some("success")) => CheckState::Success,
        ("completed", Some("failure" | "startup_failure")) => CheckState::Failure,
        ("completed", Some("neutral")) => CheckState::Neutral,
        ("completed", Some("cancelled")) => CheckState::Cancelled,
        ("completed", Some("skipped")) => CheckState::Skipped,
        ("completed", Some("timed_out")) => CheckState::TimedOut,
        ("completed", Some("action_required")) => CheckState::ActionRequired,
        ("completed", Some("stale")) => CheckState::Stale,
        _ => CheckState::Unknown,
    }
}

fn check_rollup(checks: &[CheckSummary]) -> CheckRollup {
    if checks.is_empty() {
        return CheckRollup::NoChecks;
    }
    if checks.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure
                | CheckState::Cancelled
                | CheckState::TimedOut
                | CheckState::ActionRequired
        )
    }) {
        CheckRollup::Failing
    } else if checks.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Queued | CheckState::InProgress | CheckState::Unknown
        )
    }) {
        CheckRollup::Pending
    } else {
        CheckRollup::Passing
    }
}

fn review_rollup(reviews: &[ReviewSummary]) -> ReviewRollup {
    if reviews.is_empty() {
        ReviewRollup::NoReviews
    } else if reviews
        .iter()
        .any(|review| review.state == ReviewState::ChangesRequested)
    {
        ReviewRollup::ChangesRequested
    } else if reviews
        .iter()
        .any(|review| review.state == ReviewState::Approved)
    {
        ReviewRollup::Approved
    } else {
        ReviewRollup::Reviewed
    }
}

async fn fetch_checks(
    client: &Client,
    base: &Url,
    repo: &ForgeRepository,
    sha: &str,
    token: &str,
    svc: &ForgeService,
) -> Result<Vec<CheckSummary>, Availability> {
    let mut checks = Vec::new();
    for page in 1..=MAX_DETAIL_PAGES {
        let mut url = endpoint(base, repo, &format!("commits/{sha}/check-runs"))?;
        url.query_pairs_mut()
            .append_pair("filter", "latest")
            .append_pair("per_page", &DETAIL_PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let (headers, body) = fetch_body(client, url.clone(), Some(token), svc).await?;
        let payload: GithubCheckPage =
            serde_json::from_slice(&body).map_err(|_| Availability::Unavailable)?;
        if payload.check_runs.len() > DETAIL_PAGE_SIZE {
            return Err(Availability::Unavailable);
        }
        for check in payload.check_runs {
            let name = safe_text(check.name, token, 500).ok_or(Availability::Unavailable)?;
            checks.push(CheckSummary {
                name,
                state: check_state(&check.status, check.conclusion.as_deref()),
            });
        }
        let next = next_page(
            headers.get("link").and_then(|value| value.to_str().ok()),
            &url,
            page,
        );
        match next {
            None => return Ok(checks),
            Some(_) if page == MAX_DETAIL_PAGES => return Err(Availability::Unavailable),
            Some(_) => {}
        }
    }
    Err(Availability::Unavailable)
}

async fn fetch_reviews(
    client: &Client,
    base: &Url,
    repo: &ForgeRepository,
    number: u64,
    token: &str,
    svc: &ForgeService,
) -> Result<Vec<ReviewSummary>, Availability> {
    let mut latest = BTreeMap::<String, (u64, ReviewState)>::new();
    for page in 1..=MAX_DETAIL_PAGES {
        let mut url = endpoint(base, repo, &format!("pulls/{number}/reviews"))?;
        url.query_pairs_mut()
            .append_pair("per_page", &DETAIL_PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let (headers, body) = fetch_body(client, url.clone(), Some(token), svc).await?;
        let payload: Vec<GithubReview> =
            serde_json::from_slice(&body).map_err(|_| Availability::Unavailable)?;
        if payload.len() > DETAIL_PAGE_SIZE {
            return Err(Availability::Unavailable);
        }
        for review in payload {
            let Some(user) = review.user else { continue };
            let reviewer = safe_text(user.login, token, 100).ok_or(Availability::Unavailable)?;
            match review.state.as_str() {
                "APPROVED" => {
                    latest.insert(reviewer, (review.id, ReviewState::Approved));
                }
                "CHANGES_REQUESTED" => {
                    latest.insert(reviewer, (review.id, ReviewState::ChangesRequested));
                }
                "COMMENTED" => {
                    latest
                        .entry(reviewer)
                        .or_insert((review.id, ReviewState::Commented));
                }
                "DISMISSED" => {
                    latest.remove(&reviewer);
                }
                _ => {}
            }
        }
        let next = next_page(
            headers.get("link").and_then(|value| value.to_str().ok()),
            &url,
            page,
        );
        match next {
            None => {
                return Ok(latest
                    .into_iter()
                    .map(|(reviewer, (_, state))| ReviewSummary { reviewer, state })
                    .collect())
            }
            Some(_) if page == MAX_DETAIL_PAGES => return Err(Availability::Unavailable),
            Some(_) => {}
        }
    }
    Err(Availability::Unavailable)
}

async fn fetch_details(
    client: &Client,
    base: &Url,
    repo: &ForgeRepository,
    number: u64,
    token: &str,
    svc: &ForgeService,
) -> PullDetails {
    let result = async {
        let url = endpoint(base, repo, &format!("pulls/{number}"))?;
        let (_, body) = fetch_body(client, url, Some(token), svc).await?;
        let pull: GithubPullDetails =
            serde_json::from_slice(&body).map_err(|_| Availability::Unavailable)?;
        if pull.number != number
            || !matches!(pull.state.as_str(), "open" | "closed")
            || !matches!(pull.head.sha.len(), 40 | 64)
            || !pull.head.sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Availability::Unavailable);
        }
        let checks = fetch_checks(client, base, repo, &pull.head.sha, token, svc).await?;
        let reviews = fetch_reviews(client, base, repo, number, token, svc).await?;
        Ok::<_, Availability>((checks, reviews))
    }
    .await;
    match result {
        Ok((checks, reviews)) => PullDetails {
            number,
            availability: Availability::Ready,
            check_rollup: check_rollup(&checks),
            review_rollup: review_rollup(&reviews),
            checks,
            reviews,
            retry_after_seconds: None,
        },
        Err(state) => {
            let mut details = empty_details(number, state);
            if state == Availability::RateLimited {
                details.retry_after_seconds = svc.retry_after();
            }
            details
        }
    }
}

#[cfg(test)]
mod tests;
