//! The commit-message modal (Issue #33).

use leptos::*;

use crate::api::create_commit_request;
use crate::features::dialogs::core::Dialog;
use crate::state::{CommitDialog, Features};

/// The commit-message modal (Issue #33). Shown while `commit_dialog` is `Some`;
/// a real overlay with a focused text box, so it prompts reliably where a native
/// `window.prompt()` gets blocked/flashed by the webview. Confirming POSTs the
/// commit and refreshes the graph; cancelling just closes it.
pub fn commit_dialog_view(features: Features) -> impl IntoView {
    let Features {
        graph,
        dialogs,
        shell,
        ..
    } = features;
    let submit_commit = move || {
        let Some(CommitDialog {
            allow_empty,
            branch,
        }) = shell.commit_dialog_untracked()
        else {
            return;
        };
        let message = dialogs.commit_msg_untracked().trim().to_string();
        if message.is_empty() {
            return; // Keep the dialog open; the Commit button is disabled anyway.
        }
        shell.close_commit_dialog();
        spawn_local(async move {
            match create_commit_request(&message, allow_empty, branch.as_deref()).await {
                Ok(()) => graph.update(|g| {
                    g.force_bump();
                }),
                Err(e) => {
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!("Couldn't create commit:\n{e}"));
                    }
                }
            }
        });
    };
    move || {
        shell.commit_dialog().map(
            |CommitDialog {
                 allow_empty,
                 branch,
             }| {
                // Name the branch a stub-targeted empty commit will land on — it is
                // *not* the checked-out branch, which is what anyone would otherwise
                // assume a commit dialog acts on.
                let title = match branch {
                    Some(b) => format!("Create empty commit on ‘{b}’"),
                    None if allow_empty => "Create empty commit".to_string(),
                    None => "Commit Changes".to_string(),
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
                            if dialogs.may_dismiss() {
                                dialogs.close(Dialog::Commit);
                                shell.close_commit_dialog();
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
                                prop:value=move || dialogs.commit_msg()
                                on:input=move |ev| dialogs.set_commit_msg(event_target_value(&ev))
                            ></textarea>
                            <div style="display:flex; gap:8px; justify-content:flex-end; \
                                        margin-top:14px;">
                                <button
                                    style="padding:6px 14px; font:inherit; color:var(--fg); \
                                           background:#21262d; border:1px solid #30363d; \
                                           border-radius:6px;"
                                    on:click=move |_| shell.close_commit_dialog()
                                >
                                    "Cancel"
                                </button>
                                <button
                                    style="padding:6px 14px; font:inherit; color:#fff; \
                                           background:#238636; border:1px solid #2ea043; \
                                           border-radius:6px;"
                                    prop:disabled=move || dialogs.commit_msg().trim().is_empty()
                                    on:click=move |_| submit_commit()
                                >
                                    "Commit"
                                </button>
                            </div>
                        </div>
                    </div>
                }
            },
        )
    }
}
