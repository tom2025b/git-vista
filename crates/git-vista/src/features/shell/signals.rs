//! The single holder of [`OverlayStack`], and of every overlay's payload — wasm only
//! (M1.11, #64).
//!
//! This type is the reason `Overlays` is gone. That bundle held thirteen fields and no
//! rules: any view could poke any signal, so "which overlays are up" was a fact you could
//! only learn by reading six signals at once and hoping nobody had written a seventh. Two
//! bugs lived in that gap, and both are structural rather than careless:
//!
//! * `gestures.rs`'s Esc handler destructured five of the six overlays and simply omitted
//!   the Activity panel, so Esc could not close it. Nothing could have caught that — there
//!   was no list of overlays to be incomplete against.
//! * The Activity panel and the commit detail panel both dock the right edge. Opening the
//!   detail panel closed Activity synchronously, but opening Activity closed the detail
//!   panel from a `create_effect` one reactive tick later, so both rendered together for a
//!   frame.
//!
//! Here the stack is the only writer. [`Shell::open_detail`] cannot forget to close
//! Activity, because it does not close it — [`OverlayStack::present`] evicts whatever else
//! occupies the right edge and hands the evicted overlay back, and [`Shell::clear_payload`]
//! is the one place that knows how to blank each one. The rule is enforced in one function
//! instead of remembered at eleven call sites.
//!
//! Created once in `App`, above `graph_canvas`, for the same reason [`crate::state::Features`]
//! is: the five overlay signals used to be born inside the canvas, so every epoch bump
//! destroyed them. That is also what retires Task 6's deferred step — the signals leave
//! canvas scope here rather than being moved twice.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use leptos::{
    create_rw_signal, on_cleanup, store_value, RwSignal, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, StoredValue,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::gestures;

use crate::features::activity::signals::Activity;
use crate::features::shell::core::{ModeSettler, Overlay, OverlayStack, ShellMode};
use crate::state::{CommitDialog, MenuData, PendingOp, ViewerDoc};

/// Feeds a `ShellMode` signal from window width, debounced 150ms so a Stage
/// Manager drag doesn't thrash the layout on every intermediate resize event.
///
/// This function is now only the *scheduling* half: it listens, it waits 150ms,
/// and it reads the width. Every decision — whether a check has been superseded,
/// and whether the width that check sees actually changes the band — belongs to
/// [`ModeSettler`], which is a plain host-testable type. That split is
/// deliberate: the settling behaviour is the one thing M1.12 shipped without
/// being able to verify (nobody could confirm the layout class settles rather
/// than thrashes without a real browser), and it was unverifiable precisely
/// because it was tangled up in this `web_sys` closure. See `ModeSettler`'s tests
/// for the drag-crosses-bands case this used to only be able to assert by eye.
///
/// What is still browser-only, and is *not* covered by those tests: that a
/// `resize` event fires at all, that `gestures::viewport_size()` reports the
/// live width, and that 150ms is the right interval for a Stage Manager drag.
///
/// No hysteresis: `ShellMode::for_width` never sees the previous mode, only the
/// current width — see that function's doc comment for why.
pub fn install_mode_signal() -> RwSignal<ShellMode> {
    let settler = Rc::new(RefCell::new(ModeSettler::new(gestures::viewport_size().0)));
    let mode = create_rw_signal(settler.borrow().current());

    if let Some(win) = web_sys::window() {
        let cb = Closure::<dyn FnMut()>::new(move || {
            let token = settler.borrow_mut().observe_resize();
            let settler = settler.clone();
            leptos::set_timeout(
                move || {
                    // The borrow is released before touching the signal: `set` runs
                    // subscribers synchronously, and a subscriber that resized the
                    // window would re-enter this closure while the RefCell was held.
                    let published = settler
                        .borrow_mut()
                        .settle(token, gestures::viewport_size().0);
                    if let Some(next) = published {
                        // `try_set`, not `set`, for the reason `Shell::present` uses
                        // `try_update`: this timeout can outlive its scope by up to
                        // 150ms after `on_cleanup` has pulled the listener, and a
                        // disposed scope has no layout to relayout.
                        let _ = mode.try_set(next);
                    }
                },
                Duration::from_millis(150),
            );
        });
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        let win2 = win.clone();
        on_cleanup(move || {
            let _ = win2.remove_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        });
    }

    mode
}

