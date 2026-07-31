//! The persistent hook-policy disclosure banner (M1.13a, #66, ADR 0025).
//!
//! `SECURITY_MODEL.md:236`: *"local mode may allow [repository hooks], but
//! the UI must report that fact."* Not a log line, not a toast — a
//! **persistent** indicator, because the risk it discloses is persistent for
//! as long as the session is `Allow`: every `git commit`/`merge`/etc. this
//! server spawns runs the repository's real hooks, unconditionally, for the
//! whole time the banner is up. A `Restricted` session gets no banner —
//! nothing surprising is happening there (**today** — see the caveat below).
//!
//! Same inline-styled, `styles.css`-free shape as
//! [`crate::update_required::update_required_view`] and
//! [`crate::session::not_connected_view`], deliberately: those two already
//! established the pattern for a top-level, non-`features/shell`-owned
//! notice that needs its own look without touching the shared stylesheet —
//! `#65` was under active design work on `styles.css`/`features/shell/**`
//! when this landed, so reusing an existing escape hatch rather than adding
//! a new one was the safer call. Unlike those two, this banner does **not**
//! block interaction — it's a slim bar, not a full-screen overlay, since the
//! app is fully usable in `Allow` mode; the risk is disclosed, not blocking.
//!
//! **What this banner does not mean.** A `Restricted` session showing no
//! banner does not mean hooks are actually suppressed for it — nothing in
//! `git_cmd.rs`/`git-vista-git` enforces `HookPolicy` yet (see
//! [`git_vista_protocol::HookPolicy`]'s own doc comment and ADR 0025).
//! `Restricted` is a declared value only, correctly disclosed as "not the
//! elevated-risk case," but not yet backed by real suppression. That gap is
//! M1.13b's, not this banner's to close.

use git_vista_protocol::HookPolicy;
use leptos::*;

/// The banner, or nothing.
///
/// `visible` is
/// [`SessionCore::hook_policy_banner_visible`](crate::features::session::core::SessionCore::hook_policy_banner_visible)'s
/// answer; `policy` is the session's own [`HookPolicy`], and it decides the
/// **words**. Both come from the same value on purpose.
///
/// Until #208 this took only `visible` and rendered one fixed sentence for
/// every warning state — so `Blocked` was told hooks "run automatically",
/// which is the exact opposite of the truth, and `Network` was told nothing
/// about the sandbox that was in fact containing it. The predicate tracked the
/// policy while the text did not. Both errors over-warned, so nothing was ever
/// falsely reassured, but a bar that cries wolf on a blocked session is a bar
/// nobody reads on an unsandboxed one.
///
/// The wording itself lives in
/// [`crate::hook_policy_disclosure::for_session`] so it is host-testable —
/// `impl IntoView` is opaque, so any text left in this file can only be
/// checked by eye.
pub fn hook_policy_banner_view(visible: bool, policy: HookPolicy) -> impl IntoView {
    visible.then(move || banner(crate::hook_policy_disclosure::for_session(policy)))
}

fn banner(message: &'static str) -> impl IntoView {
    view! {
        <div
            role="status"
            // padding-top adds the safe-area inset (#65): this bar is flush
            // with the top edge, above the topbar, so under viewport-fit=cover
            // it would otherwise sit inside the notch region.
            style="position:fixed; top:0; left:0; right:0; z-index:900; \
                   display:flex; align-items:center; justify-content:center; \
                   gap:8px; padding:6px 12px; font-size:0.85em; \
                   padding-top:calc(6px + env(safe-area-inset-top, 0px)); \
                   background:#3a2a0a; color:#f0c674; \
                   border-bottom:1px solid #5a4210;"
        >
            <span aria-hidden="true">"\u{26A0}"</span>
            <span>{message}</span>
        </div>
    }
}

// No test module here — the one meaningfully testable decision
// (`hook_policy_banner_visible`) already lives in, and is tested by,
// `features::session::core`. This file is markup only, exercised visually
// like every other wasm32-gated view file in this crate.
