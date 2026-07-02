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

use leptos::*;

use git_vista_core::status::ChangeKind;

use crate::api::fetch_diff;
use crate::datetime::local_timestamp;
use crate::icons::{icon_set, GitIcons};
use crate::render::RenderCtx;
use crate::state::{DetailResource, Overlays, Settings};

/// CSS class for one line of the unified patch, keyed off its prefix. The
/// file/hunk headers are checked *before* the bare +/- so `+++`/`---` read as
/// metadata, not as a one-character change.
fn diff_line_class(line: &str) -> &'static str {
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

/// Glyph + colour class for one changed file's kind, from the icon fields
/// defined for exactly this view (see icons.rs — added/modified/deleted/
/// renamed have waited for a diff surface since the icon system landed).
fn file_change_marker(ic: &GitIcons, kind: ChangeKind) -> (&'static str, &'static str) {
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
    overlays: Overlays,
    settings: Settings,
    detail: DetailResource,
    ctx: StoredValue<RenderCtx>,
) -> impl IntoView {
    let detail_id = overlays.detail_id;
    let scroll_diff = overlays.scroll_diff;
    let nerd_icons = settings.nerd_icons;
    // The commit's diff (file list + patch), fetched lazily alongside the
    // detail and keyed on the same open hash — so walking to a parent
    // re-fetches both, and closing the panel idles both.
    let diff = create_local_resource(
        move || detail_id.get(),
        |id| async move {
            match id {
                Some(id) => Some(fetch_diff(&id).await),
                None => None,
            }
        },
    );
    move || {
        detail_id.get().map(|open_id| {
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
                        let github = ctx.with_value(|c| {
                            c.repo_url.as_ref().and_then(|base| {
                                c.remote_set
                                    .contains(&d.id.0)
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
                                            on:click=move |_| detail_id.set(Some(full.clone()))
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
                        // when git couldn't count lines).
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
                                view! {
                                    <div class="detail-file">
                                        <span class=format!("nf ctx-icon {kind_class}")>
                                            {glyph}
                                        </span>
                                        <span class="detail-file-path">{label}</span>
                                        <span class="detail-file-counts">{counts}</span>
                                    </div>
                                }
                            })
                            .collect_view();
                        // The patch, coloured line by line off its prefix. Each
                        // line keeps its own trailing newline so the <pre>
                        // preserves the exact text layout.
                        let patch = d
                            .patch
                            .lines()
                            .map(|l| {
                                let class = diff_line_class(l);
                                let text = format!("{l}\n");
                                view! { <span class=class>{text}</span> }
                            })
                            .collect_view();
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
                        if scroll_diff.get_value() {
                            scroll_diff.set_value(false);
                            request_animation_frame(|| {
                                if let Some(el) =
                                    document().get_element_by_id("detail-changes")
                                {
                                    el.scroll_into_view();
                                }
                            });
                        }
                        view! {
                            <div class="detail-section-title" id="detail-changes">
                                <span class="nf ctx-icon">{ic.modified}</span>
                                {format!("Changes — {} file{}", d.files.len(),
                                         if d.files.len() == 1 { "" } else { "s" })}
                                <span class="diff-add">{format!(" +{adds}")}</span>
                                <span class="diff-del">{format!(" −{dels}")}</span>
                                {merge_note}
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
                            on:click=move |_| detail_id.set(None)
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
