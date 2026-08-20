//! Clone lifecycle — `POST /api/clone` and its `GET /api/clone-status/{key}`
//! recovery poll (#260/#263/#277/#278).
//!
//! Split out of the former monolithic `api.rs`. Everything here is
//! clone-specific and self-contained — the poll constants, the wire shape of
//! the status endpoint, and [`write_json_with_key`] (the one write in the
//! whole client that mints its idempotency key before the call rather than
//! inside it) all exist for [`clone_request`] alone; nothing else in the api
//! module reaches into this file.

use git_vista_protocol::operation::IdempotencyKey;
use git_vista_protocol::{CloneRequest, RepositoryDescriptor};

use crate::features::dialogs::core::{
    clone_poll_step, clone_response_should_poll, ClonePollOutcome, ClonePollStep,
};

use super::{
    encode_component, network_error, new_idempotency_key, refuse_if_lan_view, refuse_if_offline,
    req_get, response_error_detail, send_write_with_key, sleep_ms, timeout_error, with_deadline,
    CLONE_STATUS_POLL_TIMEOUT_MS, REQUEST_TIMEOUT_MS,
};

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
