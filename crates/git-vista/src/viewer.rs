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
use git_vista_protocol::diff::{DiffSpec, SpecDiff};
use git_vista_protocol::{PatchPlan, PatchPreview, StageDirection, StagingDiff};

use crate::api::{fetch_diff_full, fetch_file, fetch_spec_diff, staging_diff_request};
use crate::detail::{accessible_patch_view, file_change_marker};
use crate::features::a11y::focus::GraphFocus;
use crate::features::diff::selection::DiffSelection;
use crate::features::diff::staging_view::staging_body;
use crate::features::graph::core::RenderCtx;
use crate::icons::icon_set;
use crate::state::{Features, Settings, ViewerDoc};

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
///
/// `ctx` (M2.17d, #215) is only used by the `Staging` document — the
/// repository/worktree identity a [`git_vista_protocol::PatchPlan`] needs —
/// but is threaded in alongside `features` rather than fetched separately,
/// mirroring `detail::detail_panel_view`'s signature.
pub fn viewer_view(
    features: Features,
    settings: Settings,
    ctx: StoredValue<RenderCtx>,
) -> impl IntoView {
    let Features { shell, status, .. } = features;
    let nerd_icons = settings.nerd_icons;
    // The full-screen patch's roving hunk focus (M2.16e, #210) — its own
    // model, distinct from the detail panel's, because both surfaces can be
    // mounted at once. Created above the render closures for the same
    // reason as the detail panel's: a re-render must not reset the position.
    let hunk_focus = create_rw_signal(GraphFocus::new(0));
    // The staging selection's own state (M2.17d, #215) — created here, not
    // inside the render closure, for the same reason: a re-render (e.g. the
    // selection itself changing) must not reset the model. Cleared whenever
    // a *fresh* staging fetch starts (see the resource below), so reopening
    // — or switching Stage↔Unstage — never carries over a selection made
    // against a different, possibly now-stale, diff.
    let staging_selection = create_rw_signal(DiffSelection::new());
    let staging_preview = create_rw_signal(None::<Result<PatchPreview, String>>);
    // The exact plan `staging_preview` was built from (review finding,
    // #215): without this, changing the selection after a preview leaves
    // the OLD preview text on screen with nothing to say it no longer
    // matches. `staging_body` hides the preview panel once the current
    // selection's plan diverges from this, rather than showing stale
    // content — see its own comment at the point of use.
    let staging_previewed_plan = create_rw_signal(None::<PatchPlan>);
    let staging_busy = create_rw_signal(false);
    // One resource for every document kind: the key carries the enum, the
    // fetch picks the endpoint. A stale response is ignored via the id/path
    // echo, same rule as the detail panel's fetches.
    let doc = create_local_resource(
        move || shell.viewer_doc(),
        move |doc| async move {
            match doc {
                None => None,
                Some(ViewerDoc::Diff { id }) => Some(DocResult::Diff(fetch_diff_full(&id).await)),
                Some(ViewerDoc::File { id, path }) => {
                    Some(DocResult::File(fetch_file(&id, &path).await))
                }
                Some(ViewerDoc::Staging { direction }) => {
                    staging_selection.update(|s| s.clear());
                    staging_preview.set(None);
                    staging_previewed_plan.set(None);
                    Some(DocResult::Staging(staging_diff_request(direction).await))
                }
                Some(ViewerDoc::Spec { spec }) => {
                    Some(DocResult::Spec(fetch_spec_diff(&spec).await))
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
                ViewerDoc::Staging { direction } => match direction {
                    StageDirection::Stage => "Stage selected changes".to_string(),
                    StageDirection::Unstage => "Unstage selected changes".to_string(),
                },
                ViewerDoc::Spec { spec } => spec_title(spec),
            };
            let which_for_body = which.clone();
            let body = move || match doc.get().flatten() {
                None => view! { <p class="detail-status">"Loading…"</p> }.into_view(),
                Some(DocResult::Diff(Err(e)))
                | Some(DocResult::File(Err(e)))
                | Some(DocResult::Staging(Err(e)))
                | Some(DocResult::Spec(Err(e))) => view! {
                    <p class="detail-status detail-error">{format!("Couldn't load: {e}")}</p>
                }
                .into_view(),
                Some(DocResult::Diff(Ok(d))) => {
                    // Ignore a stale diff after switching documents.
                    if !matches!(&which_for_body, ViewerDoc::Diff { id } if *id == d.id) {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    diff_body(&d, nerd_icons.get(), hunk_focus)
                }
                Some(DocResult::File(Ok(f))) => {
                    if !matches!(&which_for_body, ViewerDoc::File { id, path }
                                 if *id == f.id && *path == f.path)
                    {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    file_body(&f)
                }
                Some(DocResult::Spec(Ok(d))) => {
                    // The staleness echo ADR 0053 relies on: a response whose
                    // spec is not the one currently open is a late answer to a
                    // superseded request, and is dropped rather than painted.
                    // Same rule as the `Diff`/`File` arms above.
                    if !matches!(&which_for_body, ViewerDoc::Spec { spec } if *spec == d.spec) {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    spec_body(&d, hunk_focus)
                }
                Some(DocResult::Staging(Ok(d))) => {
                    let ViewerDoc::Staging { direction } = which_for_body else {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    };
                    staging_body(
                        &d,
                        direction,
                        hunk_focus,
                        staging_selection,
                        staging_preview,
                        staging_previewed_plan,
                        staging_busy,
                        ctx,
                        status,
                        shell,
                    )
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
                    // Same fix as detail.rs's `.detail-diff-scroll`/`.detail-body`
                    // (see that file's comment): `.viewer-body` is
                    // `overflow: auto` (styles.css), which modern browsers make
                    // keyboard-focusable on their own. Left unopted-out, Tab
                    // lands here instead of on the "viewer"-scoped roving hunk
                    // header span this file's `diff_body` renders via
                    // `accessible_patch_view`, and arrow keys then scroll this
                    // div natively instead of reaching `hunk_header_span`'s
                    // `on_keydown`.
                    <div class="viewer-body" tabindex="-1">{body}</div>
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
    Staging(Result<StagingDiff, String>),
    Spec(Result<SpecDiff, String>),
}

/// The viewer title for an explicit source/target diff (M2.16, #69).
///
/// Reads as the comparison rather than as the mode name: a user picking
/// "compare with main" wants to see those two names, not the words
/// "RefVsRef". Commit ids are shortened the same way the `Diff` title does.
fn spec_title(spec: &DiffSpec) -> String {
    fn short(id: &str) -> &str {
        &id[..id.len().min(7)]
    }
    match spec {
        DiffSpec::WorktreeVsIndex => "Working tree vs index".to_string(),
        DiffSpec::IndexVsCommit { commit } => {
            format!("Index vs {}", short(commit.as_str()))
        }
        DiffSpec::CommitVsCommit { base, target } => {
            format!("{} → {}", short(base.as_str()), short(target.as_str()))
        }
        DiffSpec::RefVsRef { base, target } => {
            format!("{} → {}", base.as_str(), target.as_str())
        }
    }
}

/// An explicit source/target diff document (M2.16, #69).
///
/// Simpler than [`diff_body`] because [`SpecDiff`] carries no per-file stat
/// list — see its own doc for why that is a deliberate scope decision rather
/// than an omission (naming core's `DiffFile` from the protocol crate would
/// break the wasm build this crate exists to stay compatible with).
///
/// The patch renders through the same `accessible_patch_view` the other diff
/// surfaces use, so hunk navigation, the screen-reader prefixes and the roving
/// tab stop all behave identically here. Not windowed, for the same reason the
/// rest of this viewer is not — see the comment at the bottom of `diff_body`
/// and #362.
fn spec_body(d: &SpecDiff, hunk_focus: RwSignal<GraphFocus>) -> View {
    let truncated_note = d.truncated.then(|| {
        view! {
            <p class="detail-status">
                "Diff truncated at the server\u{2019}s size cap \u{2014} showing the first part only."
            </p>
        }
    });
    let empty_note = d.patch.trim().is_empty().then(|| {
        view! { <p class="detail-status">"No differences."</p> }
    });
    view! {
        {empty_note}
        {truncated_note}
        <pre class="detail-diff viewer-pre">
            {accessible_patch_view(&d.patch, hunk_focus, "viewer")}
        </pre>
    }
    .into_view()
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
        // NOT windowed, unlike the panel (M2.16g, #350) — tracked on #362.
        //
        // `.viewer-pre` is `white-space: pre-wrap` with `word-break:
        // break-word`, so a long line wraps and its height depends on its own
        // length *and* the container's width. The panel, which does not wrap,
        // has no such ambiguity and is windowed today.
        //
        // This comment used to say the blocker was "the column count it needs
        // is an estimate this file cannot measure without a layout read." That
        // understates one problem and overstates the other. Measuring width is
        // *not* blocked: `get_bounding_client_rect` is already used in this
        // codebase (`gestures.rs:82`, `:308`), and `features::shell::signals`'s
        // `install_mode_signal` is a shipped precedent for keeping such a
        // measurement current through a debounced resize listener.
        //
        // The real problem is that `line_heights`' `ceil(chars / columns)` is a
        // *character*-wrap model, while this CSS wraps at *word* boundaries. A
        // perfectly measured column count still would not track real rendered
        // row counts on code text. And unlike the panel's `DIFF_LINE_PX` — one
        // global scale factor whose error stays proportional — this arithmetic
        // is quantized per line: a columns estimate off by two flips a 79-char
        // line between one row and two. `LineWrap::Wrapped`'s own "good enough
        // for windowing" doc claim is inherited from the panel's situation and
        // does not hold here.
        //
        // Whether windowing the viewer is needed at all is also unmeasured —
        // see #362, which puts measuring it first.
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
