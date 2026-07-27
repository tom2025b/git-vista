//! The "Reset Test Repo" confirmation (iPad-testing follow-up).

use leptos::*;

use crate::api::reset_test_repo_request;
use crate::features::dialogs::core::Dialog;
use crate::features::dialogs::signals::Dialogs;
use crate::features::graph::core::GraphCore;

use super::alert;

/// The "Reset Test Repo" confirmation (iPad-testing follow-up). Owned by `App`
/// like the Open-URL modal — its button lives in the topbar, outside the graph
/// canvas that owns the shared confirm modal — and only ever reachable when the
/// graph said `resettable` (the repo was opted in with `gv --seed`). Same
/// iPad-proven inline-styled overlay and ghost-click guard as the other modals.
/// Confirming POSTs the reset, alerts the server's summary, and reloads.
pub fn reset_repo_view(
    reset_open: RwSignal<bool>,
    dialogs: Dialogs,
    graph: RwSignal<GraphCore>,
) -> impl IntoView {
    let run_reset = move || {
        reset_open.set(false);
        spawn_local(async move {
            match reset_test_repo_request().await {
                // Success and failure both alert — a reset is rare and drastic
                // enough that its outcome should always be said out loud — and
                // both reload: even a failed reset may have moved refs.
                Ok(msg) => {
                    alert(&msg);
                    graph.update(|g| {
                        g.force_bump();
                    });
                }
                Err(e) => {
                    alert(&format!("Couldn't reset the test repo:\n{e}"));
                    graph.update(|g| {
                        g.force_bump();
                    });
                }
            }
        });
    };
    move || {
        reset_open.get().then(|| {
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if dialogs.may_dismiss() {
                            dialogs.close(Dialog::Reset);
                            reset_open.set(false);
                        }
                    }
                >
                    <div
                        style="min-width:300px; max-width:90vw; padding:16px; \
                               background:#161b22; border:1px solid #30363d; \
                               border-radius:10px; color:var(--fg); \
                               box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                        on:click=move |ev| ev.stop_propagation()
                    >
                        <div style="font-weight:600; margin-bottom:12px;">"Reset Test Repo"</div>
                        <div style="margin-bottom:14px; line-height:1.4;">
                            "Restore this repo to its recorded seed state? Every commit, \
                             branch and uncommitted change made since the seed is \
                             DISCARDED — including deleting branches created during \
                             testing. This can't be undone."
                        </div>
                        <div style="display:flex; gap:8px; justify-content:flex-end;">
                            <button
                                style="padding:6px 14px; font:inherit; color:var(--fg); \
                                       background:#21262d; border:1px solid #30363d; \
                                       border-radius:6px;"
                                on:click=move |_| reset_open.set(false)
                            >
                                "Cancel"
                            </button>
                            <button
                                style="padding:6px 14px; font:inherit; color:#fff; \
                                       background:#da3633; border:1px solid #f85149; \
                                       border-radius:6px;"
                                on:click=move |_| run_reset()
                            >
                                "Reset"
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}
