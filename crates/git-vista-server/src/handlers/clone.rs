//! `POST /api/clone` and `POST /api/delete-clone` (Phase 12, reshaped by ADR
//! 0008): clone a public repo from a pasted URL into the persistent clones
//! store and hand its descriptor back so the browser can offer the mode
//! picker; delete a clone again on request, guarded to the clones root.
//!
//! `GET /api/clone-status/{key}` (#263) answers the same question a lost
//! `POST /api/clone` response would have: what happened to the attempt
//! admitted under this idempotency key. [`admit_clone`] is the registry
//! behind both routes — it also closes #264's server-side half, replaying a
//! *finished* attempt's recorded result instead of running a second `git
//! clone` for a key reused after completion.
//!
//! **#263 is only server-side complete as of this module** (review finding
//! — flagged rather than silently overclaimed, and checked directly against
//! `crates/git-vista/src/api.rs`'s `clone_request`, not assumed): `clone-status`
//! is built, authz-classified, and unit-tested, but the wasm client mints a
//! **fresh** idempotency key on every `POST /api/clone` call
//! (`write_json`/`write_json_with_timeout`'s own per-call key, no retry loop
//! in `clone_request` at all) and never polls this endpoint. A client that
//! loses the `POST /api/clone` response today still has no code path that
//! ever learns the clone finished, and even a manual retry would run under a
//! *different* key — the #260 symptom this issue exists for still
//! reproduces until the client is wired to (a) retain the key it sent and
//! (b) poll `clone-status` with it after a lost/failed response. Tracked as
//! a separate follow-up rather than expanded into this change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::Path as PathParam;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use git_vista_core::identity::WorktreeId;
use git_vista_protocol::{
    validate_clone_url, CloneRequest, DeleteCloneRequest, IdempotencyKey, RepositoryDescriptor,
    CLONE_IN_PROGRESS_SENTINEL,
};

use crate::state::{
    allow_repo_root, cleanup_clone, clones_root, delete_clone, descriptor_for, path_is_allowed,
    set_current, DeleteCloneOutcome,
};

/// A human-recognisable directory name for a clone of `url` — the URL's last
/// path segment, minus any `.git` suffix, restricted to safe filename
/// characters. `None` when nothing usable survives (the caller falls back to a
/// stamped name). The picker shows the directory base name (ADR 0008), so a
/// clone must not be called `clone-1721400000-0`.
fn clone_dir_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    // Strip the scheme+authority before taking the last segment — otherwise a
    // path-less URL like "https://host/" names the clone after the bare host.
    let path = path.split_once("://").map_or(path, |(_, rest)| rest);
    let path = path.trim_end_matches('/');
    let (_, tail) = path.split_once('/')?;
    let tail = tail.rsplit('/').next()?;
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let safe: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    // No hidden dirs, no "." / "..": dots can't lead or trail.
    let safe = safe.trim_matches('.').to_string();
    (!safe.is_empty()).then_some(safe)
}

/// First free directory under `root` for `name`: `name`, then `name-2`, `-3`, …
/// — clones persist (ADR 0008), so a second clone of the same repo needs its
/// own directory rather than evicting the first.
fn unique_dest(root: &Path, name: &str) -> PathBuf {
    let first = root.join(name);
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| root.join(format!("{name}-{n}")))
        .find(|p| !p.exists())
        .expect("some numeric suffix is free")
}

/// How [`run_guarded`] resolved when it did **not** hand back a value: either
/// the deadline won, or `fut` itself resolved to `Err` before the deadline.
/// Kept as an enum rather than folded into a single error type so the caller
/// can give each case its own HTTP status and message.
enum GuardedOutcome<E> {
    Failed(E),
    TimedOut,
}

/// Await `fut` under `timeout`, removing `dest` unless `fut` resolves to `Ok`
/// before the deadline fires.
///
/// # Why cleanup is a Drop guard and not a branch of the match below
///
/// MEASURED 2026-07-31, not assumed: a standalone axum server was driven by a
/// client that disconnected mid-request. Three flags, so no single one could
/// be read vacuously — `started=true` (the handler ran), `dropped=true` (it
/// unwound), and `timeout_arm=false` (the branch was skipped). The paired
/// positive, a client that waits, showed `timeout_arm=true`, so the arm
/// works; it simply does not run on disconnect.
///
/// Axum drops the handler future when the client goes away, which skips
/// **every** match arm including the timeout's. Cleanup written as a branch of
/// a completion path only runs if that path is reached, and cancellation is
/// the absence of all of them. So the half-written destination is removed by
/// a value's lifetime instead, which cancellation cannot skip.
///
/// This matters more once the client aborts on its own deadline (#216
/// follow-up): abort makes disconnect the *common* path, so a
/// timeout-arm-only cleanup would have quietly stopped running just as it
/// started being needed.
///
/// Extracted to its own function (rather than left inline in [`clone_repo`])
/// specifically so `guarded_timeout_removes_the_destination` and
/// `guarded_success_keeps_the_destination` below can drive the mechanism
/// directly, with an injectable deadline, instead of only through a full
/// clone over HTTP.
async fn run_guarded<T, E>(
    dest: &Path,
    timeout: std::time::Duration,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, GuardedOutcome<E>> {
    struct DestGuard<'a> {
        dest: &'a Path,
        keep: bool,
    }
    impl Drop for DestGuard<'_> {
        fn drop(&mut self) {
            if !self.keep {
                // Best-effort: the destination is a fresh, uniquely named
                // directory the caller created, so removing it cannot touch
                // anything the operator owns.
                let _ = std::fs::remove_dir_all(self.dest);
            }
        }
    }
    let mut guard = DestGuard { dest, keep: false };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(value)) => {
            // Survived: the destination is the caller's now.
            guard.keep = true;
            Ok(value)
        }
        Ok(Err(e)) => Err(GuardedOutcome::Failed(e)),
        // `guard` is still armed here, so the destination is removed as this
        // scope unwinds — no explicit cleanup call needed.
        Err(_elapsed) => Err(GuardedOutcome::TimedOut),
    }
}

