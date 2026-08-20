//! The per-commit context menu (Issue #18) — a plain HTML pop-up positioned at
//! the click, rendered outside the SVG so it never pans/zooms or gets clipped.
//!
//! Each item leads with a glyph (icons.rs) matching its action, and every write
//! item is suppressed on a read-only clone (Phase 12). A recurring, load-bearing
//! ordering rule runs through the handlers: a signal write must happen *before*
//! `shell.close_menu()`, because closing the menu synchronously disposes the
//! handler's own reactive owner, after which a further signal write is
//! unreliable. The "Commit …" and merge/push/delete items all follow it.
//!
//! **Disabled items are `<button>`s that are never `disabled`.** Every item
//! this menu greys out carries a reason built by
//! [`disabled_menu_item_copy`], and that reason exists for the keyboard and
//! screen-reader user #65 was about. Two things have to be true for it to
//! actually reach them, and a `<span>` gets both wrong:
//!
//! 1. `aria-label` and `aria-disabled` are only honoured on an element whose
//!    role supports them. A bare `<span>` is `role="generic"`, so both
//!    attributes are dropped on the floor — the accessible name reverts to the
//!    element's text and nothing announces the item as unavailable.
//! 2. A `<span>` is not focusable, so Tab walks straight past it. Its enabled
//!    siblings are `<button>`s and *are* tab stops, which means the item a
//!    keyboard user most needs an explanation for is the one item they can
//!    never land on.
//!
//! So these render as `<button>` with `aria-disabled="true"` and **no**
//! `prop:disabled` — a genuinely disabled button leaves the tab order and
//! takes its own explanation with it. The button has no `on:click`, so it is
//! inert by construction rather than by the browser's grace. This is the same
//! reasoning `dialogs/confirm.rs` writes out for the confirm button that
//! carries a `blocked_reason`, and `features::a11y::audit`'s
//! `every_disabled_context_menu_item_is_focusable` holds the line over this
//! file's bytes. `styles.css` already dresses both forms
//! (`.ctx-item.disabled` and `.ctx-item.disabled:focus-visible`), so nothing
//! there changes.
//!
//! The menu's items are grouped by concern into `menu/`'s child modules —
//! `view_items` (always-visible reads), `create_items` (branch/tag creation),
//! `commit_items`, `worktree_items`, `branch_items`, `tag_items`,
//! `remote_items` (rebase/fetch/pull) and `undo_items` — each a `pub(super)`
//! builder `menu_view` below calls in the same order the original single
//! function built these sections in. This file keeps the two public entry
//! points, the menu's four live resources, and the final assembly.

use leptos::*;

use crate::features::graph::collapse::WipRun;
use crate::features::graph::core::disabled_menu_item_copy;
use crate::features::shell::signals::{self as shell_state, Shell};
use crate::geometry::menu_placement;
use crate::gestures::viewport_size;
use crate::icons::icon_set;
use crate::state::{Features, MenuData, Settings};

use crate::api::{fetch_rebase_status, fetch_status, fetch_undoables, fetch_worktree_status};

mod branch_items;
mod commit_items;
mod create_items;
mod remote_items;
mod tag_items;
mod undo_items;
mod view_items;
mod worktree_items;

/// Open this menu on `commit`, for an entry point that knows only the commit and a
/// header — not the richer context the graph's own dots carry (M1.11, #64).
///
/// The Activity panel's feed rows used to hand-build a `MenuData` literal themselves,
/// which meant every field added here for the graph's menu had to be mirrored by hand at
/// a call site in a different feature. The degraded fields are degraded for the same
/// reason the inline version left them so: this entry point carries neither the
/// pushed-commit set nor the target's local branches, and a GitHub link that 404s is
/// worse than a disabled item.
pub fn open_for_commit(shell: Shell, commit: String, header: String, x: f64, y: f64) {
    shell.open_menu(MenuData {
        wip_run: None,
        commit,
        header,
        x,
        y,
        github_url: None,
        github_label: "Open on GitHub",
        create_label: "Create branch from this commit…",
        is_head: false,
        branches: Vec::new(),
        tags: Vec::new(),
        is_branch: false,
        repo_url: None,
        remote_web_url: None,
    });
}

