//! M1.13b (#66) Task 16: INV-15's per-repository half — the one place the
//! server's internal [`Tier`] becomes the disclosed
//! [`git_vista_protocol::HookPolicy`] a client actually sees.
//!
//! INV-15 is *"the hook policy is always disclosed, not only when degraded."*
//! `SECURITY_MODEL.md:271` (the "Decide hook policy explicitly" bullet under
//! Command Execution — cited by its opening words as well as its line, because
//! that document has already moved this bullet twice) requires the UI to
//! **report** the fact, present tense, for as long as it is true. A
//! silently-applied hook policy is exactly
//! the failure the invariant exists to prevent, so the mapping below has one
//! job: never produce a value that claims more than the tier dispatch actually
//! delivers.
//!
//! # This file does not decide anything — it translates
//!
//! The decision of what tier a repository's operations run in is
//! [`super::tier_for`], and it already landed (Task 8). If this module
//! re-derived that decision from `ProbeVerdict` and a trust lookup, the
//! disclosed value and the enforced value would be two independent
//! computations that could disagree — the disclosure would keep saying
//! `strict` after the dispatch changed. So [`hook_policy_for_repo`] *calls*
//! `tier_for` and only renames its answer, and
//! [`hook_policy_for_tier`] is an exhaustive match, so a new `Tier` variant is
//! a compile error here rather than a silently mis-disclosed policy.
//!
//! # ADR 0029: the plan's `CapabilityAbsent → Blocked` mapping is REJECTED
//!
//! Plan Task 16.6
//! (`docs/superpowers/plans/2026-07-28-m1.13b-sandbox.md:4915-4923`) writes:
//!
//! ```text
//! ProbeVerdict::CapabilityAbsent { .. } | ProbeVerdict::FailOpen { .. } => HookPolicy::Blocked,
//! ```
//!
//! **That mapping is not implemented here, deliberately.** It is the
//! degrade-and-block-hooks posture ADR 0029 rejects *by name*: "Rejected as an
//! attempted middle path — run the operation in a weaker tier but suppress
//! `.git/hooks/*` so the missing isolation cannot be exploited through a hook.
//! This still degrades silently (a repository that asked for Strict gets a
//! different, weaker tier without refusing)." ADR 0029 is Accepted and binding;
//! it names this exact plan snippet in its "Where the plan still disagrees with
//! this ADR" section and says the mapping must go. INV-13 is *refuse*, not
//! *degrade*.
//!
//! The plan itself agrees, at lines 3344-3352: *"Task 16 should either drop the
//! arm (taking `ProbeVerdict::Contained` as a precondition) or keep it as an
//! `unreachable!` carrying its reason."* Neither of those two is used either,
//! for a reason ADR 0029 also states: the refusal must reach the handler "as a
//! proper refusal, not a panic reachable from a network request," and
//! `hook_policy_for_repo`'s caller (`catalog.rs`, building a
//! `RepositoryDescriptor` — wired up in #202, and no longer merely prospective)
//! *is* reachable from a network request. So the third
//! option is taken: a [`Result`], whose `Err` arm names the verdict and forces
//! the caller to choose. There is no [`HookPolicy`] value that honestly means
//! "operations on this repository will refuse to run" — inventing one would be
//! the same silent degrade wearing a different name.
//!
//! Two further reasons the `Blocked` arm would have been a *vacuous* claim
//! rather than merely a wrong one, both checked rather than asserted:
//!
//! * **Nothing in the server can block hooks.** `sandbox::escape_contract`'s R8
//!   scan (`CHECKED_BLOCKERS`) fails the build if any production `Policy`
//!   literal sets a blocked hook mode; every production constructor spells
//!   `hook_mode: HookMode::Run` as a literal. Disclosing `blocked` would have
//!   promised a mechanism that provably does not exist.
//! * **The arm is unreachable in a running server.** `probe::run_at_startup`
//!   gates boot: only `ProbeVerdict::Contained` returns `Ok`, everything else
//!   exits the process. A live server's verdict is always `Contained`.
//!
//! # What this module deliberately does not disclose
//!
//! `hook_policy_for_repo` reports the policy for a **local** operation
//! (`NetworkNeed::Local`), because that is what a repository model is: a
//! standing property of the repository, not of one request. A `push` on the
//! same untrusted repository transiently runs under [`Tier::Network`], which
//! this per-repository value does not express. That gap is real and named here
//! rather than papered over: closing it means disclosing policy per *operation*
//! as well as per repository, which needs a call site that knows the operation
//! (the planner), not this one. Reporting the weaker `Network` for every
//! repository "just in case" was considered and rejected — it would fly the
//! banner permanently on every repository and so stop distinguishing anything,
//! which is the disclosure equivalent of a warning nobody reads.

