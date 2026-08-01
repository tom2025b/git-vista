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
use crate::features::a11y::focus::{FocusMove, GraphFocus};
use crate::features::graph::core::GraphCore;
use crate::features::shell::signals::Shell;
use crate::geometry::{drag_threshold, node_cy};

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

/// The *visual* viewport size in CSS px — what's actually on screen right now.
/// On iOS Safari `inner_height` over-reports while the URL bar / toolbars are
/// expanded, so a menu clamped to it can still hang past the visible bottom;
/// `visualViewport` tracks the true visible box. Falls back to the window
/// inner size (then a sane default) where the API is missing. Used to place
/// the context menu, so it's clamped against what the user can really see.
pub fn viewport_size() -> (f64, f64) {
    let win = web_sys::window();
    if let Some(vv) = win.as_ref().and_then(|w| w.visual_viewport()) {
        let (w, h) = (vv.width(), vv.height());
        if w > 0.0 && h > 0.0 {
            return (w, h);
        }
    }
    let width = win
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    (width, window_inner_height())
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
    /// The overlay stack, so a press on the canvas can dismiss an open context menu
    /// through the one writer rather than poking its signal (M1.11, #64, Task 8).
    pub shell: Shell,
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
    let GestureState {
        shell,
        moved,
        pointers,
        pinch_dist,
        down_xy,
        ..
    } = g;
    // Any press on the canvas dismisses an open menu. A tap on a dot reopens it
    // on the click that follows (pointerdown fires before click), so this just
    // handles "tap empty space / start panning to close".
    shell.close_menu();
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
    let GestureState {
        camera,
        dragging,
        moved,
        pointers,
        pinch_dist,
        down_xy,
        ..
    } = g;
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
            // Per-pointer-type slop: a finger wobbles, a mouse doesn't (issue #115).
            let threshold = drag_threshold(&ev.pointer_type());
            let far = down_xy
                .get_value()
                .is_some_and(|(sx, sy)| ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() > threshold);
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
/// jump.
///
/// `moved` (issue #139): the node/stub hit targets open their menu on
/// *pointerup*, and a target's handler runs before this ancestor handler in
/// the bubble — the pan gate is still set when they read it. The label links,
/// though, suppress on the *click* that follows this event, so the flag can't
/// be cleared synchronously here or a pan ending on a link would open it.
/// Clearing on the next animation frame keeps both consumers correct while
/// making sure a lost/interrupted gesture can never leave a stale
/// `moved=true` swallowing every later tap (the pre-#139 failure mode).
pub fn on_pointer_up(g: GestureState, ev: web_sys::PointerEvent) {
    let GestureState {
        dragging,
        moved,
        pointers,
        pinch_dist,
        ..
    } = g;
    let id = ev.pointer_id();
    pointers.update_value(|ps| ps.retain(|p| p.0 != id));
    pinch_dist.set_value(None);
    if pointers.with_value(|ps| ps.is_empty()) {
        dragging.set(false);
        request_animation_frame(move || moved.set_value(false));
    }
}

/// Wheel: zoom toward the cursor on desktop (trackpad/mouse). Up/away zooms
/// in, down/toward zooms out. Touch pinch is handled above, not here.
pub fn on_wheel(camera: RwSignal<Camera>, ev: web_sys::WheelEvent) {
    ev.prevent_default(); // don't let the page scroll
    let factor = if ev.delta_y() < 0.0 {
        ZOOM_STEP
    } else {
        1.0 / ZOOM_STEP
    };
    let (sx, sy) = (ev.offset_x() as f64, ev.offset_y() as f64);
    camera.update(|c| *c = c.zoomed_at(factor, sx, sy));
}

/// Screen-px clearance kept between a keyboard-focused row and the viewport
/// edge (M1.13, #65 keyboard-access gap). Loosely modelled on the overscan
/// rows (`OVERSCAN_ROWS` in `canvas.rs`) rather than tied to them exactly: this
/// only needs to be "comfortably more than zero" so the freshly-panned-to row
/// is not sitting flush against the edge, not to match the virtualizer's own
/// margin.
const ROW_FOCUS_MARGIN_PX: f64 = 60.0;

/// Roving-tabindex keyboard handling for one commit row's hit circle (M1.13,
/// #65 keyboard-access gap) — see `features::a11y::focus` for the state
/// machine this drives and why it is shaped the way it is.
///
/// Called from `render::nodes::build_node`'s own `on:keydown`, i.e. once per
/// row's `<circle class="node-hit">`, not once for the whole graph. That is
/// deliberate, not an oversight: a keydown can only reach this handler by
/// firing on an element that currently holds real DOM focus, and the only
/// focusable elements inside the canvas are the `.node-hit` circles — so
/// whichever row's closure receives the event *is* the focused row, with no
/// need to ask `GraphFocus` which row that was. `activate` is that row's own
/// "open my context menu at (x, y)" closure, already holding that row's
/// commit data — reusing it here (rather than re-deriving `MenuData` from a
/// row index in a second place) is what keeps the pointer and keyboard paths
/// agreeing about what "activating" a commit means by construction.
///
/// **What this cannot prove by itself.** That `tabindex="-1"` circles are
/// reliably focusable via `.focus()` on iPad Safari, that a `keydown` fired on
/// an SVG `<circle>` actually reaches this closure the way it does on an HTML
/// button, and that the `:focus-visible` ring painted by `styles.css` is
/// legible once it lands here. All three need a real device — see the task
/// report's `unverified` list.
pub fn on_node_keydown(
    focus: RwSignal<GraphFocus>,
    camera: RwSignal<Camera>,
    vp_h: RwSignal<f64>,
    ev: web_sys::KeyboardEvent,
    activate: &impl Fn(f64, f64),
) {
    let dir = match ev.key().as_str() {
        "ArrowDown" => Some(FocusMove::Next),
        "ArrowUp" => Some(FocusMove::Prev),
        "Home" => Some(FocusMove::First),
        "End" => Some(FocusMove::Last),
        _ => None,
    };
    if let Some(dir) = dir {
        // Don't let the arrow keys additionally scroll the page (there is no
        // scrollable ancestor here, but Home/End on some browsers act on the
        // document by default regardless).
        ev.prevent_default();
        let Some(next) = focus.try_update(|f| f.mv(dir)).flatten() else {
            // An empty graph: nothing to move to, nothing to focus.
            return;
        };
        // Bring the destination row on screen *before* the next frame's DOM
        // query below — the row list is virtualized
        // (`viewport::visible_row_range`), so a `Home`/`End` jump (or several
        // arrow presses in a row) can select a row with no mounted `<circle>`
        // to call `.focus()` on yet. See `Camera::ensure_row_visible`.
        let target_y = f64::from(node_cy(next));
        camera.update(|c| {
            *c = c.ensure_row_visible(vp_h.get_untracked(), target_y, ROW_FOCUS_MARGIN_PX)
        });
        focus_row_next_frame(next);
        return;
    }

    match ev.key().as_str() {
        "Enter" | " " => {
            ev.prevent_default();
            // The event's own target — this row's hit circle, since that is
            // the only thing that can have been focused — supplies the
            // screen coordinates a tap would have, so the menu opens in the
            // same place either way.
            let (x, y) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .map(|el| {
                    let r = el.get_bounding_client_rect();
                    (r.left() + r.width() / 2.0, r.top() + r.height() / 2.0)
                })
                .unwrap_or((0.0, 0.0));
            activate(x, y);
        }
        "Escape" => {
            focus.update(|f| f.escape());
            // `GraphFocus::escape` only updates the model; real DOM focus has
            // to be moved off the element separately, or the ring stays
            // painted on a row the model no longer considers focused.
            if let Some(el) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::SvgElement>().ok())
            {
                let _ = el.blur();
            }
        }
        _ => {}
    }
}

