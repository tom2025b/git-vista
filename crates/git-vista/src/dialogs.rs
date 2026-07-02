//! The three modal overlays: the commit-message dialog (Issue #33), the
//! branch-op confirmation (Issue #33 follow-up), and the Open-URL clone dialog
//! (Phase 12).
//!
//! All three share the iPad-proven recipe learned the hard way: inline,
//! viewport-sized (100vw/100vh) styles that render reliably on iOS WebKit; a
//! `<textarea>` for any text field, never a void `<input>` (which panics
//! Leptos' CSR `<template>` node-walk on iOS WebKit and stops the whole view
//! mounting); and a backdrop that ignores a dismiss landing within
//! [`DIALOG_GUARD_MS`] of opening, so iOS's synthesized post-tap "ghost click"
//! can't close the modal it just opened.

use leptos::*;

use crate::api::{branch_op_request, clone_request, create_commit_request, rebase_request};
use crate::state::{Overlays, PendingOp, DIALOG_GUARD_MS};

/// Pop a native alert with `msg` (there's always a window in the running SPA).
fn alert(msg: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(msg);
    }
}

/// Resolve a git op's result: bump `reload` so the graph re-reads on success, or
/// surface git's own error text ("Couldn't {what}:\n<git stderr>") on failure.
fn report(result: Result<(), String>, what: &str, reload: RwSignal<u32>) {
    match result {
        Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
        Err(e) => alert(&format!("Couldn't {what}:\n{e}")),
    }
}

