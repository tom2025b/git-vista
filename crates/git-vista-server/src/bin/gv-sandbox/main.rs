//! M1.13b (#66): the fused sandbox shim.
//!
//! **Fused on purpose.** Round 4 ran Landlock and seccomp as separate helper
//! binaries and hit `exec …: Permission denied` — a helper's own path must sit
//! inside a granted tree or the `execve` of it is denied, and the failure made
//! a benchmark look *faster* than bare git because nothing actually ran. One
//! binary that applies Landlock, applies seccomp (Task 4), and `execve`s git
//! avoids the entire class.
//!
//! This file must contain `.exec()` and must **not** contain `.spawn()`,
//! `.output()` or `.status()`: the shim replaces its own process image, it
//! never becomes a parent. It also names `git` literally, so the argv tripwire
//! in `argv_boundary.rs` can prove it cannot exec anything else.
//!
//! # Everything here was measured, not reasoned
//!
//! Four rounds of this design died of reasoning about kernel behaviour instead
//! of measuring it. Every non-obvious constant and every ordering decision
//! below carries the measurement that produced it. If you change one, measure
//! it again — the header on this host is *stale* relative to the running
//! kernel, so reading `/usr/include/linux/landlock.h` is not sufficient
//! evidence for anything.
//!
//! # This file is a trampoline, not the shim
//!
//! The actual pipeline (Landlock, then seccomp, then `execve`) lives in
//! `imp/mod.rs`, gated `#[cfg(unix)]` as one unit. It is unix-only by
//! construction — Landlock and seccomp are both Linux kernel facilities with
//! no Windows equivalent — and there is no partial-Windows behaviour to carve
//! out of that pipeline without weakening the boundary it enforces. This file
//! stays a thin, always-compiling trampoline for exactly one reason: keeping
//! the real body in its own file, unindented, is what `main.rs` at a crate
//! root vs. an inline `mod imp { .. }` block cannot both give you at once —
//! see the git history on this file for the inline version and why it did not
//! survive `cargo fmt --check`. Splitting to a real file costs nothing here
//! and avoids that permanently.
//!
//! This is NOT a Windows port of the sandbox — it is what makes the REST of
//! the `git-vista-server` package (the library, and the ordinary
//! `git-vista-server` binary) checkable on non-unix targets at all. See the
//! `[target.'cfg(unix)'.dependencies]` note on `seccompiler` in Cargo.toml,
//! and docs/NATIVE_DEPENDENCIES.md.

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "gv-sandbox is a Linux-only sandbox shim (Landlock + seccomp, \
         M1.13b #66) and does not run on this platform."
    );
    std::process::exit(1);
}

#[cfg(unix)]
mod imp;

#[cfg(unix)]
fn main() {
    imp::main();
}
