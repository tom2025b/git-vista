//! Pan / zoom gesture handling and the window-level listeners.
//!
//! Pointer Events unify mouse, pen and touch — crucially they fire for touch on
//! iOS Safari, where the old `movementX/Y`-on-mousemove + `wheel` approach was
//! dead (Safari reports `movementX/Y` as 0 for touch, and pinch never raises a
//! wheel event). The handlers here track every pressed pointer's position and
//! derive the gesture from how many are down (one → pan, two → pinch), plus the
//! desktop wheel-zoom. They're free functions taking a [`GestureState`] `Copy`
//! bundle, so `app.rs` wires them onto the `<svg>` as thin closures. This module
//! also installs the window `resize` and `keydown` listeners (with cleanup).

use leptos::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::camera::{Camera, ZOOM_STEP};
use crate::state::{MenuData, Overlays};

/// Pointer travel (CSS px) past which a press becomes a pan/drag rather than a
/// tap. Keeps a tap-to-open-link from being eaten by the pan handler.
const DRAG_THRESHOLD: f64 = 4.0;

/// Current browser window inner height in CSS px, or a sane default when it can't
/// be read. The window is always at least as tall as the SVG (the topbar sits
/// above it), so this is a safe *upper* bound on the viewport height — the
/// virtualizer may draw a few extra rows just past the bottom, never too few.
pub fn window_inner_height() -> f64 {
    web_sys::window()
        .and_then(|w| w.inner_height().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(800.0)
}

/// The live gesture state, held in `store_value` cells (plain mutable state, no
/// reactivity needed) plus the camera/dragging signals the handlers drive. A
/// `Copy` bundle so the `<svg>` event closures each take one handle.
///
/// `pointers` is the live list of `(pointer_id, x, y)` in client coords;
/// `pinch_dist` the previous finger distance during a pinch; `down_xy` where the
/// gesture started; `moved` whether it has travelled far enough to count as a
/// drag (vs a tap) — until then we neither pan nor capture, so a tap reaches the
/// child element's link click handler.
#[derive(Clone, Copy)]
pub struct GestureState {
    pub camera: RwSignal<Camera>,
    /// Whether any pointer is currently pressed (drives the grab/grabbing cursor).
    pub dragging: RwSignal<bool>,
    pub menu: RwSignal<Option<MenuData>>,
    pub moved: StoredValue<bool>,
    pub pointers: StoredValue<Vec<(i32, f64, f64)>>,
    pub pinch_dist: StoredValue<Option<f64>>,
    pub down_xy: StoredValue<Option<(f64, f64)>>,
}

/// The SVG's top-left in client coords, so a client position can be made
/// SVG-local for zoom anchoring (1 unit = 1px, no viewBox, so it's a shift).
fn svg_origin(ev: &web_sys::PointerEvent) -> (f64, f64) {
    ev.current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .map(|el| {
            let r = el.get_bounding_client_rect();
            (r.left(), r.top())
        })
        .unwrap_or((0.0, 0.0))
}

/// Press: just record the pointer. We deliberately do NOT capture it or start a
/// drag yet — that waits until the pointer actually moves (see [`on_pointer_move`]),
/// so a plain tap stays a tap and its click reaches the link underneath.
pub fn on_pointer_down(g: GestureState, ev: web_sys::PointerEvent) {
    let GestureState { menu, moved, pointers, pinch_dist, down_xy, .. } = g;
    // Any press on the canvas dismisses an open menu. A tap on a dot reopens it
    // on the click that follows (pointerdown fires before click), so this just
    // handles "tap empty space / start panning to close".
    menu.set(None);
    let id = ev.pointer_id();
    let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
    let first = pointers.with_value(|ps| ps.is_empty());
    pointers.update_value(|ps| match ps.iter_mut().find(|p| p.0 == id) {
        Some(slot) => *slot = (id, x, y),
        None => ps.push((id, x, y)),
    });
    if first {
        down_xy.set_value(Some((x, y)));
        moved.set_value(false);
    }
    // A new finger starting a pinch: reset the baseline so the first
    // two-pointer move just samples the distance rather than jumping.
    pinch_dist.set_value(None);
}

/// Move: update this pointer's position, then pan or pinch by how it changed.
pub fn on_pointer_move(g: GestureState, ev: web_sys::PointerEvent) {
    let GestureState { camera, dragging, moved, pointers, pinch_dist, down_xy, .. } = g;
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

    // Capture now that the gesture is live, so moves keep arriving even if the
    // pointer leaves the SVG. (Deferred to first move so taps don't capture.)
    let capture = |ev: &web_sys::PointerEvent| {
        if let Some(t) = ev.current_target() {
            if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                let _ = el.set_pointer_capture(ev.pointer_id());
            }
        }
    };

    let count = pointers.with_value(|ps| ps.len());
    if count >= 2 {
        // Two fingers => a pinch, never a tap.
        moved.set_value(true);
        dragging.set(true);
        capture(&ev);
        // Zoom by the change in distance between the first two pointers,
        // anchored at their (SVG-local) midpoint.
        let (a, b) = pointers.with_value(|ps| (ps[0], ps[1]));
        let dist = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
        let (mx, my) = ((a.1 + b.1) / 2.0 - ox, (a.2 + b.2) / 2.0 - oy);
        let prev_dist = pinch_dist.get_value().unwrap_or(0.0);
        camera.update(|c| *c = c.pinched(prev_dist, dist, mx, my));
        pinch_dist.set_value(Some(dist));
    } else if let Some((px, py)) = prev {
        // Single pointer: only treat it as a drag once it crosses the
        // threshold from where it started; below that it's still a tap.
        if !moved.get_value() {
            let far = down_xy.get_value().is_some_and(|(sx, sy)| {
                ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() > DRAG_THRESHOLD
            });
            if far {
                moved.set_value(true);
                dragging.set(true);
                capture(&ev);
            }
        }
        if moved.get_value() {
            // Pan 1:1 with the pointer's movement, independent of zoom.
            camera.update(|c| *c = c.panned(x - px, y - py));
        }
    }
}

