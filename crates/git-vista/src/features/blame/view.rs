//! The blame panel's DOM wiring (M5.33, #86) — wasm-only, thin by design
//! (ADR 0115): every decision it renders comes from `core.rs` or from the
//! house machinery `features/diff/selection.rs` and `features/graph/core.rs`
//! already proved out for staging and the commit-compare menu. This module's
//! own job is exactly the touch/pointer/keyboard event glue, mirroring
//! `features/diff/staging_view.rs`'s hunk-row shape: one 44px "select" tap
//! target for range selection (drag-extendable, `BlameSelection`), kept
//! separate from the row's own roving-tabindex body (click/Enter opens the
//! commit detail panel) — the same "two targets, never one tap meaning two
//! things" rule that module's doc states.

use leptos::*;
use wasm_bindgen::JsCast;

use git_vista_protocol::blame::{BlamePage, BlameRange, FileHistoryPage, PathState};
use git_vista_protocol::diff::{ComparisonBasis, DiffSpec};
use git_vista_protocol::CommitOid;

use crate::features::a11y::focus::GraphFocus;
use crate::features::blame::core::{path_state_message, rename_limit_banner, BlameSelection};
use crate::features::graph::core::{offer_for, roving_row_key, CompareOffer, RowKey};
use crate::features::shell::signals::Shell;
use crate::state::ViewerDoc;

/// The conventional 7-char short id, matching `menu::compare_items::short`.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

fn focus_blame_row(idx: usize) {
    if let Some(el) = document()
        .query_selector(&format!("[data-blame-row=\"{idx}\"]"))
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// The whole panel: a banner for a non-`Readable` path, a rename-limit
/// warning when the walk hit one, and the ranges themselves — or, for a
/// refused path (binary or absent), the banner alone with no row list at
/// all, since there is nothing line-shaped to show.
pub fn blame_body(
    page: &BlamePage,
    history: Option<&Result<FileHistoryPage, String>>,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<BlameSelection>,
    shell: Shell,
    compare_anchor: RwSignal<Option<String>>,
    blame_window: RwSignal<Option<(usize, usize)>>,
    history_skip: RwSignal<usize>,
) -> View {
    let state_banner = path_state_message(&page.path_state);

    if !matches!(page.path_state, PathState::Readable) {
        return view! {
            <div class="blame-panel">
                <p class="detail-status">
                    {state_banner.unwrap_or_else(|| "Nothing to show.".to_string())}
                </p>
                {history_list(history, shell, history_skip)}
            </div>
        }
        .into_view();
    }

    focus.update_untracked(|f| f.set_row_count(page.ranges.len()));
    let ranges = page.ranges.clone();
    let drag_anchor: StoredValue<Option<usize>> = store_value(None);
    let dragged: StoredValue<bool> = store_value(false);

    let rename_banner = rename_limit_banner(&page.rename_limit_hits)
        .map(|b| view! { <p class="detail-status blame-rename-warning">{b}</p> }.into_view());

    let rows: Vec<View> = ranges
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, range)| blame_row(idx, range, focus, selection, drag_anchor, dragged, shell))
        .collect();

    // The compare toolbar: visible only while a selection exists, offering
    // whatever `offer_for` says for the commit of the FIRST SELECTED ROW — a
    // selection can span rows belonging to several commits, and "the commit
    // at the top of what you selected" is the one unambiguous choice for what
    // "compare from here" means.
    let ranges_for_toolbar = page.ranges.clone();
    let toolbar = move || {
        let Some(range) = selection.with(|s| s.range()) else {
            return ().into_view();
        };
        // `range` is in ROW-INDEX space — `BlameSelection` stores the index
        // `enumerate()` handed each row, not a source line number. An earlier
        // version searched the ranges for one whose 1-based LINE interval
        // contained that index, which is a category error with two visible
        // symptoms: row 0 matched nothing (no line is numbered 0, so the
        // first row never offered a comparison at all) and later rows matched
        // whichever earlier range happened to span that small integer,
        // opening a comparison on the wrong commit. Index the slice.
        let Some(this) = ranges_for_toolbar
            .get(*range.start())
            .map(|r| r.commit.clone())
        else {
            return ().into_view();
        };
        let anchor = compare_anchor.get();
        let offer = offer_for(anchor.as_deref(), &this);
        let this_short = short(&this).to_string();
        match offer {
            CompareOffer::SetAnchor => {
                let this = this.clone();
                let on = move |_| compare_anchor.set(Some(this.clone()));
                view! {
                    <div class="blame-toolbar">
                        <button class="ctx-item" on:click=on>
                            {format!("Compare from '{this_short}'")}
                        </button>
                    </div>
                }
                .into_view()
            }
            CompareOffer::ClearAnchor => {
                let on = move |_| compare_anchor.set(None);
                view! {
                    <div class="blame-toolbar">
                        <button class="ctx-item" on:click=on>
                            "Clear comparison anchor"
                        </button>
                    </div>
                }
                .into_view()
            }
            CompareOffer::Compare { base, .. } => {
                let base_short = short(&base).to_string();
                let this = this.clone();
                let on = move |_| {
                    let (Ok(base), Ok(target)) =
                        (CommitOid::new(base.clone()), CommitOid::new(this.clone()))
                    else {
                        return;
                    };
                    shell.open_viewer(ViewerDoc::Spec {
                        spec: DiffSpec::CommitVsCommit {
                            base,
                            target,
                            basis: ComparisonBasis::Direct,
                        },
                    });
                };
                view! {
                    <div class="blame-toolbar">
                        <button class="ctx-item" on:click=on>
                            {format!("Compare with '{base_short}'")}
                        </button>
                    </div>
                }
                .into_view()
            }
        }
    };

    view! {
        <div class="blame-panel">
            {rename_banner}
            <div class="blame-rows" role="list" aria-label="Blame">
                {rows}
            </div>
            {toolbar}
            {blame_pager(page, blame_window)}
            {history_list(history, shell, history_skip)}
        </div>
    }
    .into_view()
}

