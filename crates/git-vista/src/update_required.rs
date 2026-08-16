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
use wasm_bindgen::JsCast;
use web_sys::Element;

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
    // #360: pointer-blocking and focus-blocking are different guarantees, and
    // this overlay previously had only the first. The `position: fixed` sheet
    // below covers the app against clicks, but the app's controls stay in the
    // DOM *after* this element, so Tab walked straight out of the overlay into
    // the topbar — and a user who can Tab to "Refresh" and press Enter has
    // bypassed, through the keyboard, the exact decision this screen exists to
    // enforce: stop talking to a server whose responses cannot be safely parsed.
    //
    // That gap lands hardest on the people most likely to meet this screen. This
    // is an iPad-first app; a Magic Keyboard user or a VoiceOver user navigates
    // by focus, and for them the barrier simply was not there.
    //
    // TWO mechanisms, deliberately, because they fail differently:
    //
    //   1. `inert` on every sibling — removes the app from the tab order AND
    //      from the accessibility tree, so a screen reader cannot reach it
    //      either. This is the real fix. Chrome 102+ / Safari 15.5+.
    //   2. A Tab keydown backstop on the overlay — holds the line on any engine
    //      where `inert` is unsupported or silently ignored, where mechanism 1
    //      degrades to nothing at all with no error.
    let overlay: NodeRef<html::Div> = create_node_ref();
    // `create_effect` rather than `on_load`: the effect re-runs when the ref is
    // populated, which is the reliable signal that the element is in the
    // document. `on_load` did not fire here at all — measured, not assumed.
    create_effect(move |_| {
        let Some(el) = overlay.get() else { return };
        let Some(parent) = el.parent_element() else {
            return;
        };
        let kids = parent.children();
        for i in 0..kids.length() {
            let Some(child) = kids.item(i) else { continue };
            // Skip the overlay itself; inerting it would block the one control
            // the user needs.
            if child.is_same_node(Some(el.as_ref())) {
                continue;
            }
            let _ = child.set_attribute("inert", "");
        }
        // Move focus in, so a keyboard user starts inside the overlay rather
        // than wherever they happened to be when it appeared.
        if let Some(btn) = el
            .query_selector("button")
            .ok()
            .flatten()
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = btn.focus();
        }
    });
    // Restore on unmount. The overlay is removed when a reload lands on a
    // compatible server, and leaving the app permanently inert would be a worse
    // bug than the one being fixed.
    on_cleanup(move || {
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            if let Ok(nodes) = doc.query_selector_all("[inert]") {
                for i in 0..nodes.length() {
                    if let Some(n) = nodes.item(i).and_then(|n| n.dyn_into::<Element>().ok()) {
                        let _ = n.remove_attribute("inert");
                    }
                }
            }
        }
    });
    // Mechanism 2: Tab cannot leave. There is exactly one focusable control in
    // here, so the trap is simply "keep it" rather than a first/last cycle.
    let trap = move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Tab" {
            return;
        }
        ev.prevent_default();
        if let Some(el) = overlay.get() {
            if let Some(btn) = el
                .query_selector("button")
                .ok()
                .flatten()
                .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = btn.focus();
            }
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
        <div
            node_ref=overlay
            // Focusable so the trap has somewhere to land, and so the overlay
            // itself can receive the keydown; -1 keeps it out of the tab order.
            tabindex="-1"
            role="alertdialog"
            aria-modal="true"
            aria-label="Update Required"
            on:keydown=trap
            style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                   z-index:1000; display:flex; align-items:center; \
                   justify-content:center; background:rgba(1,4,9,0.85);"
        >
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
