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

// The API reads the same graph signal synchronously, including while a new
// historical seed is loading. There is no one-effect delay in the write gate.
thread_local! {
    static VIEW_GUARD: std::cell::Cell<Option<leptos::RwSignal<super::core::GraphCore>>> = const { std::cell::Cell::new(None) };
}

pub fn install_view_guard(graph: leptos::RwSignal<super::core::GraphCore>) {
    VIEW_GUARD.with(|slot| slot.set(Some(graph)));
    leptos::on_cleanup(|| VIEW_GUARD.with(|slot| slot.set(None)));
}

pub fn refuse_if_historical() -> Result<(), String> {
    use leptos::SignalGetUntracked;
    VIEW_GUARD.with(|slot| match slot.get() {
        Some(graph) if graph.get_untracked().view().is_historical() => {
            Err("Historical view is view only. Return to live before making changes.".into())
        }
        Some(_) => Ok(()),
        // `App` installs this before any interactive surface mounts. Treat a
        // missing installation as a broken invariant, not permission to write.
        None => Err("Historical view state is unavailable. Reload before making changes.".into()),
    })
}
