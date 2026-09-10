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
/// issue cites either as needed groundwork, unlike the M12 external-changes
/// spec's citation of `InvalidateScope` below — deleted rather than kept.
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

/// #783: none of `Graph`/`Status`/`Activity` below has a production publisher
/// today — `OperationsCore::settle` (`features/operations/core.rs`) always
/// publishes `Everything` (a write can move refs, the tree and the journal at
/// once, so a self-write has no reason to narrow the scope). All three read
/// as "never constructed" on a wasm production build. They are kept anyway,
/// not deleted, because the M12 external-changes decision spec
/// (`docs/superpowers/specs/m3.26-external-changes.md`'s own "checked rather
/// than assumed" table) already cites this exact vocabulary —
/// `InvalidateScope::{Everything, Graph, Status, Activity}` — as **DONE**
/// groundwork that #551 (open, blocks #552-#556) builds its file-watch/sweep
/// producer on top of. Deleting any of the three now would mean that
/// currently-open work has to re-add it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidateScope {
    Everything,
    /// The one sibling with a real consumer already built: `GraphCore::
    /// on_invalidate` (`features/graph/core.rs`) matches `Graph` unconditionally
    /// as a case distinct from `Everything`, so this can never be `#[cfg(test)]`-
    /// gated without editing that live, always-compiled match arm — a change to
    /// invalidation behaviour, not a dead-code cleanup. Constructed today only
    /// by `graph/core/graph_core_suite.rs`'s tests of that matcher.
    #[allow(dead_code)]
    Graph,
    /// #68 (M2.15): `features/status/mod.rs`'s own module doc already documents
    /// this as the "documented destination" for the working-tree-status state
    /// machine M1.11 deliberately left unbuilt ("writing speculative state here
    /// would be worse than leaving a shaped hole"). No constructor and no
    /// matcher exist yet; both are #68's to add.
    #[allow(dead_code)]
    Status,
    /// No matcher exists anywhere (unlike `Graph`) — the Activity panel's own
    /// data currently rides the blanket `Everything` invalidation like
    /// everything else. Named explicitly in the M12 spec table above rather
    /// than merely plausible; #551 owns wiring a real producer/consumer pair
    /// for it, or removing it if M12 lands without needing scope precision
    /// this fine. Constructed today only by `graph_core_suite.rs`'s negative
    /// test (an invalidation scoped somewhere GraphCore doesn't care about).
    #[allow(dead_code)]
    Activity,
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
