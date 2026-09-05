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
use git_vista_core::virtualize::CumulativeHeights;
use git_vista_protocol::blame::BlamePage;
use git_vista_protocol::diff::{ComparisonBasis, DiffSpec, SpecDiff};
use git_vista_protocol::{PatchPlan, PatchPreview, StageDirection, StagingDiff};

use crate::api::{
    fetch_blame, fetch_conflict_panes, fetch_conflict_source, fetch_diff_full, fetch_file,
    fetch_file_history, fetch_spec_diff, resolve_conflict_content_request,
    resolve_conflict_request, staging_diff_request,
};
use crate::features::blame::view::blame_body;
// The row-height scale and overscan are the panel's constants, shared rather
// than duplicated: two surfaces measuring the same rendered text with
// different numbers is how a window and its scrollbar drift apart.
use crate::detail::{accessible_rows_window, file_change_marker, DIFF_LINE_PX, DIFF_OVERSCAN};
use crate::features::a11y::focus::GraphFocus;
use crate::features::diff::core::{render_window, LineWrap};
use crate::features::diff::rows::{flatten, row_heights};
use crate::features::diff::selection::DiffSelection;
use crate::features::diff::staging_view::staging_body;
use crate::features::graph::core::{GraphCore, RenderCtx};
use crate::features::readiness::core::{is_viewer_busy, DocIdentity, FetchOutcome};
use crate::features::shell::signals::Shell;
use crate::features::status::signals::StatusResource;
use crate::icons::icon_set;
use crate::state::{Features, Settings, ViewerDoc};
use git_vista_conflicts::core::{ConflictPanes, Pane, PaneState};
use git_vista_protocol::conflict::Resolution;
use git_vista_protocol::diff::parse_unified_diff;

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

