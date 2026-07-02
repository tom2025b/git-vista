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
//! has no Esc — and both fetches re-fire every time the panel opens, so it
//! never shows a stale feed (the issue-16 lesson).

use leptos::*;

use git_vista_core::activity::{ActivityEvent, ActivityKind, ActivitySource};

use crate::api::{fetch_activity, fetch_status};
use crate::datetime::time_ago;
use crate::icons::{icon_set, GitIcons};
use crate::state::{MenuData, Overlays, Settings};

/// How many events to request. The panel is a scrollable feed, not an
/// archive; the backend caps harder anyway.
const FEED_LIMIT: usize = 100;

/// The glyph for one event kind. Rebase deliberately shares the merge glyph —
/// the existing "Rebase onto main" menu item already reads that way — and
/// pull shares it too (a pull *is* fetch + merge).
fn kind_glyph(ic: &GitIcons, kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Commit | ActivityKind::CherryPick | ActivityKind::Other => ic.commit,
        ActivityKind::Amend => ic.modified,
        ActivityKind::Merge | ActivityKind::Rebase | ActivityKind::Pull => ic.merge,
        ActivityKind::Checkout => ic.checkout,
        ActivityKind::Reset | ActivityKind::Revert => ic.undo,
        ActivityKind::BranchCreated => ic.branch,
        ActivityKind::BranchDeleted => ic.deleted,
        ActivityKind::Push => ic.push,
        ActivityKind::Fetch => ic.branch_alt,
        ActivityKind::Clone => ic.repository,
    }
}

/// Short human name for one event kind — the row's leading word.
fn kind_label(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Commit => "Commit",
        ActivityKind::Amend => "Amend",
        ActivityKind::Merge => "Merge",
        ActivityKind::Rebase => "Rebase",
        ActivityKind::Checkout => "Switch",
        ActivityKind::Reset => "Reset",
        ActivityKind::CherryPick => "Cherry-pick",
        ActivityKind::Revert => "Revert",
        ActivityKind::BranchCreated => "Branch created",
        ActivityKind::BranchDeleted => "Branch deleted",
        ActivityKind::Push => "Push",
        ActivityKind::Fetch => "Fetch",
        ActivityKind::Pull => "Pull",
        ActivityKind::Clone => "Clone",
        ActivityKind::Other => "Event",
    }
}

/// Keep the context menu on screen when opened near the right edge (the
/// panel is right-docked, so every row tap is near it). Mirrors the menu's
/// own width plus margin; clamping beats teaching the menu to re-anchor.
fn clamp_menu_x(x: f64) -> f64 {
    let width = web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    x.min((width - 280.0).max(8.0))
}

