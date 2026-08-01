//! The staging selection view (M2.17d, #215): finger/keyboard hunk selection
//! over the live staging diff, wired to `/api/staging/{diff,preview,apply}`.
//! Rendered inside the existing full-screen viewer overlay
//! ([`crate::viewer`], `ViewerDoc::Staging`) rather than a new overlay kind —
//! it inherits that overlay's close/Escape/backdrop wiring for free.
//!
//! See [`crate::features::diff::selection`]'s module doc for the Task 1
//! design decisions this view implements: selection is an *always-on*
//! affordance layered on #210's roving hunk navigation (a checkbox beside
//! each hunk header, not a second meaning bolted onto the header's own tap),
//! finger/keyboard selection is hunk-granularity, and per-line (Pencil)
//! selection is intentionally left unwired here — the pure state for it
//! exists, but no control in this file calls `toggle_line`.
//!
//! **What is unverified from this box** (matching #210/#226/#242's honesty
//! pattern): the drag-select gesture below (pointerdown + pointerenter across
//! checkboxes) has no test coverage a browser-free box can run, and iOS
//! Safari's touch pointer-capture behaviour in particular is known to be
//! fussier than desktop's — see `gestures.rs`'s own tap-vs-drag dance for how
//! much tuning that took there. Queued for the iPad testbed pass alongside
//! the rest of this issue's DOM wiring.

use std::collections::HashMap;

use leptos::*;
use wasm_bindgen::JsCast;

use git_vista_protocol::{
    GenerationToken, HunkRef, PatchPlan, PatchPreview, RepositoryToken, StageDirection,
    StagingDiff, WorktreeToken,
};

use crate::api::{staging_apply_request, staging_preview_request};
use crate::detail::diff_line_class;
use crate::features::a11y::focus::{FocusMove, GraphFocus};
use crate::features::diff::core::selectable_hunks;
use crate::features::diff::selection::{drag_range, DiffSelection};
use crate::features::graph::core::RenderCtx;
use crate::features::shell::signals::Shell;
use crate::features::status::signals::StatusResource;

