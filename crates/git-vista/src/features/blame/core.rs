//! Pure decision logic for the blame panel (M5.33, #86): touch/keyboard range
//! selection, and the user-facing messages for a path's non-`Readable`
//! states. Framework-free and host-tested, per the `features/*/core.rs`
//! convention (ADR 0115) — `view.rs` next door is the thin, wasm-only DOM
//! wiring that reads this and nothing else.
//!
//! **Range selection reuses house machinery rather than reinventing it.**
//! [`crate::features::diff::selection::drag_range`] already computes an
//! order-independent inclusive range from a drag's start/current position —
//! exactly what a finger sweeping across blame rows needs — so
//! [`BlameSelection`] is a thin wrapper holding the drag anchor, not a new
//! range algorithm. Keyboard navigation between rows reuses
//! [`crate::features::a11y::focus::GraphFocus`] directly in `view.rs`
//! (row-count-based roving tabindex has no notion of "which feature's rows"
//! and needs no blame-specific wrapper at all).

use std::ops::RangeInclusive;

use git_vista_protocol::blame::{PathState, RenameLimitNotice};

use crate::features::diff::selection::drag_range;

/// A contiguous range of blame rows currently selected — the shape a finger
/// drag, a keyboard shift-select, or a single tap (a one-row range) all
/// produce, so a "Compare from here"/"Compare with…" action always has
/// exactly one thing to reason about rather than a scattered set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlameSelection {
    /// The row index a drag/shift-select started at. `None` when nothing has
    /// been anchored yet — the panel's initial state.
    anchor: Option<usize>,
    /// The most recent row the gesture reached. Equal to `anchor` for a
    /// plain tap (a one-row selection).
    current: Option<usize>,
}

impl BlameSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// A tap, or the start of a drag: anchor and current both become `row`.
    pub fn start(&mut self, row: usize) {
        self.anchor = Some(row);
        self.current = Some(row);
    }

    /// The gesture (a drag, or a keyboard shift-move) has reached `row`.
    /// A no-op if nothing was ever [`Self::start`]-ed — extending a selection
    /// that was never anchored has nothing to extend.
    pub fn extend_to(&mut self, row: usize) {
        if self.anchor.is_some() {
            self.current = Some(row);
        }
    }

    /// Drop the selection entirely (Escape, or tapping empty space).
    pub fn clear(&mut self) {
        self.anchor = None;
        self.current = None;
    }

    /// Nothing is selected — the compare/open actions should be disabled.
    pub fn is_empty(&self) -> bool {
        self.anchor.is_none()
    }

    /// The current selection as an order-independent inclusive range, `None`
    /// when nothing is selected.
    pub fn range(&self) -> Option<RangeInclusive<usize>> {
        match (self.anchor, self.current) {
            (Some(a), Some(c)) => Some(drag_range(a, c)),
            _ => None,
        }
    }

    /// Whether `row` falls inside the current selection — what a rendered
    /// row reads to decide its own highlighted/`aria-selected` state.
    pub fn contains(&self, row: usize) -> bool {
        self.range().is_some_and(|r| r.contains(&row))
    }
}

/// The user-facing explanation for a path that is not [`PathState::Readable`]
/// — `None` for `Readable` itself, since that state needs no banner at all.
/// Every other variant gets its own sentence, stated as a fact about the
/// repository rather than a generic "not found": see [`PathState`]'s own doc
/// for why "absent" is deliberately not one message.
pub fn path_state_message(state: &PathState) -> Option<String> {
    match state {
        PathState::Readable => None,
        PathState::Binary => Some(
            "This file is binary. Line-by-line blame has no meaning for it.".to_string(),
        ),
        PathState::NeverExisted => {
            Some("No commit in this history ever touched this path.".to_string())
        }
        PathState::Deleted { last_commit } => Some(format!(
            "This path was deleted in {}. Showing its history up to that point.",
            short(last_commit)
        )),
        PathState::RenamedAway {
            last_commit,
            current_path,
        } => Some(format!(
            "This path was renamed to '{current_path}' in {}. Showing its history up to that point.",
            short(last_commit)
        )),
    }
}

/// The banner for one or more rename-limit hits, `None` when the walk hit
/// none. Plural-aware, and states git's own suggested minimum when it gave
/// one — the whole point of [`RenameLimitNotice`] existing is to say
/// *something concrete*, not just "may be incomplete".
pub fn rename_limit_banner(hits: &[RenameLimitNotice]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let commits: Vec<String> = hits.iter().map(|h| short(&h.commit).to_string()).collect();
    let suggestion = hits
        .iter()
        .find_map(|h| h.suggested_minimum)
        .map(|n| format!(" (git suggests raising diff.renameLimit to at least {n})"))
        .unwrap_or_default();
    if hits.len() == 1 {
        Some(format!(
            "Rename detection was skipped at {} because too many files changed in that commit — \
             this history may be missing a rename before that point{suggestion}.",
            commits[0]
        ))
    } else {
        Some(format!(
            "Rename detection was skipped at {} commits ({}) because too many files changed — \
             this history may be missing renames before those points{suggestion}.",
            commits.len(),
            commits.join(", ")
        ))
    }
}

