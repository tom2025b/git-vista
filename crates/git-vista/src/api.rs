//! Backend HTTP calls — every `fetch`/`POST` the frontend makes.
//!
//! All URLs are relative, so they hit the same origin as the served SPA (no
//! CORS, no hardcoded host). The read endpoints cache-bust with a `t=<ms>`
//! query param: the backend already sends `Cache-Control: no-store`, but a
//! unique URL each call is belt-and-braces against iOS Safari's persistent
//! cache serving a stale response (so a branch created since the last launch
//! never shows). The write endpoints forward git's own error text verbatim on
//! failure, so the UI can show the real reason. Pure data plumbing — no UI —
//! so this stays testable on its own away from the view code.

use std::fmt;

use gloo_net::http::{Request, RequestBuilder};

use git_vista_core::activity::{ActivityEvent, UndoAction, Undoable};
use git_vista_core::diff::{CommitDiff, FileContent};
use git_vista_core::model::CommitDetail;
use git_vista_core::net::network_error_text;
use git_vista_core::status::RepoStatus;
use git_vista_protocol::{
    ApiError, BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest,
    DeleteCloneRequest, ProtocolInfo, RebaseStatus, RepoMode, RepositoryDescriptor, SelectRequest,
    SessionInfo, SessionRequest, CSRF_HEADER, IDEMPOTENCY_HEADER, PROTOCOL_HEADER,
    PROTOCOL_VERSION,
};

use crate::features::session::signals as session_state;
use crate::history::{Frame, Page};

/// The largest page the server will mint, mirrored here so a caller's request is
/// clamped before it goes out rather than silently rewritten server-side. Kept
/// in step with `MAX_PAGE_LIMIT` in `git-vista-server`'s read handlers.
const MAX_PAGE_LIMIT: usize = 1_000;

/// The ADR 0005 client-side counterpart of the LAN listener's structural
/// read-only-ness: clone/select/rescan/delete refuse up front on a LAN-view
/// session with a clear reason, instead of surfacing the bare `405` the
/// route-less LAN listener answers with. The absent server route remains the
/// actual boundary.
fn refuse_if_lan_view() -> Result<(), String> {
    if session_state::is_lan() {
        Err("This is a read-only LAN view session — open the localhost \
             (SSH-tunnel) link to clone, rescan, or switch repositories."
            .to_string())
    } else {
        Ok(())
    }
}

/// The ADR 0007 client-side write chokepoint: every repo-write function refuses
/// up front in Visualize mode, so a gating gap in the UI can't even attempt a
/// mutation. The server's own 403 remains the actual boundary.
fn refuse_if_visualize() -> Result<(), String> {
    if session_state::refuses_writes() {
        Err("This repository is open in Visualize mode — look-only.".to_string())
    } else {
        Ok(())
    }
}

/// Start a GET carrying the protocol header every `/api/*` request must send
/// (M1.02): the server refuses a call without it, so every read goes through
/// here rather than `Request::get` directly. The session cookie rides along
/// automatically — same-origin `fetch` sends it — so reads need no extra header.
fn req_get(url: &str) -> RequestBuilder {
    Request::get(url).header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string())
}

/// Start a POST carrying the protocol header (see [`req_get`]) and, when a session
/// is established, the CSRF token (M1.04): the server refuses a state-changing
/// request whose CSRF header doesn't match the session, so every write goes
/// through here. The session cookie is sent automatically (same-origin).
fn req_post(url: &str) -> RequestBuilder {
    let builder = Request::post(url).header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string());
    match session_state::csrf_token() {
        Some(token) => builder.header(CSRF_HEADER, &token),
        None => builder,
    }
}

