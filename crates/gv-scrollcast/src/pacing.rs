//! The scroll timeline: how fast the camera moves down the tall print image,
//! and where it stops to show a pivot callout. Framework-free by construction
//! (#325) — this crate's whole safety story rests on the same rule the app
//! does: the part that decides is pure and host-tested, and the part that
//! touches Chromium/ffmpeg (`capture.rs`/`encode.rs`) is only the plumbing
//! that carries the decision out.
//!
//! Three inputs, in order:
//!
//! 1. **Density** — how many commit rows occupy each vertical pixel band of
//!    the rendered image. A dense band (a busy week) should move slower than
//!    a sparse one (a quiet stretch), or the video reads at one flat speed
//!    that means nothing.
//! 2. **Pivots** — merges and month boundaries, read straight off
//!    [`git_vista_core::model::GraphRow`]. Each pivot gets a **dwell**: the
//!    scroll stops, a callout card holds on screen, then it resumes. This is
//!    the same timeline the density curve drives — a dwell is not a separate
//!    mechanism, it's a zero-velocity segment.
//! 3. **Duration** — the target video length; everything above is normalised
//!    to fit it, with a floor and ceiling on speed so pacing never reads as
//!    jerky.

use std::ops::RangeInclusive;

/// One commit's vertical position in the rendered image, for density
/// estimation. Not `GraphRow` itself — this module never touches lane,
/// colour or refs, only the one field density needs — so it stays testable
/// with plain integers and never needs a fixture graph to exercise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommitY {
    /// Pixel y-position in the full-height rendered image (top = 0).
    pub y: f64,
}

/// A merge or month boundary worth calling out, with the text to show while
/// the scroll is paused there.
#[derive(Debug, Clone, PartialEq)]
pub struct Pivot {
    pub y: f64,
    pub label: String,
    pub detail: String,
}

/// One segment of the finished timeline: how long the camera spends moving
/// from `y_start` to `y_end` (a scroll segment, `y_start == y_end` never
/// happens for these), or holding at one `y` while a callout shows (a dwell
/// segment, `y_start == y_end`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub y_start: f64,
    pub y_end: f64,
    pub duration_secs: f64,
}

impl Segment {
    pub fn is_dwell(&self) -> bool {
        self.y_start == self.y_end
    }
}

/// How long a pivot callout holds the scroll before resuming.
pub const DEFAULT_DWELL_SECS: f64 = 3.0;

/// The floor and ceiling on scroll speed (image pixels per second), relative
/// to the *even* speed a purely linear scroll would use. Below the floor a
/// dense region would crawl to a stop; above the ceiling a sparse region
/// would blur past too fast to read a single commit. Both are multipliers on
/// the mean speed, not absolute pixel rates, so they scale with whatever
/// `--duration` and image height the caller picked.
pub const MIN_SPEED_MULTIPLIER: f64 = 0.35;
pub const MAX_SPEED_MULTIPLIER: f64 = 3.0;

/// Commit density per vertical band, as a plain histogram: `bands[i]` is the
/// commit count in `[i * band_height, (i + 1) * band_height)`.
///
/// A histogram rather than a continuous density estimate on purpose — it is
/// the smallest thing that lets slow-vs-fast be computed by simple division,
/// it is trivially host-tested (one commit in, one bucket incremented, done),
/// and the print view's own layout is already row-quantised, so there is no
/// finer-grained truth to approximate.
pub fn commit_density(commits: &[CommitY], image_height: f64, band_height: f64) -> Vec<f64> {
    assert!(band_height > 0.0, "band_height must be positive");
    let band_count = (image_height / band_height).ceil().max(1.0) as usize;
    let mut bands = vec![0.0; band_count];
    for c in commits {
        let idx = ((c.y / band_height).floor().max(0.0)) as usize;
        if let Some(slot) = bands.get_mut(idx.min(band_count - 1)) {
            *slot += 1.0;
        }
    }
    bands
}

