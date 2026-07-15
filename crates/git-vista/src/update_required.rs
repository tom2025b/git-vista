//! The "Update Required" screen (M1.02, #102).
//!
//! A full-screen, non-dismissable overlay shown when the client's protocol
//! version falls outside the window the server accepts (learned from
//! `GET /api/protocol`). Rather than silently misread responses from a server it
//! can't safely understand, the frontend stops and asks the user to reload to
//! pick up the current build — the exact case of a cached PWA that has drifted
//! from a redeployed server. Same iPad-proven inline-styled overlay as the
//! modals in [`crate::dialogs`], at a higher z-index so it covers everything.

use leptos::*;

use git_vista_protocol::{Compatibility, ProtocolInfo, PROTOCOL_VERSION};

/// The blocking "Update Required" overlay. `server` is the negotiation payload
/// the app fetched; `verdict` says which way the client is out of range, so the
/// wording can be precise ("out of date" vs "ahead of the server").
pub fn update_required_view(server: ProtocolInfo, verdict: Compatibility) -> impl IntoView {
    let reload = move |_| {
        if let Some(win) = web_sys::window() {
            let _ = win.location().reload();
        }
    };
    let detail = match verdict {
        Compatibility::ClientTooNew => {
            "This tab is running a newer version than the server — the server was \
             probably restarted on an older build. Reload to match it."
        }
        // ClientTooOld — and, defensively, Compatible, which never reaches here.
        _ => {
            "This tab is running an out-of-date version of git-vista. Reload to \
             pick up the current build."
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
                    "Update Required"
                </div>
                <div style="margin-bottom:16px; line-height:1.5;">{detail}</div>
                <button
                    style="padding:8px 20px; font:inherit; color:#fff; \
                           background:#238636; border:1px solid #2ea043; \
                           border-radius:6px;"
                    on:click=reload
                >
                    "Reload"
                </button>
                <div style="margin-top:16px; font-size:0.8em; opacity:0.6;">
                    {format!(
                        "client protocol v{PROTOCOL_VERSION} · server accepts v{}\u{2013}v{} · server {}",
                        server.min_client_protocol,
                        server.max_client_protocol,
                        server.server_version,
                    )}
                </div>
            </div>
        </div>
    }
}
