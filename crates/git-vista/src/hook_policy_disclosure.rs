//! Per-repository hook-policy disclosure text (INV-15, #66 M1.13b, #208).
//!
//! `RepositoryDescriptor::hook_policy` is computed server-side and shipped to
//! the client, and until this module existed **nothing rendered it**. A policy
//! that is computed, transmitted and then dropped on the floor is worse than no
//! policy at all: it manufactures the appearance of a safety property that no
//! user was ever told. INV-15 is a *disclosure* invariant, so the rendering is
//! the invariant, not a nicety on top of it.
//!
//! This module is the pure half — descriptor in, the words a user reads out —
//! so the mapping is host-testable (`cargo test -p git-vista`). The markup half
//! lives in [`crate::picker`], which is `wasm32`-gated because it imports
//! Leptos; that is the same core/view split the rest of this crate uses
//! (`features/*/core.rs` vs `features/*/signals.rs`).
//!
//! # Relationship to [`crate::hook_policy_banner`]
//!
//! That module is the **session**-scoped banner from ADR 0025: one bar across
//! the top of the app for `SessionInfo::hook_policy`. This one is
//! **repository**-scoped: the tier a local operation on one catalog entry would
//! actually run under, shown on that entry's row before the user commits to
//! opening it. They answer different questions and can legitimately disagree,
//! so neither replaces the other.
//!
//! # The one rule this module exists to hold
//!
//! The warn/quiet decision is taken **only** by
//! [`RepositoryDescriptor::hook_policy_requires_banner`]. It is never
//! re-derived here from the `Option`, because hand-rolling that per call site
//! is precisely how "not disclosed" gets quietly treated as "fine": `None` has
//! three causes (an older server, an ADR-0029 refusal, or no verdict yet) and
//! not one of them is a guarantee. The descriptor's own method already folds
//! all three to "fly the banner" at the type level, so the fail-safe direction
//! is inherited rather than re-argued.
//!
//! The *wording* still matches on the policy exhaustively — no `_` arm — so a
//! [`HookPolicy`] variant added later fails this crate's build until someone
//! writes honest text for it, rather than silently inheriting a neighbour's.

use git_vista_protocol::{HookPolicy, RepositoryDescriptor};

/// What a user is told about one repository's hook policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookPolicyDisclosure {
    /// True when INV-15 requires the elevated-risk styling — straight from
    /// [`RepositoryDescriptor::hook_policy_requires_banner`], never recomputed.
    pub warn: bool,
    /// The short badge, sized for a picker row.
    pub label: &'static str,
    /// One sentence, shown in full where the user commits to a repository.
    /// Never a tooltip: a disclosure nobody notices is the failure INV-15 names.
    pub detail: &'static str,
}

/// The disclosure for one catalog entry.
pub fn for_repository(descriptor: &RepositoryDescriptor) -> HookPolicyDisclosure {
    let (label, detail) = match descriptor.hook_policy {
        // Deliberately *not* phrased as reassurance. All three causes of `None`
        // land here, and the text has to be true for the worst of them.
        None => (
            "Hooks: not disclosed",
            "This server disclosed no hook policy for this repository. \
             Not disclosed is not a guarantee — treat this repository's hooks \
             as able to run with your permissions.",
        ),
        // The only variant that earns quiet styling. The claim is kept as
        // narrow as `HookPolicy::Strict`'s own docs make it: it is *not*
        // "confined to the repository".
        Some(HookPolicy::Strict) => (
            "Hooks: sandboxed (strict)",
            "Hooks run under the strict sandbox tier: no network, and no writes \
             outside the trees the server declared.",
        ),
        Some(HookPolicy::Network) => (
            "Hooks: sandboxed, network allowed",
            "Hooks run sandboxed, but with the network reachable — this \
             repository's hooks can talk to the outside world.",
        ),
        Some(HookPolicy::Unsandboxed) => (
            "Hooks: NOT sandboxed",
            "Hooks run with no sandbox at all. A malicious repository's hooks \
             execute with your permissions.",
        ),
        // "Your hooks silently did not run" is a surprise too, which is why
        // `requires_banner` warns on this one as well.
        Some(HookPolicy::Blocked) => (
            "Hooks: blocked",
            "Hooks do not run for this repository, so a repository that relies \
             on its hooks will behave differently here.",
        ),
    };
    HookPolicyDisclosure {
        warn: descriptor.hook_policy_requires_banner(),
        label,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::RepositoryKind;

    fn descriptor(hook_policy: Option<HookPolicy>) -> RepositoryDescriptor {
        RepositoryDescriptor {
            repository: "r".into(),
            worktree: "w".into(),
            name: "demo".into(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
            hook_policy,
        }
    }

    /// Every input this function can receive, with the warn flag written out
    /// literally rather than as `d.hook_policy_requires_banner()` — asserting
    /// the mapping against the implementation it is supposed to check would
    /// pass no matter which way the polarity ran.
    #[test]
    fn only_a_disclosed_strict_policy_is_quiet() {
        let cases = [
            (None, true),
            (Some(HookPolicy::Strict), false),
            (Some(HookPolicy::Network), true),
            (Some(HookPolicy::Unsandboxed), true),
            (Some(HookPolicy::Blocked), true),
        ];
        for (policy, expected_warn) in cases {
            assert_eq!(
                for_repository(&descriptor(policy)).warn,
                expected_warn,
                "wrong warn polarity for {policy:?}"
            );
        }
    }

    /// The failure mode this whole module exists for: an absent policy read as
    /// a green light. It must warn, and its words must not be the words that
    /// describe an actually-sandboxed repository.
    #[test]
    fn an_absent_policy_warns_and_never_borrows_strict_wording() {
        let absent = for_repository(&descriptor(None));
        let strict = for_repository(&descriptor(Some(HookPolicy::Strict)));

        assert!(absent.warn, "an undisclosed policy must not be styled quiet");
        assert_ne!(absent.label, strict.label);
        assert_ne!(absent.detail, strict.detail);
        assert!(
            !absent.label.contains("sandbox") && !absent.detail.contains("sandbox"),
            "the undisclosed text must not claim any sandbox: {absent:?}"
        );
    }

    /// Each tier gets its own words. A copy-pasted arm would let two genuinely
    /// different risk levels read identically on screen.
    #[test]
    fn every_policy_state_reads_differently() {
        let states = [
            None,
            Some(HookPolicy::Strict),
            Some(HookPolicy::Network),
            Some(HookPolicy::Unsandboxed),
            Some(HookPolicy::Blocked),
        ];
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for policy in states {
            let d = for_repository(&descriptor(policy));
            assert!(!d.label.is_empty() && !d.detail.is_empty());
            assert!(
                !seen.contains(&(d.label, d.detail)),
                "{policy:?} reuses another state's wording"
            );
            seen.push((d.label, d.detail));
        }
    }

    /// The unsandboxed tier is the one a user most needs to catch at a glance,
    /// so its badge must say so rather than hiding the fact in the detail line
    /// (picker rows show the badge; only the mode screen shows the detail).
    #[test]
    fn the_unsandboxed_badge_says_so_on_its_own() {
        let d = for_repository(&descriptor(Some(HookPolicy::Unsandboxed)));
        assert!(d.label.contains("NOT sandboxed"), "{}", d.label);
    }
}
