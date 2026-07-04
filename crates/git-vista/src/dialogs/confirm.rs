//! The branch-op / undo confirmation modal (Issue #33 follow-up).

use leptos::*;

use git_vista_core::activity::UndoAction;

use crate::api::{branch_op_request, rebase_request, undo_request};
use crate::state::{Overlays, PendingOp, DIALOG_GUARD_MS};

use super::{alert, report};

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
                match branch_op_request("/api/merge", &branch).await {
                    // git's no-op: the branch brought nothing in and HEAD didn't
                    // move. The graph won't visibly change, which reads as a
                    // refresh failure — so say what (didn't) happen. Still bump
                    // `reload`: the repo may have changed under us since the
                    // graph was drawn, and a re-read after any op is cheap.
                    Ok(msg) if msg.starts_with("Already up to date") => {
                        alert(&msg);
                        reload.update(|n| *n = n.wrapping_add(1));
                    }
                    other => report(other, &format!("merge ‘{branch}’"), reload),
                }
            }),
            PendingOp::Push { branch } => spawn_local(async move {
                report(branch_op_request("/api/push", &branch).await, &format!("push ‘{branch}’"), reload);
            }),
            PendingOp::Checkout { branch, .. } => spawn_local(async move {
                match branch_op_request("/api/checkout", &branch).await {
                    // The already-on-it no-op (raced from a stale menu): say what
                    // (didn't) happen, mirroring the merge arm's up-to-date case.
                    Ok(msg) if msg.starts_with("Already on") => {
                        alert(&msg);
                        reload.update(|n| *n = n.wrapping_add(1));
                    }
                    other => report(other, &format!("check out ‘{branch}’"), reload),
                }
            }),
            PendingOp::ForceDelete { branch } => spawn_local(async move {
                report(
                    branch_op_request("/api/force-delete-branch", &branch).await,
                    &format!("force-delete ‘{branch}’"),
                    reload,
                );
            }),
            PendingOp::Rebase { base, .. } => spawn_local(async move {
                match rebase_request().await {
                    // The already-based no-op (raced from a stale menu): say what
                    // (didn't) happen, mirroring the merge arm's up-to-date case.
                    Ok(msg) if msg.starts_with("Already up to date") => {
                        alert(&msg);
                        reload.update(|n| *n = n.wrapping_add(1));
                    }
                    other => report(other, &format!("rebase onto {base}"), reload),
                }
            }),
            // The undo itself (step 5). The server re-checks everything that
            // matters — compare-and-swap on the branch tip, clean-tree guard,
            // revert auto-abort — so failure here surfaces its reason verbatim
            // (e.g. "‘main’ has moved since this undo was offered").
            PendingOp::Undo(u) => spawn_local(async move {
                report(undo_request(&u.action).await, "undo", reload);
            }),
            PendingOp::Delete { branch, .. } => spawn_local(async move {
                match branch_op_request("/api/delete-branch", &branch).await {
                    Ok(_) => reload.update(|n| *n = n.wrapping_add(1)),
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
                PendingOp::Checkout { branch, current } => match current {
                    Some(current) if current == branch => (
                        "Checkout branch",
                        format!("‘{branch}’ is already the branch you're on — nothing to switch."),
                        "Checkout",
                        false,
                        false,
                    ),
                    // A different branch, or detached HEAD (which a checkout re-attaches).
                    _ => (
                        "Checkout branch",
                        format!("Check out ‘{branch}’? This switches the working tree and HEAD to ‘{branch}’."),
                        "Checkout",
                        false,
                        true,
                    ),
                },
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
                // The undo confirmation (step 5). The server-built label already
                // says exactly what will happen ("Undo merge — reset ‘main’ to
                // abc1234"); the body adds what that means for history, and the
                // pushed warning when the discarded state is on the remote.
                PendingOp::Undo(u) => {
                    let warn = if u.warn_pushed {
                        " The discarded state is already pushed: origin keeps it \
                         (git-vista never force-pushes), so the branch will show \
                         as behind until it's pushed again."
                    } else {
                        ""
                    };
                    match &u.action {
                        UndoAction::ResetBranch { .. } => (
                            "Undo — move branch back",
                            format!(
                                "{}? The discarded commits leave the graph but stay \
                                 in the reflog.{warn}",
                                u.label
                            ),
                            "Undo",
                            true,
                            true,
                        ),
                        UndoAction::RestoreBranch { .. } => (
                            "Restore branch",
                            format!(
                                "{}? This re-creates the branch exactly where it \
                                 last pointed — nothing else changes.",
                                u.label
                            ),
                            "Restore",
                            false,
                            true,
                        ),
                        UndoAction::RevertCommit { .. } => (
                            "Revert commit",
                            format!(
                                "{}? This adds a new commit that reverses it — \
                                 history is kept, so it's safe even when pushed.",
                                u.label
                            ),
                            "Revert",
                            false,
                            true,
                        ),
                    }
                }
                PendingOp::Rebase { current, base } => match current {
                    Some(branch) => (
                        "Rebase branch",
                        format!("Rebase ‘{branch}’ onto {base}? This replays ‘{branch}’’s commits on top of the latest {base} and rewrites its history."),
                        "Rebase",
                        false,
                        true,
                    ),
                    None => (
                        "Rebase branch",
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