/// Query for row `i`'s hit circle and move DOM focus onto it, one animation
/// frame from now.
///
/// The delay is required, not a nicety: `camera.update(...)` in
/// [`on_node_keydown`] changes a *signal*, and Leptos applies the DOM update
/// that mounts a newly-visible row asynchronously relative to the signal
/// write, not inside it. Calling `.focus()` synchronously would race that
/// update and, for a jump onto a currently-unmounted row, find nothing to
/// focus. One `request_animation_frame` — the same deferral
/// `on_pointer_up` uses to clear `moved` — gives the reactive system a paint
/// to catch up in first.
fn focus_row_next_frame(row: usize) {
    request_animation_frame(move || {
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let selector = format!(".node-hit[data-row-index=\"{row}\"]");
        if let Ok(Some(el)) = doc.query_selector(&selector) {
            if let Ok(el) = el.dyn_into::<web_sys::SvgElement>() {
                let _ = el.focus();
            }
        }
    });
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
///
/// Non-Esc keys are ignored while a text field is focused (the commit / URL
/// boxes) and when a modifier is held, so typing an "r" — or the browser's own
/// Cmd/Ctrl-R reload — is left untouched. Removed on cleanup, like the resize
/// listener above, so a reload doesn't leave duplicate handlers behind.
/// `home` is a *signal*, not a value (M1.10, #63): with paged history the home
/// camera moves after mount — a later page can raise the lane high-water and
/// grow the stub cascade past the top edge, which shifts `Camera::home`. Taking
/// it by value here would freeze the reset target at the page-1 view, so `0`
/// would recentre on a layout that no longer exists. It is read *untracked at
/// key-press time* for the same reason the Reset-view button does: this is a
/// listener, not a reactive computation, and nothing should re-run when home
/// moves.
pub fn install_key_listener(
    camera: RwSignal<Camera>,
    home: RwSignal<Camera>,
    graph: RwSignal<GraphCore>,
    shell: Shell,
) {
    if let Some(win) = web_sys::window() {
        let cb =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" {
                    // A handler closer to the event may have consumed this Escape —
                    // the diff's hunk navigation calls `prevent_default` when Escape
                    // disengages it (detail.rs), and that consumption must not ALSO
                    // dismiss the overlay the user is still inside. stop_propagation
                    // alone can't protect it here: this listener and Leptos's
                    // delegated handlers share the window target, and same-target
                    // listeners all run regardless of stop_propagation.
                    if ev.default_prevented() {
                        return;
                    }
                    // Topmost first, and "topmost" is now a fact the shell holds rather
                    // than an `if/else if` chain this handler had to keep in step with the
                    // overlay set. That chain is the bug this replaces: it covered five of
                    // the six overlays and silently omitted the Activity panel, so Esc
                    // could not close it (M1.11, #64, Task 8). (Esc is a desktop
                    // convenience only — every overlay keeps a visible close control,
                    // since the iPad Magic Keyboard has no Esc key.)
                    shell.dismiss_top();
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
                    // The graph's home view — not the raw identity, so a repo
                    // whose stub cascades overshoot the top edge resets to a
                    // view that actually shows them (see `Camera::home`). Read
                    // at press time, so it is wherever the pages landed so far
                    // put it, never the mount-time value.
                    "0" => camera.set(home.get_untracked()),
                    "r" | "R" => {
                        graph.update(|g| {
                            g.force_bump();
                        });
                    }
                    _ => {}
                }
            });
        let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        let win2 = win.clone();
        on_cleanup(move || {
            let _ =
                win2.remove_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        });
    }
}
