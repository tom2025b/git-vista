//! `POST /api/plan` — build one reviewable [`Plan`] and return it, executing
//! nothing (M2.23d, #248; ADR 0046).
//!
//! This is the *only* endpoint in the server that mints a plan without also
//! running it. Its whole reason to exist is the client-review roundtrip an
//! agent needs: ask what an operation would do, read the risk / preconditions /
//! expected ref changes / recovery, and only then decide. Execution of an
//! approved plan is [`execute_plan`]'s own endpoint, `POST /api/execute-plan`
//! (#249) — keeping them apart is what makes "an agent submits a reviewable
//! plan, never argv" a structural fact rather than a convention.
//!
//! # Why the body is a bare [`GitOperation`], not a wrapper DTO
//!
//! [`GitOperation`] is already the closed, internally-tagged (`"op"`) wire
//! vocabulary whose every field is a validating newtype — a malformed branch
//! name, a non-hex oid, a pull with no integration strategy are all
//! deserialize errors at the boundary (a 400), never values a handler could
//! act on. Wrapping it in a one-field request struct would add a wire shape
//! to pin in the golden fixture and buy nothing: there is no second thing to
//! send. The symmetry with #249 is deliberate — this endpoint takes a
//! `GitOperation` and answers a `Plan`; the execute endpoint takes that same
//! `Plan` back.
//!
//! # Why building is refused in Visualize mode
//!
//! A plan is not a read: it carries an [`OperationHash`] that #249's submit
//! stage accepts as *approval* for exactly that mutation. Minting one against
//! a look-only selection is the first half of a write, so it fails the same
//! gate every write fails (ADR 0007), at the earliest honest moment rather
//! than at submit time. See ADR 0046 for the alternative considered.
//!
//! [`OperationHash`]: git_vista_protocol::OperationHash

use std::path::Path;

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{GitOperation, Plan, RepositoryToken, WorktreeToken};

use crate::planner;
use crate::state::reject_if_read_only;

/// `POST /api/plan` — the build stage alone. Answers the reviewable [`Plan`]
/// for `op` against the current selection; the repository is byte-for-byte
/// unchanged afterwards and no mutation guard is taken (see [`plan_only_in`]).
pub(crate) async fn plan_operation(
    Json(op): Json<GitOperation>,
) -> Result<Json<Plan>, (StatusCode, String)> {
    if let Some(rejected) = reject_if_read_only() {
        return Err(rejected);
    }
    // D2 (#66, Task 7): the validated resolution every write handler uses —
    // degraded-mode selections and hostile `.git` geometries refuse here.
    // A plan describes a mutation of *this* repository, so it must resolve
    // the same target the mutation itself would.
    let (repo, _entry) = crate::state::resolve_target()?;
    Ok(Json(
        plan_only_in(&repo, planner::selection_tokens(), op).await,
    ))
}

/// The seam [`plan_operation`] does its work through, with the selection
/// injected rather than read from the process-global state — the same shape
/// `planner::plan_and_execute_in` has, and for the same reason: it is what
/// lets a test drive the real endpoint body against a throwaway repository.
///
/// **This function must never reach `plan_and_execute` or `submit_plan`.**
/// That is not a comment anybody has to remember: the contract suite's
/// `every_git_write_route_reaches_the_planner` reads this body and fails if
/// either name appears in it, and fails equally if `build_plan_only` stops
/// appearing.
///
/// Taking no guard is deliberate and is #248's load-bearing property — see
/// `planner::build_plan_only`'s own doc comment, and the contract suite's
/// `every_plan_tool_operation_builds_while_the_mutation_guard_is_held`, which
/// holds the pipeline's real guard across this call for every operation kind
/// the MCP plan surface can name.
pub(crate) async fn plan_only_in(
    repo: &Path,
    tokens: (RepositoryToken, WorktreeToken),
    op: GitOperation,
) -> Plan {
    planner::build_plan_only(repo, op, tokens).await
}

/// `POST /api/execute-plan` — submit a [`Plan`] built by `POST /api/plan` for
/// execution (M2.23e, #249; ADR 0046 continued). The back half of the
/// #248/#249 pair.
///
/// # Why the body is a bare [`Plan`], not a wrapper DTO
///
/// Symmetric with [`plan_operation`] above, for the same reason: `Plan` is
/// already the closed, validating shape, and there is nothing else this
/// endpoint needs from the request — it is exactly what `/api/plan` hands
/// back.
///
/// # Why this reaches `submit_plan_tracked`, never `plan_and_execute`
///
/// `plan_and_execute` builds its own plan from a bare `GitOperation`; this
/// endpoint already has one, built earlier and possibly reviewed across a
/// roundtrip. Running it through `plan_and_execute` would silently rebuild
/// (and re-approve) it from an operation this route never received.
/// `planner::submit_plan_tracked` is the tracked entry that takes the given
/// plan's own hash/generation as the approval instead, and shares
/// `plan_and_execute_tracked`'s admit/spawn/terminalise with the composed
/// path rather than duplicating it (ADR 0016).
pub(crate) async fn execute_plan(Json(plan): Json<Plan>) -> (StatusCode, String) {
    planner::submit_plan_tracked(plan).await
}
