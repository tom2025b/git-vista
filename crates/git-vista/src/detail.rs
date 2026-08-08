//! The commit detail panel (Phase 10). Docked to the right, it shows one
//! commit's full detail — the whole message body and both the author and
//! committer signatures — fetched lazily by hash. The chrome (title + close)
//! shows the instant a commit is picked; the body reacts to the fetch: a
//! "Loading…" line, git's error, or the detail once it lands. Clicking a parent
//! hash re-points the panel at that parent, so you can walk up the history.
//!
//! Since the Activity/Undo feature (step 2) the panel also carries a
//! **Changes** section: the commit's per-file stat list and its unified patch,
//! fetched lazily from `/api/diff/{id}` alongside the detail. The menu's
//! "Show diff" item opens this same panel with the section scrolled into view.

use std::collections::HashMap;

use leptos::*;
use wasm_bindgen::JsCast;

use git_vista_core::status::ChangeKind;

use crate::api::fetch_diff;
use crate::datetime::local_timestamp;
use crate::features::a11y::focus::{FocusMove, GraphFocus};
use crate::features::diff::core::{
    hunk_nav, line_heights, render_window, scroll_to_reveal, LineWrap,
};
use git_vista_core::virtualize::CumulativeHeights;

/// One rendered diff line's height in CSS pixels: `.detail-diff`'s
/// `font-size: 0.78rem` × `line-height: 1.45` at a 16px root ≈ 18.1px.
///
/// A constant, not a measurement, and the tradeoff is deliberate: measuring
/// one rendered line would mean rendering before windowing (the cost
/// windowing exists to avoid) or a layout read on every scroll frame. Being
/// slightly wrong here shifts the window by a line or two at the edges, which
/// the overscan below absorbs — it cannot corrupt the mapping, because
/// `accessible_patch_window` keys every line on its own patch index rather
/// than on anything derived from this number.
const DIFF_LINE_PX: f64 = 18.1;

/// Extra lines rendered above and below the visible range, so a fast scroll
/// does not flash blank space for a frame before the next range is computed.
const DIFF_OVERSCAN: usize = 20;
use crate::features::graph::core::RenderCtx;
use crate::icons::{icon_set, GitIcons};
use crate::state::{DetailResource, Features, Settings, ViewerDoc};

/// CSS class for one line of the unified patch, keyed off its prefix. The
/// file/hunk headers are checked *before* the bare +/- so `+++`/`---` read as
/// metadata, not as a one-character change. Shared with the full-screen
/// viewer (viewer.rs), which colours the same patch text.
pub(crate) fn diff_line_class(line: &str) -> &'static str {
    if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("rename ")
        || line.starts_with("similarity ")
        || line.starts_with("copy ")
        || line.starts_with("Binary files")
    {
        "diff-meta"
    } else if line.starts_with("@@") {
        "diff-hunk"
    } else if line.starts_with('+') {
        "diff-add"
    } else if line.starts_with('-') {
        "diff-del"
    } else {
        ""
    }
}

