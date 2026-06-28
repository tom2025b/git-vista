//! Pure camera math for panning and zooming the commit graph.
//!
//! The camera is a screen-space affine transform — a translation plus a uniform
//! scale — that is applied to the graph as a single SVG `<g transform>`. Keeping
//! it here, free of any Leptos/DOM dependency, means the (slightly fiddly)
//! anchored-zoom arithmetic can be reasoned about and unit-tested on the host,
//! exactly like [`crate::geometry`] and [`crate::color`].
//!
//! Coordinate model: the SVG has no `viewBox`, so one user unit equals one CSS
//! pixel and pointer coordinates (`offset_x`/`movement_x`) map straight onto the
//! same space the camera works in. A content point `c` lands on screen at
//! `tx + scale * c`.

/// Smallest and largest allowed zoom, and the multiplicative step per wheel
/// notch. Clamping keeps the graph from vanishing to a dot or exploding past
/// anything useful.
pub const MIN_ZOOM: f64 = 0.2;
pub const MAX_ZOOM: f64 = 5.0;
// Consumed only by the wasm-only `app` view, so it reads as dead on host/test
// builds (where this module's blanket allow is absent).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const ZOOM_STEP: f64 = 1.1;

/// A pan/zoom viewport over the graph: translate by `(tx, ty)` then scale by
/// `scale`. Both are in CSS pixels / user units (the two coincide here).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub tx: f64,
    pub ty: f64,
    pub scale: f64,
}

impl Default for Camera {
    /// Identity: no pan, no zoom — the graph sits at its natural position.
    fn default() -> Self {
        Self { tx: 0.0, ty: 0.0, scale: 1.0 }
    }
}

impl Camera {
    /// The SVG group transform that realises this camera. Applied as
    /// `translate(...) scale(...)`, i.e. content is scaled first, then shifted.
    pub fn transform(&self) -> String {
        format!("translate({} {}) scale({})", self.tx, self.ty, self.scale)
    }

    /// Translate the view by a screen-space delta — e.g. the per-event movement
    /// of a pointer drag. Independent of zoom, so a drag tracks the cursor 1:1.
    pub fn panned(self, dx: f64, dy: f64) -> Self {
        Self { tx: self.tx + dx, ty: self.ty + dy, ..self }
    }

    /// Zoom by `factor` while pinning the content point currently under the
    /// screen position `(sx, sy)` so it stays put (focal-point zoom). The new
    /// scale is clamped to `[MIN_ZOOM, MAX_ZOOM]`, and the translation is solved
    /// from the *effective* (post-clamp) factor so the anchor holds even at the
    /// limits.
    pub fn zoomed_at(self, factor: f64, sx: f64, sy: f64) -> Self {
        let scale = (self.scale * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let f = scale / self.scale;
        // Keep `c = (s - t)/scale` fixed across the change:
        //   s = t' + scale' * c  =>  t' = s - (s - t) * f
        let tx = sx - (sx - self.tx) * f;
        let ty = sy - (sy - self.ty) * f;
        Self { tx, ty, scale }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_string_is_svg_ready() {
        let c = Camera { tx: 2.0, ty: 3.0, scale: 2.0 };
        assert_eq!(c.transform(), "translate(2 3) scale(2)");
    }

    #[test]
    fn pan_accumulates_and_leaves_scale_alone() {
        let c = Camera::default().panned(10.0, -5.0).panned(2.0, 1.0);
        assert_eq!((c.tx, c.ty, c.scale), (12.0, -4.0, 1.0));
    }

    #[test]
    fn zoom_keeps_the_cursor_point_anchored() {
        let c = Camera { tx: 3.0, ty: 7.0, scale: 1.5 };
        let (sx, sy) = (40.0, 25.0);
        let cx = (sx - c.tx) / c.scale; // content point under the cursor
        let cy = (sy - c.ty) / c.scale;

        let z = c.zoomed_at(1.2, sx, sy);
        // That same content point still maps to the cursor position.
        assert!((z.tx + z.scale * cx - sx).abs() < 1e-9);
        assert!((z.ty + z.scale * cy - sy).abs() < 1e-9);
    }

    #[test]
    fn zoom_clamps_to_the_limits() {
        assert_eq!(Camera::default().zoomed_at(1000.0, 0.0, 0.0).scale, MAX_ZOOM);
        assert_eq!(Camera::default().zoomed_at(0.0001, 0.0, 0.0).scale, MIN_ZOOM);
    }
}
