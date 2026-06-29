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

use git_vista_core::model::{Graph, RefKind};

use crate::camera::{Camera, ZOOM_STEP};
use crate::color::{branch_color, BADGE_DARK, HEAD_BADGE, MERGE_FILL, TAG_BADGE};
use crate::geometry::{
    badge_text_dx, badge_text_y, badge_top_y, badge_width, edge_path, label_bottom_y, label_top_y,
    label_x, node_cx, node_cy, BADGE_GAP, BADGE_HEIGHT, BADGE_RADIUS, NODE_RADIUS,
};
use crate::text::truncate;

/// Commit messages longer than this are truncated with an ellipsis in the label
/// (the full text stays available via the node/label hover tooltip).
const MAX_SUMMARY_CHARS: usize = 60;

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
                <span class="subtitle">"vertical git history — drag to pan, pinch or scroll to zoom"</span>
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
    // Per-branch colour slot for each row, indexed by row number (rows are stored
    // in row order), so an edge can pick up its parent's branch colour.
    let row_color: Vec<usize> = graph.rows.iter().map(|gr| gr.color).collect();

    // Edges first so the nodes paint on top of them.
    let edges = graph
        .edges
        .iter()
        .map(|e| {
            let d = edge_path(e);
            // Colour a link by the branch of its parent (the lower endpoint): a
            // first-parent link keeps the branch colour, a merge link takes the
            // merged-in branch's colour — consistent with its commits.
            let color = branch_color(row_color[e.to_row]);
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
            let color = branch_color(gr.color);
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

    // Commit labels: a fixed text column to the right of the lanes, two lines per
    // row — any ref badges then the (truncated) message on top, the short hash and
    // author dimmed below. The full message stays available on hover.
    let text_x = label_x(graph.lane_count);
    let labels = graph
        .rows
        .iter()
        .map(|gr| {
            // Lay any ref badges out left-to-right from the start of the label
            // column; the message then starts just past them (or at the column
            // edge when there are none).
            let mut bx = text_x;
            let badges = gr
                .refs
                .iter()
                .map(|r| {
                    let w = badge_width(&r.name);
                    let x = bx;
                    bx += w + BADGE_GAP;
                    // Branch badges take their branch's colour (filled for local,
                    // outlined for remote); HEAD and tags get fixed colours.
                    let branch = branch_color(gr.color);
                    let (fill, stroke, text_fill) = match r.kind {
                        RefKind::Head => (HEAD_BADGE, HEAD_BADGE, BADGE_DARK),
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
                            {name}
                        </text>
                    }
                })
                .collect_view();
            let msg_x = bx; // past the last badge, or text_x when there were none

            let msg = truncate(&gr.commit.summary, MAX_SUMMARY_CHARS);
            let meta = format!("{} · {}", gr.commit.id.short(), gr.commit.author);
            view! {
                {badges}
                <text x=msg_x y=label_top_y(gr.row) class="label-msg">
                    {msg}
                    <title>{gr.commit.summary.clone()}</title>
                </text>
                <text x=text_x y=label_bottom_y(gr.row) class="label-meta">
                    {meta}
                </text>
            }
        })
        .collect_view();

    // Camera (pan/zoom) state.
    let camera = create_rw_signal(Camera::default());
    // Whether any pointer is currently pressed (drives the grab/grabbing cursor).
    let dragging = create_rw_signal(false);

    // --- Gesture tracking on Pointer Events ---------------------------------
    // Pointer Events unify mouse, pen and touch — crucially they fire for touch
    // on iOS Safari, where the old `movementX/Y`-on-mousemove + `wheel` approach
    // was dead (Safari reports `movementX/Y` as 0 for touch, and pinch never
    // raises a wheel event). We track every pressed pointer's position ourselves
    // and derive the gesture from how many are down:
    //   * one pointer  -> pan by the change in its position (finger or mouse);
    //   * two pointers -> pinch-zoom by the change in the distance between them,
    //                     anchored at their midpoint.
    // `touch-action: none` (styles.css) hands the browser's default touch
    // gestures to us so these fire cleanly.
    //
    // State is held in `store_value` (plain mutable cells, no reactivity needed):
    // the live list of `(pointer_id, x, y)` in client coords, and the previous
    // finger distance during a pinch.
    let pointers = store_value(Vec::<(i32, f64, f64)>::new());
    let pinch_dist = store_value(Option::<f64>::None);

    // The SVG's top-left in client coords, so a client position can be made
    // SVG-local for zoom anchoring (1 unit = 1px, no viewBox, so it's a shift).
    let svg_origin = |ev: &web_sys::PointerEvent| -> (f64, f64) {
        ev.current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .map(|el| {
                let r = el.get_bounding_client_rect();
                (r.left(), r.top())
            })
            .unwrap_or((0.0, 0.0))
    };

    // Press: record the pointer and capture it so moves keep arriving even if the
    // finger/cursor strays outside the SVG mid-gesture.
    let on_pointerdown = move |ev: web_sys::PointerEvent| {
        if let Some(target) = ev.current_target() {
            if let Ok(el) = target.dyn_into::<web_sys::Element>() {
                let _ = el.set_pointer_capture(ev.pointer_id());
            }
        }
        let id = ev.pointer_id();
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        pointers.update_value(|ps| match ps.iter_mut().find(|p| p.0 == id) {
            Some(slot) => *slot = (id, x, y),
            None => ps.push((id, x, y)),
        });
        // A new finger starting a pinch: reset the baseline so the first
        // two-pointer move just samples the distance rather than jumping.
        pinch_dist.set_value(None);
        dragging.set(true);
    };

    // Move: update this pointer's position, then pan or pinch by how it changed.
    let on_pointermove = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        let (ox, oy) = svg_origin(&ev);

        // Previous position of this pointer, then store the new one.
        let prev = pointers.with_value(|ps| ps.iter().find(|p| p.0 == id).map(|p| (p.1, p.2)));
        pointers.update_value(|ps| {
            if let Some(slot) = ps.iter_mut().find(|p| p.0 == id) {
                *slot = (id, x, y);
            }
        });

        let count = pointers.with_value(|ps| ps.len());
        if count >= 2 {
            // Pinch: zoom by the change in distance between the first two
            // pointers, anchored at their (SVG-local) midpoint.
            let (a, b) = pointers.with_value(|ps| (ps[0], ps[1]));
            let dist = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
            let (mx, my) = ((a.1 + b.1) / 2.0 - ox, (a.2 + b.2) / 2.0 - oy);
            let prev_dist = pinch_dist.get_value().unwrap_or(0.0);
            camera.update(|c| *c = c.pinched(prev_dist, dist, mx, my));
            pinch_dist.set_value(Some(dist));
        } else if let Some((px, py)) = prev {
            // Pan: 1:1 with the pointer's movement, independent of zoom.
            camera.update(|c| *c = c.panned(x - px, y - py));
        }
    };

    // Release / cancel: drop the pointer; end the drag once none remain. Reset
    // the pinch baseline so lifting one of two fingers doesn't make the next move
    // jump.
    let on_pointerup = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        pointers.update_value(|ps| ps.retain(|p| p.0 != id));
        pinch_dist.set_value(None);
        if pointers.with_value(|ps| ps.is_empty()) {
            dragging.set(false);
        }
    };

    // Wheel: zoom toward the cursor on desktop (trackpad/mouse). Up/away zooms
    // in, down/toward zooms out. Touch pinch is handled above, not here.
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
                {labels}
            </g>
        </svg>
    }
}
