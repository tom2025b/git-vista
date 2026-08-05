//! The branch-op / undo / working-tree confirmation modal (Issue #33
//! follow-up; M2.18b, #220 added the second ceremony).
//!
//! Two confirmation strengths live here, and the difference is structural
//! rather than cosmetic: a branch operation or a discard is one tap on the
//! confirm button, while deleting untracked files — the one operation in this
//! app with no way back — leaves that button inert until a separate arm
//! control has been pressed. Which ceremony an operation gets, and every word
//! either one shows, is decided in `features::dialogs::core` (pure,
//! host-tested); this file is the part that needs a DOM.

use leptos::*;

use git_vista_core::activity::UndoAction;

use crate::features::dialogs::core::{
    worktree_confirm, ConfirmPrompt, Dialog, WorktreeAction, TOUCH_TARGET_STYLE,
};
use crate::features::graph::core::disabled_menu_item_copy;
use crate::state::{Features, PendingOp};

/// The confirm/cancel button base style, with #65's 44x44 floor.
///
/// Every button in this modal carries it. The floor used to be missing here:
/// the old `padding:6px 14px` on a 13px font lands around 30px tall, under
/// the minimum the rest of the app was brought up to in #65 — and this modal
/// is inline-styled (see `dialogs/mod.rs` for why), so the stylesheet census
/// in `features::a11y::audit` never saw it.
const BUTTON_BASE: &str = "padding:8px 16px; font:inherit; border-radius:6px; ";

