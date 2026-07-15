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

use leptos::*;

use git_vista_core::color::{branch_color, BADGE_DARK, HEAD_BADGE, TAG_BADGE};
use git_vista_core::model::{Graph, RefKind};

use crate::datetime::local_timestamp;
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    label_x_per_row, node_cx, node_cy, stub_headroom, stub_node_cy, stub_path, BADGE_GAP,
    BADGE_HEIGHT, BADGE_RADIUS, NODE_RADIUS, PAD_Y, ROW_HEIGHT,
};
use crate::icons::icon_set;
use crate::text::truncate;

/// Same truncation the interactive labels use (render/labels.rs).
const MAX_SUMMARY_CHARS: usize = 60;

/// Rough per-character advance (px) of the 13px monospace message text, used
/// only to size the sheet so no label is clipped.
const MSG_CHAR_W: i32 = 8;

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
pub fn print_graph_view(
    graph: Graph,
    open: RwSignal<bool>,
    nerd_icons: RwSignal<bool>,
) -> impl IntoView {
    // The graph parks in a StoredValue so the reactive closure below can read
    // it per open without cloning it into every render.
    let graph = store_value(graph);
    // Print magnification. Read only by the sheet wrapper's reactive width
    // below, so toggling it re-styles that one wrapper without rebuilding the
    // SVG — and never touches the interactive canvas.
    let scale = create_rw_signal(PrintScale::Normal);
    move || {
        let is_open = open.get();
        set_print_attr(is_open);
        is_open.then(|| {
            let sheet = graph.with_value(|g| graph_sheet(g, nerd_icons.get()));
            let repo = graph
                .with_value(|g| g.repo_label.clone())
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
fn graph_sheet(g: &Graph, nerd: bool) -> View {
    let ic = icon_set(nerd);
    let text_x = label_x_per_row(g);
    let row_color: Vec<usize> = g.rows.iter().map(|gr| gr.color).collect();
    let headroom = stub_headroom(&g.stubs) as i32;

    // Sheet size: tall enough for every row (plus stub headroom above row 0),
    // wide enough that no row's badges + message overrun the right edge.
    let height = headroom + PAD_Y * 2 + (g.rows.len().saturating_sub(1) as i32) * ROW_HEIGHT;
    let width = g
        .rows
        .iter()
        .map(|gr| {
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
            text_x[gr.row] + badges + msg.max(meta) * MSG_CHAR_W
        })
        .max()
        .unwrap_or(400)
        + PAD_Y;

    // Edges — same colour rule as render/edges.rs: first-parent links wear the
    // child's branch colour, merge links the merged-in parent's.
    let edges = g
        .edges
        .iter()
        .map(|e| {
            let d = edge_path(e);
            let child = &g.rows[e.from_row].commit;
            let parent_oid = &g.rows[e.to_row].commit.id;
            let is_first_parent = child.parents.first() == Some(parent_oid);
            let color_row = if is_first_parent {
                e.from_row
            } else {
                e.to_row
            };
            let color = branch_color(row_color[color_row]);
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
        })
        .collect_view();

    // Stub connectors + rings + names (render/stubs.rs, minus the tap targets).
    let stub_paths = g
        .stubs
        .iter()
        .map(|s| {
            let color = branch_color(s.color);
            let d = stub_path(s.anchor_lane, s.anchor_row, s.lane, s.depth);
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
        })
        .collect_view();
    let stub_tips = g
        .stubs
        .iter()
        .map(|s| {
            let color = branch_color(s.color);
            let sx = node_cx(s.lane);
            let sy = stub_node_cy(s.anchor_row, s.depth);
            view! {
                // White-filled ring on paper (the canvas fill is the dark bg).
                <circle cx=sx cy=sy r=NODE_RADIUS fill="#ffffff" stroke=color stroke-width="2" />
                <text x=sx + NODE_RADIUS + 6 y=sy + 4 class="stub-label" fill=color>
                    {truncate(&s.name, 24)}
                </text>
            }
        })
        .collect_view();

    // Dots + the per-node glyph (merge vs commit), like nodes.rs.
    let nodes = g
        .rows
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
    let labels = g
        .rows
        .iter()
        .map(|gr| {
            let mut bx = text_x[gr.row];
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
            view! {
                {badges}
                <text x=bx y=label_top_y(gr.row) class="label-msg pg-msg">{msg}</text>
                <text x=text_x[gr.row] y=label_bottom_y(gr.row) class="label-meta pg-meta">
                    <tspan class="nf">{ic.commit}</tspan>
                    {meta}
                </text>
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
