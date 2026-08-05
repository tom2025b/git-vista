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
use git_vista_core::net::{network_error_text, offline_refusal_text};
use git_vista_core::status::RepoStatus;
use git_vista_protocol::dto::TagDetail;
use git_vista_protocol::operation::{IdempotencyKey, OperationId};
use git_vista_protocol::{
    BranchRequest, CloneRequest, CreateBranchRequest, CreateCommitRequest, DeleteCloneRequest,
    PatchPlan, PatchPreview, ProtocolInfo, RebaseStatus, RepoMode, RepositoryDescriptor,
    SelectRequest, SessionInfo, SessionRequest, StageDirection, StagingDiff, WorktreePathsRequest,
    WorktreeStatus, CSRF_HEADER, IDEMPOTENCY_HEADER, OPERATION_HEADER, PROTOCOL_HEADER,
    PROTOCOL_VERSION,
};

use crate::features::dialogs::commit::{amend_body, classify_amend_response, AmendOutcome};
use crate::features::dialogs::core::{
    clone_poll_step, clone_response_should_poll, ClonePollOutcome, ClonePollStep,
};
use crate::features::graph::core::{Frame, Page};
use crate::features::session::signals as session_state;
use crate::features::shell::signals as shell_state;

/// The largest page the server will mint, mirrored here so a caller's request is
/// clamped before it goes out rather than silently rewritten server-side. Kept
/// in step with `MAX_PAGE_LIMIT` in `git-vista-server`'s read handlers.
const MAX_PAGE_LIMIT: usize = 1_000;

/// How long any single HTTP attempt may hang before it is abandoned.
///
/// # Why a timeout has to exist at all (#216, #218)
///
/// `fetch()` has **no default timeout**, and a socket forwarded over SSH can die
/// without an RST or FIN — the tunnel simply stops relaying. The browser is then
/// waiting on a connection that will never answer, and the promise never settles.
/// A future parked on that `.await` is never polled again, so *no* error branch
/// runs, however carefully it was written. That is the exact shape of #216: the
/// clone dialog clears its `Cloning…` flag on both the `Ok` and `Err` arms, and
/// the button still stuck forever, because neither arm was ever reached.
///
/// Measured on a real iPad session on 2026-07-31: the SSH tunnel dropped
/// repeatedly, and a clone hung indefinitely with no error while the server-side
/// clone path was proven working by `sandbox::clone_live`.
///
/// 60s is chosen to sit well above a slow-but-real request (a large first page
/// over a phone tether) and well below "the user has given up and reloaded".
const REQUEST_TIMEOUT_MS: u64 = 60_000;

/// The deadline for `POST /api/clone` alone.
///
/// Clone is the one write whose legitimate duration is unbounded by anything the
/// client controls: a large repository over a slow link can genuinely take
/// minutes. Applying [`REQUEST_TIMEOUT_MS`] to it would abandon a *working*
/// transfer and then retry it — and because clone is not operation-tracked, that
/// retry would start a second `git clone` rather than being deduplicated. So the
/// bound is set just under the server's own 600s ceiling
/// (`handlers/clone.rs::CLONE_TIMEOUT`): late enough never to interrupt a clone
/// the server is still making progress on, early enough that the client still
/// gives up before the server does and can report why.
const CLONE_TIMEOUT_MS: u64 = 570_000;

/// Resolve to `Some(v)` if `fut` finishes within `ms`, or `None` if the deadline
/// wins.
///
/// Built from `leptos::set_timeout` and a oneshot channel rather than pulling in
/// `gloo-timers`: `futures` is already in the dependency graph, `leptos` is
/// already a direct dependency, and the whole timer is six lines. The loser of
/// the race is dropped — for the request side that drops the `fetch` future,
/// which is the only cancellation WASM offers without an `AbortController`.
///
/// Note what this does **not** do: it does not abort the in-flight HTTP request
/// at the browser level, so the server may still complete the work. That is
/// exactly why the retry above it is only safe on reads and on idempotency-keyed
/// writes — see [`send_write_with_key`].
async fn with_deadline<T>(fut: impl std::future::Future<Output = T>, ms: u64) -> Option<T> {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    leptos::set_timeout(
        move || {
            // Err means the receiver was already dropped — the request won the
            // race and this timer is a no-op. Not a failure.
            let _ = tx.send(());
        },
        std::time::Duration::from_millis(ms),
    );
    let deadline = async move {
        let _ = rx.await;
    };
    match futures::future::select(Box::pin(fut), Box::pin(deadline)).await {
        futures::future::Either::Left((value, _)) => Some(value),
        futures::future::Either::Right(((), _)) => None,
    }
}

/// The message a caller sees when a request was abandoned rather than answered.
///
/// Deliberately names the tunnel: on this deployment a hung request is nearly
/// always a dropped SSH forward, and "reconnect the tunnel" is the action that
/// actually fixes it. A generic "network error" sends the user looking at the
/// wrong thing.
fn timeout_error() -> String {
    "The server did not answer within 60 seconds. The SSH tunnel has most likely \
     dropped — restart the port forward and try again."
        .to_string()
}

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

