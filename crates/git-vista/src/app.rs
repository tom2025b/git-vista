//! Leptos components for git-vista.
//!
//! Phase 4: on startup the frontend `fetch`es the real, laid-out commit
//! [`Graph`] from the backend's `/api/commits` endpoint (same origin) and renders
//! it as inline SVG — one circle per commit, one curved path per commit->parent
//! link, lanes laid out left to right. The whole graph lives inside a single
//! `<g transform>` driven by a [`Camera`]: pointer drags pan, the wheel zooms
//! toward the cursor (Phase 2). Earlier phases used hardcoded demo data; that's
//! gone from the render path now.
//!
//! This module is deliberately just *view assembly* + the data fetch: spatial
//! math lives in [`crate::geometry`], the lane palette in [`crate::color`], the
//! pan/zoom arithmetic in [`crate::camera`], and the lane layout is computed on
//! the backend by `git-vista-core`.

use leptos::*;
use wasm_bindgen::JsCast;

use gloo_net::http::Request;

use git_vista_core::model::Graph;

use crate::camera::{Camera, ZOOM_STEP};
use crate::color::{lane_color, MERGE_FILL};
use crate::geometry::{edge_path, node_cx, node_cy, NODE_RADIUS};

/// Fetch the laid-out graph from the backend. Relative URL → same origin as the
/// served SPA, so no CORS and no hardcoded host.
async fn fetch_graph() -> Result<Graph, String> {
    Request::get("/api/commits")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Graph>()
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn App() -> impl IntoView {
    // Load real history once on startup. `create_local_resource` because the
    // fetch future isn't `Send` (wasm).
    let graph = create_local_resource(|| (), |_| fetch_graph());

    view! {
        <main class="app">
            <header class="topbar">
                <h1>"git-vista"</h1>
                <span class="subtitle">"vertical git history — drag to pan, scroll to zoom"</span>
            </header>
            <section class="graph">
                {move || match graph.get() {
                    None => view! { <p class="status">"Loading history…"</p> }.into_view(),
                    Some(Err(e)) => view! {
                        <p class="status error">{format!("Failed to load history: {e}")}</p>
                    }
                    .into_view(),
                    Some(Ok(g)) => graph_canvas(g).into_view(),
                }}
            </section>
        </main>
    }
}

/// Render a loaded [`Graph`] as a pan/zoomable SVG canvas.
fn graph_canvas(graph: Graph) -> impl IntoView {
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
    }
}
