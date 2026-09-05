//! The staging view's own decisions — preview staleness, the button gate, and
//! the direction copy — plus the source census binding
//! `features/diff/staging_view.rs` to them.
//!
//! #653. That view is `#[cfg(target_arch = "wasm32")]`, so none of this ran
//! under `cargo test --workspace` before (ADR 0115). The staleness rule in
//! particular exists only because a reviewer caught it during #215, and no
//! test has been able to reach it in the year since.

use super::*;

use git_vista_protocol::{
    GenerationToken, HunkRef, PatchPlan, RepositoryToken, StageDirection, WorktreeToken,
};

use crate::features::diff::selection::DiffSelection;

/// A plan built the way the view builds it — through `DiffSelection`, the
/// real producer — so a change to that shape is felt here rather than by a
/// hand-assembled struct that agrees with nothing.
fn plan(generation: &str, hunk: u32) -> PatchPlan {
    let mut selection = DiffSelection::new();
    selection.toggle_hunk(
        "src/foo.rs",
        HunkRef {
            index: hunk,
            old_start: 10,
            new_start: 10,
        },
    );
    selection
        .to_patch_plan(
            RepositoryToken::new("repo-1").expect("valid token"),
            WorktreeToken::new("wt-1").expect("valid token"),
            GenerationToken::new(generation).expect("valid token"),
            StageDirection::Stage,
        )
        .expect("a non-empty selection builds a plan")
}

// ---- preview staleness -----------------------------------------------------

#[test]
fn nothing_previewed_is_neither_fresh_nor_stale() {
    assert_eq!(
        preview_state(None, Some(&plan("g1", 0))),
        PreviewState::NotRequested,
        "a view nobody has previewed must not be told its selection changed"
    );
    assert_eq!(preview_state(None, None), PreviewState::NotRequested);
}

#[test]
fn a_preview_answering_the_current_selection_is_fresh() {
    assert_eq!(
        preview_state(Some(&plan("g1", 0)), Some(&plan("g1", 0))),
        PreviewState::Fresh
    );
}

#[test]
fn toggling_a_hunk_after_preview_makes_the_shown_patch_stale() {
    // The #215 review finding, made checkable. Apply itself was always
    // correct — it rebuilds the plan from the live selection at click time —
    // but the panel kept showing the OLD patch text with nothing to say Apply
    // would no longer match it.
    assert_eq!(
        preview_state(Some(&plan("g1", 0)), Some(&plan("g1", 1))),
        PreviewState::Stale,
        "the panel is a promise about what Apply is going to do; a selection \
         that has moved since the preview was requested makes that promise \
         false"
    );
}

#[test]
fn a_preview_taken_against_an_older_generation_is_stale() {
    // The worktree moved under the selection. Same patch text, different
    // generation token — and the server would refuse the old plan anyway.
    assert_eq!(
        preview_state(Some(&plan("g1", 0)), Some(&plan("g2", 0))),
        PreviewState::Stale
    );
}

#[test]
fn a_preview_left_over_when_no_plan_can_be_built_is_stale_not_fresh() {
    // `current` is `None` for an empty selection, and for a repository the
    // server has assigned no identity to. The shown patch is not what an
    // unbuildable plan would send either — treating that as fresh is the same
    // lie in a quieter form.
    assert_eq!(
        preview_state(Some(&plan("g1", 0)), None),
        PreviewState::Stale
    );
}

// ---- the button gate -------------------------------------------------------

#[test]
fn the_server_reaching_buttons_need_a_selection_an_idle_view_and_an_identity() {
    let ready = staging_actions(false, false, true);
    assert!(ready.preview && ready.apply && ready.clear);

    for (empty, busy, identity, why) in [
        (true, false, true, "an empty selection has nothing to send"),
        (false, true, true, "a request is already in flight"),
        (
            false,
            false,
            false,
            "the server has assigned this repository no identity, so no plan \
             can name it",
        ),
    ] {
        let a = staging_actions(empty, busy, identity);
        assert!(!a.preview, "Preview must be disabled: {why}");
        assert!(!a.apply, "Apply must be disabled: {why}");
    }
}

