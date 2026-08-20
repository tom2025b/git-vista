//! Operation-id resolution, boot-time status, and cancellation —
//! `GET /api/operations/by-key/{key}`, `GET /api/operations/{id}`,
//! `POST /api/operations/{id}/cancel` (M2.20f, #232).
//!
//! Split out of the former monolithic `api.rs`.

use git_vista_protocol::operation::{IdempotencyKey, OperationId, OperationStatus};
use git_vista_protocol::OperationByKeyResponse;

use super::{
    encode_component, network_error, refuse_if_offline, refuse_if_visualize, req_get, req_post,
    sleep_ms, timeout_error, user_facing_error, with_deadline, CLONE_STATUS_POLL_TIMEOUT_MS,
    REQUEST_TIMEOUT_MS,
};

/// How long to wait between `GET /api/operations/by-key/{key}` polls while
/// resolving a just-fired write's operation id (M2.20f, #232).
///
/// **The one number in this block that is not derived from an existing
/// budget**, and deliberately far tighter than
/// [`CLONE_POLL_INTERVAL_MS`]'s 5s: that interval is sized to a `git clone`'s
/// duration, whereas what is being waited on here is a POST that is *already
/// in flight* travelling the last hop to `operations::admit`. The server
/// knows the id the instant `admit` returns — `note_minted` runs immediately
/// after it in `planner::plan_and_execute_tracked` — so the gap this bridges
/// is one handler dispatch over a loopback SSH forward, not a transfer. At
/// 5s the Cancel button and the progress bar would appear five seconds after
/// every fetch started, which is most of a small fetch.
///
/// Erring short is cheap in a way it is not for clone: this read runs no git,
/// mutates nothing, and costs the server one mutex lock
/// (`operations::lookup_by_key`), and the loop stops the moment it gets an
/// answer — for anything that settles fast, the write's own response wins the
/// race and no poll is ever sent at all.
const OPERATION_ID_POLL_INTERVAL_MS: u64 = 200;

/// The per-attempt deadline for one `by-key` poll — the same bound
/// [`CLONE_STATUS_POLL_TIMEOUT_MS`] sets, by aliasing it rather than
/// restating a number, because the reasoning transfers exactly: both
/// handlers answer from an in-memory map behind a mutex and do no I/O, so a
/// poll still outstanding past this is a dead tunnel rather than a slow
/// server, and giving up early to try again beats eating the whole budget
/// waiting.
const OPERATION_ID_POLL_TIMEOUT_MS: u64 = CLONE_STATUS_POLL_TIMEOUT_MS;

/// How many `by-key` polls to make before giving up on ever learning the id.
///
/// Derived from [`REQUEST_TIMEOUT_MS`] rather than chosen: that constant is
/// this client's bound on *one* HTTP attempt, so a write whose key has not
/// been admitted after a whole such window has not reached the server's
/// handler at all — either it is still hung on a socket
/// [`send_write_with_key`]'s own retry has not yet given up on, or the route
/// does not exist because the server predates it. Neither becomes true by
/// polling longer.
///
/// Giving up here is **not** an error. It costs the operation its live
/// progress, its Cancel button and its reload-recovery entry; the write
/// itself is untouched and still settles from its own response, exactly as it
/// did before this route existed. That degradation is the whole reason the
/// budget can be this blunt.
///
/// Attempts, not wall-clock, is what is bounded — the same honesty
/// [`CLONE_POLL_MAX_ATTEMPTS`] states: a poll that itself times out on
/// [`OPERATION_ID_POLL_TIMEOUT_MS`] only adds slack on top of the floor, which
/// biases toward giving a flaky tunnel more time rather than less. Only one
/// poll is ever open at a time (see [`resolve_operation_id`]), so a long
/// budget cannot become a pile of concurrent requests.
const OPERATION_ID_POLL_MAX_ATTEMPTS: u32 =
    (REQUEST_TIMEOUT_MS / OPERATION_ID_POLL_INTERVAL_MS) as u32;

