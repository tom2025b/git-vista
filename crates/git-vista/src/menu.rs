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

use leptos::*;

use git_vista_core::activity::UndoAction;

use crate::api::{
    create_branch_request, fetch_commit_detail, fetch_head_branch, fetch_rebase_status,
    fetch_status, fetch_undoables, fetch_worktree_status, stage_request, unstage_request,
};
use crate::features::core_traits::RequestTarget;
use crate::features::dialogs::commit::{amend_offer, AmendOffer};
use crate::features::dialogs::core::{branch_name_space_fix, Dialog, ErrorNotice};
use crate::features::graph::core::{disabled_menu_item_copy, pull_label};
use crate::features::operations::core::PendingIntent;
use crate::features::shell::signals::{self as shell_state, Shell};
use crate::features::status::core::{deletable_untracked_paths, discardable_tracked_paths};
use crate::geometry::menu_placement;
use crate::gestures::viewport_size;
use crate::icons::icon_set;
use crate::state::{CommitIntent, Features, MenuData, PendingOp, Settings};

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
                None => {
                    const REASON: &str =
                        "No GitHub page (no github.com remote, or it isn't pushed)";
                    let (aria_label, visible_reason) = disabled_menu_item_copy(label, REASON);
                    view! {
                        <button
                            class="ctx-item disabled"
                            title=REASON
                            aria-disabled="true"
                            aria-label=aria_label
                        >
                            <span class="nf ctx-icon">{ic.github}</span>
                            {label}
                            <span class="ctx-item-reason">{visible_reason}</span>
                        </button>
                    }
                    .into_view()
                }
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
                // The one pre-flight check worth doing client-side (#316):
                // a space is the common typo, and catching it here means an
                // offer to fix instead of a server round-trip to git's
                // "not a valid branch name". Everything else stays git's
                // call — its stderr now arrives unwrapped via the modal.
                let name = match branch_name_space_fix(&name) {
                    Some(fixed) => {
                        let accepted = win
                            .confirm_with_message(&format!(
                                "Branch names can't contain spaces.\nUse '{fixed}' instead?"
                            ))
                            .unwrap_or(false);
                        if !accepted {
                            return;
                        }
                        fixed
                    }
                    None => name,
                };
                let commit = commit.clone();
                spawn_local(async move {
                    match create_branch_request(&name, &commit).await {
                        // Bump the fetch counter so the new branch appears.
                        Ok(()) => graph.update(|g| {
                            g.force_bump();
                        }),
                        // The failure path finally meets the confirmation
                        // path's bar (#316): the app's own modal, showing the
                        // envelope's message — never raw JSON in an alert().
                        Err(e) => {
                            dialogs.open(Dialog::Error);
                            shell.open_error(ErrorNotice {
                                title: "Couldn't create branch",
                                body: e,
                            });
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
                        let (aria_label, visible_reason) = disabled_menu_item_copy(label, reason);
                        return view! {
                            <button
                                class="ctx-item disabled"
                                title=reason
                                aria-disabled="true"
                                aria-label=aria_label
                            >
                                <span class="nf ctx-icon">{icon}</span>
                                {label}
                                <span class="ctx-item-reason">{visible_reason}</span>
                            </button>
                        }
                        .into_view();
                    }
                    let on_commit = move |_| {
                        // Open the dialog *before* closing the menu: `shell.close_menu()`
                        // synchronously disposes this handler's own reactive owner, so
                        // any signal write after it is unreliable. Set the dialog first.
                        //
                        // No draft clear here (#226): opening is how a
                        // suspension-recovered draft comes back, so the opener must not
                        // wipe it. The draft clears on successful submit instead
                        // (`dialogs/commit.rs`'s `clear_message_for`), which is what
                        // actually consumes it. Note what `dialogs.open` *does* reset:
                        // the amend buffer and phase (#224), which belong to a different
                        // question than the one this item is asking.
                        dialogs.open(Dialog::Commit);
                        shell.open_commit_dialog(if allow_empty {
                            CommitIntent::Empty {
                                branch: stub_branch.clone(),
                            }
                        } else {
                            CommitIntent::Staged
                        });
                        // The dialog's staged-scope review renders from the shared
                        // status read, and the menu may have been sitting open since
                        // before the last stage/unstage. Refetching here is what makes
                        // the list the user is about to approve a statement about the
                        // repository *now* rather than whenever the panel last looked.
                        status.refetch();
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
            // "Amend last commit" (M2.19c, #224) — the third commit mode, beside the
            // other two and gated the same way, with one extra restriction: unlike an
            // empty commit, it is never offered on a branch stub. `GitOperation::
            // AmendCommit` has no branch target at all (it always rewrites the
            // checked-out branch's own tip), so there is no "amend that stub" to offer.
            //
            // The tapped commit's id is the compare-and-swap pin the request carries.
            // That is the point of taking it from here rather than re-reading HEAD at
            // submit time: it is the commit the user was looking at when they chose to
            // rewrite it, and the server refuses if the tip has moved since — which the
            // dialog then turns into a guided re-check rather than an error.
            //
            // The gate itself is `amend_offer`, in the host-tested core, not a
            // condition spelled out here: this file is wasm-only, so an inverted
            // or dropped condition would put "Amend last commit" on every stub —
            // or take it away everywhere — with nothing in the suite going red.
            let amend_tip = m.commit.clone();
            let amend_item = match amend_offer(is_head, is_stub) {
                AmendOffer::Offered => {
                    let on_amend = move |_| {
                        let tip = amend_tip.clone();
                        dialogs.open(Dialog::Commit);
                        shell.open_commit_dialog(CommitIntent::Amend {
                            expected_tip: tip.clone(),
                        });
                        // Hold the confirm button until the read below answers
                        // whether this commit is already on a remote (#225).
                        // Opening is synchronous and the read is not, so
                        // without this the dialog spends the whole request
                        // showing an *enabled* Amend button over a pre-flight
                        // that has nothing to read — and `amend_preflight`
                        // sends on "nothing read". Two ordering constraints,
                        // both pinned by `features::a11y::audit` because
                        // nothing here compiles under `cargo test`: after
                        // `dialogs.open` (which resets the phase), and before
                        // `shell.close_menu()` (which disposes this handler's
                        // reactive owner, after which writes are unreliable).
                        dialogs.begin_publication_read(&tip);
                        status.refetch();
                        shell.close_menu();
                        // Pre-fill with the tip's *whole* message (summary and body), not
                        // the graph row's first line: `git commit --amend -m` replaces the
                        // message outright, so seeding from a summary would silently drop
                        // the body of every commit amended from here. A failed read leaves
                        // the box empty and the confirm button disabled, which is the safe
                        // direction — the dialog never invents a message.
                        //
                        // The same read answers two questions (#225): the
                        // pre-fill, and whether this commit is already on a
                        // remote — `CommitDetail::on_remote`, an exact
                        // per-commit walk rather than membership of whatever
                        // page is loaded. Recorded against `tip` so it can only
                        // ever gate an amend of this commit. A failed read
                        // records nothing, and `amend_preflight` treats "not
                        // read" as unknown; see its doc comment for why unknown
                        // sends rather than escalates.
                        //
                        // Both answers go through `apply_amend_detail` rather
                        // than being written here, and that is the fix for a
                        // second window as real as the one the hold above
                        // closes: this callback resumes after an `await`, by
                        // which point the dialog may have been reopened on
                        // another commit. `PreflightKnowledge` holds one read
                        // at a time, so writing an abandoned tip's answer here
                        // *evicts* the answer for the commit on screen and the
                        // ceremony silently stops firing for it. The currency
                        // check lives in `detail_read_use`, where it is
                        // host-tested; nothing in this file is.
                        spawn_local(async move {
                            if let Ok(detail) = fetch_commit_detail(&tip).await {
                                dialogs.apply_amend_detail(&tip, detail.on_remote, &detail.message);
                            }
                            // Outside the `Ok` arm on purpose: a failed read
                            // has to release the button too, or one bad GET
                            // would make amend permanently unreachable. That
                            // lands on the documented `Unknown` ⇒ send path,
                            // which is a stated gap rather than a new one.
                            dialogs.finish_publication_read(&tip);
                        });
                    };
                    view! {
                        <button class="ctx-item" on:click=on_amend>
                            <span class="nf ctx-icon">{ic.commit}</span>
                            "Amend last commit"
                        </button>
                    }
                    .into_view()
                }
                AmendOffer::Blocked(reason) => {
                    let (aria_label, visible_reason) =
                        disabled_menu_item_copy("Amend last commit", reason);
                    view! {
                        <button
                            class="ctx-item disabled"
                            title=reason
                            aria-disabled="true"
                            aria-label=aria_label
                        >
                            <span class="nf ctx-icon">{ic.commit}</span>
                            "Amend last commit"
                            <span class="ctx-item-reason">{visible_reason}</span>
                        </button>
                    }
                    .into_view()
                }
            };
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
            // (live `/api/status`, tracked read so the item pops in when the
            // fetch lands) and only on the HEAD commit, like staging.
            //
            // #217: same reasoning as "Stage Changes" above — an index-only
            // change has no history to invalidate, so this refetches status
            // instead of bumping the graph epoch (which would also have reset
            // Print's `history_complete` for no reason).
            let unstage_changes = (is_head && staged_count.get().unwrap_or(0) > 0).then(|| {
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
            });
            let select_unstage = (is_head && staged_count.get().unwrap_or(0) > 0).then(|| {
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
            let live_status = worktree.get().flatten();
            let discard_changes = is_head.then(|| {
                let paths = live_status
                    .as_ref()
                    .map(discardable_tracked_paths)
                    .unwrap_or_default();
                if paths.is_empty() {
                    let reason = if live_status.is_none() {
                        "Waiting for a working-tree status read"
                    } else {
                        "No tracked file has uncommitted changes"
                    };
                    let (aria_label, visible_reason) =
                        disabled_menu_item_copy("Discard Changes…", reason);
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
                    let on = move |_| {
                        // Raise the modal *before* `close_menu` disposes this
                        // handler's reactive owner — the ordering rule this
                        // module's doc comment opens with.
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(PendingOp::DiscardTrackedPaths {
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
                let paths = live_status
                    .as_ref()
                    .map(deletable_untracked_paths)
                    .unwrap_or_default();
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
                    let on = move |_| {
                        dialogs.open(Dialog::Confirm);
                        shell.open_confirm(PendingOp::DeleteUntrackedPaths {
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
                    let (aria_label, visible_reason) = disabled_menu_item_copy(&label, &reason);
                    return view! {
                        <button
                            class="ctx-item disabled"
                            title=reason
                            aria-disabled="true"
                            aria-label=aria_label
                        >
                            <span class="nf ctx-icon">{ic.merge}</span>
                            {label}
                            <span class="ctx-item-reason">{visible_reason}</span>
                        </button>
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
            // Whether a Fetch or Pull is already running (#232, M2.20f).
            // Both share the single localStorage resume slot
            // (`prefs::INFLIGHT_REMOTE_OP_KEY` / `InFlightRemoteOp`) — see
            // that key's doc comment in `prefs.rs`. A second Fetch or Pull
            // admitted while one is in flight overwrites that one entry, so
            // on reload only the second resumes and the first is silently
            // lost (or the second settles and clears the key while the
            // first is still running). This closure is the actual gate;
            // `operations.core()` is the same public accessor
            // `in_flight_count` uses, just filtered to the two kinds that
            // share the slot rather than counting every in-flight write.
            let remote_op_running = move || {
                operations.core().with(|c| {
                    c.in_flight()
                        .find(|f| {
                            matches!(f.kind, PendingOp::Fetch { .. } | PendingOp::Pull { .. })
                        })
                        .map(|f| f.kind.describe())
                })
            };
            // "Fetch" (#232, M2.20f): repo-scoped like Rebase, not per-branch
            // like Push — there's no per-branch remote-tracking surface in
            // this menu. Single tap, styled exactly like `push_item`: no
            // live pre-check needed, because a fetch has no branch
            // dependency the way merge/checkout/delete do. ADR 0047 records
            // that in practice only `origin` is ever in play, and #232's
            // scope names no remote picker, so the remote is fixed rather
            // than offered as a choice.
            //
            // Disabled (with reason, #65) while a Fetch or Pull is already
            // in flight — see `remote_op_running` above.
            let fetch_item = (!m.is_branch).then(|| {
                if let Some(running) = remote_op_running() {
                    let reason = format!("{running} — only one Fetch or Pull can run at a time");
                    let (aria_label, visible_reason) = disabled_menu_item_copy("Fetch", &reason);
                    return view! {
                        <button
                            class="ctx-item disabled"
                            title=reason
                            aria-disabled="true"
                            aria-label=aria_label
                        >
                            <span class="nf ctx-icon">{ic.branch_alt}</span>
                            "Fetch"
                            <span class="ctx-item-reason">{visible_reason}</span>
                        </button>
                    }
                    .into_view();
                }
                let on = move |_| {
                    dialogs.open(Dialog::Confirm);
                    shell.open_confirm(PendingOp::Fetch {
                        remote: "origin".to_string(),
                    });
                    shell.close_menu();
                };
                view! {
                    <button class="ctx-item" on:click=on>
                        // Reuses the remote-branch glyph — both actions talk
                        // to the remote, and this app has no dedicated
                        // fetch/pull icon yet.
                        <span class="nf ctx-icon">{ic.branch_alt}</span>
                        "Fetch"
                    </button>
                }
                .into_view()
            });
            // "Pull" (#232, M2.20f, ADR 0044): repo-scoped like Rebase.
            // Unlike every other branch op here, this cannot open the shared
            // `Dialog::Confirm` modal directly: `MergeStrategy` has exactly
            // two variants, derives no `Default`, and carries no sentinel
            // "not yet chosen" value (plan.rs:307-316), so there is no
            // `OperationKind::Pull` this click could build before the user
            // has picked one — inventing a placeholder to "correct before
            // dispatch" would be exactly the silent default ADR 0044 spent
            // three enforcement layers ruling out at the wire layer. Instead
            // this opens the picker (`Dialogs::open_pull_picker`), which
            // holds only `{remote, branch}` until a tap on Merge or Rebase
            // supplies the missing field; only the picker's own confirm tap
            // constructs `OperationKind::Pull`, at the same instant it is
            // dispatched.
            //
            // The branch is resolved live on click, exactly like
            // `rebase_item`'s `fetch_head_branch()` pre-check above, guarded
            // by the same click-order race protection (`admit_intent`) every
            // other live-checked item here uses: a slower response from an
            // earlier tap must not reopen the picker over a dialog a later
            // tap is already showing. The intent's `kind` is never sent
            // anywhere — `operations.dispatch` is never called with it —
            // `MergeStrategy::Merge` is an inert placeholder that exists only
            // to satisfy `PendingIntent`'s shape and is discarded the
            // instant `admit_intent` returns; it never reaches the picker,
            // the wire, or the screen.
            //
            // The label (#325 follow-up) names the branch the same way
            // `rebase_item`'s does — read from `rebase_status` above, which
            // already carries the checked-out branch (`RebaseStatus::branch`,
            // itself `git_vista_git::read_head_branch`) under the identical
            // `!m.is_branch` gate this item renders behind, so this costs no
            // new resource or poll. `pull_label` (features::graph::core) is
            // pure so the composition is host-tested; `None` (status still
            // loading, or a detached HEAD) degrades to naming just the
            // remote rather than the bare "Pull" this replaces.
            //
            // Disabled (with reason, #65) while a Fetch or Pull is already
            // in flight — see `remote_op_running` above `fetch_item`.
            let pull_item = (!m.is_branch).then(|| {
                let branch = rebase_status.get().flatten().and_then(|s| s.branch);
                let label = pull_label(branch.as_deref(), "origin");
                if let Some(running) = remote_op_running() {
                    let reason = format!("{running} — only one Fetch or Pull can run at a time");
                    let (aria_label, visible_reason) = disabled_menu_item_copy(&label, &reason);
                    return view! {
                        <button
                            class="ctx-item disabled"
                            title=reason
                            aria-disabled="true"
                            aria-label=aria_label
                        >
                            <span class="nf ctx-icon">{ic.merge}</span>
                            {label}
                            <span class="ctx-item-reason">{visible_reason}</span>
                        </button>
                    }
                    .into_view();
                }
                let on = move |_| {
                    shell.close_menu();
                    let seq = operations.next_seq();
                    let key = operations.request_key(RequestTarget::Repository);
                    spawn_local(async move {
                        let remote = "origin".to_string();
                        match fetch_head_branch().await.unwrap_or(None) {
                            Some(branch) => {
                                let intent = PendingIntent {
                                    seq,
                                    key,
                                    kind: PendingOp::Pull {
                                        remote: remote.clone(),
                                        branch: branch.clone(),
                                        strategy: git_vista_protocol::plan::MergeStrategy::Merge,
                                    },
                                };
                                if !operations.admit_intent(&intent) {
                                    return;
                                }
                                dialogs.open_pull_picker(remote, branch);
                            }
                            // No branch to pull into. #316 pattern: the app's
                            // own modal, never a silent no-op and never a
                            // native alert().
                            None => {
                                dialogs.open(Dialog::Error);
                                shell.open_error(ErrorNotice {
                                    title: "Can't pull",
                                    body: "HEAD is detached — check out a branch first."
                                        .to_string(),
                                });
                            }
                        }
                    });
                };
                view! {
                    <button class="ctx-item" on:click=on>
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
                    <button class="ctx-item" on:click=on_branch>
                        // Creating a branch — the branch glyph.
                        <span class="nf ctx-icon">{ic.branch}</span>
                        {create_label}
                    </button>
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
                    {offline_notice}
                </div>
            }
        })
    }
}
