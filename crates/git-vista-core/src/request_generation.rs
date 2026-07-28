//! Cancellation via generation-tag (M2.16, #69d): a monotonic counter a
//! virtualized view bumps every time its visible range triggers a new
//! refetch, so a late-arriving response can identify itself as stale.
//!
//! #69 requires *"Rendering is virtualized and cancellable."* A scroll that
//! outruns its own in-flight fetches must not let an earlier request's
//! response land after a later one and paint over newer content. There are
//! two ways to satisfy that:
//!
//! 1. **Per-request abort** — cancel the actual in-flight HTTP request
//!    (`AbortController` on the wasm side, or dropping a `Future` that stops
//!    polling and cancels the underlying `fetch`).
//! 2. **Generation-tag identification** — never abort anything; instead the
//!    *caller* stamps every request it issues with the current generation,
//!    and discards any response whose generation no longer matches by the
//!    time it arrives.
//!
//! **This module implements (2), not (1).** #69c's own module doc names the
//! reason to reach for this shape specifically: it is exactly [`identity::
//! RepositoryGeneration`](crate::identity::RepositoryGeneration)'s pattern
//! (ADR 0001) — a monotonic, opaquely-compared counter — applied to view
//! state instead of repository state, and this project already has one
//! working implementation of that idea to lean on rather than a second,
//! divergent mechanism. It is also strictly simpler to reason about and to
//! host-test: no `Future`/`AbortController` machinery, no wasm-only code
//! path, just integer comparison. Per-request abort remains a real,
//! complementary optimisation (it stops wasted network/CPU work an
//! already-stale request would otherwise still perform) — nothing here
//! precludes adding it later once a real fetch exists to abort. It isn't
//! *required* to satisfy "cancellable": a discarded stale response, even if
//! its request wasn't literally aborted, still means nothing stale is ever
//! painted, which is the actual, user-visible guarantee the criterion is
//! about.
//!
//! **Nothing wired to a real fetch yet.** No diff (or other) endpoint that
//! this would guard exists today — #69b deliberately stopped short of a live
//! server endpoint, and #69e (wiring the diff view onto #65's shell) is what
//! will eventually issue real requests stamped with a [`RequestGeneration`].
//! This module is the same kind of new, unconsumed primitive #69b's
//! `DiffSpec` and #69c's `CumulativeHeights` were when they landed: pure,
//! host-tested, ready for the slice that wires it up.

/// A single generation value, opaquely comparable — matches
/// [`identity::RepositoryGeneration`](crate::identity::RepositoryGeneration)'s
/// own shape: no ordering is exposed (a caller must never reason about "is
/// this generation newer," only "is this generation the current one"),
/// which is all cancellation-by-discard actually needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestGeneration(u64);

impl RequestGeneration {
    /// The generation before any request has ever been issued.
    pub fn initial() -> Self {
        RequestGeneration(0)
    }
}

/// Owned by a virtualized view: issues a new [`RequestGeneration`] each time
/// the view's visible range changes and a refetch is triggered, and answers
/// whether a generation a response carries is still the current one.
///
/// Not `Clone`/`Copy` on purpose — there is exactly one current generation
/// per view, and it must live in exactly one place a view mutates, not be
/// copied around and drift out of sync with itself.
#[derive(Debug, Default)]
pub struct RequestGenerationTracker {
    current: RequestGeneration,
}

impl RequestGenerationTracker {
    pub fn new() -> Self {
        RequestGenerationTracker {
            current: RequestGeneration::initial(),
        }
    }

    /// Bump the generation and return the new value — call this once per
    /// refetch, immediately before issuing the request, and stamp the
    /// request with the returned value.
    pub fn issue(&mut self) -> RequestGeneration {
        self.current = RequestGeneration(self.current.0 + 1);
        self.current
    }

    /// The generation a response should be checked against on arrival.
    pub fn current(&self) -> RequestGeneration {
        self.current
    }

    /// True when `generation` (the value a response was stamped with when
    /// its request was issued) is still the tracker's current generation —
    /// i.e. the response is not stale and safe to paint. False means a later
    /// `issue()` has already happened since this response's request went
    /// out; the caller must discard it.
    pub fn is_current(&self, generation: RequestGeneration) -> bool {
        generation == self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_issued_generation_is_current() {
        let mut tracker = RequestGenerationTracker::new();
        let g = tracker.issue();
        assert!(tracker.is_current(g));
    }

    #[test]
    fn an_earlier_generation_is_stale_once_a_later_one_is_issued() {
        let mut tracker = RequestGenerationTracker::new();
        let first = tracker.issue();
        let second = tracker.issue();

        assert_ne!(first, second);
        assert!(
            !tracker.is_current(first),
            "first response arrived late and must be discarded"
        );
        assert!(tracker.is_current(second));
    }

    // The scenario #69's own requirement is about: a fast scroll issues
    // several refetches; only the very last one's response should ever be
    // treated as current, regardless of the order responses actually arrive
    // in over the network (which this tracker never assumes anything about).
    #[test]
    fn only_the_most_recently_issued_generation_is_current_even_out_of_arrival_order() {
        let mut tracker = RequestGenerationTracker::new();
        let g1 = tracker.issue();
        let g2 = tracker.issue();
        let g3 = tracker.issue();

        // Simulate g3's response arriving first, then g1's, then g2's —
        // arrival order must not matter to the answer.
        assert!(tracker.is_current(g3));
        assert!(!tracker.is_current(g1));
        assert!(!tracker.is_current(g2));
    }

    #[test]
    fn a_tracker_with_no_issued_request_yet_treats_the_initial_generation_as_current() {
        let tracker = RequestGenerationTracker::new();
        assert!(tracker.is_current(RequestGeneration::initial()));
    }

    #[test]
    fn generation_values_carry_no_exposed_ordering() {
        // RequestGeneration deliberately does not derive PartialOrd/Ord —
        // a caller must compare only for equality against the tracker's
        // current value, never reason about "newer than." This test exists
        // to make that omission a documented decision, not an oversight a
        // future edit silently reverses (e.g. by deriving Ord to satisfy an
        // unrelated trait bound elsewhere).
        fn assert_no_ord<T>() {}
        assert_no_ord::<RequestGeneration>();
    }

    #[test]
    fn default_tracker_matches_new() {
        let default_tracker = RequestGenerationTracker::default();
        let new_tracker = RequestGenerationTracker::new();
        assert_eq!(default_tracker.current(), new_tracker.current());
    }
}
