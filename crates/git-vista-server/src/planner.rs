//! The shared planner (M1.06b, #143): the one path every write action takes.
//!
//! Every write handler now does exactly three things: apply its own request
//! validation (unchanged wording, unchanged status codes), build one variant of
//! the closed [`GitOperation`] vocabulary (#142), and hand it to
//! [`plan_and_execute`]. From there this module:
//!
//!  1. **builds** the reviewable [`Plan`] — repository/worktree tokens, the
//!     live generation, the operation's SHA-256 hash, an expiry window, and the
//!     per-operation risk / preconditions / expected ref changes / recovery
//!     (the shapes ADR 0015 pinned in the golden fixture);
//!  2. **validates** it — the structural checks (hash equality, expiry), then
//!     the execution-time staleness gate (#145): generation equality and live
//!     re-verification of every build-time-held precondition, fail-closed;
//!  3. **executes** it — the *only* place in the server where a mutating git
//!     argv is constructed. The per-operation execution is the write handlers'
//!     old code moved here verbatim: same git commands, same journaling, same
//!     success/failure texts and status codes (this migration is a refactor,
//!     not a behavior change).
//!
//! A plan is built and executed inside a single request for now — no *route*
//! offers a client review roundtrip yet — but since M2.23c (#247) the seam is
//! real code, not just a seam-shaped spot in one function: [`build_plan_only`]
//! is the build stage alone (no guard, no execution) and [`submit_plan`] is
//! everything from the guard on (`validate → enforce_fresh → execute`),
//! composing the exact same stage functions [`plan_and_execute_in`] composes.
//! #144 closed the browser's ad-hoc-request escape hatch and #145 made the
//! validation load-bearing; #248/#249 put MCP routes on the two stages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use axum::http::StatusCode;
use sha2::{Digest, Sha256};

use git_vista_core::activity::ActivityKind;
use git_vista_core::identity::{GenerationInputs, RepositoryId};
use git_vista_core::seed::{parse_seed, Seed};
use git_vista_protocol::{
    Advisory, BranchName, CommitOid, ForcePublish, GenerationToken, GitOperation, IdempotencyKey,
    MergeStrategy, OperationHash, OperationId, OperationStage, Plan, Precondition,
    RecoveryStrategy, RefChange, RefName, RefState, RemoteName, RepositoryToken, RiskLevel,
    TagName, UnixSeconds, WorktreePath, WorktreeToken, IDEMPOTENCY_HEADER,
};

// The test suites under `planner/` open with `use super::*;`, and several of
// them still speak these protocol types directly even though the executors
// that used them in production moved into their own submodules.
#[cfg(test)]
use git_vista_protocol::{
    AmendFailureKind, CommitError, CommitFailureKind, CommitMessage, SignTagError,
    SignTagFailureKind, TagAnnotation,
};

use crate::git_cmd::{git_ok, rev_parse, ExecUnavailable};
use crate::journal;
use crate::sandbox::{network_need_for_operation, NetworkNeed};
use crate::state::{current_handle, reject_if_read_only};

/// How long a freshly issued plan stays executable. Enforced by [`validate`]
/// (#145); unreachable in practice while plans execute in the same request
/// they're built in, and the staleness window the moment a client-review
/// roundtrip exists.
const PLAN_TTL_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Build → validate → execute one operation against the current selection.
/// The single entry point every write handler calls; everything below it is
/// private to the planner.
pub(crate) async fn plan_and_execute(op: GitOperation) -> (StatusCode, String) {
    plan_and_execute_maybe_recovery(op, None).await
}

/// [`plan_and_execute`], for an operation that is itself the executed recovery
/// of an earlier one (M3.25, #78) — the row this admits records `recovers`,
/// and nothing else about the path differs.
///
/// The sole caller is [`crate::recovery_center::recover_operation`]. The live
/// re-derive-and-compare gate that decided `op` is the one operation allowed
/// to run has already happened *there*, before this function is reached — so
/// by the time `op` arrives it is exactly the operation the server itself
/// independently computed, the same posture every other write's `op` already
/// has at [`plan_and_execute`]. From here on a recovery inherits admission,
/// the staleness gate and the durable terminal record like any other mutation:
/// nothing below this call knows or cares that it is a recovery.
pub(crate) async fn plan_and_execute_recovery(
    op: GitOperation,
    recovers: OperationId,
) -> (StatusCode, String) {
    plan_and_execute_maybe_recovery(op, Some(recovers)).await
}

/// The shared body of [`plan_and_execute`] and [`plan_and_execute_recovery`]:
/// the gate-then-delegate block both take, differing only in whether the
/// admitted row names an earlier operation it recovers.
///
/// Deliberately *one* copy. Duplicating the read-only gate and the
/// idempotency-key requirement into a second entry point is exactly the drift
/// `contract_suite`'s `the_global_entry_point_delegates_through_the_lifecycle_to_the_pipeline`
/// exists to prevent — which is why that test now pins these three lines here,
/// on the path every write takes, and pins both public entry points to
/// delegating into it.
async fn plan_and_execute_maybe_recovery(
    op: GitOperation,
    recovers: Option<OperationId>,
) -> (StatusCode, String) {
    // The write gate, kept here as well as in the handlers (defense in depth —
    // no operation executes against a Visualize-mode selection).
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    // M1.08: every mutation needs the client's name for the intent behind it.
    // Required *here* rather than in a middleware route list, because a route
    // list drifts the moment someone adds an endpoint and this chokepoint
    // cannot — a handler that reaches the planner is a write, by definition.
    let Some(key) = crate::operations::current_key() else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "This request needs the {IDEMPOTENCY_HEADER} header, so a retry \
                 can be recognised as a retry. Reload the app to update."
            ),
        );
    };
    // D2 (#66, Task 7): the validated resolution — degraded-mode selections
    // and hostile/out-of-managed-root `.git` geometries refuse here, before
    // any mutating argv is built. See `state::resolve_target`'s doc comment.
    let (repo, entry) = match crate::state::resolve_target() {
        Ok(v) => v,
        Err(rejected) => return rejected,
    };
    let repo_id = Some(entry.handle.repository);
    plan_and_execute_tracked(
        key,
        repo,
        repo_id,
        selection_tokens(),
        PlanSource::Build(op),
        recovers,
    )
    .await
}

/// What [`plan_and_execute_tracked`] runs once admitted: build a plan from a
/// bare operation (the composed path, #143) or execute a plan that already
/// arrived from outside (the submit path, #249). Both admit/spawn/terminalise
/// identically — this enum is the whole seam that lets them share
/// [`plan_and_execute_tracked`] instead of each re-deriving it, which is ADR
/// 0016's funnel extended to plans that arrive pre-built.
enum PlanSource {
    /// The composed path: build the plan from this operation, still inside
    /// the guard, then execute it — [`plan_and_execute_in`].
    Build(GitOperation),
    /// The submit path: this plan already exists (minted earlier by
    /// [`build_plan_only`], possibly reviewed across a roundtrip) — take the
    /// guard and run `validate → enforce_fresh → execute` against it as-is —
    /// [`submit_plan`]. Boxed: `Plan` is ~4x the size of `GitOperation`, and
    /// clippy's `large_enum_variant` is right that an unboxed `Plan` here
    /// would make every `PlanSource` (including every `Build` one, which is
    /// on the hot path) pay `Plan`'s stack size for a variant most values
    /// never use.
    Submit(Box<Plan>),
}

impl PlanSource {
    /// The operation [`crate::operations::admit`] keys the idempotency
    /// registry on, regardless of which path produced it.
    fn operation(&self) -> &GitOperation {
        match self {
            PlanSource::Build(op) => op,
            PlanSource::Submit(plan) => &plan.operation,
        }
    }

    /// The hash `admit` compares a reused key's second request against. A
    /// submitted plan already carries its own — computed once, at build time
    /// — so this reuses it rather than recomputing: for `Submit`, hashing
    /// again here would just reproduce the plan's own `operation_hash` field.
    /// Always **derived from the operation**, never read off the plan — for
    /// `Submit` as much as for `Build`.
    ///
    /// The distinction is a trust boundary, and getting it wrong is exploitable.
    /// `admit()` uses this hash to decide Fresh/Existing/Conflict, and it runs
    /// *before* `validate()` — which is the thing that checks
    /// `operation_hash(&plan.operation) == plan.operation_hash`. A submitted
    /// plan arrives from outside (today, from an LLM's raw tool-call argument
    /// via the MCP bridge), so its `operation_hash` field is client-supplied
    /// data that nothing has verified at the moment admission needs it.
    ///
    /// Trusting that field would let a plan whose declared hash collides with
    /// an already-admitted key take `Admission::Existing` and replay the *first*
    /// operation's terminal result — the second operation never validated, never
    /// executed, and the caller told it succeeded. The inverse poisons a key:
    /// a first submission carrying a mismatched hash makes every later,
    /// correctly-hashed resubmission `Admission::Conflict` forever.
    ///
    /// Recomputing here costs one hash and removes the whole class. `validate()`
    /// still runs later and still rejects a plan whose declared hash disagrees
    /// with its operation — that check is about the plan's own integrity, and it
    /// is not a substitute for this one, because it happens after admission has
    /// already committed.
    fn hash(&self) -> OperationHash {
        operation_hash(self.operation())
    }
}

/// Run one operation under a recorded lifecycle (M1.08, #61): admit it to the
/// registry under the client's key, run the pipeline **detached**, and wait for
/// the recorded terminal result.
///
/// Three things follow from the detached run, and all three are the point:
///
/// - **A dropped client no longer cancels git.** Axum drops the handler future
///   when the connection dies; here that only drops the *waiting*. The pipeline
///   is a `tokio::spawn`ed task that runs to completion and records its result,
///   so the outcome is knowable afterwards instead of lost.
/// - **A retry replays instead of re-running.** A second request carrying the
///   same key never reaches the pipeline: it awaits the in-flight record, or
///   returns the recorded response verbatim. One user action, one git command.
/// - **A key reused for a different operation is refused**, never answered with
///   a result computed for something else.
///
/// The response body and status are unchanged from before this existed — git's
/// own message, forwarded verbatim. Only the operation-id response header is
/// new, so no endpoint's contract changed shape.
async fn plan_and_execute_tracked(
    key: IdempotencyKey,
    repo: PathBuf,
    repo_id: Option<RepositoryId>,
    tokens: (RepositoryToken, WorktreeToken),
    source: PlanSource,
    recovers: Option<OperationId>,
) -> (StatusCode, String) {
    let hash = source.hash();
    let (repository, worktree) = tokens.clone();

    let (handle, record) = match crate::operations::admit(
        &key,
        source.operation(),
        &hash,
        repository,
        worktree,
        recovers,
    ) {
        crate::operations::Admission::Fresh(handle, record) => (handle, record),
        crate::operations::Admission::Existing(record) => {
            // The same intent, already in flight or already answered.
            crate::operations::note_minted(&record.id());
            return record.wait_terminal().await;
        }
        crate::operations::Admission::Conflict => {
            return (
                StatusCode::CONFLICT,
                "That idempotency key was already used for a different operation. \
                 Reload and try again."
                    .to_string(),
            );
        }
    };

    crate::operations::note_minted(&record.id());

    let durable_key = key;
    let durable_record = record.clone();
    // Detached on purpose: this task owns the operation from here, and nothing
    // the client does to its connection can cancel it. `handle`'s Drop is the
    // backstop — a panic in the pipeline still terminalises the record, so the
    // waiter below can never hang.
    //
    // **No `.await` may sit between `admit()` above and this `tokio::spawn`
    // call.** `handle` lives in this function's own frame until the moment it
    // is moved into the spawned task below; an await point in between would
    // give a client disconnect (which aborts *this* task, per axum) a window
    // to drop `handle` — via its own Drop impl — before the pipeline ever
    // started, which fails the operation the same way a lost connection used
    // to before M1.08 existed. The durable-journal write for admission
    // therefore happens *inside* the detached task, not before it's spawned,
    // even though that means the `Accepted` row lands a beat later than the
    // in-memory state does.
    tokio::spawn(crate::operations::with_progress(
        record.clone(),
        async move {
            crate::durable::persist(durable_key.clone(), durable_record.status()).await;

            let (status, message) = match source {
                PlanSource::Build(op) => plan_and_execute_in(&repo, repo_id, tokens, op).await,
                PlanSource::Submit(plan) => submit_plan(&repo, repo_id, tokens, *plan).await,
            };
            // The generation *after* execution: the datum a reconnecting client
            // uses to decide whether its cached graph is stale, without re-reading
            // the repository. Best-effort, like every other observation here.
            let generation = Some(generation_token(&repo, &observe_live(&repo).await).await);

            // M1.09: the terminal record and its recovery ref, persisted
            // *before* `finish` publishes the same snapshot in-memory —
            // deliberately reordered from the original "finish, then persist"
            // (issue #158). `finish` unblocks every `wait_terminal` waiter,
            // including this request's own response; a waiter that resumes
            // before the durable write landed could call
            // `crate::durable::recover()` and find this row still
            // non-terminal, which `recover()` cannot distinguish from a
            // crashed process and force-fails — marking a genuinely
            // successful operation `Failed` in the journal. Computing the
            // terminal value via `terminal_status` (which only reads, never
            // publishes) and persisting it first closes that window: nothing
            // can observe "done" before the durable write is real. See
            // `OperationHandle::terminal_status`'s doc comment for the full
            // account.
            let terminal = handle.terminal_status(status, &message, generation.clone());
            crate::durable::persist(durable_key, terminal.clone()).await;

            // No recovery-ref write here, and that absence is load-bearing.
            // The pin (`refs/git-vista/recovery/<id>`) is what keeps a deleted
            // annotated tag's now-dangling tag object — and the commit under
            // it — reachable against `git gc`. Written *here* it would land
            // after `execute` already ran `git tag -d` **and** after
            // `plan_and_execute_in` dropped the per-repository mutation guard,
            // so any other operation on this repository could run (and fire
            // `gc --auto`) in the gap and prune the only copy of the object the
            // pin was supposed to save. It is now written inside the guarded
            // region, immediately before `execute` — see `pin_recovery`.
            handle.finish(status, message, generation);
        },
    ));

    record.wait_terminal().await
}

/// The guarded pipeline, with the selection injected rather than read from the
/// process-global state — which is what lets the coordination and contract
/// suites drive the real entry point against a throwaway repository.
///
/// **The plan is built before the guard is taken, and deliberately so.** The
/// obvious arrangement — guard first, then observe — serializes correctly but
/// silently defeats the point: a double-clicked Commit would queue, wait, then
/// observe the *new* state, build a perfectly fresh plan and commit a second
/// time. Both commits are individually valid; nothing is ever stale; the user
/// gets two commits. Building first means the second request carries the
/// pre-mutation generation into the guard, where [`enforce_fresh`] sees the
/// drift and refuses it (#145's gate doing the deciding, #60's guard doing the
/// serializing).
///
/// What the guard must cover is `validate → enforce_fresh → execute`: those
/// three are atomic, so the TOCTOU window between "the repository still matches
/// this plan" and "mutate it" cannot be entered by another app write. Drift
/// that happens *before* the guard is not a race — it is exactly the staleness
/// the gate exists to catch.
///
/// Consequence, accepted: two genuinely different concurrent operations also
/// end with one refusal, since the loser's generation moved too. At one user
/// that is a retry, and the alternative is duplicate mutations.
pub(crate) async fn plan_and_execute_in(
    repo: &Path,
    repo_id: Option<RepositoryId>,
    tokens: (RepositoryToken, WorktreeToken),
    op: GitOperation,
) -> (StatusCode, String) {
    // The stage reports are no-ops unless this pipeline is running under a
    // tracked operation (M1.08), so the seam the test suites drive is
    // unchanged. `Waiting` is reported *before* the guard on purpose: it is the
    // one stage a user can sit in for a long time, and "waiting for another
    // operation on this repository" is the only honest thing to show them.
    crate::operations::stage(OperationStage::Planning);
    let (plan, observed) = build_plan(repo, op, tokens).await;
    crate::operations::note_recovery(&plan.recovery);

    crate::operations::stage(OperationStage::Waiting);
    let _guard = crate::coordinator::lock(repo_id).await;
    #[cfg(test)]
    notify_lock_acquired();

    // Outside git holds the index: refuse now, in words the browser can show,
    // rather than letting git fail opaquely part-way through (#60).
    if let Some(refused) = crate::coordinator::refuse_if_git_busy(repo).await {
        return refused;
    }

    crate::operations::stage(OperationStage::Checking);
    if let Err(refused) = validate(&plan) {
        return refused;
    }
    // #145: a plan may only mutate the repository it still describes. Recheck
    // the generation and every build-time-verified precondition against the
    // live repository immediately before execution — the TOCTOU gap between
    // observation and mutation fails closed instead of proceeding stale.
    if let Err(refused) = enforce_fresh(repo, &plan, &observed).await {
        return refused;
    }
    // Still inside the guard, and after the gates: pin the restore point before
    // the command that destroys it runs. See [`pin_recovery`].
    pin_recovery(repo, &plan.recovery).await;
    crate::operations::stage(OperationStage::Executing);
    execute(repo, plan, observed).await
    // `_guard` drops here: the next queued mutation of this repository proceeds.
}

