//! The staging HTTP surface (M2.17b, #213): the base-diff read a selection
//! is made against, the preview of the exact bytes an apply would execute,
//! and the apply itself.
//!
//! The division of labour is strict, and each piece already exists
//! elsewhere: `handlers::read::staging_diff_for_repo` owns the diff argv and
//! the `diff-v1:` token, `crate::staging::require_current_selection` owns
//! staleness (409), `git_vista_protocol`'s `validate`/`build_selected_patch`
//! own the 400 class, and `planner::plan_and_execute` owns execution — this
//! module only sequences them. Preview and apply run the *same* pipeline up
//! to the built selection; apply then hands the built bytes (not the wire
//! plan) to the planner as `GitOperation::StageSelection`, whose operation
//! hash binds them, so what was previewed is provably what applies while the
//! generation holds.
//!
//! All three routes register under `full_routes` (ADR 0005): the diff read
//! is technically a read, but it exists only to feed selections into the
//! write surface, and a LAN visualize session has no business seeing
//! uncommitted worktree contents the graph itself never shows.

use axum::extract::Query;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use git_vista_protocol::{
    build_selected_patch, parse_unified_diff, GitOperation, PatchPlan, PatchPreview, SelectedPatch,
    StageDirection, StagingDiff,
};

use crate::handlers::read::staging_diff_for_repo;
use crate::staging::require_current_selection;

/// Query of `GET /api/staging/diff`: which base diff (and thereby which
/// direction's selection) the client is about to build.
#[derive(Deserialize)]
pub(crate) struct StagingDiffQuery {
    direction: StageDirection,
}

/// `GET /api/staging/diff?direction=stage|unstage` — the pinned base diff
/// plus its `diff-v1:` generation token, which the client copies verbatim
/// into the [`PatchPlan`] it sends back.
pub(crate) async fn staging_diff(
    Query(query): Query<StagingDiffQuery>,
) -> Result<Json<StagingDiff>, (StatusCode, String)> {
    let (repo, _entry) = crate::state::resolve_target()?;
    staging_diff_for_repo(&repo, query.direction)
        .await
        .map(Json)
}

/// The shared front half of preview and apply: structural validation (400),
/// target match (400), staleness gate against a fresh base-diff read (409),
/// then the pure build (400 on any cross-check mismatch). Returns the built
/// selection together with the live diff it was verified against.
async fn checked_build(
    plan: &PatchPlan,
) -> Result<(SelectedPatch, StagingDiff), (StatusCode, String)> {
    if let Err(e) = plan.validate() {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }
    let (repo, entry) = crate::state::resolve_target()?;
    // The plan names the selection it was built in; a mismatch is not
    // staleness (409) but a plan addressed to somewhere else entirely —
    // the client switched repositories with a selection in hand.
    let handle = entry.handle;
    if plan.repository.as_str() != handle.repository.to_string()
        || plan.worktree.as_str() != handle.worktree.to_string()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "This selection belongs to a different repository or worktree \
             than the current one."
                .to_string(),
        ));
    }
    let live = staging_diff_for_repo(&repo, plan.direction).await?;
    require_current_selection(plan, &live.generation)?;
    let built = build_selected_patch(&parse_unified_diff(&live.patch), plan)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok((built, live))
}

/// `POST /api/staging/preview` — the exact bytes an apply of this plan would
/// execute, without executing anything.
pub(crate) async fn staging_preview(
    Json(plan): Json<PatchPlan>,
) -> Result<Json<PatchPreview>, (StatusCode, String)> {
    let (built, live) = checked_build(&plan).await?;
    Ok(Json(PatchPreview {
        generation: live.generation,
        patch: built.patch,
        whole_files: built.whole_files,
    }))
}

/// `POST /api/staging/apply` — build under the gate, then execute through
/// the planner's single funnel (idempotency, coordinator serialization,
/// `enforce_fresh`, the sealed argv path — all inherited, none reimplemented
/// here).
pub(crate) async fn staging_apply(Json(plan): Json<PatchPlan>) -> (StatusCode, String) {
    let (built, live) = match checked_build(&plan).await {
        Ok(v) => v,
        Err(rejected) => return rejected,
    };
    crate::planner::plan_and_execute(GitOperation::StageSelection {
        direction: plan.direction,
        // The gate-time token: the executor re-mints and re-compares it
        // inside the coordinator lock, closing the gate→execute window.
        expected_diff_generation: live.generation,
        patch: built.patch,
        whole_files: built.whole_files,
    })
    .await
}