/// A fresh idempotency key: this client's name for **one user action** (M1.08,
/// #61). The server refuses a write without one, records the operation under
/// it, and replays the recorded result for any request that repeats it.
///
/// Unique, not unguessable — the key is the client's own name for its own
/// intent, and the session cookie is what authorises the request. Millisecond
/// clock for cross-tab distinctness, a per-tab counter so two actions inside
/// one millisecond still differ, and a random draw so two tabs opened in the
/// same millisecond don't collide. Token-shaped (`[A-Za-z0-9-]`) and far inside
/// the server's length cap.
fn new_idempotency_key() -> String {
    thread_local! {
        static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    let n = COUNTER.with(|c| {
        let next = c.get().wrapping_add(1);
        c.set(next);
        next
    });
    let ms = js_sys::Date::now() as u64;
    let noise = (js_sys::Math::random() * 4_294_967_296.0) as u64;
    format!("gv-{ms:x}-{n:x}-{noise:x}")
}

/// Send one write, **retried once on a network-level failure, under the same
/// idempotency key**.
///
/// The retry exists for one concrete failure: iPad Safari re-using a pooled TCP
/// socket that died silently while the tab was suspended (and, the same shape,
/// an SSH tunnel dropping mid-request). The first attempt evicts that socket, so
/// the second goes out on a fresh connection — no delay needed.
///
/// Before M1.08 that retry was only safe on `POST /api/branch`, where a
/// duplicate is harmless. Now it is safe *everywhere*, and for the reason the
/// key exists: both attempts name the same operation, so if the first one did
/// land, the server replays its recorded result instead of running git twice.
/// The key is minted **once, here** — minting inside the attempt would make the
/// retry a second intent and give back exactly the double-commit this protects
/// against.
///
/// Only network errors are retried. An HTTP error is an answer, and answers are
/// returned to the caller.
async fn send_write(url: &str, body: Option<String>) -> Result<gloo_net::http::Response, String> {
    let key = new_idempotency_key();
    let attempt = || async {
        let builder = req_post(url).header(IDEMPOTENCY_HEADER, &key);
        match &body {
            Some(json) => builder
                .header("content-type", "application/json")
                .body(json.clone())
                .map_err(|e| e.to_string())?
                .send()
                .await
                .map_err(network_error),
            None => builder.send().await.map_err(network_error),
        }
    };
    match attempt().await {
        Ok(resp) => Ok(resp),
        Err(_) => attempt().await,
    }
}

/// [`send_write`] with a JSON body.
async fn write_json<T: serde::Serialize>(
    url: &str,
    body: &T,
) -> Result<gloo_net::http::Response, String> {
    let json = serde_json::to_string(body).map_err(|e| e.to_string())?;
    send_write(url, Some(json)).await
}

/// [`send_write`] with no body — the bodyless writes (stage, unstage, rebase…).
async fn write_empty(url: &str) -> Result<gloo_net::http::Response, String> {
    send_write(url, None).await
}

/// Exchange a one-time bootstrap token for a session (`POST /api/session`, M1.04).
/// On success the server sets the HttpOnly session cookie and returns the CSRF
/// token; a `401` means the token was wrong or expired. The token travels in the
/// JSON body, never the URL, so it can't land in a server log.
pub async fn post_session(token: &str) -> Result<SessionInfo, String> {
    let body = SessionRequest {
        token: token.to_string(),
    };
    let resp = req_post("/api/session")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        resp.json::<SessionInfo>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Report the current session state (`GET /api/session`, M1.04): whether the
/// browser's cookie still names a live session, and its CSRF token if so. Hit on
/// load (and after a failed bootstrap) so a reload recovers the session — and the
/// CSRF token writes need — without re-exchanging a token.
pub async fn get_session() -> Result<SessionInfo, String> {
    let url = format!("/api/session?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<SessionInfo>()
        .await
        .map_err(|e| e.to_string())
}

/// Fetch the server's protocol contract (`GET /api/protocol`, M1.02): the
/// current protocol version and the `[min, max]` client-version window it
/// accepts. Hit at startup — and on every reload — so the app can raise an
/// "Update Required" screen instead of silently talking to an incompatible
/// server. This endpoint needs no protocol header; sending it is harmless.
pub async fn fetch_protocol() -> Result<ProtocolInfo, String> {
    let url = format!("/api/protocol?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<ProtocolInfo>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Map a `send()`-level failure — the request never completed — to the
/// actionable message built in core, instead of Safari's bare "TypeError:
/// Load failed" (the server stopped, the Wi-Fi changed, or iOS suspended the
/// tab and Safari re-used a dead pooled socket). Only the network hop gets
/// this treatment: body-serialization and response-parse errors stay verbatim,
/// since those mean a bug, not an unreachable server.
fn network_error(e: gloo_net::Error) -> String {
    network_error_text(&e.to_string())
}

/// Turn the versioned server error envelope into the message the UI should show.
/// Falling back to the raw body preserves useful errors from an older server.
async fn response_error(resp: gloo_net::http::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if let Ok(error) = serde_json::from_str::<ApiError>(&body) {
        format!("{} (request {})", error.error.message, error.request_id)
    } else if body.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        body
    }
}

/// Why a history read failed (M1.10, #63).
///
/// The old whole-graph read flattened every failure into a `String`, which is
/// exactly what paged history can't afford: the *status* is the instruction. A
/// `409` means history moved under us (reseed from a fresh Frame), a `400` on a
/// cursor request means the server refused that cursor (never send it again),
/// and everything else is retryable with the same cursor. So the status survives
/// the call instead of being formatted away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryFetchError {
    Network(String),
    Http { status: u16, message: String },
    Decode(String),
}

impl fmt::Display for HistoryFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Both already carry a message written for a human — the network
            // hint from core, or the server's own error text.
            Self::Network(message) | Self::Http { message, .. } => f.write_str(message),
            // A body we couldn't parse is nobody's fault but ours, so name the
            // failure rather than showing a bare serde message on its own.
            Self::Decode(message) => write!(f, "Invalid history response: {message}"),
        }
    }
}