/// The M2.22a client-side offline guard: every write function calls this
/// first, before `refuse_if_lan_view`/`refuse_if_visualize`, so a write
/// attempted while the browser reports no network fails immediately with an
/// honest message instead of going out to hang on `REQUEST_TIMEOUT_MS`
/// (or `CLONE_TIMEOUT_MS`) and die on a socket `navigator.onLine` already
/// knows is down.
///
/// This is prevention layered *on top of* `send_write_with_key`'s existing
/// single in-flight retry (#216/#218) — it does not change that retry, nor
/// `with_deadline`, nor either per-endpoint timeout constant. A write that
/// starts while online and then loses connectivity mid-flight is untouched by
/// this guard and still relies on the timeout/retry machinery already in
/// place; this only stops a write from being *attempted* when the browser
/// already knows, at the moment of the call, that it has no network.
///
/// The message deliberately does not claim the server or tunnel is
/// unreachable — see [`offline_refusal_text`]'s doc comment in
/// `git-vista-core` for why, and its host test for the exact wording pinned.
fn refuse_if_offline() -> Result<(), String> {
    if shell_state::is_online() {
        Ok(())
    } else {
        Err(offline_refusal_text())
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
pub fn new_idempotency_key() -> IdempotencyKey {
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
    // Hex digits and dashes only, so the validated newtype always accepts it.
    IdempotencyKey::new(format!("gv-{ms:x}-{n:x}-{noise:x}"))
        .expect("a hex-and-dash key is a valid idempotency key")
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
async fn send_write(
    url: &str,
    body: Option<String>,
) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    send_write_with_key(url, body, new_idempotency_key(), REQUEST_TIMEOUT_MS).await
}

/// [`send_write`] under a key the *caller* minted.
///
/// The dispatched operations need this: ADR 0020 mints a key per **user action**, and only
/// the caller knows where an action begins. Minting here would also mean the client could
/// not register the operation until the response came back, leaving the whole flight
/// unrepresented — the exact gap M1.11 closes.
async fn send_write_with_key(
    url: &str,
    body: Option<String>,
    key: IdempotencyKey,
    timeout_ms: u64,
) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    let attempt = || async {
        let builder = req_post(url).header(IDEMPOTENCY_HEADER, key.as_str());
        let sent = async {
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
        // #216: a hung request is not a slow request. Without this, a socket that
        // died silently mid-flight parks the caller's future forever and even the
        // retry below never runs — there is nothing to retry *after*, because the
        // first attempt never finished.
        //
        // # Why retrying a *timed-out* write is safe — and the one case where it is not
        //
        // A timeout here does **not** cancel the request: there is no
        // `AbortController`, so the first attempt may still be executing on the
        // server when the second arrives. For **operation-tracked** writes that is
        // handled server-side, and by design: `operations::admit` is "a single
        // critical section over the maps, so two concurrent requests carrying the
        // same key cannot both be admitted: the loser sees the winner's record and
        // awaits it". Concurrency is the case it was built for, not an afterthought.
        //
        // **`/api/clone` is not operation-tracked** (nor are `select`, `rescan` and
        // `delete-clone` — see [`WriteReceipt::operation`]). It never reaches
        // `admit`, so the key it carries buys it nothing, and two overlapping
        // attempts really would run two `git clone`s. That is why clone gets
        // [`CLONE_TIMEOUT_MS`] instead of this bound: long enough that a working
        // clone is never abandoned mid-transfer, so the retry does not fire while
        // the first attempt is still going. Found by an outside review of this
        // patch on 2026-07-31; the comment previously claimed the key made this
        // safe for every write, which was false for exactly the endpoint the
        // timeout was written for.
        with_deadline(sent, timeout_ms)
            .await
            .unwrap_or_else(|| Err(timeout_error()))
    };
    match attempt().await {
        Ok(resp) => Ok((resp, key)),
        Err(_) => attempt().await.map(|resp| (resp, key)),
    }
}

/// One read attempt, bounded and retried once — the read-side counterpart of
/// [`send_write_with_key`]'s retry (#218).
///
/// Reads had **neither** a timeout nor a retry, while writes had a retry. That
/// asymmetry is what made a single dropped request during history loading
/// unrecoverable without user action: the seed resource resolved to an error (or
/// never resolved at all) and nothing tried again, so the view sat on whatever
/// it had managed to draw. A read is naturally idempotent — no key needed, and
/// no risk of duplicating work — so it gets the same one-shot retry on the same
/// reasoning as the write path: the first attempt evicts a dead pooled socket,
/// the second goes out on a fresh connection.
async fn send_read(url: &str) -> Result<gloo_net::http::Response, HistoryFetchError> {
    let attempt = || async {
        with_deadline(req_get(url).send(), REQUEST_TIMEOUT_MS)
            .await
            .unwrap_or_else(|| Err(gloo_net::Error::GlooError(timeout_error())))
    };
    let first = attempt().await;
    let resp = match first {
        Ok(resp) => resp,
        Err(_) => attempt()
            .await
            .map_err(|e| HistoryFetchError::Network(network_error(e)))?,
    };
    Ok(resp)
}

/// What a write answered with, beyond its body.
///
/// The extra fact is what lets an operation *exist* on the client (M1.11, #64): for the
/// ten endpoints that reach the server's planner, the operation id to subscribe to. The
/// key is not echoed back — the caller minted it and already has it. `operation` is `None` for the four writes that are
/// not operation-tracked (`select`, `rescan`, `clone`, `delete-clone`), which is a normal
/// answer, not a failure: those settle from this response alone.
pub struct WriteReceipt {
    pub operation: Option<OperationId>,
    /// Whether the HTTP status was 2xx.
    pub ok: bool,
    /// The response body — git's own message on success, the server's reason on failure.
    pub message: String,
}

/// Read the headers *before* consuming the body: `text()` takes the response by value.
async fn receipt(resp: gloo_net::http::Response) -> WriteReceipt {
    let operation = resp
        .headers()
        .get(OPERATION_HEADER)
        .and_then(|v| OperationId::new(v).ok());
    let ok = resp.ok();
    let status = resp.status();
    let message = resp
        .text()
        .await
        .unwrap_or_else(|_| format!("HTTP {status}"));
    WriteReceipt {
        operation,
        ok,
        message,
    }
}

/// [`send_write`] with a JSON body.
async fn write_json<T: serde::Serialize>(
    url: &str,
    body: &T,
) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    write_json_with_timeout(url, body, REQUEST_TIMEOUT_MS).await
}

/// [`write_json`] under a caller-chosen deadline.
///
/// Exists for `/api/clone`, which is the one write whose *legitimate* duration
/// can exceed the ordinary bound — see [`CLONE_TIMEOUT_MS`].
async fn write_json_with_timeout<T: serde::Serialize>(
    url: &str,
    body: &T,
    timeout_ms: u64,
) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    let json = serde_json::to_string(body).map_err(|e| e.to_string())?;
    send_write_with_key(url, Some(json), new_idempotency_key(), timeout_ms).await
}

