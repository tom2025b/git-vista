//! The "Open URL" clone modal (Phase 12).

use leptos::*;

use git_vista_protocol::RepositoryDescriptor;

use crate::api::clone_request;
use crate::features::dialogs::core::{
    clone_dialog_may_dismiss, clone_settlement, CloneSettlement, Dialog,
};
use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::GraphCore;

/// The "Open URL" modal (Phase 12): clone a public repo and view it read-only.
/// Same iPad-proven inline-styled overlay as the commit modal, and a `<textarea>`
/// (NOT a void `<input>`, which panics the Leptos CSR node-walk on iOS WebKit)
/// for the URL field. `cloning` disables the button while git works so a slow
/// clone can't be fired twice; the shared `dialogs` guard protects the backdrop from the
/// iOS ghost-click, same trick as the commit modal. Unlike the other `dialogs/*`
/// modals (z-index 30), this one is also reachable from inside the open picker
/// (ADR 0006's "Clone URL…" button, which doesn't close the picker) — its
/// z-index must beat the picker's 900 or the picker intercepts every click.
pub fn open_url_view(
    open_url: RwSignal<bool>,
    clone_url: RwSignal<String>,
    cloning: RwSignal<bool>,
    dialogs: Dialogs,
    graph: RwSignal<GraphCore>,
    mode_for: RwSignal<Option<RepositoryDescriptor>>,
) -> impl IntoView {
    let submit_clone = move || {
        let url = clone_url.get_untracked().trim().to_string();
        if url.is_empty() || cloning.get_untracked() {
            return;
        }
        cloning.set(true);
        spawn_local(async move {
            // The settlement rules live host-tested in `dialogs/core.rs` (#260);
            // exhaustive destructuring (no `..`) so a new rule refuses to
            // compile until this view applies it. Epoch bump happens on BOTH
            // arms: a timed-out response does not mean the clone failed, and
            // the server may already be pointing at it (`set_current` runs
            // before the reply) — bumping makes a completed-but-lost clone
            // appear instead of staying silently absent.
            let CloneSettlement {
                clear_busy,
                close_dialog,
                clear_url,
                bump_epoch,
                mode_screen_for,
                alert,
            } = clone_settlement(clone_request(&url).await);
            if clear_busy {
                cloning.set(false);
            }
            if close_dialog {
                open_url.set(false);
            }
            if clear_url {
                clone_url.set(String::new());
            }
            if bump_epoch {
                // The server opened the clone look-only; the reload shows it,
                // and the mode screen asks Visualize/Active (ADR 0008).
                graph.update(|g| {
                    g.force_bump();
                });
            }
            if let Some(descriptor) = mode_screen_for {
                mode_for.set(Some(descriptor));
            }
            if let Some(msg) = alert {
                if let Some(w) = web_sys::window() {
                    let _ = w.alert_with_message(&msg);
                }
            }
        });
    };
    move || {
        open_url.get().then(|| view! {
        <div
            style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                   z-index:910; display:flex; align-items:center; \
                   justify-content:center; background:rgba(1,4,9,0.6);"
            on:click=move |_| {
                // A clone in flight pins the dialog open (#260): dismissing it
                // wouldn't cancel the request, it would only make the app look
                // idle while a clone is still running — the "acted like it
                // worked" half of that bug. Composes with the ghost-click
                // guard rather than replacing it. Worst-case pin is
                // 2×CLONE_TIMEOUT_MS (~19 min, hung tunnel + blind retry) —
                // an accepted trade for a single-operator tool; the
                // hide-don't-cancel + settlement-toast alternative is noted
                // in #263 if this ever grates.
                if clone_dialog_may_dismiss(cloning.get_untracked(), dialogs.may_dismiss()) {
                    dialogs.close(Dialog::OpenUrl);
                    open_url.set(false);
                }
            }
        >
            <div
                style="min-width:320px; max-width:90vw; padding:16px; \
                       background:#161b22; border:1px solid #30363d; \
                       border-radius:10px; color:var(--fg); \
                       box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                on:click=move |ev| ev.stop_propagation()
            >
                <div style="font-weight:600; margin-bottom:12px;">"Open a repository by URL"</div>
                <textarea
                    style="width:100%; box-sizing:border-box; padding:10px; \
                           font:inherit; color:var(--fg); background:#0d1117; \
                           border:1px solid #30363d; border-radius:6px; \
                           resize:none;"
                    rows="2"
                    placeholder="https://github.com/owner/repo.git"
                    prop:value=move || clone_url.get()
                    on:input=move |ev| clone_url.set(event_target_value(&ev))
                ></textarea>
                <div style="font-size:0.85em; color:var(--muted, #8b949e); margin-top:8px;">
                    "Public https:// URLs only. Clones persist until you delete them from the picker."
                </div>
                <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:14px;">
                    <button
                        style="padding:6px 14px; font:inherit; color:var(--fg); \
                               background:#21262d; border:1px solid #30363d; \
                               border-radius:6px;"
                        // Same #260 pin as the backdrop: no dismissal while a
                        // clone is in flight. Disabled is the visible signal;
                        // the handler guard backs it in case a click lands
                        // between render and state change. `true` for the
                        // ghost-click leg: explicit buttons skip that guard
                        // (it protects backdrops from synthesized taps), but
                        // the pin itself lives in one host-tested place.
                        prop:disabled=move || cloning.get()
                        on:click=move |_| {
                            if clone_dialog_may_dismiss(cloning.get_untracked(), true) {
                                open_url.set(false);
                            }
                        }
                    >
                        "Cancel"
                    </button>
                    <button
                        style="padding:6px 14px; font:inherit; color:#fff; \
                               background:#238636; border:1px solid #2ea043; \
                               border-radius:6px;"
                        prop:disabled=move || cloning.get() || clone_url.get().trim().is_empty()
                        on:click=move |_| submit_clone()
                    >
                        {move || if cloning.get() { "Cloning…" } else { "Open" }}
                    </button>
                </div>
            </div>
        </div>
    })
    }
}
