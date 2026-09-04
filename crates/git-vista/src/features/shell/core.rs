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
    /// The write-failure notice (#316) — `Shell::error_notice`. A modal like
    /// Confirm, so it participates in the same exclusivity and Esc handling
    /// instead of being a native alert() outside the overlay system.
    Error,
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
    /// - [`Overlay::CommitDialog`], [`Overlay::Confirm`] and [`Overlay::Error`] →
    ///   [`Dock::Modal`]: all three are full-screen-backdrop modals.
    /// - [`Overlay::Menu`] → [`Dock::Anchored`]: the context menu floats at a pointer
    ///   coordinate over the canvas and legitimately coexists with a right-edge panel.
    /// - [`Overlay::Viewer`] → [`Dock::FullScreen`]: it sits over the panel it was
    ///   opened from, which is why it must not evict that panel.
    pub fn dock(self) -> Dock {
        match self {
            Overlay::Detail | Overlay::Activity => Dock::RightEdge,
            Overlay::CommitDialog | Overlay::Confirm | Overlay::Error => Dock::Modal,
            Overlay::Menu => Dock::Anchored,
            Overlay::Viewer => Dock::FullScreen,
        }
    }
}

/// One of `Shell`'s payload signals — the thing that holds *what* an overlay is
/// showing, as opposed to [`OverlayStack`]'s record of *which* overlays are up.
///
/// [`OverlayStack`] deliberately knows nothing about payloads: `present` returns the
/// overlay it evicted and leaves blanking it to the caller. That split is right, but it
/// left the other half of the rule — *which* signal each overlay owns — as an
/// un-modelled fact spelled out only inside `signals.rs`'s wasm-only
/// `Shell::clear_payload`, where no host test could reach it. Rust's exhaustiveness
/// check caught a *missing* arm there; it could not catch a *wrong* one. `Overlay::Detail
/// => self.viewer_doc.set(None)` compiled cleanly, blanked the viewer instead of the
/// detail panel, and left `detail_id` set forever — the same shape as the Esc bug this
/// module's doc opens with, one layer down.
///
/// Naming the slots makes that mistake checkable: [`Overlay::teardown`] is a total map
/// into this type, and `every_overlay_blanks_a_slot_no_other_overlay_owns` proves it is
/// injective, so two overlays can never claim one signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadSlot {
    /// `Shell::menu` — the context menu's `MenuData`.
    Menu,
    /// `Shell::commit_dialog` — the pending `CommitIntent`.
    CommitDialog,
    /// `Shell::confirm_op` — the `PendingOp` awaiting confirmation.
    ConfirmOp,
    /// `Shell::error_notice` — the write-failure `ErrorNotice` (#316).
    ErrorNotice,
    /// `Shell::detail_id` — the commit id the detail panel is showing.
    DetailId,
    /// `Shell::viewer_doc` — the `ViewerDoc` the full-screen viewer is showing.
    ViewerDoc,
    /// The Activity panel's own visibility flag. The one slot that is not a `Shell`
    /// field: it lives in `features::activity`, which has its own core and its own
    /// tests, and `Shell` is only its writer.
    ActivityOpen,
}

/// Everything that must be switched off when an overlay stops being presented.
///
/// Two fields rather than one because blanking a signal is not always the whole job.
/// The viewer also leaves state behind *outside* the reactive graph — a stored
/// comparison in `prefs` — and M4.27 (#80) decided that state must die with the
/// overlay, not with the particular close path that happened to be taken. Encoding it
/// here rather than as a hard-coded `Overlay::Viewer` arm in `signals.rs` is what lets a
/// host test state the rule ("exactly one overlay leaves persisted state behind, and it
/// is the viewer") instead of the rule living only in wasm-only code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Teardown {
    /// The payload signal to blank.
    pub slot: PayloadSlot,
    /// Whether going away must also clear `prefs`'s stored comparison.
    ///
    /// True for [`Overlay::Viewer`] alone. Cleared on teardown rather than inside
    /// `Shell::close_viewer` because Esc dismisses through `dismiss_top`, which never
    /// calls `close_viewer` — a stored comparison left behind by that path would
    /// reopen itself on the next load.
    pub clears_stored_comparison: bool,
}

