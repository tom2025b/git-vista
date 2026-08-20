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

use git_vista_core::net::{network_error_text, offline_refusal_text, timeout_error_text};
use git_vista_protocol::operation::{IdempotencyKey, OperationId};
use git_vista_protocol::{
    CSRF_HEADER, IDEMPOTENCY_HEADER, OPERATION_HEADER, PROTOCOL_HEADER, PROTOCOL_VERSION,
};

use crate::features::session::signals as session_state;
use crate::features::shell::signals as shell_state;

mod activity;
mod branches;
mod clone;
mod commits;
mod diff;
mod graph;
mod operations;
mod remotes;
mod repositories;
mod session;
mod staging;
mod status;
mod tags;

pub use activity::{fetch_activity, fetch_undoables, undo_request};
pub use branches::{
    branch_op_request, create_branch_request, fetch_head_branch, fetch_rebase_status,
    rebase_request,
};
pub use clone::clone_request;
pub use commits::{amend_commit_request, create_commit_request, fetch_commit_detail};
pub use diff::{fetch_diff, fetch_diff_full, fetch_file, fetch_spec_diff};
pub use graph::{fetch_frame, fetch_page};
pub use operations::{
    cancel_operation_request, fetch_operation_status, resolve_operation_id, CancelOutcome,
};
pub use remotes::{fetch_request, preview_push, pull_request, push_request};
pub use repositories::{
    delete_clone_request, fetch_catalog, rescan_request, reset_test_repo_request, select_request,
};
pub use session::{fetch_protocol, get_session, post_session};
pub use staging::{
    stage_request, staging_apply_request, staging_diff_request, staging_preview_request,
    unstage_request,
};
pub use status::{
    delete_untracked_paths_request, discard_tracked_paths_request, fetch_status,
    fetch_worktree_status,
};
pub use tags::{create_tag_request, delete_tag_request, fetch_tags};

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

/// The per-attempt deadline for one status poll — independent of, and much
/// shorter than, [`REQUEST_TIMEOUT_MS`]: a poll that's still waiting past
/// this is far more likely a dead tunnel than a slow server (the handler
/// does no I/O beyond a mutex lock), and a shorter bound means a bad poll
/// gives up and tries again sooner rather than eating most of the interval
/// doing nothing.
const CLONE_STATUS_POLL_TIMEOUT_MS: u64 = 15_000;

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

/// The message a caller sees when a request was abandoned client-side rather
/// than answered. Wording lives in `git-vista-core::net::timeout_error_text`
/// (M2.19, #72) so it is pinned by a host test even though every call site
/// here is wasm-only; see that function's doc comment for why it no longer
/// names the SSH tunnel.
fn timeout_error() -> String {
    timeout_error_text()
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

/// [`send_write`] with no body — the bodyless writes (stage, unstage, rebase…).
async fn write_empty(url: &str) -> Result<(gloo_net::http::Response, IdempotencyKey), String> {
    send_write(url, None).await
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

/// Resolve after `ms` milliseconds — the polling loop's spacing, built the
/// same way as [`with_deadline`]'s timer (`leptos::set_timeout` + a oneshot):
/// the same six lines, the same reasoning against pulling in `gloo-timers`
/// for it.
pub(crate) async fn sleep_ms(ms: u64) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    leptos::set_timeout(
        move || {
            let _ = tx.send(());
        },
        std::time::Duration::from_millis(ms),
    );
    let _ = rx.await;
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