/// Move the blame window (#86 review).
///
/// Before this the panel fetched one page and offered no way to ask for
/// another: the endpoint took a line range, the client API could encode one,
/// and nothing ever sent a second request — so any file longer than the
/// server's default page was silently truncated in the only surface a user
/// actually sees. `total_lines` is what makes the arithmetic honest; it is
/// the file's real length, not the page's.
fn blame_pager(page: &BlamePage, window: RwSignal<Option<(usize, usize)>>) -> View {
    let (start, end, total) = (page.start_line, page.end_line, page.total_lines);
    let span = end.saturating_sub(start) + 1;
    if total == 0 || (start <= 1 && end >= total) {
        // The whole file is on screen; a pager would be furniture.
        return ().into_view();
    }
    let prev = (start > 1).then(|| {
        let new_end = start - 1;
        let new_start = new_end.saturating_sub(span - 1).max(1);
        view! {
            <button class="ctx-item" on:click=move |_| window.set(Some((new_start, new_end)))>
                "◀ Earlier lines"
            </button>
        }
        .into_view()
    });
    let next = (end < total).then(|| {
        let new_start = end + 1;
        let new_end = (new_start + span - 1).min(total);
        view! {
            <button class="ctx-item" on:click=move |_| window.set(Some((new_start, new_end)))>
                "Later lines ▶"
            </button>
        }
        .into_view()
    });
    view! {
        <div class="blame-pager">
            {prev}
            <span class="blame-pager-at">{format!("lines {start}–{end} of {total}")}</span>
            {next}
        </div>
    }
    .into_view()
}

