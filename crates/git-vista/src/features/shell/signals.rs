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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use leptos::{
    create_rw_signal, on_cleanup, store_value, RwSignal, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, StoredValue,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::gestures;

use super::sheet::{InspectorPlacement, SheetGeometry, SheetState};
use super::{sheet_render_metrics, SheetDrag, SheetRenderMetrics};
use crate::features::activity::signals::Activity;
use crate::features::dialogs::core::ErrorNotice;
use crate::features::shell::core::{
    ConnectivityCore, ModeSettler, Overlay, OverlayStack, PayloadSlot, ShellMode,
};
use crate::state::{CommitIntent, MenuData, PendingOp, ViewerDoc};

/// The reactive browser-side half of the bottom-sheet model.
///
/// [`SheetState`] is created once by [`Self::new`] above graph epochs; pointer
/// events only publish transient render motion until a matching release resolves
/// the model's remembered detent.
#[derive(Clone, Copy)]
pub(crate) struct SheetController {
    state: RwSignal<SheetState>,
    geometry: SheetGeometry,
    drag: StoredValue<Option<SheetDrag>>,
    drag_offset_px: RwSignal<Option<f64>>,
}

impl SheetController {
    pub(crate) fn new(mode: ShellMode) -> Self {
        Self {
            state: create_rw_signal(SheetState::new(mode)),
            geometry: SheetGeometry::default(),
            drag: store_value(None),
            drag_offset_px: create_rw_signal(None),
        }
    }

    pub(crate) fn placement(&self) -> InspectorPlacement {
        self.state.get().placement()
    }

    pub(crate) fn placement_untracked(&self) -> InspectorPlacement {
        self.state.get_untracked().placement()
    }

    pub(crate) fn render_metrics(&self) -> Option<SheetRenderMetrics> {
        sheet_render_metrics(&self.geometry, self.placement())
    }

    pub(crate) fn drag_offset_px(&self) -> f64 {
        self.drag_offset_px.get().unwrap_or(0.0)
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.drag_offset_px.get().is_some()
    }

    pub(crate) fn on_mode_change(&self, to: ShellMode) {
        self.cancel_drag();
        self.state.update(|state| {
            state.on_mode_change(to);
        });
    }

    pub(crate) fn cancel_drag(&self) {
        self.drag.set_value(None);
        self.drag_offset_px.set(None);
    }

    pub(crate) fn pointer_down(&self, ev: web_sys::PointerEvent) {
        let Some(detent) = self.placement_untracked().detent() else {
            return;
        };
        let Some(drag) = SheetDrag::new(
            ev.pointer_id(),
            ev.client_y() as f64,
            ev.time_stamp(),
            gestures::viewport_size().1,
            self.geometry.fraction(detent),
        ) else {
            return;
        };
        let admitted = self
            .drag
            .try_update_value(|active| SheetDrag::admit(active, drag))
            .unwrap_or(false);
        if !admitted {
            return;
        }

        if let Some(target) = ev.current_target() {
            if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                let _ = element.set_pointer_capture(ev.pointer_id());
            }
        }
        self.drag_offset_px.set(Some(0.0));
        ev.prevent_default();
    }

    pub(crate) fn pointer_move(&self, ev: web_sys::PointerEvent) {
        self.drag.update_value(|drag| {
            if let Some(frame) = drag.as_mut().and_then(|drag| {
                drag.sample(ev.pointer_id(), ev.client_y() as f64, ev.time_stamp())
            }) {
                self.drag_offset_px.set(Some(frame.translate_y_px));
            }
        });
    }

    pub(crate) fn pointer_up(&self, ev: web_sys::PointerEvent) {
        let drag = self
            .drag
            .try_update_value(|active| SheetDrag::take_matching(active, ev.pointer_id()))
            .flatten();
        let Some(mut drag) = drag else {
            return;
        };

        let frame = drag
            .sample(ev.pointer_id(), ev.client_y() as f64, ev.time_stamp())
            .unwrap_or_else(|| drag.frame());
        self.state.update(|state| {
            state.drag_released(
                &self.geometry,
                frame.released_fraction,
                frame.velocity_fraction_per_second,
            );
        });
        self.drag_offset_px.set(None);
        if let Some(target) = ev.current_target() {
            if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                let _ = element.release_pointer_capture(ev.pointer_id());
            }
        }
    }

    pub(crate) fn pointer_cancel(&self, ev: web_sys::PointerEvent) {
        let cancelled = self
            .drag
            .try_update_value(|active| SheetDrag::cancel_matching(active, ev.pointer_id()))
            .unwrap_or(false);
        if cancelled {
            self.drag_offset_px.set(None);
        }
    }
}

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

