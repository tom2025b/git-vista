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
use crate::features::diff::core::hunk_nav;
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
/// The header spans deliberately carry **no `role`**: they navigate, they do
/// not activate — hunk *selection* is staging's business (M2.17), and
/// `role="button"` would promise an action that does not exist yet. A
/// focusable element's `aria-label` is announced on focus regardless.
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
    patch
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let class = diff_line_class(l);
            let text = format!("{l}\n");
            match nav_at.remove(&i) {
                Some((idx, label)) => hunk_header_span(class, text, idx, label, focus, scope),
                // The sr-only prefix is position:absolute, so the visible
                // text layout inside the <pre> is byte-identical.
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
        let dir = match ev.key().as_str() {
            "ArrowDown" => FocusMove::Next,
            "ArrowUp" => FocusMove::Prev,
            "Home" => FocusMove::First,
            "End" => FocusMove::Last,
            "Escape" => {
                // Leave hunk navigation without closing anything: disengage
                // the model and move DOM focus off the header. Stopped here
                // so the window Esc handler doesn't also dismiss the panel —
                // a second Escape, with focus elsewhere, still does.
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
            focus_hunk(scope, next);
        }
    };
    // Tap parity: iOS Safari does not reliably focus a tabindexed span on
    // tap, so the click handler focuses it explicitly; `focus_landed` in the
    // focus handler then moves the roving position (idempotent when both
    // fire).
    let on_click = move |_| {
        focus.update(|f| f.focus_landed(idx));
        focus_hunk(scope, idx);
    };
    let on_focus = move |_| focus.update(|f| f.focus_landed(idx));
    view! {
        <span
            class=class
            data-hunk-scope=scope
            data-hunk-index=idx.to_string()
            tabindex=tabindex
            aria-label=label
            on:keydown=on_keydown
            on:click=on_click
            on:focus=on_focus
        >
            {text}
        </span>
    }
    .into_view()
}

/// Move DOM focus to hunk `idx` in `scope`. Every line of the flat rendering
/// is mounted, so this is a direct query + focus — no scroll-then-RAF dance
/// like the virtualized graph needs (`gestures::focus_row_next_frame`).
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
                        // #210 — see `accessible_patch_view`).
                        let patch = accessible_patch_view(&d.patch, hunk_focus, "detail");
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
                    <div class="detail-body">{body}{changes}</div>
                </aside>
            }
        })
    }
}