/// The rename-aware file-history list (`GET /api/file-history`'s first
/// page) — a plain, paged-underneath list, not roving-tabindex like the
/// blame rows above it: each entry is one native, focusable button (`<button
/// on:click>`), which is already in the tab order and already speaks its own
/// accessible name, so a second custom focus model would only duplicate what
/// the browser gives a `<button>` for free.
fn history_list(
    history: Option<&Result<FileHistoryPage, String>>,
    shell: Shell,
    skip: RwSignal<usize>,
) -> View {
    match history {
        None => view! { <p class="detail-status">"Loading history…"</p> }.into_view(),
        Some(Err(e)) => view! {
            <p class="detail-status detail-error">{format!("Couldn't load history: {e}")}</p>
        }
        .into_view(),
        Some(Ok(page)) => {
            if page.entries.is_empty() {
                return ().into_view();
            }
            let rows: Vec<View> = page
                .entries
                .iter()
                .map(|entry| {
                    let commit = entry.commit.clone();
                    let on = move |_| shell.open_detail(commit.clone(), false);
                    let renamed = entry.renamed_from.as_ref().map(|from| {
                        view! {
                            <span class="blame-history-renamed">
                                {format!("renamed from '{from}'")}
                            </span>
                        }
                        .into_view()
                    });
                    view! {
                        <li class="blame-history-row">
                            <button type="button" class="blame-history-commit" on:click=on>
                                {short(&entry.commit).to_string()}
                            </button>
                            <span class="blame-history-summary">{entry.summary.clone()}</span>
                            {renamed}
                        </li>
                    }
                    .into_view()
                })
                .collect();
            // `cursor` is the server's own "there may be more past this"
            // answer; it was previously fetched and ignored.
            let has_more = page.cursor.is_some();
            let shown = page.entries.len();
            let pager = (has_more || skip.get() > 0).then(|| {
                let back = (skip.get() > 0).then(|| {
                    view! {
                        <button
                            class="ctx-item"
                            on:click=move |_| skip.update(|s| *s = s.saturating_sub(shown.max(1)))
                        >
                            "◀ Newer"
                        </button>
                    }
                    .into_view()
                });
                let forward = has_more.then(|| {
                    view! {
                        <button class="ctx-item" on:click=move |_| skip.update(|s| *s += shown)>
                            "Older ▶"
                        </button>
                    }
                    .into_view()
                });
                view! { <div class="blame-pager">{back}{forward}</div> }.into_view()
            });
            view! {
                <div class="blame-history">
                    <h3 class="detail-heading">"File history"</h3>
                    <ul class="blame-history-list">{rows}</ul>
                    {pager}
                </div>
            }
            .into_view()
        }
    }
}

