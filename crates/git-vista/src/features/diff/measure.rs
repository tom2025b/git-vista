//! Measuring how many columns the viewer actually has (#362 step 3).
//!
//! [`super::rows::row_heights`] takes a `columns` count and models where the
//! browser wraps. Until now nothing produced that number: `LineWrap::Wrapped`
//! carried an estimate, and #362 recorded the estimate as the blocker. It is
//! not a hard problem — this codebase already measures the DOM in three places
//! (`gestures.rs`, `render/stubs.rs`) and already keeps a measurement current
//! across resizes (`features/shell/signals.rs`'s `install_mode_signal`). This
//! module is those two habits pointed at the viewer.
//!
//! ## The split, and why it is deliberate
//!
//! [`columns_for`] is pure arithmetic and unit-tested. Everything below it
//! touches the DOM and cannot be reached by `cargo test` at all — the browser
//! suite is the only thing that can prove it. That is the repo's standing
//! caution about wasm-gated code, applied on purpose rather than discovered
//! afterwards: a green unit suite here would say nothing about whether the
//! measurement is real.

// Only `columns_for` and its guards compile natively — that is the half a unit
// test can reach. Everything signal- or DOM-shaped is wasm-only, and is proven
// in ci/browser/tests/wrap-model.spec.mjs instead. Gating INSIDE the module
// rather than on `pub mod measure;` is what keeps the arithmetic testable;
// `staging_view` next door is gated wholesale because none of it is.
#[cfg(target_arch = "wasm32")]
use leptos::{create_rw_signal, on_cleanup, RwSignal, SignalSet};
#[cfg(target_arch = "wasm32")]
use std::time::Duration;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{closure::Closure, JsCast};

/// Fallback when the DOM cannot be measured — no window, a detached node, or a
/// container the browser has not laid out yet.
///
/// 80 is not arbitrary: it is the conventional terminal width and the value
/// the previous estimate assumed, so a build that loses its measurement
/// degrades to exactly the behaviour that shipped before rather than to
/// something new and untested.
pub const FALLBACK_COLUMNS: usize = 80;

/// How many whole monospace cells fit in `width_px`.
///
/// Guards, each for a state the DOM really produces rather than a hypothetical:
///
/// * **Non-finite or non-positive character width** — `getBoundingClientRect`
///   on an unlaid-out or `display: none` probe returns 0. Dividing by it gives
///   infinity, and `as usize` would saturate to a colossal column count that
///   makes every line one row and collapses the scroll range.
/// * **Container narrower than one cell** — a mid-animation panel can be 3px
///   wide. Zero columns would make `wrapped_rows` divide by zero; one column is
///   the smallest honest answer.
///
/// Floor, not round: a partly-visible cell cannot hold a character, and
/// rounding up would let a line claim a column it does not have and
/// under-measure its height.
pub fn columns_for(width_px: f64, char_px: f64) -> usize {
    if !width_px.is_finite() || !char_px.is_finite() || char_px <= 0.0 || width_px <= 0.0 {
        return FALLBACK_COLUMNS;
    }
    let cols = (width_px / char_px).floor();
    if cols < 1.0 {
        return 1;
    }
    // Cap before the cast: `as usize` saturates rather than wrapping, but a
    // 4-billion column count is nonsense to carry around either way.
    if cols > 100_000.0 {
        return 100_000;
    }
    cols as usize
}

/// Measure one monospace cell and the viewer's content width, in CSS pixels.
///
/// Returns `None` when there is nothing to measure — no window, or the viewer
/// is not on screen. Callers fall back rather than guessing.
#[cfg(target_arch = "wasm32")]
fn measure_viewer() -> Option<(f64, f64)> {
    let doc = web_sys::window()?.document()?;
    let body = doc.body()?;

    // The container. `.viewer-body` is the scrolling box the `<pre>` fills.
    let container = doc.query_selector(".viewer-body").ok().flatten()?;
    let width = container.get_bounding_client_rect().width();

    // A probe carrying the SAME classes as the rendered patch, so it inherits
    // the same font stack and size. Measuring the page's default font instead
    // would give a proportional-font width and a column count that is wrong by
    // whatever the two fonts differ by.
    //
    // 100 characters, then divided: one character's box is subject to
    // sub-pixel rounding, and a 0.4px error per cell is 30 columns of drift
    // across a wide viewer.
    let probe = doc.create_element("pre").ok()?;
    probe.set_class_name("detail-diff viewer-pre");
    let style = "position:absolute;visibility:hidden;left:-9999px;top:0;\
                 margin:0;padding:0;border:0;white-space:pre;width:auto";
    probe.set_attribute("style", style).ok()?;
    probe.set_text_content(Some(&"0".repeat(100)));
    body.append_child(&probe).ok()?;
    let char_px = probe.get_bounding_client_rect().width() / 100.0;
    let _ = body.remove_child(&probe);

    Some((width, char_px))
}

