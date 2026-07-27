//! The branch-op / undo confirmation modal (Issue #33 follow-up).

use leptos::*;

use git_vista_core::activity::UndoAction;

use crate::features::dialogs::core::Dialog;
use crate::state::{Overlays, PendingOp};

/// The branch-op confirmation modal (Issue #33 follow-up). Reuses the commit
/// modal's iPad-proven inline-styled overlay, minus any text input (so no void
/// `<input>` to trip the WebKit CSR bug). Confirming hands the operation to the
/// `operations` feature; cancelling or a backdrop tap closes it.
pub fn confirm_modal_view(overlays: Overlays) -> impl IntoView {
    let Overlays {
        confirm_op,
        dialogs,
        operations,
        ..
    } = overlays;

    // Confirming used to clear the dialog and then `spawn_local` a future nothing held —
    // so the write existed nowhere between the tap and its reply, and closing a panel
    // mid-flight lost every trace of it (M1.11, #64, acceptance criterion 2). Now the
    // dialog only *raises* the operation; `operations` owns it from here, and it is held
    // above the canvas, so it outlives this modal and the re-read its completion triggers.
    let run_confirmed = move || {
        let Some(op) = confirm_op.get_untracked() else {
            return;
        };
        confirm_op.set(None);
        operations.dispatch(op);
    };

    // git's safe `branch -d` refuses an unmerged branch with "not fully merged"; rather
    // than dead-end on that, the modal re-opens offering `-D`. The *rule* now lives in
    // the operations core (`escalation`, host-tested); this effect is only the part that
    // needs a dialog. `take_escalation` consumes the entry, so the offer cannot repeat.
    create_effect(move |_| {
        // Subscribe to the registry so this runs whenever an operation settles.
        operations.core().with(|c| c.recent().count());
        if let Some(next) = operations.take_escalation() {
            // Restamp the ghost-click guard, exactly as when the modal is first shown:
            // the modal never visually closes, but it is now asking a different question.
            dialogs.open(Dialog::Confirm);
            confirm_op.set(Some(next));
        }
    });

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
                        if dialogs.may_dismiss() {
                            dialogs.close(Dialog::Confirm);
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
