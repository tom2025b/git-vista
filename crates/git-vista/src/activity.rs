//! The Activity panel (Activity/Undo feature): a right-docked panel, opened
//! from the topbar, showing the live working-tree status on top and the
//! chronological event feed below — every commit, merge, rebase, branch
//! operation and push, each marked "app" (done through git-vista) or
//! "terminal" (done outside it), with relative timestamps.
//!
//! Tapping a feed row opens **the same context menu** the graph's dots use
//! (`menu::menu_view`), pointed at the commit the event references — so
//! "View details", "Show diff", "Create branch…" and (step 5) the undo items
//! all come for free, one implementation, two entry points. For a deleted
//! branch the referenced commit is its last tip, which makes "Create
//! branch from this commit" a manual restore even before the dedicated undo.
//!
//! Same chrome recipe as the detail panel (it shares the `.detail-panel` CSS
//! family): an explicit ✕ close button — never Esc-only, the iPad keyboard
//! has no Esc — and it never shows a stale feed (the issue-16 lesson): the feed
//! re-fetches every time the panel opens, and so does the working-tree status,
//! via its own v2 `WorktreeStatus` read (M2.15, #68) — grouped into touch-card
//! sections with accessible labels, not the flat v1 list the topbar chip still
//! reads separately (M1.11, #64).

use leptos::*;

use git_vista_core::activity::{ActivityEvent, ActivitySource};

use crate::api::{fetch_activity, fetch_tags, fetch_worktree_status};
use crate::datetime::time_ago;
use crate::features::activity::core::{event_commit, kind_glyph, kind_label};
use crate::features::dialogs::core::Dialog;
use crate::features::shell::signals as shell_state;
use crate::features::status::core::{chip_label, StatusHeadline, StatusSection, StatusSections};
use crate::features::tags::core::{
    tag_list_view, tag_row_lines, TagListView, TagRow, LOADING_TAGS, NO_TAGS,
};
use crate::icons::icon_set;
use crate::menu;
use crate::state::{Features, PendingOp, Settings};

/// How many events to request. The panel is a scrollable feed, not an
/// archive; the backend caps harder anyway.
const FEED_LIMIT: usize = 100;