/// Turn a density histogram into a speed multiplier per band: `multiplier[i]`
/// is how fast the camera should move through that band relative to the mean
/// pace, dense bands slower (multiplier < 1) and sparse bands faster
/// (multiplier > 1), clamped to `[MIN_SPEED_MULTIPLIER, MAX_SPEED_MULTIPLIER]`.
///
/// Inverted density, not density itself: a band with zero commits (a long gap
/// with nothing happening) should scroll fastest, and a band packed with
/// commits should scroll slowest — the multiplier is inversely proportional
/// to `1 + density`, so a zero-commit band still gets a finite (capped) speed
/// rather than a division by zero.
pub fn speed_multipliers(density: &[f64]) -> Vec<f64> {
    if density.is_empty() {
        return Vec::new();
    }
    let raw: Vec<f64> = density.iter().map(|d| 1.0 / (1.0 + d)).collect();
    // Normalise so the mean multiplier is 1.0 — "relative to even pacing" is
    // the contract callers rely on (see this fn's doc comment), and the mean
    // is what makes total distance-over-time invariant regardless of how
    // lumpy the commit history is.
    let mean = raw.iter().sum::<f64>() / raw.len() as f64;
    raw.iter()
        .map(|r| (r / mean).clamp(MIN_SPEED_MULTIPLIER, MAX_SPEED_MULTIPLIER))
        .collect()
}

/// Build the full timeline: scroll segments paced by `multipliers`, with a
/// dwell segment spliced in at each pivot's `y`, all normalised so the whole
/// thing sums to `target_duration_secs`.
///
/// Dwell time is carved out of the total budget *before* the scroll segments
/// are timed, not added on top — a `--duration 240` request means the whole
/// video is 240 seconds, pivots and all, not 240 seconds of scrolling plus
/// however many dwells happen to land.
pub fn build_timeline(
    image_height: f64,
    band_height: f64,
    multipliers: &[f64],
    pivots: &[Pivot],
    target_duration_secs: f64,
    dwell_secs: f64,
) -> Vec<Segment> {
    if multipliers.is_empty() || image_height <= 0.0 {
        return Vec::new();
    }
    let total_dwell = dwell_secs * pivots.len() as f64;
    let scroll_budget = (target_duration_secs - total_dwell).max(0.0);

    // Weight of one band = its multiplier — a slow (low-multiplier) band
    // takes proportionally *more* of the scroll budget's time for the same
    // pixel distance, which is what "slower through dense regions" means in
    // terms of a time budget rather than a speed.
    let weight_sum: f64 = multipliers.iter().map(|m| 1.0 / m).sum();

    let mut pivot_ys: Vec<f64> = pivots.iter().map(|p| p.y).collect();
    pivot_ys.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let mut segments = Vec::new();
    let mut y = 0.0_f64;
    let mut pivot_idx = 0;

    for (band_idx, &mult) in multipliers.iter().enumerate() {
        let band_end = ((band_idx + 1) as f64 * band_height).min(image_height);
        let band_weight = 1.0 / mult;
        let band_secs = if weight_sum > 0.0 {
            scroll_budget * band_weight / weight_sum
        } else {
            0.0
        };
        let band_span = band_end - y;

        // A pivot inside this band SPLITS it, rather than being appended once
        // the band has already finished scrolling. Appending was the original
        // bug and the monotonicity property test caught it: a pivot at y=250
        // inside a 200..300 band produced [scroll 200->300, dwell@250], so the
        // camera jumped *backward* 50px to hold the callout. The camera in a
        // scroll-down video may never move backward — that is the one
        // invariant this whole module owes its caller.
        //
        // Each sub-span gets the share of the band's time its pixels deserve,
        // so splitting a band changes where the dwell lands, never how long
        // the band takes overall.
        while pivot_idx < pivot_ys.len() && pivot_ys[pivot_idx] <= band_end {
            let pivot_y = pivot_ys[pivot_idx].max(y);
            let sub_span = pivot_y - y;
            if sub_span > 0.0 && band_span > 0.0 && band_secs > 0.0 {
                segments.push(Segment {
                    y_start: y,
                    y_end: pivot_y,
                    duration_secs: band_secs * (sub_span / band_span),
                });
            }
            segments.push(Segment {
                y_start: pivot_y,
                y_end: pivot_y,
                duration_secs: dwell_secs,
            });
            y = pivot_y;
            pivot_idx += 1;
        }

        // Whatever is left of the band after the last pivot inside it.
        if band_end > y && band_span > 0.0 && band_secs > 0.0 {
            segments.push(Segment {
                y_start: y,
                y_end: band_end,
                duration_secs: band_secs * ((band_end - y) / band_span),
            });
        }
        y = band_end;
    }

    segments
}

