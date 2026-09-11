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
//! and finger selection is hunk-granularity.
//!
//! **Per-line selection (#357).** A precise pointer (mouse or pen — ADR
//! 0011) gets its own per-line checkbox beside each rendered added/removed
//! line (`line_check`), calling `toggle_line`/`is_line_selected` directly —
//! the same click-only pattern as the hunk checkbox, just with no drag-select
//! (no idempotent per-line setter exists yet; nothing in this issue's scope
//! needed one). The roving-focused hunk header's own Shift+Enter/Space calls
//! `select_all_in_hunk` for every changed line in that hunk — mirroring
//! `blame_row`'s "Shift changes what the roving key means" idiom rather than
//! inventing a new one.
//!
//! **Keyboard access to one line (#770).** `ArrowRight` on a roving-focused
//! hunk header drills into that hunk's own line scope
//! (`crate::features::diff::selection::LineFocus`, a *nested* roving focus,
//! separate from #210's header-level `GraphFocus`); `ArrowUp`/`ArrowDown`
//! then move among only that hunk's changed lines, `Home`/`End` jump to the
//! first/last, `ArrowLeft`/`Escape` hand arrow keys back to the header
//! (resuming exactly where #210 left it), and `Enter`/`Space` toggles the
//! one focused line via `toggle_line` — distinct from both `toggle_hunk`
//! and Shift+Activate's whole-hunk `select_all_in_hunk`. See `LineFocus`'s
//! own doc comment for why this scope-nesting design was chosen over
//! flattening (`gv-tui`'s `panes/staging.rs`, this issue's own named prior
//! art) or overloading `GraphFocus`.
//!
//! Deliberately **not** wired here, and permanently out of scope for this
//! install (Tom, 2026-09-11 — no iPad, no Apple Pencil, Linux-only):
//! touch/Pencil-specific affordances. See
//! `docs/investigations/2026-09-11-issue-770-keyboard.md` for the full
//! decision record.
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
use crate::features::a11y::focus::GraphFocus;
use crate::features::diff::core::{
    labels_for_selectable_lines, preview_state, selectable_hunk_lines, selectable_hunks,
    stage_direction_copy, staging_actions, CompleteHunkLines, PreviewState, SelectableHunkLine,
};
use crate::features::diff::selection::{drag_range, DiffSelection, IncompleteHunk, LineFocus};
use crate::features::graph::core::{roving_row_key, KeyMods, RenderCtx, RowKey};
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