/// Write the plan's recovery pin — `refs/git-vista/recovery/<operation id>` at
/// the oid [`RecoveryStrategy`] names — **inside the mutation guard, before
/// [`execute`] runs**.
///
/// # Why the ordering is the whole point
///
/// For [`GitOperation::DeleteLocalTag`] the pin is not a convenience: it is the
/// only thing keeping the deleted annotated tag's object reachable. `git tag -d`
/// removes the sole ref to that object, and the object is in turn the sole ref
/// to the commit it tagged when no branch reaches it. Unreachable objects are
/// what `git gc` exists to remove, and nothing in this server disables
/// `gc.auto`, so *any* concurrent git invocation against the repository can
/// prune both — permanently, with the plan's `recovery` field then naming an
/// oid that no longer exists, which is a recovery record that cannot recover.
///
/// Written after `execute`, the pin would necessarily be written after that
/// window opened. Written after `plan_and_execute_in` returned — where it used
/// to live, in the tracked wrapper — it would also be after `_guard` dropped,
/// so the next queued mutation of this repository was free to run inside the
/// gap. Here, the ref exists before the ref that needs it disappears and the
/// guard is still held throughout, so the window is closed by construction
/// rather than by being narrow.
///
/// Ordering *within* the guard matters too, and this sits where it does on
/// purpose: after `validate`/`enforce_fresh`, so a refused plan leaves no ref
/// behind, and before `execute`, which is the only thing here that can destroy
/// what is being pinned.
///
/// Best-effort in the same sense as before — a failure to write it is logged,
/// never turned into a refusal — because the alternative is refusing an
/// operation the user asked for over a durability bonus. What changed is *when*
/// the bonus is claimed, not how failure is handled.
///
/// A no-op when [`crate::operations::current_operation_id`] is `None` (the
/// contract and coordination suites, which drive the pipeline untracked): there
/// is no operation id to name the ref after. The lifecycle suite, which drives
/// the real tracked wrapper, is where this is proved.
async fn pin_recovery(repo: &Path, recovery: &RecoveryStrategy) {
    let Some(id) = crate::operations::current_operation_id() else {
        return;
    };
    crate::durable::write_recovery_ref(repo, &id, recovery).await;
}

/// The build stage alone (M2.23c, #247): observe the live repository and
/// return the reviewable [`Plan`] — nothing else. Touches neither the
/// per-worktree mutation guard nor [`execute`]; the repository is byte-for-byte
/// unchanged afterwards (the contract suite proves both, the first by holding
/// the guard elsewhere for the whole call).
///
/// This is the half of [`plan_and_execute_in`] a client-review roundtrip
/// (#248's MCP plan tool) calls to *see* a plan without committing to it. The
/// plan it returns carries the observed generation and the operation's hash,
/// so [`submit_plan`] can later refuse it if the repository has moved on —
/// that refusal, not any lock, is what makes handing a plan across a review
/// roundtrip safe. Deliberately guard-free for the same reason the composed
/// path builds before locking: building only *reads*, and a concurrent review
/// must not serialize behind (or block) a running mutation.
#[cfg_attr(not(test), allow(dead_code))] // routed by #248; contract-suite-only until then
pub(crate) async fn build_plan_only(
    repo: &Path,
    op: GitOperation,
    tokens: (RepositoryToken, WorktreeToken),
) -> Plan {
    build_plan(repo, op, tokens).await.0
}

/// The submit stage (M2.23c, #247): everything [`plan_and_execute_in`] does
/// from the guard on, for a [`Plan`] that arrives from outside instead of from
/// a `build_plan` call three lines up — take the same per-worktree guard, then
/// `validate → enforce_fresh → execute`, same stage functions, same refusal
/// texts. A new-variant drift between the two compositions is pinned off by
/// the contract suite's ordered-needle test for each.
///
/// Two things differ from the composed path, both forced by the plan being
/// the only thing the submitter holds:
///
/// - **The selection is re-checked.** `tokens` is the submitting request's
///   live selection; a plan built for a different repository or worktree is
///   refused before anything is observed. The generation token cannot carry
///   this check: it digests HEAD/refs/status only, so two clones of the same
///   repository at the same commit share a generation, and a plan built
///   against one would pass `enforce_fresh` against the other.
/// - **The observation is re-derived.** `plan_and_execute_in` hands `execute`
///   the reads it took while building (journal before-oids, delete's restore
///   point, the CAS tip). A submitted plan carries no observation, so the same
///   reads are taken again through [`observe_for_submission`] — with the same
///   eyes `build_plan` used, [`observe_operation`], so the per-operation
///   `branch_tip` (a delete's recovery point) is never silently dropped.
///   Re-observing is safe *because* `enforce_fresh` anchors on the plan's
///   build-time generation: any drift between build and the guard refuses
///   execution, so whenever `execute` runs, the re-derived observation
///   describes the same repository state the plan was built against.
///   `held_at_build` is re-derived too, which reads a precondition that
///   silently broke during the review window (possible only for the
///   generation-invisible pair, `RemoteConfigured`/`SeedRecorded`) as
///   built-stale — from the submitter's seat the two cases are genuinely
///   indistinguishable, and both fail closed. *Which* refusal they fail
///   closed with now depends on [`refuses_when_unmet_at_build`]:
///   `SeedRecorded` still flows to the executor's own legacy refusal (the
///   reset's 404), while `RemoteConfigured` is refused by `enforce_fresh`
///   itself, because nothing downstream of it refuses at all (ADR 0047).
///   Not just prose: the contract suite's two
///   `review_window_*_drift_fails_closed_*` tests prove the refusal (and its
///   byte-identity with the never-held case) for both generation-invisible
///   preconditions, and
///   `a_generation_invisible_break_while_queued_is_refused_by_the_gates_live_recheck`
///   proves the re-derivation itself is load-bearing (emptying it passed the
///   whole suite before that test existed).
pub(crate) async fn submit_plan(
    repo: &Path,
    repo_id: Option<RepositoryId>,
    tokens: (RepositoryToken, WorktreeToken),
    plan: Plan,
) -> (StatusCode, String) {
    let (repository, worktree) = tokens;
    if plan.repository != repository || plan.worktree != worktree {
        return (
            StatusCode::CONFLICT,
            "This plan was built for a different repository or worktree — \
             rebuild it against the current selection."
                .to_string(),
        );
    }
    // Observed *before* the guard, mirroring the composed path's deliberate
    // build-before-lock ordering (see `plan_and_execute_in`): observation only
    // reads, and any drift between this read and execution is refused by
    // `enforce_fresh` against the plan's build-time generation.
    let observed = observe_for_submission(repo, &plan).await;
    crate::operations::note_recovery(&plan.recovery);

    crate::operations::stage(OperationStage::Waiting);
    let _guard = crate::coordinator::lock(repo_id).await;

    if let Some(refused) = crate::coordinator::refuse_if_git_busy(repo).await {
        return refused;
    }

    crate::operations::stage(OperationStage::Checking);
    if let Err(refused) = validate(&plan) {
        return refused;
    }
    if let Err(refused) = enforce_fresh(repo, &plan, &observed).await {
        return refused;
    }
    pin_recovery(repo, &plan.recovery).await;
    crate::operations::stage(OperationStage::Executing);
    execute(repo, plan, observed).await
    // `_guard` drops here, exactly as in `plan_and_execute_in`.
}

/// The submit path's own outer entry (M2.23e, #249): the same gate-then-
/// delegate shape [`plan_and_execute`] has, for a [`Plan`] that arrives from
/// outside rather than a bare [`GitOperation`] built fresh. Routed by
/// `POST /api/execute-plan` (`handlers::plan::execute_plan`).
///
/// Reaches the identical [`plan_and_execute_tracked`] lifecycle layer via
/// [`PlanSource::Submit`] — the same admission, detached spawn and terminal
/// wait every other write gets, so the submit path cannot drift from the
/// composed path on idempotency (ADR 0016's funnel, extended to plans
/// instead of bare operations). What `plan_and_execute_tracked` does with
/// the `PlanSource` once admitted is call [`submit_plan`] — never a second
/// copy of validate/enforce_fresh/execute.
///
/// One admission subtlety: `admit` keys the idempotency registry on this
/// *request's* live selection tokens (via `selection_tokens()` below), not
/// the plan's own `repository`/`worktree` fields — those are compared inside
/// `submit_plan` itself, after admission. So a retried cross-worktree
/// submission under the same key replays the original 409 rather than
/// re-deriving it; that is the correct idempotent behavior, not a gap.
pub(crate) async fn submit_plan_tracked(plan: Plan) -> (StatusCode, String) {
    // The write gate, exactly as `plan_and_execute` takes it — a plan is an
    // approval token for a mutation, so executing it is refused the same way
    // building one already is (see `handlers::plan`'s module doc).
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let Some(key) = crate::operations::current_key() else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "This request needs the {IDEMPOTENCY_HEADER} header, so a retry \
                 can be recognised as a retry. Reload the app to update."
            ),
        );
    };
    // D2 (#66, Task 7): the same validated resolution every write handler
    // uses. `submit_plan` re-checks this request's tokens against the plan's
    // own below; this call is what produces them.
    let (repo, entry) = match crate::state::resolve_target() {
        Ok(v) => v,
        Err(rejected) => return rejected,
    };
    let repo_id = Some(entry.handle.repository);
    plan_and_execute_tracked(
        key,
        repo,
        repo_id,
        selection_tokens(),
        PlanSource::Submit(Box::new(plan)),
        // A submitted plan runs through the review-roundtrip seam (#249),
        // never the Recovery Center — it recovers nothing.
        None,
    )
    .await
}