/// Every overlay the app can put on screen, and the order they were raised in.
///
/// The payload signals are private on purpose. Handing them out is how the old bundle
/// worked, and it is precisely what let a writer change what is visible without the stack
/// hearing about it.
#[derive(Clone, Copy)]
pub struct Shell {
    stack: RwSignal<OverlayStack>,
    menu: RwSignal<Option<MenuData>>,
    commit_dialog: RwSignal<Option<CommitDialog>>,
    confirm_op: RwSignal<Option<PendingOp>>,
    detail_id: RwSignal<Option<String>>,
    viewer_doc: RwSignal<Option<ViewerDoc>>,
    /// The Activity panel's visibility still lives in its own feature — it has a core with
    /// its own tests, and the shared status read keys on it. `Shell` is simply the only
    /// thing that *writes* it now.
    activity: Activity,
    /// One-shot "scroll the Changes section into view", set by the menu's "Show diff" item
    /// and consumed by the detail panel's next render. A `StoredValue`, not a signal: it is
    /// an instruction the next render consumes, not state the UI reflects.
    scroll_diff: StoredValue<bool>,
}

impl Shell {
    /// `activity` is passed in rather than created here because the topbar button and the
    /// shared status read both hold it already, and it predates the graph.
    pub fn new(activity: Activity) -> Self {
        Self {
            stack: create_rw_signal(OverlayStack::default()),
            menu: create_rw_signal(None::<MenuData>),
            commit_dialog: create_rw_signal(None::<CommitDialog>),
            confirm_op: create_rw_signal(None::<PendingOp>),
            detail_id: create_rw_signal(None::<String>),
            viewer_doc: create_rw_signal(None::<ViewerDoc>),
            activity,
            scroll_diff: store_value(false),
        }
    }

    // -- Raising an overlay ------------------------------------------------------------

    /// Show the context menu described by `data`.
    pub fn open_menu(&self, data: MenuData) {
        self.present(Overlay::Menu);
        self.menu.set(Some(data));
    }

    /// Show the commit-message modal.
    ///
    /// The caller still stamps the ghost-click guard (`Dialogs::open`) itself: the guard is
    /// about *when* a modal appeared, which is the dialogs feature's business, and routing
    /// it through here would put two unrelated rules in one call.
    pub fn open_commit_dialog(&self, d: CommitDialog) {
        self.present(Overlay::CommitDialog);
        self.commit_dialog.set(Some(d));
    }

    /// Show the branch-operation / undo confirmation modal on `op`.
    pub fn open_confirm(&self, op: PendingOp) {
        self.present(Overlay::Confirm);
        self.confirm_op.set(Some(op));
    }

    /// Show the commit detail panel on `id`, optionally scrolling to its Changes section.
    ///
    /// `scroll_to_diff` is what used to be a separate `scroll_diff.set_value(…)` poke that
    /// every caller had to remember to *clear* on the plain-details path — a wish left set
    /// by an earlier "Show diff" would otherwise fire on the next open. Making it an
    /// argument means there is no path that forgets.
    ///
    /// Closing the Activity panel is not done here and must not be: `present` evicts
    /// whatever else holds the right edge.
    pub fn open_detail(&self, id: String, scroll_to_diff: bool) {
        self.scroll_diff.set_value(scroll_to_diff);
        self.present(Overlay::Detail);
        self.detail_id.set(Some(id));
    }

    /// Show `doc` full-screen, over the panel it was opened from.
    pub fn open_viewer(&self, doc: ViewerDoc) {
        self.present(Overlay::Viewer);
        self.viewer_doc.set(Some(doc));
    }

    /// The topbar button: the same control opens and closes the Activity panel.
    pub fn toggle_activity(&self) {
        if self.activity.is_open_untracked() {
            self.close_activity();
        } else {
            self.present(Overlay::Activity);
            self.activity.open();
        }
    }

    // -- Dismissing --------------------------------------------------------------------

