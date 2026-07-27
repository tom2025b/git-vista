//! Which overlay is on top, and what Esc closes (M1.11, #64).
//!
//! Before M1.11 there was no single owner of "what's currently covering the canvas."
//! Six independent signals played that role — `Overlays::menu`, `commit_dialog`,
//! `confirm_op`, `detail_id`, `viewer` and `Activity`'s own open flag — each opened and
//! closed by whichever call site happened to touch it. Two bugs fell straight out of
//! that shape, and this module exists to make both unrepresentable rather than to patch
//! either:
//!
//! 1. `gestures.rs`'s Esc handler destructures `menu, commit_dialog, confirm_op,
//!    detail_id, viewer` — not Activity — so Esc cannot close the Activity panel. It
//!    was never a deliberate exclusion; Activity was simply the field nobody
//!    remembered to add to that list.
//! 2. The Activity panel and the commit detail panel both dock the right edge of the
//!    screen, so only one can be showing at a time. `state.rs`'s
//!    `Overlays::open_detail_panel` closes Activity **synchronously** when the detail
//!    panel opens. The reverse direction — opening Activity closing the detail panel —
//!    is only driven by a `create_effect` in `activity.rs` that fires one reactive tick
//!    *after* Activity's visibility flips. So for one frame, both panels can render at
//!    once, and the asymmetry between "synchronous" and "next tick" is exactly the
//!    kind of thing that stops being true the next time either call site is touched.
//!
//! [`OverlayStack`] fixes both by construction: every overlay opens through the same
//! [`OverlayStack::present`], which already knows which [`Dock`] it wants and evicts
//! whatever else is there — synchronously, in both directions, because there is only
//! one direction. And because Activity is a variant of [`Overlay`] like everything
//! else, there is no separate list for an Esc handler to leave it out of.

/// Where an overlay physically sits on screen, and therefore which other overlays it
/// can share the screen with.
///
/// Two overlays that resolve to the same dock are mutually exclusive by construction —
/// see [`OverlayStack::present`]. Two overlays with different docks may be presented
/// together; that is exactly how the context menu (`Anchored`) coexists with a
/// right-docked panel today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dock {
    /// The right-hand panel gutter. The Activity panel and the commit detail panel
    /// both live here — stacking them would just hide one behind the other, so at
    /// most one may be presented.
    RightEdge,
    /// A full-screen backdrop modal. The commit-message dialog and the branch-op /
    /// undo confirmation are both this: opening one is a hard interruption that the
    /// other cannot survive alongside.
    Modal,
    /// Floats at a pointer coordinate over the canvas, rather than docking to an
    /// edge. Only the context menu is this — which is exactly why it can legitimately
    /// coexist with whatever is docked at [`Dock::RightEdge`]: it isn't competing for
    /// the same screen real estate.
    Anchored,
    /// Sits on top of the panel it was opened from, covering the whole screen. Only
    /// the full-diff / full-file viewer is this, and it is why `Viewer` must never
    /// evict the panel underneath it — evicting it would strand the viewer with
    /// nothing to return to when it closes.
    FullScreen,
}

/// Something that can be presented over the canvas.
///
/// One variant per signal `Overlays` used to hold separately, named the same as the
/// field it replaces so the mapping back to the pre-M1.11 code is legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The context menu (`Overlays::menu`), opened at a pointer coordinate.
    Menu,
    /// The commit-message dialog (`Overlays::commit_dialog`).
    CommitDialog,
    /// The branch-operation / undo confirmation (`Overlays::confirm_op`).
    Confirm,
    /// The commit detail panel (`Overlays::detail_id`).
    Detail,
    /// The full-screen diff / file viewer (`Overlays::viewer`), opened from the
    /// detail panel.
    Viewer,
    /// The Activity panel (`Overlays::activity`) — the overlay Esc could not
    /// previously reach at all.
    Activity,
}

impl Overlay {
    /// Where this overlay docks, and therefore what it evicts when presented.
    ///
    /// This mapping is the whole point of the type: it is the one place the
    /// right-edge exclusivity, the two-modal exclusivity, the menu's independence,
    /// and the viewer's non-eviction are all stated, instead of being four separate
    /// facts implied by four separate call sites.
    ///
    /// - [`Overlay::Detail`] and [`Overlay::Activity`] → [`Dock::RightEdge`]: both are
    ///   right-docked panels, and stacking them just hides one.
    /// - [`Overlay::CommitDialog`] and [`Overlay::Confirm`] → [`Dock::Modal`]: both are
    ///   full-screen-backdrop modals.
    /// - [`Overlay::Menu`] → [`Dock::Anchored`]: the context menu floats at a pointer
    ///   coordinate over the canvas and legitimately coexists with a right-edge panel.
    /// - [`Overlay::Viewer`] → [`Dock::FullScreen`]: it sits over the panel it was
    ///   opened from, which is why it must not evict that panel.
    pub fn dock(self) -> Dock {
        match self {
            Overlay::Detail | Overlay::Activity => Dock::RightEdge,
            Overlay::CommitDialog | Overlay::Confirm => Dock::Modal,
            Overlay::Menu => Dock::Anchored,
            Overlay::Viewer => Dock::FullScreen,
        }
    }
}