/// Resolve arbitrary request input to an exact [`CommitOid`]. A full 40/64
/// lowercase-hex id — what the UI always sends — is taken as-is; anything else
/// (a hand-crafted symbolic or abbreviated start point) is resolved through
/// `git rev-parse`. Failure mirrors git's own "not a valid object name" text,
/// since git would previously have been the one to refuse it.
pub(crate) async fn resolve_commit_oid(
    repo: &Path,
    given: &str,
) -> Result<CommitOid, (StatusCode, String)> {
    if let Ok(oid) = CommitOid::new(given) {
        return Ok(oid);
    }
    match rev_parse(repo, given).await {
        Ok(Some(full)) => CommitOid::new(full).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("git rev-parse returned an unusable id: {e}"),
            )
        }),
        // git ran and refused the name: the request is wrong. 400, git's words.
        Ok(None) => Err((
            StatusCode::BAD_REQUEST,
            format!("fatal: not a valid object name: '{given}'"),
        )),
        // D5: git never ran, so nothing was refused. Telling the user their
        // object name is invalid would be a claim we have no evidence for —
        // and it would send them to fix a request that is probably fine.
        Err(e) => Err(couldnt_run(
            "resolve_commit_oid",
            &format!("couldn't resolve ‘{given}’: {e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// One observation of the live repository that can fail to *be* an
/// observation — D5 (#66, Task 19).
///
/// The planner used to hold every read as `Option<String>`, where `None` meant
/// two incompatible things at once: "git ran and there is nothing there"
/// (unborn HEAD, no such branch, no remote-tracking ref) and "git could not be
/// run, so nothing was read". Every consumer picked the first reading, which
/// is how a failed read became a fact about the repository — journaled as an
/// absence, hashed into the freshness token as an empty string, and compared
/// against another failed read as "nothing moved".
///
/// [`Absent`](Self::Absent) is the fact. [`Unknown`](Self::Unknown) is the
/// absence of a fact, and it is deliberately awkward to consume: there is no
/// `unwrap_or_default`, no `PartialEq`, and no `Option` conversion that
/// silently flattens it.
///
/// # No `PartialEq`
///
/// Not an oversight — deriving it is the exact bug this type exists to
/// prevent. `Unknown == Unknown` would be `true`, and the two places that
/// compare a before-tip to an after-tip (`exec_merge`, `exec_rebase`) would
/// then answer "HEAD did not move — already up to date" for a pair of reads
/// that observed *nothing*. Comparison goes through
/// [`same_observation`](Self::same_observation), which answers `false` unless
/// something was actually observed on both sides.
#[derive(Clone, Debug)]
enum Obs<T> {
    /// git ran and reported this value.
    Known(T),
    /// git ran and reported that there is nothing here.
    Absent,
    /// git could not be run. Nothing was observed; nothing may be concluded.
    Unknown,
}

/// Distinguishes one `Unknown` from the next in the generation digest. A plain
/// counter is enough: generation tokens are only ever compared for equality,
/// and only ever within one process's lifetime.
static UNKNOWN_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl<T> Obs<T> {
    /// Lift a three-state git read into an observation. `Err` is the one thing
    /// this type exists for; `Ok(None)` is a real, reportable absence.
    fn from_read(read: Result<Option<T>, ExecUnavailable>) -> Self {
        match read {
            Ok(Some(v)) => Obs::Known(v),
            Ok(None) => Obs::Absent,
            Err(_) => Obs::Unknown,
        }
    }

    /// The observed value, if anything was observed and it was there.
    ///
    /// Collapses `Absent` and `Unknown` together, so it is only correct where
    /// the caller's *next* step is "if we have a value, describe it; otherwise
    /// describe nothing" — i.e. where an omission is not itself an assertion.
    /// Anywhere a decision is made, match on the variants instead.
    fn known(&self) -> Option<&T> {
        match self {
            Obs::Known(v) => Some(v),
            Obs::Absent | Obs::Unknown => None,
        }
    }

    /// Whether git could not be run for this observation.
    fn is_unknown(&self) -> bool {
        matches!(self, Obs::Unknown)
    }
}

impl<T: PartialEq> Obs<T> {
    /// Whether these two observations say the same thing about the repository.
    ///
    /// **Two `Unknown`s are never "the same".** Nothing was observed on either
    /// side, so there is no basis for concluding the repository did not move —
    /// which is precisely the conclusion `exec_merge` and `exec_rebase` draw
    /// from a `true` here ("Already up to date").
    fn same_observation(&self, other: &Self) -> bool {
        match (self, other) {
            (Obs::Known(a), Obs::Known(b)) => a == b,
            (Obs::Absent, Obs::Absent) => true,
            _ => false,
        }
    }
}

impl<T: std::fmt::Display> Obs<T> {
    /// This observation's contribution to the generation digest (#145).
    ///
    /// Each variant carries its own tag, so an observed empty string can never
    /// collide with an absence. `Unknown` goes further and carries a nonce: an
    /// unknown observation must not merely hash *differently* from an absence,
    /// it must hash differently **every time**. Without that, a repository git
    /// cannot be run against produces the same token at plan-build and at
    /// enforce-fresh, the two compare equal, and the staleness gate certifies
    /// as unchanged a repository nobody ever looked at.
    fn digest_field(&self) -> String {
        match self {
            Obs::Known(v) => format!("known:{v}"),
            Obs::Absent => "absent".to_string(),
            Obs::Unknown => format!(
                "unknown:{}",
                UNKNOWN_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        }
    }
}

/// Pre-execution observations of the live repository, captured while building
/// the plan and reused by the executor — exactly the values the old handlers
/// read before mutating (journal "before" oids, the compare-and-swap tip), so
/// nothing is read twice and nothing is read *after* the mutation that needs
/// the before-state.
struct Observed {
    /// The checked-out branch's short name (`read_head_branch`), if any.
    head_branch: Option<String>,
    /// What `HEAD` resolves to (unborn HEAD ⇒ [`Obs::Absent`]; git unavailable
    /// ⇒ [`Obs::Unknown`]).
    head_tip: Obs<String>,
    /// The tip of the branch — or, for [`GitOperation::DeleteLocalTag`], the
    /// **unpeeled** value of the tag ref — the operation names, for the
    /// operations that need it before executing (delete's journaled restore
    /// point, reset's CAS). Unpeeled matters for the tag case: an annotated
    /// tag's ref value is its tag *object*, and that oid is what
    /// [`RecoveryStrategy::RecreateTag`] must carry for the recovery to
    /// restore the original tag rather than a look-alike (see that variant's
    /// doc in `plan.rs`).
    branch_tip: Obs<String>,
    /// `git status --porcelain=v2` at observation time — a generation input
    /// (#145) so uncommitted-work changes count as the repository moving, and
    /// the live check behind [`Precondition::CleanWorktree`].
    status: Obs<String>,
    /// Which of the plan's preconditions actually *held* when it was built,
    /// index-aligned with `Plan::preconditions`. [`enforce_fresh`] re-verifies
    /// exactly these before executing: one that failed at build time flows on
    /// to the executor's own legacy guard (same refusal text as ever), while
    /// one that held and then broke is a race — refused, fail-closed (#145).
    held_at_build: Vec<bool>,
}

/// Build the reviewable [`Plan`] for `operation` against the live repository.
///
/// Observation here is **best-effort by design**: a read that fails (unborn
/// HEAD, a branch that doesn't exist, no remote-tracking ref) simply thins the
/// plan's preconditions/ref-changes rather than refusing the operation —
/// execution then surfaces git's own error exactly as it always has. #145 is
/// where preconditions become load-bearing checks.
async fn build_plan(
    repo: &Path,
    operation: GitOperation,
    tokens: (RepositoryToken, WorktreeToken),
) -> (Plan, Observed) {
    let mut observed = observe_operation(repo, &operation).await;

    let (risk, preconditions, expected_ref_changes, recovery) =
        shape(repo, &operation, &observed).await;

    // Record which preconditions hold right now (#145): enforce_fresh only
    // re-verifies these, so a precondition that was already unmet reaches the
    // executor's legacy guard unchanged.
    observed.held_at_build = held_now(repo, &preconditions, &observed).await;

    let (repository, worktree) = tokens;
    let operation_hash = operation_hash(&operation);
    let generation = generation_token(repo, &observed).await;
    let now = crate::activity::now_secs();

    // Computed before the struct so the borrow of `operation` ends here; the
    // struct takes it by value on the next line.
    let advisories = advisories_for(repo, &operation).await;

    let plan = Plan {
        repository,
        worktree,
        generation,
        operation,
        operation_hash,
        issued_at: UnixSeconds(now),
        expires_at: UnixSeconds(now + PLAN_TTL_SECS),
        risk,
        preconditions,
        expected_ref_changes,
        recovery,
        advisories,
    };
    (plan, observed)
}

/// One pre-execution observation of the live repository, shaped for
/// `operation` — the reads `build_plan` has always taken, factored out
/// (M2.23c, #247) so [`observe_for_submission`] re-observes with **the same
/// eyes** rather than a copy that could drift. Order and content are the
/// build path's exactly: HEAD's branch, HEAD's tip, the per-operation
/// `branch_tip`, then status.
async fn observe_operation(repo: &Path, operation: &GitOperation) -> Observed {
    let head_branch = read_head_branch_blocking(repo).await;
    let head_tip = Obs::from_read(rev_parse(repo, "HEAD").await);
    let branch_tip = match operation {
        GitOperation::DeleteBranch { branch }
        | GitOperation::ForceDeleteBranch { branch }
        | GitOperation::ResetBranch { branch, .. } => {
            Obs::from_read(rev_parse(repo, branch.as_str()).await)
        }
        // M2.21a (#235): the tag delete's restore point. Fully qualified so a
        // same-named branch can never win the ambiguity, and read through
        // [`rev_parse_ref_unpeeled`] rather than `rev_parse` — that helper
        // peels (`^{commit}`), which is right for every commit-shaped caller
        // and exactly wrong here: for an annotated tag the restore point is
        // the tag *object's* oid — the value `git tag -d` prints as `(was
        // <oid>)` and the one oid from which the tag can be restored
        // byte-identically. (Caught by the contract suite's paired negative
        // `delete_local_tag_recovery_carries_the_unpeeled_tag_object`, which
        // failed against the peeling helper before this arm switched.)
        GitOperation::DeleteLocalTag { name } => {
            Obs::from_read(rev_parse_ref_unpeeled(repo, &format!("refs/tags/{name}")).await)
        }
        // No branch named by this operation: not an unreadable observation,
        // just one that was never taken.
        _ => Obs::Absent,
    };
    Observed {
        head_branch,
        head_tip,
        branch_tip,
        status: worktree_status(repo).await,
        held_at_build: Vec::new(),
    }
}

/// Which of `preconditions` hold against `observed` right now, index-aligned —
/// the build-time census `enforce_fresh` gates its re-verification on. Shared
/// verbatim by `build_plan` and [`observe_for_submission`] (M2.23c, #247).
async fn held_now(repo: &Path, preconditions: &[Precondition], observed: &Observed) -> Vec<bool> {
    let mut held = Vec::with_capacity(preconditions.len());
    for precondition in preconditions {
        held.push(
            verify_precondition(repo, precondition, observed)
                .await
                .is_ok(),
        );
    }
    held
}

/// Re-derive, for a plan submitted from outside the request that built it
/// (M2.23c, #247), the [`Observed`] that `plan_and_execute_in` would have
/// carried from its own `build_plan` call: the same per-operation reads via
/// [`observe_operation`], and `held_at_build` re-derived against the plan's
/// own precondition list via [`held_now`]. See [`submit_plan`]'s doc for why
/// re-observation is safe (the plan's build-time generation, not this read,
/// is what `enforce_fresh` anchors staleness on) and for the one semantic
/// wrinkle (`RemoteConfigured`/`SeedRecorded` drift reads as built-stale).
async fn observe_for_submission(repo: &Path, plan: &Plan) -> Observed {
    let mut observed = observe_operation(repo, &plan.operation).await;
    observed.held_at_build = held_now(repo, &plan.preconditions, &observed).await;
    observed
}

/// The current selection's opaque id tokens. In degraded mode (the served path
/// wouldn't classify as a repository, so it has no catalog entry) a fixed
/// placeholder keeps the plan well-formed; execution then fails with git's own
/// error exactly as the un-migrated handlers did.
///
/// `pub(crate)` since M2.23d (#248) so `handlers::plan` mints a plan's tokens
/// through the *same* function [`plan_and_execute`] does. A second, parallel
/// derivation would be the one way `/api/plan` could hand back a plan whose
/// tokens `submit_plan` (#249) then refuses as "built for a different
/// repository or worktree" — a bug visible only across two slices.
pub(crate) fn selection_tokens() -> (RepositoryToken, WorktreeToken) {
    match current_handle() {
        Some(handle) => (
            RepositoryToken::new(handle.repository.to_string())
                .expect("a RepositoryId displays as a non-empty uuid"),
            WorktreeToken::new(handle.worktree.to_string())
                .expect("a WorktreeId displays as a non-empty uuid"),
        ),
        None => (
            RepositoryToken::new("unregistered").expect("literal is non-empty"),
            WorktreeToken::new("unregistered").expect("literal is non-empty"),
        ),
    }
}

/// The live repository generation as the plan's opaque token (ADR 0001).
/// Computed from HEAD, every ref, and the worktree/index status (#145) — any
/// of them moving means the repository the plan described no longer exists.
/// The token is opaque and compared only for equality, so deepening its
/// inputs further later is not a wire change.
/// Async since #60: the ref read below is synchronous filesystem work and now
/// runs on a blocking thread instead of an async worker.
async fn generation_token(repo: &Path, observed: &Observed) -> GenerationToken {
    let mut inputs = GenerationInputs::new();
    // D5: each observation contributes a *tagged* field, so "git said the ref
    // is not there" and "git could not be asked" are different digests — and
    // the latter is different every time. See `Obs::digest_field`.
    inputs.field(
        "head",
        format!(
            "{}\u{0}{}",
            observed.head_branch.as_deref().unwrap_or(""),
            observed.head_tip.digest_field()
        ),
    );
    for (name, target) in refs_digest_input(repo).await {
        inputs.field(name, target);
    }
    // refs/stash, explicitly (M3.24, #77).
    //
    // `refs_digest_input` above cannot supply it: it is built on
    // `git_vista_git::read_refs`, which says in its own comment that it keeps
    // "only branches and tags". That filter is CORRECT for what read_refs is
    // for — the badges drawn on commits, where a stash entry has no business
    // appearing — and wrong for a staleness digest, which needs everything
    // that can make an approved plan untrue.
    //
    // Without this, no stash write moves the generation. A plan approved
    // before a drop would still pass `enforce_fresh`, while every stash
    // selector in it addressed a different entry, because dropping renumbers
    // the list. Caught by a test written for #77's "generation updates are
    // correct" criterion, not by inspection.
    inputs.field("stash", stash_digest_input(repo).await);
    inputs.field("status", observed.status.digest_field());
    GenerationToken::new(inputs.generation().to_string())
        .expect("a RepositoryGeneration displays as non-empty decimal")
}

// --- blocking-work offload (#60, acceptance 4) ------------------------------
//
// Every git invocation on this path already goes through
// `tokio::process::Command`, which never occupies a worker thread. These three
// helpers cover what was left: synchronous filesystem reads and the JSONL
// journal append, which on an async worker block that worker for their whole
// duration — long enough to matter on a cold cache or a slow disk.
//
// Scope is deliberately **the planner path only**. The read handlers were not
// swept as part of #60; see ADR 0019 so a later session doesn't read this as
// "done everywhere".

/// [`git_vista_git::read_head_branch`] off the async workers.
async fn read_head_branch_blocking(repo: &Path) -> Option<String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || git_vista_git::read_head_branch(&repo))
        .await
        .ok()
        .flatten()
}

/// `refs/stash`'s target, or a tagged absence, for the generation digest.
///
/// Deliberately its own read rather than a widening of
/// [`git_vista_git::read_refs`]: that function's branches-and-tags filter is
/// right for the badge list it exists to build, and loosening it would put
/// stash entries on commits in the UI.
///
/// The three outcomes stay apart, same discipline as `Obs` everywhere else:
/// a resolved oid, "there is no stash", and "the read failed". The last is
/// deliberately UNIQUE per call, so a repository whose stash cannot be read
/// invalidates every plan rather than silently digesting as "no stash" — the
/// failure mode being a stale plan surviving a change nobody could see.
async fn stash_digest_input(repo: &Path) -> String {
    match rev_parse_ref_unpeeled(repo, "refs/stash").await {
        Ok(Some(oid)) => format!("at\u{0}{oid}"),
        Ok(None) => "absent".to_string(),
        Err(_) => format!("unreadable\u{0}{}", crate::activity::now_secs()),
    }
}

/// Every ref as `(digest field name, target oid)`, read off the async workers.
/// Shaped for [`generation_token`]'s digest so the blocking read happens once,
/// in one place, rather than a `Refs` value being held across an await.
async fn refs_digest_input(repo: &Path) -> Vec<(String, String)> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || match git_vista_git::read_refs(&repo) {
        Ok(refs) => refs
            .iter()
            .map(|r| (format!("ref:{:?}:{}", r.kind, r.name), r.target.0.clone()))
            .collect(),
        Err(_) => Vec::new(),
    })
    .await
    .unwrap_or_default()
}

/// Record one successful app operation in the journal, off the async workers.
///
/// Shadows [`crate::handlers::journal_app_event`] inside this module on
/// purpose: every executor calls it by the same name it always did, and gets
/// the blocking-thread version. Best-effort exactly as before — the git
/// operation has already succeeded, so a failed join is dropped rather than
/// turned into a failed response.
///
/// # `Obs`, not `Option`, for the two oids (D5, #66 Task 19)
///
/// The tips recorded here are read *after* the mutation succeeded, so an
/// unreadable one is entirely possible (the sandbox policy can stop being
/// buildable between two commands) and there is no undo path for it — the git
/// work is already done. Taking `Obs` makes each executor state which of the
/// three cases it is handing over instead of flattening two of them into
/// `None`.
///
/// **The stored `ActivityEvent` is unchanged**, and deliberately so: its
/// `old_oid`/`new_oid` are `Option<String>` in `git_vista_core`, that schema is
/// shared with the on-disk JSONL every existing journal file is written in, and
/// widening it is not this task's to do. So an `Unknown` still stores `None` —
/// but it is never *silently* `None`: the summary carries an explicit note, so
/// a reader of the feed can tell "there was no such tip" from "we could not
/// read the tip", which is the distinction that was previously lost. The
/// mechanical consequence — no restore point, no undo offered — is the
/// fail-safe direction.
async fn journal_app_event(
    repo: &Path,
    kind: ActivityKind,
    ref_name: Option<String>,
    old_oid: Obs<String>,
    new_oid: Obs<String>,
    summary: String,
) {
    let summary = match (old_oid.is_unknown(), new_oid.is_unknown()) {
        (false, false) => summary,
        (true, false) => format!("{summary} (previous tip unknown — git could not be read)"),
        (false, true) => format!("{summary} (resulting tip unknown — git could not be read)"),
        (true, true) => format!("{summary} (tips unknown — git could not be read)"),
    };
    let old_oid = old_oid.known().cloned();
    let new_oid = new_oid.known().cloned();
    let repo = repo.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        crate::handlers::journal_app_event(&repo, kind, ref_name, old_oid, new_oid, summary)
    })
    .await;
}

/// [`journal::remove_from_snapshot`] off the async workers.
async fn remove_from_snapshot_blocking(repo: &Path, branch: &str) {
    let repo = repo.to_path_buf();
    let branch = branch.to_string();
    let _ =
        tokio::task::spawn_blocking(move || journal::remove_from_snapshot(&repo, &branch)).await;
}

/// [`journal::clear`] off the async workers.
async fn journal_clear_blocking(repo: &Path) {
    let repo = repo.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || journal::clear(&repo)).await;
}