#[test]
fn clear_stays_live_while_a_request_is_in_flight() {
    let a = staging_actions(false, true, true);
    assert!(
        a.clear && !a.apply,
        "clearing is local — a request in flight is no reason to trap the \
         user with a selection they have decided against"
    );
}

#[test]
fn clear_stays_live_in_a_repository_with_no_identity() {
    let a = staging_actions(false, false, false);
    assert!(
        a.clear && !a.preview,
        "no identity means nothing can be sent, which is exactly the state \
         where clearing the selection is the only useful thing left to do"
    );
}

#[test]
fn clear_is_the_only_one_that_ever_differs_from_the_other_two() {
    // The tempting simplification is one "can act" boolean driving all three.
    // This says why it is wrong: there are states where Clear and the other
    // two disagree, and Preview and Apply never do.
    let mut disagreements = 0;
    for empty in [false, true] {
        for busy in [false, true] {
            for identity in [false, true] {
                let a = staging_actions(empty, busy, identity);
                assert_eq!(
                    a.preview, a.apply,
                    "Preview and Apply both reach the server and must gate \
                     alike ({empty}, {busy}, {identity})"
                );
                if a.clear != a.preview {
                    disagreements += 1;
                }
            }
        }
    }
    assert!(
        disagreements > 0,
        "Clear never differed from Preview across every input combination, \
         which means it is being derived from the same condition and the \
         asymmetry has been flattened away"
    );
}

#[test]
fn an_empty_selection_disables_everything_including_clear() {
    let a = staging_actions(true, false, true);
    assert!(!a.preview && !a.apply && !a.clear);
}

// ---- direction copy --------------------------------------------------------

#[test]
fn the_two_directions_name_opposite_flows() {
    let (stage_word, stage_flow) = stage_direction_copy(StageDirection::Stage);
    let (unstage_word, unstage_flow) = stage_direction_copy(StageDirection::Unstage);
    assert_eq!((stage_word, stage_flow), ("Stage", "worktree → index"));
    assert_eq!((unstage_word, unstage_flow), ("Unstage", "index → HEAD"));
    assert_ne!(
        stage_flow, unstage_flow,
        "the arrows are the only thing on the panel saying which of the two \
         diffs the selection's coordinates address; the same text under both \
         directions tells the reader nothing"
    );
    assert!(
        stage_flow.contains("worktree") && !unstage_flow.contains("worktree"),
        "swapped: only staging reads out of the worktree. Told the other way \
         round, a reader unstaging believes their worktree edits are at risk"
    );
}

// ---- the seam --------------------------------------------------------------

const STAGING_VIEW: &str = include_str!("../staging_view.rs");

#[test]
fn the_staging_view_asks_core_whether_its_preview_may_be_shown() {
    assert!(
        STAGING_VIEW.contains("preview_state("),
        "features/diff/staging_view.rs no longer calls `preview_state`. \
         Staleness is a decision, and this file is wasm-only — re-derived \
         here it is unreachable from every test above, which is the state it \
         was in from #215 until #653"
    );
    assert!(
        STAGING_VIEW.contains("PreviewState::Stale")
            && STAGING_VIEW.contains("PreviewState::NotRequested"),
        "the view no longer distinguishes `Stale` from `NotRequested`. \
         Collapsing them puts \"Selection changed since this preview\" on a \
         panel nobody has previewed yet"
    );
}

#[test]
fn the_staging_view_asks_core_which_buttons_are_live() {
    assert!(
        STAGING_VIEW.contains("staging_actions("),
        "features/diff/staging_view.rs no longer calls `staging_actions`"
    );
    assert!(
        !STAGING_VIEW.contains("|| busy.get() ||"),
        "the view rebuilds the button gate inline again. That expression was \
         written out twice for Preview and Apply while Clear deliberately \
         omitted it; kept here, the omission reads as an oversight and gets \
         'fixed' into a bug that traps the user with a selection they cannot \
         clear"
    );
}

#[test]
fn the_staging_view_asks_core_for_its_direction_copy() {
    assert!(
        STAGING_VIEW.contains("stage_direction_copy("),
        "features/diff/staging_view.rs no longer calls `stage_direction_copy`"
    );
    assert!(
        !STAGING_VIEW.contains("index → HEAD"),
        "the view spells a flow arrow again rather than asking for it — the \
         one string on the panel that says which diff is being addressed"
    );
}