/// The context menu overlay (Issue #18): a plain HTML pop-up positioned at the
/// click, rendered outside the SVG so it never pans/zooms and isn't clipped.
/// `read_only` (Phase 12) hides every write action on a cloned repo.
pub fn menu_view(
    features: Features,
    settings: Settings,
    read_only: bool,
    on_fold_wip: Callback<WipRun>,
) -> impl IntoView {
    // `dialogs`/`operations`/`status` are read via `features` itself inside
    // the section builders below (`menu/commit_items.rs` and siblings), which
    // each destructure only the fields they need — so this binding only
    // pulls out what the resource setup and the final assembly use directly.
    let Features { graph, shell, .. } = features;
    let nerd_icons = settings.nerd_icons;
    // The undo actions for the menu's commit (step 5), fetched the moment the
    // menu opens — computed live server-side, so the section reflects the repo
    // *now*, not the possibly-stale graph. Keyed on (commit, reload) so
    // reopening on the same commit reuses the answer until something changes;
    // closed menu → no fetch. Arrives async: the menu renders immediately and
    // grows an undo section when (and only when) actions exist. Errors are
    // deliberately swallowed — a menu that can't offer undo is still a menu.
    // Not fetched for a branch stub: its `commit` is the anchor commit the
    // empty branch merely points at, so the anchor's undo actions ("reset
    // ‘main’ …") belong to other branches, not the one that was tapped.
    let undoables = create_local_resource(
        move || {
            (
                shell.menu().filter(|m| !m.is_branch).map(|m| m.commit),
                graph.get().epoch(),
            )
        },
        |(commit, _)| async move {
            match commit {
                Some(c) => fetch_undoables(&c).await.unwrap_or_default(),
                None => Vec::new(),
            }
        },
    );
    // Whether "Rebase onto main" would do anything (step: menu gating) — fetched
    // live like `undoables`, and keyed the same way, so the item can be disabled
    // with the reason ("already based on origin/main", detached HEAD, no main)
    // instead of offering a rebase that no-ops. `None` (still loading, or the
    // fetch failed) leaves the item enabled — the server no-ops safely anyway.
    let rebase_status = create_local_resource(
        move || {
            (
                shell.menu().filter(|m| !m.is_branch).is_some(),
                graph.get().epoch(),
            )
        },
        |(open, _)| async move {
            if open {
                fetch_rebase_status().await.ok()
            } else {
                None
            }
        },
    );
    // How many files are currently staged — fetched live when the menu opens
    // on the HEAD commit (the only place staging items appear), keyed like
    // `rebase_status`. Drives the "Unstage Changes" item: it appears only
    // while something is actually staged, so the menu reflects the repo *now*,
    // not the possibly-stale graph. Fetch failure => 0 => the item is absent.
    let staged_count = create_local_resource(
        move || {
            (
                shell.menu().is_some_and(|m| m.is_head && !m.is_branch),
                graph.get().epoch(),
            )
        },
        |(open, _)| async move {
            if open {
                fetch_status().await.map(|s| s.staged.len()).unwrap_or(0)
            } else {
                0
            }
        },
    );
    // The per-path working-tree status (`GET /api/status/v2`, M2.18b/#220) —
    // fetched when the menu opens on the HEAD commit, keyed exactly like
    // `staged_count`. The v1 read above cannot serve this: the discard/delete
    // confirmations must name the exact paths, and must classify each one the
    // same way the server's own `verify_path_states` will (tracked-dirty vs
    // untracked), which only the v2 per-entry shape carries.
    //
    // A failed or still-in-flight read resolves to `None`, and both items then
    // render *disabled with the reason* rather than vanishing — an item that
    // silently disappears while a status probe is slow reads as "this repo
    // can't do that", which would be a lie.
    let worktree = create_local_resource(
        move || {
            (
                shell.menu().is_some_and(|m| m.is_head && !m.is_branch),
                graph.get().epoch(),
            )
        },
        |(open, _)| async move {
            if open {
                fetch_worktree_status().await.ok()
            } else {
                None
            }
        },
    );
    move || {
        shell.menu().map(|m| {
            // Tracked read: the menu lives inside the overlay wrapper's reactive block,
            // so it re-renders live if the icon style is toggled while open.
            let ic = icon_set(nerd_icons.get());
            let is_head = m.is_head;
            let is_stub = m.is_branch;

            let (open_github, details_item, diff_item) =
                view_items::build_view_items(shell, ic, &m);
            let (create_branch_item, create_tag_item) =
                create_items::build_create_items(features, ic, &m);
            let (commit_changes, commit_empty, amend_item) =
                commit_items::build_commit_items(features, ic, &m);
            let (
                stage_changes,
                unstage_changes,
                select_stage,
                select_unstage,
                discard_changes,
                delete_untracked,
            ) = worktree_items::build_worktree_items(
                features,
                ic,
                is_head,
                is_stub,
                staged_count,
                worktree,
            );
            let branch_items = branch_items::build_branch_items(features, ic, &m, rebase_status);
            let tag_items = tag_items::build_tag_items(features, ic, &m);
            let (rebase_item, fetch_item, pull_item) =
                remote_items::build_remote_items(features, ic, &m, rebase_status);
            let undo_items = undo_items::build_undo_items(features, ic, undoables);

            // On a read-only clone (Phase 12) the menu is just the header + the
            // GitHub link: no branch/commit/merge/push/delete/undo. Otherwise
            // show the full set of write actions.
            //
            // M2.22b (#242): while the device reports offline, the write set is
            // gated the same way — one conditional at the section's chokepoint,
            // not eleven per-item checks — with a single disabled row below
            // naming why, so the menu doesn't silently shrink. The tracked
            // `online_signal` read means a connectivity flip re-renders an
            // OPEN menu (`shell.menu()` is already tracked by this closure).
            // `navigator.onLine` can read true over a dead tunnel — this
            // gating is a UX nicety; `api.rs`'s `refuse_if_offline()` guard
            // (M2.22a) is what actually prevents the write.
            let online = shell_state::online_signal().get();
            let write_items = (!read_only && online).then(|| {
                view! {
                    {undo_items}
                    {create_branch_item}
                    {create_tag_item}
                    {stage_changes}
                    {unstage_changes}
                    {select_stage}
                    {select_unstage}
                    {discard_changes}
                    {delete_untracked}
                    {commit_changes}
                    {commit_empty}
                    {amend_item}
                    {branch_items}
                    {tag_items}
                    {rebase_item}
                    {fetch_item}
                    {pull_item}
                }
            });
            // The one disabled row standing in for the whole write set while
            // offline. Same attribution rule as `offline_refusal_text`: this
            // speaks for the device's adapter, never for the server.
            let offline_notice = (!read_only && !online).then(|| {
                const REASON: &str = "This device reports it is offline";
                let (aria_label, visible_reason) = disabled_menu_item_copy("Write actions", REASON);
                view! {
                    <button
                        class="ctx-item disabled"
                        title=REASON
                        aria-disabled="true"
                        aria-label=aria_label
                    >
                        <span class="nf ctx-icon">{ic.commit}</span>
                        "Write actions"
                        <span class="ctx-item-reason">{visible_reason}</span>
                    </button>
                }
            });
            // Clamp the menu inside the *visual* viewport (iPad fix): a tap in
            // the lower half flips it above the finger, and its max-height is
            // the room actually available — anything past that scrolls
            // (.ctx-menu's overflow-y) instead of hanging offscreen where no
            // finger can reach it.
            // "Fold these N checkpoints" (#374 follow-up). Present only for a
            // commit inside a run the user opened, and deliberately FIRST: it is
            // the reason a reader taps a checkpoint dot at all, and the topbar
            // toggle is the only alternative, which folds the entire graph.
            let fold_wip_item = match m.wip_run {
                Some(run) => {
                    let on_fold = move |_| {
                        shell.close_menu();
                        on_fold_wip.call(run);
                    };
                    view! {
                        <button class="ctx-item" on:click=on_fold>
                            <span class="nf ctx-icon">{ic.commit}</span>
                            {format!("Fold these {} checkpoints", run.count)}
                        </button>
                    }
                    .into_view()
                }
                None => ().into_view(),
            };
            let (vw, vh) = viewport_size();
            let placement = menu_placement(m.x, m.y, vw, vh);
            view! {
                <div class="ctx-menu" style=placement.style()>
                    // Header glyph matches what the header names: a branch for a
                    // stub, a commit hash for a dot.
                    <div class="ctx-menu-header">
                        <span class="nf ctx-icon">
                            {if m.is_branch { ic.branch } else { ic.commit }}
                        </span>
                        {m.header.clone()}
                    </div>
                    {fold_wip_item}
                    {details_item}
                    {diff_item}
                    {open_github}
                    {write_items}
                    {offline_notice}
                </div>
            }
        })
    }
}