impl Overlay {
    /// How many variants [`Overlay`] has.
    ///
    /// Hand-written, and kept honest by [`Self::ALL`]'s array length together with
    /// `all_lists_every_overlay_exactly_once` — see [`Self::index`] for how the two
    /// combine to make an unlisted variant impossible to ship.
    pub const COUNT: usize = 7;

    /// Every overlay, in declaration order.
    ///
    /// The list that did not exist when `gestures.rs`'s Esc handler was written — the
    /// module doc's bug #1 is, exactly, a hand-copied subset of this list with one
    /// entry missing and nothing to compare it against. Every completeness census
    /// below iterates this, so a handler that forgets an overlay now has something to
    /// be incomplete *against*.
    pub const ALL: [Overlay; Self::COUNT] = [
        Overlay::Menu,
        Overlay::CommitDialog,
        Overlay::Confirm,
        Overlay::Detail,
        Overlay::Viewer,
        Overlay::Activity,
        Overlay::Error,
    ];

    /// This variant's position in [`Self::ALL`].
    ///
    /// The load-bearing half of the completeness chain, and the reason [`Self::ALL`]
    /// cannot silently fall behind the enum. Adding an eighth variant walks an author
    /// through four forced steps, none of them optional and none of them a comment
    /// asking nicely:
    ///
    /// 1. This match stops being exhaustive — a **compile error**. The new variant
    ///    needs an index, and the only unused one is `7`.
    /// 2. `all_lists_every_overlay_exactly_once` indexes a `[bool; COUNT]` with it and
    ///    panics out of bounds — a **red test**. The fix is to raise [`Self::COUNT`].
    /// 3. Raising `COUNT` makes [`Self::ALL`]'s declared length disagree with its seven
    ///    elements — a **compile error**. The variant must be added to the list.
    /// 4. [`Self::dock`] and [`Self::teardown`] are now non-exhaustive too — two more
    ///    **compile errors**, forcing a dock and a payload slot to be chosen rather
    ///    than defaulted.
    const fn index(self) -> usize {
        match self {
            Overlay::Menu => 0,
            Overlay::CommitDialog => 1,
            Overlay::Confirm => 2,
            Overlay::Detail => 3,
            Overlay::Viewer => 4,
            Overlay::Activity => 5,
            Overlay::Error => 6,
        }
    }

    /// What must be switched off when this overlay stops being presented.
    ///
    /// The companion to [`Self::dock`]: `dock` decides what an overlay *evicts*,
    /// `teardown` decides what happens to whatever got evicted. Both are total maps out
    /// of [`Overlay`], both are the one statement of their rule, and both are here
    /// rather than in `signals.rs` for the same reason — that file is
    /// `#[cfg(target_arch = "wasm32")]`, so a rule written there is a rule no host test
    /// can read.
    pub fn teardown(self) -> Teardown {
        let slot = match self {
            Overlay::Menu => PayloadSlot::Menu,
            Overlay::CommitDialog => PayloadSlot::CommitDialog,
            Overlay::Confirm => PayloadSlot::ConfirmOp,
            Overlay::Error => PayloadSlot::ErrorNotice,
            Overlay::Detail => PayloadSlot::DetailId,
            Overlay::Viewer => PayloadSlot::ViewerDoc,
            Overlay::Activity => PayloadSlot::ActivityOpen,
        };
        Teardown {
            slot,
            // M4.27 (#80). Stated as a match rather than `self == Viewer` so that an
            // eighth overlay with persisted state of its own is a compile error here
            // too, instead of silently inheriting `false`.
            clears_stored_comparison: match self {
                Overlay::Viewer => true,
                Overlay::Menu
                | Overlay::CommitDialog
                | Overlay::Confirm
                | Overlay::Error
                | Overlay::Detail
                | Overlay::Activity => false,
            },
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

/// Which of four layout modes the window is in, decided from width alone.
///
/// A pure function of width, never of the previous mode — that property is why
/// Rust owns this signal instead of a CSS/Rust hybrid: a breakpoint duplicated in
/// both places leaves a band of widths where the two disagree, a bug that only
/// reproduces at one exact window size. Stability under a Stage Manager drag
/// comes from debouncing the resize signal upstream (`signals::install_mode_signal`),
/// not from hysteresis here — hysteresis would make this a function of
/// `(width, previous_mode)`, so the same width could answer two different ways
/// depending on approach direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMode {
    /// < 600px — narrow Stage Manager / split screen. One primary task visible
    /// at a time.
    Compact,
    /// 600–1023px — iPad portrait (834pt), medium split screen (507–678pt).
    Portrait,
    /// 1024–1439px — iPad landscape (1194pt).
    Wide,
    /// 1440px and above — a wide external monitor. Named for what's actually
    /// knowable: a web app can see a width, never that a display is external.
    UltraWide,
}

impl ShellMode {
    pub fn for_width(width: f64) -> Self {
        if width < 600.0 {
            Self::Compact
        } else if width < 1024.0 {
            Self::Portrait
        } else if width < 1440.0 {
            Self::Wide
        } else {
            Self::UltraWide
        }
    }

    /// The single CSS class the stylesheet keys off. No `@media` queries for
    /// mode exist anywhere in `styles.css` — this class is the only decider.
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Compact => "shell-compact",
            Self::Portrait => "shell-portrait",
            Self::Wide => "shell-wide",
            Self::UltraWide => "shell-ultrawide",
        }
    }
}

