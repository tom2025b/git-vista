//! What the topbar says about HEAD when there is no branch name to show
//! (#473).
//!
//! # Why this is its own module rather than a `match` inside the view
//!
//! `mod app` is `#[cfg(target_arch = "wasm32")]`, so nothing inside it is
//! reachable from a host test. A mapping that lives only in markup cannot be
//! tested, and "absent" then renders as empty with nothing able to notice —
//! which is the defect this fixes, not a shape to repeat. Same posture as
//! `hook_policy_disclosure`: the decision is pure and host-tested, the view
//! only draws what it returns.

use git_vista_protocol::HeadState;

/// The notice for a HEAD that needs one, or `None` when it does not.
///
/// Only the broken state earns a notice. A detached HEAD is an ordinary,
/// deliberate state that has never had one; labelling it would cry wolf on the
/// common case, and a warning that fires on healthy repositories is a warning
/// nobody reads.
pub fn head_notice(state: HeadState) -> Option<&'static str> {
    match state {
        HeadState::Unresolvable => Some("HEAD is broken"),
        HeadState::OnBranch | HeadState::Detached | HeadState::Unborn | HeadState::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #473: the topbar showed nothing for a HEAD that resolves to nothing —
    /// indistinguishable from a healthy detached HEAD, which also shows
    /// nothing.
    ///
    /// The loop over the ordinary states is the load-bearing half. A test that
    /// only checked "Unresolvable produces a notice" would pass against a
    /// version that labelled every branchless HEAD as broken.
    ///
    /// MUTATION 1: return the notice for `Detached` too — red, an ordinary
    ///   state is reported as a fault.
    /// MUTATION 2: return `None` for `Unresolvable` — red, the silence is back.
    #[test]
    fn only_a_head_that_resolves_to_nothing_earns_a_notice() {
        assert_eq!(head_notice(HeadState::Unresolvable), Some("HEAD is broken"));

        for ordinary in [
            HeadState::OnBranch,
            HeadState::Detached,
            HeadState::Unborn,
            HeadState::Unknown,
        ] {
            assert_eq!(
                head_notice(ordinary),
                None,
                "{ordinary:?} is not a fault and must not be labelled as one"
            );
        }
    }
}
