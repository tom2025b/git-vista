//! The tag operations (M2.21d, #238): one "Delete tag" item per local tag
//! living at this target — the same one-badge-one-item shape `branch_items`
//! uses, but delete only.

use leptos::*;

use crate::features::core_traits::RequestTarget;
use crate::features::dialogs::core::Dialog;
use crate::features::operations::core::PendingIntent;
use crate::icons::GitIcons;
use crate::state::{Features, MenuData, PendingOp};

/// A tag carries no merge/push/checkout target the way a branch does, and
/// "Create tag" is offered once per menu by `create_items` rather than once
/// per existing tag. Unlike the branch delete item, there is no live "is this
/// the checked-out branch?" pre-check to await — a tag has no "checked out"
/// concept — so this handler stays synchronous and, per `menu.rs`'s module
/// doc ordering rule, writes `dialogs`/`shell.open_confirm` *before*
/// `shell.close_menu()` rather than after (contrast `branch_items`'s
/// `delete_item`, whose writes happen inside a `spawn_local` continuation
/// that runs after the synchronous handler — and hence after `close_menu` —
/// has already returned).
pub(super) fn build_tag_items(features: Features, ic: &'static GitIcons, m: &MenuData) -> View {
    let Features {
        dialogs,
        operations,
        shell,
        ..
    } = features;
    m.tags
        .iter()
        .map(|t| {
            let t = t.clone();
            let tag = t.clone();
            let on = move |_| {
                let tag = tag.clone();
                let seq = operations.next_seq();
                let key = operations.request_key(RequestTarget::Tag(tag.clone()));
                let intent = PendingIntent {
                    seq,
                    key,
                    kind: PendingOp::DeleteLocalTag { tag },
                };
                if !operations.admit_intent(&intent) {
                    return;
                }
                // Start the ghost-click guard when the modal opens —
                // before closing the menu, per the ordering note above.
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(intent.kind);
                shell.close_menu();
            };
            view! {
                <button class="ctx-item danger" on:click=on>
                    // The diff-removed glyph, inheriting the item's
                    // red — same choice `delete_item` makes above.
                    <span class="nf ctx-icon">{ic.deleted}</span>
                    {format!("Delete tag ‘{t}’")}
                </button>
            }
        })
        .collect_view()
}