/// [`write_json_with_timeout`] under a key the **caller** mints and keeps,
/// win or lose (#278).
///
/// # Why `clone_request` needs this and no other write does
///
/// `write_json`/`write_json_with_timeout` mint their key *inside*
/// [`send_write_with_key`] and hand it back only on `Ok` — every other write
/// function is fine with that, because on total failure there is nothing to
/// retain a key *for*: those endpoints aren't independently pollable, so a
/// lost key is a lost key either way. Clone is different since #263/#277:
/// `GET /api/clone-status/{key}` can answer *after* the `POST` response is
/// gone, but only if something on the client still holds the key that was
/// sent. `send_write_with_key`'s `Err(_)` arm drops it — it was captured
/// inside the retried `attempt` closure and never escapes a failed call.
/// Minting here, before the call, means the caller holds the key
/// unconditionally, independent of whether the call below ever returns
/// anything usable.
///
/// Deliberately a sibling function rather than a change to
/// `send_write_with_key`'s return type: that would mean touching every other
/// write function's `?`-propagation of a plain `Result<_, String>`, for a
/// property only this one caller needs.
async fn write_json_with_key<T: serde::Serialize>(
    url: &str,
    body: &T,
    key: IdempotencyKey,
    timeout_ms: u64,
) -> Result<gloo_net::http::Response, String> {
    let json = serde_json::to_string(body).map_err(|e| e.to_string())?;
    send_write_with_key(url, Some(json), key, timeout_ms)
        .await
        .map(|(resp, _key)| resp)
}

/// [`send_write`] with no body — the bodyless writes (stage, unstage, rebase…).
async fn write_empty(url: &str) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    send_write(url, None).await
}