/// One `GET /api/operations/by-key/{key}` poll's outcome — the decision input
/// for [`operation_id_poll_step`] (M2.20f, #232).
///
/// Shaped from the handler's own three answers
/// (`handlers/operations.rs::operation_by_key`), not guessed: `200` with an
/// [`OperationByKeyResponse`], `404` for "no operation is admitted under that
/// key **yet**", and — the case no server response can represent — the poll
/// request itself failing before any answer was read.
///
/// Note what is deliberately *not* a variant: there is no "definitely never
/// will be admitted". The handler's own doc comment says it refuses to
/// distinguish "not admitted yet" from "never will be, or aged out", because
/// "an unguessable key means there is nothing safe to say about *why* it is
/// unrecognised". So the budget, not the status code, is what ends the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OperationIdPoll {
    /// `200 {"id": …}` — the server has admitted this key and minted an id.
    Found(OperationId),
    /// `404` — nothing is admitted under this key at this instant.
    NotAdmitted,
    /// The poll never got an answer: transport failure, timeout, an
    /// unparseable body, or any other status. Retryable, like `NotAdmitted`.
    PollError(String),
}

/// What to do after one `by-key` poll: bind the id, wait and poll again, or
/// stop trying and let the write settle from its own response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OperationIdStep {
    Bind(OperationId),
    KeepPolling,
    GiveUp,
}

/// The decision behind the `by-key` poll loop — the same shape as
/// `clone_poll_step`, and for the same reason: the retryable-vs-terminal
/// distinction is a rule worth stating once, in one place, rather than
/// re-deriving it inside a loop body.
///
/// Only [`OperationIdPoll::Found`] is an answer. Both other variants are
/// retried within budget, and both for the reason the clone loop retries its
/// own `Unknown`/`PollError`: a 404 here is the *expected* first answer (the
/// POST this key belongs to is still travelling to the handler), and a failed
/// poll is the flaky-tunnel condition the whole feature exists to ride out.
///
/// Unlike `clone_poll_step`, exhaustion resolves to [`OperationIdStep::GiveUp`]
/// rather than to an error message. Nothing failed: the write is still in
/// flight and still settles from its own response. The only thing lost is the
/// live half — progress, Cancel, reload recovery — so surfacing an error here
/// would report a failure that has not happened.
///
/// **Not host-testable where it sits.** `api.rs` compiles only for wasm
/// (`main.rs`: `#[cfg(target_arch = "wasm32")] mod api;`) because `gloo-net`
/// is a wasm32-only dependency, so this function has no `cargo test` reach
/// from here; it is written free-standing, taking no wasm types, precisely so
/// it can be lifted into `features::operations::core` — which is host-compiled
/// and host-tested — without changing a line of it.
fn operation_id_poll_step(
    outcome: OperationIdPoll,
    attempts_made: u32,
    max_attempts: u32,
) -> OperationIdStep {
    match outcome {
        OperationIdPoll::Found(id) => OperationIdStep::Bind(id),
        OperationIdPoll::NotAdmitted | OperationIdPoll::PollError(_) => {
            if attempts_made >= max_attempts {
                OperationIdStep::GiveUp
            } else {
                OperationIdStep::KeepPolling
            }
        }
    }
}

/// One `GET /api/operations/by-key/{key}` attempt, mapped to the outcome
/// [`operation_id_poll_step`] decides on. Bounded by
/// [`OPERATION_ID_POLL_TIMEOUT_MS`] independently of the interval between
/// attempts, so a hung poll cannot itself eat the budget.
///
/// Cache-busted with `t=<ms>` like every other read here, and this one needs
/// it more than most: the answer *flips* from `404` to `200` partway through
/// the loop, so an iOS Safari cache that served the first 404 back for every
/// later poll would make the id permanently unlearnable — a stuck Cancel
/// button and no progress bar, with no error anywhere to explain it.
async fn fetch_operation_id_by_key(key: &IdempotencyKey) -> OperationIdPoll {
    let url = format!(
        "/api/operations/by-key/{}?t={}",
        encode_component(key.as_str()),
        js_sys::Date::now()
    );
    let resp = match with_deadline(req_get(&url).send(), OPERATION_ID_POLL_TIMEOUT_MS).await {
        None => return OperationIdPoll::PollError(timeout_error()),
        Some(Err(e)) => return OperationIdPoll::PollError(network_error(e)),
        Some(Ok(resp)) => resp,
    };
    // The handler's `by_key_not_found`, *and* the bare 404 an older server
    // gives for a route it does not have. Indistinguishable on the wire and
    // treated identically on purpose: both mean "no id to bind right now",
    // and the budget is what eventually separates "not yet" from "never".
    if resp.status() == 404 {
        return OperationIdPoll::NotAdmitted;
    }
    if !resp.ok() {
        return OperationIdPoll::PollError(format!("HTTP {}", resp.status()));
    }
    match with_deadline(
        resp.json::<OperationByKeyResponse>(),
        OPERATION_ID_POLL_TIMEOUT_MS,
    )
    .await
    {
        None => OperationIdPoll::PollError(timeout_error()),
        Some(Err(e)) => OperationIdPoll::PollError(e.to_string()),
        Some(Ok(body)) => OperationIdPoll::Found(body.id),
    }
}

