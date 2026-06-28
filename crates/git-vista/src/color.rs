//! Lane colouring for the commit graph.
//!
//! A small, stable palette indexed by lane, kept separate from the spatial
//! [`crate::geometry`] so colour decisions can evolve on their own. Phase 7
//! replaces lane-indexed colouring with per-branch colours; for now the lane
//! index is a fine proxy.

const LANE_COLORS: [&str; 6] = [
    "#2f81f7", // blue   (main)
    "#3fb950", // green
    "#d29922", // amber
    "#db61a2", // pink
    "#a371f7", // purple
    "#f78166", // coral
];

// Used only by the wasm-only `app` view, so it reads as dead on host/test builds.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
/// Background fill for hollow merge nodes — the canvas colour, so a merge reads
/// as a ring rather than a filled dot.
pub const MERGE_FILL: &str = "#0d1117";

/// Colour for the given lane, wrapping around the palette.
pub fn lane_color(lane: usize) -> &'static str {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_color_wraps_around_the_palette() {
        assert_eq!(lane_color(0), lane_color(LANE_COLORS.len()));
        assert_eq!(lane_color(1), lane_color(LANE_COLORS.len() + 1));
    }
}