/// Exchange a one-time bootstrap token for a session (`POST /api/session`, M1.04).
/// On success the server sets the HttpOnly session cookie and returns the CSRF
/// token; a `401` means the token was wrong or expired. The token travels in the
/// JSON body, never the URL, so it can't land in a server log.
///
/// Bounded and retried once on a network-level failure (#218) — the same
/// timeout+retry [`send_read`] gives every history read. Before this, session
/// establishment had **neither**: a single dropped or silently-dead connection
/// (the SSH-tunnel-drop shape #216/#218 exist for) parked this future forever,
/// with no error for [`establish_session`] to recover from, and no automatic
/// bump of the history reload once a session did eventually land — the graph
/// panel would sit on `SeedLoading`/`SeedError` until the user reloaded by hand.
/// The retry is safe even though the token is single-use: if the first attempt
/// actually landed server-side, the second gets an "invalid token" answer, and
/// [`establish_session`] already falls through to [`get_session`] on any
/// `post_session` failure to pick up the cookie the first attempt set.
pub async fn post_session(token: &str) -> Result<SessionInfo, String> {
    let body = SessionRequest {
        token: token.to_string(),
    };
    let attempt = || async {
        let sent = async {
            req_post("/api/session")
                .json(&body)
                .map_err(|e| e.to_string())?
                .send()
                .await
                .map_err(network_error)
        };
        with_deadline(sent, REQUEST_TIMEOUT_MS)
            .await
            .unwrap_or_else(|| Err(timeout_error()))
    };
    let resp = match attempt().await {
        Ok(resp) => resp,
        Err(_) => attempt().await?,
    };
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
///
/// Routed through [`send_read`] (#218), for the same reason [`post_session`]
/// now has its own timeout+retry: on an already-bootstrapped browser (the
/// `#s=` fragment stripped after first use, so every later load skips straight
/// to this call) this bare GET *was* the entirety of session establishment,
/// with no timeout and no retry — a single dropped request here, not just in
/// the history reads, could leave `establish_session()` hanging or erroring
/// with nothing to self-heal it.
pub async fn get_session() -> Result<SessionInfo, String> {
    let url = format!("/api/session?t={}", js_sys::Date::now());
    let resp = send_read(&url).await.map_err(|e| e.to_string())?;
    resp.json::<SessionInfo>().await.map_err(|e| e.to_string())
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
    // Flattened over `response_error_detail` so there is exactly one error
    // parser (#316): every one of this helper's call sites now shows the
    // envelope's `error.message` alone — the request id stopped riding along
    // in the user-facing string and goes to the console instead.
    match response_error_detail(resp).await {
        Ok(m) | Err(m) => m,
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
    let resp = send_read(&url).await?;
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
    let resp = send_read(&url).await?;
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

/// The wire shape of `GET /api/clone-status/{key}` (#263/#277), mirrored here
/// rather than shared through `git-vista-protocol`: `handlers/clone.rs`'s
/// `CloneStatusResponse` is a server-internal type, not one the protocol
/// crate exports (unlike `CloneRequest`/`RepositoryDescriptor`). Tag and
/// field names checked directly against that enum's own
/// `#[serde(tag = "state", rename_all = "snake_case")]`, not guessed.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CloneStatusBody {
    Running,
    Succeeded { descriptor: RepositoryDescriptor },
    Failed { status: u16, message: String },
}

/// How long to wait between `GET /api/clone-status/{key}` polls once a
/// `POST /api/clone` response is lost, ambiguous, or reports the attempt
/// still running under our own key (#278).
///
/// 5s: frequent enough that a small clone's outcome is noticed quickly, far
/// below anything that could be called hammering the server — this read is
/// an in-memory map lookup (`clone_records()`), no git spawned to answer it.
const CLONE_POLL_INTERVAL_MS: u64 = 5_000;

/// The per-attempt deadline for one status poll — independent of, and much
/// shorter than, [`REQUEST_TIMEOUT_MS`]: a poll that's still waiting past
/// this is far more likely a dead tunnel than a slow server (the handler
/// does no I/O beyond a mutex lock), and a shorter bound means a bad poll
/// gives up and tries again sooner rather than eating most of the interval
/// doing nothing.
const CLONE_STATUS_POLL_TIMEOUT_MS: u64 = 15_000;

/// How many polls to make before giving up on ever hearing a definitive
/// answer.
///
/// 120 × [`CLONE_POLL_INTERVAL_MS`] (5s) = 600s — `handlers/clone.rs`'s own
/// `CLONE_TIMEOUT`, the server's ceiling on a single clone attempt. By the
/// time that many *intervals* have elapsed, the server is guaranteed to have
/// settled the attempt one way or another (`CloneGuard`'s `Drop` records a
/// terminal failure even if the handler panics), so there is nothing left to
/// wait for past that point.
///
/// Attempts, not wall-clock, is what's bounded here — a poll that itself
/// times out or errors (`CLONE_STATUS_POLL_TIMEOUT_MS`) only adds slack on
/// top of the 600s floor, which biases toward giving a flaky tunnel more
/// time to recover rather than less. This client cannot tell how much of
/// the server's 600s window had already elapsed before the original `POST`
/// response was lost — a tunnel that dies at second 1 looks identical from
/// here to one that dies at second 590 — so the honest bound is the
/// server's whole window, not a guessed fraction of it.
const CLONE_POLL_MAX_ATTEMPTS: u32 = 120;

/// Resolve after `ms` milliseconds — the polling loop's spacing, built the
/// same way as [`with_deadline`]'s timer (`leptos::set_timeout` + a oneshot):
/// the same six lines, the same reasoning against pulling in `gloo-timers`
/// for it.
async fn sleep_ms(ms: u64) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    leptos::set_timeout(
        move || {
            let _ = tx.send(());
        },
        std::time::Duration::from_millis(ms),
    );
    let _ = rx.await;
}

/// One `GET /api/clone-status/{key}` attempt, mapped to the pure
/// [`ClonePollOutcome`] [`clone_poll_step`] decides on. Bounded by
/// [`CLONE_STATUS_POLL_TIMEOUT_MS`], independent of the interval between
/// attempts — a hung poll must not itself eat the whole budget.
async fn fetch_clone_status(key: &IdempotencyKey) -> ClonePollOutcome<RepositoryDescriptor> {
    let url = format!("/api/clone-status/{}", encode_component(key.as_str()));
    let resp = match with_deadline(req_get(&url).send(), CLONE_STATUS_POLL_TIMEOUT_MS).await {
        None => return ClonePollOutcome::PollError(timeout_error()),
        Some(Err(e)) => return ClonePollOutcome::PollError(network_error(e)),
        Some(Ok(resp)) => resp,
    };
    // `clone_status_not_found` (`handlers/clone.rs`): the key was never
    // admitted, was evicted past its TTL, or this `GET`'s own `POST` never
    // reached the server at all.
    if resp.status() == 404 {
        return ClonePollOutcome::Unknown;
    }
    if !resp.ok() {
        return ClonePollOutcome::PollError(format!("HTTP {}", resp.status()));
    }
    match with_deadline(resp.json::<CloneStatusBody>(), CLONE_STATUS_POLL_TIMEOUT_MS).await {
        None => ClonePollOutcome::PollError(timeout_error()),
        Some(Err(e)) => ClonePollOutcome::PollError(e.to_string()),
        Some(Ok(CloneStatusBody::Running)) => ClonePollOutcome::Running,
        Some(Ok(CloneStatusBody::Succeeded { descriptor })) => {
            ClonePollOutcome::Succeeded(descriptor)
        }
        Some(Ok(CloneStatusBody::Failed { status, message })) => {
            ClonePollOutcome::Failed(format!("{message} (server status {status})"))
        }
    }
}

/// Drive #278's poll loop against `GET /api/clone-status/{key}` after a
/// `POST /api/clone` response was lost, ambiguous, or reported the attempt
/// still running under our own key. `lost_reason` is the original problem —
/// folded into the final message only if the whole budget is spent without a
/// definitive answer (see [`clone_poll_step`]'s doc comment for why every
/// other outcome keeps polling instead of giving up early).
///
/// `on_checking_status` fires once, synchronously, the moment polling
/// actually starts — the dialog's hook for a "still cloning — checking…"
/// state distinct from its ordinary pending/error states, since a poll can
/// legitimately run for most of `CLONE_POLL_MAX_ATTEMPTS`'s ~10-minute
/// budget on a large repo.
async fn poll_clone_status_until_settled(
    key: IdempotencyKey,
    lost_reason: String,
    on_checking_status: impl FnOnce(),
) -> Result<RepositoryDescriptor, String> {
    on_checking_status();
    for attempt in 1..=CLONE_POLL_MAX_ATTEMPTS {
        sleep_ms(CLONE_POLL_INTERVAL_MS).await;
        let outcome = fetch_clone_status(&key).await;
        match clone_poll_step(outcome, attempt, CLONE_POLL_MAX_ATTEMPTS, &lost_reason) {
            ClonePollStep::Resolved(result) => return result,
            ClonePollStep::KeepPolling => {}
        }
    }
    // Unreachable in practice: `clone_poll_step` always resolves once
    // `attempts_made == max_attempts`, which the last loop iteration above
    // supplies. Kept as an honest fallback rather than `unreachable!()` — an
    // off-by-one here must degrade to an error a user can read, not a panic.
    // Detail only — `clone_settlement`'s `Err` arm supplies the heading (see
    // `clone_poll_exhausted_message`).
    Err(format!(
        "{lost_reason}\n\nGave up polling for the outcome without a definitive answer. \
         Check the repository picker before retrying."
    ))
}

/// Ask the backend to clone a public URL into the persistent clones store
/// (`POST /api/clone`, ADR 0008). `Ok` carries the fresh clone's descriptor so
/// the caller can jump straight to the Visualize/Active mode screen for it. On
/// a non-2xx response the body is the server's / git's own error text (bad
/// URL, repo not found, …), returned as `Err`.
///
/// # #278: retaining the idempotency key and polling after a lost response
///
/// The key is minted **here**, by [`write_json_with_key`], not inside the
/// helper — see that function's doc comment for why clone specifically needs
/// this while every other write is content to let `write_json`/`write_empty`
/// mint-and-maybe-return theirs. Holding the key means a lost, timed-out, or
/// "already in progress" response no longer has to be reported as a bare
/// failure: [`poll_clone_status_until_settled`] polls `GET
/// /api/clone-status/{key}` with the same key both `POST` attempts sent,
/// recovering the real outcome from #277's server-side tracking instead of
/// leaving the caller to guess (#260's original symptom).
///
/// `on_checking_status` is invoked once if and when that poll loop actually
/// starts, so the dialog can show a distinct "checking…" state rather than
/// leaving "Cloning…" up for a phase that is no longer the original request.
pub async fn clone_request(
    url: &str,
    on_checking_status: impl FnOnce(),
) -> Result<RepositoryDescriptor, String> {
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let body = CloneRequest {
        url: url.to_string(),
    };
    let key = new_idempotency_key();
    let resp = match write_json_with_key("/api/clone", &body, key.clone(), CLONE_TIMEOUT_MS).await {
        Ok(resp) => resp,
        // Both of `send_write_with_key`'s internal attempts failed at the
        // network level — the total-loss case #278 exists for. No response
        // was ever read, so nothing here can say whether the clone
        // succeeded; the retained `key` is the only way to still find out.
        Err(network_err) => {
            return poll_clone_status_until_settled(key, network_err, on_checking_status).await;
        }
    };
    // Body reads bounded too (#260): `write_json_with_key` deadlines only
    // the send, so a connection that stalls after headers would otherwise park
    // this future forever — and the clone dialog now pins itself open until
    // this settles, which turns "future never resolves" into "modal never
    // closes". The bodies here are tiny; `REQUEST_TIMEOUT_MS` is generous.
    if resp.ok() {
        match with_deadline(resp.json::<RepositoryDescriptor>(), REQUEST_TIMEOUT_MS).await {
            Some(Ok(descriptor)) => Ok(descriptor),
            // Not every `Err` here means the same thing, and the difference
            // decides whether this is pollable (review finding).
            // `gloo_net`'s `json()` is `from_str(&self.text().await?)`, so a
            // `JsError` means the *body read itself* failed at the transport
            // level — the connection died after headers arrived, which is
            // exactly the dropped-tunnel / suspended-tab case this whole
            // feature exists to survive, and is every bit as ambiguous as a
            // fully lost response. Only a `SerdeError` means the bytes
            // genuinely arrived and did not parse, which is a real bug and
            // correctly terminal. Treating both as terminal sent the feature's
            // own headline scenario down the non-polling path.
            Some(Err(e)) if is_transport_error(&e) => {
                poll_clone_status_until_settled(key, e.to_string(), on_checking_status).await
            }
            Some(Err(e)) => Err(e.to_string()),
            // Headers arrived but the body stalled — ambiguous the same way
            // a fully lost response is: poll rather than guess.
            None => poll_clone_status_until_settled(key, timeout_error(), on_checking_status).await,
        }
    } else {
        let status = resp.status();
        // `response_error` (not raw body text): an empty error body — the
        // LAN listener's bare 405, say — must still say *something*.
        match with_deadline(response_error_detail(resp), REQUEST_TIMEOUT_MS).await {
            // A body that was genuinely read: the message text is
            // trustworthy, so the narrow "already in progress" match is a
            // safe way to tell a pollable 409 from the unrelated
            // key-reused-for-a-different-URL 409.
            Some(Ok(message)) => {
                if clone_response_should_poll(status, &message) {
                    poll_clone_status_until_settled(key, message, on_checking_status).await
                } else {
                    Err(message)
                }
            }
            // The body read FAILED at the transport level (review finding).
            // `response_error` used to swallow this into an empty string and
            // fall back to "HTTP 409", which no longer contains "already in
            // progress" — so a genuinely pollable 409 whose body was lost
            // mid-read was misreported as terminal. A 409 whose body we could
            // not read is the same ambiguous class as any other lost
            // response: poll it. Any other status stays terminal, since only
            // 409 is ever ambiguous here.
            Some(Err(message)) => {
                if status == 409 {
                    poll_clone_status_until_settled(key, message, on_checking_status).await
                } else {
                    Err(message)
                }
            }
            None => poll_clone_status_until_settled(key, timeout_error(), on_checking_status).await,
        }
    }
}

/// True when a `gloo_net` error means the request/response never completed at
/// the transport level, as opposed to arriving and failing to parse.
///
/// The distinction is load-bearing for `clone_request` (see its body): a
/// transport failure leaves the clone's real outcome unknown and must be
/// resolved by polling `clone-status`; a parse failure means the server's
/// answer was received and simply not understood, which polling cannot fix.
fn is_transport_error(e: &gloo_net::Error) -> bool {
    matches!(e, gloo_net::Error::JsError(_))
}

/// [`response_error`], but preserving whether the body was actually readable.
///
/// `Ok(msg)` — the body was read (possibly empty, hence the status fallback);
/// the text is trustworthy enough to match on. `Err(msg)` — reading the body
/// itself failed, so the returned text is a status-only placeholder and must
/// not be pattern-matched for meaning (review finding: doing so silently
/// dropped a pollable 409 into the terminal path).
async fn response_error_detail(resp: gloo_net::http::Response) -> Result<String, String> {
    let status = resp.status();
    match resp.text().await {
        Ok(body) if !body.trim().is_empty() => {
            // #316: one parser for every error body. The envelope's
            // `error.message` is the handler's own text — the clone
            // in-progress sentinel included, so `clone_response_should_poll`
            // matches on the unwrapped message exactly as it did on the raw
            // body — and the request id goes to the console, never onward.
            let parsed = crate::features::dialogs::core::split_error_response(status, &body);
            if let Some(id) = &parsed.request_id {
                web_sys::console::error_1(
                    &format!("git-vista: request {id} failed: {}", parsed.message).into(),
                );
            }
            Ok(parsed.message)
        }
        Ok(_) => Ok(format!("HTTP {status}")),
        Err(_) => Err(format!("HTTP {status}")),
    }
}

/// Unwrap a non-2xx write response for the modal error path (#316): the
/// user gets `error.message` alone, and the request id — the server-side
/// correlation handle — goes to the console, never into the modal. The
/// split itself is pure and host-tested (`split_error_response`).
async fn user_facing_error(route: &str, resp: gloo_net::http::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let parsed = crate::features::dialogs::core::split_error_response(status, &body);
    if let Some(id) = &parsed.request_id {
        web_sys::console::error_1(
            &format!(
                "git-vista: POST {route} failed (request {id}): {}",
                parsed.message
            )
            .into(),
        );
    }
    parsed.message
}

/// Ask the backend to create `name` at `commit` (Issue #18, `POST /api/branch`).
/// On a non-2xx response the envelope's `error.message` is returned as `Err`
/// (#316) so the caller can show the real reason (branch exists, bad name, …)
/// without the wire JSON around it.
///
/// The network-failure retry that used to live here is now [`send_write`]'s,
/// for every write rather than only this one: since M1.08 both attempts carry
/// the same idempotency key, so a duplicate is replayed rather than re-run.
pub async fn create_branch_request(name: &str, commit: &str) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = CreateBranchRequest {
        name: name.to_string(),
        commit: commit.to_string(),
    };
    let (resp, _key) = write_json("/api/branch", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/branch", resp).await)
    }
}

