//! Leptos components for git-vista.
//!
//! Phase 1 rendered a static vertical commit graph from hardcoded demo data
//! ([`crate::graph::fake_graph`]) as inline SVG — one circle per commit, one
//! curved path per commit->parent link, lanes laid out left to right.
//!
//! Phase 2 makes that canvas interactive: the whole graph lives inside a single
//! `<g transform>` driven by a [`Camera`], the SVG fills its panel, and pointer
//! drags pan while the mouse wheel zooms toward the cursor. Later phases swap the
//! demo data for real history over the Tauri IPC boundary and layer on labels
//! and refs.
//!
//! This module is deliberately just *view assembly*: the spatial math lives in
//! [`crate::geometry`], the lane palette in [`crate::color`], and the pan/zoom
//! arithmetic in [`crate::camera`], so the component only decides what to draw
//! and wires up events — it owns no constants or maths of its own.

use leptos::*;
use wasm_bindgen::JsCast;

use crate::camera::{Camera, ZOOM_STEP};
use crate::color::{lane_color, MERGE_FILL};
use crate::geometry::{edge_path, node_cx, node_cy, NODE_RADIUS};
use crate::graph::fake_graph;

#[component]
pub fn App() -> impl IntoView {
    let graph = fake_graph();

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

    // Camera (pan/zoom) state, plus whether a drag is currently in progress.
    let camera = create_rw_signal(Camera::default());
    let dragging = create_rw_signal(false);

    // Press: begin a drag and capture the pointer so moves keep arriving even
    // if the cursor leaves the SVG mid-drag.
    let on_pointerdown = move |ev: web_sys::PointerEvent| {
        if let Some(target) = ev.target() {
            if let Ok(el) = target.dyn_into::<web_sys::Element>() {
                let _ = el.set_pointer_capture(ev.pointer_id());
            }
        }
        dragging.set(true);
    };
    // Move: while dragging, pan by the pointer's per-event delta (1:1 with the
    // cursor, regardless of zoom).
    let on_pointermove = move |ev: web_sys::PointerEvent| {
        if dragging.get() {
            camera.update(|c| *c = c.panned(ev.movement_x() as f64, ev.movement_y() as f64));
        }
    };
    // Release / cancel: end the drag.
    let on_pointerup = move |_ev: web_sys::PointerEvent| dragging.set(false);

    // Wheel: zoom toward the cursor. Up/away zooms in, down/toward zooms out.
    let on_wheel = move |ev: web_sys::WheelEvent| {
        ev.prevent_default(); // don't let the page scroll
        let factor = if ev.delta_y() < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        let (sx, sy) = (ev.offset_x() as f64, ev.offset_y() as f64);
        camera.update(|c| *c = c.zoomed_at(factor, sx, sy));
    };

    view! {
        <main class="app">
            <header class="topbar">
                <h1>"git-vista"</h1>
                <span class="subtitle">"vertical git history — drag to pan, scroll to zoom"</span>
            </header>
            <section class="graph">
                <svg
                    class="graph-svg"
                    class:grabbing=move || dragging.get()
                    on:pointerdown=on_pointerdown
                    on:pointermove=on_pointermove
                    on:pointerup=on_pointerup
                    on:pointercancel=on_pointerup
                    on:wheel=on_wheel
                >
                    <g transform=move || camera.get().transform()>
                        {edges}
                        {nodes}
                    </g>
                </svg>
            </section>
        </main>
    }
}
