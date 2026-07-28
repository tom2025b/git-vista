//! The single holder of [`SessionCore`] — wasm only (M1.11, #64).
//!
//! One `thread_local` replaces the three that lived in `api.rs`. That is the point: the
//! facts are per-tab and never rendered from a reactive read today (`via_lan` and the CSRF
//! token are both fixed while establishing the session, before the first view exists), so
//! what was actually wrong was not the storage — it was that three independent cells could
//! disagree and no rule could be stated across them. Now every mutation goes through the
//! tested core, and there is exactly one owner (acceptance criterion 1).
//!
//! wasm is single-threaded, so a `thread_local` is a process-wide holder here.

use std::cell::RefCell;

use git_vista_protocol::RepoMode;

use crate::features::core_traits::{Applied, FeatureCore};
use crate::features::session::core::{SessionCore, SessionEvent, SessionRejection};

thread_local! {
    static SESSION: RefCell<SessionCore> = RefCell::new(SessionCore::default());
}

/// Put an event through the core. Rejections are the caller's to handle.
pub fn apply(ev: SessionEvent) -> Result<Applied, SessionRejection> {
    SESSION.with(|s| s.borrow_mut().apply(ev))
}

/// The session's CSRF token, cloned for the header builder that needs to own it.
pub fn csrf_token() -> Option<String> {
    SESSION.with(|s| s.borrow().csrf_token().map(str::to_owned))
}

/// Whether the current session came through the LAN listener (ADR 0005).
pub fn is_lan() -> bool {
    SESSION.with(|s| s.borrow().is_lan())
}

/// Whether repository writes are refused up front (ADR 0007's client-side chokepoint).
pub fn refuses_writes() -> bool {
    SESSION.with(|s| s.borrow().refuses_writes())
}

/// The mode the open repository is in, or `None` before the first graph lands.
pub fn ui_mode() -> Option<RepoMode> {
    SESSION.with(|s| s.borrow().ui_mode())
}

/// Whether the persistent hook-policy banner (M1.13a, #66, ADR 0025) should
/// show for the current session.
pub fn hook_policy_banner_visible() -> bool {
    SESSION.with(|s| s.borrow().hook_policy_banner_visible())
}