/// The total duration of a built timeline, for a caller that wants to assert
/// it matches what it asked for before spending an ffmpeg pass on it.
pub fn total_duration(segments: &[Segment]) -> f64 {
    segments.iter().map(|s| s.duration_secs).sum()
}

/// Where the camera's top edge sits at `t` seconds into a built timeline.
/// Linear within each segment (a dwell's `y_start == y_end`, so the
/// interpolation is a no-op there, which is exactly a held frame). Clamps to
/// the last segment's `y_end` past the timeline's own duration rather than
/// extrapolating, so a caller asking for one frame past the end gets the
/// final frame, not garbage.
pub fn camera_y_at(segments: &[Segment], t: f64) -> f64 {
    let mut elapsed = 0.0_f64;
    for seg in segments {
        let seg_end = elapsed + seg.duration_secs;
        if t <= seg_end || seg.duration_secs == 0.0 {
            if seg.duration_secs <= 0.0 {
                return seg.y_start;
            }
            let local = ((t - elapsed) / seg.duration_secs).clamp(0.0, 1.0);
            return seg.y_start + (seg.y_end - seg.y_start) * local;
        }
        elapsed = seg_end;
    }
    segments.last().map(|s| s.y_end).unwrap_or(0.0)
}

/// Which vertical band `y` falls in, for `commit_density`/`speed_multipliers`
/// callers building `Pivot`s and needing to align them to the same bands —
/// exposed so `chapters.rs` never re-derives the bucketing rule by hand.
pub fn band_range(image_height: f64, band_height: f64) -> RangeInclusive<usize> {
    let last = ((image_height / band_height).ceil().max(1.0) as usize).saturating_sub(1);
    0..=last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commits(ys: &[f64]) -> Vec<CommitY> {
        ys.iter().map(|&y| CommitY { y }).collect()
    }

    #[test]
    fn a_single_commit_lands_in_its_own_band() {
        let density = commit_density(&commits(&[50.0]), 1000.0, 100.0);
        assert_eq!(density[0], 1.0);
        assert!(density[1..].iter().all(|&d| d == 0.0));
    }

    #[test]
    fn an_out_of_range_commit_is_clamped_to_the_last_band_not_dropped() {
        // A commit past the reported image height (rounding at the very
        // bottom of the sheet) must still be counted — losing it would
        // silently understate density exactly where the graph ends.
        let density = commit_density(&commits(&[999.0]), 1000.0, 200.0);
        assert_eq!(density.iter().sum::<f64>(), 1.0);
    }

    #[test]
    fn dense_bands_get_a_smaller_multiplier_than_sparse_bands() {
        // Mutation this catches: inverting the density->speed relationship
        // (dense=fast instead of dense=slow), which would make a busy week
        // fly past and a quiet stretch crawl — backwards from the whole
        // point of non-linear pacing.
        let mults = speed_multipliers(&[10.0, 0.0]);
        assert!(
            mults[0] < mults[1],
            "dense band {} must be slower than sparse band {}",
            mults[0],
            mults[1]
        );
    }

    #[test]
    fn uniform_density_produces_uniform_speed() {
        let mults = speed_multipliers(&[3.0, 3.0, 3.0]);
        assert!(mults.iter().all(|&m| (m - 1.0).abs() < 1e-9));
    }

    #[test]
    fn multipliers_never_exceed_the_configured_bounds() {
        // An extreme empty stretch next to an extremely busy one is exactly
        // the case the clamp exists for — without it this would blur past
        // unreadably fast or nearly freeze.
        let mults = speed_multipliers(&[0.0, 0.0, 0.0, 500.0]);
        for m in mults {
            assert!(m >= MIN_SPEED_MULTIPLIER - 1e-9);
            assert!(m <= MAX_SPEED_MULTIPLIER + 1e-9);
        }
    }

    #[test]
    fn empty_density_produces_empty_multipliers_not_a_panic() {
        assert_eq!(speed_multipliers(&[]), Vec::<f64>::new());
    }

    #[test]
    fn the_built_timeline_sums_to_the_requested_duration() {
        // Mutation this catches: dwell time added on top of the scroll
        // budget instead of carved out of it, which would silently produce
        // a video longer than --duration asked for.
        let mults = vec![1.0, 1.0, 1.0, 1.0];
        let pivots = vec![
            Pivot {
                y: 150.0,
                label: "a".into(),
                detail: "".into(),
            },
            Pivot {
                y: 350.0,
                label: "b".into(),
                detail: "".into(),
            },
        ];
        let segments = build_timeline(400.0, 100.0, &mults, &pivots, 240.0, 3.0);
        assert!((total_duration(&segments) - 240.0).abs() < 1e-6);
    }

    #[test]
    fn a_timeline_with_no_pivots_has_no_dwell_segments() {
        let mults = vec![1.0, 1.0];
        let segments = build_timeline(200.0, 100.0, &mults, &[], 60.0, 3.0);
        assert!(segments.iter().all(|s| !s.is_dwell()));
    }

    #[test]
    fn a_pivot_produces_exactly_one_dwell_segment_at_its_y() {
        let mults = vec![1.0, 1.0];
        let pivots = vec![Pivot {
            y: 120.0,
            label: "merge".into(),
            detail: "".into(),
        }];
        let segments = build_timeline(200.0, 100.0, &mults, &pivots, 60.0, 3.0);
        let dwells: Vec<_> = segments.iter().filter(|s| s.is_dwell()).collect();
        assert_eq!(dwells.len(), 1);
        assert_eq!(dwells[0].y_start, 120.0);
        assert_eq!(dwells[0].duration_secs, 3.0);
    }

    #[test]
    fn camera_position_is_monotonic_non_decreasing_across_the_whole_timeline() {
        // The one property a scroll-down video can never violate: the
        // camera must never move backward, dwell or no dwell. Sampled
        // densely enough (200 steps) that a monotonicity bug in either the
        // segment ordering or the interpolation would show up.
        let mults = vec![2.0, 0.5, 1.0, 3.0];
        let pivots = vec![Pivot {
            y: 250.0,
            label: "x".into(),
            detail: "".into(),
        }];
        let segments = build_timeline(400.0, 100.0, &mults, &pivots, 100.0, 4.0);
        let total = total_duration(&segments);
        let mut last_y = -1.0_f64;
        let steps = 200;
        for i in 0..=steps {
            let t = total * (i as f64) / (steps as f64);
            let y = camera_y_at(&segments, t);
            assert!(
                y + 1e-9 >= last_y,
                "camera moved backward: {y} < {last_y} at t={t}"
            );
            last_y = y;
        }
    }

    #[test]
    fn camera_holds_still_for_the_whole_dwell_duration() {
        let segments = vec![
            Segment {
                y_start: 0.0,
                y_end: 100.0,
                duration_secs: 10.0,
            },
            Segment {
                y_start: 100.0,
                y_end: 100.0,
                duration_secs: 3.0,
            },
            Segment {
                y_start: 100.0,
                y_end: 200.0,
                duration_secs: 10.0,
            },
        ];
        // Sampled across the whole 3-second dwell window: this must read
        // exactly 100.0 the entire time, not just at its start/end.
        for t in [10.0, 11.0, 11.5, 12.0, 12.99] {
            assert_eq!(camera_y_at(&segments, t), 100.0, "at t={t}");
        }
    }

    #[test]
    fn a_query_past_the_end_clamps_to_the_final_position() {
        let segments = vec![Segment {
            y_start: 0.0,
            y_end: 500.0,
            duration_secs: 10.0,
        }];
        assert_eq!(camera_y_at(&segments, 999.0), 500.0);
    }

    #[test]
    fn an_empty_timeline_reports_zero_position_rather_than_panicking() {
        assert_eq!(camera_y_at(&[], 5.0), 0.0);
    }

    proptest::proptest! {
        #[test]
        fn camera_never_exceeds_image_height(
            heights in proptest::collection::vec(1.0f64..2000.0, 1..8),
            duration in 10.0f64..600.0,
        ) {
            let image_height: f64 = heights.iter().sum::<f64>().max(100.0);
            let band_height = 50.0;
            let density = commit_density(&[], image_height, band_height);
            let mults = speed_multipliers(&density);
            let segments = build_timeline(image_height, band_height, &mults, &[], duration, 3.0);
            let total = total_duration(&segments);
            for i in 0..20 {
                let t = total * (i as f64) / 20.0;
                let y = camera_y_at(&segments, t);
                proptest::prop_assert!(y <= image_height + 1e-6);
                proptest::prop_assert!(y >= 0.0);
            }
        }
    }
}
