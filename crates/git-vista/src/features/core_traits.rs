//! The vocabulary every feature core shares.
//!
//! Deliberately framework-free: this module must never import `leptos`, `web_sys`,
//! `js_sys` or `wasm_bindgen`, so it compiles and is unit-tested on the host target by
//! the ordinary `cargo test --workspace` the gate already runs (M1.11, #64, decision D1).

use git_vista_protocol::plan::GenerationToken;

/// The outcome of a transition that was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// State changed.
    Committed,
    /// The event was valid but a no-op — e.g. re-applying an already-recorded terminal.
    NoChange,
}

/// A feature's state core. Implementors must leave `self` untouched when `apply` errors.
pub trait FeatureCore {
    type Event;
    type Rejection;
    fn apply(&mut self, ev: Self::Event) -> Result<Applied, Self::Rejection>;
}

/// What a request was about, so an out-of-order response can be recognised.
///
/// #783: this used to also carry `Page(u64)` and `Operation(String)`, from the
/// original M1.11 design doc's plan to generalise `PageRequestKey`'s own
/// fencing (`features/graph/core.rs`) and per-operation fencing into this one
/// type. Neither generalisation ever happened — `PageRequestKey` remains its
/// own separate, still-used mechanism, and every real operation fence
/// (`menu/{branch,tag,commit,remote}_items.rs`) already uses `Branch`/`Tag`/
/// `Commit`/`Repository` directly, which is what an operation's target
/// actually is. Both variants had zero constructors anywhere, in any build
/// (host test or wasm), and no match arm depended on them. No spec or open
/// issue cites either as needed groundwork — deleted rather than kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestTarget {
    Repository,
    Branch(String),
    Tag(String),
    Commit(String),
}

/// Identity carried by every async continuation that writes shared state.
///
/// Generalises M1.10's `PageRequestKey` (`crate::features::graph::core`) so the same fencing protects the
/// bare `spawn_local` sites in `menu.rs` and `picker.rs`, which today write unconditionally
/// and can race (design spec §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestKey {
    pub epoch: u64,
    pub generation: Option<GenerationToken>,
    pub target: RequestTarget,
}

impl RequestKey {
    /// True when this request may still commit its result.
    ///
    /// A request that carried no generation is fenced by epoch alone — that is the correct
    /// reading for endpoints that predate M1.10. A request that *did* carry one is stale
    /// the moment the live generation differs, including when the live side has none.
    pub fn is_current(&self, live_epoch: u64, live_generation: Option<&GenerationToken>) -> bool {
        if self.epoch != live_epoch {
            return false;
        }
        match (&self.generation, live_generation) {
            (None, _) => true,
            (Some(mine), Some(live)) => mine == live,
            (Some(_), None) => false,
        }
    }
}

/// Published by `operations` when a write settles; consumed by features holding server state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidate {
    pub generation: Option<GenerationToken>,
    pub scope: InvalidateScope,
}

/// #783: this enum used to also carry `Status` and `Activity`. Both were
/// deleted, and the deletion needed a second pass to get right — the first
/// draft of this comment argued for keeping them by citing #551 (M12) and #68
/// (M2.15) as open work that would consume them. **Checked again: both are
/// CLOSED** (#68 since 2026-08-07; #551 and its children #552-#556 since
/// 2026-09-06), and neither shipped anything that constructs or matches
/// either variant. M12 built an entirely separate mechanism for external
/// changes instead — `git_vista_protocol::change_feed` /
/// `features/freshness/core.rs` — with zero references to `InvalidateScope`.
/// #68 wired the status feature through `status::signals::create`/
/// `fetch_status()` directly, never through this enum. Grepping the current
/// tree confirms neither variant had a matcher anywhere, at any point in this
/// investigation, and no other open issue names a replacement plan. The
/// reason to keep them was borrowed from milestones whose own completed work
/// didn't need them; once that borrowed reason is subtracted, nothing argues
/// for keeping speculative vocabulary no one is building toward. Deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidateScope {
    Everything,
    /// Survives for a code-structural reason, independent of any issue
    /// tracker state: `GraphCore::on_invalidate` (`features/graph/core.rs`)
    /// matches `Graph` unconditionally as a case distinct from `Everything`,
    /// in code that ships to every build. Deleting the variant would force
    /// editing that live, always-compiled match arm to compensate — a change
    /// to invalidation behaviour dressed as dead-code cleanup, not a cleanup
    /// itself. No current publisher constructs it in production
    /// (`OperationsCore::settle`, `features/operations/core.rs`, always
    /// publishes `Everything`), so it is `#[allow(dead_code)]`; it is
    /// exercised only by `graph/core/graph_core_suite.rs`'s tests of that
    /// matcher.
    #[allow(dead_code)]
    Graph,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(s: &str) -> GenerationToken {
        GenerationToken::new(s).expect("valid generation token")
    }

    #[test]
    fn request_key_is_current_only_when_epoch_and_generation_both_match() {
        let key = RequestKey {
            epoch: 7,
            generation: Some(gen("42")),
            target: RequestTarget::Repository,
        };
        assert!(key.is_current(7, Some(&gen("42"))));
        assert!(!key.is_current(8, Some(&gen("42"))), "epoch moved");
        assert!(!key.is_current(7, Some(&gen("43"))), "generation moved");
        assert!(!key.is_current(8, Some(&gen("43"))), "both moved");
    }

    #[test]
    fn request_key_without_generation_is_fenced_by_epoch_alone() {
        // Pre-generation endpoints (sign-in, catalog) still need epoch fencing.
        let key = RequestKey {
            epoch: 3,
            generation: None,
            target: RequestTarget::Repository,
        };
        assert!(key.is_current(3, None));
        assert!(
            key.is_current(3, Some(&gen("99"))),
            "a live generation cannot stale a keyless request"
        );
        assert!(!key.is_current(4, None), "epoch still fences");
    }

    #[test]
    fn request_key_with_generation_is_stale_against_a_server_that_has_none() {
        // Defensive: if the server stops reporting a generation, a keyed request must not
        // silently be treated as current.
        let key = RequestKey {
            epoch: 1,
            generation: Some(gen("5")),
            target: RequestTarget::Repository,
        };
        assert!(!key.is_current(1, None));
    }
}
