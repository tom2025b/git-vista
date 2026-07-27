//! The single holder of [`DialogsCore`], plus the viewer overlay's handle — wasm only
//! (M1.11, #64).
//!
//! One [`Dialogs`] replaces the three `StoredValue<f64>` guard clocks that lived in
//! `app/canvas.rs` and `app/mod.rs`. A `StoredValue`, not an `RwSignal`, on purpose: the
//! guard is bookkeeping consulted inside event handlers and nothing renders from it, which
//! is exactly what the three clocks it replaces were. Backing it with a signal would add a
//! reactive write to eleven click handlers that never had one.
//!
//! Created once in `App` rather than per canvas mount. The old `dialog_opened_at` was
//! created inside `graph_canvas`, so every epoch bump rebuilt it back to the `0.0`
//! never-opened sentinel; hoisting it means a rebuild while a modal is up no longer
//! silently drops that modal's guard.

use leptos::{store_value, RwSignal, SignalGet, SignalSet, SignalWithUntracked, StoredValue};

use crate::features::dialogs::core::{Dialog, DialogsCore};
use crate::state::ViewerDoc;

/// The app's one ghost-click guard.
#[derive(Clone, Copy)]
pub struct Dialogs {
    core: StoredValue<DialogsCore>,
}

impl Dialogs {
    pub fn new() -> Self {
        Self {
            core: store_value(DialogsCore::default()),
        }
    }

    /// Record that `d` is opening now, and start its guard window.
    ///
    /// Call this immediately *before* setting the modal's own signal, and — where the
    /// opener closes the context menu in the same handler — before `menu.set(None)`:
    /// closing the menu disposes the handler's reactive owner, after which further writes
    /// are unreliable. That ordering rule is unchanged from the eleven inlined
    /// `set_value(js_sys::Date::now())` calls this replaces.
    pub fn open(&self, d: Dialog) {
        self.core.update_value(|c| c.open(d, js_sys::Date::now()));
    }

    /// Record that `d` closed. A no-op if some other dialog has since replaced it.
    ///
    /// Only the backdrop-dismiss paths call this today. A modal also closes via its own
    /// Cancel/Confirm button and via Esc (`gestures.rs`), and neither routes through here,
    /// so the core's record of *which* dialog is up can lag reality. That is why this type
    /// deliberately exposes no `open_dialog()` reader: the only thing it is authoritative
    /// about is the guard window, which depends on the stamp alone. Task 8's overlay stack
    /// is what makes open/close single-pathed, and only then is the record worth reading.
    pub fn close(&self, d: Dialog) {
        self.core.update_value(|c| c.close(d));
    }

    /// Whether a backdrop dismiss arriving right now should be honoured, or is iOS's
    /// synthesized post-tap ghost click landing on the modal it just opened.
    ///
    /// Named for what the call site is deciding; delegates to
    /// [`DialogsCore::may_confirm`], which is where the arithmetic is tested.
    pub fn may_dismiss(&self) -> bool {
        self.core.with_value(|c| c.may_confirm(js_sys::Date::now()))
    }
}

impl Default for Dialogs {
    fn default() -> Self {
        Self::new()
    }
}

/// The full-screen viewer's document.
///
/// Not a [`Dialog`]: the viewer has no backdrop dismiss and so has never consulted the
/// guard (see [`Dialog`]'s doc comment). It lives in this module because it is the other
/// overlay the detail panel opens, and `detail.rs` reaching into a raw
/// `RwSignal<Option<ViewerDoc>>` was one of the cross-feature writes M1.11 removes.
#[derive(Clone, Copy)]
pub struct Viewer {
    doc: RwSignal<Option<ViewerDoc>>,
}

impl Viewer {
    pub fn from_signal(doc: RwSignal<Option<ViewerDoc>>) -> Self {
        Self { doc }
    }

    /// Show `doc` full-screen, over whatever panel it was opened from.
    pub fn open(&self, doc: ViewerDoc) {
        self.doc.set(Some(doc));
    }

    pub fn close(&self) {
        self.doc.set(None);
    }

    /// A tracked read — the viewer's own view re-renders from it.
    pub fn doc(&self) -> Option<ViewerDoc> {
        self.doc.get()
    }

    pub fn is_open(&self) -> bool {
        self.doc.with_untracked(Option::is_some)
    }
}
