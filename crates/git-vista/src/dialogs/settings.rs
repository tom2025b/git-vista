//! The settings surface (M13.03, #584): one field to save the GitHub token
//! the credential helper offers for private-repository operations.
//!
//! Every decision lives in `features::settings::core` (framework-free,
//! host-tested); this file is the DOM shell around it — fetch the status on
//! open, render `core::status_line`'s sentence, gate the Save button with
//! `core::save_enabled`, and apply `core::input_after_save`'s answer once a
//! save settles. Same iPad-proven inline-styled overlay and ghost-click
//! guard as every other dialog here.

use leptos::*;

use git_vista_protocol::TokenStatus;

use crate::api::{set_token_request, token_status_request};
use crate::features::dialogs::core::Dialog;
use crate::features::dialogs::signals::Dialogs;
use crate::features::settings::core::{input_after_save, save_enabled, status_line};

/// `settings_open` is owned by `App`, the same way `reset_open`/`open_url`
/// are — the topbar button that opens this dialog lives outside the graph
/// canvas.
pub fn settings_view(settings_open: RwSignal<bool>, dialogs: Dialogs) -> impl IntoView {
    let token_input = create_rw_signal(String::new());
    // Starts unconfigured rather than `None` so `status_line` is the SOLE
    // source of that sentence — no second, duplicated "not configured"
    // string living in this view for the pre-fetch instant.
    let status = create_rw_signal(TokenStatus {
        configured: false,
        masked: None,
        source: None,
    });
    let error = create_rw_signal(None::<String>);
    let saving = create_rw_signal(false);
    let loading = create_rw_signal(false);
    let storage_notice = create_rw_signal(None::<String>);
    let confirm_clear = create_rw_signal(false);

    // Fetch the current status on every open — never cached across opens, so
    // a token saved from a second tab (or a keyring entry that has since
    // become unreadable) is reflected rather than shown stale.
    create_effect(move |_| {
        if settings_open.get() {
            confirm_clear.set(false);
            storage_notice.set(None);
            error.set(None);
            loading.set(true);
            spawn_local(async move {
                match token_status_request().await {
                    Ok(s) => status.set(s),
                    Err(e) => error.set(Some(e)),
                }
                loading.set(false);
            });
        }
    });

    let submit_save = move || {
        let token = token_input.get_untracked();
        if !save_enabled(&token, saving.get_untracked()) {
            return;
        }
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            let result = set_token_request(&token).await;
            token_input.set(input_after_save(&result, &token));
            match result {
                Ok(s) => status.set(s),
                Err(e) => error.set(Some(e)),
            }
            saving.set(false);
        });
    };

    let close = move || {
        // A save in flight is short (one keyring write, no network round
        // trip to a remote) — unlike the clone dialog's multi-minute pin,
        // there is no real state to strand by letting this close under it,
        // so no `may_dismiss`-style busy guard is needed here.
        dialogs.close(Dialog::Settings);
        settings_open.set(false);
    };

    move || {
        settings_open.get().then(|| view! {
        <div
            style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                   z-index:910; display:flex; align-items:center; \
                   justify-content:center; background:rgba(1,4,9,0.6);"
            on:click=move |_| {
                if dialogs.may_dismiss() {
                    close();
                }
            }
        >
            <div
                style="min-width:280px; max-width:90vw; max-height:85dvh; overflow:auto; padding:16px; \
                       background:#161b22; border:1px solid #30363d; \
                       border-radius:10px; color:var(--fg); \
                       box-shadow:0 12px 32px rgba(0,0,0,0.6);"
                on:click=move |ev| ev.stop_propagation()
            >
                <div style="font-weight:600; margin-bottom:4px;">"GitHub token"</div>
                <div style="font-size:0.85em; color:var(--muted, #8b949e); margin-bottom:12px;">
                    {move || {
                        if loading.get() {
                            "Checking…".to_string()
                        } else {
                            status_line(&status.get())
                        }
                    }}
                </div>
                // A `<textarea>`, never a void `<input>` — see this module's
                // own doc header on why (it panics Leptos' CSR node-walk on
                // iOS WebKit). `-webkit-text-security` visually masks the
                // typed characters the way `type="password"` would; it is
                // WebKit/Blink-only and degrades to plain visible text on
                // Firefox, which is a graceful fallback rather than a
                // failure — the value is still never round-tripped back to
                // the browser regardless of how it looks while being typed.
                <textarea
                    style="width:100%; box-sizing:border-box; padding:10px; \
                           font:inherit; color:var(--fg); background:#0d1117; \
                           border:1px solid #30363d; border-radius:6px; \
                           resize:none; -webkit-text-security:disc;"
                    rows="1"
                    placeholder="Paste a token with repo scope to save it"
                    prop:value=move || token_input.get()
                    on:input=move |ev| token_input.set(event_target_value(&ev))
                ></textarea>
                {move || error.get().map(|e| view! {
                    <div style="font-size:0.85em; color:#f85149; margin-top:8px;">{e}</div>
                })}
                <section aria-label="Browser data" style="margin-top:16px; max-width:440px;">
                    <h2 style="font-size:1em;">"Browser data"</h2>
                    <p>"Repository responses and private diffs are not saved for offline use. Static files use the browser's revalidated HTTP cache; clear that cache in browser settings."</p>
                    <p>"Export includes only icon and checkpoint-folding preferences. Clear removes saved preferences, commit drafts, comparison choices, and operation tracking from this browser, then reloads. It does not cancel server operations or erase server credentials. Other open tabs can save data again."</p>
                    <button style="min-height:44px; margin:4px;" on:click=move |_| {
                        storage_notice.set(Some(match crate::features::settings::browser_storage::export_saved_preferences() {
                            Ok(()) => "Preference download requested.".to_string(),
                            Err(e) => e,
                        }));
                    }>"Export preferences"</button>
                    <button style="min-height:44px; margin:4px;" on:click=move |_| confirm_clear.set(true)>"Clear saved browser data…"</button>
                    {move || confirm_clear.get().then(|| view! {
                        <div>
                            <p>"Delete saved drafts and preferences? Close other Git-Vista tabs first. Reload needs a working connection."</p>
                            <button style="min-height:44px; margin:4px;" on:click=move |_| {
                                if let Err(e) = crate::features::settings::browser_storage::clear_saved_data() {
                                    storage_notice.set(Some(e));
                                }
                            }>"Delete saved data and reload"</button>
                            <button style="min-height:44px; margin:4px;" on:click=move |_| confirm_clear.set(false)>"Keep saved data"</button>
                        </div>
                    })}
                    <p role="status">{move || storage_notice.get().unwrap_or_default()}</p>
                </section>
                <div style="display:flex; gap:8px; justify-content:flex-end; margin-top:14px;">
                    <button
                        style="padding:6px 14px; font:inherit; color:var(--fg); \
                               background:#21262d; border:1px solid #30363d; \
                               border-radius:6px;"
                        on:click=move |_| close()
                    >
                        "Close"
                    </button>
                    <button
                        style="padding:6px 14px; font:inherit; color:#fff; \
                               background:#238636; border:1px solid #2ea043; \
                               border-radius:6px;"
                        prop:disabled=move || !save_enabled(&token_input.get(), saving.get())
                        on:click=move |_| submit_save()
                    >
                        {move || if saving.get() { "Saving…" } else { "Save" }}
                    </button>
                </div>
            </div>
        </div>
    })
    }
}