/// `git status --porcelain=v2` at this instant.
///
/// D5 keeps the two failure modes apart: [`Obs::Absent`] is "git ran and
/// refused — the path isn't a working tree", [`Obs::Unknown`] is "git could
/// not be run at all". The old `Option` collapsed both to `None`, which the
/// generation digest then flattened to the empty string — i.e. to the exact
/// value a *clean* worktree produces.
async fn worktree_status(repo: &Path) -> Obs<String> {
    // An observation, not part of any operation: `status` reads the index and
    // the working tree, so it declares `Local` on its own behalf (D3).
    let out = match run_git(repo, NetworkNeed::Local, &["status", "--porcelain=v2"]).await {
        Ok(out) => out,
        Err(_) => return Obs::Unknown,
    };
    if out.status.success() {
        Obs::Known(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Obs::Absent
    }
}

/// Fresh observations for the execution-time check (#145): same reads as
/// plan-building, minus the per-operation `branch_tip`.
async fn observe_live(repo: &Path) -> Observed {
    Observed {
        head_branch: read_head_branch_blocking(repo).await,
        head_tip: Obs::from_read(rev_parse(repo, "HEAD").await),
        branch_tip: Obs::Absent,
        status: worktree_status(repo).await,
        held_at_build: Vec::new(),
    }
}

/// The execution-time staleness gate (#145). Refuses — fail-closed, 409 —
/// when the live repository no longer matches the plan:
///
///  1. **Generation**: recomputed from live HEAD/refs/status; any drift since
///     the plan was built refuses execution.
///  2. **Preconditions**: every precondition that *held* at build time is
///     re-verified against the live repository. One that already failed at
///     build time is skipped here — the executor's own legacy guard refuses
///     it with the exact wording it always had — **unless**
///     [`refuses_when_unmet_at_build`] says there is no such guard, in which
///     case this gate refuses it itself. See that function for why the
///     "skip it, the executor will catch it" rule needed an exception.
async fn enforce_fresh(
    repo: &Path,
    plan: &Plan,
    observed: &Observed,
) -> Result<(), (StatusCode, String)> {
    let live = observe_live(repo).await;
    // D5: an observation that never happened cannot certify freshness. The
    // generation digest already fails closed here — `Obs::Unknown` carries a
    // nonce, so an unknown on either side makes the tokens differ — but that
    // refusal would say "the repository changed", which is a claim about the
    // repository we are in no position to make. Say what actually happened.
    let unknown = observed.head_tip.is_unknown()
        || observed.branch_tip.is_unknown()
        || observed.status.is_unknown()
        || live.head_tip.is_unknown()
        || live.status.is_unknown();
    if unknown {
        return Err(couldnt_run(
            "staleness gate",
            &"couldn't read the repository's state, so this plan cannot be \
              re-verified before executing",
        ));
    }
    if generation_token(repo, &live).await.as_str() != plan.generation.as_str() {
        return Err((
            StatusCode::CONFLICT,
            "The repository changed while this plan was pending — refresh and try again."
                .to_string(),
        ));
    }
    for (i, precondition) in plan.preconditions.iter().enumerate() {
        if observed.held_at_build.get(i).copied().unwrap_or(false) {
            verify_precondition(repo, precondition, &live).await?;
        } else if refuses_when_unmet_at_build(precondition) {
            return Err(unmet_at_build(precondition));
        }
    }
    Ok(())
}

/// Whether a precondition that **already failed when the plan was built** must
/// be refused *here*, instead of being skipped and left to the executor.
///
/// # Why this exception exists
///
/// [`enforce_fresh`]'s skip is not laziness — it is what keeps a genuinely
/// unmet precondition producing git's own words rather than a second,
/// paraphrased refusal. It rests on an assumption that was written down but
/// never checked: *every* precondition that can fail at build time has an
/// executor-side guard that refuses when it does. Create-branch's
/// `RefAbsent` has one (`git branch` refuses an existing name), rebase's
/// `CleanWorktree` has one (git refuses a dirty tree), the reset's
/// `SeedRecorded` has one (`exec_reset_test_repo` re-reads the seed and 404s,
/// pinned by `contract_suite`'s
/// `review_window_seed_drift_fails_closed_with_the_never_recorded_refusal`).
///
/// [`Precondition::RemoteConfigured`] has none, and cannot have one, because
/// **`git fetch`/`git pull`/`git push` do not refuse an unknown remote —
/// they reinterpret it as a transport target.** Verified against git 2.43.0:
/// `git fetch ghost.git`, with no such remote, fetches from the *directory*
/// `ghost.git` and writes `FETCH_HEAD`. Before this arm existed the fetch
/// endpoint then answered `200 … already up to date`, because the
/// before/after diff of `refs/remotes/ghost.git/*` is empty for an ad-hoc
/// target: a fetch that reached somewhere it was never authorised to, reported
/// as a no-op. With a URL in the field the same path opened a real socket
/// (`remote_boundary_suite` reproduces it against a listener).
///
/// So this is the one precondition whose failure must stop the pipeline, and
/// the gate is the only place that can do it — the executor is precisely
/// where the damage happens.
///
/// No wildcard arm, on purpose: a new [`Precondition`] variant fails to
/// compile here until someone states which side it is on, and the contract
/// suite pins the `true` set to an exact census so widening it is a visible
/// edit rather than a side effect. The question each arm answers is narrow:
/// *if this precondition is false and we run the executor anyway, does the
/// executor refuse?*
pub(crate) fn refuses_when_unmet_at_build(precondition: &Precondition) -> bool {
    match precondition {
        // git resolves an unknown remote as a URL or a path instead of
        // refusing it. Nothing downstream says no. See above.
        Precondition::RemoteConfigured { .. } => true,
        // Every one of these is refused by the git command the executor runs
        // (a missing ref, an occupied ref, the wrong checked-out branch, a
        // dirty tree) or by the executor's own re-read (`SeedRecorded`), in
        // that command's own words — which is strictly better than a
        // paraphrase from here.
        Precondition::RefAt { .. }
        | Precondition::RefExists { .. }
        | Precondition::RefAbsent { .. }
        | Precondition::BranchCheckedOut { .. }
        | Precondition::BranchNotCheckedOut { .. }
        | Precondition::CleanWorktree
        | Precondition::SeedRecorded => false,
    }
}

/// The refusal [`refuses_when_unmet_at_build`] earns: a `409` naming the
/// precondition that was already false when the plan was built.
///
/// `409` and not `400`: the request is well-formed, and the same request
/// against the same repository with the remote configured would be accepted —
/// this is a statement about the repository, which is what every other
/// precondition refusal in `verify_precondition` is too.
fn unmet_at_build(precondition: &Precondition) -> (StatusCode, String) {
    let why = match precondition {
        Precondition::RemoteConfigured { remote } => format!(
            "Remote ‘{}’ is not configured in this repository — nothing was contacted. \
             Add it with `git remote add`, or pick a remote this repository knows.",
            remote.as_str()
        ),
        // Unreachable while `refuses_when_unmet_at_build` names exactly one
        // arm; kept total rather than panicking, and worded so a future arm
        // that forgets to add its own sentence still refuses honestly.
        other => format!("A precondition of this plan does not hold: {other:?}."),
    };
    eprintln!("git-vista: refusing an unmet precondition: {why}");
    (StatusCode::CONFLICT, why)
}

/// Check one [`Precondition`] against the live repository. `live` supplies the
/// already-read HEAD and status; ref lookups go to git directly. Refusals are
/// 409s that say what moved — except the one D5 adds, below.
///
/// # "git could not run" is a refusal, not a satisfied precondition
///
/// Before D5 (#66, Task 19) `rev_parse` answered `None` both for "git ran and
/// the ref does not resolve" and for "git could not be run", and the three
/// ref-shaped arms below disagreed about what that meant:
///
/// | Arm | on `None` | fail-closed? |
/// |---|---|---|
/// | `RefAt` | refuse ("disappeared") | yes, by luck |
/// | `RefExists` | refuse ("disappeared") | yes, by luck |
/// | `RefAbsent` | **`Ok(())` — the gate passes** | **no** |
///
/// The first two are fail-closed only incidentally: they test for *presence*,
/// so an unreadable ref reads as "not present" and refuses. `RefAbsent` tests
/// for absence, so the identical unreadable answer reads as "absent" and the
/// gate opens. `RefAbsent` is what stops `CreateBranch` and `RestoreBranch`
/// from writing over a ref that already exists, so on a host where git cannot
/// be launched every one of those plans passed its own guard.
///
/// A mechanical `Option` → `Result` rewrite would have preserved that
/// asymmetry exactly, mapping `Err` onto each arm's existing `None` behaviour.
/// All three now refuse on `Err`, with a 500 rather than the 409 they use for
/// a ref that genuinely moved: the repository did nothing wrong.
async fn verify_precondition(
    repo: &Path,
    precondition: &Precondition,
    live: &Observed,
) -> Result<(), (StatusCode, String)> {
    let refuse = |why: String| Err((StatusCode::CONFLICT, why));
    // D5: git failing to run is never evidence about a ref. Every ref-shaped
    // precondition below refuses on it — including `RefAbsent`, which is the
    // one that used to be *satisfied* by it (see this function's doc comment).
    let unreadable = |ref_name: &str, e: &ExecUnavailable| {
        Err(couldnt_run(
            &format!("precondition on ‘{ref_name}’"),
            &format!("couldn't check ‘{ref_name}’, so this plan cannot be verified: {e}"),
        ))
    };
    // The three ref-shaped checks resolve the ref **unpeeled** (M2.21a,
    // #235): a `RefAt` asserts what the ref itself holds, and for an
    // annotated tag ref that is a tag object — `rev_parse`'s `^{commit}`
    // peel would compare the plan's pinned tag-object oid against the peeled
    // commit and refuse every honest tag CAS as "moved". For every ref this
    // function checked before tags existed (branches, remote-tracking refs,
    // HEAD), the ref's value *is* a commit, so unpeeled and peeled are the
    // same bytes and this is not a behaviour change for them.
    match precondition {
        Precondition::RefAt { ref_name, oid } => {
            match rev_parse_ref_unpeeled(repo, ref_name.as_str()).await {
                Ok(Some(at)) if at == oid.as_str() => Ok(()),
                Ok(Some(_)) => refuse(format!(
                    "‘{}’ moved while this plan was pending — refresh and try again.",
                    ref_name.as_str()
                )),
                Ok(None) => refuse(format!(
                    "‘{}’ disappeared while this plan was pending — refresh and try again.",
                    ref_name.as_str()
                )),
                Err(e) => unreadable(ref_name.as_str(), &e),
            }
        }
        Precondition::RefExists { ref_name } => {
            match rev_parse_ref_unpeeled(repo, ref_name.as_str()).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => refuse(format!(
                    "‘{}’ disappeared while this plan was pending — refresh and try again.",
                    ref_name.as_str()
                )),
                Err(e) => unreadable(ref_name.as_str(), &e),
            }
        }
        Precondition::RefAbsent { ref_name } => {
            match rev_parse_ref_unpeeled(repo, ref_name.as_str()).await {
                // git ran and said the ref does not resolve: genuinely absent.
                Ok(None) => Ok(()),
                Ok(Some(_)) => refuse(format!(
                    "‘{}’ appeared while this plan was pending — refresh and try again.",
                    ref_name.as_str()
                )),
                // The polarity bug this arm used to have: `rev_parse` returned
                // `None` both for "not there" and for "git could not run", and
                // `is_none()` accepted *both* as proof of absence. So on a host
                // where git could not be launched at all, every `RefAbsent`
                // precondition passed — and `RefAbsent` is what guards
                // `CreateBranch` and `RestoreBranch` from clobbering a ref that
                // already exists. Its two siblings above happened to fail closed
                // on the same input purely because they test for presence.
                Err(e) => unreadable(ref_name.as_str(), &e),
            }
        }
        Precondition::BranchCheckedOut { branch } => {
            if live.head_branch.as_deref() == Some(branch.as_str()) {
                Ok(())
            } else {
                refuse(format!(
                    "‘{}’ is no longer the checked-out branch — refresh and try again.",
                    branch.as_str()
                ))
            }
        }
        Precondition::BranchNotCheckedOut { branch } => {
            if live.head_branch.as_deref() != Some(branch.as_str()) {
                Ok(())
            } else {
                refuse(format!(
                    "‘{}’ became the checked-out branch — refresh and try again.",
                    branch.as_str()
                ))
            }
        }
        Precondition::CleanWorktree => match &live.status {
            Obs::Known(s) if s.is_empty() => Ok(()),
            Obs::Known(_) => refuse(
                "The working tree picked up uncommitted changes while this plan was \
                 pending — refresh and try again."
                    .to_string(),
            ),
            // git ran and refused (not a working tree) on a plan that requires
            // a clean tree: refuse rather than guess (fail-closed). Unchanged
            // wording — this arm's meaning is unchanged too.
            Obs::Absent => refuse(
                "Couldn't verify the working tree is clean — refusing to execute.".to_string(),
            ),
            // D5: git could not be run. Same refusal *decision*, different
            // status and different words, because the cause is ours, not the
            // repository's.
            Obs::Unknown => Err(couldnt_run(
                "precondition CleanWorktree",
                &"couldn't run git status, so the working tree cannot be verified",
            )),
        },
        Precondition::RemoteConfigured { remote } => {
            if git_ok(repo, &["remote", "get-url", remote.as_str()])
                .await
                .is_ok()
            {
                Ok(())
            } else {
                refuse(format!(
                    "Remote ‘{}’ is no longer configured — refresh and try again.",
                    remote.as_str()
                ))
            }
        }
        Precondition::SeedRecorded => match read_seed(repo) {
            Some(Ok(_)) => Ok(()),
            _ => refuse("The recorded seed is gone or unreadable — refusing to reset.".to_string()),
        },
    }
}

/// SHA-256 (lowercase hex) of the operation's canonical JSON — the digest the
/// [`Plan::operation_hash`] contract pins (#142) and #145 recomputes before
/// executing a client-approved plan.
fn operation_hash(operation: &GitOperation) -> OperationHash {
    let canonical =
        serde_json::to_string(operation).expect("GitOperation serialization cannot fail");
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    OperationHash::new(hex).expect("a sha256 digest is 64 lowercase hex chars")
}

/// The full ref name of a local branch.
fn heads(branch: &BranchName) -> Option<RefName> {
    RefName::new(format!("refs/heads/{branch}")).ok()
}

/// The full ref name of a tag — [`heads`]' sibling for `refs/tags/` (M2.21a,
/// #235).
fn tags(tag: &TagName) -> Option<RefName> {
    RefName::new(format!("refs/tags/{tag}")).ok()
}

