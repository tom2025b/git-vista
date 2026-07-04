//! The commit-message modal (Issue #33).

use leptos::*;

use crate::api::create_commit_request;
use crate::state::{CommitDialog, Overlays, DIALOG_GUARD_MS};

/// The commit-message modal (Issue #33). Shown while `commit_dialog` is `Some`;
/// a real overlay with a focused text box, so it prompts reliably where a native
/// `window.prompt()` gets blocked/flashed by the webview. Confirming POSTs the
/// commit and refreshes the graph; cancelling just closes it.
pub fn commit_dialog_view(overlays: Overlays) -> impl IntoView {
    let Overlays { commit_dialog, commit_msg, dialog_opened_at, reload, .. } = overlays;
    let submit_commit = move || {
        let Some(CommitDialog { allow_empty, branch }) = commit_dialog.get_untracked() else {
            return;
        };
        let message = commit_msg.get_untracked().trim().to_string();
        if message.is_empty() {
            return; // Keep the dialog open; the Commit button is disabled anyway.
        }
        commit_dialog.set(None);
        spawn_local(async move {
            match create_commit_request(&message, allow_empty, branch.as_deref()).await {
                Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                Err(e) => {
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!("Couldn't create commit:\n{e}"));
                    }
                }
            }
        });
    };
    move || {
        commit_dialog.get().map(|CommitDialog { allow_empty, branch }| {
            // Name the branch a stub-targeted empty commit will land on — it is
            // *not* the checked-out branch, which is what anyone would otherwise
            // assume a commit dialog acts on.
            let title = match branch {
                Some(b) => format!("Create empty commit on ‘{b}’"),
                None if allow_empty => "Create empty commit".to_string(),
                None => "Commit staged changes".to_string(),
            };
            // The message field is a <textarea>, NOT an <input>: the void <input>
            // element breaks Leptos' CSR <template> node-walk on iOS WebKit (which
            // parses void elements differently than Blink/Gecko), panicking the whole
            // view so the modal never mounts on iPad. A textarea is non-void — and is
            // fine for a commit message. Styles are inline and viewport-sized
            // (100vw/100vh) since that's what proved to render reliably on iOS.
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if js_sys::Date::now() - dialog_opened_at.get_value()
                            > DIALOG_GUARD_MS
                        {
                            commit_dialog.set(None);
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
                        <div style="font-weight:600; margin-bottom:12px;">{title}</div>
                        <textarea
                            style="width:100%; box-sizing:border-box; padding:10px; \
                                   font:inherit; color:var(--fg); background:#0d1117; \
                                   border:1px solid #30363d; border-radius:6px; \
                                   resize:none;"
                            rows="2"
                            placeholder="Commit message"
                            prop:value=move || commit_msg.get()
                            on:input=move |ev| commit_msg.set(event_target_value(&ev))
                        ></textarea>
                        <div style="display:flex; gap:8px; justify-content:flex-end; \
                                    margin-top:14px;">
                            <button
                                style="padding:6px 14px; font:inherit; color:var(--fg); \
                                       background:#21262d; border:1px solid #30363d; \
                                       border-radius:6px;"
                                on:click=move |_| commit_dialog.set(None)
                            >
                                "Cancel"
                            </button>
                            <button
                                style="padding:6px 14px; font:inherit; color:#fff; \
                                       background:#238636; border:1px solid #2ea043; \
                                       border-radius:6px;"
                                prop:disabled=move || commit_msg.get().trim().is_empty()
                                on:click=move |_| submit_commit()
                            >
                                "Commit"
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}
