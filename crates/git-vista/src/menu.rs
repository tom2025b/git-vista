//! The per-commit context menu (Issue #18) — a plain HTML pop-up positioned at
//! the click, rendered outside the SVG so it never pans/zooms or gets clipped.
//!
//! Each item leads with a glyph (icons.rs) matching its action, and every write
//! item is suppressed on a read-only clone (Phase 12). A recurring, load-bearing
//! ordering rule runs through the handlers: a signal write must happen *before*
//! `shell.close_menu()`, because closing the menu synchronously disposes the
//! handler's own reactive owner, after which a further signal write is
//! unreliable. The "Commit …" and merge/push/delete items all follow it.

use leptos::*;

use git_vista_core::activity::UndoAction;

use crate::api::{
    create_branch_request, fetch_head_branch, fetch_rebase_status, fetch_status, fetch_undoables,
    stage_request, unstage_request,
};
use crate::features::core_traits::RequestTarget;
use crate::features::dialogs::core::Dialog;
use crate::features::operations::core::PendingIntent;
use crate::features::shell::signals::Shell;
use crate::geometry::menu_placement;
use crate::gestures::viewport_size;
use crate::icons::icon_set;
use crate::state::{CommitDialog, Features, MenuData, PendingOp, Settings};

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
        commit,
        header,
        x,
        y,
        github_url: None,
        github_label: "Open on GitHub",
        create_label: "Create branch from this commit…",
        is_head: false,
        branches: Vec::new(),
        is_branch: false,
        repo_url: None,
        remote_web_url: None,
    });
}

