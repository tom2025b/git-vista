//! "Print Graph": a clean, print-oriented view of the *whole* git graph.
//!
//! The interactive canvas can't print well — it's virtualized (only on-screen
//! rows exist in the DOM), pan/zoomed by a camera transform, and dark-themed.
//! So this module builds a separate, static SVG of the entire graph — every
//! row, edge, badge, label and stub, no virtualization, no camera — on a white
//! sheet with dark text, and shows it in a full-screen overlay with a size
//! picker and one Print / Save PDF button (the same `window.print()` flow as
//! viewer.rs; on iPad the print sheet's share button saves the PDF).
//!
//! While open it stamps `data-print` on `<html>` so the `@media print` rules
//! print only the sheet, scaled to the page width and flowing across pages.
//!
//! Since M1.10 (#63) "the whole graph" is whatever the mounted [`RenderCtx`]
//! holds, and the sheet is built from *that* — never from a snapshot taken when
//! the view opened, and never by fetching pages of its own. Printing half a
//! history would be a quietly wrong document, so the topbar button that opens
//! this view is disabled until every page has landed (`history_complete`), and
//! any epoch change closes it. Reading straight out of the aggregate is what
//! makes "what you printed is what was loaded" true by construction.

use leptos::*;

use git_vista_core::color::{branch_color, BADGE_DARK, HEAD_BADGE, TAG_BADGE};
use git_vista_core::model::RefKind;

use crate::datetime::local_timestamp;
use crate::features::graph::core::RenderCtx;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    node_cx, node_cy, stub_headroom_for, stub_node_cy, stub_path, BADGE_GAP, BADGE_HEIGHT,
    BADGE_RADIUS, NODE_RADIUS, PAD_Y, ROW_HEIGHT,
};
use crate::icons::icon_set;
use crate::text::truncate;

/// Same truncation the interactive labels use (render/labels.rs).
const MAX_SUMMARY_CHARS: usize = 60;

/// Rough per-character advance (px) of the 13px monospace message text, used
/// only to size the sheet so no label is clipped.
const MSG_CHAR_W: i32 = 8;

/// The settled commit-link rule shared by the print and interactive labels:
/// only GitHub-backed commits known to be on the remote have a reachable page.
pub(crate) fn commit_github_url(
    repo_url: Option<&str>,
    on_remote: bool,
    commit_id: &str,
) -> Option<String> {
    repo_url.and_then(|base| on_remote.then(|| format!("{base}/commit/{commit_id}")))
}

fn print_commit_url(repo_url: Option<&str>, on_remote: bool, commit_id: &str) -> Option<String> {
    commit_github_url(repo_url, on_remote, commit_id)
}

/// Stamp (or clear) `data-print` on `<html>` — shared contract with viewer.rs.
fn set_print_attr(on: bool) {
    if let Some(root) = document().document_element() {
        if on {
            let _ = root.set_attribute("data-print", "graph");
        } else {
            let _ = root.remove_attribute("data-print");
        }
    }
}

/// How big the printed graph is drawn before it goes to paper/PDF. It scales
/// the *rendered* SVG (as a % of the page-width sheet) — the graph's own
/// geometry is never touched — so it only ever affects the printout, never the
/// interactive canvas. `Large`/`ExtraLarge` make every dot, line, badge and
/// label proportionally bigger and let the graph flow across more pages.
#[derive(Clone, Copy, PartialEq)]
enum PrintScale {
    Normal,
    Large,
    ExtraLarge,
}

impl PrintScale {
    /// Rendered width of the graph as a percentage of the page-width sheet.
    fn pct(self) -> u32 {
        match self {
            Self::Normal => 100,
            Self::Large => 150,
            Self::ExtraLarge => 200,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Large => "Large",
            Self::ExtraLarge => "Extra Large",
        }
    }
}

/// One button of the Normal / Large / Extra Large size picker: selects `this`
/// and lights up while it is the chosen size.
fn scale_button(scale: RwSignal<PrintScale>, this: PrintScale) -> impl IntoView {
    view! {
        <button
            class="scale-btn"
            class:active=move || scale.get() == this
            on:click=move |_| scale.set(this)
        >
            {this.label()}
        </button>
    }
}

