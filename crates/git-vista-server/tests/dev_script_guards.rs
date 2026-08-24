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
//! Each script is hermetic — it shims every tool it touches and runs against a
//! temporary `HOME` — so it is safe under `cargo test --workspace` and in CI.

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