/// Resolve a ref to its **unpeeled** value — what the ref itself points at,
/// with the same three-state honesty as [`rev_parse`] (D5: `Ok(None)` is a
/// fact about the repository, `Err` means git did not run and is a fact about
/// nothing).
///
/// A sibling rather than a flag on `rev_parse` because the two must never be
/// confused at a call site: `rev_parse` appends `^{commit}` and *peels*,
/// which every commit-shaped caller wants — and which, applied to an
/// annotated tag's ref, silently swaps the tag object for its target commit.
/// [`GitOperation::DeleteLocalTag`]'s observation is (so far) the one reader
/// for which that swap corrupts the answer: its `RecreateTag` recovery must
/// carry the value the ref actually held. See `build_plan`'s observation arm.
async fn rev_parse_ref_unpeeled(
    repo: &Path,
    ref_name: &str,
) -> Result<Option<String>, ExecUnavailable> {
    // Local (D3): resolving a ref reads the object database, never a remote.
    let output = crate::git_cmd::git_output(repo, &["rev-parse", "--verify", "--quiet", ref_name])
        .await
        .map_err(|e| ExecUnavailable::new(format!("couldn't run git rev-parse: {e}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!id.is_empty()).then_some(id))
}

/// The remote's default branch, read from `refs/remotes/<remote>/HEAD`.
///
/// `Ok(Some(name))` — the symbolic ref resolved and names a branch.
/// `Ok(None)` — git answered, and there is no such ref: this repository simply
/// does not record a default branch (a bare `git clone` sets it; a manually
/// added remote often does not).
/// `Err(_)` — the read itself failed.
///
/// **The two non-answers are kept apart on purpose.** Both become
/// [`Advisory::DefaultBranchUnknown`] at the call site, but they are different
/// facts and the reason text says which — collapsing them here would be the
/// same "could not look reads as nothing there" mistake this file guards
/// against everywhere else.
///
/// Local only: resolving a symbolic ref reads `.git`, never a socket. This is
/// deliberate and load-bearing — see [`Advisory`]'s docs for why no variant
/// claims anything about forge branch-protection rules, which are not
/// knowable without asking the forge.
async fn default_branch_of(repo: &Path, remote: &RemoteName) -> Result<Option<String>, String> {
    let ref_name = format!("refs/remotes/{}/HEAD", remote.as_str());
    let output = crate::git_cmd::git_output(
        repo,
        &["symbolic-ref", "--quiet", "--short", ref_name.as_str()],
    )
    .await
    .map_err(|e| format!("couldn't run git symbolic-ref: {e}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    // `--short` yields `<remote>/<branch>`; the branch is what follows the
    // first slash. Split once from the left rather than taking the last
    // segment: branch names may themselves contain slashes
    // (`feature/m4.32-...`), and `rsplit` would return only the tail.
    let short = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let prefix = format!("{}/", remote.as_str());
    Ok(short
        .strip_prefix(prefix.as_str())
        .map(str::to_string)
        .filter(|b| !b.is_empty()))
}

/// The advisories this operation earns (M4.32, #85).
///
/// Only a force-with-lease push earns any. An ordinary push cannot replace
/// remote history, so warning on it would train users to click through the
/// warnings that matter — the same argument `FetchRemote`'s docs make for
/// refusing to overstate its risk.
async fn advisories_for(repo: &Path, operation: &GitOperation) -> Vec<Advisory> {
    let GitOperation::PushBranch {
        branch,
        remote,
        force: ForcePublish::WithLease { .. },
        ..
    } = operation
    else {
        return Vec::new();
    };

    let mut out = vec![Advisory::RemoteHistoryReplaced {
        branch: branch.clone(),
        remote: remote.clone(),
    }];

    out.push(match default_branch_of(repo, remote).await {
        Ok(Some(default)) if default == branch.as_str() => Advisory::DefaultBranchPush {
            branch: branch.clone(),
            remote: remote.clone(),
        },
        // Known, and this is not it: no advisory. The absence here is earned —
        // the check ran and answered.
        Ok(Some(_)) => return out,
        Ok(None) => Advisory::DefaultBranchUnknown {
            reason: format!(
                "{} does not record a default branch (no refs/remotes/{}/HEAD), \
                 so this plan cannot tell you whether {} is it",
                remote.as_str(),
                remote.as_str(),
                branch.as_str()
            ),
        },
        Err(why) => Advisory::DefaultBranchUnknown {
            reason: format!("the default branch could not be read — {why}"),
        },
    });
    out
}

/// Best-effort `CommitOid` from an observation. `Absent` and `Unknown` both
/// yield `None` — correct here only because every caller uses it to *omit* a
/// descriptive field rather than to assert one; see the `PushBranch` arm in
/// [`shape`] for the one place where the distinction had to be made explicit.
fn oid_of(observed: &Obs<String>) -> Option<CommitOid> {
    observed
        .known()
        .and_then(|o| CommitOid::new(o.as_str()).ok())
}

/// The per-operation review shapes — risk, preconditions, expected ref
/// changes, recovery — following exactly the per-variant patterns the golden
/// fixture pinned (ADR 0015). Purely descriptive today (#145 enforces).
async fn shape(
    repo: &Path,
    operation: &GitOperation,
    observed: &Observed,
) -> (
    RiskLevel,
    Vec<Precondition>,
    Vec<RefChange>,
    RecoveryStrategy,
) {
    // The checked-out branch, in the pieces most shapes want.
    let head_name = observed
        .head_branch
        .as_deref()
        .and_then(|b| BranchName::new(b).ok());
    let head_ref = head_name.as_ref().and_then(heads);
    let head_oid = oid_of(&observed.head_tip);

    // A "the checked-out branch moves to a computed commit" shape, shared by
    // commit-on-HEAD / merge / rebase / revert: precondition that the branch is
    // checked out at its observed tip, a computed after-state, and a reset-back
    // recovery — exactly the fixture's pattern for those operations.
    let head_moves = |extra: Option<Precondition>| {
        let mut preconditions = Vec::new();
        if let Some(name) = head_name.clone() {
            preconditions.push(Precondition::BranchCheckedOut { branch: name });
        }
        preconditions.extend(extra);
        let changes = match (&head_ref, &head_oid) {
            (Some(r), Some(o)) => vec![RefChange {
                ref_name: r.clone(),
                before: RefState::At(o.clone()),
                after: RefState::Computed,
            }],
            _ => Vec::new(),
        };
        let recovery = match (&head_ref, &head_oid) {
            (Some(r), Some(o)) => RecoveryStrategy::ResetRef {
                ref_name: r.clone(),
                to: o.clone(),
            },
            // An unborn or unreadable HEAD: nothing to reset back to; the
            // operation itself will say what (if anything) it did.
            _ => RecoveryStrategy::NotNeeded,
        };
        (preconditions, changes, recovery)
    };

    match operation {
        // M3.24 (#77) — the stash drawer.
        //
        // No RefChange on any of the three. A stash moves `refs/stash`, which
        // is not a branch and not a tag, and the plan's ref-change list is the
        // reviewer-facing "what will move" — listing an internal ref the UI
        // never shows would be noise, not disclosure. The same D5 posture
        // `fetch_remote` takes.
        GitOperation::PushStash { .. } => (
            RiskLevel::Reversible,
            // No precondition: a dirty tree is this operation's whole input,
            // and git refuses to create an empty stash itself.
            Vec::new(),
            Vec::new(),
            // Nothing is destroyed — the changes move into a listed,
            // inspectable entry.
            RecoveryStrategy::NotNeeded,
        ),
        GitOperation::ApplyStash { .. } => (
            RiskLevel::Reversible,
            // CleanWorktree is the load-bearing decision of this slice: with a
            // clean tree the abort path is `reset --hard` + `clean -fd`, and
            // that is PROVABLY safe because there is nothing of the user's to
            // destroy. Applying into a dirty tree would mean an abort could
            // discard work that was never in the stash.
            vec![Precondition::CleanWorktree],
            Vec::new(),
            // The entry survives an apply by definition, so there is nothing
            // to put back.
            RecoveryStrategy::NotNeeded,
        ),
        GitOperation::BranchFromStash {
            name, expected_oid, ..
        } => (
            // Destructive for the same reason pop is: the entry goes. The new
            // branch and the checkout are not what makes it destructive —
            // both are trivially undone by hand, and neither can lose work.
            RiskLevel::Destructive,
            // The branch must not already exist. Stated as a precondition
            // rather than left to git because the refusal is more useful
            // before approval than after: a caller can pick another name
            // without having consumed anything.
            //
            // `heads(name)`, NOT `RefName::from(name)`: the precondition is
            // resolved against the ref store, so it needs the full
            // `refs/heads/<name>` path. A bare short name would be checked
            // under a spelling that never exists — a precondition that always
            // passes, which is worse than none, because the plan then displays
            // a guarantee it is not making. Caught by a mutation, not review.
            heads(name)
                .map(|ref_name| Precondition::RefAbsent { ref_name })
                .into_iter()
                .collect(),
            Vec::new(),
            // The stash is the only irreplaceable thing here, so it is what
            // the recovery names. The created branch and the moved HEAD are
            // separately reversible by ordinary means and would only crowd a
            // field that can hold one strategy.
            RecoveryStrategy::RecreateStashEntry {
                at: expected_oid.clone(),
                message: None,
            },
        ),
        GitOperation::PopStash { expected_oid, .. } => (
            // Destructive, not Reversible like its apply sibling. Apply keeps
            // the entry whatever happens; pop removes it, so what can be lost
            // is the stash itself, not just a tidy worktree. RiskLevel is
            // about what can be lost.
            RiskLevel::Destructive,
            // Same reasoning as ApplyStash: with a clean tree the abort path
            // is provably safe because there is nothing of the user's to
            // destroy. It matters more here — a pop that had to be abandoned
            // in a dirty tree could discard work that was never in the stash
            // AND the entry holding the rest of it.
            vec![Precondition::CleanWorktree],
            Vec::new(),
            // The same undo a drop gets, for the same reason: if the entry was
            // removed, the commit is unreachable and only the pin keeps it
            // alive. When the pop CONFLICTS git leaves the entry in place, so
            // this recovery is simply unnecessary rather than wrong — an undo
            // that recreates an entry which still exists is refused by its own
            // preconditions, not by this tag.
            RecoveryStrategy::RecreateStashEntry {
                at: expected_oid.clone(),
                // Not recoverable from the oid; it lives in the reflog line
                // that the pop destroys. Left None rather than guessed.
                message: None,
            },
        ),
        GitOperation::DropStash { expected_oid, .. } => (
            // Destructive on the same reasoning ForceDeleteBranch is: the
            // commit becomes unreachable. RiskLevel is about what can be lost,
            // not about whether an undo was built.
            RiskLevel::Destructive,
            // The compare-and-swap that actually protects this lives in the
            // executor (`stash_entry_still_at`), not here, because the thing
            // being guarded is a reflog position — and Precondition's
            // vocabulary is ref-shaped. Stating it as a RefAt against
            // `refs/stash` would be a lie: refs/stash points at stash@{0}
            // whatever this operation targets.
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::RecreateStashEntry {
                at: expected_oid.clone(),
                // The label is not recoverable from the oid — it lives in the
                // reflog line, which is what the drop destroys. Left None
                // rather than guessed; content recovery does not depend on it.
                message: None,
            },
        ),
        GitOperation::CreateBranch { name, at } => {
            let target = heads(name);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefAbsent {
                    ref_name: r.clone(),
                })
                .collect();
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::Absent,
                    after: RefState::At(at.clone()),
                })
                .collect();
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::DeleteCreatedBranch { name: name.clone() },
            )
        }
        GitOperation::CommitOnHead { .. } => {
            let (preconditions, changes, recovery) = head_moves(None);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::EmptyCommitOnBranch {
            branch,
            expected_tip,
            ..
        } => {
            let target = heads(branch);
            let mut preconditions = vec![Precondition::BranchNotCheckedOut {
                branch: branch.clone(),
            }];
            preconditions.extend(target.iter().map(|r| Precondition::RefAt {
                ref_name: r.clone(),
                oid: expected_tip.clone(),
            }));
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(expected_tip.clone()),
                    after: RefState::Computed,
                })
                .collect();
            let recovery = match target {
                Some(r) => RecoveryStrategy::ResetRef {
                    ref_name: r,
                    to: expected_tip.clone(),
                },
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        // M4.31 (#84). Reversible, not Safe: taking one side discards the
        // other from the index, which is a real loss even though git can
        // rebuild it. And not Destructive: the discarded side is still named
        // by MERGE_HEAD, so `git checkout --merge` reconstructs the conflict
        // exactly — a definite mechanism, not a maybe.
        //
        // No `Precondition`. The obvious one — "this path is still
        // conflicted" — cannot be expressed in this vocabulary, which
        // compares refs and worktree cleanliness, not index stage entries.
        // Rather than approximate it with a precondition that checks
        // something else and reads as if it checked this, the executor
        // re-reads the conflict immediately before acting and refuses there.
        // See `exec_resolve_conflict`.
        GitOperation::ResolveConflict { .. } => (
            RiskLevel::Reversible,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::ConflictRecreatableWhileInProgress,
        ),
        // M4.31c (#432, ADR 0069). Reversible, never Safe: unlike a whole-side
        // take (index-only) this WRITES the worktree file. A hand-edit landing
        // outside the app between serve and submit is overwritten, not merged
        // — `ConflictRecreatableWhileInProgress` recovers the conflict itself
        // (`git checkout -m`), not that overwritten edit. Same "no
        // Precondition, executor re-reads" reasoning as `ResolveConflict`
        // above, extended: the executor also re-mints the `conflict-v1:`
        // token and compares the stage OID triple, neither of which any
        // Precondition in this vocabulary can express. See
        // `exec_resolve_conflict_content`.
        GitOperation::ResolveConflictContent { .. } => (
            RiskLevel::Reversible,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::ConflictRecreatableWhileInProgress,
        ),
        GitOperation::StageAll
        | GitOperation::UnstageAll
        // A staging selection is index-only like its -All siblings: the
        // working tree keeps every edit whichever way it goes, so nothing
        // can be lost and no recovery is needed. Its real admission gate is
        // the diff-generation check in the handler (staging.rs), which runs
        // before a plan is ever minted.
        | GitOperation::StageSelection { .. } => (
            RiskLevel::Safe,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        GitOperation::CheckoutBranch { branch } => {
            let target = heads(branch);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefExists {
                    ref_name: r.clone(),
                })
                .collect();
            // HEAD's symbolic move: from the current branch (or, detached, the
            // exact commit it sits on) to the named branch.
            let before = match (&head_ref, &head_oid) {
                (Some(r), _) => Some(RefState::Symbolic(r.clone())),
                (None, Some(o)) => Some(RefState::At(o.clone())),
                (None, None) => None,
            };
            let changes = match (before, &target) {
                (Some(before), Some(t)) => vec![RefChange {
                    ref_name: RefName::new("HEAD").expect("literal is valid"),
                    before,
                    after: RefState::Symbolic(t.clone()),
                }],
                _ => Vec::new(),
            };
            let recovery = match head_name {
                Some(previous) => RecoveryStrategy::CheckoutPrevious { branch: previous },
                // Detached HEAD: there is no previous *branch* to return to.
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Safe, preconditions, changes, recovery)
        }
        GitOperation::MergeBranch { .. } => {
            let extra = match (&head_ref, &head_oid) {
                (Some(r), Some(o)) => Some(Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                }),
                _ => None,
            };
            let (preconditions, changes, recovery) = head_moves(extra);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::PushBranch {
            branch,
            remote,
            force,
            ..
        } => {
            let mut preconditions = vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }];
            let target = heads(branch);
            preconditions.extend(target.iter().map(|r| Precondition::RefExists {
                ref_name: r.clone(),
            }));
            // The remote-tracking ref this push is expected to move.
            let tracking = RefName::new(format!("refs/remotes/{remote}/{branch}")).ok();
            // M2.20a (#227): the lease *is* a compare-and-swap, so it becomes
            // one — a `Precondition::RefAt` on the remote-tracking ref, the
            // same machinery `ResetBranch`/`AmendCommit` use on local refs.
            //
            // Two things this deliberately does not do. It does not re-read
            // the remote to fill the oid in: the value that makes a lease
            // mean anything is the one the *user reviewed*, and a freshly
            // read one would assert only that the remote has not moved since
            // a millisecond ago. And it does not fall back to some
            // observation when the ref is unnameable — a lease with no
            // precondition would be a force push with a reassuring label,
            // which is the one outcome `ForcePublish` exists to prevent.
            //
            // M2.20e (#231) moved where that last guarantee is kept, and the
            // move is worth stating because the old sentence here cited a
            // `501` in `execute` that no longer exists. Every `PushBranch`
            // combination now executes, so "no precondition ⇒ no execution"
            // is no longer true by refusal — it is true because
            // `planner::push::verify_lease` re-derives
            // `refs/remotes/<remote>/<branch>` from the same two validated
            // newtypes and refuses `409` on its own, whether or not this
            // function managed to name the ref. The lease is checked before a
            // socket exists either way; what changed is that the check lives
            // beside the spawn rather than in a stub next to it.
            //
            // A `match` rather than an `if let`: should `ForcePublish` ever
            // grow a third variant, this stops compiling instead of silently
            // treating the new mode as unleased.
            let lease = match force {
                ForcePublish::None => None,
                ForcePublish::WithLease {
                    expected_remote_tip,
                } => Some(expected_remote_tip),
            };
            if let (Some(oid), Some(r)) = (lease, &tracking) {
                preconditions.push(Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: oid.clone(),
                });
            }
            let remote_tip = Obs::from_read(rev_parse(repo, &format!("{remote}/{branch}")).await);
            let local_tip = Obs::from_read(rev_parse(repo, branch.as_str()).await);
            // D5: `before` is a *claim* about the remote-tracking ref shown to
            // the user for review, so `Unknown` may not be rendered as
            // `Absent` ("this ref does not exist yet"). An unreadable
            // observation thins the plan instead — the documented posture for
            // every other failed read in `build_plan`.
            let before = match &remote_tip {
                Obs::Known(_) => oid_of(&remote_tip).map(RefState::At),
                Obs::Absent => Some(RefState::Absent),
                Obs::Unknown => None,
            };
            let changes = match (tracking, oid_of(&local_tip), before) {
                (Some(r), Some(local), Some(before)) => vec![RefChange {
                    ref_name: r,
                    before,
                    after: RefState::At(local),
                }],
                _ => Vec::new(),
            };
            // A lease-force can leave commits on the remote branch referenced
            // by nothing, which an ordinary fast-forward push cannot — see
            // `RiskLevel::Destructive`'s doc for why one scalar has to rank
            // that above `Remote`. Recovery stays `Irrecoverable` for both:
            // the effect left the machine either way.
            let risk = match force {
                ForcePublish::None => RiskLevel::Remote,
                ForcePublish::WithLease { .. } => RiskLevel::Destructive,
            };
            (
                risk,
                preconditions,
                changes,
                RecoveryStrategy::Irrecoverable,
            )
        }
        GitOperation::DeleteBranch { branch } | GitOperation::ForceDeleteBranch { branch } => {
            let risk = if matches!(operation, GitOperation::DeleteBranch { .. }) {
                RiskLevel::Reversible
            } else {
                RiskLevel::Destructive
            };
            let target = heads(branch);
            let tip = oid_of(&observed.branch_tip);
            let preconditions = match (&target, &tip) {
                (Some(r), Some(o)) => vec![Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                }],
                (Some(r), None) => vec![Precondition::RefExists {
                    ref_name: r.clone(),
                }],
                _ => Vec::new(),
            };
            let changes = match (&target, &tip) {
                (Some(r), Some(o)) => vec![RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(o.clone()),
                    after: RefState::Absent,
                }],
                _ => Vec::new(),
            };
            let recovery = match tip {
                Some(at) => RecoveryStrategy::RecreateBranch {
                    name: branch.clone(),
                    at,
                },
                // No observed tip means no branch to delete — execution will
                // surface git's own refusal.
                None => RecoveryStrategy::NotNeeded,
            };
            (risk, preconditions, changes, recovery)
        }
        GitOperation::RebaseOntoBase { base } => {
            // The base as git resolves it: `origin/main` is a remote-tracking
            // ref, a bare `main` a local branch.
            let base_full = if base.as_str().contains('/') {
                RefName::new(format!("refs/remotes/{base}")).ok()
            } else {
                RefName::new(format!("refs/heads/{base}")).ok()
            };
            let extra = base_full.map(|r| Precondition::RefExists { ref_name: r });
            let (preconditions, changes, recovery) = head_moves(extra);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::RestoreBranch { name, tip } => {
            let target = heads(name);
            let preconditions = target
                .iter()
                .map(|r| Precondition::RefAbsent {
                    ref_name: r.clone(),
                })
                .collect();
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::Absent,
                    after: RefState::At(tip.clone()),
                })
                .collect();
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::DeleteCreatedBranch { name: name.clone() },
            )
        }
        GitOperation::ResetBranch {
            branch,
            to,
            expected_tip,
        } => {
            let target = heads(branch);
            let mut preconditions: Vec<Precondition> = target
                .iter()
                .map(|r| Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: expected_tip.clone(),
                })
                .collect();
            // The clean-worktree requirement holds exactly when the reset runs
            // as `git reset --hard` — i.e. the branch is the checked-out one.
            if observed.head_branch.as_deref() == Some(branch.as_str()) {
                preconditions.push(Precondition::CleanWorktree);
            }
            let changes = target
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(expected_tip.clone()),
                    after: RefState::At(to.clone()),
                })
                .collect();
            let recovery = match target {
                Some(r) => RecoveryStrategy::ResetRef {
                    ref_name: r,
                    to: expected_tip.clone(),
                },
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Destructive, preconditions, changes, recovery)
        }
        // Continue and skip both move the sequence forward and may create a
        // commit; the undo is to move HEAD back, which head_moves supplies.
        GitOperation::SequenceContinue | GitOperation::SequenceSkip => {
            let (preconditions, changes, recovery) = head_moves(None);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        // Destructive, and NOT for the reason the others in that class are.
        // Nothing in git's object database is lost — the commits being applied
        // still exist. What abort discards is every conflict RESOLUTION made
        // during this sequence, including ones an earlier --continue already
        // committed. Those were hand-made decisions about file content and
        // exist nowhere else.
        //
        // No recovery strategy names them, because none can: there is no ref
        // to move back to and no object holding the resolutions. Irrecoverable
        // is the honest tag, and it is a fact about the operation rather than
        // about what this application chose to offer.
        GitOperation::SequenceAbort => (
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        GitOperation::CherryPick { .. } | GitOperation::CherryPickMerge { .. } => {
            // A cherry-pick ADDS a commit to the current branch, so nothing
            // existing is lost and the undo is to move the branch back — the
            // same shape as any other commit-creating operation.
            //
            // CleanWorktree matters here for a reason revert does not share:
            // a cherry-pick that conflicts leaves the sequencer mid-flight,
            // and `--abort` is what unwinds it. That is only provably safe
            // with nothing of the user's in the tree to destroy.
            let (preconditions, changes, recovery) =
                head_moves(Some(Precondition::CleanWorktree));
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        GitOperation::RevertMerge { commit, .. } => {
            // Identical shape to RevertCommit: a revert ADDS a commit, so
            // nothing existing is lost and the undo is to revert the revert.
            // The mainline choice changes what the revert commit contains,
            // never what it risks.
            let (preconditions, changes, _) = head_moves(None);
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::RevertCommit {
                    commit: commit.clone(),
                },
            )
        }
        GitOperation::RevertCommit { commit } => {
            let (preconditions, changes, _) = head_moves(None);
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::RevertCommit {
                    commit: commit.clone(),
                },
            )
        }
        GitOperation::ResetTestRepo => (
            // The composite's exact moves and deletions are computed at
            // execution time from the recorded seed; the reset also wipes the
            // journal, which is what makes it irrecoverable.
            RiskLevel::Destructive,
            vec![Precondition::SeedRecorded],
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        // #219: neither operation moves a ref, so there is nothing for a
        // `Precondition`/`RefChange` to describe here — the real admission
        // gate is the per-path tracked/untracked re-verification the
        // executor runs immediately before running git (`verify_path_states`),
        // the same "real gate lives in the executor, not in `shape`" posture
        // `StageSelection` above already established for its own diff-generation
        // check.
        //
        // Distinct recovery strategies (review finding): a discarded tracked
        // path may still be recoverable from the object database if it was
        // ever staged; a deleted untracked path never can be. Sharing one
        // tag here would defeat the reason `RecoveryStrategy` is typed at
        // all.
        GitOperation::DiscardTrackedPaths { .. } => (
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::RecoverableIfStaged,
        ),
        GitOperation::DeleteUntrackedPaths { .. } => (
            RiskLevel::Destructive,
            Vec::new(),
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        // M2.19a (#222) shaped this; M2.19b (#223) executes it — see
        // `GitOperation::AmendCommit`'s doc comment for the full reasoning
        // behind every choice below, and `exec_amend_commit` for execution.
        // Deliberately *not* built from `head_moves` above: that helper
        // derives its "before" oid from `observed`, but amend needs a real
        // compare-and-swap against the operation's own `expected_tip`
        // (mirroring `EmptyCommitOnBranch`/`ResetBranch`), since "the tip
        // moved since this was reviewed" is exactly the danger a CAS
        // precondition exists to catch.
        GitOperation::AmendCommit { expected_tip, .. } => {
            let mut preconditions = Vec::new();
            if let Some(name) = head_name.clone() {
                preconditions.push(Precondition::BranchCheckedOut { branch: name });
            }
            if let Some(r) = &head_ref {
                preconditions.push(Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: expected_tip.clone(),
                });
            }
            let changes = match &head_ref {
                Some(r) => vec![RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(expected_tip.clone()),
                    after: RefState::Computed,
                }],
                None => Vec::new(),
            };
            // `ResetRef`, deliberately not a new tag — see the variant's doc
            // comment for why this is the honest choice, not the
            // closest-looking tag picked by reflex.
            let recovery = match &head_ref {
                Some(r) => RecoveryStrategy::ResetRef {
                    ref_name: r.clone(),
                    to: expected_tip.clone(),
                },
                // Detached HEAD: no branch ref for `ResetRef` to name — the
                // same degradation `head_moves` uses for the same case.
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Destructive, preconditions, changes, recovery)
        }
        // M2.20a (#227) shaped both of these — see each variant's doc comment
        // in `plan.rs` for the reasoning behind every choice below. Fetch now
        // executes (M2.20c, #229, ADR 0043) against exactly this plan; pull's
        // executor is still #230's, and `execute` refuses it.
        //
        // A fetch moves only `refs/remotes/<remote>/*`, and *which* of them
        // is not knowable until git has spoken to the remote — so there is no
        // honest `RefChange` to list here, and listing a guessed one would be
        // a claim shown to a reviewer that the operation may not honour (the
        // same D5 posture the push arm above takes with `Obs::Unknown`). The
        // one thing that must hold is that the remote is configured.
        GitOperation::FetchRemote { remote } => (
            RiskLevel::Safe,
            vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }],
            Vec::new(),
            RecoveryStrategy::NotNeeded,
        ),
        // A pull is a fetch (nothing to describe, above) plus an integration
        // that moves exactly one local ref: the checked-out branch. That is
        // precisely `head_moves`' shape, and it is reused rather than
        // re-derived so pull, merge and rebase cannot drift apart —
        // `MergeBranch` above passes the same `RefAt` extra for the same
        // reason.
        //
        // The CAS is on the *local* branch, not the remote one. A pull's
        // danger is that the local branch moved under the reviewer between
        // plan and execution (someone committed, or another pull landed);
        // where the remote sits is what the fetch half is for, and pinning it
        // would refuse pulls for the ordinary reason that the remote received
        // a new commit — the very thing being pulled.
        GitOperation::PullBranch { remote, .. } => {
            let mut extra = vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }];
            if let (Some(r), Some(o)) = (&head_ref, &head_oid) {
                extra.push(Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                });
            }
            let (mut preconditions, changes, recovery) = head_moves(None);
            preconditions.extend(extra);
            (RiskLevel::Reversible, preconditions, changes, recovery)
        }
        // M2.21a (#235, ADR 0041): contract only — the four tag shapes below
        // are pinned by the golden fixture; execution belongs to the later
        // M2.21 slices of #74 and `execute` refuses all four today.
        //
        // Create follows `CreateBranch`'s pattern (RefAbsent + delete-created
        // recovery), with one difference the annotation forces: a lightweight
        // tag ref will point exactly at `target`, but an annotated tag ref
        // points at a tag *object* the operation itself creates — a value
        // unknowable at review time, so `RefState::Computed`, the same
        // honesty `CommitOnHead` applies to its own new commit.
        GitOperation::CreateTag {
            name,
            target,
            annotation,
        } => {
            let tag_ref = tags(name);
            let preconditions = tag_ref
                .iter()
                .map(|r| Precondition::RefAbsent {
                    ref_name: r.clone(),
                })
                .collect();
            let after = match annotation {
                None => RefState::At(target.clone()),
                Some(_) => RefState::Computed,
            };
            let changes = tag_ref
                .iter()
                .map(|r| RefChange {
                    ref_name: r.clone(),
                    before: RefState::Absent,
                    after: after.clone(),
                })
                .collect();
            (
                RiskLevel::Reversible,
                preconditions,
                changes,
                RecoveryStrategy::DeleteCreatedTag { name: name.clone() },
            )
        }
        // Delete-local follows `ForceDeleteBranch`'s pattern, deliberately
        // not `DeleteBranch`'s: `git tag -d` has no `-d`-vs-`-D` safety
        // split — it deletes whether or not the tagged commit is reachable
        // from anything else, and tag refs keep no reflog, so this is the
        // unguarded delete and ranks `Destructive`. The observed value (and
        // therefore the CAS precondition and the recovery's `at`) is the
        // **unpeeled** ref value — see `build_plan`'s observation arm and
        // `RecreateTag`'s doc for why that one choice is what makes the
        // recovery an exact restoration instead of a re-authored look-alike.
        GitOperation::DeleteLocalTag { name } => {
            let tag_ref = tags(name);
            let value = oid_of(&observed.branch_tip);
            let preconditions = match (&tag_ref, &value) {
                (Some(r), Some(o)) => vec![Precondition::RefAt {
                    ref_name: r.clone(),
                    oid: o.clone(),
                }],
                (Some(r), None) => vec![Precondition::RefExists {
                    ref_name: r.clone(),
                }],
                _ => Vec::new(),
            };
            let changes = match (&tag_ref, &value) {
                (Some(r), Some(o)) => vec![RefChange {
                    ref_name: r.clone(),
                    before: RefState::At(o.clone()),
                    after: RefState::Absent,
                }],
                _ => Vec::new(),
            };
            let recovery = match value {
                Some(at) => RecoveryStrategy::RecreateTag {
                    name: name.clone(),
                    at,
                },
                // No observed value means no tag to delete — execution will
                // surface git's own refusal, exactly the branch-delete
                // degradation above.
                None => RecoveryStrategy::NotNeeded,
            };
            (RiskLevel::Destructive, preconditions, changes, recovery)
        }
        // The two remote-reaching tag operations list no ref change: a
        // remote tag has no local remote-tracking ref (tags fetch straight
        // into `refs/tags/`), so there is nothing honest to show a reviewer
        // moving — the same D5 posture as `FetchRemote` above. Risk and
        // recovery follow the push family: the effect leaves the machine
        // (`Irrecoverable`), destructively so for the remote delete
        // (commits on the remote can lose their last ref there), additively
        // for the tag push (`Remote`, like a fast-forward branch push).
        GitOperation::DeleteRemoteTag { remote, .. } => (
            RiskLevel::Destructive,
            vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }],
            Vec::new(),
            RecoveryStrategy::Irrecoverable,
        ),
        GitOperation::PushTag { name, remote } => {
            let mut preconditions = vec![Precondition::RemoteConfigured {
                remote: remote.clone(),
            }];
            preconditions.extend(tags(name).map(|r| Precondition::RefExists { ref_name: r }));
            (
                RiskLevel::Remote,
                preconditions,
                Vec::new(),
                RecoveryStrategy::Irrecoverable,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Validate
// ---------------------------------------------------------------------------

/// The structural checks a plan must pass before execution. With plans built
/// and executed in the same request these can only fail on a server bug — but
/// they are the exact seam #145 widens into generation-equality, precondition
/// and staleness enforcement for client-reviewed plans, so they run (and
/// refuse, loudly) rather than being asserted away.
fn validate(plan: &Plan) -> Result<(), (StatusCode, String)> {
    if operation_hash(&plan.operation).as_str() != plan.operation_hash.as_str() {
        return Err((
            StatusCode::CONFLICT,
            "This plan doesn't match the operation it approves — refusing to execute.".to_string(),
        ));
    }
    if crate::activity::now_secs() > plan.expires_at.0 {
        return Err((
            StatusCode::CONFLICT,
            "This plan has expired — refresh and try again.".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Execute — the only mutating-git-argv construction in the server
// ---------------------------------------------------------------------------

/// Execute a validated plan. One arm per operation; each arm is the old write
/// handler's execution code moved here unchanged — same argv, same journaling,
/// same responses.
async fn execute(repo: &Path, plan: Plan, observed: Observed) -> (StatusCode, String) {
    // D3 (#66, Task 8): the sandbox tier is chosen from the *declared* network
    // need of the typed operation, and this is the only place in the server
    // where that operation's identity is still in scope. Derive it here, before
    // the match consumes `plan.operation`, and thread it down through every
    // `exec_*` to `run_git` → `git_cmd::git_output_for` → `sandbox::policy_for`.
    //
    // Threading it rather than re-deriving it further down is the whole point.
    // By the time the argv exists, the operation is gone and only a string
    // match on the subcommand is left — the classifier `network_need` itself
    // documents as incomplete-by-construction. Passing the value keeps
    // `network_need_for_operation`'s exhaustive, wildcard-free match *in the
    // live data path*, so a new `GitOperation` variant cannot be added without
    // stating its network need. A version of this that computed the need and
    // discarded it would leave that match decorative and the guarantee empty.
    let need = network_need_for_operation(&plan.operation);
    match plan.operation {
        GitOperation::CreateBranch { name, at } => {
            branch_exec::exec_create_branch(repo, need, &name, &at).await
        }
        GitOperation::CommitOnHead {
            message,
            allow_empty,
        } => commit_exec::exec_commit_on_head(repo, need, &message, allow_empty, &observed).await,
        GitOperation::EmptyCommitOnBranch {
            branch,
            message,
            expected_tip,
        } => {
            commit_exec::exec_empty_commit_on_branch(repo, need, &branch, &message, &expected_tip)
                .await
        }
        GitOperation::ResolveConflict { path, resolution } => {
            conflict_exec::exec_resolve_conflict(repo, need, &path, resolution).await
        }
        GitOperation::ResolveConflictContent {
            path,
            expected_stages,
            expected_source,
            content,
        } => {
            conflict_exec::exec_resolve_conflict_content(
                repo,
                need,
                &path,
                &expected_stages,
                &expected_source,
                content,
            )
            .await
        }
        GitOperation::StageAll => staging_exec::exec_stage_all(repo, need).await,
        GitOperation::UnstageAll => staging_exec::exec_unstage_all(repo, need).await,
        GitOperation::CheckoutBranch { branch } => {
            branch_exec::exec_checkout(repo, need, &branch, &observed).await
        }
        GitOperation::MergeBranch { branch } => {
            branch_exec::exec_merge(
                repo,
                need,
                &RefName::from(&branch),
                &observed,
                branch_exec::IntegrationCaller::Direct,
            )
            .await
        }
        // M2.20a (#227) widened `PushBranch` with `set_upstream` and `force`;
        // M2.20e (#231, ADR 0045) wired all four combinations through
        // `planner::push`. **One arm, not one per combination**: the fields are
        // passed down whole, so there is exactly one place that turns them into
        // an argv (`push::push_argv`) and no path on which a mode the user
        // approved could be silently dropped on the way to git.
        GitOperation::PushBranch {
            branch,
            remote,
            set_upstream,
            force,
        } => push::exec_push(repo, need, &branch, &remote, set_upstream, &force).await,
        GitOperation::DeleteBranch { branch } => {
            branch_exec::exec_delete(repo, need, &branch, &observed, false).await
        }
        GitOperation::ForceDeleteBranch { branch } => {
            branch_exec::exec_delete(repo, need, &branch, &observed, true).await
        }
        GitOperation::RebaseOntoBase { base } => {
            branch_exec::exec_rebase(
                repo,
                need,
                &base,
                &observed,
                branch_exec::IntegrationCaller::Direct,
            )
            .await
        }
        GitOperation::RestoreBranch { name, tip } => {
            branch_exec::exec_restore_branch(repo, need, &name, &tip).await
        }
        GitOperation::ResetBranch {
            branch,
            to,
            expected_tip,
        } => {
            branch_exec::exec_reset_branch(repo, need, &branch, &to, &expected_tip, &observed).await
        }
        GitOperation::RevertCommit { commit } => {
            sequence_exec::exec_revert(repo, need, &commit, None, &observed).await
        }
        GitOperation::SequenceContinue => {
            sequence_exec::exec_sequence(repo, need, sequence_exec::SequenceVerb::Continue).await
        }
        GitOperation::SequenceSkip => {
            sequence_exec::exec_sequence(repo, need, sequence_exec::SequenceVerb::Skip).await
        }
        GitOperation::SequenceAbort => {
            sequence_exec::exec_sequence(repo, need, sequence_exec::SequenceVerb::Abort).await
        }
        GitOperation::CherryPick { commit } => {
            sequence_exec::exec_cherry_pick(repo, need, &commit, None).await
        }
        GitOperation::CherryPickMerge { commit, mainline } => {
            sequence_exec::exec_cherry_pick(repo, need, &commit, Some(mainline)).await
        }
        GitOperation::RevertMerge { commit, mainline } => {
            sequence_exec::exec_revert(repo, need, &commit, Some(mainline), &observed).await
        }
        GitOperation::ResetTestRepo => worktree_exec::exec_reset_test_repo(repo, need).await,
        GitOperation::StageSelection {
            direction,
            expected_diff_generation,
            patch,
            whole_files,
        } => {
            staging_exec::exec_stage_selection(
                repo,
                need,
                direction,
                &expected_diff_generation,
                &patch,
                &whole_files,
            )
            .await
        }
        GitOperation::DiscardTrackedPaths { paths } => {
            worktree_exec::exec_discard_tracked_paths(repo, need, &paths).await
        }
        GitOperation::DeleteUntrackedPaths { paths } => {
            worktree_exec::exec_delete_untracked_paths(repo, need, &paths).await
        }
        // M2.19a (#222) shipped the typed contract; M2.19b (#223, ADR 0040)
        // wired this execution — `handlers::commit::amend_commit` builds the
        // operation from `POST /api/amend-commit`.
        GitOperation::AmendCommit {
            message,
            expected_tip,
            allow_empty,
        } => {
            commit_exec::exec_amend_commit(
                repo,
                need,
                &message,
                &expected_tip,
                allow_empty,
                &observed,
            )
            .await
        }
        // M2.20a (#227) shipped the typed contract for fetch and pull;
        // M2.20c (#229, ADR 0043) wired *fetch* execution — the first code in
        // this server to open a socket with a user's credentials on it, which
        // is why it got a review of its own rather than riding in on the
        // vocabulary change. `handlers::fetch::fetch_remote` builds the
        // operation from `POST /api/fetch`.
        GitOperation::FetchRemote { remote } => fetch::exec_fetch(repo, need, &remote).await,
        // M2.20d (#230, ADR 0044) wired pull execution: the fetch half is
        // `planner::fetch`'s own `run_fetch` — the same spawn, the same
        // streamed progress, the same cancellation latch, never a second copy
        // — and the integration half is `exec_merge`/`exec_rebase` above,
        // dispatched on the `strategy` the reviewed plan carries.
        // `handlers::pull::pull_branch` builds the operation from
        // `POST /api/pull`.
        GitOperation::PullBranch {
            remote,
            branch,
            strategy,
        } => pull::exec_pull(repo, need, &remote, &branch, strategy, &observed).await,
        // M2.21a (#235, ADR 0041) shipped the typed tag contract; M2.21d
        // (#238, ADR 0048) wires the two **local** halves below —
        // `handlers::tags::create_tag` and `handlers::tags::delete_tag` build
        // them from `POST /api/tag` and `POST /api/delete-tag`.
        GitOperation::CreateTag {
            name,
            target,
            annotation,
        } => tag_exec::exec_create_tag(repo, need, &name, &target, annotation.as_ref()).await,
        GitOperation::DeleteLocalTag { name } => {
            tag_exec::exec_delete_local_tag(repo, need, &name, &observed).await
        }
        // M2.21f (#240): the two remote-reaching tag operations wired to
        // real execution — the same Network-tier chokepoint `push::exec_push`
        // already runs through (askpass hardening, streamed progress,
        // cancellation, redaction all come free from `git_streamed_for`).
        // `handlers::tags::delete_remote_tag`/`push_tag` build these from
        // `POST /api/delete-remote-tag` and `POST /api/push-tag`.
        GitOperation::DeleteRemoteTag { name, remote } => {
            remote_tags::exec_delete_remote_tag(repo, need, &name, &remote).await
        }
        GitOperation::PushTag { name, remote } => {
            remote_tags::exec_push_tag(repo, need, &name, &remote).await
        }
        // M3.24 (#77): the stash drawer. Apply, Pop and Drop are separate
        // operations on purpose — see PopStash's own comment for why it is not
        // ApplyStash with a flag.
        GitOperation::BranchFromStash {
            name,
            entry,
            expected_oid,
        } => stash::exec_branch_from_stash(repo, need, &name, &entry, &expected_oid).await,
        GitOperation::PopStash {
            entry,
            expected_oid,
        } => stash::exec_pop_stash(repo, need, &entry, &expected_oid).await,
        GitOperation::PushStash {
            message,
            keep_index,
            include_untracked,
        } => {
            stash::exec_push_stash(repo, need, message.as_ref(), keep_index, include_untracked)
                .await
        }
        GitOperation::ApplyStash {
            entry,
            expected_oid,
        } => stash::exec_apply_stash(repo, need, &entry, &expected_oid).await,
        GitOperation::DropStash {
            entry,
            expected_oid,
        } => stash::exec_drop_stash(repo, need, &entry, &expected_oid).await,
    }
}

// --- small shared runners ---------------------------------------------------

/// Spawn `git -C <repo> <args…>` and collect its output; `Err` is the
/// "couldn't run git at all" case every endpoint maps to a 500.
///
/// Goes through the sealed sandbox launcher (`crate::git_cmd::git_output`,
/// #66 Task 6) rather than a raw `Command::new("git")` — this is the
/// executor, where every client-requested mutation's argv actually runs.
async fn run_git(repo: &Path, need: NetworkNeed, args: &[&str]) -> std::io::Result<Output> {
    crate::git_cmd::git_output_for(repo, args, need).await
}

/// The wall-clock ceiling on a git spawn that may run repository hooks —
/// `pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit` — arbitrary
/// user code whose own waiting is not under this server's control, the same
/// arity [`SIGN_TIMEOUT`](tag_exec::SIGN_TIMEOUT) gives `git tag -s`'s call out to `gpg` (#72,
/// M2.19: "hooks cannot freeze the UI").
///
/// Sized against the client, not against any hook Tom actually has:
/// `REQUEST_TIMEOUT_MS = 60_000` (`crates/git-vista/src/api.rs`) is when the
/// browser gives up on the request, so the server's honest, typed answer is
/// only worth building if it arrives first — with room for the coordinator
/// `Waiting` stage and the bounded post-kill HEAD read below, both charged
/// against the same 60s. Larger than `SIGN_TIMEOUT` on purpose: a keyless
/// signing failure resolves in under a second (measured, `SIGN_TIMEOUT`'s own
/// doc), but a real `pre-commit` running a formatter or a lint pass
/// legitimately takes seconds, and 30s covers any hook that belongs on a
/// commit button. Not configurable in v1 — no per-repo tunable surface exists
/// for it to slot into, and the refusal text below names the number, so the
/// first hook that actually needs longer is self-diagnosing rather than
/// silently wrong.
const HOOKED_GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
thread_local! {
    /// Test-only override for [`HOOKED_GIT_TIMEOUT`], read by
    /// [`hooked_git_timeout`] so the sleeping-hook tests in
    /// `hook_timeout_suite` can shrink the bound to a couple of seconds
    /// instead of waiting out the real 30.
    ///
    /// A `thread_local`, not a process-wide `OnceLock`: `#[tokio::test]`
    /// defaults to a **current-thread** runtime, so every `.await` inside
    /// one test's future — and everything it transitively calls, including
    /// this function — runs on that one test's own OS thread. Scoping the
    /// override to the thread means one test's shrunk bound can never leak
    /// into, or race against, another test's real-bound assertions running
    /// concurrently on other threads under `cargo test`'s default
    /// parallel-threads-one-process model — the same hazard
    /// `sandbox::argv::SSH_AUTH_SOCK_LOCK` documents for a process-wide env
    /// var, avoided here instead of guarded against. This requires every
    /// test that sets the override to stay on the default current-thread
    /// `#[tokio::test]` flavor; none in this suite use
    /// `flavor = "multi_thread"`.
    static HOOKED_GIT_TIMEOUT_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// [`HOOKED_GIT_TIMEOUT`], or the current test thread's override — see
/// [`HOOKED_GIT_TIMEOUT_OVERRIDE`]'s doc.
fn hooked_git_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        if let Some(d) = HOOKED_GIT_TIMEOUT_OVERRIDE.with(std::cell::Cell::get) {
            return d;
        }
    }
    HOOKED_GIT_TIMEOUT
}

// ---------------------------------------------------------------------------
// Test-only: a real signal for "the guard is held", not an assumed one (#444)
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// Test-only signal that fires the instant [`plan_and_execute_in`]
    /// acquires [`crate::coordinator::lock`]. Read by
    /// `hook_timeout_suite::the_coordinator_lock_is_released_after_a_hook_timeout`,
    /// which races a hooked commit against a `CreateBranch` on the same
    /// repository and needs the create-branch leg to start only once the
    /// commit leg genuinely holds the guard.
    ///
    /// Needed because `build_plan` runs before the guard, deliberately (see
    /// [`plan_and_execute_in`]'s own doc), and does real, OS-scheduled work —
    /// a `rev_parse` subprocess spawn and a `refs_digest_input`
    /// `spawn_blocking`. `tokio::join!`'s poll order says nothing about which
    /// of two joined futures reaches those points first: both are polled on
    /// the same OS thread, but the OS — not `tokio::join!` — decides which
    /// one's subprocess or blocking thread returns first. Without this
    /// signal, `CreateBranch` can win that race, land its ref before the
    /// commit's plan is re-checked, and move the generation the commit's plan
    /// was built against — its `enforce_fresh` then correctly refuses with
    /// 409 ("the repository changed") before the request ever reaches the
    /// 400ms hook bound this test exists to observe.
    ///
    /// A `thread_local`, for the same reason as
    /// [`HOOKED_GIT_TIMEOUT_OVERRIDE`]: `#[tokio::test]`'s default
    /// current-thread runtime keeps one test's whole future tree — every
    /// `tokio::join!`ed branch included — on that one OS thread, so scoping
    /// the signal to the thread means one test's wiring can never fire into,
    /// or silently miss, another test running concurrently on a different
    /// thread under `cargo test`'s default parallel-threads-one-process
    /// model. Every test that sets it must stay on that default
    /// current-thread `#[tokio::test]` flavor.
    static LOCK_ACQUIRED_SIGNAL: std::cell::Cell<Option<std::rc::Rc<tokio::sync::Notify>>> =
        const { std::cell::Cell::new(None) };
}

/// Fire this thread's [`LOCK_ACQUIRED_SIGNAL`] if a test installed one, then
/// clear it — a no-op, and compiled out entirely in non-test builds,
/// everywhere `plan_and_execute_in` runs without a listener. Consuming the
/// signal (`Cell::take`) rather than merely reading it means it fires for
/// exactly the one acquisition a test is synchronizing against, not for
/// every later `plan_and_execute_in` call this thread happens to make —
/// including the very `CreateBranch` call this signal unblocks, which itself
/// acquires the guard once it runs.
#[cfg(test)]
fn notify_lock_acquired() {
    if let Some(notify) = LOCK_ACQUIRED_SIGNAL.with(|c| c.take()) {
        notify.notify_one();
    }
}

/// [`run_git`] for the argv shapes that run repository hooks. Same sealed
/// launcher every other spawn in this file goes through
/// ([`crate::git_cmd::git_output_bounded`]) — see that function's doc for
/// what the `SIGKILL` on elapse actually reaches (bwrap, and through the
/// Strict tier's PID namespace, git and the hook beneath it). #72 (M2.19);
/// generalizes [`run_signed_tag`](tag_exec::run_signed_tag)'s
/// [`SIGN_TIMEOUT`](tag_exec::SIGN_TIMEOUT) contract to the commit
/// path, per the M2.19 design doc.
async fn run_git_hooked(
    repo: &Path,
    need: NetworkNeed,
    args: &[&str],
) -> std::io::Result<crate::git_cmd::BoundedOutput> {
    crate::git_cmd::git_output_bounded(repo, args, need, hooked_git_timeout()).await
}

/// The uniform 500 for a git binary that couldn't be spawned, with the same
/// per-endpoint log line the handlers printed.
///
/// Generic over the reason since D5 (#66, Task 19): it takes the executors'
/// `std::io::Error` exactly as before, and also
/// [`ExecUnavailable`](crate::git_cmd::ExecUnavailable) from the gate sites,
/// so **one** response shape covers every "git could not run" in the server.
/// That single shape is what makes it distinguishable from a refusal: the
/// gates that used to answer 400 ("no such branch", "not a valid object name")
/// on this input now answer 500 here, and nothing else in the planner returns
/// a 500 for a repository-state reason.
pub(crate) fn couldnt_run<E: std::fmt::Display + ?Sized>(
    endpoint: &str,
    e: &E,
) -> (StatusCode, String) {
    eprintln!("git-vista: {endpoint} couldn't run git: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Couldn't run git: {e}"),
    )
}

/// git's own explanation from stderr, or `fallback` when it said nothing.
fn stderr_or(output: &Output, fallback: &str) -> String {
    let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if msg.is_empty() {
        fallback.to_string()
    } else {
        msg
    }
}

/// git's explanation preferring stderr, falling back to stdout (some notices —
/// an up-to-date merge, "nothing to commit" — go there with a non-zero exit),
/// and only then to `fallback`.
fn stderr_stdout_or(output: &Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        fallback.to_string()
    } else {
        stdout
    }
}

/// Run one git command, mapping failure to git's own explanation (stderr, then
/// stdout, then a generic line) — the undo executions' shared runner, moved
/// from `crate::activity`.
async fn git(repo: &Path, need: NetworkNeed, args: &[&str]) -> Result<(), String> {
    let output = run_git(repo, need, args)
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(stderr_stdout_or(&output, "git failed."))
}

/// Whether the working tree has any change at all (`git status --porcelain`
/// non-empty): staged, unstaged, untracked or conflicted — any of them makes
/// a hard reset unsafe. Moved from `crate::activity`.
async fn worktree_dirty(repo: &Path, need: NetworkNeed) -> Result<bool, String> {
    let output = run_git(repo, need, &["status", "--porcelain"])
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git status failed."));
    }
    Ok(!output.stdout.is_empty())
}

/// The conventional 7-char short id, for labels and log lines.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// The wire spelling of a [`MergeStrategy`], for the one place a human reads
/// it: the activity feed and the terminal message. Deliberately the same word
/// the request body carries (`"merge"` / `"rebase"`), so what a user reads
/// afterwards is what they can type to ask for it again.
fn strategy_word(strategy: MergeStrategy) -> &'static str {
    match strategy {
        MergeStrategy::Merge => "merge",
        MergeStrategy::Rebase => "rebase",
    }
}