use std::path::Path;

use git_vista_protocol::HookPolicy;

use super::probe::ProbeVerdict;
use super::{tier_for, NetworkNeed, Tier};

/// Why no [`HookPolicy`] could be reported for a repository: the host cannot
/// supply the tier the repository's operations require, so those operations
/// **refuse** (INV-13 / ADR 0029) rather than run under a weaker policy.
///
/// Carries the words from the verdict — not a bare marker — so a caller can put
/// the same diagnosis in front of an operator that `probe::run_at_startup`
/// prints at boot, and so `capability_absent` (install bubblewrap) stays
/// distinguishable from `fail_open` (a git-vista bug). Collapsing the two would
/// make the second look like the first, which is the distinction
/// `sandbox::probe`'s own module doc calls the one that matters most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookPolicyRefused {
    /// The host cannot supply the strict tier. Names the missing capabilities.
    CapabilityAbsent { missing: Vec<String> },
    /// The composed launcher did not contain a hostile hook. A git-vista bug.
    FailOpen { failed_checks: Vec<String> },
}

impl std::fmt::Display for HookPolicyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookPolicyRefused::CapabilityAbsent { missing } => write!(
                f,
                "this host cannot supply the sandbox tier this repository requires \
                 (missing: {missing:?}), so its operations refuse to run. Install \
                 bubblewrap and enable unprivileged user namespaces. (INV-13 / ADR \
                 0029 — there is no degraded mode and hooks are not merely blocked.)"
            ),
            HookPolicyRefused::FailOpen { failed_checks } => write!(
                f,
                "the sandbox self-test found a hole (checks: {failed_checks:?}), so \
                 this repository's operations refuse to run. This is a git-vista bug, \
                 not a host configuration problem — do not work around it."
            ),
        }
    }
}

/// [`HookPolicy`] in its role as *the disclosed form of a [`Tier`]* — the
/// return type of the seam below, and nothing else.
///
/// The alias earns its keep twice. It names the role at the one place the two
/// vocabularies meet, and it keeps a false positive out of a tripwire this
/// change must not weaken: `sandbox::escape_contract`'s R8 check scans every
/// production file under `src/sandbox/` for the text `Policy {`, treats each
/// hit as a `Policy` struct literal, and panics with "the scan broke" when it
/// cannot find a `hook_mode:` field inside. A function written `-> HookPolicy
/// {` puts exactly that text in front of a brace. R8 is a deliberate text scan
/// and lives in a file outside this change's ownership, so the seam names its
/// own return type rather than the check being loosened to accommodate it. The
/// brittleness is reported, not silently absorbed.
pub(crate) type Disclosed = HookPolicy;

/// Rename one internal [`Tier`] to the wire vocabulary. Exhaustive on purpose:
/// a fourth tier must be given a disclosed name deliberately, at review time,
/// instead of inheriting one from a wildcard arm.
///
/// The two vocabularies use the same three words, so this looks like a
/// no-op — it is not. It is the *seam*: `Tier` is a server-internal detail free
/// to be renamed, `HookPolicy` is a wire contract pinned by a golden fixture,
/// and having exactly one crossing point is what lets either move without the
/// other drifting silently.
pub(crate) fn hook_policy_for_tier(tier: Tier) -> Disclosed {
    match tier {
        Tier::Strict => HookPolicy::Strict,
        Tier::Network => HookPolicy::Network,
        Tier::Unsandboxed => HookPolicy::Unsandboxed,
    }
}

/// INV-15's per-repository disclosure: what a local operation on `repo`
/// actually runs under, or a named refusal.
///
/// # Trust is checked first, and that ordering is load-bearing
///
/// `super::repo_is_trusted` is consulted before `verdict` is looked at, mirroring
/// `tier_for`'s own `(true, _)` arm. An operator-trusted repository runs with no
/// sandbox at all, so a host that cannot compose the strict tier changes nothing
/// about it — there is no capability to be absent. Checking the verdict first
/// would refuse a repository that never needed the thing the host is missing.
///
/// It is the same trust source `tier_for` reads (`sandbox::trust::is_trusted`,
/// keyed on the canonicalised path, backed by a marker file under the server's
/// own state directory that a sandboxed repository can read but never write) —
/// reached through `super::repo_is_trusted` rather than re-implemented, so
/// there is exactly one answer to "is this repository trusted" and the
/// disclosure cannot disagree with the dispatch.
///
/// # The `Err` arm
///
/// See this module's doc comment. `CapabilityAbsent` does **not** become
/// `HookPolicy::Blocked`; that plan mapping is rejected by ADR 0029.
pub(crate) fn hook_policy_for_repo(
    repo: &Path,
    verdict: &ProbeVerdict,
) -> Result<HookPolicy, HookPolicyRefused> {
    hook_policy_for_trusted_repo(super::repo_is_trusted(repo), verdict)
}

