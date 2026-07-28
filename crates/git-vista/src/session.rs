//! Frontend half of the loopback session flow (M1.04, #57).
//!
//! On load the app must hold a session before any `/api/*` call will succeed (the
//! server answers `401` otherwise). This module:
//!
//!   1. reads the one-time bootstrap token from the `#s=<token>` URL fragment the
//!      `gv` launcher hands the operator, exchanges it for a session cookie
//!      (`POST /api/session`), and **strips the fragment from the address bar**
//!      immediately so the secret doesn't linger in history or a shared screen;
//!   2. failing that (no fragment, or a spent/expired token), checks whether the
//!      browser already holds a live session (`GET /api/session`);
//!   3. records the session's CSRF token in [`crate::api`] so writes can echo it.
//!
//! When neither path yields a session, [`not_connected_view`] blocks the app with
//! a screen telling the operator to open the setup link `gv` printed.

use leptos::*;

use crate::api::{get_session, post_session};
use crate::features::session::core::SessionEvent;
use crate::features::session::signals as session_state;

/// Establish the session on load. Returns `Ok(true)` when authenticated (cookie is
/// live and the CSRF token is recorded), `Ok(false)` when the app needs the
/// operator to open a fresh setup link, or `Err` when the server is unreachable
/// (the normal load-error path then applies, not the sign-in screen).
pub async fn establish_session() -> Result<bool, String> {
    // A bootstrap token in the fragment wins: exchange it for a fresh session.
    if let Some(token) = take_bootstrap_token() {
        if let Ok(info) = post_session(&token).await {
            // One event, so the credential and the transport fact can never be
            // recorded half-way (M1.11, #64). `Established` is always accepted.
            let _ = session_state::apply(SessionEvent::Established {
                csrf: info.csrf.clone(),
                via_lan: info.via_lan,
                hook_policy: info.hook_policy,
            });
            return Ok(info.authenticated);
        }
        // The token was invalid or already spent — fall through to see whether a
        // usable session cookie is nonetheless present.
    }
    let info = get_session().await?;
    let _ = session_state::apply(SessionEvent::Established {
        csrf: info.csrf.clone(),
        via_lan: info.via_lan,
        hook_policy: info.hook_policy,
    });
    Ok(info.authenticated)
}

/// Read the one-time token from the `#s=<token>` URL fragment, if present, and
/// strip the whole fragment from the visible URL via `history.replaceState` so the
/// secret is gone from the address bar and back/forward history the instant it's
/// read. Returns `None` when there's no `s=` fragment or it's empty.
fn take_bootstrap_token() -> Option<String> {
    let win = web_sys::window()?;
    let loc = win.location();
    let hash = loc.hash().ok()?;
    let fragment = hash.strip_prefix('#').unwrap_or(&hash);
    let token = fragment.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "s" && !value.is_empty()).then(|| value.to_string())
    })?;
    // Remove the fragment from the address bar (keep path + query) without adding a
    // history entry. Best-effort: on failure the token is still exchanged; it just
    // lingers in the URL until the next navigation.
    if let Ok(history) = win.history() {
        let path = loc.pathname().unwrap_or_default();
        let search = loc.search().unwrap_or_default();
        let clean = format!("{path}{search}");
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&clean));
    }
    Some(token)
}

/// The blocking "not connected" screen, shown when the app has no session and no
/// bootstrap token to establish one. Mirrors the M1.02 "Update Required" overlay:
/// a full-screen inline-styled panel (iPad-proven) at a high z-index. There's no
/// in-page way to authenticate — the operator must open the setup link `gv`
/// printed — so the panel explains that and offers a Reload once they have.
pub fn not_connected_view() -> impl IntoView {
    let reload = move |_| {
        if let Some(win) = web_sys::window() {
            let _ = win.location().reload();
        }
    };
    view! {
        <div style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                    z-index:1000; display:flex; align-items:center; \
                    justify-content:center; background:rgba(1,4,9,0.85);">
            <div style="min-width:300px; max-width:90vw; padding:24px; \
                        background:#161b22; border:1px solid #30363d; \
                        border-radius:10px; color:var(--fg); text-align:center; \
                        box-shadow:0 12px 32px rgba(0,0,0,0.6);">
                <div style="font-weight:600; font-size:1.2em; margin-bottom:12px;">
                    "Connect to git-vista"
                </div>
                <div style="margin-bottom:16px; line-height:1.5;">
                    "This device isn't signed in yet. On the machine running git-vista, \
                     open the setup link "
                    <code>"gv"</code>
                    " printed in your terminal (or run "
                    <code>"gv --token"</code>
                    " to reprint it), then open it here."
                </div>
                <button
                    style="padding:8px 20px; font:inherit; color:#fff; \
                           background:#238636; border:1px solid #2ea043; \
                           border-radius:6px;"
                    on:click=reload
                >
                    "Reload"
                </button>
                <div style="margin-top:16px; font-size:0.8em; opacity:0.6;">
                    "The setup link carries a one-time token that signs this browser \
                     in. It never leaves your machine."
                </div>
            </div>
        </div>
    }
}