/// #216/#263/#264: the process-wide registry of clone attempts keyed by their
/// idempotency key — the same key's whole lifecycle, from "running" through
/// "finished", not just the in-flight window #216 originally covered.
///
/// Deliberately **not** `operations::admit` (out of scope: that funnel, and
/// its `GitOperation`/planner plumbing, are reserved for M2's queued
/// sub-issues starting imminently — see this issue's task notes). It also
/// would not fit cleanly: `admit` requires a `GitOperation` (clone is
/// cataloging/filesystem work, not a write the planner drives — it is not in
/// the closed `GitOperation` enum and has no plan/hash to admit against) and a
/// `RepositoryToken`/`WorktreeToken`, neither of which exists yet at the point
/// a clone must be deduplicated — the destination directory hasn't been
/// created, let alone classified as a repository. Reusing `operations::Record`
/// verbatim would mean shoehorning clone into a shape built for something else
/// (and dragging in `OperationHandle`'s watch-channel/SSE machinery this
/// endpoint doesn't need — polling `GET`, not a live stream, is what #263
/// actually asks for); a small **structurally parallel** tracker — same
/// admit/replay shape, its own types — is the more honest fit.
///
/// A `HashMap`, not a `HashSet` (unlike the #216-era version of this static):
/// #264 requires remembering more than "busy right now" — a *finished*
/// attempt's outcome must be replayable too, so a second `POST /api/clone`
/// under the same key answers from the record instead of running a second
/// `git clone`. [`CloneRecord::outcome`] is `None` while running and `Some`
/// once terminal, so one map now covers both windows [`admit_clone`] used to
/// need a `HashSet` and an implicit "not in the set" for.
///
/// **In-memory only, deliberately not durable** (review finding) — unlike
/// `operations.rs`'s registry, which survives a restart via
/// `operations::rehydrate()` reading `crate::durable::recover()`'s journal
/// rows. A server restart wipes every clone record instantly, including one
/// still `Running` at the moment of restart. This is a real, acknowledged
/// gap, not parity with `operations.rs`: #263 covers the scenario it was
/// filed for — a *client*-side interruption (dropped SSH tunnel, a suspended
/// iOS tab, a dismissed modal) while the server keeps running — not a
/// server restart coinciding with an in-flight clone. Extending this to the
/// same durable-journal/rehydrate mechanism `operations.rs` uses is real,
/// separable follow-up work if that gap ever matters in practice, not
/// something this fix silently promises.
static CLONE_RECORDS: OnceLock<StdMutex<HashMap<IdempotencyKey, CloneRecord>>> = OnceLock::new();

fn clone_records() -> &'static StdMutex<HashMap<IdempotencyKey, CloneRecord>> {
    CLONE_RECORDS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// How many finished clone attempts stay replayable at once. Far smaller than
/// `operations::MAX_RECORDS` (256): clones are a deliberate, occasional
/// action (paste a URL, tap Clone), never the burst of many-per-second writes
/// the general registry is sized for, so the same headroom would only let a
/// runaway client grow this map for no real benefit.
const MAX_CLONE_RECORDS: usize = 64;

/// How long a finished clone's outcome stays replayable — four times
/// `operations::RECORD_TTL_SECS` (one hour), deliberately not the same value.
///
/// A quick write (commit, branch, checkout) finishes in well under a second,
/// so an hour of slack already covers any realistic reconnect. A clone can
/// legitimately run for the *entire* [`CLONE_TIMEOUT`] (ten minutes) before
/// the client's own `CLONE_TIMEOUT_MS` gives up, and the #260/#263 scenario
/// this module now fixes is exactly a client that went away *during* that
/// long wait — SSH tunnel dropped, iOS suspended the tab, the modal got
/// dismissed — and does not reconnect on any predictable schedule. A window
/// sized for a sub-second write would lose the descriptor to a user who
/// starts a clone before a meeting and checks back after it. Four hours is
/// still a TTL, not a log: still evicted, still capped by
/// [`MAX_CLONE_RECORDS`], just sized to this one write's slower, rarer,
/// "walk away and come back" shape instead of reusing a constant tuned for
/// ones that finish instantly.
const CLONE_RECORD_TTL_SECS: i64 = 4 * crate::operations::RECORD_TTL_SECS;

/// The terminal result of one clone attempt, stored verbatim so a later
/// request under the same key gets exactly what the first attempt would have
/// returned — the same "replay verbatim" contract `operations::Record` gives
/// every tracked write.
#[derive(Debug, Clone)]
enum CloneOutcome {
    Succeeded(RepositoryDescriptor),
    Failed { status: u16, message: String },
}

/// One idempotency key's clone record: still running (`outcome: None`) or
/// finished (`Some`), plus the timestamps [`evict_clone_records`] needs.
///
/// `url` is the exact `CloneRequest.url` the key was first admitted with —
/// the load-bearing safety property `operations::admit`'s own
/// `operation_hash` field exists for (review finding, #263/#264): a key must
/// never answer with a result computed for a *different* request. Without
/// this, a client bug or key reuse across two different clone intents could
/// silently replay the wrong repository's descriptor as if it were the one
/// just requested.
#[derive(Debug)]
struct CloneRecord {
    url: String,
    outcome: Option<CloneOutcome>,
    ended_at: Option<i64>,
}

/// Drop terminal (finished) records past [`CLONE_RECORD_TTL_SECS`], then
/// oldest-finished-first until the map is within [`MAX_CLONE_RECORDS`].
///
/// A record still running (`outcome: None`, `ended_at: None`) is never
/// touched, at any age or size — a request holds the guard for it, and
/// evicting it here would let a concurrent retry see "not present" and start
/// a second `git clone`, the exact TOCTOU #216 exists to close.
fn evict_clone_records(reg: &mut HashMap<IdempotencyKey, CloneRecord>, now: i64) {
    reg.retain(|_, record| {
        record
            .ended_at
            .is_none_or(|ended| now.saturating_sub(ended) <= CLONE_RECORD_TTL_SECS)
    });
    let Some(mut over) = reg.len().checked_sub(MAX_CLONE_RECORDS) else {
        return;
    };
    let mut terminal: Vec<(IdempotencyKey, i64)> = reg
        .iter()
        .filter_map(|(k, r)| r.ended_at.map(|ended| (k.clone(), ended)))
        .collect();
    terminal.sort_by_key(|(_, ended)| *ended); // oldest-finished first
    for (key, _) in terminal {
        if over == 0 {
            break;
        }
        reg.remove(&key);
        over -= 1;
    }
}

/// What starting a clone under `key` resolves to.
enum CloneAdmission {
    /// A new attempt (or no key at all): run it, holding [`CloneGuard`] for
    /// the duration.
    Fresh(CloneGuard),
    /// The key names an attempt that is already finished: answer with its
    /// recorded outcome and run no git at all — the #264 fix. (An attempt
    /// still *running* under this key is not a variant here — see the
    /// `Err(CONFLICT)` return below — because unlike a finished result there
    /// is nothing yet to hand back; refusing outright, same as #216, is still
    /// correct for that window.)
    Replay(Result<Json<RepositoryDescriptor>, (StatusCode, String)>),
}