/// The "Print Graph" overlay: a white preview sheet holding the full static
/// graph SVG, a Normal/Large/Extra-Large size picker, and one Print / Save PDF
/// button (plus Close). Rendered while `open` is true.
///
/// `ctx` is the mounted canvas's own aggregate — the same `StoredValue` every
/// row builder reads, borrowed, not copied. That is why this view lives inside
/// `graph_canvas`: it has no history of its own to go stale, and it is disposed
/// with the canvas the moment the epoch it belongs to is retired.
pub fn print_graph_view(
    ctx: StoredValue<RenderCtx>,
    open: RwSignal<bool>,
    nerd_icons: RwSignal<bool>,
) -> impl IntoView {
    // Print magnification. Read only by the sheet wrapper's reactive width
    // below, so toggling it re-styles that one wrapper without rebuilding the
    // SVG — and never touches the interactive canvas.
    let scale = create_rw_signal(PrintScale::Normal);
    move || {
        let is_open = open.get();
        set_print_attr(is_open);
        is_open.then(|| {
            // Built per *open*, straight out of the live aggregate: the sheet is
            // a rendering of what is loaded right now, not a snapshot kept
            // alongside it that a later page could contradict.
            let sheet = ctx.with_value(|c| graph_sheet(c, nerd_icons.get()));
            let repo = ctx
                .with_value(|c| c.frame.repo_label.clone())
                .unwrap_or_default();
            view! {
                <div class="print-graph-modal print-surface">
                    <div class="viewer-head">
                        <span class="viewer-title">"Print Graph"</span>
                        <span class="viewer-actions">
                            <span class="print-scale" role="group" aria-label="Print size">
                                <span class="print-scale-label">"Size"</span>
                                {scale_button(scale, PrintScale::Normal)}
                                {scale_button(scale, PrintScale::Large)}
                                {scale_button(scale, PrintScale::ExtraLarge)}
                            </span>
                            <button
                                class="viewer-btn"
                                title="Opens the print sheet — on iPad choose the \
                                       share icon (or pinch the preview open) and \
                                       ‘Save to Files’ to keep it as a PDF"
                                on:click=move |_| {
                                    if let Some(w) = web_sys::window() { let _ = w.print(); }
                                }
                            >
                                "Print / Save PDF"
                            </button>
                            <button
                                class="viewer-btn viewer-close"
                                title="Close"
                                on:click=move |_| open.set(false)
                            >
                                "Close ×"
                            </button>
                        </span>
                    </div>
                    <div class="viewer-body">
                        <div class="print-sheet">
                            <div class="print-sheet-head">{repo}</div>
                            // The size picker scales the printout by rendering
                            // the SVG at a % of the sheet width; the SVG keeps
                            // its own geometry, so the live graph is unaffected.
                            <div
                                class="print-scale-box"
                                style:width=move || format!("{}%", scale.get().pct())
                            >
                                {sheet}
                            </div>
                        </div>
                    </div>
                </div>
            }
        })
    }
}