/// Ask the backend to create a commit (Issue #33, `POST /api/commit`).
/// `allow_empty` picks `git commit --allow-empty` (empty commit) vs a plain
/// `git commit` (staged changes). `branch` targets a branch other than the
/// checked-out one — the branch-stub path, empty commits only; `None` commits
/// on HEAD as before. As with the branch request, a non-2xx body is
/// unwrapped to the envelope's `error.message` (#316), returned as `Err`.
pub async fn create_commit_request(
    message: &str,
    allow_empty: bool,
    branch: Option<&str>,
) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = CreateCommitRequest {
        message: message.to_string(),
        allow_empty,
        branch: branch.map(str::to_string),
    };
    let (resp, _key) = write_json("/api/commit", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/commit", resp).await)
    }
}

/// Rewrite the checked-out branch's tip commit (`POST /api/amend-commit`,
/// M2.19c #224 over M2.19a #222 / M2.19b #223).
///
/// Three things make this a different function from [`create_commit_request`]
/// rather than a flag on it, and all three are the endpoint's own design (ADR
/// 0040):
///
/// - **A separate route.** An amend sent to a server that predates #223 must
///   404, never be quietly accepted as a plain commit — "created a second
///   commit instead of rewriting the first" is a silent wrong outcome.
/// - **A compare-and-swap.** `expected_tip` is the full commit id the *user*
///   reviewed. The server refuses if HEAD has moved since, which is the whole
///   protection: it is not a staleness optimisation, it is what stops an amend
///   rewriting a commit nobody looked at.
/// - **A typed answer.** Every 400 from this route is an `AmendCommitError`,
///   and 200 is an `AmendCommitSuccess`. Reading them is
///   `features::dialogs::commit::classify_amend_response` — pure, host-tested
///   against bodies serialized from the server's own DTOs — so this function
///   carries no parsing or classification of its own.
///
/// Never returns `Result`: the caller must handle a stale tip differently from
/// an error (see [`AmendOutcome`]), and a `Result<_, String>` is exactly the
/// shape that would let it treat them the same.
pub async fn amend_commit_request(message: &str, expected_tip: &str) -> AmendOutcome {
    if let Err(refusal) = refuse_if_offline().and_then(|()| refuse_if_visualize()) {
        return AmendOutcome::Unavailable(refusal);
    }
    let body = amend_body(message, expected_tip);
    let resp = match write_json("/api/amend-commit", &body).await {
        Ok((resp, _key)) => resp,
        // A transport failure: the request may or may not have reached the
        // server, which is precisely what `Unavailable`'s copy says.
        Err(e) => return AmendOutcome::Unavailable(e),
    };
    let status = resp.status();
    let text = resp
        .text()
        .await
        .unwrap_or_else(|_| format!("HTTP {status}"));
    classify_amend_response(status, &text)
}