/// Build the Activity panel view. Rendered inside the overlays wrapper, so it
/// shares the reactive context the menu and modals use. `read_only` (Visualize
/// mode, ADR 0006/0007) hides the rows' Undo buttons — the feed itself is a
/// read and stays available.
pub fn activity_panel_view(
    features: Features,
    settings: Settings,
    read_only: bool,
) -> impl IntoView {
    let Features { graph, shell, .. } = features;
    let nerd_icons = settings.nerd_icons;

    // The feed keys on (open, reload): opening the panel fetches fresh, and any
    // post-operation reload — an undo confirmed from this very panel, a branch created
    // from a row's menu — refreshes it in place. Closed → resolve to None without
    // touching the network.
    let feed = create_local_resource(
        move || (shell.activity_is_open(), graph.get().epoch()),
        |(open, _)| async move {
            if open {
                Some(fetch_activity(FEED_LIMIT).await)
            } else {
                None
            }
        },
    );

    // The v2 working-tree status (M2.15, #68), keyed exactly like the feed
    // above: open the panel and it is read fresh, and any operation that
    // bumps the graph epoch refreshes it in place. This is the panel's own
    // read now, not the shared v1 resource the topbar chip still uses —
    // #68d's grouped `StatusSections` needs the entry-level detail
    // (`WorktreeStatus::entries`) v1's `RepoStatus` never carried.
    let worktree_status = create_local_resource(
        move || (shell.activity_is_open(), graph.get().epoch()),
        |(open, _)| async move {
            if open {
                fetch_worktree_status().await.ok()
            } else {
                None
            }
        },
    );

    // The tag list (M2.21b, #236), keyed exactly like the feed above: open the
    // panel and it is read fresh, and any operation that bumps the graph epoch
    // — including one that creates or deletes a tag — refreshes it in place.
    // Same key, one fetch each, no second "is the panel open" to drift.
    let tags = create_local_resource(
        move || (shell.activity_is_open(), graph.get().epoch()),
        |(open, _)| async move {
            if open {
                Some(fetch_tags().await)
            } else {
                None
            }
        },
    );

    // The right-edge exclusivity effect that used to sit here is gone (M1.11, #64,
    // Task 8). It cleared the detail panel one reactive tick *after* this panel's
    // visibility flipped, while the opposite direction wrote synchronously from a click
    // handler — so for one frame both panels rendered. The rule now lives in
    // `OverlayStack::present`, which evicts whatever already holds the right edge before
    // this panel is ever marked open, and it runs in the same tick as the tap.

    move || {
        shell.activity_is_open().then(|| {
            // Tracked read, like the other overlays: the panel re-renders live
            // if the icon style is toggled while it's open.
            let ic = icon_set(nerd_icons.get());

            // -- The working-tree status section (M2.15, #68) ----------------
            //
            // Built on the v2 grouped `StatusSections` (#68d), not a hand-rolled
            // flat list over v1 fields: each section renders as its own
            // touch-card list, `role="list"`/`role="listitem"` with the
            // pre-computed `accessible_label` per card — #68's own acceptance
            // criterion, satisfied by consuming the data #68d built for exactly
            // this rather than re-deriving labels here.
            let status_section = move || {
                worktree_status.get().flatten().map(|s| {
                    let ic = icon_set(nerd_icons.get());
                    let sections = StatusSections::from_worktree_status(&s);
                    // Same grouping rule as the topbar chip — one host-tested
                    // function, so the two can no longer disagree about the
                    // same worktree the way #348 caught them doing.
                    let headline = chip_label(
                        sections.count(StatusSection::Staged),
                        sections.count(StatusSection::Unstaged),
                        sections.count(StatusSection::Untracked),
                        sections.count(StatusSection::Conflicted),
                    );
                    let (glyph, class) = match sections.headline() {
                        StatusHeadline::Conflicted(_) => (ic.conflict, "act-status conflict"),
                        StatusHeadline::Dirty(_) => (ic.dirty, "act-status dirty"),
                        StatusHeadline::Clean => (ic.clean, "act-status clean"),
                    };
                    let sync = (s.ahead > 0 || s.behind > 0).then(|| {
                        let mut t = String::new();
                        if s.ahead > 0 {
                            t.push_str(&format!(" ↑{}", s.ahead));
                        }
                        if s.behind > 0 {
                            t.push_str(&format!(" ↓{}", s.behind));
                        }
                        if let Some(u) = s.upstream.as_deref() {
                            t.push_str(&format!(" vs {u}"));
                        }
                        view! { <span class="detail-muted">{t}</span> }
                    });

                    // One touch-card list per non-empty section, in urgency
                    // order (`StatusSection::ALL`). Conflicted/Staged/Unstaged/
                    // Untracked are capped together — a huge tree should not
                    // bury the feed this panel is really for; Ignored gets its
                    // own, smaller cap and a compact summary rather than every
                    // path, since an ignored tree (`node_modules`-shaped) can
                    // be arbitrarily large and none of those paths are
                    // actionable here.
                    const FILE_CAP: usize = 12;
                    const IGNORED_CAP: usize = 4;
                    let mut shown = 0usize;
                    let sections_view = StatusSection::ALL
                        .into_iter()
                        .filter(|&sec| sections.count(sec) > 0)
                        .map(|sec| {
                            let rows = sections.rows(sec);
                            let count = rows.len();
                            let cap = if sec == StatusSection::Ignored {
                                IGNORED_CAP
                            } else {
                                FILE_CAP.saturating_sub(shown)
                            };
                            let glyph = match sec {
                                StatusSection::Conflicted => ic.conflict,
                                StatusSection::Staged => ic.added,
                                StatusSection::Unstaged => ic.modified,
                                StatusSection::Untracked => ic.untracked,
                                StatusSection::Ignored => ic.ignored,
                            };
                            let taken = rows.iter().take(cap);
                            let card_count = taken.len();
                            if sec != StatusSection::Ignored {
                                shown += card_count;
                            }
                            let cards = taken
                                .map(|row| {
                                    view! {
                                        <div
                                            role="listitem"
                                            class="act-status-card"
                                            aria-label=row.accessible_label.clone()
                                        >
                                            <span class="nf ctx-icon">{glyph}</span>
                                            <span class="act-file-path">
                                                {row.path.clone()}
                                            </span>
                                            {row.secondary.clone().map(|sec| {
                                                view! {
                                                    <span class="detail-muted act-file-secondary">
                                                        {sec}
                                                    </span>
                                                }
                                            })}
                                        </div>
                                    }
                                })
                                .collect_view();
                            let overflow = count.saturating_sub(card_count);
                            let more = (overflow > 0).then(|| {
                                view! {
                                    <div class="detail-muted act-file">
                                        {format!("… and {overflow} more")}
                                    </div>
                                }
                            });
                            view! {
                                <div class="act-status-section">
                                    <p class="act-status-heading">
                                        {format!("{} ({})", sec.heading(), count)}
                                    </p>
                                    <div role="list" aria-label=sec.heading()>
                                        {cards}
                                        {more}
                                    </div>
                                </div>
                            }
                        })
                        .collect_view();

                    view! {
                        <div class=class>
                            <span class="nf ctx-icon">{glyph}</span>
                            {headline}
                            {sync}
                        </div>
                        {sections_view}
                    }
                })
            };

            // -- The tag list (M2.21b, #236). --------------------------------
            // Which of the four states we are in is decided in the host-tested
            // `features::tags::core`, not here: this file is
            // `#[cfg(target_arch = "wasm32")]`, so a `match` written here is
            // compiled by `trunk build` and by nothing that asserts anything.
            // What is left below is one arm per variant with no condition of
            // its own — the mapping a reader can check by eye.
            let tags_section = move || match tag_list_view(tags.get().flatten()) {
                TagListView::Loading => {
                    view! { <p class="detail-status">{LOADING_TAGS}</p> }.into_view()
                }
                TagListView::Failed(line) => {
                    view! { <p class="detail-status detail-error">{line}</p> }.into_view()
                }
                TagListView::Empty => view! { <p class="detail-status">{NO_TAGS}</p> }.into_view(),
                TagListView::Rows(rows) => rows
                    .into_iter()
                    .map(|row| tag_row_view(row, nerd_icons))
                    .collect_view(),
            };

            // -- The feed itself. --------------------------------------------
            let feed_section = move || match feed.get().flatten() {
                None => view! { <p class="detail-status">"Loading activity…"</p> }.into_view(),
                Some(Err(e)) => view! {
                    <p class="detail-status detail-error">
                        {format!("Couldn't load activity: {e}")}
                    </p>
                }
                .into_view(),
                Some(Ok(events)) if events.is_empty() => view! {
                    <p class="detail-status">
                        "Nothing recorded yet — this repo has no reflog entries."
                    </p>
                }
                .into_view(),
                Some(Ok(events)) => events
                    .into_iter()
                    .map(|e| activity_row(e, nerd_icons, features, read_only))
                    .collect_view(),
            };

            view! {
                <aside class="detail-panel activity-panel">
                    <div class="detail-head">
                        <span class="detail-title">
                            <span class="nf ctx-icon">{ic.history}</span>
                            "Activity"
                        </span>
                        <span class="act-head-buttons">
                            <button
                                class="act-refresh"
                                title="Re-read the repository and this feed"
                                on:click=move |_| graph.update(|g| {
                                    g.force_bump();
                                })
                            >
                                "Refresh"
                            </button>
                            <button
                                class="detail-close"
                                title="Close"
                                // The visible content is one glyph; VoiceOver would
                                // otherwise announce this as "multiplication sign" (#65).
                                aria-label="Close activity"
                                on:click=move |_| shell.close_activity()
                            >
                                "×"
                            </button>
                        </span>
                    </div>
                    <div class="detail-body">
                        {status_section}
                        <div class="detail-section-title act-feed-title">
                            "Tags"
                        </div>
                        {tags_section}
                        <div class="detail-section-title act-feed-title">
                            "History"
                        </div>
                        {feed_section}
                    </div>
                </aside>
            }
        })
    }
}