/// Build the whole graph as one static SVG: every edge, dot, glyph, badge,
/// label tier and stub — the interactive builders' geometry and colours, minus
/// links, menus, virtualization and LOD, with dark text for paper.
///
/// Every scrap of it comes out of the one mounted aggregate (M1.10, #63): rows,
/// edges and the monotonic per-row label x from
/// [`LoadedHistory`](crate::features::graph::core::LoadedHistory), stubs from its
/// `resolved_stubs()`, repo metadata from the Frame. No `Graph`, no second
/// `text_x`, no per-row colour vector — a second copy assembled for the printout
/// would be exactly the copy that disagrees with the graph on screen.
fn graph_sheet(c: &RenderCtx, nerd: bool) -> View {
    let ic = icon_set(nerd);
    let rows = &c.loaded.rows;
    // Grown page by page and read back through the aggregate's accessor: the
    // whole-`Graph` `label_x_per_row` has nothing to be handed here, because
    // paged history never holds a whole `Graph`.
    let text_x = c.loaded.text_x();
    // Only stubs whose anchor commit is loaded can be placed at all; the
    // aggregate has already resolved their absolute rows and lanes.
    let stubs = c.loaded.resolved_stubs();
    let headroom = stub_headroom_for(stubs.iter().map(|s| (s.anchor_row, s.stub.depth))) as i32;

    // Sheet size: tall enough for every row (plus stub headroom above row 0),
    // wide enough that no row's badges + message overrun the right edge. Rows
    // and their label x are *zipped* rather than indexed by `gr.row`: the two
    // are the same length by construction, and zipping makes that structural
    // instead of a bounds check repeated on every row.
    let height = headroom + PAD_Y * 2 + (rows.len().saturating_sub(1) as i32) * ROW_HEIGHT;
    let width = rows
        .iter()
        .zip(text_x)
        .map(|(gr, &tx)| {
            let badges: i32 = gr
                .refs
                .iter()
                .map(|r| {
                    let icon = ref_icon(ic, &r.kind);
                    badge_width(&format!("{icon} {}", r.name)) + BADGE_GAP
                })
                .sum();
            let msg = truncate(&gr.commit.summary, MAX_SUMMARY_CHARS)
                .chars()
                .count() as i32;
            let meta = format!(
                "  {} · {} · {}",
                gr.commit.id.short(),
                gr.commit.author,
                local_timestamp(gr.commit.time),
            )
            .chars()
            .count() as i32;
            tx + badges + msg.max(meta) * MSG_CHAR_W
        })
        .max()
        .unwrap_or(400)
        + PAD_Y;

    // Edges — same colour rule as render/edges.rs: first-parent links wear the
    // child's branch colour, merge links the merged-in parent's. Both endpoints
    // are looked up *checked*, for the same reason the interactive builder does
    // it: an edge whose rows aren't both loaded has nothing to connect, and a
    // panic here would take the overlay down instead of dropping one line.
    let edges = c
        .loaded
        .edges
        .iter()
        .filter_map(|e| {
            let (from, to) = (rows.get(e.from_row)?, rows.get(e.to_row)?);
            let d = edge_path(e);
            let is_first_parent = from.commit.parents.first() == Some(&to.commit.id);
            let color = branch_color(if is_first_parent {
                from.color
            } else {
                to.color
            });
            Some(view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            })
        })
        .collect_view();

    // Stub connectors + rings + names (render/stubs.rs, minus the tap targets).
    let stub_paths = stubs
        .iter()
        .map(|s| {
            let color = branch_color(s.stub.color);
            let d = stub_path(s.anchor_lane, s.anchor_row, s.lane, s.stub.depth);
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
        })
        .collect_view();
    let stub_tips = stubs
        .iter()
        .map(|s| {
            let color = branch_color(s.stub.color);
            let sx = node_cx(s.lane);
            let sy = stub_node_cy(s.anchor_row, s.stub.depth);
            view! {
                // White-filled ring on paper (the canvas fill is the dark bg).
                <circle cx=sx cy=sy r=NODE_RADIUS fill="#ffffff" stroke=color stroke-width="2" />
                <text x=sx + NODE_RADIUS + 6 y=sy + 4 class="stub-label" fill=color>
                    {truncate(&s.stub.name, 24)}
                </text>
            }
        })
        .collect_view();

    // Dots + the per-node glyph (merge vs commit), like nodes.rs.
    let nodes = rows
        .iter()
        .map(|gr| {
            let cx = node_cx(gr.lane);
            let cy = node_cy(gr.row);
            let color = branch_color(gr.color);
            let icon = if gr.commit.parents.len() > 1 {
                ic.merge
            } else {
                ic.commit
            };
            view! {
                <circle cx=cx cy=cy r=NODE_RADIUS fill=color stroke=color stroke-width="2" />
                <text
                    x=cx - NODE_RADIUS - 5
                    y=cy + 4
                    text-anchor="end"
                    class="nf node-icon"
                    fill=color
                >
                    {icon}
                </text>
            }
        })
        .collect_view();

    // The two label tiers, dark-on-white: badges keep their colours, the
    // message/meta text goes near-black/grey (classes pg-msg / pg-meta) so the
    // printout reads like a document rather than inverted screen colours.
    let labels = rows
        .iter()
        .zip(text_x)
        .map(|(gr, &tx)| {
            let mut bx = tx;
            let badges = gr
                .refs
                .iter()
                .map(|r| {
                    let icon = ref_icon(ic, &r.kind);
                    let w = badge_width(&format!("{icon} {}", r.name));
                    let x = bx;
                    bx += w + BADGE_GAP;
                    let branch = branch_color(gr.color);
                    // Same colour mapping as labels.rs — except HEAD, whose
                    // near-white fill needs a grey outline to exist on paper.
                    let (fill, stroke, text_fill) = match r.kind {
                        RefKind::Head => (HEAD_BADGE, "#57606a", BADGE_DARK),
                        RefKind::Tag => (TAG_BADGE, TAG_BADGE, BADGE_DARK),
                        RefKind::Branch => (branch, branch, BADGE_DARK),
                        RefKind::RemoteBranch => ("none", branch, branch),
                    };
                    let name = r.name.clone();
                    view! {
                        <rect
                            x=x
                            y=badge_top_y(gr.row)
                            width=w
                            height=BADGE_HEIGHT
                            rx=BADGE_RADIUS
                            ry=BADGE_RADIUS
                            fill=fill
                            stroke=stroke
                            stroke-width="1"
                        />
                        <text
                            x=x + badge_text_dx()
                            y=badge_text_y(gr.row)
                            class="badge-text"
                            fill=text_fill
                        >
                            <tspan class="nf">{icon}</tspan>
                            {format!(" {name}")}
                        </text>
                    }
                })
                .collect_view();
            let msg = truncate(&gr.commit.summary, MAX_SUMMARY_CHARS);
            let meta = format!(
                " {} · {} · {}",
                gr.commit.id.short(),
                gr.commit.author,
                local_timestamp(gr.commit.time),
            );
            let commit_url =
                print_commit_url(c.frame.repo_url.as_deref(), gr.on_remote, &gr.commit.id.0);
            let commit_label = view! {
                <text x=bx y=label_top_y(gr.row) class="label-msg pg-msg">{msg}</text>
                <text x=tx y=label_bottom_y(gr.row) class="label-meta pg-meta">
                    <tspan class="nf">{ic.commit}</tspan>
                    {meta}
                </text>
            };
            let commit_label = match commit_url {
                Some(url) => view! {
                    <g>
                        <a href=url target="_blank" rel="noopener">
                            {commit_label}
                        </a>
                    </g>
                }
                .into_view(),
                None => commit_label.into_view(),
            };
            view! {
                {badges}
                {commit_label}
            }
        })
        .collect_view();

    view! {
        <svg
            class="print-graph-svg"
            viewBox=format!("0 0 {width} {height}")
            xmlns="http://www.w3.org/2000/svg"
        >
            <g transform=format!("translate(0, {headroom})")>
                {edges}
                {stub_paths}
                {nodes}
                {labels}
                {stub_tips}
            </g>
        </svg>
    }
    .into_view()
}

/// The badge glyph for a ref kind — same mapping as render/labels.rs.
fn ref_icon(ic: &crate::icons::GitIcons, kind: &RefKind) -> &'static str {
    match kind {
        RefKind::Head => ic.commit,
        RefKind::Tag => ic.tag,
        RefKind::Branch => ic.branch,
        RefKind::RemoteBranch => ic.branch_alt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT_ID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn pushed_commits_print_as_links() {
        assert_eq!(
            print_commit_url(Some("https://github.com/owner/repo"), true, COMMIT_ID),
            Some(format!("https://github.com/owner/repo/commit/{COMMIT_ID}"))
        );
    }

    #[test]
    fn unpushed_commits_print_unlinked() {
        assert_eq!(
            print_commit_url(Some("https://github.com/owner/repo"), false, COMMIT_ID),
            None
        );
    }

    #[test]
    fn no_remote_prints_unlinked_and_does_not_panic() {
        assert_eq!(print_commit_url(None, true, COMMIT_ID), None);
    }
}