/// The decision half of the debounced resize listener: which resize checks are still
/// current, and whether a check that *is* current actually changes anything (M1.12, #65).
///
/// This exists because the one thing nobody could confirm about M1.12's first slice was
/// the thing that matters most — that the layout class **settles** under a Stage Manager
/// drag rather than thrashing. That question used to be answerable only by watching a
/// real browser, because the whole debounce lived inside a `#[cfg(target_arch = "wasm32")]`
/// closure in `signals.rs` with `web_sys` and `set_timeout` braided through it. Splitting
/// the *decision* out from the *scheduling* makes the settling property provable at the
/// host level; what remains browser-only is that `resize` fires and that
/// `gestures::viewport_size()` reports the width, neither of which is where the subtle
/// behaviour was.
///
/// Two independent reasons a scheduled check publishes nothing:
///
/// 1. **It was superseded.** Each resize event takes a fresh token from
///    [`Self::observe_resize`]; [`Self::settle`] ignores any token that is not the latest.
///    A burst of a hundred drag events therefore produces at most one publication — the
///    last one — and the ninety-nine stale timeouts are silent no-ops rather than
///    something that has to be found and cancelled.
/// 2. **Nothing changed.** Most resizes never leave the current band: dragging a window
///    from 700px to 780px is still `Portrait`. Publishing there would notify every
///    subscriber of the mode signal — Leptos's `set` fires on write, not on difference —
///    so the class attribute would be rewritten on every settled drag that changed
///    nothing. [`Self::settle`] returns `Some` only on an actual band change, which is
///    what makes "settles rather than thrashes" true for a *reader* of the signal and not
///    merely for the DOM.
///
/// Deliberately not a debouncer: it owns no clock and no timer. It cannot be, if it is to
/// be testable off-target — and the timing half (150ms, one timeout per event) is the part
/// that is genuinely uninteresting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeSettler {
    current: ShellMode,
    generation: u64,
}

impl ModeSettler {
    /// Start from the width the window has right now, with no check outstanding.
    pub fn new(initial_width: f64) -> Self {
        Self {
            current: ShellMode::for_width(initial_width),
            generation: 0,
        }
    }

    /// The mode last published — what the signal currently holds.
    pub fn current(&self) -> ShellMode {
        self.current
    }

