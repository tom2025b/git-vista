//! The working-tree / index items: "Stage Changes", "Unstage Changes",
//! "Select Changes to Stage…/…Unstage…", "Discard Changes…", "Delete
//! Untracked Files…" (M2.17d/#215, M2.18b/#220).

use leptos::*;

use crate::api::{stage_request, unstage_request};
use crate::features::dialogs::core::{Dialog, ErrorNotice};
use crate::features::graph::core::disabled_menu_item_copy;
use crate::features::status::core::{
    deletable_untracked_paths, discardable_tracked_paths, ScopedWorktreeStatus,
};
use crate::icons::GitIcons;
use crate::state::{Features, PendingOp};

/// `(stage_changes, unstage_changes, select_stage, select_unstage,
/// discard_changes, delete_untracked)` — named so [`build_worktree_items`]'s
/// return type isn't a bare six-tuple (clippy's `type_complexity`).
type WorktreeItems = (
    View,
    Option<View>,
    Option<View>,
    Option<View>,
    Option<View>,
    Option<View>,
);

/// Builds a [`WorktreeItems`].
///
/// `is_head`/`is_stub` are `MenuData::is_head`/`MenuData::is_branch`,
/// computed once by the caller since `commit_items` needs the same pair.
///
/// `staged_count` and `live_status` are both *resolved readings* rather than the
/// resources behind them. Each is pinned to the accepted repository frame, and
/// the decision — is this reply current for the live epoch and repository, or a
/// retained answer for one the user has left? — is made once in `menu_view`,
/// where the live frame is, rather than re-derived per item here. Both are read
/// inside the same reactive block that used to call `.get()` on the resources,
/// so the tracking is unchanged; a reply that is loading, failed, or scoped to
/// another frame arrives as `0` and `None`, and the items it would have fed
/// stay absent or disabled.
///
/// Taking the resolved [`ScopedWorktreeStatus`] rather than the resource keeps
/// two decisions unskippable here: no path can come from a reading outside the
/// live frame, and the opaque worktree id carried into the operation is the id
/// that reading was requested with. This function has no live selector to
/// substitute later.
pub(super) fn build_worktree_items(
    features: Features,
    ic: &'static GitIcons,
    is_head: bool,
    is_stub: bool,
    staged_count: usize,
    live_status: Option<ScopedWorktreeStatus>,
) -> WorktreeItems {
    let Features {
        dialogs,
        status,
        shell,
        ..
    } = features;
    // "Stage Changes" (git add -A): move the working-tree changes into the
    // index so they can be committed. Like committing, it acts on the
    // checked-out branch, so it's offered on the HEAD commit and disabled
    // elsewhere (and on a stub) with the reason. Immediate — no dialog —
    // then a status refetch, so the topbar chip flips to "staged" and
    // "Commit Changes" has something to commit.
    //
    // #217: this used to `graph.force_bump()`, the same "everything you
    // have is void" primitive Refresh/checkout/commit use. Staging only
    // moves working-tree changes into the index — no commit, no moved
    // ref, no changed `GraphRow` — so it never had anything to invalidate
    // in the loaded history, and bumping the epoch anyway retired the
    // whole paged aggregate (discarding Print's `history_complete` and
    // forcing a full page-1 reseed) for a no-op reason. `status.refetch()`
    // refreshes exactly the one thing that changed.
    let stage_changes = if is_head {
        let on_stage = move |_| {
            shell.close_menu();
            spawn_local(async move {
                match stage_request().await {
                    Ok(()) => status.refetch(),
                    // #316: the envelope's message in the app's own
                    // modal — never raw JSON in a native alert().
                    Err(e) => {
                        dialogs.open(Dialog::Error);
                        shell.open_error(ErrorNotice {
                            title: "Couldn't stage changes",
                            body: e,
                        });
                    }
                }
            });
        };
        view! {
            <button class="ctx-item" on:click=on_stage>
                <span class="nf ctx-icon">{ic.added}</span>
                "Stage Changes"
            </button>
        }
        .into_view()
    } else {
        let reason = if is_stub {
            "Staging applies to the checked-out branch, not a stub"
        } else {
            "Only available on the current HEAD commit"
        };
        let (aria_label, visible_reason) = disabled_menu_item_copy("Stage Changes", reason);
        view! {
            <button
                class="ctx-item disabled"
                title=reason
                aria-disabled="true"
                aria-label=aria_label
            >
                <span class="nf ctx-icon">{ic.added}</span>
                "Stage Changes"
                <span class="ctx-item-reason">{visible_reason}</span>
            </button>
        }
        .into_view()
    };
    // "Unstage Changes" (git reset HEAD): the exact inverse of "Stage
    // Changes" — the index goes back to HEAD, the working tree keeps
    // every edit. Appears only while something is actually staged
    // (the repository-pinned `/api/status` reading, tracked so the item
    // pops in when that read lands) and only on the HEAD commit, like
    // staging.
    //
    // #217: same reasoning as "Stage Changes" above — an index-only
    // change has no history to invalidate, so this refetches status
    // instead of bumping the graph epoch (which would also have reset
    // Print's `history_complete` for no reason).
    let unstage_changes = (is_head && staged_count > 0).then(|| {
        let on_unstage = move |_| {
            shell.close_menu();
            spawn_local(async move {
                match unstage_request().await {
                    Ok(()) => status.refetch(),
                    // #316: the envelope's message in the app's own
                    // modal — never raw JSON in a native alert().
                    Err(e) => {
                        dialogs.open(Dialog::Error);
                        shell.open_error(ErrorNotice {
                            title: "Couldn't unstage changes",
                            body: e,
                        });
                    }
                }
            });
        };
        view! {
            <button class="ctx-item" on:click=on_unstage>
                // The undo glyph — staging, taken back.
                <span class="nf ctx-icon">{ic.undo}</span>
                "Unstage Changes"
            </button>
        }
        .into_view()
    });
    // "Select Changes to Stage…" / "…Unstage…" (M2.17d, #215): open
    // the finger/keyboard hunk-selection view (`viewer.rs`,
    // `ViewerDoc::Staging`) instead of acting on everything the way
    // "Stage/Unstage Changes" above do. Same HEAD/staged-count gating
    // as their whole-tree counterparts — a selection view over a
    // stub's or a non-HEAD commit's changes has nothing to select
    // from, same reasoning as the plain actions above.
    let select_stage = is_head.then(|| {
        let on = move |_| {
            shell.close_menu();
            shell.open_viewer(crate::state::ViewerDoc::Staging {
                direction: git_vista_protocol::StageDirection::Stage,
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.added}</span>
                "Select Changes to Stage…"
            </button>
        }
        .into_view()
    });
    let select_unstage = (is_head && staged_count > 0).then(|| {
        let on = move |_| {
            shell.close_menu();
            shell.open_viewer(crate::state::ViewerDoc::Staging {
                direction: git_vista_protocol::StageDirection::Unstage,
            });
        };
        view! {
            <button class="ctx-item" on:click=on>
                <span class="nf ctx-icon">{ic.undo}</span>
                "Select Changes to Unstage…"
            </button>
        }
        .into_view()
    });
    // "Discard Changes…" / "Delete Untracked Files…" (M2.18b, #220):
    // the UI half of #219's two typed working-tree operations. Both
    // open the confirm modal rather than acting immediately — that
    // modal is where the paths are listed and where the delete's
    // second deliberate step lives (`features::dialogs::core`).
    //
    // HEAD-gated like the staging items above, and for the same
    // reason: the working tree belongs to the checked-out commit, so
    // offering either from a stub or an older commit would act
    // somewhere other than where the user is pointing.
    //
    // The path lists are built by the host-tested selectors in
    // `features::status::core`, which mirror the server's own
    // classification. Building them here by hand would mean a
    // confirmation the user completes and the server then 409s.
    //
    // `live_status` arrived already resolved against the live frame (#711), and
    // still carries the WorktreeId used for that read (#733). Copy both the
    // derived paths and that captured id into PendingOp. Confirm/dispatch must
    // never resolve the live selection to manufacture a selector.
    let discard_changes = is_head.then(|| {
        let scoped = live_status.as_ref();
        let paths = scoped
            .map(|reading| discardable_tracked_paths(&reading.status))
            .unwrap_or_default();
        let repo = scoped.map(|reading| reading.repo);
        if paths.is_empty() {
            let reason = if live_status.is_none() {
                "Waiting for a working-tree status read"
            } else {
                "No tracked file has uncommitted changes"
            };
            let (aria_label, visible_reason) = disabled_menu_item_copy("Discard Changes…", reason);
            view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.undo}</span>
                    "Discard Changes…"
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view()
        } else {
            let repo = repo.expect("a non-empty path list came from a scoped status read");
            let on = move |_| {
                // Raise the modal *before* `close_menu` disposes this
                // handler's reactive owner — the ordering rule this
                // module's doc comment opens with.
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(PendingOp::DiscardTrackedPaths {
                    repo,
                    paths: paths.clone(),
                });
                shell.close_menu();
            };
            view! {
                <button class="ctx-item" on:click=on>
                    <span class="nf ctx-icon">{ic.undo}</span>
                    "Discard Changes…"
                </button>
            }
            .into_view()
        }
    });
    let delete_untracked = is_head.then(|| {
        let scoped = live_status.as_ref();
        let paths = scoped
            .map(|reading| deletable_untracked_paths(&reading.status))
            .unwrap_or_default();
        let repo = scoped.map(|reading| reading.repo);
        if paths.is_empty() {
            let reason = if live_status.is_none() {
                "Waiting for a working-tree status read"
            } else {
                "No untracked files in the working tree"
            };
            let (aria_label, visible_reason) =
                disabled_menu_item_copy("Delete Untracked Files…", reason);
            view! {
                <button
                    class="ctx-item disabled"
                    title=reason
                    aria-disabled="true"
                    aria-label=aria_label
                >
                    <span class="nf ctx-icon">{ic.deleted}</span>
                    "Delete Untracked Files…"
                    <span class="ctx-item-reason">{visible_reason}</span>
                </button>
            }
            .into_view()
        } else {
            let repo = repo.expect("a non-empty path list came from a scoped status read");
            let on = move |_| {
                dialogs.open(Dialog::Confirm);
                shell.open_confirm(PendingOp::DeleteUntrackedPaths {
                    repo,
                    paths: paths.clone(),
                });
                shell.close_menu();
            };
            view! {
                <button class="ctx-item" on:click=on>
                    <span class="nf ctx-icon">{ic.deleted}</span>
                    "Delete Untracked Files…"
                </button>
            }
            .into_view()
        }
    });
    (
        stage_changes,
        unstage_changes,
        select_stage,
        select_unstage,
        discard_changes,
        delete_untracked,
    )
}
