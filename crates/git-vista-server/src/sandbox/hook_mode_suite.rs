//! M1.13b (#66): declarative functional hook-mode case.
//!
//! # The one live R8 exemption in the battery (#206)
//!
//! `blocked_hooks` needs a policy with `hook_mode: HookMode::Blocked`, and no
//! production constructor builds one: `sandbox::policy_for`,
//! `sandbox::policy_for_clone` and `sandbox::probe::boot_probe_policy` each
//! spell `HookMode::Run`, and ADR 0029 rejects the degrade-and-block posture by
//! name — a host that cannot supply the Strict tier gets a refusal
//! (`ShimError::StrictUnavailable`), never a weaker sandbox with hooks turned
//! off. So the shape this case runs against is one production genuinely cannot
//! express, and the harness builds it in `escape_contract::policy_for_case`.
//!
//! The blocker below used to read `"policy_for_repo hard-codes HookMode::Run"`.
//! That named the wrong thing twice over: `policy_for_repo` is a `#[cfg(test)]`
//! wrapper that sets no hook mode at all, and after #197 the token R8 grepped
//! for survived only inside its `debug_assert!` — so the tripwire would have
//! gone on passing even if `policy_for` had grown a route to `Blocked`. The
//! wording here now states the property R8 actually checks, over every
//! production module under `src/sandbox`. Give any production constructor a way
//! to emit `HookMode::Blocked` and R8 goes red, which is exactly when this
//! exemption must be retired and the case moved onto the production dispatch.

use super::escape_contract::{
    run_case, Class, Errno, EscapeCase, Exemption, GitPortUse, MutantId, Provenance,
};
use super::Tier;

const CASE_BLOCKED_HOOKS: EscapeCase = EscapeCase {
    id: "blocked_hooks",
    class: Class::Functional,
    tier: Tier::Network,
    hooks_blocked: true,
    build_hook: harness::blocked_hook_probe,
    probe_tag: "HOOK",
    expect_baseline: Errno(0),
    expect_baseline_provenance: Provenance::NotApplicable,
    expect_inside: Errno(2),
    expect_inside_provenance: Provenance::NotApplicable,
    expect_granted: Errno(0),
    expect_granted_provenance: Provenance::NotApplicable,
    expect_carrier_code: 0,
    dies_under: &[MutantId::M6],
    exemption: Exemption::NotProductionReachable {
        blocker: "no production policy constructor yields HookMode::Blocked",
    },
    // A shell probe that never touches the network.
    git_port: GitPortUse::Unused,
};

#[test]
fn blocked_hooks() {
    run_case(&CASE_BLOCKED_HOOKS);
}

mod harness {
    use super::super::escape_contract::HarnessCtx;

    pub(super) fn blocked_hook_probe(ctx: &HarnessCtx) -> String {
        let marker = ctx.repo.join(".git/gv_escape_hook_ran");
        format!(
            "printf 'hook ran' > {}; printf 'GVPROBE {} BEGIN\\n'; \
             printf 'HOOK rc=0 errno=0\\n'; \
             printf 'GRANTED rc=0 errno=0\\n'; printf 'GVPROBE {} END\\n'",
            marker.display(),
            ctx.nonce,
            ctx.nonce
        )
    }
}
