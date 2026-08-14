//! Roving-tabindex focus model for the commit graph's rows (M1.13, #65).
//!
//! The previous #65 lane's own report named this the largest remaining gap: the
//! commit dots (`.node-hit`) are SVG elements with `pointerup` handlers, no
//! `tabindex` and no `role`, so the graph is pointer-only. This module is the
//! "designed focus model" that report said was still needed — the (a) candidate
//! it named, roving tabindex over rows: one tab stop for the whole graph, arrow
//! keys move a single focus position between commits, `Enter`/`Space` activates
//! whichever commit is focused. It is the standard listbox/grid keyboard pattern
//! (see the WAI-ARIA Authoring Practices "grid" and "listbox" patterns), chosen
//! over a parallel linear-list alternative because it needs no second view of
//! the same data — the rows already *are* a linear list, top to bottom, and
//! `row_count` already indexes them 0..row_count exactly the way this model
//! expects. Since #374, `row_count` is the *display-space* row count (a
//! folded WIP run occupies one slot, not one per member) rather than
//! `RenderCtx::loaded.rows.len()` directly — this model doesn't care which
//! space it's counting, only that the index space is contiguous and
//! 0-based, which display space still is.
//!
//! **Scope.** This covers the commit-row hit circles built by
//! `render::nodes::build_node` — the primary interactive content the earlier
//! report singled out. It deliberately does *not* extend to branch-stub rings
//! (`render::stubs`, also `.node-hit`) or ref badges (`.clickable`,
//! `render::labels`): folding either in would mean a second, differently-shaped
//! list (stubs are a handful of eager, non-virtualized elements; badges are zero
//! or more per row, nested inside a row rather than a peer of it), which is a
//! second design decision this task's brief did not ask for and did not want
//! ("no undesigned UI beyond the keyboard model"). Both are named explicitly
//! rather than silently dropped — see the crate's task report for M1.13.
//!
//! **What this proves, precisely.** [`GraphFocus`] is a plain state machine: it
//! knows nothing about the DOM, Leptos, or SVG. Give it a row count and a
//! sequence of moves and it says which row is focused, which row a bare `Tab`
//! would land on, and what happens at the ends of the list (clamped, not
//! wrapped) — all provable on the host, the same shape as
//! `features::shell::core::ModeSettler`.
//!
//! **What this does not, and cannot, prove.** That a real browser actually
//! moves DOM focus onto the right `<circle>`, that `tabindex="-1"` plus a
//! manual `.focus()` call is reliable on SVG elements on iPad Safari (the
//! wiring in `gestures::on_node_keydown` and `render::nodes::build_node`
//! assumes it is, because there is no supported SVG-native alternative — see
//! those modules' docs), or that the focus ring is visible. Nobody working on
//! this repository has a browser in the loop; that gap is real and is recorded
//! in the task report, not hidden by a green test that never touched a device.

/// A keyboard move requested against a [`GraphFocus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    /// `ArrowUp`: one row toward the top (older→newer visually, since row 0 is
    /// the newest commit).
    Prev,
    /// `ArrowDown`: one row toward the bottom.
    Next,
    /// `Home`: the first row.
    First,
    /// `End`: the last row.
    Last,
}

/// Roving-tabindex state over the commit rows: which row currently carries the
/// tab stop, which row (if any) is actually DOM-focused right now, and what
/// arrow keys / `Home` / `End` / `Escape` / activation do to that state.
///
/// The two ideas this type keeps apart on purpose:
///
/// - **`active`** — which row is *the* tabbable one. In the standard
///   roving-tabindex pattern exactly one item in the collection carries
///   `tabindex="0"` at any time (every other item is `tabindex="-1"`, present
///   in the tab order not at all); `active` is that row's index. It survives
///   focus leaving the graph entirely, so tabbing back in resumes where the
///   user left off rather than resetting to the top every time.
/// - **`engaged`** — whether DOM focus is *actually* inside the graph right
///   now. A row can be `active` (the thing Tab would land on) without being
///   focused (nothing in the graph has focus at all yet, or `Escape` moved
///   focus back out) — `focused_row` is `None` in exactly that case, `Some`
///   otherwise.
///
/// Collapsing the two into one `Option<usize>` was tried first and rejected:
/// it cannot represent "Tab should resume at row 12, but nothing is focused
/// right now" without either forgetting row 12 (bad — resets to the top) or
/// treating "not focused" as impossible (wrong — `Escape` and losing focus to
/// something else outside the graph are both real states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphFocus {
    row_count: usize,
    active: usize,
    engaged: bool,
}