/// Learn the [`OperationId`] the server minted for `key`, **while the write
/// that carries it is still running** (M2.20f, #232).
///
/// # Why this exists at all
///
/// A tracked write's own response cannot answer this in time.
/// `planner::plan_and_execute_tracked` ends with `record.wait_terminal().await`
/// (`crates/git-vista-server/src/planner.rs:204`), so the `POST` — and with it
/// the `x-git-vista-operation` header [`receipt`] reads — is withheld until the
/// operation is already over. Everything that has to act *during* the
/// operation (the Cancel button, the progress stream, the `localStorage` entry
/// a reload recovers from) therefore has to reach the id another way. The
/// server has it the whole time: `operations::note_minted` runs immediately
/// after `admit` (`planner.rs:142`), and `lookup_by_key` reads it out without
/// waiting for anything.
///
/// # The admit race, and why it is a bounded retry rather than a single call
///
/// The caller fires the write **without awaiting it** and calls this straight
/// away, so this poll routinely reaches the server *before* the write it is
/// asking about does. A `404` is therefore the expected first answer, not a
/// failure — see [`OperationIdPoll`] — and the loop is what turns that
/// expected miss into an answer a beat later.
///
/// `should_stop` is checked before every attempt and lets the caller end the
/// loop early once the write's own response has made the question moot (it
/// answered with an id, or it settled without one). Without it, a fetch that
/// finishes in 50ms would leave this polling a key nobody is waiting on.
///
/// Exactly one request is open at any moment — attempt, await, sleep, repeat —
/// so the budget bounds total time, never the number of live connections.
///
/// `None` means the id was never learned within
/// [`OPERATION_ID_POLL_MAX_ATTEMPTS`], or the caller asked to stop. It is not
/// an error and carries no message: the write is untouched and still settles
/// from its own response.
pub async fn resolve_operation_id(
    key: IdempotencyKey,
    should_stop: impl Fn() -> bool,
) -> Option<OperationId> {
    for attempt in 1..=OPERATION_ID_POLL_MAX_ATTEMPTS {
        if should_stop() {
            return None;
        }
        // Sleep first, like `poll_clone_status_until_settled`: the write was
        // fired microseconds ago and has not yet reached a handler, so an
        // immediate poll is a guaranteed 404 and a wasted round trip.
        sleep_ms(OPERATION_ID_POLL_INTERVAL_MS).await;
        if should_stop() {
            return None;
        }
        let outcome = fetch_operation_id_by_key(&key).await;
        match operation_id_poll_step(outcome, attempt, OPERATION_ID_POLL_MAX_ATTEMPTS) {
            OperationIdStep::Bind(id) => return Some(id),
            OperationIdStep::GiveUp => return None,
            OperationIdStep::KeepPolling => {}
        }
    }
    // Unreachable in practice — the last iteration always supplies
    // `attempt == OPERATION_ID_POLL_MAX_ATTEMPTS`, which `operation_id_poll_step`
    // resolves to `GiveUp`. Kept as the same honest fallback the clone loop
    // keeps rather than `unreachable!()`: an off-by-one here must degrade to
    // "no live progress", never to a panic that takes the whole app down
    // mid-fetch.
    None
}

