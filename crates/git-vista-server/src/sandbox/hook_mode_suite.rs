//! M1.13b (#66): declarative functional hook-mode case.
//!
//! # The former R8 exemption is retired
//!
//! #831 makes `policy_for_clone_checkout` produce `HookMode::Blocked`: #827
//! removed every fresh-clone hook source, while #831's server-authored LFS
//! filter is not a hook. The battery now reaches that production constructor
//! through `escape_contract::policy_for_case`; no harness-fabricated policy and
//! no named exemption remain. ADR 0029 is unchanged for ordinary operations:
//! an unavailable Strict tier is still a refusal, never a degraded Network
//! policy with hooks suppressed.

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
    exemption: Exemption::None,
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