/// The context menu overlay (Issue #18): a plain HTML pop-up positioned at the
/// click, rendered outside the SVG so it never pans/zooms and isn't clipped.
/// `read_only` (Phase 12) hides every write action on a cloned repo.
pub fn menu_view(features: Features, settings: Settings, read_only: bool) -> impl IntoView {
    let Features {
        graph,
        dialogs,
        operations,
        status,
        shell,
        ..
    } = features;
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
    move || {
        shell.menu().map(|m| {
            // Tracked read: the menu lives inside the overlay wrapper's reactive block,
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
                        on:click=move |_| shell.close_menu()
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
            // closing the menu — `shell.close_menu()` disposes this handler's reactive
            // owner, after which a signal write is unreliable (same caveat as below).
            let detail_commit = m.commit.clone();
            let on_details = move |_| {
                // `false`: no "scroll to the Changes section" wish on a plain details
                // open. It is an argument rather than a separate poke precisely so this
                // path cannot forget to clear one left by an earlier "Show diff".
                //
                // Nothing here closes the Activity panel. The two share the right edge
                // and the overlay stack evicts whichever is already docked there — the
                // rule lives in one function now instead of at every opener (M1.11, #64,
                // Task 8).
                shell.open_detail(detail_commit.clone(), false);
                shell.close_menu();
            };
            // "View details" opens a commit's detail panel — the commit glyph.
            let details_item = view! {
                <button class="ctx-item" on:click=on_details>
                    <span class="nf ctx-icon">{ic.commit}</span>
                    "View details"
                </button>
            };
            // "Show diff": the same detail panel, but with the Changes section
            // scrolled into view once the diff lands — so the tap answers
            // "what did this commit change?" directly. The scroll wish rides
            // as an argument to `open_detail`, raised before the menu closes
            // (the reactive-owner ordering rule).
            let diff_commit = m.commit.clone();
            let on_diff = move |_| {
                shell.open_detail(diff_commit.clone(), true);
                shell.close_menu();
            };
            let diff_item = view! {
                <button class="ctx-item" on:click=on_diff>
                    // The diff-modified glyph — this item is about changed files.
                    <span class="nf ctx-icon">{ic.modified}</span>
                    "Show diff"
                </button>
            };
            // "Create branch from this commit": prompt for a name, POST it, then
            // refresh the graph on success or show git's error on failure (B3).
            let commit = m.commit.clone();
            let on_branch = move |_| {
                shell.close_menu();
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
                        Ok(()) => graph.update(|g| {
                            g.force_bump();
                        }),
                        Err(e) => {
                            if let Some(w) = web_sys::window() {
                                let _ =
                                    w.alert_with_message(&format!("Couldn't create branch:\n{e}"));
                            }
                        }
                    }
                });
            };
            let create_label = m.create_label;
            // The two "Commit …" items (Issue #33). Clicking one closes the menu
            // and opens the commit-message modal (below); the actual POST + refresh
            // happens when the user confirms there.
            //
            // On a commit dot they're enabled only on the HEAD tip — the one place
            // a plain `git commit` lands where the user clicked. On a branch stub,
            // "Create empty commit" is enabled too and targets the stub's own
            // branch (the server writes the commit object and moves just that ref,
            // no checkout needed) — it's exactly how an empty new branch takes its
            // first commit. Staged changes belong to the checked-out branch's
            // index, so that item stays HEAD-only everywhere. Anything else
            // renders disabled with the reason in its hover title.
            let is_head = m.is_head;
            let is_stub = m.is_branch;
            // A stub carries exactly its own branch name (see `MenuData::branches`).
            let stub_branch = is_stub.then(|| m.branches.first().cloned()).flatten();
            // `icon` is the glyph beside the item — the commit glyph for both
            // commit variants ("Stage Changes" below uses the diff-added glyph).
            let make_commit_item =
                move |icon: &'static str, label: &'static str, allow_empty: bool| {
                    let stub_branch = stub_branch.clone();
                    let enabled = is_head || (allow_empty && stub_branch.is_some());
                    if !enabled {
                        let reason = if is_stub {
                            "Staged changes can only be committed on the checked-out branch"
                        } else {
                            "Only available on the current HEAD commit"
                        };
                        return view! {
                            <span class="ctx-item disabled" title=reason>
                                <span class="nf ctx-icon">{icon}</span>
                                {label}
                            </span>
                        }
                        .into_view();
                    }
                    let on_commit = move |_| {
                        // Open the dialog *before* closing the menu: `shell.close_menu()`
                        // synchronously disposes this handler's own reactive owner, so
                        // any signal write after it is unreliable. Set the dialog first.
                        dialogs.clear_commit_msg();
                        dialogs.open(Dialog::Commit);
                        shell.open_commit_dialog(CommitDialog {
                            allow_empty,
                            branch: stub_branch.clone(),
                        });
                        shell.close_menu();
                    };
                    view! {
                        <button class="ctx-item" on:click=on_commit>
                            <span class="nf ctx-icon">{icon}</span>
                            {label}
                        </button>
                    }
                    .into_view()
                };
            let commit_changes = make_commit_item(ic.commit, "Commit Changes", false);
            let commit_empty = make_commit_item(ic.commit, "Create empty commit", true);
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
                            Err(e) => {
                                if let Some(w) = web_sys::window() {
                                    let _ = w.alert_with_message(&format!(
                                        "Couldn't stage changes:\n{e}"
                                    ));
                                }
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
                view! {
                    <span class="ctx-item disabled" title=reason>
                        <span class="nf ctx-icon">{ic.added}</span>
                        "Stage Changes"
                    </span>
                }
                .into_view()
            };
            // "Unstage Changes" (git reset HEAD): the exact inverse of "Stage
            // Changes" — the index goes back to HEAD, the working tree keeps
            // every edit. Appears only while something is actually staged
            // (live `/api/status`, tracked read so the item pops in when the
            // fetch lands) and only on the HEAD commit, like staging.
            let unstage_changes = (is_head && staged_count.get().unwrap_or(0) > 0).then(|| {
                let on_unstage = move |_| {
                    shell.close_menu();
                    spawn_local(async move {
                        match unstage_request().await {
                            Ok(()) => graph.update(|g| {
                                g.force_bump();
                            }),
                            Err(e) => {
                                if let Some(w) = web_sys::window() {
                                    let _ = w.alert_with_message(&format!(
                                        "Couldn't unstage changes:\n{e}"
                                    ));
                                }
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
            });
            // The branch operations (Issue #33 follow-up): merge / push / delete, one
            // set per local branch living at this target. Each opens the confirm modal
            // rather than acting immediately — the actual POST + refresh happens there.
            // Raise the confirm modal *before* `shell.close_menu()`, which disposes this
            // handler's reactive owner (same ordering caveat as the commit items above).
            let branch_items = m
                .branches
                .iter()
                .flat_map(|b| {
                    let b = b.clone();
                    // Checkout: switch HEAD (and the working tree) to this branch.
                    // Like merge/delete, the "already on it?" test resolves *live*
                    // on click, not from the possibly-stale graph — the confirm
                    // dialog disables itself when this is the checked-out branch.
                    let checkout_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            shell.close_menu();
                            // Identity is minted here, synchronously, before the await —
                            // it must record when the user tapped, not when the pre-check
                            // answered (M1.11, #64).
                            let seq = operations.next_seq();
                            let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                            spawn_local(async move {
                                let current = fetch_head_branch().await.unwrap_or(None);
                                let intent = PendingIntent {
                                    seq,
                                    key,
                                    kind: PendingOp::Checkout { branch, current },
                                };
                                if !operations.admit_intent(&intent) {
                                    return;
                                }
                                // Start the ghost-click guard when the modal opens.
                                dialogs.open(Dialog::Confirm);
                                shell.open_confirm(intent.kind);
                            });
                        };
                        view! {
                            <button class="ctx-item" on:click=on>
                                // The branch-switch glyph — HEAD moving between branches.
                                <span class="nf ctx-icon">{ic.checkout}</span>
                                {format!("Checkout ‘{b}’")}
                            </button>
                        }
                        .into_view()
                    };
                    // Merge into the checked-out branch. The target is resolved *live*
                    // on click (not from the possibly-stale graph), so the item stays
                    // generic — "into current branch" — and the confirm dialog names
                    // the real HEAD branch once the fetch returns. Whether it's a
                    // no-op self-merge or a detached HEAD is decided there too.
                    let merge_item = {
                        let branch = b.clone();
                        let on = move |_| {
                            let branch = branch.clone();
                            shell.close_menu();
                            let seq = operations.next_seq();
                            let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                            spawn_local(async move {
                                let into = fetch_head_branch().await.unwrap_or(None);
                                let intent = PendingIntent {
                                    seq,
                                    key,
                                    kind: PendingOp::Merge { branch, into },
                                };
                                if !operations.admit_intent(&intent) {
                                    return;
                                }
                                // Start the ghost-click guard when the modal opens.
                                dialogs.open(Dialog::Confirm);
                                shell.open_confirm(intent.kind);
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
                            dialogs.open(Dialog::Confirm);
                            shell.open_confirm(PendingOp::Push {
                                branch: branch.clone(),
                            });
                            shell.close_menu();
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
                            shell.close_menu();
                            let seq = operations.next_seq();
                            let key = operations.request_key(RequestTarget::Branch(branch.clone()));
                            spawn_local(async move {
                                let current = fetch_head_branch().await.unwrap_or(None);
                                let intent = PendingIntent {
                                    seq,
                                    key,
                                    kind: PendingOp::Delete { branch, current },
                                };
                                if !operations.admit_intent(&intent) {
                                    return;
                                }
                                // Start the ghost-click guard when the modal opens.
                                dialogs.open(Dialog::Confirm);
                                shell.open_confirm(intent.kind);
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
                    // "Create Pull Request": a real anchor to GitHub's compare page
                    // (`…/compare/main...<branch>`), opening in a new tab — a live
                    // link, not a scripted `window.open`, which iOS WebKit blocks
                    // (same reason as "Open on GitHub"). Shown only on a GitHub repo;
                    // omitted otherwise, since there's no compare page to point at.
                    let mut items = vec![checkout_item, merge_item, push_item];
                    // Non-GitHub forge branch link (ADR 0010): only when there is
                    // no GitHub base, so it never duplicates the GitHub items.
                    if m.repo_url.is_none() {
                        if let Some(base) = m.remote_web_url.as_ref() {
                            let url = git_vista_core::forge::branch_url(base, &b);
                            let host = git_vista_core::forge::host_label(base);
                            let branch = b.clone();
                            items.push(
                                view! {
                                    <a
                                        class="ctx-item"
                                        href=url
                                        target="_blank"
                                        rel="noopener"
                                        on:click=move |_| shell.close_menu()
                                    >
                                        <span class="nf ctx-icon">{ic.github}</span>
                                        {format!("View ‘{branch}’ on {host}")}
                                    </a>
                                }
                                .into_view(),
                            );
                        }
                    }
                    if let Some(base) = m.repo_url.as_ref() {
                        let branch = b.clone();
                        let url = format!("{base}/compare/main...{branch}");
                        items.push(
                            view! {
                                <a
                                    class="ctx-item"
                                    href=url
                                    target="_blank"
                                    rel="noopener"
                                    on:click=move |_| shell.close_menu()
                                >
                                    // The pull-request glyph flags this GitHub action.
                                    <span class="nf ctx-icon">{ic.pull_request}</span>
                                    {format!("Create Pull Request for ‘{branch}’")}
                                </a>
                            }
                            .into_view(),
                        );
                    }
                    items.push(delete_item);
                    items
                })
                .collect_view();
            // "Rebase onto main" (Issue #33 follow-up). Rebase acts on the *checked-
            // out* branch, not the clicked target — like the "Commit …" items — so
            // it's a single entry, not one per branch. Gated on the live
            // `/api/rebase-status`: disabled (with the reason) when the branch is
            // already based on the base, HEAD is detached, or there's no main —
            // a rebase that would do nothing shouldn't look available. While the
            // status is still loading the item stays enabled; the server answers
            // a raced no-op with "Already up to date" rather than a phantom
            // rebase. Resolve the live HEAD branch on click, then open the
            // confirm modal. Omitted on a branch stub: a zero-commit branch has
            // nothing to replay, and the item would silently target the checked-
            // out branch instead ("Rebase ‘main’ onto main?" from the stub's own
            // menu).
            let rebase_item = (!m.is_branch).then(|| {
                let status = rebase_status.get().flatten();
                let base = status
                    .as_ref()
                    .map_or_else(|| "main".to_string(), |s| s.base.clone());
                let label = format!("Rebase onto {base}");
                let reason = status.as_ref().and_then(|s| {
                    if s.branch.is_none() {
                        Some("HEAD is detached — no branch to rebase".to_string())
                    } else if !s.base_exists {
                        Some(format!("No ‘{}’ branch to rebase onto", s.base))
                    } else if s.up_to_date {
                        let b = s.branch.as_deref().unwrap_or("HEAD");
                        Some(format!(
                            "‘{b}’ is already based on {} — nothing to rebase",
                            s.base
                        ))
                    } else {
                        None
                    }
                });
                if let Some(reason) = reason {
                    return view! {
                        <span class="ctx-item disabled" title=reason>
                            <span class="nf ctx-icon">{ic.merge}</span>
                            {label}
                        </span>
                    }
                    .into_view();
                }
                let on = move |_| {
                    let base = base.clone();
                    shell.close_menu();
                    // Rebase targets the checked-out branch, not a named one, so its
                    // request identity is the repository itself.
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Repository);
                    spawn_local(async move {
                        let current = fetch_head_branch().await.unwrap_or(None);
                        let intent = PendingIntent {
                            seq,
                            key,
                            kind: PendingOp::Rebase { current, base },
                        };
                        if !operations.admit_intent(&intent) {
                            return;
                        }
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(intent.kind);
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // The merge glyph — rebase reintegrates onto another base.
                        <span class="nf ctx-icon">{ic.merge}</span>
                        {label}
                    </button>
                }
                .into_view()
            });
            // The undo section (step 5): one item per action `/api/undoables`
            // returned for this commit — reset-style undos when its result is
            // still a branch tip, a restore when it's a deleted branch's lost
            // tip, a revert for any non-merge commit. The tracked read means
            // the menu re-renders when the fetch lands; until then (or with
            // nothing to offer) the section simply isn't there. Each item opens
            // the shared confirm modal — it is raised BEFORE the menu closes
            // (the reactive-owner ordering rule above).
            let undo_items = undoables
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
                .collect_view();
            // On a read-only clone (Phase 12) the menu is just the header + the
            // GitHub link: no branch/commit/merge/push/delete/undo. Otherwise
            // show the full set of write actions.
            let write_items = (!read_only).then(|| {
                view! {
                    {undo_items}
                    <button class="ctx-item" on:click=on_branch>
                        // Creating a branch — the branch glyph.
                        <span class="nf ctx-icon">{ic.branch}</span>
                        {create_label}
                    </button>
                    {stage_changes}
                    {unstage_changes}
                    {commit_changes}
                    {commit_empty}
                    {branch_items}
                    {rebase_item}
                }
            });
            // Clamp the menu inside the *visual* viewport (iPad fix): a tap in
            // the lower half flips it above the finger, and its max-height is
            // the room actually available — anything past that scrolls
            // (.ctx-menu's overflow-y) instead of hanging offscreen where no
            // finger can reach it.
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
                    {details_item}
                    {diff_item}
                    {open_github}
                    {write_items}
                </div>
            }
        })
    }
}