/// Ask the backend to stage all working-tree changes (`POST /api/stage`) — a
/// plain `git add -A`, so modified/new/deleted files move into the index and can
/// then be committed. Bodyless, like the rebase request; a non-2xx body is git's
/// own error text, returned as `Err` for the caller to show.
pub async fn stage_request() -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/stage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/stage", resp).await)
    }
}

/// Ask the backend to unstage everything (`POST /api/unstage`) — a plain
/// `git reset HEAD`, the exact inverse of [`stage_request`]: the index goes
/// back to HEAD, the working tree keeps every edit. Same bodyless shape and
/// error posture as staging.
pub async fn unstage_request() -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/unstage").await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(user_facing_error("/api/unstage", resp).await)
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

/// Fetch every tag with its full metadata (`GET /api/tags`, M2.21b #236):
/// lightweight vs annotated, the tagged commit, and — for annotated tags —
/// the tag object, tagger and message.
///
/// A live read like the feed beside it: a tag can appear or vanish from a
/// terminal at any moment, so it is fetched fresh whenever the Activity panel
/// opens and cache-busted the same way.
pub async fn fetch_tags() -> Result<Vec<TagDetail>, String> {
    let url = format!("/api/tags?t={}", js_sys::Date::now());
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<Vec<TagDetail>>()
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
pub async fn undo_request(
    action: &UndoAction,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(action).map_err(|e| e.to_string())?;
    let (resp, _key) =
        send_write_with_key("/api/undo", Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
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

/// Fetch the generation-tagged working-tree status (`GET /api/status/v2`,
/// #68c) — the per-path [`WorktreeStatus`] the discard/delete menu items need
/// to name exactly which files each operation would touch (M2.18b, #220).
///
/// Additive alongside [`fetch_status`], which serves the topbar chip's
/// coarser v1 shape and is untouched — migrating that consumer is 68d's job,
/// not this one's.
///
/// Routed through [`send_read`] (#218) rather than a bare `req_get`, for the
/// reason that function documents: a read with no timeout over a dropped SSH
/// tunnel never settles, and this one gates a destructive confirmation.
pub async fn fetch_worktree_status() -> Result<WorktreeStatus, String> {
    let url = format!("/api/status/v2?t={}", js_sys::Date::now());
    let resp = send_read(&url).await.map_err(|e| e.to_string())?;
    if resp.ok() {
        resp.json::<WorktreeStatus>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend to discard uncommitted changes to `paths`
/// (`POST /api/discard-tracked-paths`, M2.18a/#219, wired by M2.18b/#220).
///
/// Every path must be tracked-and-dirty *at execution time*: the server
/// re-derives that from a fresh `git status` immediately before running git
/// and refuses the whole batch — never partially applies — if any path has
/// since drifted. That 409 is a normal answer here, not a bug, and its text
/// names the path.
///
/// The body is [`WorktreePathsRequest`], the server's own DTO, so the
/// `#[serde(deny_unknown_fields)]` on it cannot be violated by a stray field
/// invented on this side.
pub async fn discard_tracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { paths }).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(
        "/api/discard-tracked-paths",
        Some(json),
        key,
        REQUEST_TIMEOUT_MS,
    )
    .await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to delete untracked `paths` outright
/// (`POST /api/delete-untracked-paths`).
///
/// A **separate function** from [`discard_tracked_paths_request`], mirroring
/// the two separate `GitOperation` variants behind them — never one call
/// parameterised by a bool (#71). The two requests are the same shape and
/// different operations, and the one with no way back does not share a code
/// path with the one that has a qualified recovery story.
///
/// Retries are safe for the same reason every other write's are: the
/// idempotency key is minted by the caller and replayed rather than re-run.
pub async fn delete_untracked_paths_request(
    paths: Vec<String>,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let json = serde_json::to_string(&WorktreePathsRequest { paths }).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(
        "/api/delete-untracked-paths",
        Some(json),
        key,
        REQUEST_TIMEOUT_MS,
    )
    .await?;
    Ok(receipt(resp).await)
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
pub async fn rebase_request(key: IdempotencyKey) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = send_write_with_key("/api/rebase", None, key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
}

/// Ask the backend to reset a seeded *test repo* to its recorded state
/// (`POST /api/reset-test-repo`). Only offered when the graph said
/// `resettable` (the repo was opted in with `gv --seed`). `Ok` carries the
/// server's summary line ("… 2 branches restored, 1 deleted, HEAD → ‘main’");
/// a non-2xx body is the server's reason (not a test repo, corrupt seed, or
/// the exact git step that refused), returned as `Err` for the dialog to show.
///
/// **M2.22a decision (#241):** this function was write-shaped but not in that
/// issue's enumerated list of 11 write functions — flagged there as an open
/// question rather than silently included or excluded. Decided **in**: it is
/// a real `POST` that mutates the repo (restores/deletes branches, moves
/// HEAD) over the exact same socket as every other write here, so it is
/// exposed to the exact same failure this guard exists to prevent — a write
/// going out and hanging/dying on a dropped SSH tunnel while the browser
/// already knew it had no network. "Test-repo-only" describes when the UI
/// *offers* this action (`resettable` graphs only, gated by `gv --seed`), not
/// whether the write itself is safe to attempt while offline; those are
/// independent facts, and only the second one is this guard's business.
pub async fn reset_test_repo_request() -> Result<String, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_empty("/api/reset-test-repo").await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(user_facing_error("/api/reset-test-repo", resp).await)
    }
}

/// Ask the backend to run a branch operation on `branch` (Issue #33 follow-up).
/// `path` is the endpoint — `/api/merge`, `/api/push`, `/api/delete-branch`, or
/// `/api/force-delete-branch` — all of which take the same `{ branch }` body. As with the other requests, a
/// non-2xx body is git's own error text, returned as `Err` for the caller to show.
/// `Ok` carries the server's success line — most callers ignore it, but the merge
/// flow reads it to tell a real merge from git's "Already up to date" no-op.
pub async fn branch_op_request(
    path: &str,
    branch: &str,
    key: IdempotencyKey,
) -> Result<WriteReceipt, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let body = BranchRequest {
        branch: branch.to_string(),
    };
    let json = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    let (resp, _key) = send_write_with_key(path, Some(json), key, REQUEST_TIMEOUT_MS).await?;
    Ok(receipt(resp).await)
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
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let body = SelectRequest {
        worktree: worktree.to_string(),
        mode,
    };
    let (resp, _key) = write_json("/api/select", &body).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}

/// Re-scan the configured repo root (`POST /api/rescan`, ADR 0009). `Ok` carries
/// the server's one-line summary for the picker to show.
pub async fn rescan_request() -> Result<String, String> {
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let (resp, _key) = write_empty("/api/rescan").await?;
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
    refuse_if_offline()?;
    refuse_if_lan_view()?;
    let body = DeleteCloneRequest {
        worktree: worktree.to_string(),
    };
    let (resp, _key) = write_json("/api/delete-clone", &body).await?;
    if resp.ok() {
        Ok(resp.text().await.unwrap_or_default())
    } else {
        Err(response_error(resp).await)
    }
}

/// Fetch the staging base diff (`GET /api/staging/diff?direction=stage|unstage`,
/// M2.17d, #215) — the pinned diff a hunk/line selection is made against, and
/// the `diff-v1:` generation token the resulting [`PatchPlan`] must carry
/// back verbatim. A read, like [`fetch_status`]/[`fetch_rebase_status`], so no
/// offline/visualize guard here — those gate only the two writes below.
pub async fn staging_diff_request(direction: StageDirection) -> Result<StagingDiff, String> {
    let dir = match direction {
        StageDirection::Stage => "stage",
        StageDirection::Unstage => "unstage",
    };
    let url = format!(
        "/api/staging/diff?direction={dir}&t={}",
        js_sys::Date::now()
    );
    let resp = req_get(&url).send().await.map_err(network_error)?;
    if resp.ok() {
        resp.json::<StagingDiff>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend what applying `plan` would do, without doing it
/// (`POST /api/staging/preview`, M2.17d, #215) — the exact bytes
/// `staging_apply_request` would execute while the plan's generation still
/// holds. A non-2xx body is the server's reason (stale generation, malformed
/// selection), returned as `Err` for the caller to show.
pub async fn staging_preview_request(plan: &PatchPlan) -> Result<PatchPreview, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_json("/api/staging/preview", plan).await?;
    if resp.ok() {
        resp.json::<PatchPreview>().await.map_err(|e| e.to_string())
    } else {
        Err(response_error(resp).await)
    }
}

/// Ask the backend to execute `plan` (`POST /api/staging/apply`, M2.17d,
/// #215) — stage or unstage exactly the selected hunks/lines, through the
/// same build-and-check pipeline [`staging_preview_request`] ran. A non-2xx
/// body is the server's reason, returned as `Err` for the caller to show.
pub async fn staging_apply_request(plan: &PatchPlan) -> Result<(), String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let (resp, _key) = write_json("/api/staging/apply", plan).await?;
    if resp.ok() {
        Ok(())
    } else {
        Err(response_error(resp).await)
    }
}
