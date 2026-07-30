//! Pure target-size arithmetic and the accessible names this crate renders (M1.12, #65).
//!
//! Framework-free and host-tested, like every other `core.rs` under `features/`. Nothing
//! here reads the DOM; it turns numbers and states into verdicts and literal strings, so
//! the parts of issue #65 that *are* arithmetic can be settled without a device.

/// The minimum interactive target, in CSS pixels, that issue #65 names: "interactive
/// targets meet 44-by-44 CSS pixel guidance".
///
/// 44 is Apple's Human Interface Guidelines figure and also WCAG 2.2 SC 2.5.5 (Target
/// Size, Enhanced). Both axes, not the diagonal or the area.
pub const MIN_TAP_TARGET_PX: f64 = 44.0;

/// The accessible name of the graph region landmark, rendered by `app::App` as the
/// `aria-label` on `<section class="graph">`.
///
/// A `<section>` is only exposed as a `region` landmark when it has an accessible name;
/// without one it is an anonymous generic container and a screen-reader user has no
/// landmark to jump to. The literal lives here so `audit`'s markup tripwire and the
/// markup itself cannot drift apart.
pub const GRAPH_REGION_LABEL: &str = "Commit history graph";

/// The extra radius, in user units, that `render::nodes` and `render::stubs` draw around
/// each commit dot as an invisible pointer target (`r = NODE_RADIUS + 8`).
///
/// Mirrored here so [`node_hit_extent_px`] can be exercised on the host — `render/` is
/// wasm-only and cannot be linked into a host test. `audit`'s
/// `node_hit_padding_still_matches_the_render_code` tripwire is what keeps the mirror
/// honest: change the literal in either render module and that test fails.
pub const NODE_HIT_PADDING: f64 = 8.0;

/// A rendered interactive target's size in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapTarget {
    pub width_px: f64,
    pub height_px: f64,
}

/// Whether a target clears [`MIN_TAP_TARGET_PX`], and by how much it misses if not.
///
/// The shortfalls are reported per axis rather than as one number because the two fixes
/// are different — a short button is a padding change, a narrow one usually is not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetVerdict {
    /// Both axes are at least [`MIN_TAP_TARGET_PX`].
    Meets,
    /// At least one axis is short. A shortfall of `0.0` on an axis means that axis is
    /// fine and the *other* one is what failed.
    Undersized { short_by_x_px: f64, short_by_y_px: f64 },
}

impl TapTarget {
    pub fn new(width_px: f64, height_px: f64) -> Self {
        Self {
            width_px,
            height_px,
        }
    }

    /// A square target of the given side, which is the shape every circular hit area in
    /// the graph reduces to (the bounding box is what a finger has to land inside).
    pub fn square(side_px: f64) -> Self {
        Self::new(side_px, side_px)
    }

    pub fn verdict(self) -> TargetVerdict {
        let short_x = (MIN_TAP_TARGET_PX - self.width_px).max(0.0);
        let short_y = (MIN_TAP_TARGET_PX - self.height_px).max(0.0);
        if short_x == 0.0 && short_y == 0.0 {
            TargetVerdict::Meets
        } else {
            TargetVerdict::Undersized {
                short_by_x_px: short_x,
                short_by_y_px: short_y,
            }
        }
    }
}

/// The on-screen diameter, in CSS pixels, of the invisible hit circle over a commit dot.
///
/// The hit circle is drawn *inside* the camera's `<g transform>`, so it is not a fixed
/// number of CSS pixels: it grows and shrinks with the zoom. `camera.rs` records why the
/// conversion is this simple — "the SVG has no `viewBox`, so one user unit equals one
/// CSS pixel" — which makes the diameter `2 * (r + padding) * scale`.
///
/// This is the arithmetic that makes the commit dot's #65 status *decidable* rather than
/// a matter of opinion: it is not one size, it is a size per zoom level.
pub fn node_hit_extent_px(node_radius: f64, hit_padding: f64, camera_scale: f64) -> f64 {
    2.0 * (node_radius + hit_padding) * camera_scale
}