impl GraphFocus {
    /// A fresh graph with `row_count` rows, nothing focused yet, and the tab
    /// stop defaulted to row 0 — the newest commit, and the natural place a
    /// `Tab` from outside the graph should land.
    pub fn new(row_count: usize) -> Self {
        Self {
            row_count,
            active: 0,
            engaged: false,
        }
    }

    /// The row count changed — paging appends rows as the user scrolls
    /// (`LoadedHistory::append_page`), so this is called after every accepted
    /// page. Clamps `active` down if it now points past the end; row counts
    /// only grow in this app, so that branch is defensive rather than expected,
    /// the same posture `viewport::visible_row_range` takes toward its own
    /// bounds.
    pub fn set_row_count(&mut self, row_count: usize) {
        self.row_count = row_count;
        if self.active >= row_count {
            self.active = row_count.saturating_sub(1);
        }
        if row_count == 0 {
            self.engaged = false;
        }
    }

    /// The row whose hit target should carry `tabindex="0"` right now — every
    /// other row's is `tabindex="-1"`. `None` when there are no rows at all
    /// (nothing to receive the tab stop).
    pub fn tabbable_row(&self) -> Option<usize> {
        (self.row_count > 0).then_some(self.active)
    }

    /// The row currently holding real keyboard focus, or `None` when focus is
    /// elsewhere — before the first `Tab` into the graph, or after `Escape`.
    pub fn focused_row(&self) -> Option<usize> {
        self.engaged.then_some(self.active)
    }

    /// A `focus` DOM event landed on the tabbable row (a bare `Tab` into the
    /// graph, or a mouse/touch tap that happens to also focus the element). A
    /// no-op on an empty graph, so a stray event after the last row unloads
    /// can't resurrect a focus that has nowhere to live.
    pub fn focus_entered(&mut self) {
        if self.row_count > 0 {
            self.engaged = true;
        }
    }

    /// Focus landed directly on row `row` — a finger/Pencil tap on an element
    /// that is not the current tab stop (M2.16e, #210: every diff hunk header
    /// is tappable, not only the tabbable one). The roving position follows
    /// the tap, so the next arrow key moves from where the user actually is,
    /// not from where keyboard focus last was. Clamped like every other
    /// transition; a no-op on an empty list for [`focus_entered`]'s reason.
    pub fn focus_landed(&mut self, row: usize) {
        if self.row_count == 0 {
            return;
        }
        self.active = row.min(self.row_count - 1);
        self.engaged = true;
    }

    /// Move the roving focus by `dir`. Returns the row to call `.focus()` on
    /// in the DOM, or `None` on an empty graph (nothing to move to). Always
    /// clamps rather than wraps at either end — `ArrowUp` on the first row and
    /// `ArrowDown` on the last both leave `active` exactly where it was, which
    /// is what lets a repeated key press double as "am I at the end?" without
    /// a separate query.
    pub fn mv(&mut self, dir: FocusMove) -> Option<usize> {
        if self.row_count == 0 {
            self.engaged = false;
            return None;
        }
        self.active = match dir {
            FocusMove::Prev => self.active.saturating_sub(1),
            FocusMove::Next => (self.active + 1).min(self.row_count - 1),
            FocusMove::First => 0,
            FocusMove::Last => self.row_count - 1,
        };
        self.engaged = true;
        Some(self.active)
    }