/// Admit `key` as a fresh clone attempt for `url`, refuse it as a duplicate
/// of one still running, replay a finished one's recorded result, or refuse
/// it outright as a **key collision** — the same key reused for a genuinely
/// different `url` than it was first admitted with.
///
/// `key` is `None` when the request carries no idempotency key at all — a
/// direct call outside the middleware's task-local scope (the unit tests
/// below), or, in principle, a client that omitted the header. Without a key
/// there is no "same attempt" to compare against, so this can't dedupe and
/// doesn't try: it always admits fresh.
///
/// This is the single critical section: the check-and-insert happens under
/// one lock acquisition, so two concurrent calls for the same key cannot both
/// observe "not present" and both proceed — exactly the property
/// `operations::admit`'s doc comment names for its own map, applied here to
/// clone's narrower one.
fn admit_clone(
    key: Option<IdempotencyKey>,
    url: &str,
) -> Result<CloneAdmission, (StatusCode, String)> {
    let Some(key) = key else {
        return Ok(CloneAdmission::Fresh(CloneGuard {
            key: None,
            url: url.to_string(),
            finished: false,
        }));
    };
    let now = crate::activity::now_secs();
    let mut reg = clone_records().lock().expect("clone records lock");
    evict_clone_records(&mut reg, now);

    if let Some(record) = reg.get(&key) {
        // The load-bearing safety property (review finding): a key must
        // never answer with a result computed for a different request —
        // mirrors operations::admit's operation_hash check, same posture.
        if record.url != url {
            return Err((
                StatusCode::CONFLICT,
                "That idempotency key was already used for a different clone URL. \
                 Retry with a fresh key."
                    .to_string(),
            ));
        }
        return match &record.outcome {
            None => Err((
                StatusCode::CONFLICT,
                format!(
                    "A clone for this request is {CLONE_IN_PROGRESS_SENTINEL}. Wait for it \
                     to finish before retrying."
                ),
            )),
            Some(CloneOutcome::Succeeded(descriptor)) => {
                Ok(CloneAdmission::Replay(Ok(Json(descriptor.clone()))))
            }
            Some(CloneOutcome::Failed { status, message }) => {
                let status =
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                Ok(CloneAdmission::Replay(Err((status, message.clone()))))
            }
        };
    }

    reg.insert(
        key.clone(),
        CloneRecord {
            url: url.to_string(),
            outcome: None,
            ended_at: None,
        },
    );
    Ok(CloneAdmission::Fresh(CloneGuard {
        key: Some(key),
        url: url.to_string(),
        finished: false,
    }))
}

/// RAII completion for [`admit_clone`]: the claimed key's record is settled
/// exactly once — with the real outcome via [`CloneGuard::finish`] (the
/// normal path: `clone_repo` calls this once, right after the clone attempt
/// resolves, success or failure), or, if this guard drops without that ever
/// happening (a panic unwinding through the handler — `CatchPanicLayer`
/// converts it to a 500 response, but this registry entry would otherwise be
/// stuck `Running` forever, permanently refusing every future retry of this
/// key with `409 Conflict`), a generic recorded failure — mirroring
/// `operations::OperationHandle`'s own crash-safety net for exactly the same
/// reason.
///
/// No key at all (`key: None`, the no-idempotency-key case) makes both
/// `finish` and `Drop` no-ops: nothing was claimed, so nothing needs settling
/// — `url` is still populated in that case (both [`admit_clone`] construction
/// sites have it in hand), but goes unused, which is harmless.
///
/// `url` exists so [`record_outcome`] never has to fabricate one (#288,
/// review finding): the defensive `or_insert_with` branch it guards against
/// — the record having been evicted or otherwise missing by the time this
/// guard settles — needs a real URL to re-insert, and the caller's admitted
/// URL, carried here from the moment `admit_clone` had it, is that value.
#[derive(Debug)]
struct CloneGuard {
    key: Option<IdempotencyKey>,
    url: String,
    finished: bool,
}

impl CloneGuard {
    /// Record `result` as this key's terminal outcome, replacing the
    /// `Running` entry [`admit_clone`] inserted.
    fn finish(mut self, result: &Result<Json<RepositoryDescriptor>, (StatusCode, String)>) {
        self.finished = true;
        let Some(key) = self.key.take() else {
            return;
        };
        let outcome = match result {
            Ok(Json(descriptor)) => CloneOutcome::Succeeded(descriptor.clone()),
            Err((status, message)) => CloneOutcome::Failed {
                status: status.as_u16(),
                message: message.clone(),
            },
        };
        record_outcome(key, self.url.clone(), outcome);
    }
}

impl Drop for CloneGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(key) = self.key.take() {
            record_outcome(
                key,
                self.url.clone(),
                CloneOutcome::Failed {
                    status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    message: "The clone stopped without finishing. Check the repository \
                              before retrying."
                        .to_string(),
                },
            );
        }
    }
}

/// Settle `key`'s record to a terminal `outcome`, inserting one if eviction
/// (or, in principle, a bug) already dropped it — `finish`/`Drop` must always
/// be able to leave a replayable record behind, never silently no-op.
///
/// `url` is the caller's real, admitted URL — [`CloneGuard`] carries it from
/// the moment `admit_clone` had it — used only by the `or_insert_with` branch
/// below, when there's no existing record to `and_modify`. Before #288 that
/// branch fabricated a placeholder URL instead of taking one from the caller;
/// a re-inserted record now carries the same URL admission would have stored,
/// so a later `admit_clone` for this key compares against the truth rather
/// than a manufactured mismatch.
fn record_outcome(key: IdempotencyKey, url: String, outcome: CloneOutcome) {
    let now = crate::activity::now_secs();
    if let Ok(mut reg) = clone_records().lock() {
        reg.entry(key)
            .and_modify(|r| {
                r.outcome = Some(outcome.clone());
                r.ended_at = Some(now);
            })
            .or_insert_with(|| CloneRecord {
                url,
                outcome: Some(outcome),
                ended_at: Some(now),
            });
    }
}

/// The response shape of [`clone_status`] — [`OperationStatus`]-shaped in
/// spirit (running vs. a terminal, replayable outcome) but its own type: a
/// clone attempt has no `GitOperation`, no repository/worktree token before it
/// succeeds, and no recovery strategy, so borrowing `OperationStatus` itself
/// would mean populating fields that don't apply rather than describing this
/// endpoint's actual shape.
///
/// [`OperationStatus`]: git_vista_protocol::OperationStatus
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CloneStatusResponse {
    /// Still running; no result yet.
    Running,
    /// Finished successfully — the descriptor a lost `POST /api/clone`
    /// response would have carried.
    Succeeded { descriptor: RepositoryDescriptor },
    /// Finished with a failure; the same status/message the original response
    /// would have carried.
    Failed { status: u16, message: String },
}

