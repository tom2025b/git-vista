//! M1.13b (#66): declarative functional hook-mode case.

use super::escape_contract::{Class, Errno, EscapeCase, Exemption, MutantId, run_case};
use super::Tier;

const CASE_BLOCKED_HOOKS: EscapeCase = EscapeCase {
    id: "blocked_hooks",
    class: Class::Functional,
    tier: Tier::Network,
    hooks_blocked: true,
    build_hook: harness::blocked_hook_probe,
    probe_tag: "HOOK",
    expect_baseline: Errno(0),
    expect_inside: Errno(2),
    expect_granted: Errno(0),
    expect_carrier_code: 0,
    dies_under: &[MutantId::M6],
    exemption: Exemption::NotProductionReachable {
        blocker: "policy_for_repo hard-codes HookMode::Run",
    },
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
             printf 'HOOK rc=0 errno=0 Seccomp: 0 NoNewPrivs: 0\\n'; \
             printf 'GRANTED rc=0 errno=0\\n'; printf 'GVPROBE {} END\\n'",
            marker.display(),
            ctx.nonce,
            ctx.nonce
        )
    }
}