    /// Esc: dismiss the topmost overlay, whatever it is.
    ///
    /// This replaces a hand-written `if/else if` chain over five of the six overlays. The
    /// chain's order — viewer, menu, the modals, then the detail panel — is reproduced by
    /// insertion order, because that is the order the real open sequences produce: the
    /// viewer is opened *from* the detail panel, and every modal is opened from a menu that
    /// closes itself in the same handler. What changes is that the Activity panel is
    /// reachable by Esc at all.
    pub fn dismiss_top(&self) {
        if let Some(o) = self.stack.try_update(|s| s.dismiss_top()).flatten() {
            self.clear_payload(o);
        }
    }

    pub fn close_menu(&self) {
        self.dismiss(Overlay::Menu);
    }

    pub fn close_commit_dialog(&self) {
        self.dismiss(Overlay::CommitDialog);
    }

    pub fn close_confirm(&self) {
        self.dismiss(Overlay::Confirm);
    }

    pub fn close_detail(&self) {
        self.dismiss(Overlay::Detail);
    }

    pub fn close_viewer(&self) {
        self.dismiss(Overlay::Viewer);
    }

    pub fn close_activity(&self) {
        self.dismiss(Overlay::Activity);
    }

    // -- Reading -----------------------------------------------------------------------

    /// A tracked read — the menu re-renders from it.
    pub fn menu(&self) -> Option<MenuData> {
        self.menu.get()
    }

    /// A tracked read — the commit modal re-renders from it.
    pub fn commit_dialog(&self) -> Option<CommitDialog> {
        self.commit_dialog.get()
    }

    /// An untracked read, for the submit handler that must not subscribe.
    pub fn commit_dialog_untracked(&self) -> Option<CommitDialog> {
        self.commit_dialog.get_untracked()
    }

    /// A tracked read — the confirm modal re-renders from it.
    pub fn confirm_op(&self) -> Option<PendingOp> {
        self.confirm_op.get()
    }

    /// An untracked read, for the confirm handler that must not subscribe.
    pub fn confirm_op_untracked(&self) -> Option<PendingOp> {
        self.confirm_op.get_untracked()
    }

    /// A tracked read — the detail panel and its two `Resource` keys read it.
    pub fn detail_id(&self) -> Option<String> {
        self.detail_id.get()
    }

    /// A tracked read — the viewer and its `Resource` key read it.
    pub fn viewer_doc(&self) -> Option<ViewerDoc> {
        self.viewer_doc.get()
    }

    /// A tracked read of the Activity panel's visibility.
    pub fn activity_is_open(&self) -> bool {
        self.activity.is_open()
    }

    /// Consume the "scroll to the Changes section" wish, if one was left by "Show diff".
    ///
    /// Reads *and* clears, so the wish cannot fire twice — the panel re-renders on every
    /// diff fetch, and a wish that outlived its open would scroll an unrelated commit's
    /// panel.
    pub fn take_diff_scroll(&self) -> bool {
        let wanted = self.scroll_diff.get_value();
        if wanted {
            self.scroll_diff.set_value(false);
        }
        wanted
    }

    // -- The one place the stack and the payloads move together ------------------------

    /// Raise `o`, blanking whatever it evicted from its dock.
    ///
    /// `try_update` rather than `update` for the same reason the operations feature uses
    /// it: it answers `None` when the owning scope is already disposed, and a disposed
    /// scope cannot show an overlay either — so nothing is cleared and nothing is left
    /// half-presented.
    fn present(&self, o: Overlay) {
        if let Some(evicted) = self.stack.try_update(|s| s.present(o)).flatten() {
            self.clear_payload(evicted);
        }
    }

    /// Remove `o` from the stack and blank its payload.
    ///
    /// Unconditionally clears the payload, even when the stack did not hold `o`. The two
    /// can only disagree if something bypassed this type, and in that case the visible
    /// state is what the user cares about.
    fn dismiss(&self, o: Overlay) {
        self.stack.update(|s| {
            s.dismiss(o);
        });
        self.clear_payload(o);
    }

    /// The one function that knows how each overlay is switched off.
    fn clear_payload(&self, o: Overlay) {
        match o {
            Overlay::Menu => self.menu.set(None),
            Overlay::CommitDialog => self.commit_dialog.set(None),
            Overlay::Confirm => self.confirm_op.set(None),
            Overlay::Detail => self.detail_id.set(None),
            Overlay::Viewer => self.viewer_doc.set(None),
            Overlay::Activity => self.activity.close(),
        }
    }
}