/// [`hook_policy_for_repo`] with the filesystem read hoisted out, so the whole
/// mapping is testable without granting real operator trust.
///
/// That split is not tidiness. `trust::grant` writes a marker under
/// `state::sandbox_trust_dir()` — the operator's real `~/.local/state`
/// directory — and `trust.rs`'s own test module documents why redirecting it
/// through `$HOME`/`XDG_STATE_HOME` is forbidden here: a previous version did,
/// leaked the environment mutation, and intermittently killed every parallel
/// test that reads `$HOME`. So the trusted branch is proved on this pure
/// function, `trust::is_trusted`'s own fail-closed behaviour is proved in
/// `trust.rs`, and [`hook_policy_for_repo`] is proved end to end for the
/// untrusted case against a real path.
fn hook_policy_for_trusted_repo(
    trusted: bool,
    verdict: &ProbeVerdict,
) -> Result<HookPolicy, HookPolicyRefused> {
    if trusted {
        // `tier_for(_, true)` is `Unsandboxed` for every network need, so the
        // `NetworkNeed` passed here cannot change the answer — routing through
        // `tier_for` anyway keeps this arm honest if that ever stops being true.
        return Ok(hook_policy_for_tier(tier_for(NetworkNeed::Local, true)));
    }
    // No wildcard: a new `ProbeVerdict` variant must be given a disclosure
    // decision here rather than inheriting one.
    match verdict {
        ProbeVerdict::Contained => Ok(hook_policy_for_tier(tier_for(NetworkNeed::Local, false))),
        // ADR 0029, not the plan. Refuse; do not disclose a policy at all.
        ProbeVerdict::CapabilityAbsent { missing } => Err(HookPolicyRefused::CapabilityAbsent {
            missing: missing.iter().map(|s| (*s).to_string()).collect(),
        }),
        ProbeVerdict::FailOpen { failed_checks } => Err(HookPolicyRefused::FailOpen {
            failed_checks: failed_checks.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absent() -> ProbeVerdict {
        ProbeVerdict::CapabilityAbsent {
            missing: vec!["bwrap", "user_namespaces"],
        }
    }

    fn fail_open() -> ProbeVerdict {
        ProbeVerdict::FailOpen {
            failed_checks: vec!["fs_write_outside=OPEN want=DENIED".to_string()],
        }
    }

    /// **The negative case this task exists to pin.** ADR 0029 rejects
    /// degrade-and-block-hooks by name, and plan Task 16.6 implements exactly
    /// that. This asserts the plan's mapping is *absent*: a capability-absent
    /// host produces a refusal, and specifically not `HookPolicy::Blocked`.
    ///
    /// Written as two separate assertions on purpose. `is_err()` alone would
    /// still pass if a later edit reintroduced `Blocked` behind some other
    /// error, and `!= Blocked` alone would pass for any wrong-but-not-blocked
    /// policy, so both halves are needed to make the claim non-vacuous.
    #[test]
    fn capability_absent_refuses_and_never_becomes_blocked() {
        let got = hook_policy_for_trusted_repo(false, &absent());
        assert_eq!(
            got,
            Err(HookPolicyRefused::CapabilityAbsent {
                missing: vec!["bwrap".to_string(), "user_namespaces".to_string()],
            }),
            "ADR 0029: a capability-absent host must refuse, naming what is missing"
        );
        assert_ne!(
            got.ok(),
            Some(HookPolicy::Blocked),
            "plan Task 16.6's `CapabilityAbsent => HookPolicy::Blocked` is the \
             degrade-and-block-hooks posture ADR 0029 rejects by name"
        );
    }

    /// The same for `FailOpen`, and it must stay a *distinct* refusal:
    /// `capability_absent` tells an operator to install bubblewrap,
    /// `fail_open` tells them they have found a git-vista bug. Merging them
    /// would make the second look like the first.
    #[test]
    fn fail_open_refuses_distinguishably_and_never_becomes_blocked() {
        let got = hook_policy_for_trusted_repo(false, &fail_open());
        assert!(matches!(got, Err(HookPolicyRefused::FailOpen { .. })));
        assert_ne!(got.ok(), Some(HookPolicy::Blocked));

        let a = hook_policy_for_trusted_repo(false, &absent()).unwrap_err();
        let f = hook_policy_for_trusted_repo(false, &fail_open()).unwrap_err();
        assert_ne!(a, f, "the two refusals must not collapse into one");
        // ...and the diagnosis reaches an operator without reading this file.
        assert!(a.to_string().contains("bwrap"), "{a}");
        assert!(f.to_string().contains("fs_write_outside=OPEN"), "{f}");
        assert!(
            !a.to_string().contains("bug"),
            "capability absence is a host problem, not a git-vista bug: {a}"
        );
        assert!(
            f.to_string().contains("bug"),
            "a hole IS a git-vista bug and must say so: {f}"
        );
    }

    /// The happy path, and the only verdict a running server can hold: an
    /// untrusted repository on a contained host discloses `strict`, the one
    /// value that silences the banner.
    #[test]
    fn contained_and_untrusted_discloses_strict() {
        assert_eq!(
            hook_policy_for_trusted_repo(false, &ProbeVerdict::Contained),
            Ok(HookPolicy::Strict)
        );
        assert!(!HookPolicy::Strict.requires_banner());
    }

    /// Operator trust wins over every verdict, including the ones that refuse.
    /// A repository the operator runs with no sandbox does not need the tier
    /// the host is missing, so refusing it would be refusing for a capability
    /// it never asked for — and the answer must fly the banner permanently.
    #[test]
    fn operator_trust_discloses_unsandboxed_whatever_the_verdict() {
        for verdict in [ProbeVerdict::Contained, absent(), fail_open()] {
            assert_eq!(
                hook_policy_for_trusted_repo(true, &verdict),
                Ok(HookPolicy::Unsandboxed),
                "trust must be decided before the verdict is consulted ({verdict:?})"
            );
        }
        assert!(HookPolicy::Unsandboxed.requires_banner());
    }

    /// The seam, pinned in both directions: every `Tier` has a disclosed name,
    /// and no two tiers share one. A mapping that collapsed `Strict` and
    /// `Network` onto the same disclosed value would silence the banner for a
    /// network-reachable sandbox, which is the whole point of separating them.
    #[test]
    fn every_tier_maps_to_a_distinct_disclosed_policy() {
        let tiers = [Tier::Strict, Tier::Network, Tier::Unsandboxed];
        let mapped: Vec<HookPolicy> = tiers.iter().copied().map(hook_policy_for_tier).collect();
        assert_eq!(
            mapped,
            vec![
                HookPolicy::Strict,
                HookPolicy::Network,
                HookPolicy::Unsandboxed
            ]
        );
        for (i, a) in mapped.iter().enumerate() {
            for b in &mapped[i + 1..] {
                assert_ne!(a, b, "two tiers collapsed onto one disclosed policy");
            }
        }
        // The banner follows the tier, not the other way round: only the
        // fullest-isolation tier is silent.
        assert!(!hook_policy_for_tier(Tier::Strict).requires_banner());
        assert!(hook_policy_for_tier(Tier::Network).requires_banner());
        assert!(hook_policy_for_tier(Tier::Unsandboxed).requires_banner());
    }

    /// The disclosure agrees with the dispatch, checked against `tier_for`
    /// itself rather than against a second copy of its rules — this is the
    /// anti-drift test the module doc argues for. If `tier_for` ever starts
    /// returning a different tier for a local operation, this fails instead of
    /// the server quietly disclosing a policy it no longer runs under.
    #[test]
    fn disclosure_tracks_tier_for_rather_than_restating_it() {
        for trusted in [false, true] {
            let disclosed = hook_policy_for_trusted_repo(trusted, &ProbeVerdict::Contained)
                .expect("Contained never refuses");
            let enforced = hook_policy_for_tier(tier_for(NetworkNeed::Local, trusted));
            assert_eq!(
                disclosed, enforced,
                "disclosed policy diverged from tier_for (trusted={trusted})"
            );
        }
    }

    /// The real wiring, not the pure function: an ordinary temp directory has
    /// no trust marker, so `hook_policy_for_repo` must reach
    /// `trust::is_trusted` and get `false` — the fail-closed answer — and
    /// disclose `strict`. This is what proves the public entry point is
    /// actually joined to the trust store rather than hard-coding `false`.
    #[test]
    fn an_untrusted_real_path_discloses_strict_through_the_public_entry_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            hook_policy_for_repo(dir.path(), &ProbeVerdict::Contained),
            Ok(HookPolicy::Strict)
        );
        // A path that does not exist cannot canonicalise, and every uncertainty
        // in the trust chain means untrusted — so it must not become
        // `Unsandboxed` by accident.
        let missing = dir.path().join("no-such-repo");
        assert_eq!(
            hook_policy_for_repo(&missing, &ProbeVerdict::Contained),
            Ok(HookPolicy::Strict),
            "an unresolvable path must fail closed, never disclose unsandboxed"
        );
    }
}
