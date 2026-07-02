//! Level of detail: how much per-commit text to draw at the current zoom.
//!
//! Every label — the ref badges, the commit message, and the dimmed
//! `hash · author · date` meta line — lives inside the pan/zoom `<g>`, so it
//! shrinks with the graph. Zoomed far out those labels become an unreadable
//! smear that only clutters the structure, so this maps the camera's `scale` to
//! a [`Detail`] level and the view drops the least-important text first. Pure
//! and host-testable, like [`crate::camera`] and [`crate::geometry`]; the view
//! layer just reads the level reactively off the camera signal.
//!
//! (The per-node icons beside the dots are *not* LOD-driven: they're always on,
//! behind their own user toggle — see `show_node_icons` in `app.rs`.)

/// How much label text to render at the current zoom. Ordered coarse → fine:
/// each finer level is a superset of the text the coarser one shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Detail {
    /// Zoomed far out: graph structure only (dots, edges, stubs) — no text. At
    /// this size labels are an unreadable smear, so they're dropped entirely.
    GraphOnly,
    /// Mid zoom: ref badges and the commit message, but not the smaller,
    /// secondary meta line.
    Message,
    /// Zoomed in: everything, including the `hash · author · date` meta line.
    Full,
}

impl Detail {
    /// Whether the ref badges and the commit message are drawn — everything but
    /// the fully zoomed-out structure view.
    pub fn shows_message(self) -> bool {
        !matches!(self, Detail::GraphOnly)
    }

    /// Whether the dimmed meta line (`hash · author · date`) is drawn — only at
    /// the closest level, where its 11px text is comfortably readable.
    pub fn shows_meta(self) -> bool {
        matches!(self, Detail::Full)
    }
}

/// Below this zoom the 13px message text renders under ~7px — too small to
/// read — so we drop to structure only.
pub const MESSAGE_SCALE: f64 = 0.5;

/// At or above this zoom everything shows; between it and [`MESSAGE_SCALE`] the
/// message stays but the smaller meta line is dropped.
pub const FULL_SCALE: f64 = 0.8;

/// The [`Detail`] level to render at camera zoom `scale`. Thresholds are chosen
/// against the camera's `[MIN_ZOOM, MAX_ZOOM]` = `[0.2, 5.0]` range so the
/// default (unzoomed, `scale = 1.0`) view is [`Detail::Full`] and zooming out
/// peels detail away in two steps.
pub fn detail_for(scale: f64) -> Detail {
    if scale < MESSAGE_SCALE {
        Detail::GraphOnly
    } else if scale < FULL_SCALE {
        Detail::Message
    } else {
        Detail::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoomed_out_shows_structure_only() {
        // Well below the message threshold, and right at the camera's min zoom.
        let d = detail_for(0.2);
        assert_eq!(d, Detail::GraphOnly);
        assert!(!d.shows_message());
        assert!(!d.shows_meta());
    }

    #[test]
    fn mid_zoom_shows_the_message_but_not_the_meta_line() {
        let d = detail_for((MESSAGE_SCALE + FULL_SCALE) / 2.0);
        assert_eq!(d, Detail::Message);
        assert!(d.shows_message());
        assert!(!d.shows_meta());
    }

    #[test]
    fn the_default_unzoomed_view_shows_everything() {
        // Camera::default() has scale 1.0 — the view you first see must be full.
        let d = detail_for(1.0);
        assert_eq!(d, Detail::Full);
        assert!(d.shows_message());
        assert!(d.shows_meta());
    }

    #[test]
    fn thresholds_are_inclusive_from_below() {
        // Exactly at a threshold takes the higher (finer) level: the boundary
        // belongs to the more-detailed side, so text appears as soon as you
        // reach it rather than one notch late.
        assert_eq!(detail_for(MESSAGE_SCALE), Detail::Message);
        assert_eq!(detail_for(FULL_SCALE), Detail::Full);
        // Just under each boundary is still the coarser level.
        assert_eq!(detail_for(MESSAGE_SCALE - 1e-9), Detail::GraphOnly);
        assert_eq!(detail_for(FULL_SCALE - 1e-9), Detail::Message);
    }

    #[test]
    fn detail_is_monotonic_in_zoom() {
        // Zooming in never removes text: shows_message / shows_meta are
        // non-decreasing as scale grows across the whole camera range.
        let mut prev_msg = false;
        let mut prev_meta = false;
        let mut s = 0.2;
        while s <= 5.0 {
            let d = detail_for(s);
            assert!(d.shows_message() >= prev_msg, "message flag regressed at {s}");
            assert!(d.shows_meta() >= prev_meta, "meta flag regressed at {s}");
            prev_msg = d.shows_message();
            prev_meta = d.shows_meta();
            s += 0.05;
        }
    }
}
