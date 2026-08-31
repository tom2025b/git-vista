//! Re-execute the current test binary to run one `#[ignore]`d test in a child
//! process with a controlled environment.
//!
//! # Why a test would need its own process
//!
//! Some defects are *inheritance* defects: a variable in the spawning
//! process's environment changes what a child does (`GIT_OBJECT_DIRECTORY`
//! redirecting an object write is the case that created this module — #576's
//! audit). Demonstrating one from inside `cargo test`'s
//! many-tests-one-process model forces a choice:
//!
//! * `std::env::set_var` process-wide — measured 2026-08-31 in git-vista-server's
//!   parallel binary, this poisoned sibling tests' fixture builders through
//!   the very inheritance under test: 22 foreign objects landed in the
//!   asserting test's ODB and three unrelated tests lost theirs. A lock only
//!   serializes the tests that take it.
//! * Or give the redirected environment to a **child process running exactly
//!   one test**, set on that child's `Command` alone. Nothing else can
//!   inherit what no other process carries.
//!
//! This helper is the second choice, packaged once.
//!
//! # Why it lives in this crate
//!
//! `argv_boundary.rs` walks `git-vista-server` and `git-vista-git` and
//! requires every `Command` there to be an allowlisted, literal `git` spawn —
//! the right rule for crates that ship. This crate is the deliberately
//! unsandboxed test-support layer (a dev-dependency that "must never reach
//! the release binary", per the server's own Cargo.toml) and already
//! constructs raw `Command`s for every fixture; a test-binary re-exec is the
//! same trust level as the fixture `git` beside it. Putting it here is not a
//! way around the boundary — it is the boundary working: no new spawn site
//! appears in a scanned crate.

use std::ffi::OsStr;
use std::process::{Command, Output};

/// Run `test_name` — which must be `#[ignore]`d, so ordinary runs never
/// execute it directly — in a fresh copy of the current test binary, with
/// `envs` added to the child's environment.
///
/// The child inherits everything else (`PATH`, `HOME`, the sandbox shim's
/// requirements), runs with `--nocapture` so the driven test's prints reach
/// the returned [`Output`], and with `--exact` so a name that no longer
/// matches runs *zero* tests rather than a surprise superset. Callers must
/// not treat a successful exit alone as proof the test ran — libtest exits 0
/// on "0 tests run" — which is why the pattern pairs this with a sentinel
/// printed by the driven test and asserted by the caller.
pub fn run_ignored_test(test_name: &str, envs: &[(&str, &OsStr)]) -> Output {
    let exe = std::env::current_exe().expect("the test binary knows its own path");
    let mut cmd = Command::new(exe);
    cmd.arg(test_name)
        .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"]);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("re-run the current test binary")
}
