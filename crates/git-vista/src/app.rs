//! Leptos components for git-vista.
//!
//! Phase 1: render a static vertical commit graph from hardcoded demo data
//! ([`crate::graph::fake_graph`]) as inline SVG — one circle per commit, one
//! curved path per commit->parent link, lanes laid out left to right. Later
//! phases swap the demo data for real history over the Tauri IPC boundary and
//! layer on pan/zoom, labels, and refs.
//!
//! This module is deliberately just *view assembly*: the spatial math lives in
//! [`crate::geometry`] and the lane palette in [`crate::color`], so the
//! component only decides what to draw, not where things land or what colour
//! they are.

use leptos::*;

use crate::color::{lane_color, MERGE_FILL};
use crate::geometry::{canvas_size, edge_path, node_cx, node_cy, NODE_RADIUS};
use crate::graph::fake_graph;

#[component]
pub fn App() -> impl IntoView {
    let graph = fake_graph();

    let (width, height) = canvas_size(graph.lane_count, graph.rows.len());

    // Edges first so the nodes paint on top of them.
    let edges = graph
        .edges
        .iter()
        .map(|e| {
            let d = edge_path(e);
            // Colour a link by the busier of its two lanes: a branch/merge takes
            // the side-branch colour, a straight link keeps its own.
            let color = lane_color(e.from_lane.max(e.to_lane));
            view! {
                <path d=d fill="none" stroke=color stroke-width="2" stroke-linecap="round" />
            }
        })
        .collect_view();

    let nodes = graph
        .rows
        .iter()
        .map(|gr| {
            let cx = node_cx(gr.lane);
            let cy = node_cy(gr.row);
            let color = lane_color(gr.lane);
            // Draw merge commits as a hollow ring so they stand out from the
            // ordinary filled dots.
            let merge = gr.commit.is_merge();
            let fill = if merge { MERGE_FILL } else { color };
            let stroke_width = if merge { "3" } else { "2" };
            view! {
                <circle
                    cx=cx
                    cy=cy
                    r=NODE_RADIUS
                    fill=fill
                    stroke=color
                    stroke-width=stroke_width
                >
                    <title>{format!("{} — {}", gr.commit.id.short(), gr.commit.summary)}</title>
                </circle>
            }
        })
        .collect_view();

    view! {
        <main class="app">
            <header class="topbar">
                <h1>"git-vista"</h1>
                <span class="subtitle">"vertical git history"</span>
            </header>
            <section class="graph">
                <svg
                    class="graph-svg"
                    width=width
                    height=height
                    viewBox=format!("0 0 {width} {height}")
                >
                    {edges}
                    {nodes}
                </svg>
            </section>
        </main>
    }
}