/// Move DOM focus to the staging view's hunk `idx`. Mirrors
/// `detail::focus_hunk`, scoped to this surface's own `data-hunk-scope`.
fn focus_hunk(idx: usize) {
    if let Some(el) = document()
        .query_selector(&format!(
            "[data-hunk-scope=\"staging\"][data-hunk-index=\"{idx}\"]"
        ))
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// The repository/worktree tokens the current view is scoped to, derived
/// from [`RenderCtx::frame`]'s ids (M2.17d, #215) — the same ids the
/// server's `checked_build` cross-checks a [`PatchPlan`] against. `None`
/// when either id is absent (a repository the server hasn't assigned one
/// to) or malformed (would fail [`RepositoryToken::new`]/[`WorktreeToken::new`]'s
/// token-shape validation) — staging is unavailable in that case, reported
/// rather than guessed at.
fn repo_tokens(ctx: StoredValue<RenderCtx>) -> Option<(RepositoryToken, WorktreeToken)> {
    ctx.with_value(|c| {
        let repo = c.frame.repo_id.as_deref()?;
        let wt = c.frame.worktree_id.as_deref()?;
        let repo = RepositoryToken::new(repo).ok()?;
        let wt = WorktreeToken::new(wt).ok()?;
        Some((repo, wt))
    })
}

/// One hunk header row: the roving-tabindex header span (keyboard/tap parity
/// with #210) plus its own selection checkbox, a separate 44px tap target
/// (Task 1: never overload the header's own tap-to-focus click).
#[allow(clippy::too_many_arguments)]
fn hunk_row(
    text: String,
    idx: usize,
    label: String,
    file: String,
    anchor: HunkRef,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<DiffSelection>,
    drag_anchor: StoredValue<Option<usize>>,
    hunks_by_flat_idx: StoredValue<Vec<(String, HunkRef)>>,
) -> View {
    let tabindex = move || {
        if focus.with(|f| f.tabbable_row()) == Some(idx) {
            "0"
        } else {
            "-1"
        }
    };
    // Two independent closures (not one shared `Fn` reused twice) — the
    // captured `file: String` is not `Copy`, so a single closure value can't
    // be called from two different sites without cloning it at each call.
    let checked_for_pressed = {
        let file = file.clone();
        move || selection.with(|s| s.is_hunk_selected(&file, anchor.index))
    };
    let checked_for_glyph = {
        let file = file.clone();
        move || selection.with(|s| s.is_hunk_selected(&file, anchor.index))
    };
    let on_keydown = {
        let file = file.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.alt_key() || ev.ctrl_key() || ev.meta_key() || ev.shift_key() {
                return;
            }
            match ev.key().as_str() {
                "ArrowDown" | "ArrowUp" | "Home" | "End" => {
                    let dir = match ev.key().as_str() {
                        "ArrowDown" => FocusMove::Next,
                        "ArrowUp" => FocusMove::Prev,
                        "Home" => FocusMove::First,
                        _ => FocusMove::Last,
                    };
                    ev.prevent_default();
                    ev.stop_propagation();
                    if let Some(next) = focus.try_update(|f| f.mv(dir)).flatten() {
                        focus_hunk(next);
                    }
                }
                "Escape" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    focus.update(|f| f.escape());
                    if let Some(el) = ev
                        .target()
                        .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                    {
                        let _ = el.blur();
                    }
                }
                // Keyboard/VoiceOver equivalence (Task 1): whatever the
                // checkbox's tap does, Space/Enter on the currently
                // roving-focused header does too.
                " " | "Enter" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    selection.update(|s| s.toggle_hunk(&file, anchor));
                }
                _ => {}
            }
        }
    };
    let on_click = move |_| {
        focus.update(|f| f.focus_landed(idx));
        focus_hunk(idx);
    };
    let on_focus = move |_| focus.update(|f| f.focus_landed(idx));
    let header = view! {
        <span
            class="stage-hunk-text"
            role="group"
            data-hunk-scope="staging"
            data-hunk-index=idx.to_string()
            tabindex=tabindex
            aria-label=label.clone()
            on:keydown:undelegated=on_keydown
            on:click=on_click
            on:focus=on_focus
        >
            {text}
        </span>
    };
    let check_label = format!("Select for staging: {label}");
    let on_check_click = {
        let file = file.clone();
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            selection.update(|s| s.toggle_hunk(&file, anchor));
        }
    };
    // Drag-select (Task 3): pointerdown records the flat-index anchor;
    // entering a later checkbox while the primary pointer is still down
    // extends the selection deterministically (`set_hunk_selected`, not
    // `toggle`) across the whole range — see `selection::drag_range` and
    // this module's doc for what's unverified here.
    let on_pointer_down = move |ev: web_sys::PointerEvent| {
        drag_anchor.set_value(Some(idx));
        let _ = ev.pointer_id();
    };
    let on_pointer_enter = move |ev: web_sys::PointerEvent| {
        if ev.buttons() != 1 {
            return;
        }
        let Some(anchor_idx) = drag_anchor.get_value() else {
            return;
        };
        if anchor_idx == idx {
            return;
        }
        let range = drag_range(anchor_idx, idx);
        hunks_by_flat_idx.with_value(|all| {
            for i in range {
                if let Some((f, h)) = all.get(i) {
                    selection.update(|s| s.set_hunk_selected(f, *h, true));
                }
            }
        });
    };
    let on_pointer_up = move |_: web_sys::PointerEvent| drag_anchor.set_value(None);
    view! {
        <div class="stage-hunk">
            <button
                type="button"
                class="stage-hunk-check"
                // A native <button> is focusable by default (tabIndex 0), so
                // without this every hunk would add its own Tab stop —
                // breaking #210's "one Tab stop for the whole patch" roving
                // invariant this module's own doc claims to preserve
                // (review finding). Keyboard users already have full
                // functional equivalence via the roving header's Space/Enter
                // toggle; this button exists purely as a pointer/tap target,
                // so it stays out of the Tab sequence entirely rather than
                // needing its own reactive tabindex.
                tabindex="-1"
                aria-pressed=move || checked_for_pressed().to_string()
                aria-label=check_label
                on:click=on_check_click
                on:pointerdown=on_pointer_down
                on:pointerenter=on_pointer_enter
                on:pointerup=on_pointer_up
            >
                {move || if checked_for_glyph() { "\u{2713}" } else { "" }}
            </button>
            {header}
        </div>
    }
    .into_view()
}