/// The smallest camera scale at which the commit-dot hit circle reaches
/// [`MIN_TAP_TARGET_PX`].
///
/// Returns `f64::INFINITY` for a degenerate (zero or negative) hit radius: no zoom makes
/// a zero-sized target tappable, and that is a truer answer than a division result.
pub fn min_camera_scale_for_guidance(node_radius: f64, hit_padding: f64) -> f64 {
    let diameter = 2.0 * (node_radius + hit_padding);
    if diameter <= 0.0 {
        return f64::INFINITY;
    }
    MIN_TAP_TARGET_PX / diameter
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The threshold is written out as a literal here on purpose. Every other assertion
    /// in this module compares against `MIN_TAP_TARGET_PX`, so if that constant were
    /// silently changed to 24 those tests would all keep passing while proving nothing —
    /// this is the one place that pins the number issue #65 actually names.
    #[test]
    fn guidance_threshold_is_forty_four_css_pixels() {
        assert_eq!(MIN_TAP_TARGET_PX, 44.0);
    }

    #[test]
    fn graph_region_label_is_the_exact_words_rendered() {
        assert_eq!(GRAPH_REGION_LABEL, "Commit history graph");
    }

    /// Each case states its expected verdict as a literal rather than re-deriving it
    /// from `verdict()`, and the boundary is checked from both sides on both axes.
    #[test]
    fn tap_target_verdicts_are_literal_per_case() {
        assert_eq!(TapTarget::new(44.0, 44.0).verdict(), TargetVerdict::Meets);
        assert_eq!(TapTarget::new(60.0, 44.0).verdict(), TargetVerdict::Meets);
        assert_eq!(TapTarget::new(44.0, 60.0).verdict(), TargetVerdict::Meets);

        assert_eq!(
            TapTarget::new(43.0, 44.0).verdict(),
            TargetVerdict::Undersized {
                short_by_x_px: 1.0,
                short_by_y_px: 0.0,
            }
        );
        assert_eq!(
            TapTarget::new(44.0, 43.0).verdict(),
            TargetVerdict::Undersized {
                short_by_x_px: 0.0,
                short_by_y_px: 1.0,
            }
        );
        assert_eq!(
            TapTarget::new(30.0, 30.0).verdict(),
            TargetVerdict::Undersized {
                short_by_x_px: 14.0,
                short_by_y_px: 14.0,
            }
        );
    }

    /// A target that is generously wide but short still fails — the guidance is per
    /// axis, and a 200x28 toolbar button is exactly the shape that tempts people to
    /// treat "big enough overall" as passing.
    #[test]
    fn a_wide_but_short_target_is_undersized() {
        assert_eq!(
            TapTarget::new(200.0, 28.0).verdict(),
            TargetVerdict::Undersized {
                short_by_x_px: 0.0,
                short_by_y_px: 16.0,
            }
        );
    }

    #[test]
    fn square_target_uses_the_same_side_on_both_axes() {
        let t = TapTarget::square(30.0);
        assert_eq!(t.width_px, 30.0);
        assert_eq!(t.height_px, 30.0);
    }

    /// The commit dot at the app's default zoom, with the numbers the render code
    /// actually uses (`NODE_RADIUS` = 7, padding 8): a 30 CSS pixel target, 14 short on
    /// both axes. `audit` is what ties those two inputs to their real definitions.
    #[test]
    fn commit_dot_hit_circle_is_thirty_pixels_at_default_zoom() {
        let side = node_hit_extent_px(7.0, 8.0, 1.0);
        assert_eq!(side, 30.0);
        assert_eq!(
            TapTarget::square(side).verdict(),
            TargetVerdict::Undersized {
                short_by_x_px: 14.0,
                short_by_y_px: 14.0,
            }
        );
    }

    #[test]
    fn node_hit_extent_scales_linearly_with_the_camera() {
        assert_eq!(node_hit_extent_px(7.0, 8.0, 0.2), 6.0);
        assert_eq!(node_hit_extent_px(7.0, 8.0, 2.0), 60.0);
        assert_eq!(node_hit_extent_px(7.0, 8.0, 5.0), 150.0);
    }

    /// 44 / 30 — the zoom the user has to reach before a commit dot is a compliant
    /// target. Stated as the literal ratio so the number cannot drift silently.
    #[test]
    fn commit_dot_needs_about_one_point_five_zoom_to_meet_guidance() {
        let s = min_camera_scale_for_guidance(7.0, 8.0);
        assert_eq!(s, 44.0 / 30.0);
        assert!((s - 1.466_666_6).abs() < 1e-6, "unexpected scale {s}");

        // Just under it fails, at it passes: the two sides of the boundary.
        assert!(matches!(
            TapTarget::square(node_hit_extent_px(7.0, 8.0, s - 0.01)).verdict(),
            TargetVerdict::Undersized { .. }
        ));
        assert_eq!(
            TapTarget::square(node_hit_extent_px(7.0, 8.0, s)).verdict(),
            TargetVerdict::Meets
        );
    }

    /// The graph's minimum zoom (`camera::MIN_ZOOM` = 0.2) is nowhere near guidance —
    /// worth pinning, because "zoom in and it's fine" is only an answer if the user is
    /// not zoomed out reading the whole history.
    #[test]
    fn commit_dot_is_six_pixels_at_minimum_zoom() {
        assert_eq!(node_hit_extent_px(7.0, 8.0, crate::camera::MIN_ZOOM), 6.0);
    }

    #[test]
    fn degenerate_hit_radius_is_unreachable_at_any_zoom() {
        assert_eq!(min_camera_scale_for_guidance(0.0, 0.0), f64::INFINITY);
        assert_eq!(min_camera_scale_for_guidance(-5.0, 2.0), f64::INFINITY);
    }
}
