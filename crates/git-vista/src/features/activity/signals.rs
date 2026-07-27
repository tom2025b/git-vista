//! The reactive wrapper over [`ActivityCore`] — wasm only (M1.11, #64).
//!
//! An `RwSignal`, not a `StoredValue`: unlike the dialogs guard, the panel's visibility is
//! rendered from (the panel itself, the topbar button's state, and the shared status
//! Resource's key all read it), so it has to be tracked.

use leptos::{create_rw_signal, RwSignal, SignalGet, SignalUpdate, SignalWithUntracked};

use crate::features::activity::core::ActivityCore;

/// The Activity panel's visibility, with a named owner.
#[derive(Clone, Copy)]
pub struct Activity {
    core: RwSignal<ActivityCore>,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            core: create_rw_signal(ActivityCore::default()),
        }
    }

    /// A tracked read — the panel, the shared status Resource's key and the topbar all
    /// re-evaluate from it.
    pub fn is_open(&self) -> bool {
        self.core.get().is_open()
    }

    /// An untracked read, for event handlers that must not subscribe.
    pub fn is_open_untracked(&self) -> bool {
        self.core.with_untracked(ActivityCore::is_open)
    }

    pub fn open(&self) {
        self.core.update(ActivityCore::open);
    }

    pub fn close(&self) {
        self.core.update(ActivityCore::close);
    }

    /// The topbar button: the same control opens and closes.
    pub fn toggle(&self) {
        self.core.update(ActivityCore::toggle);
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}