/// The commit-message modal (Issue #33). Shown while `commit_dialog` is `Some`;
/// a real overlay with a focused text box, so it prompts reliably where a native
/// `window.prompt()` gets blocked/flashed by the webview. Confirming POSTs the
/// commit and refreshes the graph; cancelling just closes it.
pub fn commit_dialog_view(overlays: Overlays) -> impl IntoView {
    let Overlays { commit_dialog, commit_msg, dialog_opened_at, reload, .. } = overlays;
    let submit_commit = move || {
        let Some(allow_empty) = commit_dialog.get_untracked() else {
            return;
        };
        let message = commit_msg.get_untracked().trim().to_string();
        if message.is_empty() {
            return; // Keep the dialog open; the Commit button is disabled anyway.
        }
        commit_dialog.set(None);
        spawn_local(async move {
            match create_commit_request(&message, allow_empty).await {
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
        commit_dialog.get().map(|allow_empty| {
            let title = if allow_empty { "Create empty commit" } else { "Commit staged changes" };
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

/// The branch-op confirmation modal (Issue #33 follow-up). Reuses the commit
/// modal's iPad-proven inline-styled overlay, minus any text input (so no void
/// `<input>` to trip the WebKit CSR bug). Confirming runs the pending op and
/// refreshes; cancelling or a backdrop tap closes it.
pub fn confirm_modal_view(overlays: Overlays) -> impl IntoView {
    let Overlays { confirm_op, dialog_opened_at, reload, .. } = overlays;
    let run_confirmed = move || {
        let Some(op) = confirm_op.get_untracked() else {
            return;
        };
        confirm_op.set(None);
        // Each arm runs its git op and then either bumps `reload` (re-read the graph)
        // or surfaces git's own error. Two arms are special: Rebase hits a bodyless
        // endpoint (it acts on HEAD, not a named branch), and Delete upgrades git's
        // "not fully merged" refusal into a Force-Delete confirmation rather than a
        // dead-end alert.
        match op {
            PendingOp::Merge { branch, .. } => spawn_local(async move {
                report(branch_op_request("/api/merge", &branch).await, &format!("merge ‘{branch}’"), reload);
            }),
            PendingOp::Push { branch } => spawn_local(async move {
                report(branch_op_request("/api/push", &branch).await, &format!("push ‘{branch}’"), reload);
            }),
            PendingOp::ForceDelete { branch } => spawn_local(async move {
                report(
                    branch_op_request("/api/force-delete-branch", &branch).await,
                    &format!("force-delete ‘{branch}’"),
                    reload,
                );
            }),
            PendingOp::Rebase { .. } => spawn_local(async move {
                report(rebase_request().await, "rebase onto main", reload);
            }),
            PendingOp::Delete { branch, .. } => spawn_local(async move {
                match branch_op_request("/api/delete-branch", &branch).await {
                    Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                    // git's safe `-d` refuses an unmerged branch with "not fully
                    // merged". Rather than dead-end on that error, re-open the modal
                    // offering a force delete (`-D`). Reset the ghost-click guard as
                    // the modal re-opens, exactly as when it's first shown.
                    Err(e) if e.contains("not fully merged") => {
                        dialog_opened_at.set_value(js_sys::Date::now());
                        confirm_op.set(Some(PendingOp::ForceDelete { branch }));
                    }
                    Err(e) => alert(&format!("Couldn't delete ‘{branch}’:\n{e}")),
                }
            }),
        }
    };
    move || {
        confirm_op.get().map(|op| {
            // `enabled` gates the confirm button: a merge into itself or a detached
            // HEAD has no valid target, so the dialog is informational (Cancel only).
            let (title, body, confirm_label, danger, enabled) = match &op {
                PendingOp::Merge { branch, into } => match into {
                    Some(into) if into != branch => (
                        "Merge branch",
                        format!("Merge ‘{branch}’ into ‘{into}’? This updates ‘{into}’ in the working tree."),
                        "Merge",
                        false,
                        true,
                    ),
                    Some(into) => (
                        "Merge branch",
                        format!("‘{into}’ is the branch you're on — there's nothing to merge into itself."),
                        "Merge",
                        false,
                        false,
                    ),
                    None => (
                        "Merge branch",
                        format!("HEAD is detached, so there's no branch to merge ‘{branch}’ into. Check out a branch first."),
                        "Merge",
                        false,
                        false,
                    ),
                },
                PendingOp::Push { branch } => (
                    "Push branch",
                    format!("Push ‘{branch}’ to origin?"),
                    "Push",
                    false,
                    true,
                ),
                PendingOp::Delete { branch, current } => match current {
                    Some(current) if current == branch => (
                        "Delete branch",
                        format!("‘{branch}’ is the branch you're on — check out another branch before deleting it."),
                        "Delete",
                        true,
                        false,
                    ),
                    // A different branch, or detached HEAD: safe to offer the delete.
                    _ => (
                        "Delete branch",
                        format!("Delete branch ‘{branch}’? Only a fully-merged branch can be deleted here."),
                        "Delete",
                        true,
                        true,
                    ),
                },
                // Reached only after a safe delete was refused for "not fully merged"
                // (see `run_confirmed`): offer the override, spelling out the risk.
                PendingOp::ForceDelete { branch } => (
                    "Force delete branch",
                    format!("‘{branch}’ isn't fully merged — force-deleting it discards any commits it holds that aren't on another branch. This can't be undone. Force delete it anyway?"),
                    "Force Delete",
                    true,
                    true,
                ),
                PendingOp::Rebase { current } => match current {
                    Some(branch) => (
                        "Rebase onto main",
                        format!("Rebase ‘{branch}’ onto main? This replays ‘{branch}’’s commits on top of the latest main and rewrites its history."),
                        "Rebase",
                        false,
                        true,
                    ),
                    None => (
                        "Rebase onto main",
                        "HEAD is detached, so there's no branch to rebase. Check out a branch first.".to_string(),
                        "Rebase",
                        false,
                        false,
                    ),
                },
            };
            // The confirm button is muted when disabled, red for a destructive
            // delete, green otherwise.
            let confirm_style = if !enabled {
                "padding:6px 14px; font:inherit; color:var(--muted); \
                 background:#21262d; border:1px solid #30363d; border-radius:6px; \
                 opacity:0.6;"
            } else if danger {
                "padding:6px 14px; font:inherit; color:#fff; \
                 background:#da3633; border:1px solid #f85149; border-radius:6px;"
            } else {
                "padding:6px 14px; font:inherit; color:#fff; \
                 background:#238636; border:1px solid #2ea043; border-radius:6px;"
            };
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if js_sys::Date::now() - dialog_opened_at.get_value() > DIALOG_GUARD_MS {
                            confirm_op.set(None);
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
                        <div style="margin-bottom:14px; line-height:1.4;">{body}</div>
                        <div style="display:flex; gap:8px; justify-content:flex-end;">
                            <button
                                style="padding:6px 14px; font:inherit; color:var(--fg); \
                                       background:#21262d; border:1px solid #30363d; \
                                       border-radius:6px;"
                                on:click=move |_| confirm_op.set(None)
                            >
                                "Cancel"
                            </button>
                            <button
                                style=confirm_style
                                prop:disabled=!enabled
                                on:click=move |_| run_confirmed()
                            >
                                {confirm_label}
                            </button>
                        </div>
                    </div>
                </div>
            }
        })
    }
}

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