/// The staging patch, rendered with the selection UI. Structurally mirrors
/// `detail::accessible_patch_view` (same line-by-line walk, same
/// `diff_line_class` colouring) but is a separate function, not a refactor
/// of it — #210's function and its DOM contract are untouched.
fn staging_patch_view(
    patch: &str,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<DiffSelection>,
) -> View {
    let hunks = selectable_hunks(patch);
    focus.update_untracked(|f| f.set_row_count(hunks.len()));
    let hunks_by_flat_idx = store_value(
        hunks
            .iter()
            .map(|h| {
                (
                    h.file.clone(),
                    HunkRef {
                        index: h.ordinal,
                        old_start: h.old_start,
                        new_start: h.new_start,
                    },
                )
            })
            .collect::<Vec<_>>(),
    );
    let drag_anchor: StoredValue<Option<usize>> = store_value(None);
    // Reuse `hunk_nav`'s labels (the spoken VoiceOver text): both walks
    // visit the same headers in the same order (pinned by
    // `selectable_hunks_line_indices_match_hunk_nav_exactly`).
    let labels: Vec<String> = crate::features::diff::core::hunk_nav(patch)
        .into_iter()
        .map(|e| e.label)
        .collect();
    let mut nav_at: HashMap<usize, usize> = hunks
        .iter()
        .enumerate()
        .map(|(idx, h)| (h.line_index, idx))
        .collect();
    patch
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let class = diff_line_class(l);
            let text = format!("{l}\n");
            match nav_at.remove(&i) {
                Some(idx) => {
                    let h = &hunks[idx];
                    let anchor = HunkRef {
                        index: h.ordinal,
                        old_start: h.old_start,
                        new_start: h.new_start,
                    };
                    let label = labels.get(idx).cloned().unwrap_or_default();
                    hunk_row(
                        text,
                        idx,
                        label,
                        h.file.clone(),
                        anchor,
                        focus,
                        selection,
                        drag_anchor,
                        hunks_by_flat_idx,
                    )
                }
                None if class == "diff-hunk" => {
                    view! { <span class="diff-hunk-combined">{text}</span> }.into_view()
                }
                None if class == "diff-add" => view! {
                    <span class=class>
                        <span class="sr-only">"added line: "</span>
                        {text}
                    </span>
                }
                .into_view(),
                None if class == "diff-del" => view! {
                    <span class=class>
                        <span class="sr-only">"removed line: "</span>
                        {text}
                    </span>
                }
                .into_view(),
                None => view! { <span class=class>{text}</span> }.into_view(),
            }
        })
        .collect_view()
}