    /// A resize event arrived. Returns the token the check scheduled for *this* event
    /// must present to [`Self::settle`]; every token handed out before this one is now
    /// stale.
    pub fn observe_resize(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// A scheduled check fired. Returns the new mode to publish, or `None` — either
    /// because a later resize superseded this check, or because `width` is still in the
    /// band already current.
    ///
    /// Takes the width at the moment the check fires rather than the width at the moment
    /// the event arrived, on purpose: the surviving check is the one that must reflect
    /// where the drag actually stopped.
    pub fn settle(&mut self, token: u64, width: f64) -> Option<ShellMode> {
        if token != self.generation {
            return None;
        }
        let next = ShellMode::for_width(width);
        if next == self.current {
            return None;
        }
        self.current = next;
        Some(next)
    }
}

/// Whether the browser last reported the network adapter as up (M2.22a, #241).
///
/// Framework-free, like [`OverlayStack`] and [`ModeSettler`] above: the only
/// thing worth testing off-target is that a sequence of online/offline
/// transitions lands where it should, so that behaviour is split out from
/// `signals.rs::install_connectivity_signal()`'s `web_sys` event-listener
/// wiring the same way `ModeSettler` is split from the resize listener. What
/// stays browser-only is that `online`/`offline` actually fire and that
/// `navigator.onLine` reports the adapter truthfully — neither of those is
/// where a transition bug would hide.
///
/// Seeded from `navigator.onLine` at startup rather than defaulting to a
/// fixed value: an app opened while already offline (airplane mode, a dead
/// tunnel before the first request) must refuse writes from the first paint,
/// not only after the first `offline` event it happens to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectivityCore {
    online: bool,
}

impl ConnectivityCore {
    /// `online` is the seed read from `navigator.onLine` (or from a test).
    pub fn new(online: bool) -> Self {
        Self { online }
    }

    /// The plain, synchronous read `api.rs`'s `refuse_if_offline()` guard
    /// checks — no reactive subscription, so a write function can call it
    /// inline the same way `session::signals::is_lan()` is called inline.
    pub fn is_online(&self) -> bool {
        self.online
    }

