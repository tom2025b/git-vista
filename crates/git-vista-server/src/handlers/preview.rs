//! `POST /api/preview` — draw the repository as it *would* be after a
//! [`Plan`], writing nothing to it (M10.08, #576; ADR 0099).
//!
//! The read half of the plan-review roundtrip `/api/plan` opens. `/api/plan`
//! answers *what an operation will do*, in words; this answers *what the graph
//! will look like*, as data. Neither executes anything.
//!
//! # Why the body is a bare [`Plan`], not a wrapper DTO
//!
//! Symmetric with [`crate::handlers::plan::execute_plan`], and for the same
//! reason: `Plan` is already the closed, validating shape, and it is exactly
//! what `/api/plan` handed the client back. A wrapper would add a wire shape
//! to pin and buy nothing.
//!
//! # Why this route is a POST, and therefore behind CSRF
//!
//! It changes nothing, and it still takes the full write posture. Two reasons.
//! `security.rs`'s gate keys on the HTTP method, so a POST needs CSRF
//! regardless of what it does. And the body has to be a `Plan` — an
//! internally-tagged enum inside a struct of newtypes — which a query string
//! can only express by flattening it back into loose optional parameters, the
//! un-explicit shape those types exist to remove. Same argument
//! `/api/diff/spec` and `/api/plan` already record.
//!
//! # Why a read-only repository is NOT refused here
//!
//! Every other write-posture handler opens with
//! [`crate::state::reject_if_read_only`] and answers 403. This one
//! deliberately does not, and that is the whole reason
//! [`PreviewUnavailable::RepositoryReadOnly`] exists as a named arm rather
//! than an HTTP status: the *operation* is fine, the *repository* cannot host
//! the computation, and the caller's next move ("reopen in Active mode") is
//! different from the one a 403 on `/api/plan` asks for. Refusing here would
//! also make that arm unreachable in production and exercisable only from a
//! test, which is how a named reason rots into decoration.
//!
//! [`PreviewUnavailable::RepositoryReadOnly`]:
//!     git_vista_protocol::preview::PreviewUnavailable::RepositoryReadOnly

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::Plan;

use crate::preview::{preview, PreviewResponse};

/// `POST /api/preview` — the hypothetical graph for `plan`.
///
/// Resolves the same target a mutation would ([`crate::state::resolve_target`]),
/// so a degraded selection or a hostile `.git` geometry refuses here with the
/// same 409 every write path gives them — those are facts about the *request*,
/// not about the preview, and folding them into `Unavailable` would hide a
/// refused resolution behind a feature-level "could not tell".
pub(crate) async fn preview_plan(
    Json(plan): Json<Plan>,
) -> Result<Json<PreviewResponse>, (StatusCode, String)> {
    let (repo, _entry) = crate::state::resolve_target()?;
    Ok(Json(preview(&repo, &plan).await))
}