/// Follow the browser's own print lifecycle, so Ctrl+P / ⌘P / the print menu
/// unwindow the patch too — not only the in-app button.
///
/// `beforeprint` fires before the print document is laid out and `afterprint`
/// once it is done, which is the same window `print_now` brackets by hand.
/// Both paths are wired because either alone leaves a hole: the button is the
/// documented iPad route, and `beforeprint` is the only hook for a print the
/// app never initiated.
#[cfg(target_arch = "wasm32")]
fn install_print_signal(printing: RwSignal<bool>) {
    use wasm_bindgen::{closure::Closure, JsCast};
    let Some(win) = web_sys::window() else { return };

    let before = Closure::<dyn FnMut()>::new(move || {
        let _ = printing.try_set(true);
    });
    let after = Closure::<dyn FnMut()>::new(move || {
        let _ = printing.try_set(false);
    });
    let _ = win.add_event_listener_with_callback("beforeprint", before.as_ref().unchecked_ref());
    let _ = win.add_event_listener_with_callback("afterprint", after.as_ref().unchecked_ref());

    let w2 = win.clone();
    on_cleanup(move || {
        let _ =
            w2.remove_event_listener_with_callback("beforeprint", before.as_ref().unchecked_ref());
        let _ =
            w2.remove_event_listener_with_callback("afterprint", after.as_ref().unchecked_ref());
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn install_print_signal(_printing: RwSignal<bool>) {}

/// Open the browser's print flow. On iPad Safari the resulting sheet is also
/// the "Save PDF" path (share → Save to Files, or pinch out the preview).
///
/// `printing` is raised BEFORE `window.print()` and lowered after, because the
/// viewer is windowed (#362): only the rows around the scroll position are
/// mounted, and the rest of the document's height is two spacer `<div>`s. The
/// print stylesheet un-clips `.viewer-body`, so those spacers become real,
/// empty pages — the whole patch paginates, but only the mounted sliver
/// carries text. Raising this flag makes `diff_body` render every row with no
/// spacers, which is exactly the unwindowed behaviour that printed correctly
/// before #362.
///
/// The deferral is required, not defensive: `window.print()` blocks
/// synchronously while the browser lays out the print document, so the flag
/// must reach the DOM *first*. Leptos flushes its effects on a microtask, so
/// setting the signal and printing in the same tick would print the old,
/// windowed DOM — the exact bug this fixes.
#[cfg(target_arch = "wasm32")]
fn print_now(printing: RwSignal<bool>) {
    printing.set(true);
    leptos::set_timeout(
        move || {
            if let Some(w) = web_sys::window() {
                let _ = w.print();
            }
            // Back to windowed as soon as the (blocking) print sheet returns,
            // so the on-screen viewer does not keep a whole 5MB patch mounted.
            let _ = printing.try_set(false);
        },
        std::time::Duration::from_millis(50),
    );
}

/// Native builds have no `window`; the button is wasm-only in practice, but the
/// signature has to exist for `cargo test` to compile this module.
#[cfg(not(target_arch = "wasm32"))]
fn print_now(_printing: RwSignal<bool>) {}

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
    let Features {
        shell,
        status,
        graph,
        compare_anchor,
        ..
    } = features;
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
    // Windowing state for the full-screen patch (#362). Created here, above
    // the render closures, for the same reason as `hunk_focus`: a re-render
    // must not reset the scroll position or throw away the measurement.
    //
    // `(scroll_top, client_height)` — the two inputs `render_window` needs,
    // read off `.viewer-body` because that is the scrolling box here (the
    // panel reads its own `.detail-diff-scroll`).
    let viewer_scroll = create_rw_signal((0.0_f64, 0.0_f64));
    // How many monospace cells fit across the viewer, measured rather than
    // assumed (#362 step 3). Falls back to 80 until the container is laid
    // out, which is what the estimate used to be — so a build that cannot
    // measure degrades to the behaviour that shipped before, not to something
    // new.
    let viewer_columns = crate::features::diff::measure::install_columns_signal();
    // Raised while the browser is producing a print document. `diff_body`
    // renders unwindowed when it is set — see `print_now` for why the window
    // and the print stylesheet cannot both be honoured at once.
    let printing = create_rw_signal(false);
    install_print_signal(printing);
    let staging_selection = create_rw_signal(DiffSelection::new());
    // M5.33 (#86): the blame panel's own roving focus and touch/keyboard
    // range selection — created here, above the render closures, for the
    // same reason as `hunk_focus`: a re-render must not reset the position.
    let blame_focus = create_rw_signal(GraphFocus::new(0));
    let blame_selection = create_rw_signal(crate::features::blame::core::BlameSelection::new());
    let staging_preview = create_rw_signal(None::<Result<PatchPreview, String>>);
    // The exact plan `staging_preview` was built from (review finding,
    // #215): without this, changing the selection after a preview leaves
    // the OLD preview text on screen with nothing to say it no longer
    // matches. `staging_body` hides the preview panel once the current
    // selection's plan diverges from this, rather than showing stale
    // content — see its own comment at the point of use.
    let staging_previewed_plan = create_rw_signal(None::<PatchPlan>);
    let staging_busy = create_rw_signal(false);
    // M4.31b (#429): the conflict view's own write state. `resolve_error`
    // holds the server's OWN sentence about a refusal — which side, and why —
    // rendered inline rather than thrown at an alert box, because "you cannot
    // take a side that is not there" is the answer the user needs in front of
    // the panes they are choosing between.
    let resolve_busy = create_rw_signal(false);
    let resolve_error = create_rw_signal(None::<String>);
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
                Some(ViewerDoc::Blame { path, rev }) => {
                    // A selection made against a since-replaced path/rev
                    // would address ranges that may no longer exist —
                    // same rule `Staging`'s arm applies to its own selection.
                    blame_selection.update(|s| s.clear());
                    Some(DocResult::Blame(fetch_blame(&path, &rev, None, None).await))
                }
                Some(ViewerDoc::Conflict { path }) => {
                    // A refusal belongs to the path it was about; carrying it
                    // onto the next file would explain the wrong conflict.
                    resolve_error.set(None);
                    let result = match ctx.with_value(|c| c.frame.worktree_id.clone()) {
                        Some(repo) => fetch_conflict_panes(&repo, &path).await,
                        None => Err(
                            "This repository has no worktree id, so its conflicts cannot be resolved."
                                .to_string(),
                        ),
                    };
                    Some(DocResult::Conflict(result))
                }
            }
        },
    );
    // The blame panel's own file-history list — a second resource rather
    // than folded into `doc` above: `BlamePage` and `FileHistoryPage` are two
    // different reads of the same path/rev, and `blame_body` renders the
    // history list underneath the ranges regardless of which one is still
    // settling, so gating the whole panel on both landing together would
    // make the ranges wait on a fetch they do not need.
    let blame_history = create_local_resource(
        move || shell.viewer_doc(),
        |doc| async move {
            match doc {
                Some(ViewerDoc::Blame { path, rev }) => {
                    Some(fetch_file_history(&path, &rev, None).await)
                }
                _ => None,
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
                ViewerDoc::Conflict { path } => format!("Conflict — {path}"),
                ViewerDoc::Blame { path, rev } => {
                    format!("Blame — {path} @ {}", &rev[..rev.len().min(7)])
                }
            };
            let which_for_body = which.clone();
            // #387: the readiness signal, cloned off before `which_for_body`
            // moves into `body` below. Recomputed from the SAME two facts
            // `body`'s own match reads (what's open, what the resource
            // settled on) — see `viewer_doc_identity`/`doc_result_outcome`
            // for the (data-only) reduction and `is_viewer_busy` for the
            // actual decision, which is host-tested in
            // `features/readiness/core.rs`.
            let which_for_busy = which_for_body.clone();
            let is_busy = move || {
                let outcome = doc_result_outcome(doc.get().flatten().as_ref());
                is_viewer_busy(&viewer_doc_identity(&which_for_busy), &outcome)
            };
            let body = move || match doc.get().flatten() {
                None => view! { <p class="detail-status">"Loading…"</p> }.into_view(),
                Some(DocResult::Diff(Err(e)))
                | Some(DocResult::File(Err(e)))
                | Some(DocResult::Staging(Err(e)))
                | Some(DocResult::Conflict(Err(e)))
                | Some(DocResult::Blame(Err(e)))
                | Some(DocResult::Spec(Err(e))) => view! {
                    <p class="detail-status detail-error">{format!("Couldn't load: {e}")}</p>
                }
                .into_view(),
                Some(DocResult::Diff(Ok(d))) => {
                    // Ignore a stale diff after switching documents.
                    if !matches!(&which_for_body, ViewerDoc::Diff { id } if *id == d.id) {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    diff_body(
                        &d,
                        nerd_icons.get(),
                        hunk_focus,
                        viewer_scroll,
                        viewer_columns,
                        printing,
                    )
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
                Some(DocResult::Conflict(Ok(panes))) => {
                    // Same staleness echo as every other arm: a response for a
                    // path that is no longer open is a late answer to a
                    // superseded request, and is dropped rather than painted.
                    if !matches!(&which_for_body, ViewerDoc::Conflict { path }
                                 if *path == panes.path)
                    {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    let Some(repo) = ctx.with_value(|c| c.frame.worktree_id.clone()) else {
                        return view! {
                            <p class="detail-status detail-error">
                                "This repository has no worktree id, so its conflicts cannot be resolved."
                            </p>
                        }
                        .into_view();
                    };
                    conflict_body(
                        repo,
                        &panes,
                        resolve_busy,
                        resolve_error,
                        status,
                        graph,
                        shell,
                    )
                }
                Some(DocResult::Blame(Ok(page))) => {
                    // Same staleness echo as every other arm.
                    if !matches!(&which_for_body, ViewerDoc::Blame { path, rev }
                                 if *path == page.path && *rev == page.rev)
                    {
                        return view! { <p class="detail-status">"Loading…"</p> }.into_view();
                    }
                    let history = blame_history.get().flatten();
                    blame_body(
                        &page,
                        history.as_ref(),
                        blame_focus,
                        blame_selection,
                        shell,
                        compare_anchor,
                    )
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
                <div
                    class="viewer-modal print-surface"
                    aria-busy=move || is_busy().to_string()
                >
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
                                on:click=move |_| print_now(printing)
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
                    // `accessible_rows_window`, and arrow keys then scroll this
                    // div natively instead of reaching `hunk_header_span`'s
                    // `on_keydown`.
                    <div
                        class="viewer-body"
                        tabindex="-1"
                        on:scroll=move |ev| {
                            use wasm_bindgen::JsCast;
                            if let Some(el) = ev.target()
                                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                            {
                                viewer_scroll.set(
                                    (el.scroll_top() as f64, el.client_height() as f64),
                                );
                            }
                            // Re-measure on scroll as well as on resize: the
                            // first measurement happens before this container
                            // exists, so without this the viewer would keep the
                            // 80-column fallback until the window was resized.
                            crate::features::diff::measure::remeasure(viewer_columns);
                        }
                    >{body}</div>
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
    /// All four panes of one conflicted path (M4.31a, #428).
    Conflict(Result<ConflictPanes, String>),
    /// Rename-aware blame for one path/revision (M5.33, #86). Only the
    /// default first page — `blame_body` owns paging further lines/history
    /// itself, the same way `spec_body`/`diff_body` own their own internal
    /// signals below this match.
    Blame(Result<BlamePage, String>),
}

/// Reduce the currently-open document to the identity
/// [`is_viewer_busy`] compares (#387) — data-only marshalling from
/// [`ViewerDoc`], no decision of its own. See
/// `features/readiness/core.rs`'s module doc for why this conversion, not
/// the predicate it feeds, is the part only the browser leg proves.
fn viewer_doc_identity(which: &ViewerDoc) -> DocIdentity {
    match which {
        ViewerDoc::Diff { id } => DocIdentity::Diff { id: id.clone() },
        ViewerDoc::File { id, path } => DocIdentity::File {
            id: id.clone(),
            path: path.clone(),
        },
        ViewerDoc::Staging { .. } => DocIdentity::Staging,
        ViewerDoc::Spec { spec } => DocIdentity::Spec { spec: spec.clone() },
        ViewerDoc::Conflict { path } => DocIdentity::Conflict { path: path.clone() },
        ViewerDoc::Blame { path, rev } => DocIdentity::Blame {
            path: path.clone(),
            rev: rev.clone(),
        },
    }
}

/// Reduce the viewer resource's resolved value to the same identity
/// granularity `viewer_doc_identity` produces — same rule: data-only, no
/// decision. Exhaustive over [`DocResult`] (no wildcard arm), on purpose:
/// a new `DocResult` variant added here without a matching arm fails the
/// build instead of silently reading as settled.
fn doc_result_outcome(result: Option<&DocResult>) -> FetchOutcome {
    match result {
        None => FetchOutcome::Pending,
        Some(DocResult::Diff(Err(_)))
        | Some(DocResult::File(Err(_)))
        | Some(DocResult::Staging(Err(_)))
        | Some(DocResult::Spec(Err(_)))
        | Some(DocResult::Conflict(Err(_)))
        | Some(DocResult::Blame(Err(_))) => FetchOutcome::Err,
        Some(DocResult::Diff(Ok(d))) => FetchOutcome::Ok(DocIdentity::Diff { id: d.id.clone() }),
        Some(DocResult::File(Ok(f))) => FetchOutcome::Ok(DocIdentity::File {
            id: f.id.clone(),
            path: f.path.clone(),
        }),
        Some(DocResult::Staging(Ok(_))) => FetchOutcome::Ok(DocIdentity::Staging),
        Some(DocResult::Spec(Ok(d))) => FetchOutcome::Ok(DocIdentity::Spec {
            spec: d.spec.clone(),
        }),
        Some(DocResult::Conflict(Ok(panes))) => FetchOutcome::Ok(DocIdentity::Conflict {
            path: panes.path.clone(),
        }),
        Some(DocResult::Blame(Ok(page))) => FetchOutcome::Ok(DocIdentity::Blame {
            path: page.path.clone(),
            rev: page.rev.clone(),
        }),
    }
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
        // The basis is IN the label, not ignored with `..` (M4.27, #80). Two
        // comparisons of the same pair answer different questions and produce
        // patches that look identical, so a label that omitted it would leave
        // the viewer unable to say which one is on screen — the precise
        // confusion the field was added to end. `...` is git's own notation
        // for the merge-base form, so it reads correctly to anyone who has
        // typed it.
        DiffSpec::CommitVsCommit {
            base,
            target,
            basis,
        } => match basis {
            ComparisonBasis::Direct => {
                format!("{} → {}", short(base.as_str()), short(target.as_str()))
            }
            ComparisonBasis::SinceMergeBase => {
                format!("{}...{}", short(base.as_str()), short(target.as_str()))
            }
        },
        DiffSpec::RefVsRef {
            base,
            target,
            basis,
        } => match basis {
            ComparisonBasis::Direct => format!("{} → {}", base.as_str(), target.as_str()),
            ComparisonBasis::SinceMergeBase => {
                format!("{}...{}", base.as_str(), target.as_str())
            }
        },
    }
}

/// An explicit source/target diff document (M2.16, #69).
///
/// Simpler than [`diff_body`] because [`SpecDiff`] carries no per-file stat
/// list — see its own doc for why that is a deliberate scope decision rather
/// than an omission (naming core's `DiffFile` from the protocol crate would
/// break the wasm build this crate exists to stay compatible with).
///
/// The patch renders through the same `accessible_rows_window` the other diff
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
            {accessible_rows_window(&flatten(&parse_unified_diff(&d.patch)), hunk_focus, "viewer", None, None)}
        </pre>
    }
    .into_view()
}

/// The full-diff document: the per-file stat list, then the whole unified
/// patch coloured line by line — the detail panel's Changes section, at
/// full-screen scale and without the panel's patch cap.
fn diff_body(
    d: &CommitDiff,
    nerd: bool,
    hunk_focus: RwSignal<GraphFocus>,
    scroll: RwSignal<(f64, f64)>,
    columns: RwSignal<usize>,
    printing: RwSignal<bool>,
) -> View {
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
    // Parsed and flattened ONCE, not per scroll tick: `Rc` so the closure
    // below can hold it without re-parsing a five-megabyte patch every time
    // the user drags the scrollbar.
    let flat = std::rc::Rc::new(flatten(&parse_unified_diff(&d.patch)));
    let patch = move || {
        let (top, viewport) = scroll.get();
        let cols = columns.get();
        let heights = CumulativeHeights::new(&row_heights(
            &flat.rows,
            DIFF_LINE_PX,
            // The viewer WRAPS (`.viewer-pre` is `pre-wrap`), unlike the panel.
            // `row_heights` models word wrapping at this column count (#362
            // step 2), and the count is measured rather than guessed (step 3).
            LineWrap::Wrapped { columns: cols },
        ));
        // Before the first scroll event the box has not been measured; fall
        // back to a viewport tall enough that the first paint is never short
        // of content. Same fallback the panel uses, same reason.
        let viewport = if viewport > 0.0 { viewport } else { 800.0 };
        // Printing renders the WHOLE patch with no spacers. The spacers are
        // what make a windowed viewer's scroll range honest on screen; on
        // paper they are blank pages, because the print stylesheet un-clips
        // the container and every spacer pixel becomes printable area with no
        // text in it. See `print_now`.
        let w = if printing.get() {
            crate::features::diff::core::RenderWindow {
                start: 0,
                end: flat.rows.len(),
                pad_top: 0.0,
                pad_bottom: 0.0,
            }
        } else {
            render_window(&heights, viewport, top, DIFF_OVERSCAN)
        };
        view! {
            <div style=format!("height:{}px", w.pad_top)></div>
            <pre class="detail-diff viewer-pre">
                {accessible_rows_window(
                    &flat, hunk_focus, "viewer", Some(w.start..w.end), None,
                )}
            </pre>
            <div style=format!("height:{}px", w.pad_bottom)></div>
        }
    };
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
        // WINDOWED as of #362. The panel was windowed by #350; this surface
        // was deferred, and the deferral outlived its tracking issue.
        //
        // What unblocked it, in order:
        //   1. MEASURED first (#362 step 1). A 4000-line patch renders in
        //      590ms unwindowed — fine. But the slope projects ~11s and
        //      ~246k DOM nodes at the 5MB cap, past the budget. The old
        //      argument ("the cap is 25x the panel's") was never measured.
        //   2. `row_heights` now models WORD wrapping, matching this
        //      element's `pre-wrap` + `break-word`, instead of
        //      `ceil(chars/columns)`. Verified against Chromium, which is
        //      what caught East Asian Wide characters being counted as one
        //      cell instead of two.
        //   3. The column count is measured off this container rather than
        //      estimated (`features::diff::measure`).
        {patch}
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

/// One conflict pane: its heading, and whatever its [`PaneState`] permits.
///
/// **Every non-text state renders as its own explicit note, never as an empty
/// `<pre>`.** That is #428's second and third acceptance criteria made real —
/// an empty text box asserts "this version was blank", which is a claim about
/// the repository, and only [`PaneState::Text`] is entitled to make it.
/// [`PaneState::describe`] owns the wording, so this function cannot drift
/// from the host-tested core's account of what each state means.
fn conflict_pane(pane: Pane, state: &PaneState) -> View {
    let body = match state {
        PaneState::Text { content, truncated } => {
            let truncated_note = truncated.then(|| {
                view! {
                    <p class="detail-status">
                        "Content truncated — this side is larger than the viewer's cap."
                    </p>
                }
            });
            view! {
                {truncated_note}
                <pre class="detail-diff viewer-pre">{content.clone()}</pre>
            }
            .into_view()
        }
        // Absent, Unreadable, Binary, AwaitingContent and ContentUnavailable
        // all land here — each one says what it is, in the core's own words.
        // `detail-error` marks the two that are faults rather than facts.
        other => {
            let class = match other {
                PaneState::Unreadable { .. } | PaneState::ContentUnavailable { .. } => {
                    "detail-status detail-error"
                }
                _ => "detail-status",
            };
            let text = other.describe();
            view! { <p class=class>{text}</p> }.into_view()
        }
    };
    view! {
        <section class="conflict-pane">
            <h3 class="conflict-pane-head">{pane.label()}</h3>
            {body}
        </section>
    }
    .into_view()
}

/// The two coordinates of one conflict document. Keeping them in one value
/// makes it impossible for the editor callbacks to clone a path without also
/// carrying the Frame's latched repository (#621, ADR 0109).
#[derive(Clone)]
struct ConflictTarget {
    repo: String,
    path: String,
}

/// The four-pane conflict view (M4.31a, #428).
///
/// Iterates [`Pane::ALL`] rather than naming four fields, so a pane cannot be
/// silently omitted — #428's first acceptance criterion is that all four are
/// reachable, and a hand-written list of three would satisfy every type check.
fn conflict_body(
    repo: String,
    panes: &ConflictPanes,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    status: StatusResource,
    graph: RwSignal<GraphCore>,
    shell: Shell,
) -> View {
    let target = ConflictTarget {
        repo,
        path: panes.path.clone(),
    };
    let rendered: Vec<View> = Pane::ALL
        .iter()
        .map(|p| conflict_pane(*p, panes.pane(*p)))
        .collect();

    // One button per `Resolution` variant — the vocabulary is closed, so the
    // three here ARE the three that exist. `TakeDeletion` is deliberately its
    // own control rather than "take the side that deleted it": those are the
    // same outcome but different requests, and only one of them stays correct
    // if the user has misread which side deleted what (see `Resolution`'s own
    // doc comment).
    // M4.31d (#430): the conflict's shape, in a sentence, above the controls.
    // `None` for an ordinary text conflict — a note on every conflict would
    // train the eye to skip it, and then the binary and delete/modify cases
    // would be skipped too.
    let note = panes.surface.note.clone().map(|text| {
        view! { <p class="detail-status conflict-note">{text}</p> }
    });

    let controls: Vec<View> = [
        (
            Resolution::TakeOurs,
            "Take ours",
            "conflict-take-ours",
            panes.surface.take_ours.clone(),
        ),
        (
            Resolution::TakeTheirs,
            "Take theirs",
            "conflict-take-theirs",
            panes.surface.take_theirs.clone(),
        ),
        (
            Resolution::TakeDeletion,
            "Delete file",
            "conflict-delete",
            panes.surface.take_deletion.clone(),
        ),
    ]
    .into_iter()
    .map(|(resolution, label, class, offered)| {
        // A withheld control is replaced by its reason, never rendered as a
        // dead button. `ConflictedFile::refuses` would answer these with a 409
        // anyway (protocol conflict.rs:343) — saying so here is the difference
        // between an explained absence and a walked-into server error.
        if let Err(withheld) = offered {
            let text = withheld.describe();
            return view! {
                <p class="detail-status conflict-withheld">{text}</p>
            }
            .into_view();
        }
        let target = target.clone();
        let on = move |_| {
            let target = target.clone();
            error.set(None);
            busy.set(true);
            spawn_local(async move {
                match resolve_conflict_request(&target.repo, &target.path, resolution).await {
                    Ok(()) => {
                        // BOTH, and the second one is the load-bearing half.
                        // `status.refetch()` updates the topbar chip's v1
                        // read; the Activity panel's conflicted list is a
                        // SEPARATE v2 resource keyed on the graph epoch
                        // (M2.15, #68). Refetching status alone left the chip
                        // saying "1 conflicted" while the panel still listed
                        // two rows — caught by the browser test, invisible to
                        // every unit test, because no unit test has a panel.
                        status.refetch();
                        graph.update(|g| {
                            g.force_bump();
                        });
                        shell.close_viewer();
                    }
                    // The server's own sentence, kept whole. It names which
                    // side and why; shortening it here would produce exactly
                    // the "it failed" this endpoint was built to avoid.
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        };
        view! {
            <button
                class=format!("viewer-btn {class}")
                prop:disabled=move || busy.get()
                on:click=on
            >
                {label}
            </button>
        }
        .into_view()
    })
    .collect();

    let refusal = move || {
        error.get().map(|e| {
            view! {
                <p class="detail-status detail-error conflict-refusal" role="alert">{e}</p>
            }
        })
    };

    // M4.31c (#432): the line/block resolver, gated on the SAME predicate the
    // server asks before executing one.
    let editor = conflict_editor(
        target,
        panes.surface.text_resolution_allowed,
        busy,
        error,
        status,
        graph,
        shell,
    );

    view! {
        <div class="viewer-doc-head">{panes.path.clone()}</div>
        {note}
        <div class="conflict-actions">{controls}</div>
        {refusal}
        {editor}
        <div class="conflict-panes">{rendered}</div>
    }
    .into_view()
}

/// The line/block resolver and free-text editor (M4.31c, #432, ADR 0069).
///
/// # Why this is gated, and on what
///
/// Only rendered when `surface.text_resolution_allowed` says so — the flag
/// #430 computed and nothing consumed until now. That flag is
/// `ConflictedFile::text_resolvable`, the SAME predicate the server asks
/// before executing a content resolution, so the button cannot appear for a
/// file the executor would refuse. Two copies of that rule is how #430 shipped
/// a wrong sentence; there is one.
///
/// # What lives here versus in `markers`
///
/// Nothing here decides what content a choice produces. Parsing the marker
/// file and composing the result are
/// [`markers::parse`](git_vista_conflicts::markers::parse) and
/// [`markers::compose`](git_vista_conflicts::markers::compose), both
/// framework-free and host-tested, for the reason ADR 0066 gives: `cargo test`
/// never compiles this file, so a decision made here would be pinned by
/// nothing. This function fetches, renders, and submits.
fn conflict_editor(
    target: ConflictTarget,
    allowed: bool,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    status: StatusResource,
    graph: RwSignal<GraphCore>,
    shell: Shell,
) -> View {
    use git_vista_conflicts::markers::{compose, conflict_count, parse, unchosen, Choice};

    if !allowed {
        return ().into_view();
    }

    // `None` until the user asks for it. The marker file is a second read, and
    // a conflict the user resolves with a whole-side button should not pay for
    // it — the same reasoning `fetch_conflict_panes` uses to skip binary blobs.
    let source = create_rw_signal::<Option<git_vista_protocol::ConflictSource>>(None);
    let choices = create_rw_signal::<Vec<Choice>>(Vec::new());
    // Set once the user edits the composed text by hand. From then on it is
    // authoritative and the per-block buttons stop rewriting it — silently
    // discarding someone's typing to re-apply a button is the one thing an
    // editor must never do.
    let edited = create_rw_signal::<Option<String>>(None);

    let open_target = target.clone();
    let open = move |_| {
        let target = open_target.clone();
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            match fetch_conflict_source(&target.repo, &target.path).await {
                Ok(src) => {
                    let n = conflict_count(&parse(&src.content));
                    choices.set(vec![Choice::Unchosen; n]);
                    edited.set(None);
                    source.set(Some(src));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let submit_target = target;
    let submit = move |_| {
        let Some(src) = source.get() else { return };
        let blocks = parse(&src.content);
        // Hand-edited text wins outright; otherwise compose from the choices.
        // `compose` returning None means a block is still unchosen, and the
        // button is disabled in that case — this is belt and braces, and it
        // refuses rather than submitting a guess.
        let Some(content) = edited.get().or_else(|| compose(&blocks, &choices.get())) else {
            error.set(Some(
                "Every conflict needs a choice before this can be applied.".to_string(),
            ));
            return;
        };
        let target = submit_target.clone();
        // Echoed back unchanged — never recomputed here. A client that
        // recomputed the stages it was given could only ever agree with itself.
        let stages = src.stages.clone();
        let token = src.source.clone();
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            match resolve_conflict_content_request(
                &target.repo,
                &target.path,
                stages,
                token,
                content,
            )
            .await
            {
                Ok(()) => {
                    // BOTH refreshes, for the reason #429 documents: the topbar
                    // chip and the Activity panel's conflicted list are
                    // separate resources, and refetching one leaves the other
                    // claiming a conflict that is resolved.
                    status.refetch();
                    graph.update(|g| {
                        g.force_bump();
                    });
                    shell.close_viewer();
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="conflict-editor">
            {move || match source.get() {
                None => view! {
                    <button
                        class="viewer-btn conflict-edit-open"
                        prop:disabled=move || busy.get()
                        on:click=open.clone()
                    >
                        "Resolve line by line…"
                    </button>
                }.into_view(),
                Some(src) => {
                    let blocks = parse(&src.content);
                    let total = conflict_count(&blocks);
                    let mut nth = 0usize;
                    let rows: Vec<View> = blocks
                        .iter()
                        .map(|b| match b {
                            git_vista_conflicts::markers::Block::Context { text } => {
                                view! { <pre class="conflict-blk conflict-blk-context">{text.clone()}</pre> }
                                    .into_view()
                            }
                            git_vista_conflicts::markers::Block::Conflict {
                                ours, theirs, ..
                            } => {
                                let i = nth;
                                nth += 1;
                                let (o, t) = (ours.clone(), theirs.clone());
                                let pick = move |c: Choice| {
                                    move |_| {
                                        // A hand-edit is never silently thrown
                                        // away by a later button press.
                                        if edited.get().is_none() {
                                            choices.update(|v| {
                                                if let Some(slot) = v.get_mut(i) {
                                                    *slot = c;
                                                }
                                            });
                                        }
                                    }
                                };
                                let chosen = move || choices.get().get(i).copied()
                                    .unwrap_or(Choice::Unchosen);
                                view! {
                                    <div class="conflict-blk conflict-blk-conflict">
                                        <div class="conflict-blk-head">
                                            {format!("Conflict {} of {}", i + 1, total)}
                                        </div>
                                        <pre class="conflict-blk-ours">{o}</pre>
                                        <pre class="conflict-blk-theirs">{t}</pre>
                                        <div class="conflict-blk-actions">
                                            <button
                                                class="viewer-btn"
                                                prop:disabled=move || busy.get() || edited.get().is_some()
                                                on:click=pick(Choice::Ours)
                                            >"Ours"</button>
                                            <button
                                                class="viewer-btn"
                                                prop:disabled=move || busy.get() || edited.get().is_some()
                                                on:click=pick(Choice::Theirs)
                                            >"Theirs"</button>
                                            <button
                                                class="viewer-btn"
                                                prop:disabled=move || busy.get() || edited.get().is_some()
                                                on:click=pick(Choice::Both)
                                            >"Both"</button>
                                            <span class="conflict-blk-state">
                                                // The words come from the
                                                // shared vocabulary, not from
                                                // here: the terminal shows the
                                                // same four, and two copies of
                                                // the wording drift.
                                                {move || chosen().describe()}
                                            </span>
                                        </div>
                                    </div>
                                }
                                .into_view()
                            }
                        })
                        .collect();

                    let blocks_for_area = blocks.clone();
                    let area = move || {
                        edited
                            .get()
                            .or_else(|| compose(&blocks_for_area, &choices.get()))
                            .unwrap_or_default()
                    };
                    // Stored, not a bare closure: it is read by two separate
                    // reactive scopes (the button's disabled state and the
                    // status line), and a plain closure moves into the first.
                    let blocks_for_open = StoredValue::new(blocks.clone());
                    let open_count =
                        move || unchosen(&blocks_for_open.get_value(), &choices.get()).len();

                    view! {
                        <div class="conflict-blocks">{rows}</div>
                        <div class="conflict-compose-head">
                            "Result — edit freely; typing here takes over from the buttons above."
                        </div>
                        <textarea
                            class="conflict-compose"
                            prop:value=area
                            on:input=move |ev| edited.set(Some(event_target_value(&ev)))
                        ></textarea>
                        <div class="conflict-actions">
                            <button
                                class="viewer-btn conflict-apply"
                                prop:disabled=move || {
                                    busy.get() || (edited.get().is_none() && open_count() > 0)
                                }
                                on:click=submit.clone()
                            >
                                "Apply this resolution"
                            </button>
                            <span class="conflict-blk-state">
                                {move || {
                                    let n = open_count();
                                    if edited.get().is_some() {
                                        "edited by hand".to_string()
                                    } else if n == 0 {
                                        "every conflict chosen".to_string()
                                    } else {
                                        format!("{n} conflict(s) still need a choice")
                                    }
                                }}
                            </span>
                        </div>
                    }
                    .into_view()
                }
            }}
        </div>
    }
    .into_view()
}
