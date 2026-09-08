//! Companion to `forces_shim_build.rs` for `gv-sandbox-reaper` (#728).
//!
//! Cargo already builds every `[[bin]]`/autodiscovered `src/bin/*` target once
//! any file exists under `tests/` — `forces_shim_build.rs`'s own module doc
//! measured that. This file's only job is to assert the *reaper* binary
//! specifically landed where `sandbox::reaper::reaper_path` will look for it,
//! the same way `forces_shim_build.rs` already does for `gv-sandbox` — one
//! assertion per binary, so a broken build of either is caught by name rather
//! than by a bare "not found" three modules away that looks unrelated.

/// Mirrors `forces_shim_build.rs::the_shim_binary_is_built_and_sits_beside_the_test_binary`.
#[test]
fn the_reaper_binary_is_built_and_sits_beside_the_test_binary() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe.parent().expect("exe parent").to_path_buf();
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent().expect("deps parent").to_path_buf();
    }
    let reaper = dir.join("gv-sandbox-reaper");
    assert!(
        reaper.is_file(),
        "gv-sandbox-reaper was not built by this `cargo test` invocation.\n\
         Looked for: {}\n\
         This file (tests/forces_reaper_build.rs) exists precisely to make that \
         failure legible. If a file already exists under tests/ (it does — this \
         one) and the binary still is not built, Cargo's autobins behaviour has \
         changed and `sandbox::reaper` needs a new mechanism.",
        reaper.display()
    );
}