/// The parsed seed, if this repo has one. `None` => not a test repo;
/// `Some(Err)` => the seed files exist but are corrupt (refuse to reset).
fn read_seed(repo: &Path) -> Option<Result<Seed, String>> {
    let dir = journal::state_dir(repo)?;
    let refs = std::fs::read_to_string(dir.join("seed-refs")).ok()?;
    let head = std::fs::read_to_string(dir.join("seed-head")).ok()?;
    Some(parse_seed(&refs, &head))
}

// --- #219 (M2.18a): discard tracked-path changes / delete untracked paths --
//
// The first genuinely irreversible operation in this vocabulary:
// `DeleteUntrackedPaths` has no journal-backed undo at all, because an
// untracked path was never in git's object database to begin with. Every
// guard below exists because of that fact — a bug here can delete real,
// uncommitted, unstaged work with zero recovery path, so each guard is
// written to refuse rather than guess whenever it cannot prove a path is
// safe.

/// A requested path's tracked/untracked classification, as `git status`
/// reports it right now — the shape [`verify_path_states`] re-checks
/// immediately before running `git checkout --`/`git clean -f` (#219's race
/// guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// A tracked path with an uncommitted change (staged and/or unstaged, or
    /// a rename) — what [`GitOperation::DiscardTrackedPaths`] expects every
    /// one of its paths to be.
    TrackedDirty,
    /// An untracked path (porcelain `?`) — what
    /// [`GitOperation::DeleteUntrackedPaths`] expects every one of its paths
    /// to be.
    Untracked,
    /// Anything else `git status` reports for the path (ignored, a merge
    /// conflict) or nothing at all (clean, deleted, never existed) — never a
    /// valid target for either operation.
    Other,
}