/// Read an operation's current record (`GET /api/operations/{id}`) —
/// answers whether or not the operation has finished (the route's own doc
/// comment, `handlers/operations.rs::operation_status`). Used only for
/// the boot-time reconnect (#232, M2.20f): a resumed Fetch/Pull needs to
/// know, once, what state it is in before deciding whether to replay a
/// settlement or resubscribe to its SSE stream. Everywhere else on this
/// client the record is read live, off the stream itself — this is the
/// one place a plain poll is the right tool, because there is no stream
/// to subscribe to until this call has answered.
pub async fn fetch_operation_status(id: &OperationId) -> Result<OperationStatus, String> {
    let url = format!("/api/operations/{}?t={}", id.as_str(), js_sys::Date::now());
    req_get(&url)
        .send()
        .await
        .map_err(network_error)?
        .json::<OperationStatus>()
        .await
        .map_err(|e| e.to_string())
}

/// What the server said in answer to `POST /api/operations/{id}/cancel`
/// (M2.20f, #232).
///
/// A cancel is a *request* to stop, never a promise the operation is
/// finished: the endpoint "never terminalises the record itself... only the
/// pipeline may do that, and only after it has observed what actually
/// happened to the repository" (the server's own doc comment on
/// `handlers::operations::cancel_operation`). So this type answers only the
/// narrower question — did the server accept the attempt — and the real
/// outcome still arrives later, through the operation's ordinary settlement
/// path (the progress stream, or `GET /api/operations/{id}`), exactly like
/// every other write here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// `202` — the cancellation latch is set, or was already set by an
    /// earlier, replayed request. The server answers a repeated cancel of a
    /// still-running operation with the same `202` (idempotent, not an
    /// error), so this variant covers both.
    Requested,
    /// `409` — the operation had already reached a terminal state before the
    /// cancel arrived. Nothing to stop; read the recorded result instead.
    AlreadyFinished,
    /// `409` — this kind of operation does not watch the cancellation latch
    /// (the server's `planner::honours_cancellation` said no). Setting it
    /// would be a no-op dressed up as an action. The client should never
    /// reach this in practice — the menu is meant to keep a cancel button
    /// from being offered at all for a non-cancellable kind — but the
    /// answer is still handled honestly rather than assumed impossible.
    NotCancellable,
    /// `404` — no such operation id: never issued, or evicted. Vanishingly
    /// unlikely mid-session (the id was read off this very operation's own
    /// bind), but still a real answer, not a transport failure.
    Unknown,
}

/// Ask the backend to cancel a running operation
/// (`POST /api/operations/{id}/cancel`, M2.20f, #232).
///
/// Deliberately **not** routed through [`send_write_with_key`]: unlike every
/// other write here, a cancel does not name a fresh user *intent* to
/// dedupe — it targets an operation id that already exists, and the server
/// itself already answers a repeated cancel of a still-running operation
/// with the same `202` (see [`CancelOutcome::Requested`]). So this call
/// carries no idempotency header and no body — the id in the URL is the
/// whole request — and goes straight through `req_post` + [`with_deadline`],
/// bounded by the ordinary [`REQUEST_TIMEOUT_MS`]: a cancel only sets a
/// latch, so unlike a fetch/pull it never waits on a transfer and needs
/// none of [`FETCH_TIMEOUT_MS`]'s slack.
///
/// Calls [`refuse_if_visualize`] as every other repo-write function here
/// does, even though a Fetch/Pull can never be in flight during Visualize
/// mode in the first place (`dispatch` already refuses them there) — this
/// is defense in depth matching the file's own convention, not a reachable
/// path.
pub async fn cancel_operation_request(id: &OperationId) -> Result<CancelOutcome, String> {
    refuse_if_offline()?;
    refuse_if_visualize()?;
    let url = format!("/api/operations/{}/cancel", id.as_str());
    let resp = match with_deadline(req_post(&url).send(), REQUEST_TIMEOUT_MS).await {
        None => return Err(timeout_error()),
        Some(Err(e)) => return Err(network_error(e)),
        Some(Ok(resp)) => resp,
    };
    match resp.status() {
        202 => Ok(CancelOutcome::Requested),
        404 => Ok(CancelOutcome::Unknown),
        409 => {
            let message = user_facing_error(&url, resp).await;
            Ok(if message.contains("already finished") {
                CancelOutcome::AlreadyFinished
            } else {
                CancelOutcome::NotCancellable
            })
        }
        _ => Err(user_facing_error(&url, resp).await),
    }
}