/// One tag row (M2.21b, #236): the tag icon, its name, a pill saying which
/// **kind** of tag it is, the tagged commit's short id, and — for an annotated
/// tag — the tagger and the message's first line.
///
/// Every "what does absence look like" decision was already made in
/// `features::tags::core`, which is host-tested; this function only spends
/// what it is handed.
///
/// That is why the sub-lines arrive as a `Vec<TagRowLine>` from
/// [`tag_row_lines`] rather than as three `Option` fields this file unwraps:
/// this module is `#[cfg(target_arch = "wasm32")]` and never compiles under
/// `cargo test --workspace`, so an `Option::unwrap_or_default()` written here
/// — which would put an *empty* "tagger" line on screen, the exact false claim
/// the `None`-not-`""` design exists to prevent — would be caught by nothing
/// but a human reading the diff. With the vector, an absent field is a line
/// that is not in it, and `a_lightweight_tag_contributes_no_lines_at_all`
/// tests that on the host. What is left here is a per-variant class choice.
///
/// Reuses the `act-file` / `act-pill` / `detail-muted` classes the working-tree
/// section above already styles, so the list needs no new CSS (and so the
/// a11y stylesheet census keeps covering it).
fn tag_row_view(row: TagRow, nerd_icons: RwSignal<bool>) -> impl IntoView {
    let ic = icon_set(nerd_icons.get_untracked());
    let lines = tag_row_lines(&row)
        .into_iter()
        .map(|line| {
            let class = if line.muted() {
                "act-file detail-muted"
            } else {
                "act-file"
            };
            let text = line.text().to_string();
            view! { <div class=class><span class="act-file-path">{text}</span></div> }
        })
        .collect_view();
    let TagRow {
        name,
        kind_label,
        target_short,
        target,
        signature_badge,
        ..
    } = row;
    let signature = signature_badge.map(|s| view! { <span class="act-pill">{s}</span> });
    view! {
        <div class="act-file" title=format!("{name} → {target}")>
            <span class="nf ctx-icon">{ic.tag}</span>
            <span class="act-file-path">{name}</span>
            <span class="act-pill">{kind_label}</span>
            <span class="detail-muted">{target_short}</span>
            {signature}
        </div>
        {lines}
    }
}

