//! #66 (M1.13b) Task 25, step 5: the `class = functional` blocked-hooks case.
//!
//! Empty stub landed in step 3 so the module list is fixed before any case is
//! rewritten (`docs/superpowers/plans` / the anti-vacuity contract's ordered
//! work). Step 5 moves `blocked_hooks_are_suppressed_while_the_commit_still_succeeds`
//! here from `escape_suite.rs`, rewritten as a `const CASE_BLOCKED_HOOKS:
//! EscapeCase` under `dies_under: [M6]`, with the R8 exemption blocker naming
//! `policy_for_repo`'s hard-coded `HookMode::Run`.
//!
//! `mod harness` is the one place this file may contain setup code (R1); it is
//! empty until step 5 gives it something to hold. `sandbox::escape_contract`'s
//! `case_region` tripwire splits this file at that marker, so the marker must
//! exist even with nothing in it yet.

#[allow(dead_code)]
mod harness {}
