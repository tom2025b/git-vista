//! The full-screen viewer: one commit's whole diff ("Expand Full Diff" in the
//! detail panel) or one file's full content (tapping a file in the diff list),
//! with a single Print / Save PDF control.
//!
//! That button drives the browser's print flow (`window.print()`): that is the
//! one path that reliably produces a PDF on iPad Safari — the print sheet's
//! share button (or a pinch-out on the preview) offers "Save to Files"/share as
//! PDF. While the viewer is open it stamps `data-print` on `<html>`; the
//! `@media print` rules in styles.css then print *only* the viewer's content,
//! light-themed and flowing across pages, instead of the dark app shell.
//!
//! iPad rules honoured here: the Close button is a visible control (no
//! Esc-only exit — the Magic Keyboard has no Esc key), and no void elements
//! (`<input>` et al panic Leptos' CSR template walk on iOS WebKit).

use leptos::*;

use git_vista_core::diff::{CommitDiff, FileContent};

use crate::api::{fetch_diff_full, fetch_file};
use crate::detail::{accessible_patch_view, file_change_marker};
use crate::features::a11y::focus::GraphFocus;
use crate::features::shell::signals::Shell;
use crate::icons::icon_set;
use crate::state::{Settings, ViewerDoc};

/// Stamp (or clear) the `data-print` attribute on `<html>`. The print styles
/// key off it, so a plain browser print with no viewer open still prints the
/// normal page.
fn set_print_attr(on: bool) {
    if let Some(root) = document().document_element() {
        if on {
            let _ = root.set_attribute("data-print", "viewer");
        } else {
            let _ = root.remove_attribute("data-print");
        }
    }
}

/// Open the browser's print flow. On iPad Safari the resulting sheet is also
/// the "Save PDF" path (share → Save to Files, or pinch out the preview).
fn print_now() {
    if let Some(w) = web_sys::window() {
        let _ = w.print();
    }
}