/// Percent-encode one query-parameter value, so an opaque id or signed cursor
/// containing `&`, `=`, `+` or `/` can't cut the query short or be silently
/// re-read as a different parameter.
fn encode_component(value: &str) -> String {
    js_sys::encode_uri_component(value)
        .as_string()
        .unwrap_or_default()
}

/// Decode one history representation, **checking the status first**.
///
/// The status has to be read off the response before the body is consumed, and
/// preserved: a `409`'s body is not a decode failure, and reporting it as one
/// would hide the one signal the drift path keys on. 304 is never interpreted —
/// these requests send no `If-None-Match`, so the server has nothing to match
/// and a 304 would be a protocol violation, not a cache hit.
async fn history_json<T: serde::de::DeserializeOwned>(
    resp: gloo_net::http::Response,
) -> Result<T, HistoryFetchError> {
    if !resp.ok() {
        let status = resp.status();
        let message = response_error(resp).await;
        return Err(HistoryFetchError::Http { status, message });
    }
    resp.json::<T>()
        .await
        .map_err(|e| HistoryFetchError::Decode(e.to_string()))
}

/// Fetch the once-per-view [`Frame`] (`GET /api/frame`, M1.10): refs, branch
/// colours and the repo's own metadata — no commits at all. Relative URL → same
/// origin as the served SPA, cache-busted with `t=<ms>` like every other read
/// (the backend ignores the param; iOS Safari's persistent cache does not).
///
/// No `?repo=` selector: the Frame *is* what resolves the view's target, and
/// every page fetched after it pins that answer via
/// [`Frame::worktree_id`](git_vista_protocol::HistoryFrame::worktree_id).
pub async fn fetch_frame() -> Result<Frame, HistoryFetchError> {
    let url = format!("/api/frame?t={}", js_sys::Date::now());
    let resp = req_get(&url)
        .send()
        .await
        .map_err(|e| HistoryFetchError::Network(network_error(e)))?;
    history_json(resp).await
}