/// Build the Activity panel view. Rendered inside the overlays wrapper, so it
/// shares the reactive context the menu and modals use.
pub fn activity_panel_view(overlays: Overlays, settings: Settings) -> impl IntoView {
    let Overlays { menu, detail_id, activity_open, reload, .. } = overlays;
    let nerd_icons = settings.nerd_icons;

    // Both fetches key on (open, reload): opening the panel fetches fresh,
    // and any post-operation reload — an undo confirmed from this very panel,
    // a branch created from a row's menu — refreshes it in place. Closed →
    // resolve to None without touching the network.
    let feed = create_local_resource(
        move || (activity_open.get(), reload.get()),
        |(open, _)| async move {
            if open {
                Some(fetch_activity(FEED_LIMIT).await)
            } else {
                None
            }
        },
    );
    let status = create_local_resource(
        move || (activity_open.get(), reload.get()),
        |(open, _)| async move {
            if open {
                fetch_status().await.ok()
            } else {
                None
            }
        },
    );

    // The right edge belongs to one panel at a time: opening Activity closes
    // the commit detail panel (and the menu handlers do the reverse).
    create_effect(move |_| {
        if activity_open.get() {
            detail_id.set(None);
        }
    });

    move || {
        activity_open.get().then(|| {
            // Tracked read, like the other overlays: the panel re-renders live
            // if the icon style is toggled while it's open.
            let ic = icon_set(nerd_icons.get());

            // -- The working-tree status section (step 1's data, richer). ----
            let status_section = move || {
                status.get().flatten().map(|s| {
                    let ic = icon_set(nerd_icons.get());
                    let (glyph, class, headline) = if !s.conflicted.is_empty() {
                        (ic.conflict, "act-status conflict",
                         format!("{} conflicted file(s)", s.conflicted.len()))
                    } else if !s.is_clean() {
                        let n = s.change_count();
                        (ic.dirty, "act-status dirty",
                         format!("{n} uncommitted change{}", if n == 1 { "" } else { "s" }))
                    } else {
                        (ic.clean, "act-status clean", "working tree clean".to_string())
                    };
                    let sync = (s.ahead > 0 || s.behind > 0).then(|| {
                        let mut t = String::new();
                        if s.ahead > 0 { t.push_str(&format!(" ↑{}", s.ahead)); }
                        if s.behind > 0 { t.push_str(&format!(" ↓{}", s.behind)); }
                        s.upstream.as_deref().map(|u| t.push_str(&format!(" vs {u}")));
                        view! { <span class="detail-muted">{t}</span> }
                    });
                    // The dirty files, one compact row each, capped so a huge
                    // tree doesn't bury the feed this panel is really for.
                    const FILE_CAP: usize = 12;
                    let mut rows: Vec<(String, &'static str, &'static str)> = Vec::new();
                    for f in &s.staged {
                        rows.push((f.path.clone(), "staged", ic.added));
                    }
                    for f in &s.unstaged {
                        rows.push((f.path.clone(), "modified", ic.modified));
                    }
                    for p in &s.untracked {
                        rows.push((p.clone(), "untracked", ic.untracked));
                    }
                    for p in &s.conflicted {
                        rows.push((p.clone(), "conflict", ic.conflict));
                    }
                    let overflow = rows.len().saturating_sub(FILE_CAP);
                    rows.truncate(FILE_CAP);
                    let files = rows
                        .into_iter()
                        .map(|(path, tag, glyph)| {
                            view! {
                                <div class="act-file">
                                    <span class="nf ctx-icon">{glyph}</span>
                                    <span class="act-file-path">{path}</span>
                                    <span class="act-pill">{tag}</span>
                                </div>
                            }
                        })
                        .collect_view();
                    let more = (overflow > 0).then(|| {
                        view! {
                            <div class="detail-muted act-file">
                                {format!("… and {overflow} more")}
                            </div>
                        }
                    });
                    view! {
                        <div class=class>
                            <span class="nf ctx-icon">{glyph}</span>
                            {headline}
                            {sync}
                        </div>
                        {files}
                        {more}
                    }
                })
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
                    .map(|e| activity_row(e, nerd_icons, menu))
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
                                on:click=move |_| reload.update(|n| *n = n.wrapping_add(1))
                            >
                                "Refresh"
                            </button>
                            <button
                                class="detail-close"
                                title="Close"
                                on:click=move |_| activity_open.set(false)
                            >
                                "×"
                            </button>
                        </span>
                    </div>
                    <div class="detail-body">
                        {status_section}
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

/// One feed row. Tapping it opens the shared context menu on the commit the
/// event references (its result, or — for a deletion — the tip that died).
/// Events that reference no commit render as plain, non-tappable rows.
fn activity_row(
    event: ActivityEvent,
    nerd_icons: RwSignal<bool>,
    menu: RwSignal<Option<MenuData>>,
) -> impl IntoView {
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

    // The commit this event is "about": where the ref ended up — or, for a
    // deletion (no new state), the tip that was deleted. A null oid (all
    // zeros, e.g. a creation's old side) never gets here: new_oid is the
    // created tip and deletions carry a real old_oid.
    let commit = event
        .new_oid
        .clone()
        .filter(|oid| !oid.bytes().all(|b| b == b'0'))
        .or_else(|| event.old_oid.clone().filter(|oid| !oid.bytes().all(|b| b == b'0')));

    let header = format!(
        "{}{}",
        kind_label(event.kind),
        event.ref_name.as_deref().map(|r| format!(" · {r}")).unwrap_or_default()
    );
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
    };

    match commit {
        Some(commit) => {
            let on_tap = move |ev: web_sys::MouseEvent| {
                // The same MenuData the graph's dots build — one menu, two
                // entry points. No GitHub link from here (the panel doesn't
                // carry the pushed-commit set, and a wrong link that 404s is
                // worse than a disabled item).
                menu.set(Some(MenuData {
                    commit: commit.clone(),
                    header: header.clone(),
                    x: clamp_menu_x(ev.client_x() as f64),
                    y: ev.client_y() as f64,
                    github_url: None,
                    github_label: "Open on GitHub",
                    create_label: "Create branch from this commit…",
                    is_head: false,
                    branches: Vec::new(),
                    is_branch: false,
                    repo_url: None,
                }));
            };
            view! {
                <button class="act-row" on:click=on_tap>
                    {row_body}
                </button>
            }
            .into_view()
        }
        None => view! { <div class="act-row act-row-static">{row_body}</div> }.into_view(),
    }
}