/// The viewer's column count, kept current across resizes.
///
/// Same shape as `install_mode_signal`: listen, debounce, publish with
/// `try_set`. The debounce is not politeness — a resize fires continuously
/// while a window is dragged, and re-measuring on every event would force a
/// layout read per frame against a `<pre>` holding the whole patch.
///
/// `try_set`, not `set`, for the reason that function documents: the timeout
/// can outlive its scope after `on_cleanup` has pulled the listener, and a
/// disposed scope has no layout to relayout.
#[cfg(target_arch = "wasm32")]
pub fn install_columns_signal() -> RwSignal<usize> {
    let columns = create_rw_signal(initial_columns());

    if let Some(win) = web_sys::window() {
        let cb = Closure::<dyn FnMut()>::new(move || {
            leptos::set_timeout(
                move || {
                    if let Some((w, c)) = measure_viewer() {
                        let _ = columns.try_set(columns_for(w, c));
                    }
                },
                Duration::from_millis(150),
            );
        });
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        let win2 = win.clone();
        on_cleanup(move || {
            let _ = win2.remove_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        });
    }

    columns
}

#[cfg(target_arch = "wasm32")]
fn initial_columns() -> usize {
    match measure_viewer() {
        Some((w, c)) => columns_for(w, c),
        // The viewer is not open yet on first install, so this is the normal
        // path rather than an error — `remeasure` runs when it opens.
        None => FALLBACK_COLUMNS,
    }
}

/// Re-measure now — for the moment the viewer opens, when no resize has fired
/// but the container has only just been laid out.
#[cfg(target_arch = "wasm32")]
pub fn remeasure(columns: RwSignal<usize>) {
    if let Some((w, c)) = measure_viewer() {
        columns.set(columns_for(w, c));
    }
    // No else: a failed measurement leaves the last good value in place, which
    // is a better answer than reverting to the fallback mid-session.
}

#[cfg(test)]
mod tests {
    use super::*;

    // These cover `columns_for` only. The DOM half is unreachable from
    // `cargo test` and is proven in ci/browser/tests/wrap-model.spec.mjs;
    // saying so here so a reader does not mistake this module for covered.

    #[test]
    fn a_normal_container_divides_into_whole_cells() {
        assert_eq!(columns_for(800.0, 8.0), 100);
    }

    #[test]
    fn a_partial_cell_does_not_count() {
        // 803/8 = 100.375. The 101st cell is only fractionally visible and
        // cannot hold a character; claiming it would under-measure heights.
        assert_eq!(columns_for(803.0, 8.0), 100);
    }

    #[test]
    fn a_zero_character_width_falls_back_instead_of_dividing_by_zero() {
        // getBoundingClientRect on an unlaid-out probe returns 0. Without this
        // guard the division is infinity and the cast saturates to a colossal
        // column count, which makes every line one row and collapses the
        // scroll range to nothing.
        assert_eq!(columns_for(800.0, 0.0), FALLBACK_COLUMNS);
    }

    #[test]
    fn non_finite_measurements_fall_back() {
        assert_eq!(columns_for(f64::NAN, 8.0), FALLBACK_COLUMNS);
        assert_eq!(columns_for(800.0, f64::NAN), FALLBACK_COLUMNS);
        assert_eq!(columns_for(f64::INFINITY, 8.0), FALLBACK_COLUMNS);
    }

    #[test]
    fn a_hidden_container_falls_back_rather_than_reporting_zero() {
        // A display:none or not-yet-open viewer measures 0 wide. Zero columns
        // would divide by zero downstream in `wrapped_rows`.
        assert_eq!(columns_for(0.0, 8.0), FALLBACK_COLUMNS);
    }

    #[test]
    fn a_container_narrower_than_one_cell_still_reports_one() {
        // Mid-animation panels really are a few pixels wide. One column is the
        // smallest answer that keeps the wrap model sane.
        assert_eq!(columns_for(3.0, 8.0), 1);
    }

    #[test]
    fn an_absurd_measurement_is_capped_rather_than_saturating() {
        assert_eq!(columns_for(1e30, 1.0), 100_000);
    }

    #[test]
    fn the_fallback_matches_the_estimate_that_shipped_before_measurement() {
        // A build that loses its measurement must degrade to the previously
        // shipped behaviour, not to something new and untested.
        assert_eq!(FALLBACK_COLUMNS, 80);
    }
}
