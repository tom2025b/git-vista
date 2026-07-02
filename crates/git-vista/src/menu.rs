//! The per-commit context menu (Issue #18) — a plain HTML pop-up positioned at
//! the click, rendered outside the SVG so it never pans/zooms or gets clipped.
//!
//! Each item leads with a glyph (icons.rs) matching its action, and every write
//! item is suppressed on a read-only clone (Phase 12). A recurring, load-bearing
//! ordering rule runs through the handlers: a signal write must happen *before*
//! `menu.set(None)`, because closing the menu synchronously disposes the
//! handler's own reactive owner, after which a further signal write is
//! unreliable. The "Commit …" and merge/push/delete items all follow it.

use leptos::*;

use crate::api::{create_branch_request, fetch_head_branch};
use crate::icons::icon_set;
use crate::state::{Overlays, PendingOp, Settings};

/// The context menu overlay (Issue #18): a plain HTML pop-up positioned at the
/// click, rendered outside the SVG so it never pans/zooms and isn't clipped.
/// `read_only` (Phase 12) hides every write action on a cloned repo.
pub fn menu_view(overlays: Overlays, settings: Settings, read_only: bool) -> impl IntoView {
    let Overlays {
        menu,
        commit_dialog,
        commit_msg,
        confirm_op,
        detail_id,
        dialog_opened_at,
        reload,
    } = overlays;
    let nerd_icons = settings.nerd_icons;
    move || {
        menu.get().map(|m| {
            // Tracked read: the menu lives inside the overlays' reactive block,
            // so it re-renders live if the icon style is toggled while open.
            let ic = icon_set(nerd_icons.get());
            let label = m.github_label;
            let open_github = match m.github_url.clone() {
                // Live link: a real anchor, opening GitHub in a new tab. Tapping it
                // also closes the menu.
                Some(url) => view! {
                    <a
                        class="ctx-item"
                        href=url
                        target="_blank"
                        rel="noopener"
                        on:click=move |_| menu.set(None)
                    >
                        // The GitHub mark flags the one item that leaves the app.
                        <span class="nf ctx-icon">{ic.github}</span>
                        {label}
                    </a>
                }
                .into_view(),
                // No GitHub page for this target (no github remote, or unpushed):
                // show the option but disabled, with a reason on hover.
                None => view! {
                    <span
                        class="ctx-item disabled"
                        title="No GitHub page (no github.com remote, or it isn't pushed)"
                    >
                        <span class="nf ctx-icon">{ic.github}</span>
                        {label}
                    </span>
                }
                .into_view(),
            };
            // "View details" (Phase 10): open the side panel for this commit. A
            // read, so it's shown for read-only clones too. Set `detail_id` before
            // closing the menu — `menu.set(None)` disposes this handler's reactive
            // owner, after which a signal write is unreliable (same caveat as below).
            let detail_commit = m.commit.clone();
            let on_details = move |_| {
                detail_id.set(Some(detail_commit.clone()));
                menu.set(None);
            };
            // "View details" opens a commit's detail panel — the commit glyph.
            let details_item = view! {
                <button class="ctx-item" on:click=on_details>
                    <span class="nf ctx-icon">{ic.commit}</span>
                    "View details"
                </button>
            };
            // "Create branch from this commit": prompt for a name, POST it, then
            // refresh the graph on success or show git's error on failure (B3).
            let commit = m.commit.clone();
            let on_branch = move |_| {
                menu.set(None);
                let Some(win) = web_sys::window() else { return };
                // A native prompt — simple and works in iPad Safari. Empty / cancel
                // does nothing.
                let name = match win.prompt_with_message("Name for the new branch:") {
                    Ok(Some(n)) => n.trim().to_string(),
                    _ => return,
                };
                if name.is_empty() {
                    return;
                }
                let commit = commit.clone();
                spawn_local(async move {
                    match create_branch_request(&name, &commit).await {
                        // Bump the fetch counter so the new branch appears.
                        Ok(()) => reload.update(|n| *n = n.wrapping_add(1)),
                        Err(e) => {
                            if let Some(w) = web_sys::window() {
                                let _ = w.alert_with_message(&format!("Couldn't create branch:\n{e}"));
                            }
                        }
                    }
                });
            };
            let create_label = m.create_label;
            // The two "Commit …" items (Issue #33). Clicking one closes the menu
            // and opens the commit-message modal (below); the actual POST + refresh
            // happens when the user confirms there. They're enabled only on the
            // HEAD tip (the only place a commit can land without moving HEAD);
            // elsewhere they render disabled with a reason.
            let is_head = m.is_head;
            // `icon` distinguishes the two variants: the staged-changes commit
            // gets the diff-added glyph (it records staged additions), the empty
            // commit the plain commit glyph.
            let make_commit_item = move |icon: &'static str,
                                         label: &'static str,
                                         allow_empty: bool| {
                if !is_head {
                    return view! {
                        <span
                            class="ctx-item disabled"
                            title="Only available on the current HEAD commit"
                        >
                            <span class="nf ctx-icon">{icon}</span>
                            {label}
                        </span>
                    }
                    .into_view();
                }
                let on_commit = move |_| {
                    // Open the dialog *before* closing the menu: `menu.set(None)`
                    // synchronously disposes this handler's own reactive owner, so
                    // any signal write after it is unreliable. Set the dialog first.
                    commit_msg.set(String::new());
                    dialog_opened_at.set_value(js_sys::Date::now());
                    commit_dialog.set(Some(allow_empty));
                    menu.set(None);
                };
                view! {
                    <button class="ctx-item" on:click=on_commit>
                        <span class="nf ctx-icon">{icon}</span>
                        {label}
                    </button>
                }
                .into_view()
            };
            let commit_staged = make_commit_item(ic.added, "Commit staged changes", false);
            let commit_empty = make_commit_item(ic.commit, "Create empty commit", true);
            // The branch operations (Issue #33 follow-up): merge / push / delete, one
            // set per local branch living at this target. Each opens the confirm modal
            // rather than acting immediately — the actual POST + refresh happens there.
            // Set `confirm_op` *before* `menu.set(None)`, which disposes this handler's
            // reactive owner (same ordering caveat as the commit items above).
            let branch_items = m
                .branches
                .iter()
                .flat_map(|b| {
                    let b = b.clone();
                    // Merge into the checked-out branch. The target is resolved *live*
                    // on click (not from the possibly-stale graph), so the item stays
                    // generic — "into current branch" — and the confirm dialog names
                    // the real HEAD branch once the fetch returns. Whether it's a
                    // no-op self-merge or a detached HEAD is decided there too.
                    let merge_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            menu.set(None);
                            spawn_local(async move {
                                let into = fetch_head_branch().await.unwrap_or(None);
                                // Start the ghost-click guard when the modal opens.
                                dialog_opened_at.set_value(js_sys::Date::now());
                                confirm_op.set(Some(PendingOp::Merge { branch, into }));
                            });
                        };
                        view! {
                            <button class="ctx-item" on:click=on>
                                // The merge glyph, matching the merge-dot marker.
                                <span class="nf ctx-icon">{ic.merge}</span>
                                {format!("Merge ‘{b}’ into current branch")}
                            </button>
                        }
                        .into_view()
                    };
                    // Push: always available; git reports if there's no origin/upstream.
                    let push_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            dialog_opened_at.set_value(js_sys::Date::now());
                            confirm_op.set(Some(PendingOp::Push { branch: branch.clone() }));
                            menu.set(None);
                        };
                        view! {
                            <button class="ctx-item" on:click=on>
                                // Push updates the *remote* branch — its glyph.
                                <span class="nf ctx-icon">{ic.branch_alt}</span>
                                {format!("Push ‘{b}’")}
                            </button>
                        }
                        .into_view()
                    };
                    // Delete: like merge, the "is this the checked-out branch?" test is
                    // resolved live on click, not from the possibly-stale graph. The
                    // confirm dialog blocks deleting the current branch; git's safe
                    // `-d` still refuses an unmerged one server-side.
                    let delete_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            menu.set(None);
                            spawn_local(async move {
                                let current = fetch_head_branch().await.unwrap_or(None);
                                // Start the ghost-click guard when the modal opens.
                                dialog_opened_at.set_value(js_sys::Date::now());
                                confirm_op.set(Some(PendingOp::Delete { branch, current }));
                            });
                        };
                        view! {
                            <button class="ctx-item danger" on:click=on>
                                // The diff-removed glyph, inheriting the item's red.
                                <span class="nf ctx-icon">{ic.deleted}</span>
                                {format!("Delete ‘{b}’")}
                            </button>
                        }
                        .into_view()
                    };
                    [merge_item, push_item, delete_item]
                })
                .collect_view();
            // On a read-only clone (Phase 12) the menu is just the header + the
            // GitHub link: no branch/commit/merge/push/delete. Otherwise show the
            // full set of write actions.
            let write_items = (!read_only).then(|| {
                view! {
                    <button class="ctx-item" on:click=on_branch>
                        // Creating a branch — the branch glyph.
                        <span class="nf ctx-icon">{ic.branch}</span>
                        {create_label}
                    </button>
                    {commit_staged}
                    {commit_empty}
                    {branch_items}
                }
            });
            view! {
                <div class="ctx-menu" style=format!("left: {}px; top: {}px;", m.x, m.y)>
                    // Header glyph matches what the header names: a branch for a
                    // stub, a commit hash for a dot.
                    <div class="ctx-menu-header">
                        <span class="nf ctx-icon">
                            {if m.is_branch { ic.branch } else { ic.commit }}
                        </span>
                        {m.header.clone()}
                    </div>
                    {details_item}
                    {open_github}
                    {write_items}
                </div>
            }
        })
    }
}
