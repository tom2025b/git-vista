//! The graph's wasm-only pieces — wasm only (M1.11, #64).
//!
//! Everything decidable is decided in [`super::core`], on the host, under test. This
//! file holds only what genuinely needs a DOM type: cancelling a link's navigation
//! when the "click" is actually the tail of a drag.

use leptos::StoredValue;

/// Cancel a link's navigation only when the "click" is actually the tail of a
/// drag/pan (desktop). Links are real SVG `<a target="_blank">` anchors, so a tap
/// is native link navigation — which works on iOS WebKit, where a scripted
/// `window.open` pop-up is silently blocked. `moved` is the gesture's drag flag
/// (set in pointermove). Moved here from `render/mod.rs` (M1.11, #64): the only
/// reason it lived there was proximity to `RenderCtx`, which has since moved to
/// the host-testable core.
pub fn suppress(moved: StoredValue<bool>, ev: web_sys::MouseEvent) {
    if moved.get_value() {
        ev.prevent_default();
    }
}
