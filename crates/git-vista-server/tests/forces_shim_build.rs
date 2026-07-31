//! This file exists so that `cargo test` builds the `gv-sandbox` binary.
//!
//! # Do not delete this, and do not "tidy it up" into nothing
//!
//! Measured 2026-07-29: `cargo test` does **not** build a package's `[[bin]]`
//! targets merely because they are declared. It builds them when the
//! invocation's build plan includes an integration-test target — that is, when
//! at least one file exists in `tests/`. Any file will do; it does not need to
//! reference the binary, and an earlier claim that it had to mention
//! `CARGO_BIN_EXE_gv-sandbox` was measured and refuted (a clean-target rebuild
//! with every such reference deleted still produced the binary).
//!
//! Without a file here, `target/<profile>/gv-sandbox` is never produced, and
//! every unit test that resolves the shim beside the running test binary fails
//! with a bare "not found" that looks like a bug in the resolver rather than a
//! missing build step.
//!
//! Note also what this file deliberately does *not* do: it does not use
//! `env!("CARGO_BIN_EXE_gv-sandbox")`. That macro is unavailable inside the
//! `#[cfg(test)] mod` unit tests where the sandbox suite actually lives — it is
//! a hard compile error there, in a bin target and in a lib target alike — so
//! the resolver in `sandbox::shim` is the mechanism, and this file is only the
//! trigger that makes the binary exist for it to find.

/// Asserts the reason this file exists actually holds: after `cargo test`, the
/// shim is on disk where `sandbox::shim::shim_path` will look for it.
///
/// If this fails, the build-plan behaviour above has changed and every
/// shim-dependent unit test is about to fail for a reason that will look
/// unrelated.
#[test]
fn the_shim_binary_is_built_and_sits_beside_the_test_binary() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe.parent().expect("exe parent").to_path_buf();
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent().expect("deps parent").to_path_buf();
    }
    let shim = dir.join("gv-sandbox");
    assert!(
        shim.is_file(),
        "gv-sandbox was not built by this `cargo test` invocation.\n\
         Looked for: {}\n\
         This file (tests/forces_shim_build.rs) exists precisely to pull the \
         binary into the build plan. If it is present and the binary still is \
         not, Cargo's behaviour has changed and `sandbox::shim` needs a new \
         mechanism.",
        shim.display()
    );
}