/// Classify every path a live `git status --porcelain=v2 -z` reports, folded
/// to exactly the tracked/untracked distinction [`verify_path_states`] needs.
/// Reuses `git_vista_protocol`'s already-tested `-z` parser rather than a
/// bespoke one — see that module's doc comment for why `-z` (never the
/// quoted/tab format) is the only shape that survives an arbitrary path
/// losslessly.
///
/// A renamed entry's *new* path is `TrackedDirty` (it is the path that
/// exists on disk right now); its `origin_path` is not inserted at all — the
/// old name no longer names anything a discard/delete could act on.
/// Ignored and conflicted entries are never inserted, so they classify as
/// [`PathKind::Other`] by absence — never a valid target for either
/// operation.
fn classify_path_states(parsed: &git_vista_protocol::ParsedStatus) -> HashMap<String, PathKind> {
    use git_vista_protocol::StatusEntry;
    let mut out = HashMap::new();
    for entry in &parsed.entries {
        match entry {
            StatusEntry::Changed { path, .. } | StatusEntry::Renamed { path, .. } => {
                out.insert(path.clone(), PathKind::TrackedDirty);
            }
            StatusEntry::Untracked { path, .. } => {
                out.insert(path.clone(), PathKind::Untracked);
            }
            StatusEntry::Ignored { .. } | StatusEntry::Conflicted { .. } => {}
        }
    }
    out
}

/// The race guard (#219): re-resolve every requested path against a **fresh**
/// `git status --porcelain=v2 -z`, immediately before running the destructive
/// git command, and refuse — the whole batch, not just the drifted path — if
/// any path's live classification no longer matches what this operation
/// requires.
///
/// This is deliberate redundancy on top of the generic staleness gate
/// (`enforce_fresh`, which already refuses on *any* worktree drift via the
/// whole-repository `status` digest) — the same reasoning
/// `exec_stage_selection` already documents for its own inside-the-lock
/// diff-generation re-check: the generic gate proves *something* moved, this
/// proves *these exact paths* are still what the plan claims, with a refusal
/// that names the path rather than the whole repository. Given the stakes
/// (`DeleteUntrackedPaths` has no undo at all), that redundancy is the point,
/// not an oversight.
///
/// Checking every path before running git at all — rather than looping
/// path-by-path through separate git invocations — is what makes "refuse
/// (not skip, not partially apply)" true by construction: either every path
/// passes and the one `git checkout --`/`git clean -f` call runs over the
/// whole batch, or the whole batch is refused before git ever runs.
async fn verify_path_states(
    repo: &Path,
    need: NetworkNeed,
    paths: &[WorktreePath],
    expect: PathKind,
    op_name: &str,
) -> Result<(), (StatusCode, String)> {
    let output = match run_git(repo, need, &["status", "--porcelain=v2", "-z"]).await {
        Ok(o) => o,
        Err(e) => {
            return Err(couldnt_run(
                op_name,
                &format!("couldn't run git status: {e}"),
            ))
        }
    };
    if !output.status.success() {
        return Err(couldnt_run(
            op_name,
            &"git status failed, so these paths cannot be re-verified before executing",
        ));
    }
    let parsed = git_vista_protocol::parse_porcelain_v2_z(&output.stdout);
    let live = classify_path_states(&parsed);
    for path in paths {
        let actual = live.get(path.as_str()).copied().unwrap_or(PathKind::Other);
        if actual != expect {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "‘{}’ changed while this was pending — refusing rather than \
                     partially applying.",
                    path.as_str()
                ),
            ));
        }
    }
    Ok(())
}