/// Release / cancel: drop the pointer; end the drag once none remain. Reset
/// the pinch baseline so lifting one of two fingers doesn't make the next move
/// jump. `moved` is left for the click that may follow, and reset on next press.
pub fn on_pointer_up(g: GestureState, ev: web_sys::PointerEvent) {
    let GestureState { dragging, pointers, pinch_dist, .. } = g;
    let id = ev.pointer_id();
    pointers.update_value(|ps| ps.retain(|p| p.0 != id));
    pinch_dist.set_value(None);
    if pointers.with_value(|ps| ps.is_empty()) {
        dragging.set(false);
    }
}

/// Wheel: zoom toward the cursor on desktop (trackpad/mouse). Up/away zooms
/// in, down/toward zooms out. Touch pinch is handled above, not here.
pub fn on_wheel(camera: RwSignal<Camera>, ev: web_sys::WheelEvent) {
    ev.prevent_default(); // don't let the page scroll
    let factor = if ev.delta_y() < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
    let (sx, sy) = (ev.offset_x() as f64, ev.offset_y() as f64);
    camera.update(|c| *c = c.zoomed_at(factor, sx, sy));
}

/// Refresh the viewport height on window resize (rotate the iPad, resize the
/// desktop window). Removed on cleanup so a graph reload — which reruns the
/// canvas with fresh signals — doesn't stack a second live listener writing
/// to the disposed `vp_h`.
pub fn install_resize_listener(vp_h: RwSignal<f64>) {
    if let Some(win) = web_sys::window() {
        let cb = Closure::<dyn FnMut()>::new(move || vp_h.set(window_inner_height()));
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        let win2 = win.clone();
        on_cleanup(move || {
            let _ = win2.remove_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        });
    }
}

/// Phase 13 — keyboard shortcuts (a window keydown listener):
///   * Esc backs out of whatever overlay is open — the menu first, then a modal,
///     then the detail panel. It's only a shortcut: every overlay also closes via
///     its Cancel button or a backdrop tap, since some iPad Magic Keyboards have
///     no physical Esc key.
///   * +/= zoom in, -/_ zoom out (anchored at the viewport centre, as there's no
///     cursor for a key press), 0 resets pan & zoom.
///   * r re-reads the repository (same as the Refresh button).
/// Non-Esc keys are ignored while a text field is focused (the commit / URL
/// boxes) and when a modifier is held, so typing an "r" — or the browser's own
/// Cmd/Ctrl-R reload — is left untouched. Removed on cleanup, like the resize
/// listener above, so a reload doesn't leave duplicate handlers behind.
pub fn install_key_listener(camera: RwSignal<Camera>, reload: RwSignal<u32>, overlays: Overlays) {
    let Overlays { menu, commit_dialog, confirm_op, detail_id, .. } = overlays;
    if let Some(win) = web_sys::window() {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" {
                    if menu.get_untracked().is_some() {
                        menu.set(None);
                    } else if commit_dialog.get_untracked().is_some() {
                        commit_dialog.set(None);
                    } else if confirm_op.get_untracked().is_some() {
                        confirm_op.set(None);
                    } else if detail_id.get_untracked().is_some() {
                        detail_id.set(None);
                    }
                    return;
                }
                // Leave keys alone while typing in a field, or when a modifier is held.
                let typing = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .map(|el| {
                        let tag = el.tag_name();
                        tag.eq_ignore_ascii_case("textarea") || tag.eq_ignore_ascii_case("input")
                    })
                    .unwrap_or(false);
                if typing || ev.ctrl_key() || ev.meta_key() || ev.alt_key() {
                    return;
                }
                let centre = || {
                    let vw = web_sys::window()
                        .and_then(|w| w.inner_width().ok())
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1200.0);
                    (vw / 2.0, window_inner_height() / 2.0)
                };
                match ev.key().as_str() {
                    "+" | "=" => {
                        let (cx, cy) = centre();
                        camera.update(|c| *c = c.zoomed_at(ZOOM_STEP, cx, cy));
                    }
                    "-" | "_" => {
                        let (cx, cy) = centre();
                        camera.update(|c| *c = c.zoomed_at(1.0 / ZOOM_STEP, cx, cy));
                    }
                    "0" => camera.set(Camera::default()),
                    "r" | "R" => reload.update(|n| *n = n.wrapping_add(1)),
                    _ => {}
                }
            },
        );
        let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        let win2 = win.clone();
        on_cleanup(move || {
            let _ = win2.remove_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        });
    }
}
