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

use leptos::*;

/// The banner, or nothing. `visible` is
/// [`SessionCore::hook_policy_banner_visible`](crate::features::session::core::SessionCore::hook_policy_banner_visible)'s
/// answer — the actual `Allow`-vs-`Restricted` decision is tested there, on
/// the host, since `impl IntoView` is an opaque return type a host test
/// can't inspect. This function only turns "yes" into markup.
pub fn hook_policy_banner_view(visible: bool) -> impl IntoView {
    visible.then(banner)
}

fn banner() -> impl IntoView {
    view! {
        <div
            role="status"
            style="position:fixed; top:0; left:0; right:0; z-index:900; \
                   display:flex; align-items:center; justify-content:center; \
                   gap:8px; padding:6px 12px; font-size:0.85em; \
                   background:#3a2a0a; color:#f0c674; \
                   border-bottom:1px solid #5a4210;"
        >
            <span aria-hidden="true">"\u{26A0}"</span>
            <span>
                "Repository hooks run automatically for this session. "
                "A malicious repository's hooks execute with your permissions."
            </span>
        </div>
    }
}

// No test module here — the one meaningfully testable decision
// (`hook_policy_banner_visible`) already lives in, and is tested by,
// `features::session::core`. This file is markup only, exercised visually
// like every other wasm32-gated view file in this crate.