/// The branch-op confirmation modal (Issue #33 follow-up). Reuses the commit
/// modal's iPad-proven inline-styled overlay, minus any text input (so no void
/// `<input>` to trip the WebKit CSR bug). Confirming hands the operation to the
/// `operations` feature; cancelling or a backdrop tap closes it.
pub fn confirm_modal_view(features: Features) -> impl IntoView {
    let Features {
        dialogs,
        operations,
        shell,
        ..
    } = features;

    // Confirming used to clear the dialog and then `spawn_local` a future nothing held —
    // so the write existed nowhere between the tap and its reply, and closing a panel
    // mid-flight lost every trace of it (M1.11, #64, acceptance criterion 2). Now the
    // dialog only *raises* the operation; `operations` owns it from here, and it is held
    // above the canvas, so it outlives this modal and the re-read its completion triggers.
    let run_confirmed = move || {
        let Some(op) = shell.confirm_op_untracked() else {
            return;
        };
        shell.close_confirm();
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
            // `open` also disarms the two-tap control, so a re-asked question never
            // inherits an arm the user gave the previous one.
            dialogs.open(Dialog::Confirm);
            shell.open_confirm(next);
        }
    });

    move || {
        shell.confirm_op().map(|op| {
            // Tracked read: the arm control and the confirm button both re-render the
            // moment step one is taken.
            let armed = dialogs.confirm_armed();
            // `enabled` gates the confirm button: a merge into itself or a detached
            // HEAD has no valid target, so the dialog is informational (Cancel only).
            let ConfirmPrompt {
                title,
                body,
                confirm_label,
                danger,
                enabled,
                arm,
                blocked_reason,
            } = match &op {
                PendingOp::Merge { branch, into } => match into {
                    Some(into) if into != branch => ConfirmPrompt::plain(
                        "Merge branch",
                        format!("Merge ‘{branch}’ into ‘{into}’? This updates ‘{into}’ in the working tree."),
                        "Merge",
                        false,
                        true,
                    ),
                    Some(into) => ConfirmPrompt::plain(
                        "Merge branch",
                        format!("‘{into}’ is the branch you're on — there's nothing to merge into itself."),
                        "Merge",
                        false,
                        false,
                    ),
                    None => ConfirmPrompt::plain(
                        "Merge branch",
                        format!("HEAD is detached, so there's no branch to merge ‘{branch}’ into. Check out a branch first."),
                        "Merge",
                        false,
                        false,
                    ),
                },
                PendingOp::Push { branch } => ConfirmPrompt::plain(
                    "Push branch",
                    format!("Push ‘{branch}’ to origin?"),
                    "Push",
                    false,
                    true,
                ),
                PendingOp::Checkout { branch, current } => match current {
                    Some(current) if current == branch => ConfirmPrompt::plain(
                        "Checkout branch",
                        format!("‘{branch}’ is already the branch you're on — nothing to switch."),
                        "Checkout",
                        false,
                        false,
                    ),
                    // A different branch, or detached HEAD (which a checkout re-attaches).
                    _ => ConfirmPrompt::plain(
                        "Checkout branch",
                        format!("Check out ‘{branch}’? This switches the working tree and HEAD to ‘{branch}’."),
                        "Checkout",
                        false,
                        true,
                    ),
                },
                PendingOp::Delete { branch, current } => match current {
                    Some(current) if current == branch => ConfirmPrompt::plain(
                        "Delete branch",
                        format!("‘{branch}’ is the branch you're on — check out another branch before deleting it."),
                        "Delete",
                        true,
                        false,
                    ),
                    // A different branch, or detached HEAD: safe to offer the delete.
                    _ => ConfirmPrompt::plain(
                        "Delete branch",
                        format!("Delete branch ‘{branch}’? Only a fully-merged branch can be deleted here."),
                        "Delete",
                        true,
                        true,
                    ),
                },
                // Reached only after a safe delete was refused for "not fully merged"
                // (see `run_confirmed`): offer the override, spelling out the risk.
                PendingOp::ForceDelete { branch } => ConfirmPrompt::plain(
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
                    // M2.20e (#231): this used to read "git-vista never
                    // force-pushes", which stopped being true the day an
                    // explicit force-with-lease publish existed. The invariant
                    // it was describing is narrower and still holds — an *undo*
                    // never rewrites the remote — so the sentence now says that,
                    // and points at the thing a user would otherwise go looking
                    // for.
                    let warn = if u.warn_pushed {
                        " The discarded state is already pushed: undoing here \
                         changes nothing on origin (an undo never force-pushes), \
                         so the branch will show as behind until it's pushed \
                         again. Rewriting what origin has is a separate, \
                         explicit force-publish."
                    } else {
                        ""
                    };
                    match &u.action {
                        UndoAction::ResetBranch { .. } => ConfirmPrompt::plain(
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
                        UndoAction::RestoreBranch { .. } => ConfirmPrompt::plain(
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
                        UndoAction::RevertCommit { .. } => ConfirmPrompt::plain(
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
                    Some(branch) => ConfirmPrompt::plain(
                        "Rebase branch",
                        format!("Rebase ‘{branch}’ onto {base}? This replays ‘{branch}’’s commits on top of the latest {base} and rewrites its history."),
                        "Rebase",
                        false,
                        true,
                    ),
                    None => ConfirmPrompt::plain(
                        "Rebase branch",
                        "HEAD is detached, so there's no branch to rebase. Check out a branch first.".to_string(),
                        "Rebase",
                        false,
                        false,
                    ),
                },
                // The two working-tree operations (M2.18b, #220). Both prompts —
                // wording, which ceremony, what is enabled — come from the pure
                // core, so the asymmetry between them is decided somewhere a host
                // test can read it rather than inside this wasm-only view.
                PendingOp::DiscardTrackedPaths { paths } => {
                    worktree_confirm(WorktreeAction::DiscardTracked, paths, armed)
                }
                PendingOp::DeleteUntrackedPaths { paths } => {
                    worktree_confirm(WorktreeAction::DeleteUntracked, paths, armed)
                }
            };
            // The confirm button is muted when disabled, red for a destructive
            // delete, green otherwise.
            let confirm_style = if !enabled {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:var(--muted); \
                         background:#21262d; border:1px solid #30363d; opacity:0.6;")
            } else if danger {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; \
                         background:#da3633; border:1px solid #f85149;")
            } else {
                format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}color:#fff; \
                         background:#238636; border:1px solid #2ea043;")
            };
            // #65: a reason conveyed only through `title=` never surfaces on a tap
            // and is never announced, so it goes into the button's `aria-label`
            // *and* onto the screen as its own line. `disabled_menu_item_copy` is
            // the same composition `menu.rs`'s disabled items use — reused rather
            // than restated, so there is one rule for it.
            let (confirm_aria, visible_reason) = match blocked_reason {
                Some(reason) => {
                    let (aria, visible) = disabled_menu_item_copy(confirm_label, reason);
                    (aria, Some(visible))
                }
                None => (confirm_label.to_string(), None),
            };
            // Step one of the two-tap ceremony, for the operation that has one.
            // A `<button>` with `aria-pressed`, not a checkbox: this modal takes
            // no form controls (see the module doc), and the state change has to
            // be announced, not merely visible.
            let arm_control = arm.map(|step| {
                let arm_style = if step.pressed {
                    format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                             margin-bottom:12px; color:#f0f6fc; background:#5a1e1e; \
                             border:1px solid #f85149;")
                } else {
                    format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}width:100%; text-align:left; \
                             margin-bottom:12px; color:var(--fg); background:#21262d; \
                             border:1px solid #30363d;")
                };
                view! {
                    <button
                        style=arm_style
                        aria-pressed=if step.pressed { "true" } else { "false" }
                        on:click=move |_| dialogs.arm_confirm()
                    >
                        {step.label}
                    </button>
                }
            });
            view! {
                <div
                    style="position:fixed; top:0; left:0; width:100vw; height:100vh; \
                           z-index:30; display:flex; align-items:center; \
                           justify-content:center; background:rgba(1,4,9,0.6);"
                    on:click=move |_| {
                        // Ignore the iOS ghost click that fires just after opening.
                        if dialogs.may_dismiss() {
                            dialogs.close(Dialog::Confirm);
                            shell.close_confirm();
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
                        // `pre-wrap`: the working-tree prompts list one path per line,
                        // and every other prompt is a single paragraph either way.
                        <div style="margin-bottom:14px; line-height:1.4; \
                                    white-space:pre-wrap; max-height:50vh; \
                                    overflow-y:auto;">{body}</div>
                        {arm_control}
                        {visible_reason.map(|reason| view! {
                            <div style="margin-bottom:10px; color:var(--muted); \
                                        line-height:1.4;">{reason}</div>
                        })}
                        <div style="display:flex; gap:8px; justify-content:flex-end;">
                            <button
                                style=format!("{BUTTON_BASE}{TOUCH_TARGET_STYLE}\
                                               color:var(--fg); background:#21262d; \
                                               border:1px solid #30363d;")
                                on:click=move |_| shell.close_confirm()
                            >
                                "Cancel"
                            </button>
                            // Two ways to be inert, and which one applies turns
                            // on whether this button carries its own reason.
                            //
                            // A branch arm's reason lives in the body text
                            // (`blocked_reason: None`), so `prop:disabled`
                            // stays exactly as it was — no behaviour change to
                            // anything that predates #220.
                            //
                            // A working-tree arm's reason is folded into
                            // `aria-label`, and a genuinely disabled button
                            // leaves the tab order — which would make that
                            // reason unreachable by the exact user it was
                            // written for (#65's finding, again). Those stay
                            // focusable and are refused in the handler instead.
                            // That guard is also what makes the two-tap
                            // ceremony real rather than decorative: `disabled`
                            // is the browser's to honour, `enabled` is ours.
                            <button
                                style=confirm_style
                                prop:disabled=!enabled && blocked_reason.is_none()
                                aria-disabled=if enabled { "false" } else { "true" }
                                aria-label=confirm_aria
                                on:click=move |_| {
                                    if enabled {
                                        run_confirmed();
                                    }
                                }
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
