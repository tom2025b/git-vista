//! GitHub adapter at the handler boundary (#89). Only neutral DTOs leave here.
//! No local Git lock spans provider I/O; admission is fail-fast and independent.
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
use serde::Deserialize;
use tokio::sync::Semaphore;

const PAGE_SIZE: usize = 30;
const MAX_BODY: usize = 1024 * 1024;
const MAX_PAGE: u32 = 10_000;
const REQUEST_BUDGET: Duration = Duration::from_secs(8);
const TOKEN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
pub(crate) struct PullQuery {
    repo: Option<String>,
    #[serde(default = "first_page")]
    page: u32,
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

/// Mapping accepts only a normalized github.com owner/repo pair. API and web
/// links are constructed here; neither browser input nor upstream links supply
/// a credential destination. Path/query/fragment injection is rejected.
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
    ForgePage {
        repository: None,
        availability,
        capabilities: ForgeCapabilities {
            summaries: availability != Availability::Unsupported,
            checks: false,
            reviews: false,
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

pub(crate) async fn pulls(
    Query(query): Query<PullQuery>,
) -> Result<Response, (StatusCode, String)> {
    if !(1..=MAX_PAGE).contains(&query.page) {
        return Err((StatusCode::BAD_REQUEST, "Invalid pull request page.".into()));
    }
    let path = super::read::resolve_repo(query.repo.as_deref())?.0;
    let svc = service();
    let Ok(_request) = svc.requests.clone().try_acquire_owned() else {
        return Ok(response(empty(query.page, Availability::Unavailable)));
    };
    let base = tokio::task::spawn_blocking(move || git_vista_git::origin_url(&path))
        .await
        .ok()
        .flatten();
    let Some(repo) = base.as_deref().and_then(repository_from_remote) else {
        return Ok(response(empty(query.page, Availability::Unsupported)));
    };
    if let Some(seconds) = svc.retry_after() {
        let mut page = empty(query.page, Availability::RateLimited);
        page.retry_after_seconds = Some(seconds);
        return Ok(response(page));
    }
    let token = match svc
        .token_with(crate::state::credential_token, TOKEN_BUDGET)
        .await
    {
        Ok(token) => token,
        Err(state) => return Ok(response(empty(query.page, state))),
    };
    // Even an accidentally credential-shaped repo name must not reflect the token.
    if token
        .as_deref()
        .is_some_and(|t| !t.is_empty() && repo.web_url.contains(t))
    {
        return Ok(response(empty(query.page, Availability::Unavailable)));
    }
    let url = format!("https://api.github.com/repos/{}/pulls?state=open&sort=created&direction=desc&per_page={PAGE_SIZE}&page={}", repo.name, query.page);
    let page = match client().and_then(|c| {
        Url::parse(&url)
            .map(|u| (c, u))
            .map_err(|_| Availability::Unavailable)
    }) {
        Ok((client, url)) => {
            fetch_page(&client, url, repo, query.page, token.as_deref(), svc).await
        }
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
    let mut request = client
        .get(url.clone())
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2026-03-10");
    if let Some(token) = token {
        // Sensitive HeaderValue suppresses accidental debug exposure inside HTTP.
        let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        else {
            return empty(page, Availability::Unavailable);
        };
        value.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let Ok(mut upstream) = request.send().await else {
        return empty(page, Availability::Unavailable);
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
        let mut result = empty(page, Availability::RateLimited);
        result.retry_after_seconds = svc.retry_after();
        return result;
    }
    if matches!(status.as_u16(), 401 | 403 | 404) {
        return empty(page, Availability::AccessUncertain);
    }
    if status.as_u16() != 200 {
        return empty(page, Availability::Unavailable);
    }
    let next = next_page(
        upstream.headers().get("link").and_then(|s| s.to_str().ok()),
        &url,
        page,
    );
    if upstream
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return empty(page, Availability::Unavailable);
    }
    let mut body = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= MAX_BODY => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return empty(page, Availability::Unavailable),
        }
    }
    decode(&body, repo, page, next, token).unwrap_or_else(|state| empty(page, state))
}

#[cfg(test)]
mod tests;