thread_local! {
    /// Backs [`is_online`] — a `thread_local`, not a signal, for the same reason
    /// `session::signals::SESSION` is one: `api.rs`'s write guards need a plain
    /// synchronous read that works even from a call site with no reactive
    /// subscription, and wasm is single-threaded so a `thread_local` is a
    /// process-wide holder here.
    static CONNECTIVITY: RefCell<ConnectivityCore> = RefCell::new(ConnectivityCore::new(true));
}

/// Whether the browser last reported the network adapter as up (M2.22a, #241).
///
/// The plain, synchronous read `api.rs`'s `refuse_if_offline()` guard checks —
/// mirrors `session::signals::is_lan()`'s shape exactly. Reads whatever
/// [`install_connectivity_signal`] last wrote; before that function has run
/// (there is no window to seed from, e.g. a non-browser host build) this
/// defaults to `true` — fail *open* on the read itself, since refusing every
/// write because connectivity was never wired up would be a worse failure
/// mode than occasionally letting a write reach the existing network-timeout
/// handling in `api.rs`.
pub fn is_online() -> bool {
    CONNECTIVITY.with(|c| c.borrow().is_online())
}

thread_local! {
    /// The reactive twin of [`CONNECTIVITY`], stored by
    /// [`install_connectivity_signal`] so views far from `App`'s prop chain
    /// (the context menu, the picker, the Activity panel) can subscribe
    /// without threading one more parameter through every layer — the same
    /// distribution shape `session::signals::is_lan()` uses for its
    /// per-session facts, applied to a signal instead of a plain read.
    static ONLINE_SIGNAL: Cell<Option<RwSignal<bool>>> = const { Cell::new(None) };
}

/// The reactive connectivity signal for UI gating (M2.22b, #242).
///
/// What this reflects is `navigator.onLine` — the network *adapter*, which can
/// happily read `true` over a dead SSH tunnel. Everything rendered from this
/// signal (the offline banner, hidden/disabled write controls) is therefore a
/// UX nicety layered on top of the real boundary: `api.rs`'s
/// `refuse_if_offline()` guard, which reads the plain [`is_online`] accessor
/// and refuses the write before it touches the wire. A control this signal
/// failed to hide still cannot write while offline.
///
/// Before [`install_connectivity_signal`] has run there is nothing to return,
/// so this hands back a fresh always-`true` signal — fail *open*, matching
/// [`is_online`]'s own default and for the same reason. In practice `App`
/// installs before any view that calls this mounts, so that branch is a
/// non-browser/startup safety net, not a code path.
pub fn online_signal() -> RwSignal<bool> {
    ONLINE_SIGNAL
        .with(|s| s.get())
        .unwrap_or_else(|| create_rw_signal(true))
}