/// One feed row. Tapping it opens the shared context menu on the commit the
/// event references (its result, or — for a deletion — the tip that died).
/// Events that reference no commit render as plain, non-tappable rows. An
/// event still carrying an undo hint (step 5) gets a direct Undo button that
/// opens the shared confirm modal — the same `PendingOp::Undo` flow the graph
/// menu's undo section uses.
///
/// The row is a `<div>` with a click handler, not a `<button>`: the Undo
/// button lives inside it, and a button nested in a button is invalid HTML
/// that browsers un-nest unpredictably.
fn activity_row(
    event: ActivityEvent,
    nerd_icons: RwSignal<bool>,
    features: Features,
    read_only: bool,
) -> impl IntoView {
    let Features { dialogs, shell, .. } = features;
    let ic = icon_set(nerd_icons.get_untracked());
    let glyph = kind_glyph(ic, event.kind);
    let when = time_ago(event.time);
    let source = match event.source {
        ActivitySource::App => view! { <span class="act-pill act-app">"app"</span> }.into_view(),
        ActivitySource::External => {
            view! { <span class="act-pill act-terminal">"terminal"</span> }.into_view()
        }
    };
    let ref_pill = event
        .ref_name
        .clone()
        .map(|r| view! { <span class="act-pill act-ref">{r}</span> });

    // The commit this event is "about" — where the ref ended up, or the tip a deletion
    // killed. The null-oid rule that decides it is host-tested in the feature core
    // (M1.11, #64); it used to be an inline pair of `.filter()`s here.
    let commit = event_commit(&event);

    let header = format!(
        "{}{}",
        kind_label(event.kind),
        event
            .ref_name
            .as_deref()
            .map(|r| format!(" · {r}"))
            .unwrap_or_default()
    );
    // The direct Undo control (step 5): shown only while the server still
    // says this event is undoable — `event.undo` is recomputed on every feed
    // read, so a hint can't outlive its validity by more than one refresh
    // (and the server's compare-and-swap catches even that window). Opens the
    // shared confirm modal; the panel stays open, and the confirmed undo's
    // `reload` bump refreshes this very feed in place.
    // In Visualize mode the button is absent entirely (ADR 0007 gating audit):
    // the api.rs chokepoint and the server 403 back this up in depth.
    //
    // M2.22b (#242): hidden while the device reports offline, too. A closure,
    // not a value, because a row renders once per feed read — a plain
    // `.then()` here would leave the button up until the next refresh, while
    // this tracked read hides it the moment connectivity flips. If the button
    // held focus at that moment, focus falls to <body> (the row is a plain
    // div, not focusable) — a focus *loss*, not a trap; the keyboard user
    // re-enters from the top. Accepted for a rare transition rather than
    // adding focus-management code no host test can exercise; the iPad
    // testbed pass drives it. `navigator.onLine` can read true over a dead
    // tunnel — hiding is the UX nicety, `api.rs`'s `refuse_if_offline()`
    // guard is the boundary.
    let undo_hint = event.undo.clone();
    let undo_btn = move || {
        (!read_only && shell_state::online_signal().get())
            .then(|| undo_hint.clone())
            .flatten()
            .map(|u| {
                let title = u.label.clone();
                let on = move |ev: web_sys::MouseEvent| {
                    // The row underneath opens the context menu — this tap shouldn't.
                    ev.stop_propagation();
                    // Opens the shared confirm modal; it is that modal's own Confirm button
                    // that dispatches the undo. Deliberately NOT `operations.dispatch(…)`
                    // directly: this is a destructive action reachable from two places, and
                    // the graph menu's identical item confirms first.
                    dialogs.open(Dialog::Confirm);
                    shell.open_confirm(PendingOp::Undo(u.clone()));
                };
                view! {
                    <button class="act-undo" title=title on:click=on>
                        <span class="nf">{ic.undo}</span>
                        " Undo"
                    </button>
                }
            })
    };

    let row_body = view! {
        <span class="nf ctx-icon act-glyph">{glyph}</span>
        <span class="act-main">
            <span class="act-summary">{event.summary.clone()}</span>
            <span class="act-meta">
                {ref_pill}
                {source}
                <span class="act-when">{when}</span>
            </span>
        </span>
        {undo_btn}
    };

    match commit {
        Some(commit) => {
            let on_tap = move |ev: web_sys::MouseEvent| {
                // The same menu the graph's dots open — one menu, two entry points — but
                // built by the module that owns `MenuData` rather than by a literal here
                // (M1.11, #64). Raw tap coords: the menu view itself clamps every entry
                // point (geometry.rs::menu_placement), which replaced the right-edge
                // clamp that used to live here.
                menu::open_for_commit(
                    shell,
                    commit.clone(),
                    header.clone(),
                    ev.client_x() as f64,
                    ev.client_y() as f64,
                );
            };
            view! {
                <div class="act-row" on:click=on_tap>
                    {row_body}
                </div>
            }
            .into_view()
        }
        None => view! { <div class="act-row act-row-static">{row_body}</div> }.into_view(),
    }
}