/// The full-screen viewer overlay. Renders while the shell holds a viewer document;
/// fetches its document lazily (keyed on the open doc, like the detail panel),
/// and closes via its own visible button.
pub fn viewer_view(shell: Shell, settings: Settings) -> impl IntoView {
    let nerd_icons = settings.nerd_icons;
    // The full-screen patch's roving hunk focus (M2.16e, #210) — its own
    // model, distinct from the detail panel's, because both surfaces can be
    // mounted at once. Created above the render closures for the same
    // reason as the detail panel's: a re-render must not reset the position.
    let hunk_focus = create_rw_signal(GraphFocus::new(0));
    // One resource for either document kind: the key carries the enum, the
    // fetch picks the endpoint. A stale response is ignored via the id/path
    // echo, same rule as the detail panel's fetches.
    let doc = create_local_resource(
        move || shell.viewer_doc(),
        |doc| async move {
            match doc {
                None => None,
                Some(ViewerDoc::Diff { id }) => Some(DocResult::Diff(fetch_diff_full(&id).await)),
                Some(ViewerDoc::File { id, path }) => {
                    Some(DocResult::File(fetch_file(&id, &path).await))
                }
            }
        },
    );
    move || {
        let open = shell.viewer_doc();
        // The print CSS keys off <html data-print> — set while open, cleared
        // when closed, so a plain print without the viewer stays the full page.
        set_print_attr(open.is_some());
        open.map(|which| {
            let ic = icon_set(nerd_icons.get());
            let title = match &which {
                ViewerDoc::Diff { id } => format!("Full diff — {}", &id[..id.len().min(7)]),
                ViewerDoc::File { id, path } => {
                    format!("{path} @ {}", &id[..id.len().min(7)])
                }
            };
            let body = move || match doc.get().flatten() {
                None => view! { <p class="detail-status">"Loading…"</p> }.into_view(),
                Some(DocResult::Diff(Err(e))) | Some(DocResult::File(Err(e))) => view! {
                    <p class="detail-status detail-error">{format!("Couldn't load: {e}")}</p>
                }
                .into_view(),
                Some(DocResult::Diff(Ok(d))) => {
                    // Ignore a stale diff after switching documents.
                    if !matches!(&which, ViewerDoc::Diff { id } if *id == d.id) {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    diff_body(&d, nerd_icons.get(), hunk_focus)
                }
                Some(DocResult::File(Ok(f))) => {
                    if !matches!(&which, ViewerDoc::File { id, path }
                                 if *id == f.id && *path == f.path)
                    {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    file_body(&f)
                }
            };
            view! {
                <div class="viewer-modal print-surface">
                    <div class="viewer-head">
                        <span class="viewer-title">
                            <span class="nf ctx-icon">{ic.modified}</span>
                            {title}
                        </span>
                        <span class="viewer-actions">
                            <button
                                class="viewer-btn"
                                title="Opens the print sheet — on iPad choose the \
                                       share icon (or pinch the preview open) and \
                                       ‘Save to Files’ to keep it as a PDF"
                                on:click=move |_| print_now()
                            >
                                "Print / Save PDF"
                            </button>
                            <button
                                class="viewer-btn viewer-close"
                                title="Close"
                                on:click=move |_| shell.close_viewer()
                            >
                                "Close ×"
                            </button>
                        </span>
                    </div>
                    <div class="viewer-body">{body}</div>
                </div>
            }
        })
    }
}

/// What the viewer's one resource resolved to — tagged by document kind so a
/// diff response can never render as a file (or vice versa) during a switch.
#[derive(Clone)]
enum DocResult {
    Diff(Result<CommitDiff, String>),
    File(Result<FileContent, String>),
}

/// The full-diff document: the per-file stat list, then the whole unified
/// patch coloured line by line — the detail panel's Changes section, at
/// full-screen scale and without the panel's patch cap.
fn diff_body(d: &CommitDiff, nerd: bool, hunk_focus: RwSignal<GraphFocus>) -> View {
    let ic = icon_set(nerd);
    let (adds, dels) = d.totals();
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
                _ => view! { <span class="detail-muted">"binary"</span> }.into_view(),
            };
            view! {
                <div class="detail-file">
                    <span class=format!("nf ctx-icon {kind_class}")>{glyph}</span>
                    <span class="detail-file-path">{label}</span>
                    <span class="detail-file-counts">{counts}</span>
                </div>
            }
        })
        .collect_view();
    // Same accessible flat rendering as the detail panel (M2.16e, #210),
    // under its own scope so DOM focus queries can't cross surfaces.
    let patch = accessible_patch_view(&d.patch, hunk_focus, "viewer");
    let truncated_note = d.truncated.then(|| {
        view! {
            <p class="detail-status">
                "Patch truncated — this diff is larger than even the full view's cap."
            </p>
        }
    });
    let merge_note = d
        .against_first_parent
        .then(|| view! { <span class="detail-muted">" · vs first parent"</span> });
    view! {
        <div class="viewer-doc-head">
            {format!("Changes — {} file{}", d.files.len(),
                     if d.files.len() == 1 { "" } else { "s" })}
            <span class="diff-add">{format!(" +{adds}")}</span>
            <span class="diff-del">{format!(" −{dels}")}</span>
            {merge_note}
        </div>
        {files}
        {truncated_note}
        <pre class="detail-diff viewer-pre">{patch}</pre>
    }
    .into_view()
}

/// The full-file document: the whole content in one `<pre>` (or the binary /
/// truncated notes where content can't be shown verbatim).
fn file_body(f: &FileContent) -> View {
    if f.binary {
        return view! {
            <p class="detail-status">
                "Binary file — no text preview. (The diff's file list shows its change kind.)"
            </p>
        }
        .into_view();
    }
    let truncated_note = f.truncated.then(|| {
        view! {
            <p class="detail-status">
                "Content truncated — this file is larger than the viewer's cap."
            </p>
        }
    });
    let lines = f.content.lines().count();
    view! {
        <div class="viewer-doc-head">
            {format!("{} — {} line{}", f.path, lines, if lines == 1 { "" } else { "s" })}
        </div>
        {truncated_note}
        <pre class="detail-diff viewer-pre">{f.content.clone()}</pre>
    }
    .into_view()
}