/// The staging view's whole body: the patch with selection checkboxes, and
/// the Preview → Apply flow. `preview`/`busy` are owned by the caller
/// (`viewer.rs`) the same way `hunk_focus`/`selection` are, so a re-render of
/// this body (e.g. the selection changing) does not reset in-flight state.
#[allow(clippy::too_many_arguments)]
pub fn staging_body(
    d: &StagingDiff,
    direction: StageDirection,
    hunk_focus: RwSignal<GraphFocus>,
    selection: RwSignal<DiffSelection>,
    preview: RwSignal<Option<Result<PatchPreview, String>>>,
    previewed_plan: RwSignal<Option<PatchPlan>>,
    busy: RwSignal<bool>,
    ctx: StoredValue<RenderCtx>,
    status: StatusResource,
    shell: Shell,
) -> View {
    let action_word = match direction {
        StageDirection::Stage => "Stage",
        StageDirection::Unstage => "Unstage",
    };
    let generation = d.generation.clone();
    let patch = staging_patch_view(&d.patch, hunk_focus, selection);
    let truncated_note = d.truncated.then(|| {
        view! {
            <p class="detail-status">
                "Patch truncated — this diff is larger than the panel shows; \
                 selections only address what's shown."
            </p>
        }
    });

    // A plain function, not a shared closure: `generation` (`GenerationToken`)
    // is `Clone` but not `Copy`, so a closure capturing it can't be called
    // from two independent `move` event handlers without cloning it fresh at
    // each call site anyway — a free function taking it by reference makes
    // that explicit instead of fighting the borrow checker over a shared
    // closure value.
    fn build_plan(
        ctx: StoredValue<RenderCtx>,
        selection: RwSignal<DiffSelection>,
        generation: &GenerationToken,
        direction: StageDirection,
    ) -> Option<PatchPlan> {
        let (repository, worktree) = repo_tokens(ctx)?;
        selection.with(|s| s.to_patch_plan(repository, worktree, generation.clone(), direction))
    }

    let on_preview = {
        let generation = generation.clone();
        move |_| {
            let Some(plan) = build_plan(ctx, selection, &generation, direction) else {
                return;
            };
            busy.set(true);
            preview.set(None);
            // Recorded at request time, not after the response lands: what
            // matters is which selection this preview answers, and that's
            // fixed the moment the request is built.
            previewed_plan.set(Some(plan.clone()));
            spawn_local(async move {
                let result = staging_preview_request(&plan).await;
                preview.set(Some(result));
                busy.set(false);
            });
        }
    };

    // True once the selection has changed since the shown preview was
    // requested (review finding, #215): without this, toggling a hunk after
    // Preview left the OLD patch text on screen with nothing to say Apply
    // would no longer match it — Apply itself was always correct (it builds
    // fresh from the current selection below), but the panel was lying about
    // what that would be. `preview_view` hides stale content instead of
    // rendering it, forcing a fresh Preview before the patch text is trusted
    // again.
    let preview_stale = {
        let generation = generation.clone();
        move || match previewed_plan.get() {
            None => false,
            Some(p) => build_plan(ctx, selection, &generation, direction).as_ref() != Some(&p),
        }
    };

    let on_apply = move |_| {
        let Some(plan) = build_plan(ctx, selection, &generation, direction) else {
            return;
        };
        busy.set(true);
        spawn_local(async move {
            match staging_apply_request(&plan).await {
                Ok(()) => {
                    selection.update(|s| s.clear());
                    preview.set(None);
                    previewed_plan.set(None);
                    status.refetch();
                    shell.close_viewer();
                }
                Err(e) => {
                    if let Some(w) = web_sys::window() {
                        let _ = w.alert_with_message(&format!(
                            "Couldn't {} selected changes:\n{e}",
                            action_word.to_lowercase()
                        ));
                    }
                }
            }
            busy.set(false);
        });
    };

    let no_identity = repo_tokens(ctx).is_none();
    let preview_view = move || {
        if preview_stale() {
            return Some(
                view! {
                    <p class="detail-status">
                        "Selection changed since this preview — press Preview \
                         again to see the current patch."
                    </p>
                }
                .into_view(),
            );
        }
        preview.get().map(|r| match r {
            Ok(p) => {
                let files_note = if p.whole_files.is_empty() {
                    view! {}.into_view()
                } else {
                    view! {
                        <p class="detail-status">
                            {format!("Whole files: {}", p.whole_files.join(", "))}
                        </p>
                    }
                    .into_view()
                };
                view! {
                    <div class="stage-preview">
                        {files_note}
                        <pre class="detail-diff viewer-pre">{p.patch.clone()}</pre>
                    </div>
                }
                .into_view()
            }
            Err(e) => view! {
                <p class="detail-status detail-error">{format!("Preview failed: {e}")}</p>
            }
            .into_view(),
        })
    };

    view! {
        <div class="viewer-doc-head">
            {format!(
                "{action_word} selected changes — {}",
                match direction {
                    StageDirection::Stage => "worktree → index",
                    StageDirection::Unstage => "index → HEAD",
                },
            )}
        </div>
        {no_identity.then(|| view! {
            <p class="detail-status detail-error">
                "This repository has no identity assigned yet — staging selection \
                 isn't available for this view."
            </p>
        })}
        <div class="stage-actions">
            <button
                class="viewer-btn"
                prop:disabled=move || selection.with(|s| s.is_empty()) || busy.get() || no_identity
                on:click=on_preview
            >
                "Preview"
            </button>
            <button
                class="viewer-btn"
                prop:disabled=move || selection.with(|s| s.is_empty()) || busy.get() || no_identity
                on:click=on_apply
            >
                {format!("{action_word} Selected")}
            </button>
            <button
                class="viewer-btn"
                prop:disabled=move || selection.with(|s| s.is_empty())
                on:click=move |_| selection.update(|s| s.clear())
            >
                "Clear selection"
            </button>
        </div>
        {preview_view}
        {truncated_note}
        <pre class="detail-diff viewer-pre">{patch}</pre>
    }
    .into_view()
}