/// `GET /api/clone-status/{key}` (#263): what happened to a clone attempt
/// admitted under `key`, for a client that lost the original `POST
/// /api/clone` response and wants to reconcile without re-POSTing.
///
/// Keyed by the client's own [`IdempotencyKey`], not a server-minted id like
/// `GET /api/operations/{id}`: an operation id only reaches the client inside
/// a response header, and a lost response is exactly the failure mode this
/// endpoint exists to recover from, so relying on one would reintroduce the
/// same single point of failure one layer up. The idempotency key, by
/// contrast, the client mints and holds *before* sending the `POST` — it
/// survives the response being lost by construction.
///
/// Unknown/expired/malformed keys answer identically (404): a key is
/// client-chosen, not a server secret, but this still avoids distinguishing
/// "never existed" from "evicted" for no benefit to a legitimate caller.
pub(crate) async fn clone_status(PathParam(key): PathParam<String>) -> Response {
    let Ok(key) = IdempotencyKey::new(key) else {
        return clone_status_not_found();
    };
    let now = crate::activity::now_secs();
    let body = {
        let mut reg = clone_records().lock().expect("clone records lock");
        evict_clone_records(&mut reg, now);
        reg.get(&key).map(|record| match &record.outcome {
            None => CloneStatusResponse::Running,
            Some(CloneOutcome::Succeeded(descriptor)) => CloneStatusResponse::Succeeded {
                descriptor: descriptor.clone(),
            },
            Some(CloneOutcome::Failed { status, message }) => CloneStatusResponse::Failed {
                status: *status,
                message: message.clone(),
            },
        })
    };
    match body {
        Some(body) => Json(body).into_response(),
        None => clone_status_not_found(),
    }
}

fn clone_status_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        "No such clone attempt — it may have finished long enough ago to be forgotten.",
    )
        .into_response()
}

/// Clone a public repository from a pasted URL into the persistent clones
/// store (ADR 0008) and open it look-only pending the operator's mode choice.
///
/// Same B3 posture as the other git handlers: shell out to `git clone` and forward
/// git's own error text (bad host, repo not found, …) verbatim. The URL is
/// validated by [`validate_clone_url`] — only `http(s)://`/`git://`, so a pasted
/// SSH URL can't trigger a key prompt — and is passed as its own argv entry, never
/// a shell line. A full clone is made; the graph view's paged history walk
/// (`walk_history_topo`, `handlers/read.rs`) has no `HISTORY_LIMIT` cap and is
/// not bounded by anything downstream of this handler — `HISTORY_LIMIT` only
/// caps the Activity panel's separate remote-commit read (`activity.rs`). Found
/// misleading during the #218 investigation. Clones persist under the clones
/// root (ADR 0008) until deleted via `/api/delete-clone`.
///
/// #216/#263/#264: unlike every other write, this handler used not to be
/// operation-tracked at all, so the idempotency key the client already sends
/// bought it nothing — the key reaches this task-local scope regardless (the
/// M1.08 `idempotency` middleware wraps every `/api/*` route, clone included),
/// but nothing here used to read it. [`admit_clone`] fixes that on both
/// windows a clone can be retried in:
///
/// - **Still running** (#216): a retry that overlaps a still-running first
///   attempt is refused before either can reach [`unique_dest`] — the actual
///   race, since two concurrent calls can both see the same destination path
///   as free before either creates it.
/// - **Already finished** (#264): a retry after the first attempt completed
///   answers with the recorded descriptor and runs no `git clone` at all,
///   instead of `unique_dest` handing it a fresh `-2` directory for a repo
///   already on disk.
///
/// The actual clone runs in [`run_clone`], a plain function with no knowledge
/// of the registry — [`clone_repo`] itself is only the admit/finish
/// bookkeeping around it, so [`CloneGuard::finish`] sees and records the
/// *real* outcome (whichever of `run_clone`'s many return points produced it)
/// without every one of those return points needing to know a registry
/// exists.
pub(crate) async fn clone_repo(
    Json(req): Json<CloneRequest>,
) -> Result<Json<RepositoryDescriptor>, (StatusCode, String)> {
    match admit_clone(crate::operations::current_key(), &req.url)? {
        CloneAdmission::Replay(result) => result,
        CloneAdmission::Fresh(guard) => {
            let result = run_clone(req).await;
            guard.finish(&result);
            result
        }
    }
}