/// Fetch one page of history (`GET /api/commits`, M1.10): rows, edges and stubs
/// plus the cursor for the next page, or `None` once history is exhausted.
///
/// `repo` is the accepted Frame's `worktree_id` — passed on page 1 *and* on
/// every append, so a server whose default selection changes mid-scroll can't
/// splice another repository's rows onto this graph. A degraded Frame has no id
/// and passes `None`, which keeps the server's default-selection behaviour.
/// `cursor` is `None` for page 1. Both are percent-encoded; `limit` is clamped
/// into the server's own accepted range.
pub async fn fetch_page(
    repo: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page, HistoryFetchError> {
    // Built by appending, not by `format!`ing a fixed shape: an absent selector
    // must be *omitted*, not sent empty — an empty `?repo=` is a different
    // request from no `?repo=` at all.
    let mut url = String::from("/api/commits?");
    if let Some(repo) = repo {
        url.push_str(&format!("repo={}&", encode_component(repo)));
    }
    if let Some(cursor) = cursor {
        url.push_str(&format!("cursor={}&", encode_component(cursor)));
    }
    url.push_str(&format!(
        "limit={}&t={}",
        limit.clamp(1, MAX_PAGE_LIMIT),
        js_sys::Date::now()
    ));
    let resp = req_get(&url)
        .send()
        .await
        .map_err(|e| HistoryFetchError::Network(network_error(e)))?;
    history_json(resp).await
}

/// Fetch one commit's full detail for the side panel (Phase 10,
/// `GET /api/commit/<id>`). Same-origin relative URL, cache-busted like the graph
/// fetch. A non-2xx body is the server's reason (e.g. "No such commit."),
/// returned as `Err` for the panel to show.
pub async fn fetch_commit_detail(id: &str) -> Result<CommitDetail, String> {
    let url = format!("/api/commit/{id}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDetail>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to clone a public URL into the persistent clones store
/// (`POST /api/clone`, ADR 0008). `Ok` carries the fresh clone's descriptor so
/// the caller can jump straight to the Visualize/Active mode screen for it. On
/// a non-2xx response the body is the server's / git's own error text (bad
/// URL, repo not found, …), returned as `Err`.
pub async fn clone_request(url: &str) -> Result<RepositoryDescriptor, String> {
    refuse_if_lan_view()?;
    let body = CloneRequest {
        url: url.to_string(),
    };
    let resp = write_json("/api/clone", &body).await?;
    if resp.ok() {
        resp.json::<RepositoryDescriptor>()
            .await
            .map_err(|e| e.to_string())
    } else {
        // `response_error` (not raw body text): an empty error body — the
        // LAN listener's bare 405, say — must still say *something*.
        Err(response_error(resp).await)
    }
}

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the body is git's own error text, returned as `Err` so
/// the caller can show the real reason (branch exists, bad name, …).
///
/// The network-failure retry that used to live here is now [`send_write`]'s,
/// for every write rather than only this one: since M1.08 both attempts carry
/// the same idempotency key, so a duplicate is replayed rather than re-run.
pub async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
    refuse_if_visualize()?;
    let body = CreateBranchRequest {
        name: name.to_string(),
        commit: commit.to_string(),
    };
    let resp = write_json("/api/branch", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to create a commit (Issue #33, `POST /api/commit`).
/// `allow_empty` picks `git commit --allow-empty` (empty commit) vs a plain
/// `git commit` (staged changes). `branch` targets a branch other than the
/// checked-out one — the branch-stub path, empty commits only; `None` commits
/// on HEAD as before. As with the branch request, a non-2xx body is git's own
/// error text, returned as `Err`.
pub async fn create_commit_request(
    message: &str,
    allow_empty: bool,
    branch: Option<&str>,
) -> Result<(), String> {
    refuse_if_visualize()?;
    let body = CreateCommitRequest {
        message: message.to_string(),
        allow_empty,
        branch: branch.map(str::to_string),
    };
    let resp = write_json("/api/commit", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to stage all working-tree changes (`POST /api/stage`) — a
/// plain `git add -A`, so modified/new/deleted files move into the index and can
/// then be committed. Bodyless, like the rebase request; a non-2xx body is git's
/// own error text, returned as `Err` for the caller to show.
pub async fn stage_request() -> Result<(), String> {
    refuse_if_visualize()?;
    let resp = write_empty("/api/stage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to unstage everything (`POST /api/unstage`) — a plain
/// `git reset HEAD`, the exact inverse of [`stage_request`]: the index goes
/// back to HEAD, the working tree keeps every edit. Same bodyless shape and
/// error posture as staging.
pub async fn unstage_request() -> Result<(), String> {
    refuse_if_visualize()?;
    let resp = write_empty("/api/unstage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the live checked-out branch (Issue #33 follow-up), used to name the merge
/// target the moment the user clicks "Merge" — so it's correct even if the graph on
/// screen predates a branch switch. `Ok(None)` => detached HEAD. Cache-busted like
/// the graph fetch, since the answer changes as branches are switched.
pub async fn fetch_head_branch() -> Result<Option<String>, String> {
    let url = format!("/api/head-branch?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<Option<String>>()
        .await
        .map_err(|e| e.to_string())
}

/// Fetch the activity feed (`GET /api/activity`): the chronological list of
/// repo events — commits, merges, rebases, branch creations/deletions,
/// pushes… — each attributed app-vs-terminal and carrying an undo hint when
/// the event is still undoable. Fetched fresh every time the panel opens,
/// cache-busted like the other live reads.
pub async fn fetch_activity(limit: usize) -> Result<Vec<ActivityEvent>, String> {
    let url = format!("/api/activity?limit={limit}&t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<ActivityEvent>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the undo actions that apply to one commit (`GET /api/undoables/{id}`,
/// Activity/Undo step 5), computed live server-side — so the context menu's
/// undo section reflects the repo *now*, not the possibly-stale graph. Empty
/// on a read-only clone. Cache-busted like the other live reads.
pub async fn fetch_undoables(commit: &str) -> Result<Vec<Undoable>, String> {
    let url = format!("/api/undoables/{commit}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<Undoable>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to execute one undo action (`POST /api/undo`, Activity/Undo
/// step 5). The body is the tagged [`UndoAction`] exactly as the server handed
/// it out. A non-2xx body is the server's reason — including the 409s for a
/// moved branch (compare-and-swap) or a dirty working tree — returned as `Err`
/// for the confirm flow to show.
pub async fn undo_request(action: &UndoAction) -> Result<(), String> {
    refuse_if_visualize()?;
    let resp = write_json("/api/undo", action).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch one commit's diff — file list + unified patch — for the detail
/// panel's Changes section (`GET /api/diff/{id}`). Fetched lazily alongside
/// the commit detail, cache-busted the same way. A non-2xx body is the
/// server's reason, returned as `Err` for the panel to show.
pub async fn fetch_diff(id: &str) -> Result<CommitDiff, String> {
    let url = format!("/api/diff/{id}?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch one commit's diff with the patch cap lifted (`GET /api/diff/{id}?full=1`)
/// for the full-screen viewer — the panel's capped fetch is [`fetch_diff`].
pub async fn fetch_diff_full(id: &str) -> Result<CommitDiff, String> {
    let url = format!("/api/diff/{id}?full=1&t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<CommitDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch one file's full content at one commit (`GET /api/file/{id}/{path}`)
/// for the full file viewer. The path rides in the URL path (encoded per
/// segment so `#`/`?` in a filename can't cut the request short); slashes stay
/// literal — the server's wildcard route consumes them.
pub async fn fetch_file(id: &str, path: &str) -> Result<FileContent, String> {
    let encoded: Vec<String> = path
        .split('/')
        .map(|seg| {
            js_sys::encode_uri_component(seg)
                .as_string()
                .unwrap_or_default()
        })
        .collect();
    let url = format!(
        "/api/file/{id}/{}?t={}",
        encoded.join("/"),
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<FileContent>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch the live working-tree status (`GET /api/status`) — branch, ahead/
/// behind, and the dirty-file lists — for the topbar chip and the Activity
/// panel's status section. Resolved fresh server-side per request and cache-
/// busted like the other live reads, since it changes with every edit.
pub async fn fetch_status() -> Result<RepoStatus, String> {
    let url = format!("/api/status?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<RepoStatus>().await.map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Fetch whether "Rebase onto main" would do anything right now
/// (`GET /api/rebase-status`): the checked-out branch, the base the server
/// would use (`origin/main` vs `main`), and whether HEAD is already based on
/// it. Fetched live when the menu opens — like `fetch_undoables` — so the
/// item's enabled state reflects the repo *now*, not the possibly-stale graph.
pub async fn fetch_rebase_status() -> Result<RebaseStatus, String> {
    let url = format!("/api/rebase-status?t={}", js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<RebaseStatus>()
        .await
        .map_err(|e| e.to_string())
}

/// Ask the backend to rebase the checked-out branch onto main (`POST /api/rebase`).
/// Unlike the branch ops it carries no body — it always acts on the current HEAD,
/// and the server picks `origin/main` vs `main` as the base. `Ok` carries the
/// server's success line so the caller can tell a real rebase from the
/// "Already up to date" no-op (a raced click from a stale menu). A non-2xx body
/// is git's own error text (conflicts, detached HEAD, …), returned as `Err`.
pub async fn rebase_request() -> Result<String, String> {
    refuse_if_visualize()?;
    let resp = write_empty("/api/rebase").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to reset a seeded *test repo* to its recorded state
/// (`POST /api/reset-test-repo`). Only offered when the graph said
/// `resettable` (the repo was opted in with `gv --seed`). `Ok` carries the
/// server's summary line ("… 2 branches restored, 1 deleted, HEAD → ‘main’");
/// a non-2xx body is the server's reason (not a test repo, corrupt seed, or
/// the exact git step that refused), returned as `Err` for the dialog to show.
pub async fn reset_test_repo_request() -> Result<String, String> {
    refuse_if_visualize()?;
    let resp = write_empty("/api/reset-test-repo").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to run a branch operation on `branch` (Issue #33 follow-up).
/// `path` is the endpoint — `/api/merge`, `/api/push`, `/api/delete-branch`, or
/// `/api/force-delete-branch` — all of which take the same `{ branch }` body. As with the other requests, a
/// non-2xx body is git's own error text, returned as `Err` for the caller to show.
/// `Ok` carries the server's success line — most callers ignore it, but the merge
/// flow reads it to tell a real merge from git's "Already up to date" no-op.
pub async fn branch_op_request(path: &str, branch: &str) -> Result<String, String> {
    refuse_if_visualize()?;
    let body = BranchRequest {
        branch: branch.to_string(),
    };
    let resp = write_json(path, &body).await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// The servable repositories (`GET /api/catalog`) — M1.03 built the endpoint,
/// the repo picker finally consumes it. Cache-busted like every live read: the
/// catalog changes at runtime (clones, rescans).
pub async fn fetch_catalog() -> Result<Vec<RepositoryDescriptor>, String> {
    let url = format!("/api/catalog?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<RepositoryDescriptor>>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Make `worktree` the current repo in `mode` (`POST /api/select`, ADR 0007).
/// A forged/unknown id comes back 404 from the fail-closed catalog; the picker
/// shows the server's reason.
pub async fn select_request(worktree: &str, mode: RepoMode) -> Result<(), String> {
    refuse_if_lan_view()?;
    let body = SelectRequest {
        worktree: worktree.to_string(),
        mode,
    };
    let resp = write_json("/api/select", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}

/// Re-scan the configured repo root (`POST /api/rescan`, ADR 0009). `Ok` carries
/// the server's one-line summary for the picker to show.
pub async fn rescan_request() -> Result<String, String> {
    refuse_if_lan_view()?;
    let resp = write_empty("/api/rescan").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}

/// Delete a persistent clone by id (`POST /api/delete-clone`, ADR 0008). `Ok`
/// carries the server's confirmation line for the picker; refusals (not a
/// clone, currently open, unknown id) come back as `Err` with the reason.
pub async fn delete_clone_request(worktree: &str) -> Result<String, String> {
    refuse_if_lan_view()?;
    let body = DeleteCloneRequest {
        worktree: worktree.to_string(),
    };
    let resp = write_json("/api/delete-clone", &body).await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}
