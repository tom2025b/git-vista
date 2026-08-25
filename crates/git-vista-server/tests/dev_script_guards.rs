//! Runs the `ci/*_test.sh` guards that protect `dev` itself, so `cargo test`
//! is what executes them (#469).
//!
//! # Why these are shell tests, and why they need this file
//!
//! What they guard lives in `dev`: the shell's errexit state across a
//! subshell boundary, and which `node` a bare `node` resolves to. No Rust test
//! can observe either, so the honest test drives the real script with the
//! toolchain replaced underneath it.
//!
//! But a guard nobody runs is not a guard. Before this file, `ci/` held those
//! scripts and **nothing invoked them** — not `dev gate`, not a workflow, not
//! `cargo test`. That is the same shape as the defect #469 is about: a check
//! that exists, looks reassuring, and never fires.
//!
//! **Every `ci/*_test.sh` is wrapped here, not just the new one.** Covering one
//! and leaving the others unrun would be worse than the gap it replaced: the
//! file's name would imply coverage that does not exist, which is the exact
//! trade this file argues against. If a script is ever added to `ci/` and
//! deliberately left out, say which and why right here.
//!
//! Each is hermetic — shims every tool it reaches, or sources `dev` and calls
//! one function against temporary directories — so all three are safe under
//! `cargo test --workspace` and in CI. None builds anything real, none writes
//! to the evidence store, and none touches the operator's checkout.

use std::path::PathBuf;
use std::process::Command;

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repo root resolves from CARGO_MANIFEST_DIR")
}

fn run_guard(script: &str) {
    let root = repo_root();
    let path = root.join("ci").join(script);
    assert!(
        path.is_file(),
        "the guard script {script} is missing — it is not optional; delete this \
         test deliberately or restore the script"
    );

    let out = Command::new("bash")
        .arg(&path)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("could not run {script}: {e}"));

    assert!(
        out.status.success(),
        "{script} failed ({});\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// #469: `cmd_browser` must refuse a node Playwright will refuse, say so by
/// version, stop before the build, accept a new-enough one, and put the node
/// it accepted on `PATH` — `run.sh` reaches `npx`, not `node`.
#[test]
fn the_browser_leg_checks_nodes_version_not_merely_its_presence() {
    run_guard("browser_node_version_test.sh");
}

/// #434: `./dev gate` must be able to FAIL, and must stop at the first failing
/// step. The gate once could not say no, and recorded a `verified: true` for a
/// commit whose wasm build could not compile.
#[test]
fn a_failing_step_fails_the_gate_and_stops_it() {
    run_guard("gate_errexit_test.sh");
}

/// #476: `gv doctor` must MEASURE the managed-clones root from the running
/// listener's own environment, and say `unknown` when it cannot — never print
/// a constant. The line it replaces had never been correct on this box, and it
/// sat directly beneath a measured line whose credibility it borrowed.
#[test]
fn the_doctor_measures_the_clones_root_and_refuses_to_guess() {
    run_guard("doctor_clones_root_test.sh");
}

/// #331 follow-up: `dev testbed` must build onto the scratch SSD when it is
/// attached and must never fall back into the caller's current directory when
/// it is not — the dangling-symlink case, which is the whole reason the
/// fallback exists.
#[test]
fn the_testbed_target_never_falls_back_into_the_callers_directory() {
    run_guard("testbed_target_test.sh");
}
