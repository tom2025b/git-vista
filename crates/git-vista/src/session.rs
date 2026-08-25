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
use crate::bootstrap_fragment::token_in_fragment;
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

/// Re-check whether a session cookie is usable, **without** re-attempting the
/// bootstrap-token exchange (#218) — the retry half of [`establish_session`].
///
/// Two reasons this is not just `establish_session` called again:
///
/// * **The token is single-use and already spent.** `take_bootstrap_token`
///   consumes it from the URL fragment on the first attempt, and the server
///   replaces it the moment one is redeemed. A retry re-POSTing it can only
///   fail; the cookie it may already have set is the thing worth looking for.
/// * **`POST /api/session` is rate-limited on the LAN listener and `GET` is
///   not** (`handlers::session::create_session` calls `SignInLimiter::check`;
///   `session_status` does not). Retrying the POST would spend a real
///   sign-in attempt per try — five per minute is the whole budget — so a
///   flaky-network user could lock themselves out of sign-in precisely when
///   the retry is meant to be helping them. A GET costs nothing.
pub async fn recheck_session() -> Result<bool, String> {
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
    let token = token_in_fragment(&hash)?.to_string();
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

/// Re-run sign-in when a bootstrap token arrives in the address bar of a tab
/// that is **already open** (#392, ADR 0073).
///
/// Pasting a fresh `gv` link over the URL of a live tab edits only the
/// fragment, which is a same-document navigation: nothing reloads, and
/// [`establish_session`] has already run and will not run again. The token sits
/// visibly in the URL and the app never signs in. Opening the same link in a
/// new tab works, and a manual refresh appears to "fix" it — which is what
/// makes the defect confusing rather than merely annoying. It is also the exact
/// motion a server restart forces, because a restart rotates the token.
///
/// # Why this reloads rather than re-bootstrapping in place
///
/// A reload re-runs the *whole* of startup, which is the only path that has
/// ever been reasoned about on this security boundary. Doing it in place would
/// mean re-resolving a session while the app is mounted, and this module's
/// documented posture (see [`recheck_session`], and
/// `SessionCore`'s per-tab facts) is that `via_lan`, the CSRF token and the
/// hook policy are **fixed once `establish_session` resolves**. Consumers rely
/// on that: `api`'s CSRF token is a `thread_local`, and the hook-policy banner
/// reads session state non-reactively precisely because it cannot change. A
/// second, in-flight `Established` event would leave every one of them holding
/// values from a session that no longer exists — a half-swapped identity, which
/// is a worse thing to have on a sign-in path than a page reload.
///
/// Redeeming a token does not disturb the old session: `SessionManager::
/// exchange` rotates the bootstrap token and *inserts* a new session, so
/// nothing that was already signed in is signed out by this.
///
/// # Why this cannot loop
///
/// `take_bootstrap_token` strips the fragment with `history.replaceState`,
/// which does **not** fire `hashchange` — so the reload's own cleanup is
/// silent. Nothing else in the crate writes `location.hash`, so the only source
/// of an event here is a human pasting a URL. A fragment carrying no usable
/// token returns early and the live tab is left alone; that negative is the
/// load-bearing half, since a reload is destructive to a signed-in tab's state.
///
/// One stated dependency: repeated pastes of the *same* link only work because
/// that `replaceState` succeeds. If it fails (it is best-effort, by that
/// function's own comment) the fragment lingers, and re-pasting an identical
/// URL is a no-op navigation the browser never reports.
pub fn install_token_paste_reload() {
    use wasm_bindgen::{closure::Closure, JsCast};

    let Some(win) = web_sys::window() else { return };

    let target = win.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let Ok(hash) = target.location().hash() else {
            return;
        };
        if token_in_fragment(&hash).is_none() {
            return;
        }
        let _ = target.location().reload();
    });
    let _ = win.add_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref());

    let win2 = win.clone();
    on_cleanup(move || {
        let _ = win2.remove_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref());
    });
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
