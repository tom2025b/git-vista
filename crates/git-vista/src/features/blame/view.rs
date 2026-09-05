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
) -> View {
    let state_banner = path_state_message(&page.path_state);

    if !matches!(page.path_state, PathState::Readable) {
        return view! {
            <div class="blame-panel">
                <p class="detail-status">
                    {state_banner.unwrap_or_else(|| "Nothing to show.".to_string())}
                </p>
                {history_list(history, shell)}
            </div>
        }
        .into_view();
    }

    focus.update_untracked(|f| f.set_row_count(page.ranges.len()));
    let ranges = page.ranges.clone();
    let drag_anchor: StoredValue<Option<usize>> = store_value(None);

    let rename_banner = rename_limit_banner(&page.rename_limit_hits)
        .map(|b| view! { <p class="detail-status blame-rename-warning">{b}</p> }.into_view());

    let rows: Vec<View> = ranges
        .iter()
        .cloned()
        .enumerate()
        .map(|(idx, range)| blame_row(idx, range, focus, selection, drag_anchor, shell))
        .collect();

    // The compare toolbar: visible only while a selection exists, offering
    // whatever `offer_for` says given the current anchor and the commit that
    // owns the FIRST line of the selected range — a range can span more than
    // one commit, and "the commit at the top of what you selected" is the
    // one unambiguous choice for what "compare from here" means.
    let ranges_for_toolbar = page.ranges.clone();
    let toolbar = move || {
        let Some(range) = selection.with(|s| s.range()) else {
            return ().into_view();
        };
        let start = *range.start();
        let Some(this) = ranges_for_toolbar
            .iter()
            .find(|r| r.start_line <= start && start <= r.end_line)
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
            {history_list(history, shell)}
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
fn history_list(history: Option<&Result<FileHistoryPage, String>>, shell: Shell) -> View {
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
            view! {
                <div class="blame-history">
                    <h3 class="detail-heading">"File history"</h3>
                    <ul class="blame-history-list">{rows}</ul>
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
fn blame_row(
    idx: usize,
    range: BlameRange,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<BlameSelection>,
    drag_anchor: StoredValue<Option<usize>>,
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

    let on_select_pointer_down = move |_: web_sys::PointerEvent| {
        drag_anchor.set_value(Some(idx));
        selection.update(|s| s.start(idx));
    };
    let on_select_pointer_enter = move |ev: web_sys::PointerEvent| {
        if ev.buttons() != 1 {
            return;
        }
        if drag_anchor.get_value().is_some() {
            selection.update(|s| s.extend_to(idx));
        }
    };
    let on_select_pointer_up = move |_: web_sys::PointerEvent| drag_anchor.set_value(None);
    let on_select_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        selection.update(|s| {
            if s.contains(idx) && s.range() == Some(idx..=idx) {
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