/// One [`BlameRange`] row: a roving-tabindex body (click/Enter opens the
/// commit's detail panel — the "map blame ranges to commits" criterion) plus
/// its own 44px select target (drag-extendable via [`BlameSelection`] — the
/// "touch selection is accessible" criterion).
#[allow(clippy::too_many_arguments)]
fn blame_row(
    idx: usize,
    range: BlameRange,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<BlameSelection>,
    drag_anchor: StoredValue<Option<usize>>,
    // Set when a drag actually moved across rows, so the click that ends it
    // can tell itself apart from a tap. Shared with the panel, not per-row:
    // the click lands on whichever row the pointer was released over, which
    // is not the row the drag started on.
    dragged: StoredValue<bool>,
    shell: Shell,
) -> View {
    let tabindex = move || {
        if focus.with(|f| f.tabbable_row()) == Some(idx) {
            "0"
        } else {
            "-1"
        }
    };
    let selected = move || selection.with(|s| s.contains(idx));
    let commit = range.commit.clone();
    let label = format!(
        "Lines {}-{}, {} by {}: {}",
        range.start_line,
        range.end_line,
        short(&range.commit),
        range.author,
        range.summary
    );

    let on_activate = {
        let commit = commit.clone();
        move || shell.open_detail(commit.clone(), false)
    };
    let on_click = {
        let on_activate = on_activate.clone();
        move |_| {
            focus.update(|f| f.focus_landed(idx));
            on_activate();
        }
    };
    let on_focus = move |_| focus.update(|f| f.focus_landed(idx));
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.alt_key() || ev.ctrl_key() || ev.meta_key() {
            return;
        }
        let Some(intent) = roving_row_key(&ev.key()) else {
            return;
        };
        ev.prevent_default();
        ev.stop_propagation();
        match intent {
            RowKey::Move(dir) => {
                if let Some(next) = focus.try_update(|f| f.mv(dir)).flatten() {
                    if ev.shift_key() {
                        selection.update(|s| {
                            if s.is_empty() {
                                s.start(idx);
                            }
                            s.extend_to(next);
                        });
                    }
                    focus_blame_row(next);
                }
            }
            RowKey::Dismiss => {
                focus.update(|f| f.escape());
                selection.update(|s| s.clear());
                if let Some(el) = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                {
                    let _ = el.blur();
                }
            }
            RowKey::Activate => on_activate(),
        }
    };

    // One gesture, one meaning (#86 review). `pointerdown` used to call
    // `start(idx)` AND the click that inevitably follows used to toggle — so a
    // plain tap selected the row and then immediately cleared it, leaving the
    // control looking dead. The browser spec could not see it because it
    // dispatched pointer events directly and never the click a real tap
    // produces.
    //
    // Now `pointerdown` only ANCHORS (for a possible drag) and commits
    // nothing; `click` is the single place a tap's selection is decided, and
    // it fires for pointer and keyboard activation alike.
    let on_select_pointer_down = move |_: web_sys::PointerEvent| {
        drag_anchor.set_value(Some(idx));
    };
    // A drag: the first row entered while the pointer is still down starts the
    // selection at the anchor, and each subsequent one extends it. Starting
    // here rather than on `pointerdown` is what keeps a tap (down, up, no
    // movement) from committing anything before the click decides.
    let on_select_pointer_enter = move |ev: web_sys::PointerEvent| {
        if ev.buttons() != 1 {
            return;
        }
        let Some(anchor) = drag_anchor.get_value() else {
            return;
        };
        dragged.set_value(true);
        selection.update(|s| {
            if s.is_empty() {
                s.start(anchor);
            }
            s.extend_to(idx);
        });
    };
    let on_select_pointer_up = move |_: web_sys::PointerEvent| drag_anchor.set_value(None);
    let on_select_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        // A click that merely ends a drag must not re-decide what the drag
        // selected. Only a click with no drag behind it is a tap.
        if dragged.get_value() {
            dragged.set_value(false);
            return;
        }
        selection.update(|s| {
            if s.range() == Some(idx..=idx) {
                s.clear();
            } else {
                s.start(idx);
            }
        });
    };

    view! {
        <div class="blame-row-wrap">
            <button
                type="button"
                class="blame-select"
                tabindex="-1"
                aria-pressed=move || selected().to_string()
                aria-label=format!("Select line {} for comparison", range.start_line)
                on:click=on_select_click
                on:pointerdown=on_select_pointer_down
                on:pointerenter=on_select_pointer_enter
                on:pointerup=on_select_pointer_up
            >
                {move || if selected() { "\u{2713}" } else { "" }}
            </button>
            <span
                class="blame-row"
                role="group"
                data-blame-row=idx.to_string()
                tabindex=tabindex
                aria-label=label
                on:keydown:undelegated=on_keydown
                on:click=on_click
                on:focus=on_focus
            >
                <span class="blame-lines">
                    {if range.start_line == range.end_line {
                        format!("{}", range.start_line)
                    } else {
                        format!("{}-{}", range.start_line, range.end_line)
                    }}
                </span>
                <span class="blame-commit">{short(&range.commit).to_string()}</span>
                <span class="blame-author">{range.author.clone()}</span>
                <span class="blame-summary">{range.summary.clone()}</span>
            </span>
        </div>
    }
    .into_view()
}
