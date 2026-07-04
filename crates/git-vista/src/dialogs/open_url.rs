//! The "Open URL" clone modal (Phase 12).

use leptos::*;

use crate::api::clone_request;
use crate::state::DIALOG_GUARD_MS;

/// The "Open URL" modal (Phase 12): clone a public repo and view it read-only.
/// Same iPad-proven inline-styled overlay as the commit modal, and a `<textarea>`
/// (NOT a void `<input>`, which panics the Leptos CSR node-walk on iOS WebKit)
/// for the URL field. `cloning` disables the button while git works so a slow
/// clone can't be fired twice; `open_opened_at` guards the backdrop against the
/// iOS ghost-click, same trick as the commit modal.
pub fn open_url_view(
    open_url: RwSignal<bool>,
    clone_url: RwSignal<String>,
    cloning: RwSignal<bool>,
    open_opened_at: StoredValue<f64>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let submit_clone = move || {
        let url = clone_url.get_untracked().trim().to_string();
        if url.is_empty() || cloning.get_untracked() {
            return;
        }
        cloning.set(true);
        spawn_local(async move {
            match clone_request(&url).await {
                Ok(()) => {
                    cloning.set(false);
                    open_url.set(false);
                    clone_url.set(String::new());
                    // Re-read via the shared fetch counter so the cloned graph loads.
                    reload.update(|n| *n = n.wrapping_add(1));
                }
                Err(e) => {
                    cloning.set(false);
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!("Couldn't clone:\n{e}"));
                    }
                }
            }
        });
    };
    move || open_url.get().then(|| view! {
        <div
            style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                   z-index:30; display:flex; align-items:center; \
                   justify-content:center; background:rgba(1,4,9,0.6);"
            on:click=move |_| {
                if js_sys::Date::now() - open_opened_at.get_value() > DIALOG_GUARD_MS {
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
                    "Public https:// URLs only. Cloned repos are read-only."
                </div>
                <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:14px;">
                    <button
                        style="padding:6px 14px; font:inherit; color:var(--fg); \
                               background:#21262d; border:1px solid #30363d; \
                               border-radius:6px;"
                        on:click=move |_| open_url.set(false)
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
