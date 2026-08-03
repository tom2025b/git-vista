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
use git_vista_core::seed::{parse_seed, reset_plan, Seed};
use git_vista_protocol::{
    AmendCommitError, AmendCommitSuccess, AmendFailureKind, BranchName, CommitMessage, CommitOid,
    ForcePublish, GenerationToken, GitOperation, IdempotencyKey, MergeStrategy, OperationHash,
    OperationStage, Plan, Precondition, RecoveryStrategy, RefChange, RefName, RefState, RemoteName,
    RepositoryToken, RiskLevel, TagName, UnixSeconds, WorktreePath, WorktreeToken,
    IDEMPOTENCY_HEADER,
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
    plan_and_execute_tracked(key, repo, repo_id, selection_tokens(), op).await
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
    op: GitOperation,
) -> (StatusCode, String) {
    let hash = operation_hash(&op);
    let (repository, worktree) = tokens.clone();

    let (handle, record) = match crate::operations::admit(&key, &op, &hash, repository, worktree) {
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

            let (status, message) = plan_and_execute_in(&repo, repo_id, tokens, op).await;
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
            if let Some(recovery) = &terminal.recovery {
                crate::durable::write_recovery_ref(&repo, &terminal.id, recovery).await;
            }

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
    crate::operations::stage(OperationStage::Executing);
    execute(repo, plan, observed).await
    // `_guard` drops here: the next queued mutation of this repository proceeds.
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
///   built-stale: it flows to the executor's own legacy refusal instead of
///   `enforce_fresh`'s — from the submitter's seat the two cases are
///   genuinely indistinguishable, and both fail closed. Not just prose: the
///   contract suite's two `review_window_*_drift_fails_closed_*` tests prove
///   the refusal (and its byte-identity with the never-held case) for both
///   generation-invisible preconditions, and
///   `a_generation_invisible_break_while_queued_is_refused_by_the_gates_live_recheck`
///   proves the re-derivation itself is load-bearing (emptying it passed the
///   whole suite before that test existed).
#[cfg_attr(not(test), allow(dead_code))] // routed by #249; contract-suite-only until then
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
    crate::operations::stage(OperationStage::Executing);
    execute(repo, plan, observed).await
    // `_guard` drops here, exactly as in `plan_and_execute_in`.
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
#[cfg_attr(not(test), allow(dead_code))] // routed by #249; contract-suite-only until then
async fn observe_for_submission(repo: &Path, plan: &Plan) -> Observed {
    let mut observed = observe_operation(repo, &plan.operation).await;
    observed.held_at_build = held_now(repo, &plan.preconditions, &observed).await;
    observed
}

/// The current selection's opaque id tokens. In degraded mode (the served path
/// wouldn't classify as a repository, so it has no catalog entry) a fixed
/// placeholder keeps the plan well-formed; execution then fails with git's own
/// error exactly as the un-migrated handlers did.
fn selection_tokens() -> (RepositoryToken, WorktreeToken) {
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
///     it with the exact wording it always had.
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
        }
    }
    Ok(())
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
        GitOperation::CreateBranch { name, at } => exec_create_branch(repo, need, &name, &at).await,
        GitOperation::CommitOnHead {
            message,
            allow_empty,
        } => exec_commit_on_head(repo, need, &message, allow_empty, &observed).await,
        GitOperation::EmptyCommitOnBranch {
            branch,
            message,
            expected_tip,
        } => exec_empty_commit_on_branch(repo, need, &branch, &message, &expected_tip).await,
        GitOperation::StageAll => exec_stage_all(repo, need).await,
        GitOperation::UnstageAll => exec_unstage_all(repo, need).await,
        GitOperation::CheckoutBranch { branch } => {
            exec_checkout(repo, need, &branch, &observed).await
        }
        GitOperation::MergeBranch { branch } => {
            exec_merge(
                repo,
                need,
                &RefName::from(&branch),
                &observed,
                IntegrationCaller::Direct,
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
            exec_delete(repo, need, &branch, &observed, false).await
        }
        GitOperation::ForceDeleteBranch { branch } => {
            exec_delete(repo, need, &branch, &observed, true).await
        }
        GitOperation::RebaseOntoBase { base } => {
            exec_rebase(repo, need, &base, &observed, IntegrationCaller::Direct).await
        }
        GitOperation::RestoreBranch { name, tip } => {
            exec_restore_branch(repo, need, &name, &tip).await
        }
        GitOperation::ResetBranch {
            branch,
            to,
            expected_tip,
        } => exec_reset_branch(repo, need, &branch, &to, &expected_tip, &observed).await,
        GitOperation::RevertCommit { commit } => exec_revert(repo, need, &commit, &observed).await,
        GitOperation::ResetTestRepo => exec_reset_test_repo(repo, need).await,
        GitOperation::StageSelection {
            direction,
            expected_diff_generation,
            patch,
            whole_files,
        } => {
            exec_stage_selection(
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
            exec_discard_tracked_paths(repo, need, &paths).await
        }
        GitOperation::DeleteUntrackedPaths { paths } => {
            exec_delete_untracked_paths(repo, need, &paths).await
        }
        // M2.19a (#222) shipped the typed contract; M2.19b (#223, ADR 0040)
        // wired this execution — `handlers::commit::amend_commit` builds the
        // operation from `POST /api/amend-commit`.
        GitOperation::AmendCommit {
            message,
            expected_tip,
            allow_empty,
        } => exec_amend_commit(repo, need, &message, &expected_tip, allow_empty, &observed).await,
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
        // M2.21a (#235, ADR 0041) ships the typed tag contract only — the
        // same staging as fetch/pull above. Execution belongs to the later
        // M2.21 slices (#74): create/delete are their own slices, and the
        // two remote-reaching operations are the first tag code that would
        // open a socket with credentials on it, which earns a review of its
        // own. These arms exist because this match must stay exhaustive over
        // the closed vocabulary (#142); reached, they refuse rather than
        // no-op silently or improvise a git command.
        GitOperation::CreateTag { .. } => (
            StatusCode::NOT_IMPLEMENTED,
            "Creating a tag is not yet wired for execution (M2.21, tracked \
             under #74) — this plan's contract exists, but nothing executed it."
                .to_string(),
        ),
        GitOperation::DeleteLocalTag { .. } => (
            StatusCode::NOT_IMPLEMENTED,
            "Deleting a local tag is not yet wired for execution (M2.21, \
             tracked under #74) — this plan's contract exists, but nothing \
             executed it."
                .to_string(),
        ),
        GitOperation::DeleteRemoteTag { .. } => (
            StatusCode::NOT_IMPLEMENTED,
            "Deleting a remote tag is not yet wired for execution (M2.21, \
             tracked under #74) — this plan's contract exists, but nothing \
             executed it."
                .to_string(),
        ),
        GitOperation::PushTag { .. } => (
            StatusCode::NOT_IMPLEMENTED,
            "Pushing a tag is not yet wired for execution (M2.21, tracked \
             under #74) — this plan's contract exists, but nothing executed it."
                .to_string(),
        ),
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

// --- per-operation executions (moved verbatim from the handlers) ------------

/// `git branch <name> <at>` (`/api/branch`). B3 posture: git validates the
/// name, refuses a duplicate, and its stderr is forwarded verbatim on failure.
async fn exec_create_branch(
    repo: &Path,
    need: NetworkNeed,
    name: &BranchName,
    at: &CommitOid,
) -> (StatusCode, String) {
    let output = match run_git(repo, need, &["branch", name.as_str(), at.as_str()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/branch", &e),
    };
    if output.status.success() {
        println!("[/api/branch] created branch '{name}' at {at}");
        // Journal the creation with the resolved tip (the user may have given
        // an abbreviated or symbolic start point).
        let tip = Obs::from_read(rev_parse(repo, name.as_str()).await);
        journal_app_event(
            repo,
            ActivityKind::BranchCreated,
            Some(name.as_str().to_string()),
            Obs::Absent, // a created branch has no previous tip, by definition
            tip,
            format!("created branch ‘{name}’"),
        )
        .await;
        (StatusCode::OK, format!("Created branch '{name}'."))
    } else {
        let msg = stderr_or(&output, "git branch failed.");
        eprintln!("git-vista: /api/branch failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git commit [--allow-empty] -m <message>` on HEAD (`/api/commit`).
async fn exec_commit_on_head(
    repo: &Path,
    need: NetworkNeed,
    message: &CommitMessage,
    allow_empty: bool,
    observed: &Observed,
) -> (StatusCode, String) {
    // The pre-commit tip, captured for the journal before git moves anything.
    // `Obs::Absent` on an unborn HEAD (first commit) — journaled as a
    // creation-like event with no old state, which is exactly what it is.
    let old = observed.head_tip.clone();

    let mut args = vec!["commit"];
    if allow_empty {
        args.push("--allow-empty");
    }
    args.push("-m");
    args.push(message.as_str());

    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/commit", &e),
    };
    if output.status.success() {
        println!("[/api/commit] created commit (allow_empty={allow_empty})");
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        // The branch the commit landed on; "HEAD" when detached.
        let branch = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        let summary = message
            .as_str()
            .lines()
            .next()
            .unwrap_or(message.as_str())
            .to_string();
        journal_app_event(repo, ActivityKind::Commit, Some(branch), old, new, summary).await;
        (StatusCode::OK, "Created commit.".to_string())
    } else {
        // "nothing to commit, working tree clean" goes to *stdout* with a
        // non-zero exit — prefer stderr, fall back to stdout.
        let msg = stderr_stdout_or(&output, "git commit failed.");
        eprintln!("git-vista: /api/commit failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// The branch-stub path of `/api/commit`: `git commit-tree` on the branch
/// tip's own tree (an empty commit by construction), then a compare-and-swap
/// `git update-ref` from `expected_tip`. HEAD, index and working tree are
/// untouched throughout.
async fn exec_empty_commit_on_branch(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    message: &CommitMessage,
    expected_tip: &CommitOid,
) -> (StatusCode, String) {
    let refname = format!("refs/heads/{branch}");
    let tip = expected_tip.as_str();

    // Write the commit object: the parent's own tree, so nothing changes.
    let output = match run_git(
        repo,
        need,
        &[
            "commit-tree",
            &format!("{tip}^{{tree}}"),
            "-p",
            tip,
            "-m",
            message.as_str(),
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git commit-tree: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git commit-tree failed.");
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let new = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "git commit-tree returned no commit id.".to_string(),
        );
    }

    // Advance the ref — compare-and-swap on the expected tip, with a reflog
    // line in git's own "commit (empty): …" shape so the activity feed reads
    // it like any other commit.
    let summary = message
        .as_str()
        .lines()
        .next()
        .unwrap_or(message.as_str())
        .to_string();
    let output = match run_git(
        repo,
        need,
        &[
            "update-ref",
            "-m",
            &format!("commit (empty): {summary}"),
            refname.as_str(),
            new.as_str(),
            tip,
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git update-ref: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("‘{branch}’ has moved since this was offered — refresh and try again.")
        } else {
            msg
        };
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::CONFLICT, msg);
    }

    println!("[/api/commit] created empty commit on '{branch}' ({new})");
    journal_app_event(
        repo,
        ActivityKind::Commit,
        Some(branch.as_str().to_string()),
        // Both known first-hand: `tip` is the CAS pin this operation was built
        // on, `new` is `commit-tree`'s own stdout. Neither is a read that
        // could have come back unknown.
        Obs::Known(tip.to_string()),
        Obs::Known(new),
        summary,
    )
    .await;
    (StatusCode::OK, "Created commit.".to_string())
}

/// `git commit --amend [--allow-empty] -m <message>` (`/api/amend-commit`,
/// M2.19b #223, ADR 0040): rewrite the checked-out branch's tip commit in
/// place — the first history-rewriting execution in this vocabulary, so every
/// step here is defensive by design.
///
/// The order of operations is deliberate:
///
///  1. **Detached-HEAD refusal.** Amend targets the checked-out *branch* (the
///     variant's doc comment: there is no "amend some other commit"
///     primitive), and the plan's `ResetRef` recovery needs a branch ref to
///     reset — on detached HEAD `shape` degrades recovery to `NotNeeded`,
///     which would be a lie the moment a rewrite actually happened. Refuse
///     rather than run with no recovery story.
///  2. **The compare-and-swap.** The executor-level guard, mirroring
///     `exec_empty_commit_on_branch`'s CAS and `exec_reset_branch`'s: the tip
///     observed at plan-build time must equal the operation's `expected_tip`.
///     This is the leg that catches a request whose `expected_tip` was stale
///     *from the start* — `enforce_fresh` re-verifies only preconditions that
///     held at build time, so a failed-at-build `RefAt` flows through to
///     exactly this refusal (a 400: the client's picture of the repository is
///     wrong, which is a request problem, not a race — races are the gate's
///     409s). D5: an `Absent` observation (unborn HEAD — nothing to amend)
///     refuses here too, and an `Unknown` one never reaches this function at
///     all (`enforce_fresh` refuses unreadable observations with a 500).
///  3. **The published-history flag**, read *before* the rewrite while the
///     amended-away commit is still the tip. Advisory, never blocking — the
///     user may be amending published history knowingly, and the pre-flight
///     ceremony belongs to the client (M2.19d); ADR 0040 records why.
///  4. `git commit --amend`, through the sealed chokepoint like every other
///     mutation (hooks — when the sandbox's `HookMode` runs them at all —
///     execute as children of this one spawn; there is no separate hook
///     path to bypass, which `argv_boundary`'s spawn-site census pins).
///  5. On failure, the typed classification ([`classify_amend_failure`]);
///     on success, the journal event (old tip → new tip, `ActivityKind::Amend`)
///     whose oid pair is what makes the amend visible in `/api/activity` and
///     undoable via its reset-back hint. The durable `ResetRef` recovery ref
///     is not written here: the tracked pipeline writes it for every
///     operation from the plan's own `recovery` (see
///     `plan_and_execute_tracked`), which `shape` pins to
///     `ResetRef { <branch>, expected_tip }` for this operation.
async fn exec_amend_commit(
    repo: &Path,
    need: NetworkNeed,
    message: &CommitMessage,
    expected_tip: &CommitOid,
    allow_empty: bool,
    observed: &Observed,
) -> (StatusCode, String) {
    let Some(branch) = observed.head_branch.clone() else {
        return amend_refusal(
            AmendFailureKind::Other,
            "Amending requires a checked-out branch — HEAD is detached. \
             Check out a branch and try again.",
        );
    };
    match observed.head_tip.known().map(String::as_str) {
        Some(tip) if tip == expected_tip.as_str() => {}
        Some(_) => {
            return amend_refusal(
                AmendFailureKind::StaleTip,
                "HEAD has moved since this amend was reviewed — refresh and try again.",
            )
        }
        None => {
            return amend_refusal(
                AmendFailureKind::StaleTip,
                "There is no commit here to amend — refresh and try again.",
            )
        }
    }

    let published = amended_commit_is_published(repo, expected_tip).await;

    let mut args = vec!["commit", "--amend"];
    if allow_empty {
        args.push("--allow-empty");
    }
    args.push("-m");
    args.push(message.as_str());
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/amend-commit", &e),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let kind = classify_amend_failure(
            &stderr,
            signing_requested(repo, need).await,
            rejectable_hook_present(repo, need).await,
        );
        // Amend shares `git commit`'s quirk: some refusals go to stdout.
        let msg = stderr_stdout_or(&output, "git commit --amend failed.");
        return amend_refusal(kind, &msg);
    }

    let new = Obs::from_read(rev_parse(repo, "HEAD").await);
    let summary = message
        .as_str()
        .lines()
        .next()
        .unwrap_or(message.as_str())
        .to_string();
    println!(
        "[/api/amend-commit] amended tip of '{branch}' ({} → {})",
        short(expected_tip.as_str()),
        new.known().map(|o| short(o)).unwrap_or("unknown"),
    );
    journal_app_event(
        repo,
        ActivityKind::Amend,
        Some(branch),
        // The pre-amend tip is the CAS pin this operation was built on — an
        // exact value, not a read. The new tip is a post-mutation read that
        // can honestly be `Unknown` (D5), in which case the journal notes it
        // and no undo is offered.
        Obs::Known(expected_tip.as_str().to_string()),
        new.clone(),
        summary,
    )
    .await;
    let body = AmendCommitSuccess {
        message: "Amended commit.".to_string(),
        old_tip: expected_tip.as_str().to_string(),
        new_tip: new.known().cloned(),
        amended_published_commit: published,
    };
    (
        StatusCode::OK,
        serde_json::to_string(&body).expect("AmendCommitSuccess serialization cannot fail"),
    )
}

/// The one constructor for `/api/amend-commit`'s 400 contract: every refusal
/// body from that endpoint — the handler's request-shape rejections and the
/// executor's classified failures alike — is an [`AmendCommitError`] built
/// here, so a client can always parse a 400 from this route as that one type.
pub(crate) fn amend_refusal(kind: AmendFailureKind, message: &str) -> (StatusCode, String) {
    eprintln!("git-vista: /api/amend-commit refused ({kind:?}): {message}");
    (
        StatusCode::BAD_REQUEST,
        serde_json::to_string(&AmendCommitError {
            kind,
            message: message.to_string(),
        })
        .expect("AmendCommitError serialization cannot fail"),
    )
}

/// Whether `tip` is reachable from any remote-tracking ref — the
/// published-history guard's question (#223). Three-state on purpose:
/// `Some(true)`/`Some(false)` are the walk's real answer, `None` is "the walk
/// failed", which the response must not collapse into `false` (a
/// shared-history warning that silently reads unknown as unpublished fails
/// open — the exact `Obs` lesson, applied to the wire).
///
/// Reuses [`git_vista_git::remote_membership`] — the shared remote walk
/// `handlers::read` already uses twice for its own on-remote flags — rather
/// than the capped [`git_vista_git::read_remote_commits`] the activity feed
/// uses. The issue named the capped helper, but the cap is wrong for *this*
/// question: `read_remote_commits` keeps only the newest `HISTORY_LIMIT`
/// remote commits, and the tip being amended is routinely deep below that in
/// remote terms — this repository's own workflow (branches preserved forever
/// after merging) makes "amend the tip of a branch merged into origin/main
/// long ago" an ordinary case, and a capped walk would answer `false` for
/// exactly the shared commit the flag exists to warn about. A false negative
/// is the dangerous direction for a defense-in-depth flag, so the exact,
/// stop-when-found membership walk is the right shared helper; nothing is
/// re-implemented (ADR 0040 records the substitution).
async fn amended_commit_is_published(repo: &Path, tip: &CommitOid) -> Option<bool> {
    let repo = repo.to_path_buf();
    let requested: std::collections::HashSet<git_vista_core::model::Oid> =
        std::iter::once(git_vista_core::model::Oid(tip.as_str().to_string())).collect();
    tokio::task::spawn_blocking(
        move || match git_vista_git::remote_membership(&repo, &requested) {
            Ok(found) => Some(!found.is_empty()),
            Err(e) => {
                eprintln!(
                    "git-vista: /api/amend-commit couldn't check remote reachability \
                     (reporting it as unknown, not as unpublished): {e}"
                );
                None
            }
        },
    )
    .await
    .unwrap_or(None)
}

/// Whether this repository's own config asks for commit signing
/// (`commit.gpgsign`, normalized through `--type=bool`). A probe for
/// [`classify_amend_failure`]'s ssh-format leg — locale-independent, unlike
/// stderr. Unset, unreadable, or git-couldn't-run all answer `false`: a
/// classification probe must never invent a claim it could not read.
async fn signing_requested(repo: &Path, need: NetworkNeed) -> bool {
    match run_git(
        repo,
        need,
        &["config", "--type=bool", "--get", "commit.gpgsign"],
    )
    .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "true",
        _ => false,
    }
}

/// Whether a hook that can reject `git commit --amend` (`pre-commit`,
/// `prepare-commit-msg`, `commit-msg`) exists — executable — in the
/// **effective** hooks directory.
///
/// "Effective" is the load-bearing word: the directory is asked of git
/// *through the same sealed chokepoint the amend itself ran through*
/// (`rev-parse --git-path hooks`), so when the sandbox policy is
/// `HookMode::Blocked` — which injects `-c core.hooksPath=<server-owned
/// empty dir>` into every spawn, shim and unsandboxed tier alike — this
/// probe sees that same empty directory and answers `false`. A repository
/// whose hooks cannot run can never have a failure classified as a hook
/// rejection, with no separate policy plumbing to drift out of sync.
async fn rejectable_hook_present(repo: &Path, need: NetworkNeed) -> bool {
    let hooks_dir = match run_git(repo, need, &["rev-parse", "--git-path", "hooks"]).await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    if hooks_dir.is_empty() {
        return false;
    }
    // `--git-path` answers relative to the repository when it answers
    // relatively at all (the spawn runs `git -C <repo>`).
    let dir = {
        let p = PathBuf::from(&hooks_dir);
        if p.is_absolute() {
            p
        } else {
            repo.join(p)
        }
    };
    ["pre-commit", "prepare-commit-msg", "commit-msg"]
        .iter()
        .any(|hook| {
            std::fs::metadata(dir.join(hook))
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                })
                .unwrap_or(false)
        })
}

/// Classify a failed `git commit --amend` into the typed
/// [`AmendFailureKind`] the wire carries (#223), so the frontend never
/// regex-sniffs stderr itself. Pure over its three inputs — the async probes
/// live in the callers — so every branch is unit-testable without a spawn.
///
/// What each leg rests on, and how it degrades (all verified empirically
/// against git 2.43 — see the paired tests):
///
///  * **Signing, gpg format:** git's canonical `gpg failed to sign the data`
///    line. Exact under the C/English locales; under a translated locale the
///    body text differs and this leg falls through — degrading toward
///    `Other`, which promises nothing (safe).
///  * **Signing, ssh format:** the leading error line names the key path and
///    varies, but `fatal: failed to write commit object` is common to every
///    failed-signer shape — meaningful as a signing signal only when the
///    repo's config actually requested signing, which is what the
///    locale-independent `signing_requested` probe supplies. Without that
///    guard, a genuine object-store write failure would masquerade as a
///    signing problem.
///  * **Hook rejection:** git prints **nothing of its own** when a hook
///    rejects a commit — a silently-failing `pre-commit` yields exit 1 with
///    empty stderr *and* stdout — so there is no positive marker to match,
///    only an inference: a rejectable hook exists (the effective-hooks-dir
///    probe), and stderr carries no `fatal:` (the prefix is hardcoded in
///    git's `die()`, never localized, so this guard is locale-proof) and not
///    the one known non-fatal refusal this argv can produce (the
///    would-become-empty advice, "You asked to amend the most recent
///    commit…"). Known residuals, accepted and safe-directional: a hook that
///    itself prints `fatal:` classifies as `Other` (right message, weaker
///    kind); under a non-English locale the would-become-empty text is
///    translated, so with a hook present that refusal classifies as
///    `HookRejected` (wrong kind, and the message shown is still git's own
///    correct advice).
///  * Everything else: [`AmendFailureKind::Other`], with git's words
///    forwarded untouched.
fn classify_amend_failure(
    stderr: &str,
    signing_requested: bool,
    rejectable_hook_present: bool,
) -> AmendFailureKind {
    if stderr.contains("gpg failed to sign the data") {
        return AmendFailureKind::SigningFailed;
    }
    if signing_requested && stderr.contains("failed to write commit object") {
        return AmendFailureKind::SigningFailed;
    }
    if rejectable_hook_present
        && !stderr.contains("fatal:")
        && !stderr.contains("You asked to amend the most recent commit")
    {
        return AmendFailureKind::HookRejected;
    }
    AmendFailureKind::Other
}

/// `git add -A` (`/api/stage`).
async fn exec_stage_all(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    let output = match run_git(repo, need, &["add", "-A"]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stage", &e),
    };
    if output.status.success() {
        println!("[/api/stage] staged all changes (git add -A)");
        (StatusCode::OK, "Staged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git add failed.");
        eprintln!("git-vista: /api/stage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git apply --cached` of a built selection, then pathspec staging of the
/// whole-file part (M2.17b, #213; `/api/staging/apply`).
///
/// Order is deliberate: the patch leg runs first because it is the leg that
/// can fail (a hunk that no longer applies), and `git apply` is atomic — it
/// refuses the whole patch rather than applying half. The pathspec leg
/// (`git add --` / `git reset -q HEAD --`) after it is near-infallible, so
/// a failure almost always leaves the index wholly untouched. The residual
/// window — patch applied, pathspec then failing — is reported exactly as
/// what happened rather than papered over; the working tree is untouched in
/// every outcome, which is what makes this Safe-risk.
async fn exec_stage_selection(
    repo: &Path,
    need: NetworkNeed,
    direction: git_vista_protocol::StageDirection,
    expected_diff_generation: &git_vista_protocol::GenerationToken,
    patch: &str,
    whole_files: &[String],
) -> (StatusCode, String) {
    use git_vista_protocol::StageDirection;
    // The gate, re-run INSIDE the coordinator lock (the handler's ran
    // outside it): re-mint the diff-v1 token and refuse if the base diff
    // moved between gate and execution. Without this, a concurrent write in
    // that window could shift file content and `git apply` would still
    // apply mid-file hunks at drifted offsets — silently staging content
    // the user never previewed.
    match crate::handlers::read::staging_diff_for_repo(repo, direction).await {
        Ok(live) => {
            if let Err(refused) = crate::staging::require_current_selection_token(
                expected_diff_generation,
                &live.generation,
            ) {
                return refused;
            }
        }
        Err(e) => return e,
    }
    let mut done: Vec<String> = Vec::new();
    if !patch.is_empty() {
        // `--recount`: a safety net, not a correctness dependency. The hunk
        // header counts this server builds (`patch_build::append_hunk` for a
        // whole hunk, `append_sub_hunk` for #214's line-level sub-hunks) are
        // computed from the exact lines being emitted, so they are already
        // supposed to be right. `--recount` tells `git apply` to ignore the
        // `@@ -a,b +c,d @@` counts entirely and derive them itself from the
        // body — cheap insurance against an off-by-one in that hand-computed
        // arithmetic (most exposed in `append_sub_hunk`'s three-way
        // context/added/removed bookkeeping) turning into a hard "patch does
        // not apply" or, worse, a hunk applying at the wrong offset. Harmless
        // when the counts already agree with the body, which is every case
        // today.
        let args: &[&str] = match direction {
            StageDirection::Stage => &["apply", "--cached", "--whitespace=nowarn", "--recount"],
            StageDirection::Unstage => &[
                "apply",
                "--cached",
                "--reverse",
                "--whitespace=nowarn",
                "--recount",
            ],
        };
        let output =
            match crate::git_cmd::git_output_with_stdin(repo, args, need, patch.as_bytes()).await {
                Ok(o) => o,
                Err(e) => return couldnt_run("/api/staging/apply", &e),
            };
        if !output.status.success() {
            let mut msg = stderr_or(&output, "git apply failed.");
            // A replacement character in the patch means the file is not
            // valid UTF-8 — the lossy read can never byte-match the blob,
            // so git's "does not apply" is misleading without this.
            if patch.contains('\u{fffd}') {
                msg.push_str(
                    " (the file does not appear to be valid UTF-8 — hunk \
                     staging cannot address it; stage the entire file instead)",
                );
            }
            eprintln!("git-vista: /api/staging/apply patch leg failed: {msg}");
            // Nothing staged: apply is atomic, and the pathspec leg never ran.
            return (StatusCode::BAD_REQUEST, msg);
        }
        done.push("applied the selected hunks".to_string());
    }
    if !whole_files.is_empty() {
        // `git reset -q HEAD -- <path>` exits 0 even when the pathspec
        // matches nothing — a silent false success on a write surface. The
        // stage leg needs no twin check: `git add` of a nonexistent path
        // fails loudly on its own.
        if matches!(direction, StageDirection::Unstage) {
            let mut check: Vec<&str> = vec!["diff", "--cached", "--name-only", "-z", "--"];
            check.extend(whole_files.iter().map(String::as_str));
            let listed = match run_git(repo, need, &check).await {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                Ok(o) => {
                    let msg = stderr_or(&o, "pathspec check failed.");
                    return (StatusCode::BAD_REQUEST, msg);
                }
                Err(e) => return couldnt_run("/api/staging/apply", &e),
            };
            let matched: std::collections::HashSet<&str> =
                listed.split('\0').filter(|p| !p.is_empty()).collect();
            if let Some(missing) = whole_files.iter().find(|p| !matched.contains(p.as_str())) {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("nothing is staged at {missing}, so there is nothing to unstage"),
                );
            }
        }
        let mut args: Vec<&str> = match direction {
            StageDirection::Stage => vec!["add", "--"],
            StageDirection::Unstage => vec!["reset", "-q", "HEAD", "--"],
        };
        args.extend(whole_files.iter().map(String::as_str));
        let output = match run_git(repo, need, &args).await {
            Ok(o) => o,
            Err(e) => return couldnt_run("/api/staging/apply", &e),
        };
        if !output.status.success() {
            let msg = stderr_or(&output, "pathspec staging failed.");
            eprintln!("git-vista: /api/staging/apply pathspec leg failed: {msg}");
            let and_yet = if done.is_empty() {
                String::new()
            } else {
                // The residual non-atomic window, reported as fact.
                " The selected hunks were already applied to the index; \
                 the whole-file part was not."
                    .to_string()
            };
            return (StatusCode::BAD_REQUEST, format!("{msg}{and_yet}"));
        }
        done.push(format!("staged {} file(s) whole", whole_files.len()));
    }
    let verb = match direction {
        StageDirection::Stage => "Staged selection",
        StageDirection::Unstage => "Unstaged selection",
    };
    println!("[/api/staging/apply] {verb}: {}", done.join(", "));
    (StatusCode::OK, format!("{verb}."))
}

/// `git reset -q HEAD` (`/api/unstage`) — the exact inverse of stage-all; the
/// working tree keeps every edit, so nothing is lost.
async fn exec_unstage_all(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    let output = match run_git(repo, need, &["reset", "-q", "HEAD"]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/unstage", &e),
    };
    if output.status.success() {
        println!("[/api/unstage] unstaged all changes (git reset -q HEAD)");
        (StatusCode::OK, "Unstaged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git reset failed.");
        eprintln!("git-vista: /api/unstage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// One `git <args…> <ref>` branch operation with the shared error posture
/// (stderr, then stdout, then a generic line) — the old `run_branch_op` core.
///
/// `target` is a [`RefName`] rather than a [`BranchName`] since M2.20d
/// (#230): four of the five callers pass a local branch (converted for free —
/// see `impl From<&BranchName> for RefName`), and the fifth is a pull's
/// integration half, whose target is the remote-tracking name `origin/main`
/// and never a local branch. Both newtypes carry the identical
/// non-empty/not-option-shaped gate, so nothing about what may reach an argv
/// changed; only the name of the thing being described did.
async fn run_branch_cmd(
    repo: &Path,
    need: NetworkNeed,
    endpoint: &str,
    args: &[&str],
    target: &RefName,
    ok_msg: String,
) -> (StatusCode, String) {
    let mut argv: Vec<&str> = args.to_vec();
    argv.push(target.as_str());
    let output = match run_git(repo, need, &argv).await {
        Ok(o) => o,
        Err(e) => return couldnt_run(endpoint, &e),
    };
    if output.status.success() {
        println!("[{endpoint}] {ok_msg}");
        (StatusCode::OK, ok_msg)
    } else {
        let msg = stderr_stdout_or(&output, "git command failed.");
        eprintln!("git-vista: {endpoint} failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git checkout <branch>` (`/api/checkout`). Asking for the branch already
/// checked out is a no-op — git exits 0 — so journalling it would put a
/// phantom event in the Activity feed; a real switch is journaled and the
/// feed's dedup collapses git's own reflog copy into it.
async fn exec_checkout(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    observed: &Observed,
) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        need,
        "/api/checkout",
        &["checkout"],
        &RefName::from(branch),
        format!("checked out '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        if observed.head_branch.as_deref() == Some(branch.as_str()) {
            return (
                StatusCode::OK,
                format!("Already on ‘{branch}’ — it's the checked-out branch."),
            );
        }
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        journal_app_event(
            repo,
            ActivityKind::Checkout,
            Some(branch.as_str().to_string()),
            observed.head_tip.clone(),
            new,
            format!("checked out ‘{branch}’"),
        )
        .await;
    }
    resp
}

/// Why an integration ([`exec_merge`] / [`exec_rebase`]) is running — and
/// therefore what the activity feed must say happened (M2.20d, #230).
///
/// **The feed records the operation the user approved, not the git command
/// that implemented it.** A pull's second half runs `git merge` or
/// `git rebase`, but a user who pressed "Pull" never asked for a merge; a feed
/// showing `Fetch` + `Merge` for one approved `PullBranch` describes an
/// operation nobody submitted, and its undo hint would offer to undo half of
/// it. So the caller says who it is, and the one entry names the pull and the
/// strategy that ran.
///
/// A typed two-variant enum rather than a `bool` for ADR 0015's reason and one
/// more: the `Pull` arm has to *carry* the strategy, which a flag could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegrationCaller {
    /// `POST /api/merge` or `POST /api/rebase` — the user asked for exactly
    /// this git operation, and the feed says exactly that. The wording every
    /// existing entry already has.
    Direct,
    /// The integration half of `POST /api/pull` (M2.20d, #230). One
    /// [`ActivityKind::Pull`] entry naming the strategy, replacing — never
    /// accompanying — the `Merge`/`Rebase` entry the `Direct` arm writes.
    Pull(MergeStrategy),
}

impl IntegrationCaller {
    /// The `(kind, summary)` this caller's journal entry carries, given the
    /// ref that was integrated and the branch it landed on.
    fn journal_as(
        self,
        target: &RefName,
        into: &str,
        direct: (ActivityKind, String),
    ) -> (ActivityKind, String) {
        match self {
            IntegrationCaller::Direct => direct,
            IntegrationCaller::Pull(strategy) => (
                ActivityKind::Pull,
                format!(
                    "pulled ‘{target}’ into ‘{into}’ ({} strategy)",
                    strategy_word(strategy)
                ),
            ),
        }
    }
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

/// `git merge --no-edit <ref>` into the checked-out branch (`/api/merge`, and
/// the merge half of `/api/pull`).
async fn exec_merge(
    repo: &Path,
    need: NetworkNeed,
    target: &RefName,
    observed: &Observed,
    caller: IntegrationCaller,
) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        need,
        "/api/merge",
        &["merge", "--no-edit"],
        target,
        format!("merged '{target}' into HEAD"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        // git exits 0 with "Already up to date." when the branch brings
        // nothing in — HEAD hasn't moved. That's no merge: journalling one
        // would put an event in the Activity feed that never happened.
        //
        // D5: `same_observation`, not `==`. Both sides are reads that can come
        // back `Unknown`, and `Unknown == Unknown` would report "already up to
        // date" — a statement about where HEAD is — on the strength of two
        // reads that never saw HEAD at all. `Obs` has no `PartialEq` precisely
        // so this line cannot be written the wrong way.
        if new.same_observation(&observed.head_tip) {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{target}’ has no commits the current branch doesn’t already have."),
            );
        }
        let into = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        let (kind, summary) = caller.journal_as(
            target,
            &into,
            (
                ActivityKind::Merge,
                format!("merged ‘{target}’ into ‘{into}’"),
            ),
        );
        journal_app_event(
            repo,
            kind,
            Some(into.clone()),
            observed.head_tip.clone(),
            new,
            summary,
        )
        .await;
    }
    resp
}

/// `git branch -d`/`-D <branch>` (`/api/delete-branch` /
/// `/api/force-delete-branch`). The tip was captured BEFORE the delete (git
/// removes the branch's reflog with the branch) — that journaled oid is
/// precisely what "Restore branch" replays, and after a force-delete it may be
/// the ONLY path back to the commits (until gc).
async fn exec_delete(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    observed: &Observed,
    force: bool,
) -> (StatusCode, String) {
    let (endpoint, flag, verb) = if force {
        ("/api/force-delete-branch", "-D", "force-deleted")
    } else {
        ("/api/delete-branch", "-d", "deleted")
    };
    let resp = run_branch_cmd(
        repo,
        need,
        endpoint,
        &["branch", flag],
        &RefName::from(branch),
        format!("{verb} branch '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        journal_app_event(
            repo,
            ActivityKind::BranchDeleted,
            Some(branch.as_str().to_string()),
            observed.branch_tip.clone(),
            Obs::Absent, // the branch is gone: its new tip is a real absence
            format!("{verb} branch ‘{branch}’"),
        )
        .await;
        // Drop it from the snapshot now, so the feed's snapshot diff can't
        // also report this app deletion as an external one.
        remove_from_snapshot_blocking(repo, branch.as_str()).await;
    }
    resp
}

/// `git rebase <base>` of the checked-out branch (`/api/rebase`). A failed
/// rebase (almost always conflicts) is `--abort`ed so a browser-only user is
/// never left mid-rebase with no shell to fix it.
async fn exec_rebase(
    repo: &Path,
    need: NetworkNeed,
    base: &RefName,
    observed: &Observed,
    caller: IntegrationCaller,
) -> (StatusCode, String) {
    let old = observed.head_tip.clone();
    let target = base;
    let base = base.as_str();

    let output = match run_git(repo, need, &["rebase", base]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/rebase", &e),
    };
    if output.status.success() {
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        let branch = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        // git exits 0 without moving HEAD when the branch is already based on
        // the base — that's no rebase, and journalling one would put a phantom
        // event in the Activity feed. Say what (didn't) happen instead.
        //
        // D5: same reasoning as `exec_merge` — two unreadable tips are not
        // evidence that HEAD stayed put.
        if new.same_observation(&old) {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ is already based on {base}."),
            );
        }
        println!("[/api/rebase] rebased HEAD onto {base}");
        let (kind, summary) = caller.journal_as(
            target,
            &branch,
            (
                ActivityKind::Rebase,
                format!("rebased ‘{branch}’ onto {base}"),
            ),
        );
        journal_app_event(repo, kind, Some(branch.clone()), old, new, summary).await;
        (StatusCode::OK, format!("Rebased onto {base}."))
    } else {
        let msg = stderr_stdout_or(&output, "git rebase failed.");
        // Best-effort: back out of the half-applied rebase so the working tree
        // isn't stuck mid-rebase. Harmless (exits non-zero, ignored) when none
        // is running.
        let _ = run_git(repo, need, &["rebase", "--abort"]).await;
        eprintln!("git-vista: /api/rebase failed (aborted): {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git branch <name> <tip>` — re-create a deleted branch at its journaled tip
/// (`/api/undo`). The safe undo: `git branch` creates, never destroys, and
/// fails by itself if the name came back into use since the hint.
async fn exec_restore_branch(
    repo: &Path,
    need: NetworkNeed,
    name: &BranchName,
    tip: &CommitOid,
) -> (StatusCode, String) {
    match git(repo, need, &["branch", name.as_str(), tip.as_str()]).await {
        Ok(()) => {
            println!(
                "[/api/undo] restored branch '{name}' at {}",
                short(tip.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::BranchCreated,
                Some(name.as_str().to_string()),
                Obs::Absent, // restored from nothing: there was no branch here
                Obs::Known(tip.as_str().to_string()),
                format!("restored branch ‘{name}’ at {}", short(tip.as_str())),
            )
            .await;
            (StatusCode::OK, format!("Restored branch ‘{name}’."))
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo restore failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// Move a branch back to `to` (`/api/undo`): `git reset --hard` when it's the
/// checked-out branch with a clean worktree, else `git branch -f` (no worktree
/// involved). `expected_tip` is compare-and-swap — a hint from a stale menu
/// can never reset away work that happened after it was shown.
async fn exec_reset_branch(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    to: &CommitOid,
    expected_tip: &CommitOid,
    observed: &Observed,
) -> (StatusCode, String) {
    // Compare-and-swap: the hint was computed against `expected_tip`; if the
    // branch has moved since, this undo would discard newer work the user
    // never saw in the dialog — refuse instead.
    // D5: `Obs::Unknown` fails this check, the same as a mismatch would — a
    // compare-and-swap whose "compare" never read anything must not swap.
    if observed.branch_tip.known().map(String::as_str) != Some(expected_tip.as_str()) {
        return (
            StatusCode::CONFLICT,
            format!("‘{branch}’ has moved since this undo was offered — refresh and try again."),
        );
    }
    let checked_out = observed.head_branch.as_deref() == Some(branch.as_str());
    let result = if checked_out {
        // `git reset --hard` rewrites the working tree, so it runs only
        // against a fully clean one — never eat uncommitted work.
        match worktree_dirty(repo, need).await {
            Err(msg) => Err(msg),
            Ok(true) => {
                return (
                    StatusCode::CONFLICT,
                    "The working tree has uncommitted changes — commit them first \
                     so the undo can't destroy them."
                        .to_string(),
                );
            }
            Ok(false) => git(repo, need, &["reset", "--hard", to.as_str()]).await,
        }
    } else {
        // Not checked out: move the ref alone, no worktree involved.
        git(repo, need, &["branch", "-f", branch.as_str(), to.as_str()]).await
    };
    match result {
        Ok(()) => {
            println!(
                "[/api/undo] reset branch '{branch}' to {}",
                short(to.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::Reset,
                Some(branch.as_str().to_string()),
                // Both are exact ids from the request, not reads.
                Obs::Known(expected_tip.as_str().to_string()),
                Obs::Known(to.as_str().to_string()),
                format!("undid — reset ‘{branch}’ to {}", short(to.as_str())),
            )
            .await;
            (
                StatusCode::OK,
                format!("Reset ‘{branch}’ to {}.", short(to.as_str())),
            )
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo reset failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// `git revert --no-edit <commit>` (`/api/undo`) — the history-preserving
/// undo; a conflicted revert is auto-aborted (like `/api/rebase`) so a
/// browser-only user is never left mid-revert.
async fn exec_revert(
    repo: &Path,
    need: NetworkNeed,
    commit: &CommitOid,
    observed: &Observed,
) -> (StatusCode, String) {
    let commit = commit.as_str();
    match git(repo, need, &["revert", "--no-edit", commit]).await {
        Ok(()) => {
            println!("[/api/undo] reverted {}", short(commit));
            let new = Obs::from_read(rev_parse(repo, "HEAD").await);
            let branch = read_head_branch_blocking(repo)
                .await
                .unwrap_or_else(|| "HEAD".into());
            journal_app_event(
                repo,
                ActivityKind::Revert,
                Some(branch),
                observed.head_tip.clone(),
                new,
                format!("reverted {}", short(commit)),
            )
            .await;
            (StatusCode::OK, format!("Reverted {}.", short(commit)))
        }
        Err(msg) => {
            // Back out of a conflicted half-applied revert so the tree isn't
            // stuck. Harmless when no revert is in progress.
            let _ = git(repo, need, &["revert", "--abort"]).await;
            eprintln!("git-vista: /api/undo revert failed (aborted): {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
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

/// Reset a *test repo* to its recorded seed (`/api/reset-test-repo`): move
/// every seeded branch back to its recorded tip, check out the seeded HEAD
/// branch, force the worktree clean, DELETE branches the seed doesn't know —
/// allowed nowhere else in git-vista — and wipe the app journal (its events
/// describe history that no longer exists). Hard-gated: only a repo
/// explicitly opted in with `gv --seed <path>` has seed files.
async fn exec_reset_test_repo(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    // `need` is threaded for the same reason every other `exec_*` threads it —
    // so the declared value reaches `policy_for` — but this operation's git
    // steps go through `git_cmd::git_ok`, which declares `Local` at its own
    // seam (see its comment). Consume it explicitly so a future edit that adds
    // a `run_git` step here has the right value already in scope.
    let _ = need;
    let seed = match read_seed(repo) {
        None => {
            return (
                StatusCode::NOT_FOUND,
                "This repo has no recorded seed — it isn't marked as a test repo. \
                 Run `gv --seed <path>` once on the server machine to record its reset point."
                    .to_string(),
            )
        }
        Some(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The recorded seed is corrupt ({e}) — re-record it with `gv --seed`."),
            )
        }
        Some(Ok(seed)) => seed,
    };

    // Objects first, verification second: unbundle is best-effort (idempotent,
    // cheap), then every seeded tip must resolve or the reset refuses to start —
    // never a half-restore.
    if let Some(dir) = journal::state_dir(repo) {
        let bundle = dir.join("seed.bundle");
        if bundle.exists() {
            let _ = git_ok(repo, &["bundle", "unbundle", &bundle.display().to_string()]).await;
        }
    }
    for r in &seed.refs {
        match rev_parse(repo, &format!("{}^{{commit}}", r.oid)).await {
            Ok(Some(_)) => {}
            // git ran and could not find the object: a real, reportable
            // problem with the seed.
            Ok(None) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "Seed commit {} for ‘{}’ no longer exists in this repo — \
                         re-record the seed with `gv --seed`.",
                        &r.oid[..7],
                        r.name
                    ),
                )
            }
            // D5: git never ran. Telling the operator to re-record a seed that
            // is probably intact would send them to destroy the one recovery
            // point this endpoint restores from. Refuse without a diagnosis.
            Err(e) => {
                return couldnt_run(
                    "/api/reset-test-repo",
                    &format!("couldn't verify seed commit for ‘{}’: {e}", r.name),
                )
            }
        }
    }

    // What the repo looks like NOW, then the pure plan of moves + deletions.
    let current_refs = match run_git(
        repo,
        need,
        &[
            "for-each-ref",
            "refs/heads",
            "--format=%(objectname) %(refname:short)",
        ],
    )
    .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                l.split_once(' ')
                    .map(|(oid, name)| (name.to_string(), oid.to_string()))
            })
            .collect::<Vec<_>>(),
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't list the repo's current branches.".to_string(),
            )
        }
    };
    let plan = reset_plan(&seed, &current_refs);

    // Apply, in an order where each step makes the next valid: refs back first
    // (so the seed HEAD branch exists at the right tip), then a forced checkout
    // + hard reset + clean (so HEAD is off any branch about to be deleted and
    // the worktree matches the seed exactly), then the deletions.
    for r in &plan.update {
        if let Err(e) = git_ok(
            repo,
            &["update-ref", &format!("refs/heads/{}", r.name), &r.oid],
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped while restoring ‘{}’: {e}", r.name),
            );
        }
    }
    for step in [
        &["checkout", "-f", seed.head.as_str()] as &[&str],
        &["reset", "--hard"],
        &["clean", "-fd"],
    ] {
        if let Err(e) = git_ok(repo, step).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped at `git {}`: {e}", step.join(" ")),
            );
        }
    }
    let mut deleted = 0;
    for name in &plan.delete {
        // The ONLY place git-vista deletes a branch: a seeded test repo, inside
        // an explicit reset, for branches created after the seed was recorded.
        match git_ok(repo, &["branch", "-D", name]).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Reset stopped deleting test branch ‘{name}’: {e}"),
                )
            }
        }
    }
    // The journal now describes history that no longer exists (dead undo
    // targets included) — wipe it with the snapshot; both regenerate.
    journal_clear_blocking(repo).await;

    let msg = format!(
        "Reset to seed: {} branch(es) restored, {} deleted, HEAD → ‘{}’, working tree clean.",
        plan.update.len(),
        deleted,
        seed.head
    );
    println!("[/api/reset-test-repo] {msg}");
    (StatusCode::OK, msg)
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

/// `git checkout -- <paths>` (`/api/discard-tracked-paths`, #219): discard
/// uncommitted changes to already-tracked paths, restoring each to its
/// checked-out (index, else HEAD) version. See
/// [`GitOperation::DiscardTrackedPaths`]'s doc comment for the exact,
/// qualified recovery story this response/journal text spells out — this is
/// destructive, and only *sometimes* undoable outside git-vista.
async fn exec_discard_tracked_paths(
    repo: &Path,
    need: NetworkNeed,
    paths: &[WorktreePath],
) -> (StatusCode, String) {
    if let Err(refused) = symlink_containment_guard(repo, paths, "/api/discard-tracked-paths").await
    {
        return refused;
    }
    if let Err(refused) = verify_path_states(
        repo,
        need,
        paths,
        PathKind::TrackedDirty,
        "/api/discard-tracked-paths",
    )
    .await
    {
        return refused;
    }
    // `git checkout HEAD -- <paths>`, not the bare `git checkout -- <paths>`
    // the issue's own shorthand suggested: bare `checkout --` only resets
    // the worktree to the INDEX, so a path whose only difference is staged
    // (index != HEAD, worktree == index) is a silent no-op — verified
    // empirically before this fix landed (review finding: it returned 200
    // and journaled "discarded" while the git command changed nothing).
    // `checkout HEAD --` resets both index and worktree to HEAD, discarding
    // staged and unstaged changes alike, which is what "discard uncommitted
    // changes" means to a caller regardless of staging state — and the
    // staged blob (if any) still survives as a dangling object until the
    // next `git gc`, confirmed with `git fsck --unreachable`, so the
    // recovery-story text below stays true either way.
    let mut args: Vec<&str> = vec!["checkout", "HEAD", "--"];
    args.extend(paths.iter().map(WorktreePath::as_str));
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/discard-tracked-paths", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git checkout failed.");
        eprintln!("git-vista: /api/discard-tracked-paths failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let count = paths.len();
    let s = if count == 1 { "" } else { "s" };
    let summary = format!(
        "discarded uncommitted changes to {count} tracked path{s} — recoverable only \
         for content staged before this ran, and only until git gc; a worktree-only \
         edit is gone"
    );
    println!("[/api/discard-tracked-paths] {summary}");
    journal_app_event(
        repo,
        ActivityKind::Other,
        None,
        Obs::Absent,
        Obs::Absent,
        summary,
    )
    .await;
    (
        StatusCode::OK,
        format!(
            "Discarded uncommitted changes to {count} tracked path{s}. Recoverable only \
             for content that was staged before this ran, and only until the next git \
             gc — a worktree-only edit is gone."
        ),
    )
}

/// After `git clean -f -- <paths>` has run, ask the **filesystem** which of
/// `requested` is actually gone, and build an honest partial-result message
/// naming any that survived — `None` when every requested path really was
/// removed.
///
/// **Why the filesystem and not `git clean`'s stdout (#284).** Until #284
/// this parsed `git clean`'s own output for `Removing <path>`. That string is
/// passed through gettext in git's source, so it is translated whenever a
/// `git.mo` catalog is installed and `LANG`/`LC_MESSAGES` names a non-English
/// locale — and production spawns inherit the server's environment in full,
/// because `sandbox::spawn`'s `env_clear`/`env` are `#[cfg(test)]`-only *by
/// design* (argv and env cannot change after policy classification). Under
/// `LANG=fr_FR.UTF-8` with translations installed, three successfully deleted
/// files matched no prefix, all three looked un-deleted, and the endpoint
/// returned 409 telling the user their files had survived — after they were
/// irreversibly gone. That is the exact inversion of the property this
/// function exists to provide, so the parse is gone: a dirent that is still
/// there was not deleted, in every language.
///
/// **`symlink_metadata`, not `Path::exists`.** `exists()` follows the link, so
/// a *dangling* symlink — one whose target is already gone — reports as
/// absent while its dirent is still sitting in the worktree. `git clean` can
/// and does delete dangling symlinks, so both "clean removed it" and "clean
/// skipped it" would look identical to `exists()`, reintroducing a false
/// success in the narrow case. `symlink_metadata` stats the entry itself and
/// tells the two apart. (Same reason `symlink_containment_guard` uses it.)
///
/// **What this can still get wrong, stated plainly.** If something *else*
/// deleted a requested path in the same window, we cannot see that it was
/// not us. See [`DeleteOutcome`] for how much of that window the
/// before-snapshot closes and what is left.
///
/// A stat error other than "not found" (an unreadable parent directory, say)
/// counts as absent, deliberately: presence is the claim that has to be
/// *proved* here, and an error proves nothing.
///
/// Synchronous `stat` calls in an async fn, deliberately: one per requested
/// path, bounded by the request, on entries `symlink_containment_guard` and
/// `verify_path_states` stat'd microseconds earlier — not worth a
/// `spawn_blocking` hop and the join-error branch that comes with it.
fn present_paths<'a>(repo: &Path, requested: &[&'a str]) -> Vec<&'a str> {
    requested
        .iter()
        .copied()
        .filter(|p| std::fs::symlink_metadata(repo.join(p)).is_ok())
        .collect()
}

/// What the worktree says happened to each requested path, split three ways
/// by comparing a presence snapshot taken immediately *before* the `git
/// clean` spawn against one taken immediately after.
///
/// **Why three buckets and not two (#284, review finding).** The first cut of
/// this only looked at the worktree *after* the spawn, so "absent now" was
/// read as "we deleted it". That silently credits this operation with a
/// deletion it did not perform: `git clean -f -- a.txt b.txt` exits 0 and
/// says nothing when `b.txt` is already gone (verified directly against real
/// git), so a second git-vista tab, a shell `rm`, or an editor auto-clean
/// removing `b.txt` first produced a 200 reading "Deleted 2 untracked paths
/// permanently" — and, worse, a *journal* entry saying the same. The journal
/// is the durable record for the one operation in this vocabulary with no
/// undo of any kind; an entry claiming a destruction we did not cause is a
/// corrupt audit trail, not a rounding error.
///
/// **How much window this actually closes, stated plainly.** Not all of it.
/// [`verify_path_states`] already refuses a path that has vanished by the
/// time its `git status` runs (a missing path classifies as
/// [`PathKind::Other`], never [`PathKind::Untracked`]), so the exposure was
/// always the gap between that read and `git clean`'s own `unlink`. The
/// before-snapshot moves the near edge of that gap from "before a `git
/// status` subprocess spawn, a porcelain parse, and a `git clean` subprocess
/// spawn" to "after all of those, one `stat` before the spawn" — milliseconds
/// down to whatever elapses inside `git clean` itself. What remains is an
/// external deleter landing *inside* the child process's own run. Closing
/// that last sliver needs a repo-wide exclusive lock this endpoint does not
/// hold, which is a different and much larger decision; narrowing it by three
/// orders of magnitude costs one `stat` per requested path on entries that
/// were stat'd twice already.
///
/// **The bias is unchanged and deliberate.** A path still on disk is always
/// reported as a survivor, whoever put it there. This can still never claim a
/// destroyed file survived — the inversion #284 was filed about, and the only
/// failure direction that makes a user stop looking for data that is gone for
/// good.
#[derive(Debug, PartialEq, Eq)]
struct DeleteOutcome<'a> {
    /// Present before the spawn, absent after: this operation removed it.
    /// The count the response and the journal are allowed to claim.
    deleted: Vec<&'a str>,
    /// Absent before the spawn and absent after: gone for good, but not by
    /// our hand. Reported, never counted as ours.
    already_gone: Vec<&'a str>,
    /// Still in the worktree: not deleted, whatever it points at. `git clean`
    /// silently skips a path that has become tracked since the pre-flight
    /// check (no error, exit 0), which is how this bucket normally fills.
    survived: Vec<&'a str>,
}

/// Compare the before-snapshot against the live worktree. `present_before`
/// comes from [`present_paths`] called immediately before the spawn.
fn observe_deletion<'a>(
    repo: &Path,
    requested: &[&'a str],
    present_before: &[&'a str],
) -> DeleteOutcome<'a> {
    let present_after = present_paths(repo, requested);
    let mut outcome = DeleteOutcome {
        deleted: Vec::new(),
        already_gone: Vec::new(),
        survived: Vec::new(),
    };
    for p in requested.iter().copied() {
        if present_after.contains(&p) {
            outcome.survived.push(p);
        } else if present_before.contains(&p) {
            outcome.deleted.push(p);
        } else {
            outcome.already_gone.push(p);
        }
    }
    outcome
}

impl DeleteOutcome<'_> {
    /// The 409 body when some requested path is still on disk — `None` when
    /// nothing survived. Refusing now cannot un-delete what already went, so
    /// what this can still do is name exactly what happened instead of a
    /// count that does not match reality.
    fn partial_refusal(&self) -> Option<String> {
        if self.survived.is_empty() {
            return None;
        }
        let survived_list = self.survived.join(", ");
        let survived_verb = if self.survived.len() == 1 {
            "was"
        } else {
            "were"
        };
        let destroyed = if self.deleted.is_empty() {
            "Partial result: nothing was deleted".to_string()
        } else {
            format!(
                "Partial result: {} {} deleted permanently",
                self.deleted.join(", "),
                if self.deleted.len() == 1 {
                    "was"
                } else {
                    "were"
                }
            )
        };
        let mut msg = format!(
            "{destroyed}, but {survived_list} {survived_verb} not — its state changed \
             (likely became tracked) in the instant between the pre-flight check and \
             this running. Nothing further was applied for {survived_list}; re-check \
             its status before retrying."
        );
        msg.push_str(&self.already_gone_note());
        Some(msg)
    }

    /// The whole client-facing outcome — status, response body, journal line
    /// — derived from nothing but what the worktree proved.
    ///
    /// **Why this composes the message instead of the executor (review
    /// finding).** The count started life as `paths.len()` in the executor,
    /// which is what defect 2 of #284 fixed for duplicates and what the
    /// before-snapshot fixes for foreign deletions. Both are the same mistake:
    /// counting what was *asked for* rather than what was *observed*. While
    /// that arithmetic lived inline in an `async fn` that does its own
    /// `stat`ing, no test could reach a state where the two counts differ, so
    /// reverting it to `paths.len()` passed the entire suite — a green test
    /// proving nothing. Owning it here makes the divergent case constructible
    /// (see `a_report_counts_only_what_this_operation_destroyed`) and leaves
    /// the executor a thin caller with no count of its own to get wrong.
    fn report(&self) -> (StatusCode, String, String) {
        if let Some(msg) = self.partial_refusal() {
            let journal = format!("delete-untracked-paths partial result — {msg}");
            return (StatusCode::CONFLICT, msg, journal);
        }
        // `self.deleted.len()`, and no other number is in scope to reach for:
        // the count is the user's only record of what is gone for good.
        let count = self.deleted.len();
        let s = if count == 1 { "" } else { "s" };
        let note = self.already_gone_note();
        // Deliberately no "undo"/"restore"/"recover" anywhere in this text (a
        // regression test greps for exactly those words) — this is the one
        // operation in the vocabulary where saying so plainly is the honest
        // thing to say, not merely the cautious one.
        let journal = format!(
            "deleted {count} untracked path{s} permanently — never stored in git, no \
             way to bring the content back{note}"
        );
        let body = format!(
            "Deleted {count} untracked path{s} permanently. That content was never \
             stored in git, so there is no way to bring it back.{note}"
        );
        (StatusCode::OK, body, journal)
    }

    /// One sentence disclosing paths that were already gone before the spawn,
    /// empty when there were none. Kept separate so both the refusal body and
    /// the success body say the same thing about them.
    fn already_gone_note(&self) -> String {
        if self.already_gone.is_empty() {
            return String::new();
        }
        let list = self.already_gone.join(", ");
        let verb = if self.already_gone.len() == 1 {
            "was"
        } else {
            "were"
        };
        format!(
            " {list} {verb} already gone before this ran, so {} not deleted by this \
             operation — something else outside Git-Vista removed {}.",
            if self.already_gone.len() == 1 {
                "it was"
            } else {
                "they were"
            },
            if self.already_gone.len() == 1 {
                "it"
            } else {
                "them"
            }
        )
    }
}

/// `git clean -f -- <paths>` (`/api/delete-untracked-paths`, #219): delete
/// untracked paths from the working tree outright. **No journal-backed undo
/// exists for this at all** — an untracked path was never written to git's
/// object database, so there is nothing anywhere in this repository to reset
/// back to. See [`GitOperation::DeleteUntrackedPaths`]'s doc comment.
async fn exec_delete_untracked_paths(
    repo: &Path,
    need: NetworkNeed,
    paths: &[WorktreePath],
) -> (StatusCode, String) {
    if let Err(refused) =
        symlink_containment_guard(repo, paths, "/api/delete-untracked-paths").await
    {
        return refused;
    }
    if let Err(refused) = verify_path_states(
        repo,
        need,
        paths,
        PathKind::Untracked,
        "/api/delete-untracked-paths",
    )
    .await
    {
        return refused;
    }
    let mut args: Vec<&str> = vec!["clean", "-f", "--"];
    args.extend(paths.iter().map(WorktreePath::as_str));
    // Snapshot presence as late as possible — the very last thing before the
    // spawn — so what this operation is credited with destroying is what it
    // actually destroyed, not merely what is missing afterwards. See
    // [`DeleteOutcome`] for the window this does and does not close.
    let requested: Vec<&str> = paths.iter().map(WorktreePath::as_str).collect();
    let present_before = present_paths(repo, &requested);
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/delete-untracked-paths", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git clean failed.");
        eprintln!("git-vista: /api/delete-untracked-paths failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }

    // The TOCTOU this closes (review finding, empirically demonstrated): a
    // path can become tracked in the gap between `verify_path_states`'s read
    // and this exact `git clean` call — a concurrent `git add`, an IDE
    // auto-stage, a second git-vista tab. `git clean -f -- p1 p2 p3` is NOT
    // atomic across a multi-path pathspec: it silently SKIPS a path that's
    // since become tracked (no error, exit 0) while still deleting the
    // rest of the batch — verified directly against real git. Locking out
    // the whole race window needs a repo-wide exclusive lock this endpoint
    // doesn't hold; what's tractable and load-bearing without one is never
    // reporting success that isn't true: every requested path is re-stat'd
    // before this returns 200, and one still on disk was not deleted
    // (`observe_deletion`; #284 replaced an English-only parse of `git
    // clean`'s stdout with that check — see [`DeleteOutcome`]'s doc comment,
    // which also covers why the *before* snapshot above is needed to avoid
    // the mirror-image dishonesty of crediting ourselves with someone else's
    // deletion). The timing race itself isn't something a permanent test can
    // trigger deterministically, but the honesty property this exists for
    // doesn't depend on how a mismatch arose.
    //
    // Everything client-facing past this point is [`DeleteOutcome::report`]'s
    // — this executor deliberately keeps no count of its own to get wrong.
    let outcome = observe_deletion(repo, &requested, &present_before);
    let (status, body, summary) = outcome.report();
    if status == StatusCode::CONFLICT {
        eprintln!("git-vista: /api/delete-untracked-paths partial: {body}");
    } else {
        println!("[/api/delete-untracked-paths] {summary}");
    }
    journal_app_event(
        repo,
        ActivityKind::Other,
        None,
        Obs::Absent,
        Obs::Absent,
        summary,
    )
    .await;
    (status, body)
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

/// `POST /api/fetch`'s error-body constructor, re-exported so the handler's
/// own request-shape refusals carry the same contract the executor's do.
pub(crate) use fetch::error_body as fetch_error_body;

/// `POST /api/pull`'s error-body constructor, re-exported for the same reason
/// [`fetch_error_body`] is: the handler's request-shape refusals — above all
/// the missing-`strategy` 400 that is #230's whole point — must parse as the
/// endpoint's one error type, exactly like the executor's do.
pub(crate) use pull::error_body as pull_error_body;

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
        GitOperation::CreateBranch { .. }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tokens() -> (RepositoryToken, WorktreeToken) {
        (
            RepositoryToken::new("test-repo").unwrap(),
            WorktreeToken::new("test-worktree").unwrap(),
        )
    }

    fn run(repo: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap()
                .success(),
            "git {args:?} failed in {repo:?}"
        );
    }

    /// `git rev-parse HEAD` in `repo`, trimmed — for tests that need a real
    /// oid to build a compare-and-swap `GitOperation` against (#222).
    async fn git_rev_parse_head(repo: &Path) -> String {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A fresh repository on branch `main` with one committed file and a
    /// clean working tree.
    fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        (dir, repo)
    }

    /// #145 acceptance 1 + 4 (the race): a plan built against generation N is
    /// refused once anything moves — a new commit, or even just the working
    /// tree picking up an untracked file — and a fresh plan passes.
    #[tokio::test]
    async fn a_generation_move_refuses_execution() {
        let (_dir, repo) = seeded_repo();
        let (plan, observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;

        // Fresh plan against an untouched repository: allowed.
        assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

        // Worktree-only drift (no ref moved): still a generation move.
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("changed while this plan was pending"), "{why}");

        // Ref drift (a new commit) on a *fresh* plan built after the file
        // appeared: build, then commit, then try to execute.
        let (plan, observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
        run(&repo, &["add", "b.txt"]);
        run(&repo, &["commit", "-q", "-m", "moved"]);
        let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("changed while this plan was pending"), "{why}");
    }

    /// Capture one git command's stdout in a fixture repo.
    fn run_out(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed in {repo:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// A committed 20-line file plus edits at both ends — far enough apart
    /// that `git diff` emits two hunks.
    fn repo_with_two_hunks() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("a.txt"), &body).unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "twenty lines"]);
        let edited = body
            .replace("line 2\n", "line 2 changed\n")
            .replace("line 18\n", "line 18 changed\n");
        std::fs::write(repo.join("a.txt"), edited).unwrap();
        (dir, repo)
    }

    /// The wire plan for "hunk `index` of `path`", anchored from the parsed
    /// diff itself (the same way a client copies anchors out of the served
    /// diff).
    fn plan_for_hunk_at(
        parsed: &git_vista_protocol::ParsedPatch,
        path: &str,
        index: u32,
        direction: git_vista_protocol::StageDirection,
    ) -> git_vista_protocol::PatchPlan {
        let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
            panic!("expected a hunks-shaped file");
        };
        let h = &hunks[index as usize];
        git_vista_protocol::PatchPlan {
            repository: RepositoryToken::new("test-repo").unwrap(),
            worktree: WorktreeToken::new("test-worktree").unwrap(),
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            direction,
            files: vec![git_vista_protocol::FileSelection {
                path: path.to_string(),
                selection: git_vista_protocol::SelectionShape::Hunks {
                    hunks: vec![git_vista_protocol::HunkRef {
                        index,
                        old_start: h.old_start,
                        new_start: h.new_start,
                    }],
                },
            }],
        }
    }

    /// [`plan_for_hunk_at`] for the common case, `a.txt`.
    fn plan_for_hunk(
        parsed: &git_vista_protocol::ParsedPatch,
        index: u32,
        direction: git_vista_protocol::StageDirection,
    ) -> git_vista_protocol::PatchPlan {
        plan_for_hunk_at(parsed, "a.txt", index, direction)
    }

    /// The wire plan for "these specific `lines` of hunk `index` of a.txt"
    /// (#214) — the line-level sibling of [`plan_for_hunk`].
    fn plan_for_lines(
        parsed: &git_vista_protocol::ParsedPatch,
        index: u32,
        lines: Vec<u32>,
        direction: git_vista_protocol::StageDirection,
    ) -> git_vista_protocol::PatchPlan {
        let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
            panic!("expected a hunks-shaped file");
        };
        let h = &hunks[index as usize];
        git_vista_protocol::PatchPlan {
            repository: RepositoryToken::new("test-repo").unwrap(),
            worktree: WorktreeToken::new("test-worktree").unwrap(),
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            direction,
            files: vec![git_vista_protocol::FileSelection {
                path: "a.txt".to_string(),
                selection: git_vista_protocol::SelectionShape::Lines {
                    hunks: vec![git_vista_protocol::HunkLines {
                        hunk: git_vista_protocol::HunkRef {
                            index,
                            old_start: h.old_start,
                            new_start: h.new_start,
                        },
                        lines,
                    }],
                },
            }],
        }
    }

    /// A committed multi-line file (`a.txt`) with an uncommitted edit
    /// spanning two adjacent single-line replacements — far enough apart in
    /// *content* but close enough in *position* that `git diff` emits one
    /// hunk with more than one added/removed line, so a line-level selection
    /// can pick a genuine subset of it (#214). `b.txt` is a second tracked,
    /// unmodified file a drift test can mutate on its own.
    fn repo_with_multiline_hunk() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("a.txt"), &body).unwrap();
        std::fs::write(repo.join("b.txt"), "unrelated\n").unwrap();
        run(&repo, &["add", "a.txt", "b.txt"]);
        run(&repo, &["commit", "-q", "-m", "ten lines plus b.txt"]);
        let edited = body
            .replace("line 4\n", "line 4 changed\n")
            .replace("line 5\n", "line 5 changed\n");
        std::fs::write(repo.join("a.txt"), edited).unwrap();
        (dir, repo)
    }

    /// A file renamed with further content edits, fully staged (`git add
    /// -A` of a filesystem `mv` plus an edit) — the only way this server's
    /// staging surface can ever actually present a `FileDiff::Hunks` entry
    /// whose `old_path != new_path` (see
    /// `unstaging_a_content_hunk_of_a_renamed_file_reverses_only_the_content`'s
    /// doc for why).
    fn repo_with_staged_rename_and_edit() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        let body: String = (1..=6).map(|i| format!("line {i}\n")).collect();
        std::fs::write(repo.join("a.txt"), &body).unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "six lines"]);
        std::fs::rename(repo.join("a.txt"), repo.join("renamed.txt")).unwrap();
        let edited = body.replace("line 3\n", "line 3 changed\n");
        std::fs::write(repo.join("renamed.txt"), edited).unwrap();
        run(&repo, &["add", "-A"]);
        (dir, repo)
    }

    /// M2.17b acceptance, the mechanism end to end on a real repository:
    /// building the selected patch from git's own diff and applying it
    /// `--cached` stages exactly the selected hunk — the other hunk stays a
    /// worktree-only edit.
    #[tokio::test]
    async fn a_selected_hunk_stages_alone_and_the_rest_stays_unstaged() {
        let (_dir, repo) = repo_with_two_hunks();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let plan = plan_for_hunk(&parsed, 0, git_vista_protocol::StageDirection::Stage);
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
        assert!(built.patch.contains("line 2 changed"));
        assert!(!built.patch.contains("line 18 changed"));

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        let cached = run_out(&repo, &["diff", "--cached", "--no-color"]);
        assert!(cached.contains("line 2 changed"), "{cached}");
        assert!(!cached.contains("line 18 changed"), "{cached}");
        let worktree = run_out(&repo, &["diff", "--no-color"]);
        assert!(worktree.contains("line 18 changed"), "{worktree}");
        assert!(!worktree.contains("line 2 changed"), "{worktree}");
    }

    /// The reverse leg: with both hunks staged, unstaging one (built from
    /// the index-vs-HEAD base per the direction contract) moves exactly it
    /// back to worktree-only.
    #[tokio::test]
    async fn unstaging_a_selected_hunk_reverses_only_it() {
        let (_dir, repo) = repo_with_two_hunks();
        run(&repo, &["add", "a.txt"]);
        let diff = run_out(&repo, &["diff", "--cached", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let plan = plan_for_hunk(&parsed, 0, git_vista_protocol::StageDirection::Unstage);
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Unstage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Unstage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        let cached = run_out(&repo, &["diff", "--cached", "--no-color"]);
        assert!(!cached.contains("line 2 changed"), "{cached}");
        assert!(cached.contains("line 18 changed"), "{cached}");
        let worktree = run_out(&repo, &["diff", "--no-color"]);
        assert!(worktree.contains("line 2 changed"), "{worktree}");
    }

    /// The pathspec leg: an entire-file selection stages its file whole and
    /// leaves other modified files untouched.
    #[tokio::test]
    async fn an_entire_file_selection_stages_only_its_pathspec() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        run(&repo, &["add", "c.txt"]);
        run(&repo, &["commit", "-q", "-m", "second file"]);
        std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
        std::fs::write(repo.join("c.txt"), "c changed\n").unwrap();

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            "",
            &["c.txt".to_string()],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
        assert_eq!(cached.trim(), "c.txt");
        let worktree = run_out(&repo, &["diff", "--name-only"]);
        assert_eq!(worktree.trim(), "a.txt");
    }

    // --- #214 (M2.17c): line-level staging ---------------------------------

    /// M2.17c acceptance, line-level mechanism end to end on a real
    /// repository: within a single-line replacement (`repo_with_two_hunks`'s
    /// first hunk — `line 2` → `line 2 changed`, its second hunk at `line
    /// 18` untouched throughout), selecting only the ADDED line reclassifies
    /// the removed line to context (so the old content stays present) and
    /// adds the new content alongside it. The clean, non-crossing case; see
    /// `a_crossing_line_selection_reorders_content_exactly_as_git_apply_does`
    /// below for the case where positional reconstruction does something
    /// more surprising.
    #[tokio::test]
    async fn a_line_selection_stages_only_the_selected_replacement() {
        let (_dir, repo) = repo_with_two_hunks();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        // Hunk 0's lines: 0 context "line 1", 1 removed "line 2", 2 added
        // "line 2 changed", 3-5 context (verified against real `git diff`
        // output). Select only the added line.
        let plan = plan_for_lines(
            &parsed,
            0,
            vec![2],
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
        assert_eq!(
            built.patch,
            "--- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,5 +1,6 @@\n\
             \x20line 1\n\
             \x20line 2\n\
             +line 2 changed\n\
             \x20line 3\n\
             \x20line 4\n\
             \x20line 5\n"
        );
        assert!(!built.patch.contains("line 18"));

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        // The index now holds BOTH lines: the untouched original and the
        // freshly staged addition, in that order.
        let staged_content = run_out(&repo, &["show", ":a.txt"]);
        let staged_lines: Vec<&str> = staged_content.lines().collect();
        assert_eq!(staged_lines[0], "line 1");
        assert_eq!(staged_lines[1], "line 2");
        assert_eq!(staged_lines[2], "line 2 changed");
        // The other hunk was never touched.
        assert!(!run_out(&repo, &["diff", "--cached", "--no-color"]).contains("line 18"));
    }

    /// #214: when a line-level selection "crosses" a diff's own grouping —
    /// here the hunk emits both removed lines before both added lines
    /// (`-line4 -line5 +line4changed +line5changed`, not interleaved
    /// remove/add pairs), which is exactly what git's diff algorithm does
    /// for two adjacent single-line replacements — selecting a subset that
    /// spans the boundary reorders content on the new side.
    ///
    /// **Confirmed against real `git apply`, not assumed.** Outside this
    /// suite: hand-built the exact sub-hunk text `append_sub_hunk` produces
    /// for this selection, ran `git apply --cached --whitespace=nowarn
    /// --recount` against a real repository. The resulting index content is
    /// `line 5` immediately followed by `line 4 changed` — reordered
    /// relative to the file's original line order. This is an inherent
    /// property of the unified-diff format itself (new-side content order
    /// is exactly the top-to-bottom order of context+added lines in the
    /// hunk body — module doc) applied to a diff that happened to group its
    /// removes and adds separately; it is not a defect in `append_sub_hunk`.
    /// A real user hand-editing this same hunk in `git add -p`'s `e` (edit)
    /// mode would produce byte-identical text and hit the identical
    /// reordering — confirmed too: `git add -p`'s `s` (split) refuses this
    /// hunk outright ("Sorry, cannot split this hunk"), so `e` is the only
    /// real-git path to a partial selection here, and it edits the same raw
    /// bytes positionally, with no realignment logic of its own.
    ///
    /// This test pins that `append_sub_hunk` reproduces the reordering
    /// exactly, so a future "smarter" rewrite that tries to realign
    /// crossing pairs doesn't silently diverge from what `git apply` itself
    /// does with the bytes this server emits.
    #[tokio::test]
    async fn a_crossing_line_selection_reorders_content_exactly_as_git_apply_does() {
        let (_dir, repo) = repo_with_multiline_hunk();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        // Hunk 0's lines: 0-2 context, 3 removed "line 4", 4 removed
        // "line 5", 5 added "line 4 changed", 6 added "line 5 changed",
        // 7-9 context. Select "-line 4" (3, stays removed) and "+line 4
        // changed" (5, stays added); "-line 5" (4) is left unselected and
        // reclassifies to context, landing BEFORE the selected addition in
        // the sub-hunk body because index 4 precedes index 5 in the
        // original hunk.
        let plan = plan_for_lines(
            &parsed,
            0,
            vec![3, 5],
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        let staged = run_out(&repo, &["show", ":a.txt"]);
        let lines: Vec<&str> = staged.lines().collect();
        // line 4 is gone (its removal was selected); line 5 survives as
        // context, but now sits BEFORE line 4's replacement — the
        // reordering, confirmed to match real `git apply` bit for bit.
        let idx_line5 = lines.iter().position(|l| *l == "line 5").unwrap();
        let idx_line4changed = lines.iter().position(|l| *l == "line 4 changed").unwrap();
        assert!(
            idx_line5 < idx_line4changed,
            "expected the known reordering (line 5 before line 4 changed): {lines:?}"
        );
        assert!(!lines.contains(&"line 4"), "{lines:?}");
    }

    /// #214, Task 4 (the issue's own acceptance bar: "explicit test
    /// coverage, not just staleness rejection") — flavor one: a line-level
    /// selection built against one worktree state is refused once the
    /// *same* file picks up a further, unrelated edit, and stages nothing
    /// as a side effect of the refusal.
    ///
    /// **Honest framing (review finding):** the gate this drives is
    /// `diff-v1:`, a SHA-256 of the entire staging-base diff's bytes
    /// (`handlers/read.rs::staging_diff_for_repo`) — shape-agnostic to
    /// `Hunks` vs `Lines`, the exact same mechanism a whole-file or
    /// whole-hunk selection already relies on. This test proves that
    /// mechanism protects a `Lines` selection too (real coverage the issue
    /// asks for), not that line-level reconstruction has any staleness
    /// exposure of its own — it doesn't; `append_sub_hunk` never reads live
    /// repository state, only the pinned diff already in hand.
    #[tokio::test]
    async fn a_line_level_selection_refuses_after_the_same_file_changes_further() {
        let (_dir, repo) = repo_with_multiline_hunk();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let plan = plan_for_lines(
            &parsed,
            0,
            vec![3, 5],
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

        let stale = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();

        // a.txt picks up a further, unrelated edit (line 8) after the
        // selection was built against the diff above.
        let current = std::fs::read_to_string(repo.join("a.txt")).unwrap();
        std::fs::write(
            repo.join("a.txt"),
            current.replace("line 8\n", "line 8 changed\n"),
        )
        .unwrap();

        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &stale.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{msg}");
        assert!(
            msg.contains("changed while this selection was pending"),
            "{msg}"
        );

        let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
        assert!(
            cached.trim().is_empty(),
            "expected nothing staged after a refused selection, got {cached:?}"
        );
    }

    /// #214, Task 4, flavor two: an edit to a completely unrelated tracked
    /// file (a.txt itself untouched) also moves the diff-v1 token and
    /// refuses the selection just as hard, staging nothing.
    #[tokio::test]
    async fn a_line_level_selection_refuses_after_an_unrelated_file_changes() {
        let (_dir, repo) = repo_with_multiline_hunk();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let plan = plan_for_lines(
            &parsed,
            0,
            vec![3, 5],
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

        let stale = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();

        // b.txt changes; a.txt (and thus the selection's own file) is
        // untouched.
        std::fs::write(repo.join("b.txt"), "unrelated changed\n").unwrap();

        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &stale.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{msg}");
        assert!(
            msg.contains("changed while this selection was pending"),
            "{msg}"
        );

        let cached = run_out(&repo, &["diff", "--cached", "--name-only"]);
        assert!(
            cached.trim().is_empty(),
            "expected nothing staged after a refused selection, got {cached:?}"
        );
    }

    /// #214 review finding (blocker, `append_sub_hunk`): reclassifying an
    /// unselected Removed line to context copied its `no_newline_at_eof`
    /// flag verbatim, even when a later selected Added line still followed
    /// it in the reconstructed body — a self-contradictory patch (`\ No
    /// newline at end of file` attached to a non-final line) that real `git
    /// apply --cached --recount` accepted anyway, silently concatenating the
    /// two lines with no separating newline and corrupting the staged blob.
    /// Confirmed against real git 2.43.0 with the exact production argv
    /// before the fix; this test pins the corrected behavior through the
    /// same real path. A file's committed last line lacks a trailing
    /// newline (`oldlast`); the edit replaces it with `newlast` (also no
    /// trailing newline). Selecting only the Added half (leaving the
    /// Removed half unstaged) must stage BOTH lines, properly separated, not
    /// a merged `oldlastnewlast`.
    #[tokio::test]
    async fn a_reclassified_eof_line_does_not_merge_with_what_follows_it() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("a.txt"), "context\noldlast").unwrap(); // no trailing \n
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "no trailing newline"]);
        std::fs::write(repo.join("a.txt"), "context\nnewlast").unwrap(); // no trailing \n

        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        // Hunk 0's lines: 0 context "context", 1 removed "oldlast" (eof), 2
        // added "newlast" (eof). Select only the added line.
        let plan = plan_for_lines(
            &parsed,
            0,
            vec![2],
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
        assert_eq!(
            built.patch,
            "--- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,2 +1,3 @@\n\
             \x20context\n\
             -oldlast\n\
             \\ No newline at end of file\n\
             +oldlast\n\
             +newlast\n\
             \\ No newline at end of file\n"
        );

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        // Byte-exact: three properly newline-separated lines, no merge, no
        // trailing newline after the true final line.
        let staged = std::process::Command::new("git")
            .args(["show", ":a.txt"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout;
        assert_eq!(staged, b"context\noldlast\nnewlast");
    }

    /// #214 review finding (should-fix): every existing line-level test
    /// selects lines from a single `HunkLines` entry. `PatchPlan::validate`
    /// already requires `Vec<HunkLines>` support (`well_ordered` checks
    /// ordinals strictly ascend across it) and `append_file_patch_lines`
    /// already loops over every entry — but nothing drove more than one.
    /// This selects the added line of `repo_with_two_hunks`'s *first* hunk
    /// (index 2 of hunk 0: `line 2` -> `line 2 changed`) AND the added line
    /// of its *second*, unrelated hunk (index 4 of hunk 1: `line 18` ->
    /// `line 18 changed`) in one `PatchPlan`, and proves both land in the
    /// index from a single apply.
    #[tokio::test]
    async fn a_multi_hunk_line_level_selection_stages_both_hunks_lines() {
        let (_dir, repo) = repo_with_two_hunks();
        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
            panic!("expected a hunks-shaped file");
        };
        assert_eq!(hunks.len(), 2, "fixture drift: expected two hunks");
        let plan = git_vista_protocol::PatchPlan {
            repository: RepositoryToken::new("test-repo").unwrap(),
            worktree: WorktreeToken::new("test-worktree").unwrap(),
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            direction: git_vista_protocol::StageDirection::Stage,
            files: vec![git_vista_protocol::FileSelection {
                path: "a.txt".to_string(),
                selection: git_vista_protocol::SelectionShape::Lines {
                    hunks: vec![
                        git_vista_protocol::HunkLines {
                            hunk: git_vista_protocol::HunkRef {
                                index: 0,
                                old_start: hunks[0].old_start,
                                new_start: hunks[0].new_start,
                            },
                            lines: vec![2],
                        },
                        git_vista_protocol::HunkLines {
                            hunk: git_vista_protocol::HunkRef {
                                index: 1,
                                old_start: hunks[1].old_start,
                                new_start: hunks[1].new_start,
                            },
                            lines: vec![4],
                        },
                    ],
                },
            }],
        };
        assert_eq!(plan.validate(), Ok(()));
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
        assert!(built.patch.contains("line 2 changed"));
        assert!(built.patch.contains("line 18 changed"));

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        let staged = run_out(&repo, &["show", ":a.txt"]);
        assert!(staged.contains("line 2\nline 2 changed"), "{staged}");
        assert!(staged.contains("line 18\nline 18 changed"), "{staged}");
    }

    /// #214, Task 3: byte-exact CRLF round trip through a REAL `git apply`
    /// (not just an assertion on the built string, per the task brief).
    #[tokio::test]
    async fn a_hunk_of_a_crlf_file_applies_byte_exact() {
        let (_dir, repo) = seeded_repo();
        let body = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";
        std::fs::write(repo.join("crlf.txt"), body).unwrap();
        run(&repo, &["add", "crlf.txt"]);
        run(&repo, &["commit", "-q", "-m", "crlf file"]);
        let edited = "one\r\nTWO\r\nthree\r\nfour\r\nFIVE\r\n";
        std::fs::write(repo.join("crlf.txt"), edited).unwrap();

        let diff = run_out(&repo, &["diff", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let git_vista_protocol::FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
            panic!("expected Hunks");
        };
        // Confirm the \r survived parsing before trusting reconstruction.
        assert!(
            hunks[0].lines.iter().any(|l| l.text.ends_with('\r')),
            "{:?}",
            hunks[0].lines
        );

        let plan = plan_for_hunk_at(
            &parsed,
            "crlf.txt",
            0,
            git_vista_protocol::StageDirection::Stage,
        );
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Stage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Stage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        // Byte-exact: the staged blob still has \r\n line endings, matching
        // the worktree file exactly — not the LF-only bytes str::lines()
        // used to silently produce (diff.rs's split_diff_lines fix).
        let staged = run_out(&repo, &["show", ":crlf.txt"]);
        assert!(staged.contains("TWO\r\n"), "{staged:?}");
        assert!(staged.contains("FIVE\r\n"), "{staged:?}");
        assert!(staged.contains("one\r\n"), "{staged:?}");
    }

    /// #214, Task 2 (renames-with-content): empirically confirmed, not
    /// assumed.
    ///
    /// `append_file_patch`/`append_file_patch_lines` emit only `--- a/<old>`
    /// / `+++ b/<new>` and hunks for a renamed-and-edited file — no `rename
    /// from`/`rename to`/`similarity index` lines, because the parser
    /// deliberately drops those (diff.rs's module doc: a rename with
    /// content edits parses as plain `FileDiff::Hunks`, not `Renamed`).
    ///
    /// **This shape is only ever reachable in the Unstage direction.**
    /// Verified against a real repo: the Stage direction's base diff is a
    /// bare `git diff` (worktree vs index); with the rename done via a plain
    /// filesystem move (not staged), the old path shows as a plain deletion
    /// and the new path, being untracked, is invisible to `git diff`
    /// entirely (untracked files never appear in it — confirmed directly).
    /// A `FileDiff::Hunks` entry with `old_path != new_path` can only come
    /// from a diff that compares two *trees* where git's own rename
    /// detection paired an old and a new path — `index-vs-HEAD` (`git diff
    /// --cached` after `git add -A` of a rename+edit) or a commit/ref
    /// comparison. Of those, only Unstage's `index-vs-HEAD` base is wired to
    /// this server's staging surface today.
    ///
    /// **`git apply --cached --reverse` handles the headerless form
    /// correctly as-is** — verified directly outside this suite: staged a
    /// rename+edit (`git add -A` of a `mv` + content edit), built the exact
    /// `--- a/old +++ b/new` + hunk text this server emits, ran `git apply
    /// --cached --reverse --recount` against the real repo. Result: the
    /// rename stayed staged (still `R`, still 100% similarity — the
    /// *content* hunk is what got reversed, not the rename), the reversed
    /// content landed correctly at the *new* path in the worktree (never at
    /// `old_path`, which no longer exists anywhere), and the index still
    /// held the pre-edit content at the new path. No `rename from`/`rename
    /// to` lines were needed. The forward (`--cached`, no `--reverse`)
    /// direction, by contrast, does need them — attempted without, it fails
    /// outright (`"<new path>: does not exist in index"`) — but the forward
    /// direction is exactly the one this shape can never reach (previous
    /// paragraph), so nothing here needs the headers added. This test
    /// exercises the reachable (Unstage) leg end to end; the forward leg's
    /// unreachability is the empirical finding, not something a passing
    /// test can assert.
    #[tokio::test]
    async fn unstaging_a_content_hunk_of_a_renamed_file_reverses_only_the_content() {
        let (_dir, repo) = repo_with_staged_rename_and_edit();

        let staged_before = run_out(&repo, &["diff", "--cached", "--no-color"]);
        assert!(
            staged_before.contains("rename from a.txt"),
            "{staged_before}"
        );
        assert!(
            staged_before.contains("rename to renamed.txt"),
            "{staged_before}"
        );

        let diff = run_out(&repo, &["diff", "--cached", "--no-color", "--no-textconv"]);
        let parsed = git_vista_protocol::parse_unified_diff(&diff);
        let (old_path, new_path, hunk0) = match &parsed.files[0] {
            git_vista_protocol::FileDiff::Hunks {
                old_path,
                new_path,
                hunks,
            } => (old_path.clone(), new_path.clone(), hunks[0].clone()),
            other => panic!("expected a rename-with-edit to parse as Hunks, got {other:?}"),
        };
        assert_eq!(old_path.as_deref(), Some("a.txt"));
        assert_eq!(new_path.as_deref(), Some("renamed.txt"));

        let plan = git_vista_protocol::PatchPlan {
            repository: RepositoryToken::new("test-repo").unwrap(),
            worktree: WorktreeToken::new("test-worktree").unwrap(),
            generation: GenerationToken::new("diff-v1:test").unwrap(),
            direction: git_vista_protocol::StageDirection::Unstage,
            files: vec![git_vista_protocol::FileSelection {
                path: "renamed.txt".to_string(),
                selection: git_vista_protocol::SelectionShape::Hunks {
                    hunks: vec![git_vista_protocol::HunkRef {
                        index: 0,
                        old_start: hunk0.old_start,
                        new_start: hunk0.new_start,
                    }],
                },
            }],
        };
        let built = git_vista_protocol::build_selected_patch(&parsed, &plan).unwrap();
        assert!(built.patch.starts_with("--- a/a.txt\n+++ b/renamed.txt\n"));
        assert!(
            !built.patch.contains("rename from"),
            "no rename headers should be needed: {}",
            built.patch
        );

        let live = crate::handlers::read::staging_diff_for_repo(
            &repo,
            git_vista_protocol::StageDirection::Unstage,
        )
        .await
        .unwrap();
        let (status, msg) = exec_stage_selection(
            &repo,
            NetworkNeed::Local,
            git_vista_protocol::StageDirection::Unstage,
            &live.generation,
            &built.patch,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{msg}");

        // The rename itself is still staged, untouched, still 100%
        // similarity — only the content hunk was reversed.
        let staged_after = run_out(&repo, &["diff", "--cached", "--no-color"]);
        assert!(staged_after.contains("rename from a.txt"), "{staged_after}");
        assert!(
            staged_after.contains("similarity index 100%"),
            "{staged_after}"
        );
        assert!(!staged_after.contains("line 3 changed"), "{staged_after}");

        // The content change is back in the worktree, at the NEW path —
        // never at old_path, which doesn't exist anywhere anymore.
        assert!(!repo.join("a.txt").exists());
        let worktree_content = std::fs::read_to_string(repo.join("renamed.txt")).unwrap();
        assert!(
            worktree_content.contains("line 3 changed"),
            "{worktree_content}"
        );
    }

    /// #145 acceptance 2: a plan whose operation no longer matches its
    /// declared hash is refused (tamper detection).
    #[tokio::test]
    async fn a_tampered_operation_is_refused() {
        let (_dir, repo) = seeded_repo();
        let (mut plan, _observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
        plan.operation = GitOperation::UnstageAll; // no longer what the hash approves
        let (status, why) = validate(&plan).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("doesn't match"), "{why}");
    }

    /// #145 acceptance 3: an expired plan is refused with a reason the client
    /// can show.
    #[tokio::test]
    async fn an_expired_plan_is_refused() {
        let (_dir, repo) = seeded_repo();
        let (mut plan, _observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
        plan.expires_at = UnixSeconds(crate::activity::now_secs() - 1);
        let (status, why) = validate(&plan).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("expired"), "{why}");
    }

    /// #145 acceptance 4, precondition flavor: a precondition that *held* at
    /// build time and broke before execution refuses — here the push remote
    /// disappears, which moves no ref and so slips past the generation check.
    #[tokio::test]
    async fn a_broken_precondition_refuses_execution() {
        let (_dir, repo) = seeded_repo();
        run(&repo, &["remote", "add", "origin", "/nowhere/upstream.git"]);
        let op = GitOperation::PushBranch {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(),
            set_upstream: false,
            force: ForcePublish::None,
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;
        assert!(
            observed.held_at_build.iter().any(|&h| h),
            "remote precondition should hold"
        );
        assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

        run(&repo, &["remote", "remove", "origin"]);
        let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("no longer configured"), "{why}");
    }

    /// A precondition that already failed at build time is *not* enforced here:
    /// it flows to the executor's legacy guard so refusal texts stay exactly
    /// what they always were.
    #[tokio::test]
    async fn a_precondition_unmet_at_build_time_is_left_to_the_executor() {
        let (_dir, repo) = seeded_repo();
        let op = GitOperation::PushBranch {
            branch: BranchName::new("main").unwrap(),
            remote: RemoteName::new("origin").unwrap(), // never configured
            set_upstream: false,
            force: ForcePublish::None,
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;
        assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());
    }

    // -----------------------------------------------------------------------
    // M2.19a (#222): `GitOperation::AmendCommit`'s `shape` — contract only,
    // no execution (see the variant's own doc comment and planner::execute's
    // stub arm). These pin the plan-building side: risk, the CAS
    // precondition, the expected ref change, and the recovery strategy.
    // -----------------------------------------------------------------------

    /// The happy-path shape: `Destructive` risk, a `BranchCheckedOut` +
    /// `RefAt(expected_tip)` precondition pair on the checked-out branch, a
    /// `Computed` ref change from `expected_tip`, and `ResetRef` recovery
    /// back to `expected_tip` — exactly the design the variant's doc comment
    /// argues for, pinned so a later edit that quietly reached for
    /// `RecoverableIfStaged` or `Irrecoverable` instead fails here.
    #[tokio::test]
    async fn amend_commit_shape_is_destructive_with_cas_precondition_and_reset_recovery() {
        let (_dir, repo) = seeded_repo();
        let head = git_rev_parse_head(&repo).await;
        let head_oid = CommitOid::new(head.clone()).unwrap();

        let op = GitOperation::AmendCommit {
            message: CommitMessage::new("fix: typo").unwrap(),
            expected_tip: head_oid.clone(),
            allow_empty: false,
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;

        assert_eq!(plan.risk, RiskLevel::Destructive);
        assert_eq!(
            plan.preconditions,
            vec![
                Precondition::BranchCheckedOut {
                    branch: BranchName::new("main").unwrap(),
                },
                Precondition::RefAt {
                    ref_name: RefName::new("refs/heads/main").unwrap(),
                    oid: head_oid.clone(),
                },
            ]
        );
        assert_eq!(
            plan.expected_ref_changes,
            vec![RefChange {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                before: RefState::At(head_oid.clone()),
                after: RefState::Computed,
            }]
        );
        assert_eq!(
            plan.recovery,
            RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: head_oid,
            }
        );
        // Both preconditions genuinely hold against the freshly seeded repo —
        // proves the shape isn't vacuously satisfied by an always-true check.
        assert!(observed.held_at_build.iter().all(|&h| h));
    }

    /// `expected_tip` is a *live* check, not a value the plan merely carries:
    /// build a plan whose `expected_tip` matches HEAD, then let another
    /// commit land before execution. `refs/heads/main` moving trips
    /// `enforce_fresh`'s generation check before its per-precondition loop
    /// ever runs — the same layering every other tip-moved race in this
    /// codebase goes through (`a_generation_move_refuses_execution`;
    /// `EmptyCommitOnBranch` and `ResetBranch`'s own `RefAt` preconditions
    /// are shadowed by it too, for the identical reason: any ref move is by
    /// construction also a generation move). The named `RefAt` precondition
    /// still earns its place — it is what the reviewer/UI sees named and
    /// individually reviewable in `Plan::preconditions`, and it is the
    /// backstop `verify_precondition` would use should a future generation
    /// algorithm ever narrow which refs it digests. What matters here, and
    /// what this test actually proves, is the end-to-end guarantee: a plan
    /// built against one tip is refused, not silently honoured, once that
    /// tip has moved.
    #[tokio::test]
    async fn amend_commit_refuses_when_the_tip_moved_after_the_plan_was_built() {
        let (_dir, repo) = seeded_repo();
        let head = git_rev_parse_head(&repo).await;

        let op = GitOperation::AmendCommit {
            message: CommitMessage::new("fix: typo").unwrap(),
            expected_tip: CommitOid::new(head).unwrap(),
            allow_empty: false,
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;
        assert!(
            observed.held_at_build.iter().all(|&h| h),
            "both preconditions should hold at build time"
        );
        assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

        // The race: another commit lands on main before this plan executes.
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "raced ahead"]);

        let (status, why) = enforce_fresh(&repo, &plan, &observed).await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(why.contains("repository changed"), "{why}");
    }

    // -----------------------------------------------------------------------
    // M2.19b (#223): `classify_amend_failure` — the pure classification the
    // wire's `AmendFailureKind` rests on. Driven branch by branch with the
    // stderr shapes captured from a real git 2.43 (see the function's doc
    // comment), plus the paired negatives that keep each leg from going
    // vacuous. The end-to-end versions (real hooks, real failed signers,
    // through the full pipeline) live in `contract_suite`.
    // -----------------------------------------------------------------------

    /// Every classification branch, with its paired negative on the same
    /// row: the input that must NOT take that branch differs from the
    /// matching one by exactly the load-bearing fact.
    #[test]
    fn classify_amend_failure_covers_every_branch_with_paired_negatives() {
        use AmendFailureKind::*;
        // Captured verbatim from git 2.43 (scratch experiments, 2026-08-02).
        let gpg = "error: gpg failed to sign the data:\n(no gpg output)\nfatal: failed to write commit object";
        let ssh = "error: Couldn't load public key /k: No such file or directory?\n\nfatal: failed to write commit object";
        let empty_amend = "You asked to amend the most recent commit, but doing so would make\nit empty. You can repeat your command with --allow-empty, or you can\nremove the commit entirely with \"git reset HEAD^\".";
        let merge_fatal = "fatal: You are in the middle of a merge -- cannot amend.";

        // (stderr, signing_requested, hook_present) → expected kind, and why.
        let cases: &[(&str, bool, bool, AmendFailureKind, &str)] = &[
            // -- signing, gpg format: the canonical line decides alone --
            (gpg, true, false, SigningFailed, "gpg line, signing on"),
            (
                gpg,
                false,
                false,
                SigningFailed,
                "the canonical gpg line is decisive even unprobed",
            ),
            (
                gpg,
                true,
                true,
                SigningFailed,
                "signing outranks a present hook",
            ),
            // -- signing, ssh format: needs the config probe --
            (
                ssh,
                true,
                false,
                SigningFailed,
                "ssh-format signer failure with signing configured",
            ),
            (
                ssh,
                false,
                false,
                Other,
                "paired negative: the identical stderr WITHOUT signing configured is a \
              plain object-write failure — blaming the signer would hide disk trouble",
            ),
            // -- hook rejection: silence plus a hook, and nothing fatal --
            (
                "",
                false,
                true,
                HookRejected,
                "the real shape: silent hook, empty stderr",
            ),
            (
                "nope: bad message",
                false,
                true,
                HookRejected,
                "a chatty hook is still a hook",
            ),
            (
                "",
                false,
                false,
                Other,
                "paired negative: the identical silence with NO hook present must not \
              invent a hook to blame",
            ),
            (
                merge_fatal,
                false,
                true,
                Other,
                "paired negative: git's own fatal refusals never classify as a hook, \
              hook present or not — the fatal: prefix is die()'s, unlocalized",
            ),
            (
                empty_amend,
                false,
                true,
                Other,
                "paired negative: the would-become-empty advice is git's, not the \
              hook's, even though it is non-fatal and a hook is present",
            ),
            // -- everything else --
            (
                merge_fatal,
                false,
                false,
                Other,
                "an ordinary fatal is Other",
            ),
            (
                empty_amend,
                false,
                false,
                Other,
                "the empty-amend advice is Other",
            ),
        ];
        for (stderr, signing, hook, expected, why) in cases {
            assert_eq!(
                classify_amend_failure(stderr, *signing, *hook),
                *expected,
                "{why} (stderr={stderr:?}, signing={signing}, hook={hook})"
            );
        }
    }

    // -----------------------------------------------------------------------
    // M2.20a (#227): `FetchRemote` / `PullBranch` / the widened `PushBranch`
    // in `shape` — contract only, no execution (see each variant's doc
    // comment in `plan.rs` and `planner::execute`'s stub arms).
    // -----------------------------------------------------------------------

    /// A repository with a real, configured `origin` on disk — several shape
    /// tests below need `RemoteConfigured` to actually hold, so that
    /// `held_at_build` proving the preconditions are satisfiable means
    /// something.
    async fn seeded_repo_with_remote() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        let remote = dir.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "--bare", "-b", "main"]);
        run(
            &repo,
            &["remote", "add", "origin", &remote.display().to_string()],
        );
        (dir, repo)
    }

    /// Fetch is `Safe` with `NotNeeded` recovery, one `RemoteConfigured`
    /// precondition, and **no** expected ref change.
    ///
    /// The negative assertions are the point. `Safe`/`NotNeeded` is an
    /// unusual pairing for a network operation, and the plausible wrong
    /// answers are exactly the ones a later edit would reach for by reflex:
    /// `RiskLevel::Remote` (because it talks to a remote) or
    /// `RecoveryStrategy::Irrecoverable` (because push has it). Both are
    /// pinned as *not* the answer, with the reasoning in the variant's doc
    /// comment — a fetch cannot lose anything a user owns.
    #[tokio::test]
    async fn fetch_remote_shape_is_safe_with_nothing_to_recover() {
        let (_dir, repo) = seeded_repo_with_remote().await;
        let op = GitOperation::FetchRemote {
            remote: RemoteName::new("origin").unwrap(),
        };
        let (plan, observed) = build_plan(&repo, op, tokens()).await;

        assert_eq!(plan.risk, RiskLevel::Safe);
        assert_ne!(
            plan.risk,
            RiskLevel::Remote,
            "reach and risk are independent axes — see the variant's doc"
        );
        assert_eq!(
            plan.preconditions,
            vec![Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            }]
        );
        assert!(
            plan.expected_ref_changes.is_empty(),
            "which refs/remotes/* move is unknowable before git speaks to the \
             remote; a guessed RefChange would be a claim shown to a reviewer"
        );
        assert_eq!(plan.recovery, RecoveryStrategy::NotNeeded);
        assert_ne!(plan.recovery, RecoveryStrategy::Irrecoverable);
        assert!(
            observed.held_at_build.iter().all(|&h| h),
            "the remote is configured, so the one precondition must hold — \
             otherwise this test would pass against an unsatisfiable shape"
        );
    }

    /// Pull is `Reversible` with a CAS on the **local** branch and `ResetRef`
    /// recovery back to the tip the plan observed — the same story merge and
    /// rebase have, because a pull is a fetch plus one of those.
    ///
    /// Two negatives carry the reasoning: it must not be `Irrecoverable`
    /// (that is push's tag, for an effect that left the machine — a pull's
    /// did not), and its `RefAt` must name `refs/heads/main`, not
    /// `refs/remotes/origin/main`. Pinning the remote tip would refuse a pull
    /// for the ordinary reason that the remote received a commit, i.e. for
    /// the very thing being pulled.
    #[tokio::test]
    async fn pull_branch_shape_is_reversible_with_a_local_cas_and_reset_recovery() {
        let (_dir, repo) = seeded_repo_with_remote().await;
        let head_oid = CommitOid::new(git_rev_parse_head(&repo).await).unwrap();
        let main = RefName::new("refs/heads/main").unwrap();

        for strategy in [
            git_vista_protocol::MergeStrategy::Merge,
            git_vista_protocol::MergeStrategy::Rebase,
        ] {
            let op = GitOperation::PullBranch {
                remote: RemoteName::new("origin").unwrap(),
                branch: BranchName::new("main").unwrap(),
                strategy,
            };
            let (plan, observed) = build_plan(&repo, op, tokens()).await;

            assert_eq!(plan.risk, RiskLevel::Reversible, "{strategy:?}");
            assert_eq!(
                plan.preconditions,
                vec![
                    Precondition::BranchCheckedOut {
                        branch: BranchName::new("main").unwrap(),
                    },
                    Precondition::RemoteConfigured {
                        remote: RemoteName::new("origin").unwrap(),
                    },
                    Precondition::RefAt {
                        ref_name: main.clone(),
                        oid: head_oid.clone(),
                    },
                ],
                "{strategy:?}"
            );
            assert!(
                !plan.preconditions.iter().any(|p| matches!(
                    p,
                    Precondition::RefAt { ref_name, .. }
                        if ref_name.as_str().starts_with("refs/remotes/")
                )),
                "{strategy:?}: a pull must not pin the remote tip — that would \
                 refuse the pull for the reason it exists"
            );
            assert_eq!(
                plan.expected_ref_changes,
                vec![RefChange {
                    ref_name: main.clone(),
                    before: RefState::At(head_oid.clone()),
                    after: RefState::Computed,
                }],
                "{strategy:?}"
            );
            assert_eq!(
                plan.recovery,
                RecoveryStrategy::ResetRef {
                    ref_name: main.clone(),
                    to: head_oid.clone(),
                },
                "{strategy:?}"
            );
            assert_ne!(
                plan.recovery,
                RecoveryStrategy::Irrecoverable,
                "{strategy:?}: a pull's effect never left this machine"
            );
            assert!(observed.held_at_build.iter().all(|&h| h), "{strategy:?}");
        }
    }

    /// The lease is a compare-and-swap on the **remote-tracking** ref, and it
    /// exists only when a lease was actually asked for.
    ///
    /// Both halves run against the same repository, so the difference is
    /// attributable to `force` and nothing else. Without the negative half a
    /// `shape` that emitted the lease precondition unconditionally would pass
    /// — and an unconditional precondition on `refs/remotes/origin/main`
    /// would refuse ordinary pushes whenever the remote had moved, which is
    /// most of the time.
    #[tokio::test]
    async fn only_a_lease_force_push_pins_the_remote_tracking_ref() {
        let (_dir, repo) = seeded_repo_with_remote().await;
        let tracking = RefName::new("refs/remotes/origin/main").unwrap();
        let lease_tip = CommitOid::new("4".repeat(40)).unwrap();

        let lease_precondition = |plan: &Plan| {
            plan.preconditions
                .iter()
                .find(
                    |p| matches!(p, Precondition::RefAt { ref_name, .. } if *ref_name == tracking),
                )
                .cloned()
        };

        let (plain, _) = build_plan(
            &repo,
            GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: ForcePublish::None,
            },
            tokens(),
        )
        .await;
        assert_eq!(plain.risk, RiskLevel::Remote);
        assert_eq!(
            lease_precondition(&plain),
            None,
            "a fast-forward push must not pin the remote tip"
        );

        let (leased, _) = build_plan(
            &repo,
            GitOperation::PushBranch {
                branch: BranchName::new("main").unwrap(),
                remote: RemoteName::new("origin").unwrap(),
                set_upstream: false,
                force: ForcePublish::WithLease {
                    expected_remote_tip: lease_tip.clone(),
                },
            },
            tokens(),
        )
        .await;
        assert_eq!(
            leased.risk,
            RiskLevel::Destructive,
            "a lease-force can leave remote commits referenced by nothing"
        );
        assert_eq!(
            lease_precondition(&leased),
            Some(Precondition::RefAt {
                ref_name: tracking,
                oid: lease_tip.clone(),
            }),
            "the lease must become a live compare-and-swap on the tracking ref"
        );
        // The oid must be the *reviewed* one, not one re-read from the repo.
        // A lease re-derived at plan time would assert only that the remote
        // has not moved since a millisecond ago, and would protect nobody.
        assert_ne!(
            lease_tip.as_str(),
            git_rev_parse_head(&repo).await,
            "the fixture's lease oid must differ from anything in the repo, or \
             this test could not tell a carried oid from a re-read one"
        );
        // Recovery is unchanged by the force mode: the effect left the machine
        // either way.
        assert_eq!(plain.recovery, RecoveryStrategy::Irrecoverable);
        assert_eq!(leased.recovery, RecoveryStrategy::Irrecoverable);
    }

    // -----------------------------------------------------------------------
    // D5 (#66, Task 19): ExecUnavailable propagates as its own value.
    //
    // Every test below drives a *real* unrunnable repository
    // (`git_cmd::unrunnable_repo` — a `.git` whose geometry the sandbox policy
    // refuses, so no git is ever spawned). Nothing here is stubbed, and none
    // of it would pass if `rev_parse` had simply been made infallible.
    // -----------------------------------------------------------------------

    /// An `Observed` with no unreadable fields, for the precondition checks
    /// that only consult `live.head_branch` / `live.status`.
    fn live_observed() -> Observed {
        Observed {
            head_branch: Some("main".to_string()),
            head_tip: Obs::Known("0".repeat(40)),
            branch_tip: Obs::Absent,
            status: Obs::Known(String::new()),
            held_at_build: Vec::new(),
        }
    }

    fn ref_name(s: &str) -> RefName {
        RefName::new(s).expect("valid ref name")
    }

    /// **The gate criterion.** `resolve_commit_oid` is an id-resolution gate,
    /// and before D5 it answered the *same* 400 "not a valid object name" for
    /// "git rejected this name" and for "git never ran". Those are now
    /// different statuses, and the git-unavailable one must not be a 4xx: the
    /// request was fine.
    #[tokio::test]
    async fn a_gate_distinguishes_git_unavailable_from_a_ref_that_is_absent() {
        let (_dir, repo) = seeded_repo();
        let (_hostile_dir, hostile) = crate::git_cmd::unrunnable_repo();

        // git ran and refused the name: the client's request is wrong.
        let (absent_status, absent_why) = resolve_commit_oid(&repo, "no-such-rev")
            .await
            .expect_err("a bogus rev must be refused");
        assert_eq!(absent_status, StatusCode::BAD_REQUEST);
        assert!(
            absent_why.contains("not a valid object name"),
            "git's own wording is preserved for the real refusal: {absent_why}"
        );

        // git never ran: nothing was refused, so nothing may be blamed on the
        // request.
        let (unavailable_status, unavailable_why) = resolve_commit_oid(&hostile, "no-such-rev")
            .await
            .expect_err("an unrunnable repository must be refused");
        assert_eq!(
            unavailable_status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "‘git could not run’ is a server fault, never a 400"
        );
        assert!(
            unavailable_why.contains("Couldn't run git"),
            "{unavailable_why}"
        );
        assert!(
            !unavailable_why.contains("not a valid object name"),
            "the old text asserted the user's input was bad on no evidence: \
             {unavailable_why}"
        );
        assert_ne!(
            absent_status, unavailable_status,
            "the two outcomes must be distinguishable by status alone"
        );
    }

    /// **The polarity criterion.** `RefAbsent` used to be *satisfied* by an
    /// unreadable ref, while its two siblings refused on the identical input.
    ///
    /// The first assertion reproduces the old expression verbatim against the
    /// same fixture, so this is a regression pin and not merely a statement of
    /// current behaviour: if `rev_parse` ever collapses back to a two-state
    /// answer, that line is what the collapse would restore.
    #[tokio::test]
    async fn ref_absent_no_longer_treats_an_unreadable_ref_as_absent() {
        let (_hostile_dir, hostile) = crate::git_cmd::unrunnable_repo();
        let name = ref_name("refs/heads/feature");
        let live = live_observed();

        // The pre-D5 logic, written out: `rev_parse(...).await.is_none()`,
        // where `None` meant either "absent" or "git could not run".
        let pre_d5_said_absent = rev_parse(&hostile, name.as_str())
            .await
            .ok()
            .flatten()
            .is_none();
        assert!(
            pre_d5_said_absent,
            "the fixture must be one where the old expression answered \
             ‘absent’, or this test pins nothing"
        );

        let (status, why) = verify_precondition(
            &hostile,
            &Precondition::RefAbsent {
                ref_name: name.clone(),
            },
            &live,
        )
        .await
        .expect_err("an unreadable ref is not proof the ref is absent");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(why.contains("Couldn't run git"), "{why}");

        // And its two siblings, on the identical input, agree — the asymmetry
        // is gone rather than inverted.
        for precondition in [
            Precondition::RefExists {
                ref_name: name.clone(),
            },
            Precondition::RefAt {
                ref_name: name.clone(),
                oid: CommitOid::new("0".repeat(40)).unwrap(),
            },
        ] {
            let (status, _) = verify_precondition(&hostile, &precondition, &live)
                .await
                .expect_err("every ref precondition refuses on an unreadable ref");
            assert_eq!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "and all three now use the *same* status for it"
            );
        }
    }

    /// The fix must not have been "refuse always": on a repository git can
    /// run in, `RefAbsent` still passes for a branch that really is absent and
    /// still refuses for one that exists. Without this, the test above would
    /// pass against a `verify_precondition` that had been broken outright.
    #[tokio::test]
    async fn ref_absent_still_distinguishes_a_real_absence_from_a_real_ref() {
        let (_dir, repo) = seeded_repo();
        let live = live_observed();

        verify_precondition(
            &repo,
            &Precondition::RefAbsent {
                ref_name: ref_name("refs/heads/never-created"),
            },
            &live,
        )
        .await
        .expect("a branch that does not exist satisfies RefAbsent");

        let (status, why) = verify_precondition(
            &repo,
            &Precondition::RefAbsent {
                ref_name: ref_name("refs/heads/main"),
            },
            &live,
        )
        .await
        .expect_err("a branch that exists breaks RefAbsent");
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a ref that really is there is a 409 about the repository, \
             not a 500 about us"
        );
        assert!(
            why.contains("appeared while this plan was pending"),
            "{why}"
        );
    }

    /// **The freshness criterion.** Two `Unknown` observations must not
    /// produce equal generation tokens, or the staleness gate compares two
    /// non-observations, finds them "the same", and certifies as unchanged a
    /// repository nobody read.
    ///
    /// The control in the middle is what makes this non-vacuous: two identical
    /// *real* observations must still compare equal, so the property being
    /// pinned is "unknown is uncomparable", not "the token is random".
    #[tokio::test]
    async fn two_unknown_observations_never_compare_equal() {
        let (_dir, repo) = seeded_repo();

        let unknown = || Observed {
            head_branch: Some("main".to_string()),
            head_tip: Obs::Unknown,
            branch_tip: Obs::Absent,
            status: Obs::Known(String::new()),
            held_at_build: Vec::new(),
        };
        let known = || Observed {
            head_branch: Some("main".to_string()),
            head_tip: Obs::Known("abc123".to_string()),
            branch_tip: Obs::Absent,
            status: Obs::Known(String::new()),
            held_at_build: Vec::new(),
        };
        let absent = || Observed {
            head_branch: Some("main".to_string()),
            head_tip: Obs::Absent,
            branch_tip: Obs::Absent,
            status: Obs::Known(String::new()),
            held_at_build: Vec::new(),
        };

        // Control: two identical real observations DO compare equal. Without
        // this the whole freshness gate would be broken, not fixed.
        assert_eq!(
            generation_token(&repo, &known()).await.as_str(),
            generation_token(&repo, &known()).await.as_str(),
            "a real observation must be reproducible, or nothing is ever fresh"
        );
        assert_eq!(
            generation_token(&repo, &absent()).await.as_str(),
            generation_token(&repo, &absent()).await.as_str(),
        );

        // The criterion: two unknowns do not.
        assert_ne!(
            generation_token(&repo, &unknown()).await.as_str(),
            generation_token(&repo, &unknown()).await.as_str(),
            "two failed reads must not certify each other as unchanged"
        );

        // And unknown is distinguishable from both of the real answers.
        assert_ne!(
            generation_token(&repo, &unknown()).await.as_str(),
            generation_token(&repo, &absent()).await.as_str(),
        );
        assert_ne!(
            generation_token(&repo, &unknown()).await.as_str(),
            generation_token(&repo, &known()).await.as_str(),
        );
    }

    /// The digest tags are load-bearing on their own: an observed empty status
    /// (a *clean* worktree) must not hash the same as one that could not be
    /// read. Pre-D5 both went in as `""` via `unwrap_or_default`.
    #[tokio::test]
    async fn a_clean_worktree_does_not_hash_like_an_unreadable_one() {
        let (_dir, repo) = seeded_repo();
        let with = |status| Observed {
            head_branch: Some("main".to_string()),
            head_tip: Obs::Known("abc123".to_string()),
            branch_tip: Obs::Absent,
            status,
            held_at_build: Vec::new(),
        };
        assert_ne!(
            generation_token(&repo, &with(Obs::Known(String::new())))
                .await
                .as_str(),
            generation_token(&repo, &with(Obs::Absent)).await.as_str(),
            "‘clean’ and ‘not a working tree’ are different states"
        );
    }

    /// The gate is wired, not merely capable: a plan whose build-time
    /// observation was `Unknown` is refused by `enforce_fresh` with the
    /// git-unavailable status — and says so, rather than blaming the
    /// repository for changing.
    #[tokio::test]
    async fn enforce_fresh_refuses_a_plan_built_on_an_unreadable_observation() {
        let (_dir, repo) = seeded_repo();
        let (plan, mut observed) = build_plan(&repo, GitOperation::StageAll, tokens()).await;
        assert!(enforce_fresh(&repo, &plan, &observed).await.is_ok());

        observed.head_tip = Obs::Unknown;
        let (status, why) = enforce_fresh(&repo, &plan, &observed)
            .await
            .expect_err("an unreadable observation cannot certify freshness");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(why.contains("Couldn't run git"), "{why}");
        assert!(
            !why.contains("changed while this plan was pending"),
            "we have no evidence the repository changed: {why}"
        );
    }

    /// The comparison behind “Already up to date”. `exec_merge` and
    /// `exec_rebase` decide whether HEAD moved by calling
    /// [`Obs::same_observation`]; two unreadable tips must not answer "it
    /// didn't".
    ///
    /// Note that `new == observed.head_tip` — what those two sites used to say
    /// — no longer compiles at all: [`Obs`] deliberately has no `PartialEq`.
    #[test]
    fn two_unknown_tips_are_not_the_same_observation() {
        let unknown: Obs<String> = Obs::Unknown;
        assert!(
            !unknown.same_observation(&Obs::Unknown),
            "two reads that saw nothing are not evidence that nothing moved"
        );
        assert!(!unknown.same_observation(&Obs::Absent));
        assert!(!unknown.same_observation(&Obs::Known("a".into())));
        assert!(!Obs::Known("a".to_string()).same_observation(&Obs::Unknown));

        // The real answers still compare the way the callers need.
        assert!(Obs::Known("a".to_string()).same_observation(&Obs::Known("a".to_string())));
        assert!(!Obs::Known("a".to_string()).same_observation(&Obs::Known("b".to_string())));
        assert!(Obs::<String>::Absent.same_observation(&Obs::Absent));
    }
}
