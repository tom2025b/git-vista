//! The persistent offline banner (M2.22b, #242) — wasm only.
//!
//! Mounted by `App` unconditionally; shows its strip whenever the reactive
//! connectivity signal (`features::shell::signals::online_signal`, M2.22a's
//! `navigator.onLine` mirror) reports offline. In flow directly under the topbar, not
//! a `position:fixed` bar like `hook_policy_banner`: that one owns the top
//! edge (safe-area inset and all), and stacking a second fixed bar under it
//! would mean the two coordinating about each other's height. In-flow, this
//! strip simply pushes the content down while it exists and can never overlap
//! anything.
//!
//! `navigator.onLine` can read `true` over a dead SSH tunnel — this banner and
//! the controls M2.22b hides are a UX nicety, not the safety boundary. The
//! boundary is `api.rs`'s `refuse_if_offline()` guard (M2.22a), which refuses
//! every write client-side before it touches the wire, backed by the wording
//! in `git_vista_core::net::offline_refusal_text`.
//!
//! The wording lives in [`git_vista_core::net::offline_banner_text`] so it is
//! host-testable — the same split `hook_policy_banner` makes with
//! `hook_policy_disclosure`, and for the same reason: text left in this file
//! could only be checked by eye.

use leptos::*;

/// The offline strip: a permanently-mounted `role="status"` live region whose
/// *content* toggles with the signal.
///
/// Permanently mounted on purpose: a live region only reliably announces
/// content that *changes inside* a region the accessibility tree already
/// knows about — iOS VoiceOver in particular often stays silent for a
/// `role="status"` element inserted into the DOM pre-populated. Here the
/// wrapper exists from mount, empty and zero-height, and the styled strip
/// appears inside it when connectivity drops — which is exactly the mutation
/// VoiceOver announces. The transition is the whole point of this banner
/// (the write controls vanish in the same tick), so it must be heard, not
/// only seen. (`hook_policy_banner` renders on-demand instead, defensibly:
/// it appears once at session start, not on a mid-session flip.)
///
/// Not interactive, so it needs no touch target and cannot trap focus when
/// its content empties.
pub fn offline_banner_view(online: RwSignal<bool>) -> impl IntoView {
    view! {
        <div role="status">
            {move || {
                (!online.get()).then(|| view! {
                    <div style="display:flex; align-items:center; \
                                justify-content:center; gap:8px; \
                                padding:6px 12px; font-size:0.85em; \
                                background:#3a2a0a; color:#f0c674; \
                                border-bottom:1px solid #5a4210;">
                        <span aria-hidden="true">"\u{26A0}"</span>
                        <span>{git_vista_core::net::offline_banner_text()}</span>
                    </div>
                })
            }}
        </div>
    }
}
