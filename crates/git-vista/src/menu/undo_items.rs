//! The undo section (step 5): one item per action `/api/undoables` returned
//! for this commit — reset-style undos when its result is still a branch
//! tip, a restore when it's a deleted branch's lost tip, a revert for any
//! non-merge commit.

use leptos::*;

use git_vista_core::activity::{UndoAction, Undoable};

use crate::features::dialogs::core::Dialog;
use crate::icons::GitIcons;
use crate::state::{Features, PendingOp};

/// The tracked read means the menu re-renders when the fetch lands; until
/// then (or with nothing to offer) the section simply isn't there. Each item
/// opens the shared confirm modal — it is raised BEFORE the menu closes (the
/// reactive-owner ordering rule `menu.rs`'s module doc opens with).
pub(super) fn build_undo_items(
    features: Features,
    ic: &'static GitIcons,
    undoables: Resource<(Option<String>, u64), Vec<Undoable>>,
) -> View {
    let Features { dialogs, shell, .. } = features;
    undoables
        .get()
        .unwrap_or_default()
        .into_iter()
        .map(|u| {
            // A reset discards commits from the graph — red like the
            // delete item; restore/revert only add, so they stay plain.
            let class = match u.action {
                UndoAction::ResetBranch { .. } => "ctx-item danger",
                _ => "ctx-item",
            };
            let label = u.label.clone();
            let on = move |_| {
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(PendingOp::Undo(u.clone()));
                shell.close_menu();
            };
            view! {
                <button class=class on:click=on>
                    <span class="nf ctx-icon">{ic.undo}</span>
                    {label}
                </button>
            }
        })
        .collect_view()
}