/// Seed the connectivity read from `navigator.onLine` and keep it current via
/// the window's `online`/`offline` events — the exact listener/cleanup shape
/// of [`install_mode_signal`] above, applied to a boolean instead of a resize.
///
/// Returns a reactive `RwSignal<bool>` for the UI to read (M2.22b); the plain
/// [`is_online`] accessor is what the write guard reads, so a write can be
/// refused even from a context that must not create a reactive subscription.
/// Both are kept in step by every event this function's listeners handle.
pub fn install_connectivity_signal() -> RwSignal<bool> {
    let initial = web_sys::window()
        .map(|w| w.navigator().on_line())
        // No `window` at all only happens off-browser; `true` here matches
        // `is_online`'s own fail-open default for the same reason.
        .unwrap_or(true);
    CONNECTIVITY.with(|c| c.replace(ConnectivityCore::new(initial)));
    let online = create_rw_signal(initial);
    // Published for [`online_signal`]'s far-flung readers (M2.22b, #242).
    ONLINE_SIGNAL.with(|s| s.set(Some(online)));

    if let Some(win) = web_sys::window() {
        let online_for_on = online;
        let cb_online = Closure::<dyn FnMut()>::new(move || {
            CONNECTIVITY.with(|c| c.borrow_mut().set_online(true));
            // `try_set`, not `set`, for the reason `install_mode_signal` uses it:
            // this listener can outlive the scope that created the signal.
            let _ = online_for_on.try_set(true);
        });
        let _ = win.add_event_listener_with_callback("online", cb_online.as_ref().unchecked_ref());

        let online_for_off = online;
        let cb_offline = Closure::<dyn FnMut()>::new(move || {
            CONNECTIVITY.with(|c| c.borrow_mut().set_online(false));
            let _ = online_for_off.try_set(false);
        });
        let _ =
            win.add_event_listener_with_callback("offline", cb_offline.as_ref().unchecked_ref());

        let win2 = win.clone();
        on_cleanup(move || {
            let _ = win2
                .remove_event_listener_with_callback("online", cb_online.as_ref().unchecked_ref());
            let _ = win2.remove_event_listener_with_callback(
                "offline",
                cb_offline.as_ref().unchecked_ref(),
            );
        });
    }

    online
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
    commit_dialog: RwSignal<Option<CommitIntent>>,
    confirm_op: RwSignal<Option<PendingOp>>,
    /// The write-failure notice (#316), rendered by `error_modal_view`.
    error_notice: RwSignal<Option<ErrorNotice>>,
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
            commit_dialog: create_rw_signal(None::<CommitIntent>),
            confirm_op: create_rw_signal(None::<PendingOp>),
            error_notice: create_rw_signal(None::<ErrorNotice>),
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
    pub fn open_commit_dialog(&self, d: CommitIntent) {
        self.present(Overlay::CommitDialog);
        self.commit_dialog.set(Some(d));
    }

    /// Show the branch-operation / undo confirmation modal on `op`.
    pub fn open_confirm(&self, op: PendingOp) {
        self.present(Overlay::Confirm);
        self.confirm_op.set(Some(op));
    }

    /// Show the write-failure notice modal (#316). The caller stamps the
    /// ghost-click guard (`dialogs.open(Dialog::Error)`) right before this,
    /// matching the existing convention for Commit/Confirm.
    pub fn open_error(&self, notice: ErrorNotice) {
        self.present(Overlay::Error);
        self.error_notice.set(Some(notice));
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

    pub fn close_error(&self) {
        self.dismiss(Overlay::Error);
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
    pub fn commit_dialog(&self) -> Option<CommitIntent> {
        self.commit_dialog.get()
    }

    /// An untracked read, for the submit handler that must not subscribe.
    pub fn commit_dialog_untracked(&self) -> Option<CommitIntent> {
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

    /// A tracked read — the error modal re-renders from it.
    pub fn error_notice(&self) -> Option<ErrorNotice> {
        self.error_notice.get()
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
    ///
    /// Asks [`Overlay::teardown`] *which* things to switch off and only performs them.
    /// The mapping used to be a `match o` right here, and this file is
    /// `#[cfg(target_arch = "wasm32")]` — so the decision "Detail owns `detail_id`, and
    /// only the viewer leaves persisted state behind" lived somewhere no host test could
    /// read it. Exhaustiveness caught a missing arm; nothing caught a wrong one. With the
    /// map in `core`, `every_overlay_blanks_a_slot_no_other_overlay_owns` proves no two
    /// overlays claim one signal, and the source census in that module proves this
    /// function still asks rather than re-deriving.
    fn clear_payload(&self, o: Overlay) {
        let teardown = o.teardown();
        if teardown.clears_stored_comparison {
            // The viewer is gone, so there is nothing to come back to. Done HERE
            // rather than in `close_viewer` because this is the one funnel every
            // close path reaches — Esc goes through `dismiss_top`, which never
            // calls `close_viewer` and would otherwise leave a stored comparison
            // that reopens itself on the next load. Which overlays this applies to
            // is `teardown`'s decision, not this function's.
            crate::prefs::clear_comparison();
        }
        match teardown.slot {
            PayloadSlot::Menu => self.menu.set(None),
            PayloadSlot::CommitDialog => self.commit_dialog.set(None),
            PayloadSlot::ConfirmOp => self.confirm_op.set(None),
            PayloadSlot::ErrorNotice => self.error_notice.set(None),
            PayloadSlot::DetailId => self.detail_id.set(None),
            PayloadSlot::ViewerDoc => self.viewer_doc.set(None),
            PayloadSlot::ActivityOpen => self.activity.close(),
        }
    }
}
