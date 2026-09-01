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

use git_vista_protocol::preview::PreviewUnavailable;
use git_vista_protocol::Plan;

use crate::preview::{preview, PreviewResponse, PreviewTarget};

/// `POST /api/preview` — the hypothetical graph for `plan`.
///
/// Resolves the same target a mutation would ([`crate::state::resolve_target`]),
/// so a degraded selection or a hostile `.git` geometry refuses here with the
/// same 409 every write path gives them — those are facts about the *request*,
/// not about the preview, and folding them into `Unavailable` would hide a
/// refused resolution behind a feature-level "could not tell".
///
/// # The commondir the preview deletes from is decided **here**
///
/// A preview creates a scratch store inside the repository's `<commondir>` and
/// sweeps abandoned ones out of it, and that sweep ends in `remove_dir_all` —
/// a bare syscall in this process with no sandbox in front of it. So the
/// commondir is resolved and containment-checked at this boundary, carried on
/// a [`PreviewTarget`], and never looked up again anywhere below. Audit
/// finding 3 (#576) was exactly the reverse: validated once here, re-resolved
/// later with the containment-free resolver, and a `.git` pointer swapped in
/// between was followed straight into the delete.
///
/// **Known residual, not closed by this handler alone.** The request is still
/// resolved twice — once by [`crate::state::resolve_target`] and once by
/// [`PreviewTarget::in_managed_catalog`] — because `state::resolve_target`
/// drops the resolution it checked instead of returning it. Both resolutions
/// go through the full multi-root containment check and the second one is what
/// is carried, so nothing unvalidated is ever deleted from; but a geometry
/// swapped between the two calls is followed into another *allowed* location.
/// Closing it is one additive change in `state.rs` — a `ValidatedTarget` that
/// carries `repo_paths::RepoPaths` alongside the entry — which `state.rs`'s own
/// `read_only_for_path` doc already names as the right shape for the sibling
/// gap there.
pub(crate) async fn preview_plan(
    Json(plan): Json<Plan>,
) -> Result<Json<PreviewResponse>, (StatusCode, String)> {
    let (repo, _entry) = crate::state::resolve_target()?;
    let target = PreviewTarget::in_managed_catalog(&repo).map_err(|reason| {
        // The same 409 `resolve_target` gives a hostile geometry, and for the
        // same reason: this is a fact about the *request*, not about the
        // preview. `ScratchStore` is the only arm this constructor produces;
        // the fallback exists so a future arm cannot silently become a 200.
        let detail = match reason {
            PreviewUnavailable::ScratchStore { detail } => detail,
            other => format!("{other:?}"),
        };
        (
            StatusCode::CONFLICT,
            format!("This repository's git directory could not be validated: {detail}"),
        )
    })?;
    Ok(Json(preview(&target, &plan).await))
}