/// The overlays currently presented, bottom-to-top.
///
/// **Invariant: at most one overlay per [`Dock`], after any sequence of operations.**
/// [`Self::present`] is the only way to add an overlay, and it enforces this by
/// evicting whatever already occupies the incoming overlay's dock before pushing —
/// see this module's `at_most_one_overlay_per_dock_after_any_sequence` test for a
/// mixed-sequence proof.
///
/// Insertion order is dismissal order (LIFO): [`Self::dismiss_top`] pops the last
/// element. That reproduces the priority `gestures.rs`'s Esc handler used to spell out
/// by hand — viewer first, then the menu, then the modals, then the detail panel —
/// not by copying that priority as a constant, but because it is exactly the order the
/// real open sequences produce: the viewer only ever opens from an already-open detail
/// panel, a modal only ever opens over whatever else is up, and so on. The one case
/// this *changes* rather than reproduces: Activity is now reachable by Esc at all,
/// which it never was before (see the module doc's bug #1).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OverlayStack {
    stack: Vec<Overlay>,
}

impl OverlayStack {
    /// Present `o` on top.
    ///
    /// Three outcomes, in priority order:
    /// 1. `o` is already somewhere in the stack: move it to the top (raise it) and
    ///    return `None`. This is what keeps re-presenting an overlay from ever
    ///    duplicating it.
    /// 2. Some *other* overlay already occupies `o.dock()`: remove that overlay, push
    ///    `o` on top, and return `Some(evicted)`. The caller owns clearing the evicted
    ///    overlay's payload signal — this core only tracks which overlay is up, not
    ///    what it's showing.
    /// 3. Neither: push `o` on top and return `None`.
    pub fn present(&mut self, o: Overlay) -> Option<Overlay> {
        if let Some(pos) = self.stack.iter().position(|&existing| existing == o) {
            self.stack.remove(pos);
            self.stack.push(o);
            return None;
        }

        let dock = o.dock();
        let evicted = self
            .stack
            .iter()
            .position(|&existing| existing.dock() == dock)
            .map(|pos| self.stack.remove(pos));

        self.stack.push(o);
        evicted
    }

    /// Dismiss the topmost overlay — what Esc does, unconditionally, against
    /// whatever is actually on top. Returns `None` on an empty stack rather than
    /// panicking: a stray Esc with nothing open is not an error.
    pub fn dismiss_top(&mut self) -> Option<Overlay> {
        self.stack.pop()
    }

    /// Dismiss `o` from wherever it sits in the stack, not only from the top —
    /// e.g. a non-Esc close button on a buried overlay. Returns whether it was
    /// there to remove.
    pub fn dismiss(&mut self, o: Overlay) -> bool {
        match self.stack.iter().position(|&existing| existing == o) {
            Some(pos) => {
                self.stack.remove(pos);
                true
            }
            None => false,
        }
    }

    /// The overlay currently on top, if any.
    pub fn top(&self) -> Option<Overlay> {
        self.stack.last().copied()
    }

    /// Whether `o` is presented anywhere in the stack, top or buried.
    pub fn contains(&self, o: Overlay) -> bool {
        self.stack.contains(&o)
    }

    /// Whether nothing is presented at all.
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_dismisses_the_activity_panel() {
        // The real bug this core exists to make unrepresentable: gestures.rs's Esc
        // handler destructures `menu, commit_dialog, confirm_op, detail_id, viewer`
        // but not Activity, so Esc cannot close the Activity panel. Owning the whole
        // stack in one place means there is no separate per-overlay list left for a
        // future overlay to be left out of.
        let mut s = OverlayStack::default();
        s.present(Overlay::Activity);
        assert_eq!(s.dismiss_top(), Some(Overlay::Activity));
        assert_eq!(s.top(), None);
    }

    #[test]
    fn escape_dismisses_the_topmost_overlay_first() {
        // The plan wrote this test with Activity then Detail, but under the eviction
        // rule those two can never both be on the stack — they share Dock::RightEdge,
        // so presenting the second evicts the first rather than stacking on top of
        // it. Detail then Viewer is used instead: a pair that genuinely stacks,
        // because the viewer only ever opens *from* an already-open detail panel.
        // The intent the original test was checking — LIFO dismissal — is unchanged.
        let mut s = OverlayStack::default();
        s.present(Overlay::Detail);
        s.present(Overlay::Viewer);
        assert_eq!(s.dismiss_top(), Some(Overlay::Viewer));
        assert_eq!(
            s.top(),
            Some(Overlay::Detail),
            "the one underneath survives"
        );
    }

