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

use std::cell::RefCell;

use gloo_net::http::{Request, RequestBuilder};

use git_vista_core::activity::{ActivityEvent, UndoAction, Undoable};
use git_vista_core::diff::{CommitDiff, FileContent};
use git_vista_core::model::{CommitDetail, Graph};
use git_vista_core::net::network_error_text;
use git_vista_core::status::RepoStatus;
use git_vista_protocol::{
    ApiError, BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest,
    DeleteCloneRequest, ProtocolInfo, RebaseStatus, RepoMode, RepositoryDescriptor, SelectRequest,
    SessionInfo, SessionRequest, CSRF_HEADER, PROTOCOL_HEADER, PROTOCOL_VERSION,
};

// The current session's CSRF token (M1.04). Set once the session is established
// (`POST`/`GET /api/session`), then echoed in the [`CSRF_HEADER`] on every write —
// the server refuses a state-changing request without it. A `thread_local` is all
// we need: wasm is single-threaded, and the token is per-tab, not persisted.
thread_local! {
    static CSRF_TOKEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Record (or clear) the session's CSRF token — called by [`crate::session`] after
/// establishing or checking the session. `None` clears it (logged out / no session).
pub fn set_csrf_token(token: Option<String>) {
    CSRF_TOKEN.with(|c| *c.borrow_mut() = token);
}

fn csrf_token() -> Option<String> {
    CSRF_TOKEN.with(|c| c.borrow().clone())
}

// The mode the current repo is open in (ADR 0006/0007), mirrored from the last
// graph load / selection. Purely defense in depth: in Visualize the write fns
// below refuse before any network call; the server's 403 is the real boundary.
thread_local! {
    static UI_MODE: RefCell<Option<RepoMode>> = const { RefCell::new(None) };
}

/// Record the current repo's mode — set when a graph lands and when a selection
/// is made. `None` clears it (unknown, e.g. before the first load).
pub fn set_ui_mode(mode: Option<RepoMode>) {
    UI_MODE.with(|m| *m.borrow_mut() = mode);
}

// Whether the current session came through the LAN listener (ADR 0005) —
// mirrored from the session-establish/-check response. Purely a UI signal: it
// drives hiding the Active option on the mode screen. The server's own route
// absence on the LAN listener is the actual write boundary.
thread_local! {
    static VIA_LAN: RefCell<bool> = const { RefCell::new(false) };
}

/// Record whether the current session is LAN-scoped — called by
/// [`crate::session`] after establishing or checking the session.
pub fn set_via_lan(via_lan: bool) {
    VIA_LAN.with(|v| *v.borrow_mut() = via_lan);
}

/// Whether the current session came through the LAN listener (ADR 0005).
pub fn is_lan_session() -> bool {
    VIA_LAN.with(|v| *v.borrow())
}

/// The ADR 0007 client-side write chokepoint: every repo-write function refuses
/// up front in Visualize mode, so a gating gap in the UI can't even attempt a
/// mutation. The server's own 403 remains the actual boundary.
fn refuse_if_visualize() -> Result<(), String> {
    let visualize = UI_MODE.with(|m| *m.borrow() == Some(RepoMode::Visualize));
    if visualize {
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
    match csrf_token() {
        Some(token) => builder.header(CSRF_HEADER, &token),
        None => builder,
    }
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

/// Fetch the laid-out graph from the backend. Relative URL → same origin as the
/// served SPA, so no CORS and no hardcoded host.
///
/// The URL carries a per-load cache-busting `t=<ms>` param: the backend already
/// sends `Cache-Control: no-store`, but a unique URL each launch is belt-and-
/// braces against iOS Safari's persistent cache serving a stale graph (so a branch
/// created since the last launch never shows). The backend ignores the param.
pub async fn fetch_graph() -> Result<Graph, String> {
    let url = format!("/api/commits?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Graph>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
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
    let body = CloneRequest {
        url: url.to_string(),
    };
    let resp = req_post("/api/clone")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        resp.json::<RepositoryDescriptor>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(resp
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", resp.status())))
    }
}

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the body is git's own error text, returned as `Err` so
/// the caller can show the real reason (branch exists, bad name, …).
///
/// A network-level send failure gets ONE automatic retry: the classic iPad
/// cause is Safari re-using a pooled TCP socket that silently died while the
/// tab was suspended — the failed attempt evicts that socket, so the second
/// attempt goes out on a fresh connection (no delay needed). The retry is safe
/// for *this* endpoint because a duplicated `git branch` is harmless: if the
/// first request did land, the retry just returns git's own "already exists".
pub async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
    refuse_if_visualize()?;
    let body = CreateBranchRequest {
        name: name.to_string(),
        commit: commit.to_string(),
    };
    let send = || async {
        req_post("/api/branch")
            .json(&body)
            .map_err(|e| e.to_string())?
            .send()
            .await
            .map_err(network_error)
    };
    let resp = match send().await {
        Ok(resp) => resp,
        Err(_) => send().await?,
    };
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
    let resp = req_post("/api/commit")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
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
    let resp = req_post("/api/stage").send().await.map_err(network_error)?;
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
    let resp = req_post("/api/unstage")
        .send()
        .await
        .map_err(network_error)?;
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
    let resp = req_post("/api/undo")
        .json(action)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
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
    let resp = req_post("/api/rebase")
        .send()
        .await
        .map_err(network_error)?;
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
    let resp = req_post("/api/reset-test-repo")
        .send()
        .await
        .map_err(network_error)?;
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
    let resp = req_post(path)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
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
    let body = SelectRequest {
        worktree: worktree.to_string(),
        mode,
    };
    let resp = req_post("/api/select")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}

/// Re-scan the configured repo root (`POST /api/rescan`, ADR 0009). `Ok` carries
/// the server's one-line summary for the picker to show.
pub async fn rescan_request() -> Result<String, String> {
    let resp = req_post("/api/rescan")
        .send()
        .await
        .map_err(network_error)?;
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
    let body = DeleteCloneRequest {
        worktree: worktree.to_string(),
    };
    let resp = req_post("/api/delete-clone")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(network_error)?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}