/// The actual clone attempt: everything `clone_repo` used to do directly,
/// unchanged, now run under [`admit_clone`]'s guard rather than doing the
/// admission itself.
async fn run_clone(req: CloneRequest) -> Result<Json<RepositoryDescriptor>, (StatusCode, String)> {
    let url = match validate_clone_url(&req.url) {
        Ok(u) => u,
        Err(e) => return Err((StatusCode::BAD_REQUEST, e)),
    };

    let root = clones_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!(
            "git-vista: /api/clone couldn't create {}: {e}",
            root.display()
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't prepare temp dir: {e}"),
        ));
    }
    // The clones root is an allowed root (M1.03): every clone registers under it,
    // and nothing outside it can be served. Adding it here (rather than only at
    // startup) also covers the case where a previous run's root was cleared.
    allow_repo_root(&root);
    // Recognisable, unique per-clone dir (ADR 0008): the repo's own name where
    // the URL yields one, a stamped name otherwise. Never collides — suffixed
    // (`-2`, `-3`, …) or stamped-and-countered.
    let dest = match clone_dir_name(&url) {
        Some(name) => unique_dest(&root, &name),
        None => {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            root.join(format!("clone-{stamp}-{n}"))
        }
    };

    println!("[/api/clone] cloning {url} → {}", dest.display());
    // D4 (#66, Task 7/D2): clone's own dedicated policy constructor, not the
    // general-purpose `sandbox::policy_for` other git spawns go through.
    //
    // The policy is built from the **clones root**, not from a repository: the
    // destination does not exist yet at policy time — `sandbox::policy_for`
    // would refuse it outright (`repo_paths::resolve` requires an existing
    // `.git`), which is exactly why this is a separate constructor rather than
    // a call to that one. `policy_for_clone` grants RW on `root` (what `git
    // clone` needs to be able to write) and pins `trusted = false`
    // structurally — see that function's doc comment for why clone must never
    // be reachable at the `Unsandboxed` tier even once per-repo operator trust
    // exists, unlike every other repository operation.
    //
    // A prior comment here described a `policy_for_clone` "still awaiting
    // approval" as the reason this went through the general policy path in
    // the interim; D4 is now approved and implemented, so that interim is
    // gone — this is the direct call the earlier comment anticipated.
    //
    // Also note this needs the resolver grant — see `NETWORK_ONLY_RO_TREES`:
    // sandboxed with only `/usr /bin /lib /lib64 /etc` readable, every clone of
    // a named remote would fail `Could not resolve host`. `policy_for_clone`
    // gets it the same way `policy_for` does, via `default_system_trees`.
    //
    // `git clone` takes no `-C`, but the launcher's fixed `-C <root>` is
    // harmless (the clones root is a real directory, created just above) and
    // keeps one argv shape for every spawn site. The URL still travels as its
    // own argv entry, after `validate_clone_url`, behind `--`.
    let dest_str = dest.to_string_lossy();
    // `--` so the URL is never read as an option, even past validation.
    let args: [&str; 4] = ["clone", "--", url.as_str(), &dest_str];
    let output = match crate::sandbox::policy_for_clone(&root) {
        Ok(policy) => {
            // #216: bound the child's lifetime. `git clone` against a remote that
            // stops answering mid-transfer does not fail — it *waits*, and this
            // handler waits with it, holding the request open forever. The client
            // now times out at 60s (`api.rs::REQUEST_TIMEOUT_MS`), but a client
            // timeout does not reap the child: without this the server keeps a
            // wedged git and a half-written destination directory indefinitely,
            // and the next attempt collides with the leftover.
            //
            // Ten minutes, not the client's sixty seconds, and deliberately so:
            // a large repository over a slow link is a *legitimately* long clone,
            // and killing a working transfer because a phone tether is slow would
            // trade one bug for a worse one. This bound exists to stop a wedged
            // clone living forever, not to enforce a latency budget.
            const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

            // `kill_on_drop`: without it a cancelled handler leaves the git
            // child running, still writing into a directory `run_guarded`
            // below has already removed. The orphan outlives the request that
            // authorised it, which is precisely what this milestone's process
            // lifecycle work (INV-8) exists to prevent.
            let spawned = crate::sandbox::spawn::command_async(&policy, &root, &args)
                .kill_on_drop(true)
                .output();
            match run_guarded(&dest, CLONE_TIMEOUT, spawned).await {
                Ok(o) => o,
                Err(GuardedOutcome::Failed(e)) => {
                    eprintln!("git-vista: /api/clone couldn't run git: {e}");
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Couldn't run git: {e}"),
                    ));
                }
                Err(GuardedOutcome::TimedOut) => {
                    eprintln!(
                        "git-vista: /api/clone timed out after {}s cloning {url}",
                        CLONE_TIMEOUT.as_secs()
                    );
                    return Err((
                        StatusCode::GATEWAY_TIMEOUT,
                        format!(
                            "The clone did not finish within {} minutes and was stopped. \
                             The remote may be unreachable or the repository very large.",
                            CLONE_TIMEOUT.as_secs() / 60
                        ),
                    ));
                }
            }
        }
        Err(e) => {
            eprintln!("git-vista: /api/clone couldn't build a sandbox policy: {e}");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            ));
        }
    };

    if !output.status.success() {
        // git printed why (host down, repo not found, auth needed…) on stderr.
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git clone failed.".to_string()
        } else {
            msg
        };
        cleanup_clone(&dest); // remove the empty/partial dir git may have left
        eprintln!("git-vista: /api/clone failed: {msg}");
        return Err((StatusCode::BAD_REQUEST, msg));
    }

    // Defence in depth (M1.03): the destination is built under the clones root by
    // construction, but confirm its canonical path really is within an allowed
    // root before serving it — a clone must never escape the clones directory.
    let canonical = std::fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone());
    if !path_is_allowed(&canonical) {
        cleanup_clone(&dest);
        eprintln!(
            "git-vista: /api/clone destination escaped the clones root: {}",
            dest.display()
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone destination was rejected.".to_string(),
        ));
    }

    // ADR 0008: clones persist — no eviction of any previous clone. Open the
    // fresh clone look-only (safe default); the browser follows up with the
    // Visualize/Active mode screen for it, using the descriptor we return.
    // Built from the handle `set_current` just gave back, not by re-reading
    // CURRENT — a concurrent /api/select landing in between must never hand
    // this response someone else's repository.
    let descriptor = set_current(&dest, git_vista_protocol::RepoMode::Visualize)
        .and_then(|h| descriptor_for(h.worktree));
    match descriptor {
        Some(d) => {
            println!("[/api/clone] now viewing {}", dest.display());
            Ok(Json(d))
        }
        // set_current fell to degraded mode (the clone didn't classify as a
        // repo) — surface it rather than handing back a phantom descriptor.
        None => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Clone finished but the repository could not be registered.".to_string(),
        )),
    }
}