/// The symlink-containment guard (#219): refuse any requested path whose
/// fully resolved location — following every symlinked path component, AND a
/// symlinked final entry, exactly what `std::fs::canonicalize` follows —
/// lands outside the worktree root. Also refuses a path that names a
/// **directory** outright (review finding, both operations): `git status`
/// collapses an entirely-untracked directory to one `?? dir/` entry, so
/// naming that one entry to `DeleteUntrackedPaths` would recursively delete
/// everything nested under it via `git clean -f` (verified: no `-d` flag
/// needed for an *explicitly named* directory, unlike wildcard/recursive
/// traversal) while the response and journal report only the one requested
/// entry — silently understating the blast radius of the one operation in
/// this vocabulary with no undo at all. Refusing directories outright is the
/// conservative fix consistent with this module's "refuse rather than
/// guess" posture, not an attempt to make directory deletion safe; naming
/// individual files remains fully supported. This also resolves the
/// trailing-slash mismatch a live directory entry's porcelain spelling
/// (`dir/`) would otherwise cause against an unslashed request — directories
/// are never a valid target either way now, so the spelling stops mattering.
///
/// Reuses `bin/gv-sandbox/main.rs`'s `resolve_excludes` pattern (canonicalize
/// each path, compare against the canonicalized worktree root, fail closed
/// on any canonicalize error other than `NotFound`) rather than a fresh
/// lexical check — `WorktreePath`'s own `..`-rejection is necessary but not
/// sufficient, because a symlink's target is not spelled in the path string
/// at all (see that newtype's doc comment).
///
/// `NotFound` is deliberately not a refusal here, mirroring `resolve_excludes`
/// exactly: a path that does not exist has nothing to prove an escape about,
/// and [`verify_path_states`] independently refuses a path whose status no
/// longer matches what the operation expects — which a vanished path always
/// does. `std::fs::canonicalize`/`std::fs::symlink_metadata` are blocking
/// filesystem I/O, so this runs on a blocking thread (the same offload
/// discipline as every other synchronous read in this module — see the
/// "blocking-work offload" section above).
async fn symlink_containment_guard(
    repo: &Path,
    paths: &[WorktreePath],
    op_name: &'static str,
) -> Result<(), (StatusCode, String)> {
    let repo_owned = repo.to_path_buf();
    let rels: Vec<String> = paths.iter().map(|p| p.as_str().to_string()).collect();
    let result = tokio::task::spawn_blocking(move || -> Result<(), (StatusCode, String)> {
        let repo_canon = std::fs::canonicalize(&repo_owned).map_err(|e| {
            couldnt_run(op_name, &format!("couldn't resolve the worktree root: {e}"))
        })?;
        for rel in &rels {
            let joined = repo_owned.join(rel);
            match std::fs::canonicalize(&joined) {
                Ok(resolved) => {
                    if !resolved.starts_with(&repo_canon) {
                        return Err((
                            StatusCode::CONFLICT,
                            format!(
                                "‘{rel}’ resolves outside the worktree through a symlink — \
                                 refusing."
                            ),
                        ));
                    }
                    // Real, in-worktree directory: refuse (see doc comment).
                    // `symlink_metadata` (not `metadata`) on the RESOLVED
                    // path so a symlink-to-a-directory is judged by what it
                    // actually points at, already proven in-bounds above.
                    let is_dir = std::fs::symlink_metadata(&resolved)
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                    if is_dir {
                        return Err((
                            StatusCode::CONFLICT,
                            format!(
                                "‘{rel}’ names a directory — refusing rather than deleting \
                                 its contents recursively under one requested entry; name \
                                 individual files instead."
                            ),
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Nothing to prove an escape about; `verify_path_states`
                    // independently refuses a vanished path. See this
                    // function's own doc comment.
                }
                Err(e) => {
                    return Err(couldnt_run(
                        op_name,
                        &format!("couldn't resolve ‘{rel}’: {e}"),
                    ));
                }
            }
        }
        Ok(())
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(join_err) => Err(couldnt_run(
            op_name,
            &format!("containment check task panicked: {join_err}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests — the #145 staleness contract, on a real throwaway repository, and
// the #146 end-to-end contract suite (build → validate → execute for every
// operation kind; the single-funnel proof; refusals that protect).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod contract_suite;

/// git's `--progress` records, parsed once for both directions (M2.20c #229
/// built it inside `fetch`; M2.20e #231 moved it here when push needed the same
/// parser). One owner, because nothing fails loudly when a progress bar is
/// subtly wrong — two copies would drift and no test would notice.
mod transfer;

/// M2.20c (#229): the fetch executor. Its own file rather than another
/// `exec_*` in this one, because a fetch brings three concerns no other
/// operation has — live progress parsing, cancellation, and a failure
/// taxonomy — and they belong together.
mod fetch;

/// M2.20d (#230, ADR 0044): the pull executor. Its own file for the same
/// reason `fetch` has one — a pull composes two halves with different failure
/// vocabularies and a conflict-abort story neither half has on its own — and
/// it deliberately owns *no* spawn of its own: the fetch comes from
/// [`fetch::run_fetch`] and the integration from `exec_merge`/`exec_rebase`.
mod pull;

/// M2.20e (#231, ADR 0045): the push executor. Its own file for the reason
/// `fetch` and `pull` have one, plus a sharper one: it is the only operation
/// here that can make *another party's* commits unreachable, and the code that
/// decides whether it may is worth reading in one piece.
mod push;
mod stash;

/// M2.21f (#240, closes a slice of #74): the two remote-reaching tag
/// executors — `git push <remote> refs/tags/<name>` and
/// `git push <remote> --delete refs/tags/<name>`. Own file for the reason
/// `push` has one: each runs through the same Network-tier chokepoint
/// (`git_cmd::git_streamed_for`) but, unlike a branch push, diffs no local
/// ref before/after (D5 — see `GitOperation::DeleteRemoteTag`'s doc in
/// plan.rs), which is different enough from `push`'s remote-tracking-ref
/// bookkeeping to earn its own file rather than a third shape crammed in.
mod remote_tags;

/// The branch-lifecycle executors (create/checkout/merge/delete/rebase/
/// restore/reset) and `IntegrationCaller`, shared with `pull`'s integration
/// half; see the module doc.
mod branch_exec;

/// The commit-writing executors (commit on HEAD, empty commit on a branch,
/// amend) and the failure classifiers only they use — see the module's own
/// doc for why they move as one piece.
mod commit_exec;

/// The two local tag executors and the signed-tag execution path with its
/// failure taxonomy — see the module's own doc, and the source-scanning test
/// that anchors inside it.
mod tag_exec;

/// The conflict-resolution executors — the only ones whose write leg is a
/// worktree file write rather than a git mutation; see the module doc.
mod conflict_exec;

/// The index-staging executors — stage-all, unstage-all, and the patch-fed
/// line/hunk selection path; see the module doc.
mod staging_exec;

/// The sequencer executors — cherry-pick, two-step revert, and the
/// continue/skip/abort verbs; see the module doc.
mod sequence_exec;

/// The worktree-destroying executors — reset-test-repo, discard tracked,
/// delete untracked — and their observation-based outcome reports; see the
/// module doc.
mod worktree_exec;

/// `POST /api/amend-commit`'s handler-side 400 constructor, re-exported so
/// `handlers::commit`'s `planner::amend_refusal` path is unchanged by the
/// commit executors moving into [`commit_exec`].
pub(crate) use commit_exec::amend_refusal;

/// `POST /api/fetch`'s error-body constructor, re-exported so the handler's
/// own request-shape refusals carry the same contract the executor's do.
pub(crate) use fetch::error_body as fetch_error_body;

/// `POST /api/pull`'s error-body constructor, re-exported for the same reason
/// [`fetch_error_body`] is: the handler's request-shape refusals — above all
/// the missing-`strategy` 400 that is #230's whole point — must parse as the
/// endpoint's one error type, exactly like the executor's do.
pub(crate) use pull::error_body as pull_error_body;

/// Whether `branch` has a recorded upstream (`<branch>@{upstream}`), exposed
/// to callers outside `planner` (#233). `push::upstream_of` itself stays
/// `pub(super)` — its own doc comment explains why (only `push_suite`'s test
/// needs it) — and this wrapper exists because that function returns [`Obs`],
/// which is private to this module: a `pub(crate)` function may not return a
/// private type (E0446), and widening `Obs` itself would give it a much wider
/// audience than one read endpoint needs. So this collapses the three-state
/// read into the `Result<Option<_>, ExecUnavailable>` shape
/// `/api/rebase-status`'s own reads already use: `Known`/`Absent` are both
/// real observations (`Some`/`None`), and `Unknown` — git could not be run —
/// becomes `Err`, so a failed read fails the whole response instead of
/// reporting a silent `false`.
pub(crate) async fn upstream_of(
    repo: &Path,
    branch: &BranchName,
) -> Result<Option<String>, ExecUnavailable> {
    match push::upstream_of(repo, branch).await {
        Obs::Known(upstream) => Ok(Some(upstream)),
        Obs::Absent => Ok(None),
        Obs::Unknown => Err(ExecUnavailable::new(format!(
            "couldn't resolve the upstream of '{}'",
            branch.as_str()
        ))),
    }
}

/// Whether cancelling this operation can actually stop it (M2.20c, #229).
///
/// **This is a claim about [`execute`], not a wish.** An arm answering `true`
/// promises that the executor it dispatches to takes
/// [`crate::operations::cancel_signal`] and hands it to the process it
/// spawns; anything else must answer `false`, because
/// `POST /api/operations/{id}/cancel` reports its answer to an operator, and
/// "cancelling…" for an operation nothing will ever stop is worse than a
/// plain refusal.
///
/// No wildcard arm, on purpose: a new `GitOperation` variant fails to compile
/// here until someone states which side it is on. The contract suite pins
/// the `true` set to an exact census, so widening it is a visible edit rather
/// than a side effect.
///
/// `FetchRemote`, `PullBranch` (M2.20d, #230) and `PushBranch` (M2.20e, #231)
/// qualify — the three operations that move objects over a transport. Every
/// other executor runs a git command that finishes in milliseconds; the
/// machinery would be real but the window to use it would not, and a cancel
/// endpoint that usually arrives too late teaches users to distrust it.
///
/// # What `true` means for a pull, exactly
///
/// A pull is a fetch plus an integration, and only the first half is long.
/// `planner::pull` hands the latch to the same streaming spawn `planner::fetch`
/// uses, so a cancel during the transfer SIGKILLs the child, and it reads the
/// latch **once more** between the halves so a cancel that lands while the
/// fetch is finishing stops the integration from ever starting. A cancel that
/// arrives *during* `git merge`/`git rebase` is not honoured — those are
/// millisecond-scale local commands and interrupting one is how a repository
/// is left half-integrated. That is a narrower promise than fetch's, and it is
/// the honest one: the cancellable window is where the time actually goes.
///
/// # And what it means for a push
///
/// Narrower still, and the difference is worth stating because it is not about
/// this server's diligence. `planner::push` hands the latch to the same spawn,
/// so a cancel kills `git push` promptly — but a push's effect is on a *remote*,
/// and git records `refs/remotes/<remote>/<branch>` only after that remote has
/// reported the update accepted. A cancel landing in between stops this
/// repository from learning about a change that already happened elsewhere. So
/// the promise is "the transfer stops", never "nothing was published", and
/// `push::cancelled_response` says exactly that rather than the reassuring
/// version.
pub(crate) fn honours_cancellation(op: &GitOperation) -> bool {
    match op {
        GitOperation::FetchRemote { .. }
        | GitOperation::PullBranch { .. }
        | GitOperation::PushBranch { .. } => true,
        // M3.24 (#77): stash verbs are local and finish in milliseconds —
        // there is no transfer to interrupt, so cancellation has nothing to
        // honour. M4.31 (#84)'s resolve is two short local git calls, with no
        // window in which a cancel could arrive and mean anything.
        GitOperation::PushStash { .. }
        | GitOperation::ApplyStash { .. }
        | GitOperation::PopStash { .. }
        | GitOperation::BranchFromStash { .. }
        | GitOperation::DropStash { .. }
        | GitOperation::ResolveConflict { .. }
        // M4.31c (#432): a worktree write plus one `git add`, both local and
        // millisecond-scale — no transfer, nothing for a cancel to interrupt.
        | GitOperation::ResolveConflictContent { .. }
        | GitOperation::CreateBranch { .. }
        | GitOperation::CommitOnHead { .. }
        | GitOperation::EmptyCommitOnBranch { .. }
        | GitOperation::AmendCommit { .. }
        | GitOperation::StageAll
        | GitOperation::UnstageAll
        | GitOperation::StageSelection { .. }
        | GitOperation::CheckoutBranch { .. }
        | GitOperation::MergeBranch { .. }
        | GitOperation::DeleteBranch { .. }
        | GitOperation::ForceDeleteBranch { .. }
        | GitOperation::RebaseOntoBase { .. }
        | GitOperation::RestoreBranch { .. }
        | GitOperation::ResetBranch { .. }
        | GitOperation::RevertCommit { .. }
        | GitOperation::RevertMerge { .. }
        | GitOperation::CherryPick { .. }
        | GitOperation::CherryPickMerge { .. }
        | GitOperation::SequenceContinue
        | GitOperation::SequenceSkip
        | GitOperation::SequenceAbort
        | GitOperation::ResetTestRepo
        | GitOperation::DiscardTrackedPaths { .. }
        | GitOperation::DeleteUntrackedPaths { .. }
        | GitOperation::CreateTag { .. }
        | GitOperation::DeleteLocalTag { .. }
        | GitOperation::DeleteRemoteTag { .. }
        | GitOperation::PushTag { .. } => false,
    }
}

#[cfg(test)]
mod coordination_suite;

// The #61 lifecycle suite: identity, replay under one key, and survival of a
// disconnected client.
#[cfg(test)]
mod lifecycle_suite;

// M2.20c (#229): the fetch slice's behavioural tests — real spawns, real
// cancellation, the dropped-connection replay, and redaction on the live path.
#[cfg(test)]
mod fetch_suite;

// M2.20d (#230): the pull slice's behavioural tests — the merge-vs-rebase
// history difference against one diverged fixture, the conflict abort, the
// cancel that stops the integration starting, and the journal.
#[cfg(test)]
mod pull_suite;

// M2.20e (#231): the push slice's behavioural tests — a real remote, the lease
// refused in both of its two distinct ways, the upstream actually recorded,
// live progress, and a cancel that stops the push.
#[cfg(test)]
mod push_suite;

// The remote-target boundary (#229 follow-up, ADR 0047): a real listener that
// must never be connected to, its paired positive control, and the
// unconfigured-remote refusal — for fetch and for pull, which share the
// machinery verbatim.
#[cfg(test)]
mod remote_boundary_suite;

// #72 (M2.19): a hook that never returns cannot hang a commit/amend request
// forever, and cannot hold the coordinator guard forever either.
#[cfg(test)]
mod hook_timeout_suite;

// M4.32 (#85): which advisories a force-with-lease push earns, and that
// "could not determine the default branch" never reads as "not it".
#[cfg(test)]
mod advisory_suite;

// M4.31e (#431): conflict resolution survives a reconnect and a crash.
#[cfg(test)]
mod reconnect_suite;
// #327 defect B: `git revert`'s failure classification.
#[cfg(test)]
mod revert_suite;

// M2.21d (#238) / M2.21e (#239, ADR 0048): the tag argv shape and the
// signed-tag execution path's failure classification.
#[cfg(test)]
mod tag_signing_suite;

// The #145 staleness contract: admission-hash tamper detection, generation
// drift, plan expiry, precondition drift, and the underlying
// Observed/enforce_fresh machinery (D5, #66 Task 19).
#[cfg(test)]
mod staleness_suite;

// #214 (M2.17c): line-level and hunk-level staging/unstaging.
#[cfg(test)]
mod hunk_staging_suite;

// M2.19a (#222) / M2.19b (#223) / #72 (M2.19): `AmendCommit`'s shape and the
// pure amend/commit failure classifiers.
#[cfg(test)]
mod commit_classification_suite;

// M2.20a (#227) / M2.21a (#235) / M2.21f (#240): the plan-building shape of
// every remote-reaching operation.
#[cfg(test)]
mod remote_operation_shape_suite;