    #[test]
    fn presenting_an_already_present_overlay_raises_it_rather_than_duplicating() {
        // Detail and Menu dock differently (RightEdge vs. Anchored), so nothing here
        // is evicted — this is purely about re-presenting Detail while it's already
        // up, with something else already on top of it.
        let mut s = OverlayStack::default();
        s.present(Overlay::Detail);
        s.present(Overlay::Menu);
        s.present(Overlay::Detail);
        assert_eq!(s.dismiss_top(), Some(Overlay::Detail));
        assert_eq!(s.dismiss_top(), Some(Overlay::Menu));
        assert_eq!(s.dismiss_top(), None, "no duplicate entry was left behind");
    }

    #[test]
    fn dismissing_an_empty_stack_is_harmless() {
        let mut s = OverlayStack::default();
        assert_eq!(s.dismiss_top(), None);
    }

    #[test]
    fn presenting_a_right_edge_panel_evicts_the_one_already_docked_there() {
        let mut s = OverlayStack::default();
        s.present(Overlay::Activity);
        assert_eq!(
            s.present(Overlay::Detail),
            Some(Overlay::Activity),
            "opening the detail panel must evict Activity from the shared right edge"
        );
        assert_eq!(s.top(), Some(Overlay::Detail));
        assert!(!s.contains(Overlay::Activity));
    }

    #[test]
    fn presenting_the_activity_panel_evicts_an_open_detail_panel_too() {
        // The reverse direction is the second shipped bug, not a symmetric restating
        // of the previous test: state.rs's `Overlays::open_detail_panel` closes
        // Activity *synchronously*, but the reverse close
        // (`close_detail_for_activity`) only ever ran from a `create_effect` in
        // activity.rs that fires one reactive tick later — so for one frame both
        // panels rendered at once. Both directions now go through this same
        // `present`, so both are synchronous and the asymmetry cannot recur.
        let mut s = OverlayStack::default();
        s.present(Overlay::Detail);
        assert_eq!(
            s.present(Overlay::Activity),
            Some(Overlay::Detail),
            "opening Activity must evict an open detail panel just as synchronously"
        );
        assert_eq!(s.top(), Some(Overlay::Activity));
        assert!(!s.contains(Overlay::Detail));
    }

    #[test]
    fn at_most_one_overlay_per_dock_after_any_sequence() {
        // The invariant OverlayStack exists to hold: present() may reorder or evict,
        // but it may never leave two overlays sharing a dock. Runs a sequence that
        // exercises raises, evictions in both right-edge directions, both modals and
        // the non-evicting viewer, then drains the stack through the public API and
        // checks every dock was seen at most once.
        let mut s = OverlayStack::default();
        for o in [
            Overlay::Menu,
            Overlay::Activity,
            Overlay::CommitDialog,
            Overlay::Detail,  // evicts Activity
            Overlay::Confirm, // evicts CommitDialog
            Overlay::Viewer,
            Overlay::Activity, // evicts Detail
            Overlay::Menu,     // already present: raised, not duplicated
            Overlay::Confirm,  // already present: raised, not duplicated
        ] {
            s.present(o);
        }

        let mut seen_docks = Vec::new();
        while let Some(o) = s.dismiss_top() {
            let d = o.dock();
            assert!(
                !seen_docks.contains(&d),
                "{d:?} was docked by more than one overlay at once"
            );
            seen_docks.push(d);
        }
    }

    #[test]
    fn dismiss_removes_a_buried_overlay_without_disturbing_the_top() {
        let mut s = OverlayStack::default();
        s.present(Overlay::Menu);
        s.present(Overlay::CommitDialog);
        assert!(
            s.dismiss(Overlay::Menu),
            "Menu was present, so dismiss must report true"
        );
        assert_eq!(
            s.top(),
            Some(Overlay::CommitDialog),
            "removing a buried overlay must not disturb the top"
        );
        assert!(!s.contains(Overlay::Menu));
    }

    #[test]
    fn dismissing_an_overlay_that_is_not_present_returns_false_and_changes_nothing() {
        let mut s = OverlayStack::default();
        s.present(Overlay::Menu);
        assert!(!s.dismiss(Overlay::Detail));
        assert_eq!(
            s.top(),
            Some(Overlay::Menu),
            "a dismiss of something absent must not touch what is actually there"
        );
    }

    #[test]
    fn the_two_modals_evict_each_other() {
        let mut s = OverlayStack::default();
        assert_eq!(s.present(Overlay::CommitDialog), None);
        assert_eq!(s.present(Overlay::Confirm), Some(Overlay::CommitDialog));
        assert_eq!(s.present(Overlay::CommitDialog), Some(Overlay::Confirm));
        assert_eq!(s.top(), Some(Overlay::CommitDialog));
    }

    #[test]
    fn a_full_screen_viewer_does_not_evict_the_panel_it_was_opened_from() {
        let mut s = OverlayStack::default();
        s.present(Overlay::Detail);
        assert_eq!(
            s.present(Overlay::Viewer),
            None,
            "the viewer docks separately from the panel underneath it"
        );
        assert!(s.contains(Overlay::Detail));
        assert!(s.contains(Overlay::Viewer));
    }
}