/// `POST /api/delete-clone` (ADR 0008): remove a clone — catalog entry and
/// directory — addressed by opaque id. Malformed id → 400; unknown id → 404
/// (fail closed, like the reads); a repo that isn't a clone → 400; the
/// currently open repo → 409. The guard is [`crate::state::delete_clone`]:
/// nothing outside the canonical clones root is ever removed.
pub(crate) async fn delete_clone_repo(Json(req): Json<DeleteCloneRequest>) -> (StatusCode, String) {
    let worktree: WorktreeId = match req.worktree.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "Not a repository id.".to_string()),
    };
    match delete_clone(worktree, &clones_root()) {
        DeleteCloneOutcome::NotFound => (StatusCode::NOT_FOUND, "No such repository.".to_string()),
        DeleteCloneOutcome::NotAClone => (
            StatusCode::BAD_REQUEST,
            "Not a clone — only cloned repositories can be deleted.".to_string(),
        ),
        DeleteCloneOutcome::CurrentlyOpen => (
            StatusCode::CONFLICT,
            "This repository is open right now. Open another repository first.".to_string(),
        ),
        DeleteCloneOutcome::DeleteFailed(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't delete the clone: {e}"),
        ),
        DeleteCloneOutcome::Deleted => (StatusCode::OK, "Clone deleted.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_dir_name_takes_the_last_segment_and_strips_dot_git() {
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World.git"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://github.com/octocat/Hello-World"),
            Some("Hello-World".to_string())
        );
        assert_eq!(
            clone_dir_name("https://gitlab.com/group/sub/repo.git/"),
            Some("repo".to_string())
        );
    }

    #[test]
    fn clone_dir_name_drops_query_fragment_and_unsafe_characters() {
        assert_eq!(
            clone_dir_name("https://host/repo.git?ref=main#frag"),
            Some("repo".to_string())
        );
        assert_eq!(
            clone_dir_name("https://host/we ird$name"),
            Some("weirdname".to_string())
        );
    }

    #[test]
    fn clone_dir_name_refuses_names_that_reduce_to_nothing_or_dots() {
        assert_eq!(clone_dir_name("https://host/"), None);
        assert_eq!(clone_dir_name("https://host/..."), None);
        assert_eq!(clone_dir_name("https://host/$$$"), None);
    }

    #[test]
    fn unique_dest_suffixes_until_free() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo"));
        std::fs::create_dir_all(root.path().join("repo")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-2"));
        std::fs::create_dir_all(root.path().join("repo-2")).unwrap();
        assert_eq!(unique_dest(root.path(), "repo"), root.path().join("repo-3"));
    }

    /// The missing paired positive for #216's Drop-guard fix: the earlier
    /// standalone axum experiment proved the mechanism (`timeout_arm=false`
    /// on client disconnect, with a client-waits leg showing `true`), but
    /// never proved `run_guarded` itself removes the directory when its own
    /// `tokio::time::timeout` — not a client disconnect — is what fires.
    /// Without this leg, "the guard cleans up" was argued, not measured.
    #[tokio::test]
    async fn guarded_timeout_removes_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("half-cloned");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("partial-file"), b"partial").unwrap();
        assert!(
            dest.exists(),
            "premise: the directory exists before the run"
        );

        let never_finishes = std::future::pending::<Result<(), std::io::Error>>();
        let result = run_guarded(&dest, std::time::Duration::from_millis(20), never_finishes).await;

        assert!(
            matches!(result, Err(GuardedOutcome::TimedOut)),
            "expected TimedOut for a future that never resolves"
        );
        assert!(
            !dest.exists(),
            "the destination must be removed when the deadline wins, not left as a \
             half-cloned directory the next attempt collides with"
        );
    }

    /// The paired negative: a fast, successful future must NOT have its
    /// destination removed. Without this leg, `guarded_timeout_removes_the_destination`
    /// would be equally consistent with `run_guarded` always deleting `dest`
    /// regardless of outcome — a "cleanup" that destroys every real clone too.
    #[tokio::test]
    async fn guarded_success_keeps_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("real-clone");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("real-file"), b"real").unwrap();

        let resolves_immediately = std::future::ready(Ok::<_, std::io::Error>(42u32));
        let result = run_guarded(
            &dest,
            std::time::Duration::from_secs(60),
            resolves_immediately,
        )
        .await;

        assert!(
            matches!(result, Ok(42)),
            "the value must pass through unchanged"
        );
        assert!(
            dest.exists() && dest.join("real-file").exists(),
            "a successful clone's destination must survive — this is the failure mode \
             a naive 'always clean up' implementation would introduce"
        );
    }

    #[tokio::test]
    async fn delete_clone_refuses_a_malformed_and_an_unknown_id() {
        let (status, _) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            worktree: "not-an-id".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, msg) = delete_clone_repo(axum::Json(DeleteCloneRequest {
            // Valid id shape, never registered → fail-closed 404.
            worktree: "99999999-9999-5999-8999-999999999999".into(),
        }))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(msg, "No such repository.");
    }

    /// #216: proves the in-progress guard that stops two concurrent
    /// `/api/clone` requests for the same idempotency key from both spawning
    /// `git clone`.
    ///
    /// Modeled at the level of [`admit_clone`] rather than the full HTTP
    /// handler, for the same reason `guarded_timeout_removes_the_destination`
    /// above drives `run_guarded` directly instead of through a real clone:
    /// `admit_clone` is the very first thing `clone_repo` does, gating its
    /// *entire* body — a call refused here never reaches [`unique_dest`] (the
    /// actual TOCTOU: two concurrent calls can both see the same destination
    /// path as free before either creates it) or spawns git at all.
    /// Exercising this through a real concurrent HTTP clone would need either
    /// the public internet (this crate's one network test,
    /// `sandbox::clone_live`, is `#[ignore]`d for exactly that reason and does
    /// not run in `./dev gate`) or a local git-over-HTTP fixture server —
    /// infrastructure this property doesn't need in order to be proven.
    ///
    /// The two attempts are made to genuinely overlap — the second is not
    /// attempted until the first has already claimed the key and is
    /// deliberately held "still running" — via oneshot channels, rather than
    /// hoping the tokio scheduler interleaves them. `spawned` stands in for
    /// "reached the point `clone_repo` would call `unique_dest`/spawn `git
    /// clone`": only the winner may increment it.
    #[tokio::test]
    async fn overlapping_clone_attempts_for_the_same_key_are_not_both_admitted() {
        let key = IdempotencyKey::new("test-clone-overlap-216").unwrap();
        let spawned = std::sync::Arc::new(AtomicU64::new(0));

        let (first_claimed_tx, first_claimed_rx) = tokio::sync::oneshot::channel();
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();

        let first = {
            let key = key.clone();
            let spawned = std::sync::Arc::clone(&spawned);
            tokio::spawn(async move {
                let guard = match admit_clone(Some(key), "https://example.invalid/repo.git")
                    .expect("the first attempt must be admitted")
                {
                    CloneAdmission::Fresh(guard) => guard,
                    CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
                };
                // Stand-in for "spawned git clone" — the real handler's next
                // steps (`unique_dest`, then `command_async(...).output()`)
                // after this guard, before either has actually run.
                spawned.fetch_add(1, Ordering::SeqCst);
                let _ = first_claimed_tx.send(());
                // Held "in progress" — deliberately not finished — until the
                // second attempt has had its chance to race it. This *is*
                // the overlap: the second call below happens while this
                // guard is still alive, not after it.
                let _ = release_first_rx.await;
                guard.finish(&Ok(Json(test_descriptor("overlap-216"))));
            })
        };

        first_claimed_rx
            .await
            .expect("the first attempt must claim before the second is tried");

        // The second attempt, made while the first is still holding its
        // guard: genuinely overlapping, not sequential.
        let second_result = admit_clone(Some(key.clone()), "https://example.invalid/repo.git");

        let _ = release_first_tx.send(());
        first.await.expect("the first task must not panic");

        let Err((status, message)) = &second_result else {
            panic!("a second request for a key already in progress must be refused, not admitted");
        };
        assert_eq!(*status, StatusCode::CONFLICT);
        // #289: drives the real refusal arm (not a helper that just returns
        // the constant) and asserts on the actual returned message, imported
        // from the protocol crate rather than a hand-copied literal — this
        // is what makes rewording the surrounding sentence free while moving
        // or dropping the sentinel itself fails this test.
        assert!(
            message.contains(CLONE_IN_PROGRESS_SENTINEL),
            "the still-running refusal must contain the sentinel the client's \
             clone_response_should_poll matches on to keep polling this 409 — got {message:?}"
        );
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            1,
            "only the admitted attempt may reach the point clone_repo would spawn git \
             clone — a second admission here is exactly the race that let two concurrent \
             `git clone`s target the same destination directory"
        );
    }

    /// #264, the sibling issue's own reproduction scenario: once a clone
    /// under key `K` has **completed**, a second `POST /api/clone` under the
    /// same `K` must not run a new `git clone` — it must replay the
    /// completed descriptor.
    ///
    /// **Before the fix** (the #216-era `HashSet`, which forgot a key the
    /// instant its guard dropped): this admission would have returned
    /// `Fresh` again here, exactly the bug #264 reports — `unique_dest` would
    /// then have hand it a `-2` suffixed directory for a repo already on
    /// disk. Asserting `Replay(Ok(descriptor))` with the *same* descriptor is
    /// what distinguishes this fix from that behaviour.
    #[test]
    fn same_key_after_completion_replays_the_recorded_descriptor_instead_of_recloning() {
        let key = IdempotencyKey::new("test-clone-264-dedup").unwrap();
        let descriptor = test_descriptor("Hello-World");

        let guard = match admit_clone(Some(key.clone()), "https://example.invalid/repo.git")
            .expect("first admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };
        guard.finish(&Ok(Json(descriptor.clone())));

        match admit_clone(Some(key), "https://example.invalid/repo.git").expect("second admission")
        {
            CloneAdmission::Replay(Ok(Json(replayed))) => {
                assert_eq!(
                    replayed, descriptor,
                    "a completed clone's key must replay the SAME descriptor, not a fresh -2 clone"
                );
            }
            CloneAdmission::Replay(Err(e)) => {
                panic!("expected a replayed success, got a replayed failure instead: {e:?}")
            }
            CloneAdmission::Fresh(_) => {
                panic!("a completed key must replay, not be admitted fresh again")
            }
        }
    }

    /// The failure-side mirror of the success case above: a key that
    /// completed with an *error* also replays that error verbatim, the same
    /// "a refusal is an outcome" contract `operations::Record` gives every
    /// tracked write — a client retrying with the same key must not have git
    /// run again on the strength of a stale hope that this time it works
    /// (a genuinely new attempt mints a new key, per the module's own
    /// idempotency-key contract).
    #[test]
    fn same_key_after_a_failed_attempt_replays_the_recorded_failure() {
        let key = IdempotencyKey::new("test-clone-264-failure-replay").unwrap();

        let guard = match admit_clone(Some(key.clone()), "https://example.invalid/repo.git")
            .expect("first admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };
        guard.finish(&Err((
            StatusCode::BAD_REQUEST,
            "fatal: repository not found".to_string(),
        )));

        match admit_clone(Some(key), "https://example.invalid/repo.git").expect("second admission")
        {
            CloneAdmission::Replay(Err((status, message))) => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(message, "fatal: repository not found");
            }
            _ => panic!("expected the recorded failure to be replayed"),
        }
    }

    /// A genuinely different key is admitted fresh regardless of what another
    /// key's record holds — dedup must key on the client's stated intent, not
    /// spuriously refuse or replay across unrelated attempts.
    #[test]
    fn a_different_key_is_admitted_fresh_even_after_another_key_completed() {
        let first_key = IdempotencyKey::new("test-clone-264-other-key-a").unwrap();
        let second_key = IdempotencyKey::new("test-clone-264-other-key-b").unwrap();

        let guard = match admit_clone(Some(first_key), "https://example.invalid/repo.git")
            .expect("first admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };
        guard.finish(&Ok(Json(test_descriptor("repo-a"))));

        assert!(
            matches!(
                admit_clone(Some(second_key), "https://example.invalid/repo.git"),
                Ok(CloneAdmission::Fresh(_))
            ),
            "an unrelated key must be admitted fresh, not answered from someone else's record"
        );
    }

    /// The load-bearing safety property (review finding, mirrors
    /// `operations.rs`'s own `the_same_key_with_a_different_operation_is_a_conflict`):
    /// a key must never answer with a result computed for a *different*
    /// request. Reusing a key for a genuinely different URL — a client bug,
    /// or key reuse across two different clone intents — is refused outright
    /// rather than silently replaying (or worse, running a second `git
    /// clone` for) the wrong repository under a stale key.
    #[test]
    fn the_same_key_with_a_different_url_is_a_conflict() {
        let key = IdempotencyKey::new("test-clone-key-collision").unwrap();

        let guard = match admit_clone(Some(key.clone()), "https://example.invalid/repo-a.git")
            .expect("first admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };
        guard.finish(&Ok(Json(test_descriptor("repo-a"))));

        // Same key, DIFFERENT url — even though the first attempt already
        // completed (the #264 replay path would otherwise fire here).
        match admit_clone(Some(key), "https://example.invalid/repo-b.git") {
            Err((status, msg)) => {
                assert_eq!(status, StatusCode::CONFLICT);
                assert!(msg.contains("different clone URL"), "{msg}");
            }
            Ok(_) => panic!("a key reused for a different url must be refused, not admitted"),
        }
    }

    /// The fabrication this issue exists for (#288, review finding):
    /// `record_outcome`'s `or_insert_with` branch used to invent
    /// `"https://example.invalid/repo.git"` for a key with no existing
    /// record — and that fabricated string was byte-identical to the URL
    /// every other fixture in this module already used, so no test could
    /// tell a correctly-threaded URL from a fabrication. `real_url` below is
    /// deliberately **not** `"https://example.invalid/repo.git"`, so a
    /// regression back to the fabricated placeholder is something this test
    /// can actually catch rather than pass vacuously.
    ///
    /// Drives `record_outcome` directly on a key with **no** existing
    /// record — the exact `or_insert_with` branch, reached the same way
    /// eviction or a bug would reach it, without needing either — then
    /// proves the re-inserted record carries the caller's real URL rather
    /// than a placeholder two ways: `admit_clone` with the *same* key and
    /// *same* URL must replay the recorded outcome (a fabricated record
    /// would compare its placeholder against `real_url`, mismatch, and 409);
    /// and `admit_clone` with the same key but a *different* URL must still
    /// 409 as a key collision (proving the stored `url` really is
    /// `real_url` and not a wildcard or an ignored field — a record that
    /// answered "same" no matter what URL was compared would pass the first
    /// assertion for the wrong reason).
    #[test]
    fn record_outcome_re_insert_carries_the_caller_s_real_url_not_a_fabrication() {
        let key = IdempotencyKey::new("test-clone-288-re-insert-url").unwrap();
        let real_url = "https://example.invalid/re-inserted.git";

        record_outcome(
            key.clone(),
            real_url.to_string(),
            CloneOutcome::Succeeded(test_descriptor("re-inserted")),
        );

        // Same key, same URL: a correctly-threaded record replays. A
        // fabricated record's `url` ("https://example.invalid/repo.git")
        // would not equal `real_url`, so `admit_clone` would 409 here
        // instead of replaying — the exact false collision #288 reports.
        match admit_clone(Some(key.clone()), real_url).expect("same key, same url must not error") {
            CloneAdmission::Replay(Ok(Json(descriptor))) => {
                assert_eq!(descriptor.name, "re-inserted");
            }
            CloneAdmission::Replay(Err(e)) => {
                panic!("expected a replayed success, got a replayed failure instead: {e:?}")
            }
            CloneAdmission::Fresh(_) => {
                panic!(
                    "a key with an existing (re-inserted) record must replay, not be \
                     admitted fresh"
                )
            }
        }

        // Same key, a DIFFERENT url: must still 409. This is the negative
        // check — it proves the stored record genuinely holds `real_url`
        // (a mismatch is detected), ruling out a re-insert that silently
        // ignored the URL or stored an always-matching wildcard.
        match admit_clone(Some(key), "https://example.invalid/a-different-repo.git") {
            Err((status, msg)) => {
                assert_eq!(status, StatusCode::CONFLICT);
                assert!(msg.contains("different clone URL"), "{msg}");
            }
            Ok(_) => panic!("a different url against the re-inserted record must still 409"),
        }
    }

    /// A guard dropped without ever calling [`CloneGuard::finish`] — the
    /// stand-in for a panic unwinding through `clone_repo` — still leaves a
    /// terminal, replayable record behind rather than silently freeing the
    /// key for reuse. This is `operations::OperationHandle`'s own
    /// crash-safety net, applied to clone's tracker: without it, a panicking
    /// clone attempt would leave its key `Running` forever, permanently
    /// refusing every future retry with `409 Conflict` — worse than a generic
    /// recorded failure a retry can at least see and act on.
    #[test]
    fn a_dropped_guard_without_finish_records_a_generic_failure() {
        let key = IdempotencyKey::new("test-clone-guard-drop-without-finish").unwrap();

        let guard = match admit_clone(Some(key.clone()), "https://example.invalid/repo.git")
            .expect("first admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };
        drop(guard); // no `.finish(..)` — simulates an unwind

        match admit_clone(Some(key), "https://example.invalid/repo.git").expect("second admission")
        {
            CloneAdmission::Replay(Err((status, message))) => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert!(message.contains("stopped without finishing"), "{message}");
            }
            _ => panic!("a dropped-without-finish guard must leave a replayable failure behind"),
        }
    }

    /// A request with no idempotency key at all (a direct call outside the
    /// middleware's task-local scope, as every test above already is) is
    /// waved through rather than refused: there is no "same attempt" to
    /// compare against, so admission can't dedupe and doesn't try. Two such
    /// calls in a row must both be `Fresh` — a `None` key must never collide
    /// with itself.
    #[test]
    fn no_key_is_always_admitted_fresh() {
        assert!(matches!(
            admit_clone(None, "https://example.invalid/repo.git"),
            Ok(CloneAdmission::Fresh(_))
        ));
        assert!(matches!(
            admit_clone(None, "https://example.invalid/repo.git"),
            Ok(CloneAdmission::Fresh(_))
        ));
    }

    /// [`evict_clone_records`] must never drop a still-running (`ended_at:
    /// None`) record, at any age or over-cap pressure — a request holds the
    /// guard for it, and evicting it here would let a concurrent retry see
    /// "not present" and start a second `git clone`, the exact TOCTOU #216
    /// exists to close. Mirrors `operations::eviction_never_drops_a_live_record`.
    #[test]
    fn evict_clone_records_never_drops_a_running_record() {
        let mut reg = HashMap::new();
        let running_key = IdempotencyKey::new("test-evict-clone-running").unwrap();
        reg.insert(
            running_key.clone(),
            CloneRecord {
                url: "https://example.invalid/repo.git".to_string(),
                outcome: None,
                ended_at: None,
            },
        );
        // Overflow the cap with long-finished filler records.
        for n in 0..(MAX_CLONE_RECORDS + 8) {
            let k = IdempotencyKey::new(format!("test-evict-clone-filler-{n}")).unwrap();
            reg.insert(
                k,
                CloneRecord {
                    url: "https://example.invalid/repo.git".to_string(),
                    outcome: Some(CloneOutcome::Succeeded(test_descriptor("filler"))),
                    ended_at: Some(0), // ancient — expired AND over the cap
                },
            );
        }

        evict_clone_records(&mut reg, crate::activity::now_secs());

        assert!(
            reg.contains_key(&running_key),
            "a record still running must survive any amount of pressure"
        );
    }

    /// Both eviction rules in one test: an expired terminal record is dropped
    /// regardless of the cap, and — separately — an over-cap terminal record
    /// that has *not* expired is dropped oldest-finished-first.
    #[test]
    fn evict_clone_records_drops_expired_and_then_oldest_over_the_cap() {
        let mut reg = HashMap::new();
        let now = 1_000_000i64;

        let expired_key = IdempotencyKey::new("test-evict-clone-expired").unwrap();
        reg.insert(
            expired_key.clone(),
            CloneRecord {
                url: "https://example.invalid/repo.git".to_string(),
                outcome: Some(CloneOutcome::Succeeded(test_descriptor("expired"))),
                ended_at: Some(now - CLONE_RECORD_TTL_SECS - 1),
            },
        );
        let fresh_key = IdempotencyKey::new("test-evict-clone-fresh").unwrap();
        reg.insert(
            fresh_key.clone(),
            CloneRecord {
                url: "https://example.invalid/repo.git".to_string(),
                outcome: Some(CloneOutcome::Succeeded(test_descriptor("fresh"))),
                ended_at: Some(now),
            },
        );

        evict_clone_records(&mut reg, now);

        assert!(
            !reg.contains_key(&expired_key),
            "an expired record must be dropped"
        );
        assert!(
            reg.contains_key(&fresh_key),
            "a record inside the TTL must survive"
        );
    }

    /// `GET /api/clone-status/{key}` (#263): an unknown key is `404`, not a
    /// crash or a leaked distinction between "never existed" and "evicted".
    #[tokio::test]
    async fn clone_status_of_an_unknown_key_is_not_found() {
        let response = clone_status(PathParam("test-clone-status-unknown".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A malformed path segment (not token-shaped, so it can't name a key
    /// this server would ever have accepted) is the same 404, not a 500.
    #[tokio::test]
    async fn clone_status_of_a_malformed_key_is_not_found() {
        let response = clone_status(PathParam("not a token".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The #263 reproduction end to end at the poll endpoint: a still-running
    /// attempt reports `running`, and once finished the *same* poll reports
    /// the descriptor — proving a reconnecting client that lost the original
    /// `POST /api/clone` response can recover it here instead.
    #[tokio::test]
    async fn clone_status_reports_running_then_the_finished_descriptor() {
        let key = IdempotencyKey::new("test-clone-status-lifecycle").unwrap();
        let guard = match admit_clone(Some(key.clone()), "https://example.invalid/repo.git")
            .expect("admission")
        {
            CloneAdmission::Fresh(guard) => guard,
            CloneAdmission::Replay(_) => panic!("a fresh key must not replay"),
        };

        let running = clone_status(PathParam(key.as_str().to_string())).await;
        assert_eq!(running.status(), StatusCode::OK);

        let descriptor = test_descriptor("status-lifecycle-repo");
        guard.finish(&Ok(Json(descriptor.clone())));

        let body = axum::body::to_bytes(
            clone_status(PathParam(key.as_str().to_string()))
                .await
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["state"], "succeeded");
        assert_eq!(value["descriptor"]["repository"], descriptor.repository);
    }

    /// A minimal [`RepositoryDescriptor`] fixture — every field this module's
    /// tests need filled with an obviously-fake but valid value, `name`
    /// varied per call so distinct fixtures are trivially distinguishable in
    /// a failed assertion.
    fn test_descriptor(name: &str) -> RepositoryDescriptor {
        RepositoryDescriptor {
            repository: format!("repo-{name}"),
            worktree: format!("worktree-{name}"),
            name: name.to_string(),
            kind: git_vista_protocol::RepositoryKind::MainWorktree,
            read_only: true,
            path: None,
            remote_web_url: None,
            hook_policy: None,
        }
    }
}