/// The coloured patch with accessible hunk navigation (M2.16e, #210) — the
/// flat per-line rendering both diff surfaces use, with every ordinary hunk
/// header a roving-tabindex stop: one `Tab` stop for the whole patch,
/// ArrowUp/Down move between hunks, Home/End jump, Escape leaves the patch
/// without closing anything, and a finger/Pencil tap on any header moves the
/// roving position there ([`GraphFocus::focus_landed`]). Added/removed lines
/// get a screen-reader-only prefix so VoiceOver's touch exploration says what
/// changed instead of a bare "plus"/"minus".
///
/// The header spans carry `role="group"`, not `role="button"`: they navigate,
/// they do not activate — hunk *selection* is staging's business (M2.17), and
/// a button role would promise an action that does not exist yet. `group` is
/// a naming-permitted, non-interactive role, which a bare `<span>` is not: a
/// role-less span computes to `generic`, where ARIA 1.2 prohibits
/// `aria-label`, and WebKit is known to drop prohibited names. Whether
/// VoiceOver actually speaks the label on focus is *expected, not verified
/// from this box* — it is on the iPad testbed list.
///
/// Known keyboard-continuity gap (honest limit): if the patch re-renders
/// while DOM focus sits on a hunk header (walking to a parent commit), the
/// focused span unmounts and browser focus drops to `<body>`; the model's
/// clamped position survives, but the user re-enters the patch with Tab from
/// the top. Deliberate — restoring focus across a render needs the RAF dance
/// the graph uses, and #69e's structural rendering replaces this wiring.
///
/// `scope` keeps the detail panel's stops and the full-screen viewer's stops
/// distinct in DOM queries — both can be mounted at once.
///
/// This is the flat-rendering wiring; when #69e renders `ParsedPatch`
/// structurally (and virtualizes, at which point moving focus needs the
/// scroll-into-view-then-focus dance the graph's `focus_row_next_frame`
/// already does), the index-based focus model transfers and this function is
/// replaced. Scope note argued on #210.
pub(crate) fn accessible_patch_view(
    patch: &str,
    focus: RwSignal<GraphFocus>,
    scope: &'static str,
) -> View {
    accessible_patch_window(patch, focus, scope, None, None)
}