/// Move DOM focus to hunk `hunk_idx`'s changed-line checkbox at position
/// `ordinal` within that hunk's own changed-line list (#770's nested
/// `LineFocus` scope — see `crate::features::diff::selection::LineFocus`'s
/// doc for why this is a *second*, hunk-scoped index space rather than a
/// raw patch-line offset). Mirrors `focus_hunk` exactly, scoped one level
/// deeper by `data-line-hunk`/`data-line-ordinal`.
fn focus_line(hunk_idx: usize, ordinal: usize) {
    if let Some(el) = document()
        .query_selector(&format!(
            "[data-hunk-scope=\"staging\"][data-line-hunk=\"{hunk_idx}\"][data-line-ordinal=\"{ordinal}\"]"
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
    // Every non-context local line index in this hunk (#357), in the same
    // sorted order `LineFocus`'s own line cursor indexes into (#770) — what
    // Shift+Activate below hands to `select_all_in_hunk`, and what
    // `ArrowRight` below hands `line_focus.enter` as its `changed_len`.
    // Plain owned data, not a `StoredValue`: it's read-only inside the
    // `move` keydown closure, never re-fetched, so it needs no reactive
    // plumbing of its own.
    changed_lines: Vec<u32>,
    // #770: which hunk/line the *nested* line scope is drilled into, if
    // any. Distinct from `focus` (`GraphFocus`, #210's header-level roving
    // position) on purpose — see `LineFocus`'s module doc for why arrow
    // keys belong to exactly one of the two at a time, never both.
    line_focus: RwSignal<LineFocus>,
) -> View {
    // While the nested line scope owns the one Tab stop, every header is
    // outside the Tab order. Landing on a header exits that scope below.
    let tabindex = move || {
        if focus.with(|f| f.tabbable_row()) == Some(idx) && !line_focus.with(|lf| lf.is_engaged()) {
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
    let has_lines = !changed_lines.is_empty();
    let on_keydown = {
        let file = file.clone();
        move |ev: web_sys::KeyboardEvent| {
            // The nested scope owns navigation until a header landing or
            // Left/Escape exits it; never advance the outer cursor here.
            if line_focus.with_untracked(|lf| lf.is_engaged()) {
                return;
            }
            // Which key means what, and the modifier policy that bails a
            // press out entirely, is `features::graph::core::roving_row_key`'s
            // to say (#653, #660): the canvas's own row handler drives the
            // same focus model with the same keys and the same policy, and
            // both files are wasm-only, so each held a copy no host test
            // could reach. What each intent *does* here — toggling a hunk
            // rather than opening a menu — stays.
            let mods = KeyMods {
                shift: ev.shift_key(),
                ctrl: ev.ctrl_key(),
                meta: ev.meta_key(),
                alt: ev.alt_key(),
            };
            // #770: `ArrowRight` drills into this hunk's line scope — decided
            // *before* `roving_row_key` (which has no opinion on
            // `ArrowRight` at all, and never will: that map is shared with
            // the canvas and blame rows, neither of which has a nested
            // scope to enter). Same bail-out policy as every other roving
            // key (`mods.bails_out()`, via `KeyMods`) so Ctrl/Cmd/Alt+Right
            // still reaches the browser. A hunk with no changed lines
            // refuses to engage (`LineFocus::enter` returns `false`) and
            // this falls through to the normal roving-key handling below,
            // so `ArrowRight` on such a hunk is simply not consumed here.
            if ev.key() == "ArrowRight" && !mods.bails_out() {
                let entered = line_focus
                    .try_update(|lf| lf.enter(idx, changed_lines.len()))
                    .unwrap_or(false);
                if entered {
                    ev.prevent_default();
                    ev.stop_propagation();
                    focus_line(idx, 0);
                    return;
                }
            }
            let Some(intent) = roving_row_key(&ev.key(), mods) else {
                return;
            };
            ev.prevent_default();
            ev.stop_propagation();
            match intent {
                RowKey::Move(dir) => {
                    if let Some(next) = focus.try_update(|f| f.mv(dir)).flatten() {
                        focus_hunk(next);
                    }
                }
                RowKey::Dismiss => {
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
                //
                // Shift+Activate (#357) is a *different* transition, not a
                // modifier on the same one: `select_all_in_hunk` narrows the
                // hunk to its explicit changed-line set (`Lines`) rather than
                // selecting it whole (`None`/`Hunks`) — the same "Shift
                // changes what the roving key means" idiom `blame_row` already
                // uses for its own Shift+Arrow range-extend, applied to this
                // surface's own Activate instead of Move. This reads `mods.shift`
                // (already captured above for `roving_row_key`) rather than
                // calling `ev.shift_key()` again — `roving_row_key` alone still
                // decides whether this press is handled at all (#660); this is
                // only choosing between two actions once it already has said
                // yes. ArrowRight enters the separate one-line keyboard scope.
                RowKey::Activate => {
                    if mods.shift {
                        selection.update(|s| {
                            s.select_all_in_hunk(&file, anchor, changed_lines.iter().copied())
                        });
                    } else {
                        selection.update(|s| s.toggle_hunk(&file, anchor));
                    }
                }
            }
        }
    };
    let on_click = move |_| {
        line_focus.update(|lf| lf.exit());
        focus.update(|f| f.focus_landed(idx));
        focus_hunk(idx);
    };
    let on_focus = move |_| {
        line_focus.update(|lf| lf.exit());
        focus.update(|f| f.focus_landed(idx));
    };
    let header_label = {
        let label = label.clone();
        move || {
            if line_focus.with(|lf| lf.active().map(|(hunk, _)| hunk)) == Some(idx) {
                format!("{label}. Line scope entered. ArrowLeft or Escape returns to the hunk.")
            } else if has_lines {
                format!("{label}. ArrowRight enters line scope; ArrowLeft or Escape returns to the hunk.")
            } else {
                label.clone()
            }
        }
    };
    let header = view! {
        <span
            class="stage-hunk-text"
            role="group"
            data-hunk-scope="staging"
            data-hunk-index=idx.to_string()
            tabindex=tabindex
            aria-label=header_label
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

/// One selectable line's own tap target (#357) — a per-line checkbox beside
/// an added/removed line, mirroring `hunk_row`'s own check button and
/// `blame_row`'s `.blame-select`. Click toggles; no drag-select — no
/// idempotent per-line setter exists yet (`set_hunk_selected`'s doc explains
/// why `toggle_hunk`/`toggle_line` are wrong for a drag gesture), and nothing
/// in this issue's scope needed one built.
///
/// `tabindex` is *reactive* here (#770, unlike #357's original permanent
/// `"-1"`): `"0"` only while `LineFocus` is drilled into exactly this line,
/// `"-1"` otherwise — the nested roving-tabindex contract `LineFocus`'s own
/// module doc describes, one tab stop at a time the same way `hunk_row`'s
/// header list already works.
// #770: `hunk_idx`/`ordinal`/`changed_len`/`line_focus` are the nested-scope
// wiring — everything else is unchanged from #357. `ordinal` is this line's
// position in the hunk's own `changed_lines` list (the same sorted order
// `hunk_row`'s `changed_lines` param and `LineFocus`'s line cursor share),
// not `local` (the raw index into the hunk's parsed `Hunk::lines` that
// `toggle_line`/`is_line_selected` address) — two different coordinate
// spaces on purpose, per `LineFocus`'s own doc comment.
#[allow(clippy::too_many_arguments)]
fn line_check(
    file: String,
    label: String,
    anchor: HunkRef,
    hunk_idx: usize,
    ordinal: usize,
    local: u32,
    changed_len: usize,
    focus: RwSignal<GraphFocus>,
    line_focus: RwSignal<LineFocus>,
    selection: RwSignal<DiffSelection>,
) -> View {
    // Two independent closures, same reason `hunk_row`'s
    // `checked_for_pressed`/`checked_for_glyph` split does: `file` isn't
    // `Copy`, so one closure value can't be called from two `move` sites.
    let checked_for_pressed = {
        let file = file.clone();
        move || selection.with(|s| s.is_line_selected(&file, anchor.index, local))
    };
    let checked_for_glyph = {
        let file = file.clone();
        move || selection.with(|s| s.is_line_selected(&file, anchor.index, local))
    };
    // #770: this line is in the Tab sequence (`tabindex="0"`) only while
    // `LineFocus` is actually drilled into it — every other line stays
    // `tabindex="-1"`, the same one-tab-stop-at-a-time roving-tabindex
    // contract `hunk_row`'s own `tabindex` closure keeps for headers.
    let tabindex = move || {
        if line_focus.with(|lf| lf.active()) == Some((hunk_idx, ordinal)) {
            "0"
        } else {
            "-1"
        }
    };
    let on_click = {
        let file = file.clone();
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            selection.update(|s| s.toggle_line(&file, anchor, local));
        }
    };
    let on_pointer_down = move |_: web_sys::PointerEvent| {
        if line_focus
            .try_update(|lf| lf.enter_at(hunk_idx, ordinal, changed_len))
            .unwrap_or(false)
        {
            focus.update(|f| f.focus_landed(hunk_idx));
        }
    };
    // #770's own keyboard path onto this exact line. `ArrowUp`/`ArrowDown`/
    // `Home`/`End`/`Enter`/`Space` are asked of `roving_row_key` — the same
    // single source of truth `hunk_row`'s own header keydown already defers
    // to (#653/#660) — rather than re-matched here, so this scope's keys can
    // never drift from the header scope's. `ArrowLeft` is the one press
    // `roving_row_key` has no opinion on (nothing else it drives has a
    // nested scope to leave), so it is decided here, under the same
    // `mods.bails_out()` policy `roving_row_key` itself applies.
    // `RowKey::Move` moves `LineFocus`'s own cursor among this hunk's
    // changed lines (never `focus`/`GraphFocus` — that stays wherever the
    // header left it, per `LineFocus`'s doc); `RowKey::Dismiss` (Escape) and
    // `ArrowLeft` both hand arrow keys back to the header; `RowKey::Activate`
    // toggles *this one line* — genuinely new keyboard access `toggle_hunk`
    // and Shift+Activate's whole-hunk `select_all_in_hunk` do not provide.
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let mods = KeyMods {
            shift: ev.shift_key(),
            ctrl: ev.ctrl_key(),
            meta: ev.meta_key(),
            alt: ev.alt_key(),
        };
        if ev.key() == "ArrowLeft" && !mods.bails_out() {
            ev.prevent_default();
            ev.stop_propagation();
            line_focus.update(|lf| lf.exit());
            focus_hunk(hunk_idx);
            return;
        }
        let Some(intent) = roving_row_key(&ev.key(), mods) else {
            return;
        };
        ev.prevent_default();
        ev.stop_propagation();
        match intent {
            RowKey::Move(dir) => {
                if let Some(next) = line_focus
                    .try_update(|lf| lf.mv(dir, changed_len))
                    .flatten()
                {
                    focus_line(hunk_idx, next);
                }
            }
            RowKey::Dismiss => {
                line_focus.update(|lf| lf.exit());
                focus_hunk(hunk_idx);
            }
            RowKey::Activate => {
                selection.update(|s| s.toggle_line(&file, anchor, local));
            }
        }
    };
    view! {
        <button
            type="button"
            class="stage-line-check"
            data-hunk-scope="staging"
            data-line-hunk=hunk_idx.to_string()
            data-line-ordinal=ordinal.to_string()
            tabindex=tabindex
            aria-pressed=move || checked_for_pressed().to_string()
            aria-label=label
            on:click=on_click
            on:pointerdown=on_pointer_down
            on:keydown:undelegated=on_keydown
        >
            {move || if checked_for_glyph() { "\u{2713}" } else { "" }}
        </button>
    }
    .into_view()
}

/// `line_check` for raw line `i`, or `None` when `i` isn't a line inside a
/// selectable hunk — a combined-diff body, or (patch truncation) a line past
/// what its hunk header declared. A plain function taking `line_coords` by
/// reference, not a closure: called from two match arms in the same `.map`
/// over owned data (`selection`, `hunks_by_flat_idx`) that closures would
/// otherwise have to clone at each call site, the same reasoning
/// `staging_body`'s own `build_plan` free function documents.
#[allow(clippy::too_many_arguments)]
fn line_checkbox_for(
    i: usize,
    line_coords: &HashMap<usize, SelectableHunkLine>,
    line_labels: &HashMap<usize, String>,
    hunks_by_flat_idx: StoredValue<Vec<(String, HunkRef)>>,
    // #770: `(hunk_idx, local) -> ordinal` — this line's position in its
    // hunk's own changed-line list, the coordinate `LineFocus`'s cursor and
    // `hunk_row`'s `changed_lines` param share. Built once by the caller
    // (`staging_patch_view`) from the same `changed_by_hunk` it already
    // built for `select_all_in_hunk`'s Shift+Activate path, not re-derived
    // per line.
    line_ordinals: &HashMap<(usize, u32), usize>,
    changed_lens: &[usize],
    focus: RwSignal<GraphFocus>,
    line_focus: RwSignal<LineFocus>,
    selection: RwSignal<DiffSelection>,
) -> Option<View> {
    let coord = line_coords.get(&i)?;
    let (file, anchor) = hunks_by_flat_idx.with_value(|all| all.get(coord.hunk_idx).cloned())?;
    let ordinal = *line_ordinals.get(&(coord.hunk_idx, coord.local))?;
    let changed_len = *changed_lens.get(coord.hunk_idx)?;
    Some(line_check(
        file,
        line_labels.get(&i)?.clone(),
        anchor,
        coord.hunk_idx,
        ordinal,
        coord.local,
        changed_len,
        focus,
        line_focus,
        selection,
    ))
}

/// The staging patch, rendered with the selection UI. Still a raw-text
/// line-by-line walk (same `diff_line_class` colouring the surfaces shared
/// before #361), deliberately separate from `detail::accessible_rows_window`:
/// selection anchors (`HunkRef`) are keyed on `selectable_hunks`' line-index
/// coordinate space (#215), and that walk is this view's own contract. Only
/// the spoken labels come from the structured path now.
fn staging_patch_view(
    patch: &str,
    focus: RwSignal<GraphFocus>,
    selection: RwSignal<DiffSelection>,
    line_focus: RwSignal<LineFocus>,
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
    // Per-line coordinates (#357): every hunk body line, keyed by its raw
    // `patch.lines()` index, to (flat hunk index, local index into that
    // hunk's own `Hunk::lines`) — the same walk `selectable_hunks` itself is
    // built on (`core::walk_hunks`), so the two can never disagree about
    // where a hunk starts.
    let line_coords: HashMap<usize, SelectableHunkLine> = selectable_hunk_lines(patch);
    let line_labels = labels_for_selectable_lines(patch, &hunks, &line_coords);
    // Every hunk's own non-context (addable/removable) local line indices,
    // flat-indexed the same way `hunks_by_flat_idx` is — what Shift+Activate
    // on a hunk header hands `select_all_in_hunk` (see `hunk_row`). Built
    // once here rather than re-derived per keypress: this is exactly the
    // "explicit set the caller must enumerate" `select_all_in_hunk`'s own
    // doc comment says the pure `selection` module cannot know on its own.
    let mut changed_by_hunk: Vec<Vec<u32>> = vec![Vec::new(); hunks.len()];
    for coord in line_coords.values() {
        if coord.kind != git_vista_protocol::diff::LineKind::Context {
            if let Some(v) = changed_by_hunk.get_mut(coord.hunk_idx) {
                v.push(coord.local);
            }
        }
    }
    for v in &mut changed_by_hunk {
        v.sort_unstable();
    }
    // #770: the inverse of `changed_by_hunk` — `(hunk_idx, local) ->
    // ordinal` — so each rendered line checkbox can find its own position
    // in the hunk's changed-line list without re-deriving it per line.
    let line_ordinals: HashMap<(usize, u32), usize> = changed_by_hunk
        .iter()
        .enumerate()
        .flat_map(|(hunk_idx, lines)| {
            lines
                .iter()
                .enumerate()
                .map(move |(ordinal, &local)| ((hunk_idx, local), ordinal))
        })
        .collect();
    let changed_lens: Vec<usize> = changed_by_hunk.iter().map(Vec::len).collect();
    // The spoken VoiceOver labels, from the structured path (#361): the same
    // `hunk_label` text the detail panel and viewer speak, paired with
    // `selectable_hunks` by (file, per-file ordinal) — NOT by position,
    // because the two walks are asymmetric by construction (the structured
    // parser needs a `diff --git` section, the raw walk does not) and a
    // positional zip would let one dropped file shift every later label onto
    // the wrong checkbox, or fall back to an empty aria-label (review
    // findings). `labels_for_selectable_hunks` returns exactly one label per
    // entry of `hunks`, in the same order; a hunk the parser cannot see gets
    // an honest raw-walk fallback, never silence. Pinned host-side by
    // `selectable_labels_pair_by_file_and_ordinal_even_when_the_parsers_disagree`.
    let labels: Vec<String> = crate::features::diff::rows::labels_for_selectable_hunks(
        &git_vista_protocol::diff::parse_unified_diff(patch),
        &hunks,
    );
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
                    // Index-aligned by construction: `labels_for_selectable_hunks`
                    // maps over `hunks`, so `labels.len() == hunks.len()` always.
                    let label = labels[idx].clone();
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
                        changed_by_hunk.get(idx).cloned().unwrap_or_default(),
                        line_focus,
                    )
                }
                None if class == "diff-hunk" => {
                    view! { <span class="diff-hunk-combined">{text}</span> }.into_view()
                }
                None if class == "diff-add" => {
                    let check = line_checkbox_for(
                        i,
                        &line_coords,
                        &line_labels,
                        hunks_by_flat_idx,
                        &line_ordinals,
                        &changed_lens,
                        focus,
                        line_focus,
                        selection,
                    );
                    view! {
                        <span class=class>
                            {check}
                            <span class="sr-only">"added line: "</span>
                            {text}
                        </span>
                    }
                    .into_view()
                }
                None if class == "diff-del" => {
                    let check = line_checkbox_for(
                        i,
                        &line_coords,
                        &line_labels,
                        hunks_by_flat_idx,
                        &line_ordinals,
                        &changed_lens,
                        focus,
                        line_focus,
                        selection,
                    );
                    view! {
                        <span class=class>
                            {check}
                            <span class="sr-only">"removed line: "</span>
                            {text}
                        </span>
                    }
                    .into_view()
                }
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
    // The verb and the flow arrows come from core (#653): which of the two
    // diffs the coordinates address is a fact about the direction, and this
    // file is wasm-only, so a copy kept here is unreachable from every test.
    let (action_word, flow) = stage_direction_copy(direction);
    let generation = d.generation.clone();
    let whole_hunk_lines = store_value(CompleteHunkLines::from_patch(&d.patch));
    let plan_error = create_rw_signal(None::<String>);
    create_effect(move |_| {
        selection.track();
        plan_error.set(None);
    });
    // #770: created here rather than threaded in from `viewer.rs` the way
    // `hunk_focus`/`selection` are (owned by the caller so an in-flight
    // Preview/Apply survives a re-render, per this function's own doc
    // above). `viewer.rs` is outside this issue's allowed surface, so
    // `line_focus` resets to disengaged on every fresh `staging_body` call
    // (a diff refetch — e.g. after Apply) instead of surviving one the way
    // `hunk_focus` does. That is an acceptable loss for ephemeral keyboard
    // focus (unlike `selection`, it carries no user intent to stage
    // anything), but it is a real, deliberate gap: threading it through
    // `viewer.rs` properly is a follow-up for whoever owns that file.
    let line_focus = create_rw_signal(LineFocus::new());
    let patch = staging_patch_view(&d.patch, hunk_focus, selection, line_focus);
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
        whole_hunk_lines: StoredValue<CompleteHunkLines>,
    ) -> Result<Option<PatchPlan>, IncompleteHunk> {
        let Some((repository, worktree)) = repo_tokens(ctx) else {
            return Ok(None);
        };
        whole_hunk_lines.with_value(|lines| {
            selection.with(|s| {
                s.to_patch_plan(repository, worktree, generation.clone(), direction, lines)
            })
        })
    }

    let on_preview = {
        let generation = generation.clone();
        move |_| {
            let plan = match build_plan(ctx, selection, &generation, direction, whole_hunk_lines) {
                Ok(Some(plan)) => plan,
                Ok(None) => return,
                Err(error) => {
                    plan_error.set(Some(error.to_string()));
                    return;
                }
            };
            plan_error.set(None);
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
    let preview_showable = {
        let generation = generation.clone();
        move || {
            preview_state(
                previewed_plan.get().as_ref(),
                build_plan(ctx, selection, &generation, direction, whole_hunk_lines)
                    .ok()
                    .flatten()
                    .as_ref(),
            )
        }
    };

    let on_apply = move |_| {
        let plan = match build_plan(ctx, selection, &generation, direction, whole_hunk_lines) {
            Ok(Some(plan)) => plan,
            Ok(None) => return,
            Err(error) => {
                plan_error.set(Some(error.to_string()));
                return;
            }
        };
        plan_error.set(None);
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
    let gate = move || staging_actions(selection.with(|s| s.is_empty()), busy.get(), !no_identity);
    // Three states, three renderings, and `PreviewState` is what tells them
    // apart (#653). The `Stale` arm is the #215 review finding: Apply was
    // always correct — it rebuilds the plan from the live selection at click
    // time — but the panel was lying about what that would be, leaving the
    // previous patch text on screen after a hunk was toggled. Showing the
    // notice on `NotRequested` too would put "selection changed" on a view
    // nobody has previewed yet.
    let preview_view = move || match preview_showable() {
        PreviewState::NotRequested => None,
        PreviewState::Stale => Some(
            view! {
                <p class="detail-status">
                    "Selection changed since this preview — press Preview \
                     again to see the current patch."
                </p>
            }
            .into_view(),
        ),
        PreviewState::Fresh => preview.get().map(|r| match r {
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
        }),
    };

    view! {
        <div class="viewer-doc-head">
            {format!("{action_word} selected changes — {flow}")}
        </div>
        {no_identity.then(|| view! {
            <p class="detail-status detail-error">
                "This repository has no identity assigned yet — staging selection \
                 isn't available for this view."
            </p>
        })}
        <div class="stage-actions">
            // Which buttons are live is `staging_actions`' to say (#653). The
            // asymmetry it holds is the point: Clear needs only a selection,
            // because clearing is local — a request in flight is no reason to
            // trap the user with a selection they have decided against, and a
            // repository with no identity is exactly the state where clearing
            // is the only useful thing left.
            <button
                class="viewer-btn"
                prop:disabled=move || !gate().preview
                on:click=on_preview
            >
                "Preview"
            </button>
            <button
                class="viewer-btn"
                prop:disabled=move || !gate().apply
                on:click=on_apply
            >
                {format!("{action_word} Selected")}
            </button>
            <button
                class="viewer-btn"
                prop:disabled=move || !gate().clear
                on:click=move |_| selection.update(|s| s.clear())
            >
                "Clear selection"
            </button>
        </div>
        {move || plan_error.get().map(|error| view! {
            <p class="detail-status detail-error" role="alert">{error}</p>
        })}
        {preview_view}
        {truncated_note}
        <pre class="detail-diff viewer-pre">{patch}</pre>
    }
    .into_view()
}