    /// Apply a window `online` or `offline` event.
    pub fn set_online(&mut self, online: bool) {
        self.online = online;
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
            Overlay::Error,    // evicts Confirm — the third modal (#316)
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
    fn the_modals_evict_each_other() {
        // Three modals since #316's error notice joined the dock. Any modal
        // presenting must evict whichever modal currently holds Dock::Modal.
        let mut s = OverlayStack::default();
        assert_eq!(s.present(Overlay::CommitDialog), None);
        assert_eq!(s.present(Overlay::Confirm), Some(Overlay::CommitDialog));
        assert_eq!(s.present(Overlay::CommitDialog), Some(Overlay::Confirm));
        assert_eq!(s.present(Overlay::Error), Some(Overlay::CommitDialog));
        assert_eq!(s.present(Overlay::Confirm), Some(Overlay::Error));
        assert_eq!(s.top(), Some(Overlay::Confirm));
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

    #[test]
    fn for_width_picks_compact_below_600() {
        assert_eq!(ShellMode::for_width(599.0), ShellMode::Compact);
        assert_eq!(ShellMode::for_width(0.0), ShellMode::Compact);
    }

    #[test]
    fn for_width_picks_portrait_from_600_to_1023() {
        assert_eq!(ShellMode::for_width(600.0), ShellMode::Portrait);
        assert_eq!(ShellMode::for_width(834.0), ShellMode::Portrait);
        assert_eq!(ShellMode::for_width(1023.0), ShellMode::Portrait);
    }

    #[test]
    fn for_width_picks_wide_from_1024_to_1439() {
        assert_eq!(ShellMode::for_width(1024.0), ShellMode::Wide);
        assert_eq!(ShellMode::for_width(1194.0), ShellMode::Wide);
        assert_eq!(ShellMode::for_width(1439.0), ShellMode::Wide);
    }

    #[test]
    fn for_width_picks_ultrawide_at_1440_and_above() {
        assert_eq!(ShellMode::for_width(1440.0), ShellMode::UltraWide);
        assert_eq!(ShellMode::for_width(2560.0), ShellMode::UltraWide);
    }

    #[test]
    fn for_width_is_a_pure_function_same_width_same_answer_every_time() {
        for _ in 0..5 {
            assert_eq!(ShellMode::for_width(650.0), ShellMode::Portrait);
        }
    }

    #[test]
    fn a_drag_that_crosses_bands_publishes_only_the_band_it_ended_in() {
        // The property M1.12 shipped without being able to check: the layout class
        // settles rather than thrashing. This is the shape a Stage Manager drag actually
        // has — resize events keep arriving while earlier events' 150ms checks are firing,
        // and each check reads the width at the moment it fires, which mid-drag is some
        // intermediate width the user never rested at.
        let mut s = ModeSettler::new(1200.0);
        assert_eq!(s.current(), ShellMode::Wide);

        // A drag from 1200 down to 500. Five events; the first three land before the
        // first check fires.
        let t1 = s.observe_resize();
        let t2 = s.observe_resize();
        let t3 = s.observe_resize();

        // t1's check fires — but the drag is still moving and the window is 900px wide
        // right now. 900 is Portrait, a genuinely different band from both where the drag
        // started and where it ends, so a `None` here is not a coincidence of the widths:
        assert_ne!(ShellMode::for_width(900.0), ShellMode::for_width(1200.0));
        assert_ne!(ShellMode::for_width(900.0), ShellMode::for_width(500.0));
        assert_eq!(
            s.settle(t1, 900.0),
            None,
            "a superseded check must not publish the band the drag happened to be passing through"
        );
        assert_eq!(s.current(), ShellMode::Wide, "still where the drag started");

        let t4 = s.observe_resize();
        let t5 = s.observe_resize();

        // The rest of the checks drain, all now reading the resting width.
        let published: Vec<_> = [t2, t3, t4, t5]
            .into_iter()
            .filter_map(|t| s.settle(t, 500.0))
            .collect();
        assert_eq!(
            published,
            vec![ShellMode::Compact],
            "exactly one publication, and it is the band the drag ended in"
        );
    }

    #[test]
    fn settling_inside_the_current_band_publishes_nothing() {
        // Most resizes never leave the band: 700 -> 780 is Portrait either way. Leptos's
        // `set` notifies on write rather than on difference, so publishing here would
        // rewrite the class attribute and wake every subscriber for a layout that did not
        // change. The current token is used, so staleness is not what makes this `None`.
        let mut s = ModeSettler::new(700.0);
        let t = s.observe_resize();
        assert_eq!(ShellMode::for_width(780.0), ShellMode::Portrait);
        assert_eq!(s.settle(t, 780.0), None);
        assert_eq!(s.current(), ShellMode::Portrait);
    }

    #[test]
    fn a_band_change_is_published_exactly_once_even_if_the_same_check_runs_again() {
        let mut s = ModeSettler::new(700.0);
        let t = s.observe_resize();
        assert_eq!(s.settle(t, 1200.0), Some(ShellMode::Wide));
        assert_eq!(
            s.settle(t, 1200.0),
            None,
            "the band is now current, so re-running the same check publishes nothing"
        );
        assert_eq!(s.current(), ShellMode::Wide);
    }

    #[test]
    fn a_check_older_than_the_latest_resize_is_always_stale() {
        let mut s = ModeSettler::new(700.0);
        let old = s.observe_resize();
        let newer = s.observe_resize();
        assert_ne!(old, newer, "each event must take its own token");
        assert_eq!(s.settle(old, 1200.0), None);
        assert_eq!(
            s.current(),
            ShellMode::Portrait,
            "a stale check must not move the mode even when the width would"
        );
        assert_eq!(
            s.settle(newer, 1200.0),
            Some(ShellMode::Wide),
            "the latest check still works after a stale one was rejected"
        );
    }

    #[test]
    fn the_initial_mode_comes_from_the_width_at_construction() {
        assert_eq!(ModeSettler::new(834.0).current(), ShellMode::Portrait);
        assert_eq!(ModeSettler::new(2560.0).current(), ShellMode::UltraWide);
    }

    #[test]
    fn a_check_that_never_fired_does_not_block_later_ones() {
        // A timeout can be dropped outright (the tab was backgrounded, the scope was
        // disposed). Nothing in the settler waits on it, so a later burst still settles.
        let mut s = ModeSettler::new(500.0);
        let _abandoned = s.observe_resize();
        let t = s.observe_resize();
        assert_eq!(s.settle(t, 1500.0), Some(ShellMode::UltraWide));
    }

    #[test]
    fn css_class_has_one_distinct_class_per_variant() {
        let classes = [
            ShellMode::Compact.css_class(),
            ShellMode::Portrait.css_class(),
            ShellMode::Wide.css_class(),
            ShellMode::UltraWide.css_class(),
        ];
        for c in &classes {
            assert!(c.starts_with("shell-"), "unexpected class shape: {c}");
        }
        let unique: std::collections::HashSet<_> = classes.iter().collect();
        assert_eq!(unique.len(), 4, "classes must be pairwise distinct");
    }

    #[test]
    fn connectivity_seeds_from_the_constructor_argument() {
        assert!(ConnectivityCore::new(true).is_online());
        assert!(!ConnectivityCore::new(false).is_online());
    }

    #[test]
    fn connectivity_transitions_offline_then_back_online() {
        let mut c = ConnectivityCore::new(true);
        assert!(c.is_online());
        c.set_online(false);
        assert!(!c.is_online(), "an offline event must flip the read");
        c.set_online(true);
        assert!(c.is_online(), "a later online event must flip it back");
    }

    #[test]
    fn connectivity_can_start_offline_and_come_online() {
        // The app can be opened while already offline (airplane mode, a dead
        // tunnel before the first paint) — the seed must reflect that, not
        // assume online until proven otherwise.
        let mut c = ConnectivityCore::new(false);
        assert!(!c.is_online());
        c.set_online(true);
        assert!(c.is_online());
    }

    #[test]
    fn repeated_identical_events_are_harmless() {
        let mut c = ConnectivityCore::new(true);
        c.set_online(true);
        c.set_online(true);
        assert!(c.is_online(), "duplicate online events change nothing");
        c.set_online(false);
        c.set_online(false);
        assert!(!c.is_online(), "duplicate offline events change nothing");
    }
    // ---- the overlay census ---------------------------------------------
    //
    // `dock` and `present` above are proved directly. These prove the *other*
    // half of the consolidation — that the set of overlays is closed, that each
    // one owns exactly one payload signal, and that the wasm-only `Shell` still
    // asks for both answers instead of keeping its own copies.

    #[test]
    fn all_lists_every_overlay_exactly_once() {
        // The list `gestures.rs` did not have. Step 2 of the chain in
        // `Overlay::index`'s doc: an eighth variant gets index 7, indexes this
        // array out of bounds, and the panic is what sends the author to
        // `COUNT` — which in turn makes `ALL`'s declared length wrong, which is
        // a compile error they cannot ignore.
        let mut seen = [false; Overlay::COUNT];
        for o in Overlay::ALL {
            let i = o.index();
            assert!(!seen[i], "{o:?} appears in Overlay::ALL more than once");
            seen[i] = true;
        }
        for (i, present) in seen.iter().enumerate() {
            assert!(
                *present,
                "no overlay in Overlay::ALL has index {i} — a variant exists that ALL \
                 does not list, so every census that iterates ALL is silently skipping it"
            );
        }
    }

    #[test]
    fn every_overlay_blanks_a_slot_no_other_overlay_owns() {
        // The check `match o { … }` in the old `clear_payload` could not make.
        // Exhaustiveness proved every overlay had *an* arm; nothing proved the
        // arm blanked that overlay's own signal. `Overlay::Detail =>
        // self.viewer_doc.set(None)` compiled, closed the viewer instead of the
        // detail panel, and stranded `detail_id` set forever.
        let mut owner: Vec<(PayloadSlot, Overlay)> = Vec::new();
        for o in Overlay::ALL {
            let slot = o.teardown().slot;
            if let Some((_, first)) = owner.iter().find(|(s, _)| *s == slot) {
                panic!(
                    "{o:?} and {first:?} both claim {slot:?} — one of them can never be blanked"
                );
            }
            owner.push((slot, o));
        }
        assert_eq!(owner.len(), Overlay::COUNT, "every overlay must own a slot");
    }

    #[test]
    fn only_the_viewer_leaves_persisted_state_behind() {
        // M4.27 (#80) as a statement rather than an `Overlay::Viewer` arm buried
        // in wasm-only code. Both directions matter: the viewer must clear the
        // stored comparison (or Esc leaves one that reopens itself on the next
        // load), and nothing else may (clearing it when the *menu* closes would
        // silently discard a comparison the user is still in).
        let clearing: Vec<Overlay> = Overlay::ALL
            .into_iter()
            .filter(|o| o.teardown().clears_stored_comparison)
            .collect();
        assert_eq!(
            clearing,
            vec![Overlay::Viewer],
            "exactly one overlay clears prefs' stored comparison, and it is the viewer"
        );
    }

    // ---- the seam ------------------------------------------------------
    //
    // Everything above proves the rules. These prove `signals.rs` still asks
    // them. That file is `#[cfg(target_arch = "wasm32")]`, so `cargo test` never
    // compiles it: a rule re-derived there would leave every test above green
    // while the running app stopped using the answer. Reading the source as text
    // is the only way a host test can see it at all — the same trade
    // `features::history::core` makes against `app/mod.rs`, and
    // `features::a11y::audit` against markup it cannot mount. It is a text
    // guard, not a compiled proof, and is stated that way on purpose.
    //
    // This `include_str!` is also what retires `wasm_module_census::EXEMPT`'s
    // entry for this file: that census's coverage proxy is "a host test reads
    // this file", and this is that read.

    /// The source of the wasm-only shell, read as text.
    const SIGNALS: &str = include_str!("signals.rs");

    /// Text between `open` and the first `close` after it.
    fn block_after(haystack: &str, open: &str, close: &str, what: &str) -> String {
        let after = haystack
            .split_once(open)
            .unwrap_or_else(|| panic!("signals.rs no longer contains {what} (anchor: {open:?})"))
            .1;
        let end = after.find(close).unwrap_or_else(|| {
            panic!("{what} is no longer a closed block (looking for {close:?})")
        });
        after[..end].to_string()
    }

    /// Collapse every run of whitespace to one space, so these guards survive
    /// rustfmt moving a line break without surviving an actual edit.
    fn squeezed(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The body of one `Shell` method, whitespace-squeezed.
    fn shell_fn(name: &str) -> String {
        squeezed(&block_after(
            SIGNALS,
            &format!("fn {name}("),
            "\n    }",
            &format!("`Shell::{name}`"),
        ))
    }

    #[test]
    fn clear_payload_asks_core_what_to_switch_off() {
        let body = shell_fn("clear_payload");
        assert!(
            body.contains("o.teardown()"),
            "`Shell::clear_payload` no longer calls `Overlay::teardown`, so the \
             overlay-to-signal map has a second home again and only one of them is \
             tested. Body was:\n{body}"
        );
        assert!(
            !body.contains("Overlay::"),
            "`Shell::clear_payload` names `Overlay` variants again, which means it is \
             re-deriving the map instead of asking for it. Body was:\n{body}"
        );
        assert!(
            body.contains("teardown.clears_stored_comparison"),
            "`Shell::clear_payload` no longer asks whether the overlay leaves persisted \
             state behind — M4.27 (#80)'s rule is hard-coded here again. Body was:\n{body}"
        );
        assert!(
            body.contains("crate::prefs::clear_comparison()"),
            "nothing in `Shell::clear_payload` clears the stored comparison any more. \
             If that moved elsewhere, check Esc still reaches it: `dismiss_top` never \
             calls `close_viewer`, which is why it lives in this funnel. Body was:\n{body}"
        );
    }

    #[test]
    fn each_payload_slot_blanks_the_signal_it_is_named_for() {
        // The half `clear_payload_asks_core_what_to_switch_off` cannot reach.
        // Moving the map into core made a *wrong overlay* unrepresentable; it did
        // not make a *wrong signal* unrepresentable, because `PayloadSlot::DetailId
        // => self.viewer_doc.set(None)` still compiles. The slot names and the
        // field names correspond one-for-one, so the correspondence is checkable
        // as text — and this is the only level at which it is checkable at all.
        //
        // The table is built by an exhaustive match, so an eighth slot is a
        // compile error here before it is a missing guard.
        let body = shell_fn("clear_payload");
        for o in Overlay::ALL {
            let slot = o.teardown().slot;
            let expected = match slot {
                PayloadSlot::Menu => "PayloadSlot::Menu => self.menu.set(None)",
                PayloadSlot::CommitDialog => {
                    "PayloadSlot::CommitDialog => self.commit_dialog.set(None)"
                }
                PayloadSlot::ConfirmOp => "PayloadSlot::ConfirmOp => self.confirm_op.set(None)",
                PayloadSlot::ErrorNotice => {
                    "PayloadSlot::ErrorNotice => self.error_notice.set(None)"
                }
                PayloadSlot::DetailId => "PayloadSlot::DetailId => self.detail_id.set(None)",
                PayloadSlot::ViewerDoc => "PayloadSlot::ViewerDoc => self.viewer_doc.set(None)",
                PayloadSlot::ActivityOpen => "PayloadSlot::ActivityOpen => self.activity.close()",
            };
            assert!(
                body.contains(expected),
                "`Shell::clear_payload` has no arm blanking {slot:?}'s own signal \
                 (expected {expected:?}), so {o:?} either cannot be switched off or \
                 switches off something else. Body was:\n{body}"
            );
        }
    }

    #[test]
    fn every_overlay_is_raised_before_its_payload_is_filled() {
        // The ordering half, and the one that asserting values alone cannot
        // reach. The module doc's bug #2 was a *timing* bug: both right-edge
        // panels rendered together for a frame because the eviction happened a
        // reactive tick after the new panel appeared. `present` evicts
        // synchronously now, but that only helps while the opener calls it
        // *before* filling its own payload — Leptos `set` runs subscribers
        // synchronously, so swapping the two lines of `open_detail` puts the
        // detail panel on screen while Activity is still up, and every test in
        // this file stays green.
        //
        // Driven by an exhaustive match so an eighth overlay must name its
        // opener here rather than quietly having none.
        for o in Overlay::ALL {
            let (opener, fill) = match o {
                Overlay::Menu => ("open_menu", "self.menu.set(Some("),
                Overlay::CommitDialog => ("open_commit_dialog", "self.commit_dialog.set(Some("),
                Overlay::Confirm => ("open_confirm", "self.confirm_op.set(Some("),
                Overlay::Error => ("open_error", "self.error_notice.set(Some("),
                Overlay::Detail => ("open_detail", "self.detail_id.set(Some("),
                Overlay::Viewer => ("open_viewer", "self.viewer_doc.set(Some("),
                // The one control that both opens and closes; the open branch is
                // still an ordinary present-then-fill.
                Overlay::Activity => ("toggle_activity", "self.activity.open()"),
            };
            let body = shell_fn(opener);
            let raise = format!("self.present(Overlay::{o:?})");
            let raise_at = body.find(&raise).unwrap_or_else(|| {
                panic!(
                    "`Shell::{opener}` no longer raises {o:?} through `present`, so \
                     nothing evicts whatever shares its dock. Body was:\n{body}"
                )
            });
            let fill_at = body.find(fill).unwrap_or_else(|| {
                panic!(
                    "`Shell::{opener}` no longer fills {o:?}'s payload with {fill:?} — \
                     if the field was renamed, rename it here too. Body was:\n{body}"
                )
            });
            assert!(
                raise_at < fill_at,
                "`Shell::{opener}` fills {o:?}'s payload before raising it. The evicted \
                 overlay is blanked inside `present`, so filling first puts both \
                 overlays on screen at once — exactly the frame this module was \
                 written to make impossible. Body was:\n{body}"
            );
        }
    }
}