/// The conventional 7-char short id, matching
/// `crate::menu::compare_items::short` and the server's own truncation.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn a_fresh_selection_is_empty() {
        let sel = BlameSelection::new();
        assert!(sel.is_empty());
        assert_eq!(sel.range(), None);
        assert!(!sel.contains(0));
    }

    #[test]
    fn a_tap_selects_exactly_one_row() {
        let mut sel = BlameSelection::new();
        sel.start(5);
        assert!(!sel.is_empty());
        assert_eq!(sel.range(), Some(5..=5));
        assert!(sel.contains(5));
        assert!(!sel.contains(4));
        assert!(!sel.contains(6));
    }

    #[test]
    fn extending_downward_widens_the_range() {
        let mut sel = BlameSelection::new();
        sel.start(3);
        sel.extend_to(7);
        assert_eq!(sel.range(), Some(3..=7));
    }

    #[test]
    fn extending_upward_past_the_anchor_still_produces_an_ascending_range() {
        // A finger dragging UP from where it started must not invert the
        // range or drop rows — `drag_range` already guarantees this; this
        // test pins that the wrapper actually delegates to it.
        let mut sel = BlameSelection::new();
        sel.start(10);
        sel.extend_to(4);
        assert_eq!(sel.range(), Some(4..=10));
        assert!(sel.contains(4));
        assert!(sel.contains(10));
    }

    #[test]
    fn extend_without_a_prior_start_does_nothing() {
        let mut sel = BlameSelection::new();
        sel.extend_to(5);
        assert!(sel.is_empty(), "nothing to extend without an anchor");
    }

    #[test]
    fn clear_drops_the_whole_selection() {
        let mut sel = BlameSelection::new();
        sel.start(2);
        sel.extend_to(6);
        sel.clear();
        assert!(sel.is_empty());
        assert_eq!(sel.range(), None);
    }

    #[test]
    fn starting_again_replaces_the_previous_selection_rather_than_extending_it() {
        let mut sel = BlameSelection::new();
        sel.start(2);
        sel.extend_to(6);
        sel.start(20);
        assert_eq!(
            sel.range(),
            Some(20..=20),
            "a fresh tap must not inherit the old anchor"
        );
    }
}

#[cfg(test)]
mod message_tests {
    use super::*;

    #[test]
    fn a_readable_path_has_no_banner() {
        assert_eq!(path_state_message(&PathState::Readable), None);
    }

    #[test]
    fn binary_has_its_own_distinct_message() {
        let msg = path_state_message(&PathState::Binary).unwrap();
        assert!(msg.to_lowercase().contains("binary"));
    }

    #[test]
    fn never_existed_says_so_plainly() {
        let msg = path_state_message(&PathState::NeverExisted).unwrap();
        assert!(msg.contains("never") || msg.contains("No commit"));
    }

    #[test]
    fn deleted_names_the_commit_short_form() {
        let msg = path_state_message(&PathState::Deleted {
            last_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        })
        .unwrap();
        assert!(msg.contains("0123456"), "must use the 7-char short form");
        assert!(
            !msg.contains("0123456789abcdef"),
            "must not leak the full 40-char id into the sentence"
        );
    }

    #[test]
    fn renamed_away_names_both_the_commit_and_the_new_path() {
        let msg = path_state_message(&PathState::RenamedAway {
            last_commit: "abcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
            current_path: "new/name.rs".to_string(),
        })
        .unwrap();
        assert!(msg.contains("abcdefa"));
        assert!(msg.contains("new/name.rs"));
    }

    #[test]
    fn three_distinct_absent_states_produce_three_distinct_messages() {
        // The whole point of PathState (see its doc): a client must not have
        // to infer which absence it is from a shared, generic sentence.
        let never = path_state_message(&PathState::NeverExisted).unwrap();
        let deleted = path_state_message(&PathState::Deleted {
            last_commit: "1111111111111111111111111111111111111111".to_string(),
        })
        .unwrap();
        let renamed = path_state_message(&PathState::RenamedAway {
            last_commit: "1111111111111111111111111111111111111111".to_string(),
            current_path: "x.rs".to_string(),
        })
        .unwrap();
        assert_ne!(never, deleted);
        assert_ne!(deleted, renamed);
        assert_ne!(never, renamed);
    }

    #[test]
    fn no_hits_means_no_banner() {
        assert_eq!(rename_limit_banner(&[]), None);
    }

    #[test]
    fn one_hit_states_the_commit_and_gits_own_suggestion() {
        let banner = rename_limit_banner(&[RenameLimitNotice {
            commit: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            suggested_minimum: Some(31),
        }])
        .unwrap();
        assert!(banner.contains("deadbee"));
        assert!(banner.contains("31"));
    }

    #[test]
    fn a_hit_with_no_suggested_minimum_still_produces_a_banner() {
        let banner = rename_limit_banner(&[RenameLimitNotice {
            commit: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            suggested_minimum: None,
        }])
        .unwrap();
        assert!(banner.contains("deadbee"));
    }

    #[test]
    fn multiple_hits_are_pluralized_and_name_every_commit() {
        let banner = rename_limit_banner(&[
            RenameLimitNotice {
                commit: "1111111111111111111111111111111111111111".to_string(),
                suggested_minimum: None,
            },
            RenameLimitNotice {
                commit: "2222222222222222222222222222222222222222".to_string(),
                suggested_minimum: None,
            },
        ])
        .unwrap();
        assert!(banner.contains("1111111"));
        assert!(banner.contains("2222222"));
        assert!(banner.contains("2 commits") || banner.contains("commits"));
    }
}
