//! The staging (patch-plan) surface — M2.17, #70.
//!
//! #212 seeds this module with the staleness gate; #213 adds the
//! preview/apply endpoints that build the selected patch through the
//! planner's single funnel, and #214 the sub-hunk apply semantics. All
//! three are sequenced strictly — this is the "same staging handler/planner
//! region" the issues warn against parallelizing.
//!
//! A [`PatchPlan`] is a selection made against one exact diff, so it is
//! admitted only while the worktree's live generation still equals the one
//! the plan carries — the same equality-only posture as `Plan` execution
//! (`planner::enforce_fresh`) and history paging
//! (`history::require_same_generation`), free-riding the same
//! `ErrorCode::Conflict` envelope (409 on the wire). Structural problems in
//! the plan itself are the 400 class ([`PatchPlan::validate`], checked in
//! the protocol crate) — a selection can be malformed or stale, and the two
//! must never be conflated: retrying a malformed plan is pointless, while
//! retrying a stale one after a refresh is exactly right.

use axum::http::StatusCode;
use git_vista_protocol::{GenerationToken, PatchPlan};

/// Refuse a selection whose diff no longer exists — the worktree moved
/// between the user viewing the diff and submitting the selection.
///
/// **Provenance contract, the half equality alone cannot pin**: `live` must
/// be minted by the *same recipe and namespace* that tagged the diff read
/// the selection was made from. Three incompatible producers already exist —
/// `planner::generation_token` (bare decimal digest),
/// `history.rs` (`history-v1:` prefixed), and `/api/status/v2`
/// (`status-v1:` prefixed, folding in the porcelain bytes) — and comparing a
/// token from one against a token from another 409s forever, never admits.
/// #213's diff read therefore serves its own namespaced token (`diff-v1:`
/// per the `status.rs` precedent), and this gate recomputes with exactly
/// that recipe. See `status.rs`'s module doc for the namespacing rationale.
///
/// Both of #213's handlers (`handlers::staging::staging_preview` /
/// `staging_apply`) call this before building any patch — seeded by #212 so
/// the staleness contract landed with the DTO, consumed by #213 as planned.
pub(crate) fn require_current_selection(
    plan: &PatchPlan,
    live: &GenerationToken,
) -> Result<(), (StatusCode, String)> {
    if plan.generation == *live {
        Ok(())
    } else {
        Err((
            StatusCode::CONFLICT,
            "The repository changed while this selection was pending — refresh the diff and \
             reselect."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{RepositoryToken, StageDirection, WorktreeToken};

    fn generation(s: &str) -> GenerationToken {
        GenerationToken::new(s).unwrap()
    }

    fn plan_at(generation_token: &str) -> PatchPlan {
        PatchPlan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: generation(generation_token),
            direction: StageDirection::Stage,
            files: vec![],
        }
    }

    // The pure-token pattern `require_same_generation`'s test uses: no repo,
    // two hand-built tokens, both legs — the same-generation leg proves the
    // gate can admit at all (a gate that always refuses would also pass a
    // refusal-only test).
    #[test]
    fn a_moved_generation_is_a_conflict_and_a_matching_one_admits() {
        let live = generation("41");
        assert!(require_current_selection(&plan_at("41"), &live).is_ok());

        let (status, why) =
            require_current_selection(&plan_at("40"), &live).expect_err("stale must refuse");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            why.contains("changed while this selection was pending"),
            "{why}"
        );
    }

    // Equality only — ADR 0001 forbids ordering. A "newer-looking" live
    // token refuses exactly like an older one; the gate must not parse.
    #[test]
    fn tokens_are_compared_only_for_equality_never_ordered() {
        let (status, _) = require_current_selection(&plan_at("42"), &generation("41"))
            .expect_err("plan ahead of live must refuse too");
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