    /// `Escape`: stop reporting a live focus (`focused_row` becomes `None`)
    /// without forgetting which row to resume at (`tabbable_row` is
    /// unchanged). The wiring layer's job is to also move real DOM focus off
    /// the element — see `gestures::on_node_keydown`.
    pub fn escape(&mut self) {
        self.engaged = false;
    }

    /// `Enter` / `Space`: the row to activate, i.e. open its context menu the
    /// same way a tap on its dot would. `None` when nothing is currently
    /// focused — an activation key reaching this model with `engaged == false`
    /// would mean the event fired on an element that isn't actually focused,
    /// which should not happen given the wiring in `render::nodes::build_node`,
    /// but the model does not assume its caller got that right.
    pub fn activate(&self) -> Option<usize> {
        self.focused_row()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below is written as a literal, not re-derived by
    // calling the method under test on itself — the standing rule for this
    // milestone ("never assert a mapping by calling the function that defines
    // it") applies just as much to a state machine's transitions as it does to
    // a lookup table.

    #[test]
    fn a_fresh_graph_defaults_the_tab_stop_to_row_zero_but_focuses_nothing() {
        let f = GraphFocus::new(5);
        assert_eq!(f.tabbable_row(), Some(0));
        assert_eq!(f.focused_row(), None);
        assert_eq!(f.activate(), None);
    }

    #[test]
    fn an_empty_graph_has_no_tabbable_row() {
        let f = GraphFocus::new(0);
        assert_eq!(f.tabbable_row(), None);
        assert_eq!(f.focused_row(), None);
    }

    #[test]
    fn tabbing_in_focuses_the_tabbable_row() {
        let mut f = GraphFocus::new(5);
        f.focus_entered();
        assert_eq!(f.focused_row(), Some(0));
        assert_eq!(f.activate(), Some(0));
    }

    #[test]
    fn focus_entered_on_an_empty_graph_is_a_no_op() {
        let mut f = GraphFocus::new(0);
        f.focus_entered();
        assert_eq!(f.focused_row(), None, "nothing exists to receive focus");
    }

    #[test]
    fn a_tap_moves_the_roving_position_to_the_tapped_row() {
        let mut f = GraphFocus::new(5);
        f.focus_landed(3);
        assert_eq!(f.focused_row(), Some(3));
        assert_eq!(f.tabbable_row(), Some(3), "the tab stop follows the tap");
        // The next arrow moves from the tapped row, not from the old stop.
        assert_eq!(f.mv(FocusMove::Next), Some(4));
    }

    #[test]
    fn a_tap_past_the_end_clamps_and_an_empty_list_ignores_it() {
        let mut f = GraphFocus::new(3);
        f.focus_landed(99);
        assert_eq!(f.focused_row(), Some(2), "clamped to the last row");

        let mut empty = GraphFocus::new(0);
        empty.focus_landed(0);
        assert_eq!(empty.focused_row(), None, "nothing exists to receive focus");
        assert_eq!(empty.tabbable_row(), None);
    }

    #[test]
    fn next_and_prev_move_one_row_and_engage_focus() {
        let mut f = GraphFocus::new(5);
        assert_eq!(f.mv(FocusMove::Next), Some(1));
        assert_eq!(f.focused_row(), Some(1));
        assert_eq!(f.tabbable_row(), Some(1));
        assert_eq!(f.mv(FocusMove::Next), Some(2));
        assert_eq!(f.mv(FocusMove::Prev), Some(1));
    }

    #[test]
    fn arrow_up_at_the_first_row_clamps_rather_than_wraps() {
        let mut f = GraphFocus::new(5);
        f.focus_entered();
        assert_eq!(f.focused_row(), Some(0));
        assert_eq!(f.mv(FocusMove::Prev), Some(0), "already at the top");
        assert_eq!(
            f.mv(FocusMove::Prev),
            Some(0),
            "repeating it changes nothing"
        );
    }

    #[test]
    fn arrow_down_at_the_last_row_clamps_rather_than_wraps() {
        let mut f = GraphFocus::new(3);
        f.mv(FocusMove::Last);
        assert_eq!(f.focused_row(), Some(2));
        assert_eq!(f.mv(FocusMove::Next), Some(2), "already at the bottom");
        assert_eq!(
            f.mv(FocusMove::Next),
            Some(2),
            "repeating it changes nothing"
        );
    }

    #[test]
    fn home_and_end_jump_regardless_of_current_position() {
        let mut f = GraphFocus::new(10);
        f.mv(FocusMove::Next);
        f.mv(FocusMove::Next);
        f.mv(FocusMove::Next); // active = 3
        assert_eq!(f.mv(FocusMove::Last), Some(9));
        assert_eq!(f.mv(FocusMove::First), Some(0));
    }

    #[test]
    fn a_single_row_graph_clamps_every_direction_to_that_row() {
        let mut f = GraphFocus::new(1);
        assert_eq!(f.mv(FocusMove::Next), Some(0));
        assert_eq!(f.mv(FocusMove::Prev), Some(0));
        assert_eq!(f.mv(FocusMove::Last), Some(0));
        assert_eq!(f.mv(FocusMove::First), Some(0));
    }

    #[test]
    fn escape_clears_the_live_focus_but_remembers_where_to_resume() {
        let mut f = GraphFocus::new(5);
        f.mv(FocusMove::Next);
        f.mv(FocusMove::Next); // active = 2, engaged
        f.escape();
        assert_eq!(f.focused_row(), None, "no live focus after Escape");
        assert_eq!(
            f.tabbable_row(),
            Some(2),
            "but Tab back in must resume at the same row, not reset to 0"
        );
        assert_eq!(f.activate(), None, "Enter after Escape activates nothing");

        // Tabbing back in resumes exactly there.
        f.focus_entered();
        assert_eq!(f.focused_row(), Some(2));
    }

    #[test]
    fn activate_reflects_exactly_the_focused_row_not_the_tabbable_one() {
        let mut f = GraphFocus::new(5);
        // active is 0 (default), but nothing is focused yet.
        assert_eq!(f.tabbable_row(), Some(0));
        assert_eq!(f.activate(), None);
        f.focus_entered();
        assert_eq!(f.activate(), Some(0));
    }

    #[test]
    fn moving_beyond_a_row_that_no_longer_exists_never_happens_because_mv_reclamps_first() {
        // set_row_count shrinking (defensive; row counts only grow in this
        // app, but the model does not assume that from outside) pulls `active`
        // back in bounds immediately, before any further move is attempted.
        let mut f = GraphFocus::new(10);
        f.mv(FocusMove::Last); // active = 9
        f.set_row_count(3);
        assert_eq!(
            f.tabbable_row(),
            Some(2),
            "clamped into the new, smaller range"
        );
        assert_eq!(
            f.mv(FocusMove::Next),
            Some(2),
            "still clamped at the new last row"
        );
    }

    #[test]
    fn set_row_count_to_zero_disengages_focus() {
        let mut f = GraphFocus::new(4);
        f.focus_entered();
        assert_eq!(f.focused_row(), Some(0));
        f.set_row_count(0);
        assert_eq!(f.focused_row(), None);
        assert_eq!(f.tabbable_row(), None);
    }

    #[test]
    fn set_row_count_growing_leaves_the_current_row_untouched() {
        let mut f = GraphFocus::new(3);
        f.mv(FocusMove::Next); // active = 1
        f.set_row_count(20);
        assert_eq!(
            f.tabbable_row(),
            Some(1),
            "growth must not move an in-range row"
        );
        assert_eq!(f.focused_row(), Some(1));
    }

    #[test]
    fn mv_on_an_empty_graph_returns_none_and_cannot_engage() {
        let mut f = GraphFocus::new(0);
        assert_eq!(f.mv(FocusMove::Next), None);
        assert_eq!(f.mv(FocusMove::First), None);
        assert_eq!(f.focused_row(), None);
    }
}