/// [`accessible_patch_view`], rendering only `window`'s slice of the patch
/// (M2.16g, #350) — `None` renders every line, which is what the full-screen
/// viewer still does while its wrapping heights stay unmeasured.
///
/// **The hunk-navigation contract does not change with the window.** The
/// focus model is told the *total* hunk count, not the windowed count, and
/// each rendered header keeps its **global** hunk index. A windowed count
/// would silently renumber every hunk as the user scrolls — "hunk 3 of 40"
/// becoming "hunk 1 of 2" — and clamp the focus model to whatever happens to
/// be on screen, which is precisely the navigation regression #350 warns
/// about. Scrolling a hunk into view before focusing it is the caller's job
/// (`features::diff::core::scroll_to_reveal`); this function's job is to keep
/// the indices stable while it happens.
pub(crate) fn accessible_patch_window(
    patch: &str,
    focus: RwSignal<GraphFocus>,
    scope: &'static str,
    window: Option<std::ops::Range<usize>>,
    reveal: Option<Callback<usize>>,
) -> View {
    let nav = hunk_nav(patch);
    // `update_untracked`: this runs while a render closure is already
    // executing; the tabindex closures created below read the fresh count
    // when they first run, so nothing needs the notification.
    focus.update_untracked(|f| f.set_row_count(nav.len()));
    let mut nav_at: HashMap<usize, (usize, String)> = nav
        .into_iter()
        .enumerate()
        .map(|(idx, e)| (e.line_index, (idx, e.label)))
        .collect();
    let range = window.unwrap_or(0..usize::MAX);
    patch
        .lines()
        .enumerate()
        // `filter` rather than `skip`/`take`: the enumeration index must stay
        // the patch's own line index, because that is the coordinate
        // `nav_at` — and #210's whole focus model — is keyed on.
        .filter(|(i, _)| range.contains(i))
        .map(|(i, l)| {
            let class = diff_line_class(l);
            let text = format!("{l}\n");
            match nav_at.remove(&i) {
                Some((idx, label)) => {
                    hunk_header_span(class, text, idx, label, focus, scope, reveal)
                }
                // An `@@` line that is not a navigation stop is a combined
                // (`@@@`) merge header — header-coloured but inert, so it
                // must not wear .diff-hunk's interactive styling (pointer
                // cursor, 44px band) and look tappable.
                None if class == "diff-hunk" => {
                    view! { <span class="diff-hunk-combined">{text}</span> }.into_view()
                }
                // The sr-only prefix is position:absolute and
                // user-select:none, so the visible text layout inside the
                // <pre> — and a selected-and-copied patch — is byte-identical.
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

/// One navigable hunk header span — see [`accessible_patch_view`].
fn hunk_header_span(
    class: &'static str,
    text: String,
    idx: usize,
    label: String,
    focus: RwSignal<GraphFocus>,
    scope: &'static str,
    reveal: Option<Callback<usize>>,
) -> View {
    // Exactly one header carries `tabindex="0"` — the roving stop. Reactive,
    // so every keydown/tap that moves the model retargets the tab stop.
    let tabindex = move || {
        if focus.with(|f| f.tabbable_row()) == Some(idx) {
            "0"
        } else {
            "-1"
        }
    };
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        // Leave modified chords (Cmd+ArrowDown scroll-to-end, Shift+Home
        // selection, …) to the browser — same posture as the window shortcut
        // handler in gestures.rs.
        if ev.alt_key() || ev.ctrl_key() || ev.meta_key() || ev.shift_key() {
            return;
        }
        let dir = match ev.key().as_str() {
            "ArrowDown" => FocusMove::Next,
            "ArrowUp" => FocusMove::Prev,
            "Home" => FocusMove::First,
            "End" => FocusMove::Last,
            "Escape" => {
                // Leave hunk navigation without closing anything: disengage
                // the model and move DOM focus off the header. Two-part
                // contract with the window Esc handler (gestures.rs): this
                // handler is attached *undelegated* — the listener sits on
                // the span itself, so `stop_propagation` genuinely halts the
                // event before it reaches the window (Leptos's default
                // delegation would run it AT the window, where sibling
                // listeners are immune to stop_propagation) — and the window
                // handler additionally skips `dismiss_top` when
                // `default_prevented` is set, so the panel stays open even
                // if delegation details shift. A second Escape, with focus
                // elsewhere, still closes it.
                ev.prevent_default();
                ev.stop_propagation();
                focus.update(|f| f.escape());
                if let Some(el) = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                {
                    let _ = el.blur();
                }
                return;
            }
            _ => return,
        };
        // Arrows must move hunk focus, not scroll the pane — scrolling still
        // follows the focused element via the browser's own focus handling.
        ev.prevent_default();
        ev.stop_propagation();
        if let Some(next) = focus.try_update(|f| f.mv(dir)).flatten() {
            focus_hunk_revealed(scope, next, reveal);
        }
    };
    // Tap parity: iOS Safari does not reliably focus a tabindexed span on
    // tap, so the click handler focuses it explicitly; `focus_landed` in the
    // focus handler then moves the roving position (idempotent when both
    // fire).
    let on_click = move |_| {
        focus.update(|f| f.focus_landed(idx));
        focus_hunk_revealed(scope, idx, reveal);
    };
    let on_focus = move |_| focus.update(|f| f.focus_landed(idx));
    view! {
        <span
            class=class
            role="group"
            data-hunk-scope=scope
            data-hunk-index=idx.to_string()
            tabindex=tabindex
            aria-label=label
            on:keydown:undelegated=on_keydown
            on:click=on_click
            on:focus=on_focus
        >
            {text}
        </span>
    }
    .into_view()
}

/// Move DOM focus to hunk `idx` in `scope`, **first scrolling it into the
/// rendered window if windowing has left it unmounted** (M2.16g, #350).
///
/// Before windowing, every patch line was in the DOM and a bare
/// `query_selector` + `focus()` always found its target. That premise died
/// with #350: a hunk outside the rendered slice does not exist as an element,
/// so the query returns `None` and focus silently goes nowhere — the
/// navigation regression #350 explicitly warns about, and one this function
/// would have shipped if `reveal` were not threaded in.
///
/// `reveal` is the caller's "make line `line` renderable" hook (it sets the
/// scroll position via `features::diff::core::scroll_to_reveal`). Because the
/// re-render that mounts the newly-revealed line happens *after* the current
/// tick, the focus call is deferred one animation frame — the scroll-then-RAF
/// dance the old doc comment was able to say it did not need.
fn focus_hunk_revealed(scope: &'static str, idx: usize, reveal: Option<Callback<usize>>) {
    let Some(reveal) = reveal else {
        // No windowing in this scope (the full-screen viewer renders every
        // line), so nothing can unmount underneath us and a direct focus is
        // both sufficient and side-effect-free.
        focus_hunk(scope, idx);
        return;
    };

    // Reveal FIRST, unconditionally — including when the element is already
    // mounted.
    //
    // The obvious optimisation here is to skip the reveal when `find_hunk`
    // already returns the element, and that optimisation is what broke #210.
    // "Mounted" is not "visible": the overscan mounts ~20 lines beyond the
    // viewport in each direction, so the next hunk is routinely present in the
    // DOM and *off-screen*. Calling `.focus()` on an off-screen element makes
    // the browser scroll it into view on our behalf — that scroll fires
    // `on_diff_scroll`, the window re-renders, and the freshly focused node is
    // replaced by a new one at the same index. Focus lands on the old node,
    // which no longer exists, so it falls back to `<body>` and every subsequent
    // arrow key is the container's native scrolling.
    //
    // Observed directly in the browser: `focusin` on the target hunk, then
    // `focusout` with `relatedTarget: null` one frame later, `scrollTop` 82 to
    // 598, `document.activeElement` BODY.
    //
    // Doing the scroll ourselves makes it deterministic and lets the re-render
    // happen before we assert focus, rather than because of it. `reveal` is a
    // no-op when the hunk is already fully visible (`scroll_to_reveal` returns
    // `None`), so this costs nothing in the case the old fast path optimised
    // for.
    reveal.call(idx);
    focus_hunk(scope, idx);
    // Re-assert after the frame commits. The reveal's re-render can replace the
    // node we just focused with a fresh element at the same index, and focus
    // does not survive that; `focus_hunk` is a query-by-index, so it finds
    // whichever node now holds the position. Focusing an already-focused
    // element is a no-op, so the common path pays only one wasted query.
    request_animation_frame(move || focus_hunk(scope, idx));
}

/// Move DOM focus to hunk `idx` in `scope`, by position rather than by holding
/// a node reference — deliberately, because the node at a given index is not
/// stable across a windowed re-render.
///
/// This comment used to claim that "every line of the flat rendering is
/// mounted, so this is a direct query + focus — no scroll-then-RAF dance like
/// the virtualized graph needs". That stopped being true when #350 landed and
/// the claim outlived the fact by long enough to send an investigation down the
/// wrong path. Callers that can be re-rendered must go through
/// [`focus_hunk_revealed`], which does exactly the dance this once said was
/// unnecessary.
fn focus_hunk(scope: &str, idx: usize) {
    if let Some(el) = document()
        .query_selector(&format!(
            "[data-hunk-scope=\"{scope}\"][data-hunk-index=\"{idx}\"]"
        ))
        .ok()
        .flatten()
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// Glyph + colour class for one changed file's kind, from the icon fields
/// defined for exactly this view (see icons.rs — added/modified/deleted/
/// renamed have waited for a diff surface since the icon system landed).
/// Shared with the full-screen viewer (viewer.rs).
pub(crate) fn file_change_marker(ic: &GitIcons, kind: ChangeKind) -> (&'static str, &'static str) {
    match kind {
        ChangeKind::Added => (ic.added, "file-added"),
        ChangeKind::Modified => (ic.modified, "file-modified"),
        ChangeKind::Deleted => (ic.deleted, "file-deleted"),
        ChangeKind::Renamed => (ic.renamed, "file-renamed"),
    }
}

/// Build the detail panel view. `detail` is the lazily-fetched commit keyed on
/// `detail_id`; `ctx` supplies the repo's GitHub base + pushed-commit set for the
/// "Open on GitHub" link.
pub fn detail_panel_view(
    features: Features,
    settings: Settings,
    detail: DetailResource,
    ctx: StoredValue<RenderCtx>,
) -> impl IntoView {
    let Features { shell, .. } = features;
    let nerd_icons = settings.nerd_icons;
    // The commit's diff (file list + patch), fetched lazily alongside the
    // detail and keyed on the same open hash — so walking to a parent
    // re-fetches both, and closing the panel idles both.
    let diff = create_local_resource(
        move || shell.detail_id(),
        |id| async move {
            match id {
                Some(id) => Some(fetch_diff(&id).await),
                None => None,
            }
        },
    );
    // The patch's roving hunk focus (M2.16e, #210) — created here, above the
    // render closures, so an icon toggle's re-render doesn't reset which hunk
    // the keyboard was on. Walking to a parent re-renders the patch, and
    // `accessible_patch_view` re-clamps the model to the new hunk count.
    let hunk_focus = create_rw_signal(GraphFocus::new(0));
    // M2.16g (#350): the diff's own scroll position and measured viewport
    // height, the two inputs `render_window` needs. Declared here, beside
    // `hunk_focus` and for the same reason its comment gives — outside the
    // render closures, so a re-render (icon toggle, walking to a parent)
    // does not reset where the diff was scrolled to.
    let diff_scroll = create_rw_signal((0.0_f64, 0.0_f64));
    let diff_box: NodeRef<html::Div> = create_node_ref();
    let on_diff_scroll = move |_| {
        if let Some(el) = diff_box.get_untracked() {
            // `get_untracked` inside an event handler: reading the ref must
            // not subscribe this handler to it, or every scroll frame would
            // re-run the effect that installed the handler.
            diff_scroll.set((el.scroll_top() as f64, el.client_height() as f64));
        }
    };
    move || {
        shell.detail_id().map(|open_id| {
            // Tracked read, like the menu: the panel re-renders live if the icon
            // style is toggled while it's open.
            let ic = icon_set(nerd_icons.get());
            let changes_id = open_id.clone();
            let body = move || {
                // While the fetch is in flight `get()` is `None`; a stale value from
                // the previously-viewed commit is also treated as loading, so the
                // panel never shows one commit's chrome over another's detail.
                match detail.get().flatten() {
                    None => view! { <p class="detail-status">"Loading…"</p> }.into_view(),
                    Some(Err(e)) => view! {
                        <p class="detail-status detail-error">{format!("Couldn't load commit: {e}")}</p>
                    }
                    .into_view(),
                    Some(Ok(d)) if d.id.0 != open_id => {
                        view! { <p class="detail-status">"Loading…"</p> }.into_view()
                    }
                    Some(Ok(d)) => {
                        // Link to the commit on GitHub when the repo has a github.com
                        // origin *and* this commit is pushed — same rule the labels
                        // and menu use, so the link never 404s.
                        // Whether this commit is on the remote comes from the
                        // *detail payload* (M1.10, #63), not from the loaded
                        // rows: the panel routinely shows a commit far below the
                        // last loaded page — walking parents reaches arbitrary
                        // history — so "is it in a row we happen to have" would
                        // wrongly dim every link outside the loaded window.
                        let github = ctx.with_value(|c| {
                            c.frame.repo_url.as_ref().and_then(|base| {
                                d.on_remote
                                    .then(|| format!("{base}/commit/{}", d.id.0))
                            })
                        });
                        // Author and committer lines. Show the committer only when it
                        // differs from the author (name/email or time) — for most
                        // commits they're identical and a second identical line is noise.
                        let committer_differs = d.committer_name != d.author_name
                            || d.committer_email != d.author_email
                            || d.commit_time != d.author_time;
                        let committer_row = committer_differs.then(|| {
                            view! {
                                <div class="detail-field">
                                    <span class="detail-key">"Committer"</span>
                                    <span class="detail-val">
                                        {format!("{} <{}>", d.committer_name, d.committer_email)}
                                        <span class="detail-date">
                                            {format!(" · {}", local_timestamp(d.commit_time))}
                                        </span>
                                    </span>
                                </div>
                            }
                        });
                        // Parents: each short hash re-points the panel at that parent,
                        // so you can walk up the history from within the panel.
                        let parents = if d.parents.is_empty() {
                            view! { <span class="detail-val detail-muted">"none (root commit)"</span> }
                                .into_view()
                        } else {
                            d.parents
                                .iter()
                                .map(|p| {
                                    let full = p.0.clone();
                                    let short = p.short().to_string();
                                    view! {
                                        <button
                                            class="detail-parent"
                                            on:click=move |_| shell.open_detail(full.clone(), false)
                                            title="View this parent"
                                        >
                                            {short}
                                        </button>
                                    }
                                    .into_view()
                                })
                                .collect_view()
                        };
                        let github_row = match github {
                            Some(url) => view! {
                                <a class="detail-github" href=url target="_blank" rel="noopener">
                                    // Same GitHub mark as the menu's external link.
                                    <span class="nf ctx-icon">{ic.github}</span>
                                    "Open on GitHub"
                                </a>
                            }
                            .into_view(),
                            None => ().into_view(),
                        };
                        // Non-GitHub forge link (ADR 0010): same pushed-commit
                        // gating, shown only when there's no GitHub base so it
                        // never doubles the row above.
                        let forge_row = ctx.with_value(|c| {
                            if c.frame.repo_url.is_some() {
                                return None;
                            }
                            c.frame.remote_web_url.as_ref().and_then(|base| {
                                // Same per-commit answer as the GitHub row above:
                                // the detail payload, never the loaded rows.
                                d.on_remote.then(|| {
                                    let url =
                                        git_vista_core::forge::commit_url(base, &d.id.0);
                                    let host = git_vista_core::forge::host_label(base);
                                    view! {
                                        <a
                                            class="detail-github"
                                            href=url
                                            target="_blank"
                                            rel="noopener"
                                        >
                                            <span class="nf ctx-icon">{ic.github}</span>
                                            {format!("View commit on {host}")}
                                        </a>
                                    }
                                })
                            })
                        });
                        view! {
                            <div class="detail-field">
                                <span class="detail-key">"Commit"</span>
                                <span class="detail-val detail-hash">{d.id.0.clone()}</span>
                            </div>
                            <div class="detail-field">
                                <span class="detail-key">"Author"</span>
                                <span class="detail-val">
                                    {format!("{} <{}>", d.author_name, d.author_email)}
                                    <span class="detail-date">
                                        {format!(" · {}", local_timestamp(d.author_time))}
                                    </span>
                                </span>
                            </div>
                            {committer_row}
                            <div class="detail-field">
                                <span class="detail-key">"Parents"</span>
                                <span class="detail-parents">{parents}</span>
                            </div>
                            {github_row}
                            {forge_row}
                            <pre class="detail-msg">{d.message.clone()}</pre>
                        }
                        .into_view()
                    }
                }
            };
            // The Changes section (Activity/Undo step 2): the per-file stat
            // list and the coloured unified patch, reacting to its own fetch
            // exactly like `body` does — so a slow diff never blocks the
            // detail fields, and vice versa.
            let changes = move || {
                match diff.get().flatten() {
                    None => view! { <p class="detail-status">"Loading changes…"</p> }
                        .into_view(),
                    Some(Err(e)) => view! {
                        <p class="detail-status detail-error">
                            {format!("Couldn't load diff: {e}")}
                        </p>
                    }
                    .into_view(),
                    // A stale diff (from the previously-viewed commit) is
                    // still "loading", same rule as the detail body.
                    Some(Ok(d)) if d.id != changes_id => {
                        view! { <p class="detail-status">"Loading changes…"</p> }.into_view()
                    }
                    Some(Ok(d)) => {
                        let ic = icon_set(nerd_icons.get());
                        let (adds, dels) = d.totals();
                        // One row per changed file: kind glyph, path (renames
                        // show "old → new"), then its +/− counts ("binary"
                        // when git couldn't count lines). The row is a button:
                        // tapping it opens that file's full content at this
                        // commit in the full-screen viewer (viewer.rs).
                        let files = d
                            .files
                            .iter()
                            .map(|f| {
                                let (glyph, kind_class) = file_change_marker(ic, f.kind);
                                let label = match &f.old_path {
                                    Some(old) => format!("{old} → {}", f.path),
                                    None => f.path.clone(),
                                };
                                let counts = match (f.additions, f.deletions) {
                                    (Some(a), Some(r)) => view! {
                                        <span class="diff-add">{format!("+{a}")}</span>
                                        <span class="diff-del">{format!(" −{r}")}</span>
                                    }
                                    .into_view(),
                                    _ => view! {
                                        <span class="detail-muted">"binary"</span>
                                    }
                                    .into_view(),
                                };
                                let file_id = d.id.clone();
                                let file_path = f.path.clone();
                                let open_file = move |_| {
                                    shell.open_viewer(ViewerDoc::File {
                                        id: file_id.clone(),
                                        path: file_path.clone(),
                                    });
                                };
                                view! {
                                    <button
                                        class="detail-file"
                                        title="View this file's full content (with Print / Save PDF)"
                                        on:click=open_file
                                    >
                                        <span class=format!("nf ctx-icon {kind_class}")>
                                            {glyph}
                                        </span>
                                        <span class="detail-file-path">{label}</span>
                                        <span class="detail-file-counts">{counts}</span>
                                    </button>
                                }
                            })
                            .collect_view();
                        // The patch, coloured line by line off its prefix, with
                        // hunk headers as roving keyboard/tap stops (M2.16e,
                        // #210 — see `accessible_patch_view`), rendered a
                        // window at a time (M2.16g, #350).
                        //
                        // `.detail-diff` is `white-space: pre`, so every line
                        // is exactly one row and `LineWrap::Never` is a
                        // measured fact about this surface, not an assumption
                        // — the full-screen viewer wraps and is deliberately
                        // left un-windowed for now (see `viewer.rs`).
                        let patch_text = d.patch.clone();
                        // #350: make hunk `idx` renderable before focus moves
                        // to it. Without this the roving focus can address a
                        // hunk that windowing has not mounted, and `.focus()`
                        // lands on nothing — see `focus_hunk_revealed`.
                        let reveal_patch = d.patch.clone();
                        let reveal = Callback::new(move |idx: usize| {
                            let nav = hunk_nav(&reveal_patch);
                            let Some(entry) = nav.get(idx) else { return };
                            let heights = CumulativeHeights::new(&line_heights(
                                &reveal_patch,
                                DIFF_LINE_PX,
                                LineWrap::Never,
                            ));
                            let (scroll, viewport) = diff_scroll.get_untracked();
                            let viewport = if viewport > 0.0 { viewport } else { 800.0 };
                            if let Some(next) = scroll_to_reveal(
                                &heights, entry.line_index, viewport, scroll,
                            ) {
                                // Move the real scroll container too, not just
                                // the signal: the signal drives which lines
                                // render, the element drives what the user
                                // sees, and letting them disagree would render
                                // the right window somewhere off-screen.
                                if let Some(el) = diff_box.get_untracked() {
                                    el.set_scroll_top(next as i32);
                                }
                                diff_scroll.set((next, viewport));
                            }
                        });
                        let patch = view! {
                            <div
                                class="detail-diff-scroll"
                                node_ref=diff_box
                                on:scroll=on_diff_scroll
                                // Opts this scrollable region OUT of the browser's
                                // own default focusability. Per the CSS Overflow /
                                // HTML focus spec, any element with `overflow:auto`
                                // that actually overflows becomes keyboard-focusable
                                // on its own — no explicit `tabindex` needed — so
                                // Tab can land on it directly (Chromium has shipped
                                // this since M89; other engines follow the same
                                // resolution). Without this line that auto-inserted
                                // stop sits BEFORE any hunk header in document
                                // order, so Tab lands on the scroll container
                                // itself, not on the roving-tabindex span in
                                // `hunk_header_span` — and the browser's native
                                // keydown handling on a focused scrollable element
                                // is exactly "arrow keys scroll it", which is the
                                // observed bug (#210's `on_keydown` never fires
                                // because DOM focus was never on a header span).
                                // `tabindex="-1"` keeps the element programmatically
                                // focusable (`focus_hunk`'s `.focus()` calls target
                                // spans, not this div, so that is moot here) while
                                // removing it from sequential (Tab) navigation.
                                tabindex="-1"
                            >
                                {move || {
                                    let (scroll, viewport) = diff_scroll.get();
                                    let heights = CumulativeHeights::new(&line_heights(
                                        &patch_text,
                                        DIFF_LINE_PX,
                                        LineWrap::Never,
                                    ));
                                    // Before the first scroll event the box has
                                    // not been measured; fall back to a viewport
                                    // tall enough that the first paint is never
                                    // short of content.
                                    let viewport = if viewport > 0.0 { viewport } else { 800.0 };
                                    let w = render_window(&heights, viewport, scroll, DIFF_OVERSCAN);
                                    view! {
                                        <div style=format!("height:{}px", w.pad_top)></div>
                                        <pre class="detail-diff">
                                            {accessible_patch_window(
                                                &patch_text, hunk_focus, "detail",
                                                Some(w.start..w.end), Some(reveal),
                                            )}
                                        </pre>
                                        <div style=format!("height:{}px", w.pad_bottom)></div>
                                    }
                                }}
                            </div>
                        }
                        .into_view();
                        let truncated_note = d.truncated.then(|| {
                            view! {
                                <p class="detail-status">
                                    "Patch truncated — this commit's full diff is larger \
                                     than the panel shows."
                                </p>
                            }
                        });
                        let merge_note = d.against_first_parent.then(|| {
                            view! {
                                <span class="detail-muted">" · vs first parent"</span>
                            }
                        });
                        // "Show diff" was tapped: scroll this section into view
                        // now that it exists. RAF defers until after the DOM
                        // commit; the flag is one-shot so a later re-render
                        // (icon toggle, parent walk) doesn't scroll again.
                        // Reads *and* clears in one call, so a wish left by "Show diff"
                        // cannot fire again on the next commit's panel.
                        if shell.take_diff_scroll() {
                            request_animation_frame(|| {
                                if let Some(el) =
                                    document().get_element_by_id("detail-changes")
                                {
                                    el.scroll_into_view();
                                }
                            });
                        }
                        // "Expand Full Diff": the same diff, full-screen and
                        // uncapped (`?full=1`), with Print / Save PDF.
                        let expand_id = d.id.clone();
                        let expand = view! {
                            <button
                                class="detail-expand"
                                title="Open the whole diff full-screen, uncapped, \
                                       with Print / Save PDF"
                                on:click=move |_| {
                                    shell.open_viewer(ViewerDoc::Diff {
                                        id: expand_id.clone(),
                                    });
                                }
                            >
                                "Expand Full Diff"
                            </button>
                        };
                        view! {
                            <div class="detail-section-title" id="detail-changes">
                                <span class="nf ctx-icon">{ic.modified}</span>
                                {format!("Changes — {} file{}", d.files.len(),
                                         if d.files.len() == 1 { "" } else { "s" })}
                                <span class="diff-add">{format!(" +{adds}")}</span>
                                <span class="diff-del">{format!(" −{dels}")}</span>
                                {merge_note}
                                {expand}
                            </div>
                            {files}
                            {truncated_note}
                            <pre class="detail-diff">{patch}</pre>
                        }
                        .into_view()
                    }
                }
            };
            view! {
                <aside class="detail-panel">
                    <div class="detail-head">
                        // The commit glyph titles the panel — it's one commit's view.
                        <span class="detail-title">
                            <span class="nf ctx-icon">{ic.commit}</span>
                            "Commit details"
                        </span>
                        <button
                            class="detail-close"
                            title="Close"
                            // The visible content is one glyph; VoiceOver would
                            // otherwise announce this as "multiplication sign" (#65).
                            aria-label="Close commit details"
                            on:click=move |_| shell.close_detail()
                        >
                            "×"
                        </button>
                    </div>
                    // Same auto-focusable-scroll-container opt-out as
                    // `.detail-diff-scroll` above (`overflow-y: auto` in
                    // styles.css) — this outer container predates tonight's
                    // windowing change, which is why the roving-tabindex
                    // hunk headers have reportedly never received arrow-key
                    // focus: Tab was landing here (or on `.detail-diff-scroll`
                    // nested inside it) before it ever reached a header span.
                    <div class="detail-body" tabindex="-1">{body}{changes}</div>
                </aside>
            }
        })
    }
}
