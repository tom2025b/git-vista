//! Regression test for the symlinked-`$HOME` exclude bypass (issue tracked in
//! the M1.13b sandbox hardening pass, 2026-07-30).
//!
//! # What this proves, and why it has to be an integration test
//!
//! `gv-sandbox/main.rs`'s own `#[cfg(test)] mod tests` builds real rulesets
//! and calls the real `landlock_add_rule`, but deliberately never calls
//! `landlock_restrict_self` on the test process — restricting the process
//! running the test suite would be irreversible for the rest of that process's
//! life. That means the unit tests can prove a rule the kernel *would have
//! accepted*, but not what a process actually confined by the resulting
//! ruleset can and cannot read.
//!
//! This test proves the latter, the only way that is possible: by running the
//! **actual compiled shim binary** as a real, separate, `execve`'d process —
//! the exact one `sandbox::spawn` launches in production — against a real
//! symlinked directory on disk, and reading the outcome off its exit code and
//! stdout. `tests/forces_shim_build.rs` explains why an integration test is
//! what pulls `gv-sandbox` into the build plan at all, and why this file
//! resolves it beside its own `current_exe()` rather than via
//! `CARGO_BIN_EXE_gv-sandbox` — matching that file's convention rather than
//! introducing a second way to find the same binary.
//!
//! # The bug this catches
//!
//! `sandbox::mod::secret_excludes_for_home` builds excludes as unresolved
//! string joins over `$HOME` — `home.join(".ssh")` and friends — and
//! `gv-sandbox`'s `enumerate` resolves every entry it walks with
//! `std::fs::canonicalize` before testing it against that list. If a
//! component of the granted tree is a symlink, the resolved walked path and
//! the unresolved exclude live in different string namespaces, and the pure
//! lexical comparison in `is_or_inside_exclude` never matches — silently
//! granting the "excluded" secret through its symlinked name. The fix
//! (`resolve_excludes` in `gv-sandbox/main.rs`, called at the top of
//! `apply_landlock`) canonicalises the excludes once, in-process, before the
//! first grant is built, and `grant_tree` canonicalises the tree root the
//! same way so the two stay in the same namespace at every comparison.
//!
//! # Mutation-tested
//!
//! Commenting out the `resolve_excludes` call in `apply_landlock` (passing
//! `&a.excludes` again instead of the resolved list) makes this test fail:
//! the canary read succeeds and prints its contents instead of being denied.
//! Confirmed 2026-07-30; the call is restored in the committed source.

use std::path::PathBuf;
use std::process::Command;

/// Where the shim landed, resolved the same way `tests/forces_shim_build.rs`
/// does: beside this integration test's own binary, stripping the `deps`
/// component `cargo test` inserts.
fn shim_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe.parent().expect("exe parent").to_path_buf();
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent().expect("deps parent").to_path_buf();
    }
    let shim = dir.join("gv-sandbox");
    assert!(
        shim.is_file(),
        "gv-sandbox not found at {} — see tests/forces_shim_build.rs",
        shim.display()
    );
    shim
}

/// A directory tree with a symlinked component and a canary secret beneath
/// it: `<tmp>/real/secretdir/canary.txt` reachable both directly and through
/// `<tmp>/linked -> <tmp>/real`. The exclude passed to the shim names the
/// **unresolved** path — `<tmp>/linked/secretdir` — exactly as
/// `secret_excludes_for_home` would for a symlinked `$HOME`, never the
/// resolved `<tmp>/real/secretdir`.
struct Fixture {
    _root: tempfile::TempDir,
    linked_tree: PathBuf,
    unresolved_exclude: PathBuf,
    unresolved_canary: PathBuf,
    unresolved_sibling: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let real = root.path().join("real");
    let secretdir = real.join("secretdir");
    let ok = real.join("ok");
    std::fs::create_dir_all(&secretdir).expect("mkdir secretdir");
    std::fs::create_dir_all(&ok).expect("mkdir ok");

    let canary = secretdir.join("canary.txt");
    std::fs::write(&canary, "[secret]\n\ttoken = LEAKED_CANARY_VALUE\n").expect("write canary");
    let sibling = ok.join("normal.txt");
    std::fs::write(&sibling, "[normal]\n\tvalue = fine\n").expect("write sibling");

    let linked = root.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).expect("symlink real -> linked");

    Fixture {
        unresolved_exclude: linked.join("secretdir"),
        unresolved_canary: linked.join("secretdir").join("canary.txt"),
        unresolved_sibling: linked.join("ok").join("normal.txt"),
        linked_tree: linked,
        _root: root,
    }
}

/// Run the shim against `fx`, asking it to `git config -f <target> --list` —
/// the same probe `sandbox/mod.rs`'s own doc comments use to measure a file
/// grant, because a config-format file's contents come back verbatim in
/// stdout on success and `git` reports `Permission denied` distinctly on
/// denial, so the two outcomes cannot be confused with each other.
fn run_shim_against(fx: &Fixture, target: &std::path::Path) -> std::process::Output {
    let shim = shim_path();
    // The dynamic linker and git itself need to be executable/readable, and
    // git needs a writable /dev/null — the same minimum production grants via
    // `DEFAULT_RO_TREES`/`DEFAULT_RW_TREES` (`sandbox/mod.rs`), reproduced
    // here by hand because this test drives the shim directly rather than
    // through `sandbox::spawn`.
    Command::new(&shim)
        .args(["--abi-floor", "6"])
        .args(["--ro", "/usr"])
        .args(["--ro", "/bin"])
        .args(["--ro", "/lib"])
        .args(["--ro", "/lib64"])
        .args(["--ro", "/etc"])
        .arg("--ro")
        .arg(&fx.linked_tree)
        .args(["--rw", "/dev"])
        .arg("--exclude")
        .arg(&fx.unresolved_exclude)
        .arg("--hooks-run")
        .arg("--net-deny")
        .arg("--")
        .arg("git")
        .arg("config")
        .arg("-f")
        .arg(target)
        .arg("--list")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("gv-sandbox runs")
}

/// The premise: this host can actually demonstrate the claim below. A hard
/// failure here, never a skip — matching `handled_ruleset`'s own reasoning in
/// `gv-sandbox/main.rs`'s unit tests (a green test that proved nothing is
/// worse than a red one), and matching `escape_contract::run_case`'s refusal
/// to treat a missing capability as anything but a harness defect on a box
/// this project runs its escape battery on.
fn assert_host_can_run_this() {
    assert!(
        std::path::Path::new("/usr").is_dir()
            && std::path::Path::new("/bin").is_dir()
            && std::path::Path::new("/dev").is_dir(),
        "this test's minimal grant set assumes a standard Linux layout"
    );
}

/// The regression: a secret excluded by its **unresolved**, symlinked name
/// must not be readable through that same symlinked name. Before the fix,
/// this printed `secret.token=LEAKED_CANARY_VALUE` and exited 0 — measured
/// directly against this binary, 2026-07-30.
#[test]
fn a_secret_excluded_through_a_symlinked_component_is_not_readable() {
    assert_host_can_run_this();
    let fx = fixture();
    let out = run_shim_against(&fx, &fx.unresolved_canary.clone());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("LEAKED_CANARY_VALUE"),
        "the excluded secret was readable through its symlinked path — the exclude-resolution \
         fix in `apply_landlock`/`grant_tree` (gv-sandbox/main.rs) is not doing its job.\n\
         combined output:\n{combined}"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "git must fail to read the excluded file, not merely omit the value.\n\
         combined output:\n{combined}"
    );
}

/// The control: a sibling file in the *same* symlinked tree, not excluded,
/// must remain readable — proving the fix withholds exactly the excluded
/// object and does not over-restrict the whole symlinked tree along with it.
#[test]
fn a_sibling_in_the_same_symlinked_tree_remains_readable() {
    assert_host_can_run_this();
    let fx = fixture();
    let out = run_shim_against(&fx, &fx.unresolved_sibling.clone());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "a non-excluded sibling under the same symlinked grant must remain readable — the fix \
         must not over-restrict the whole tree.\ncombined output:\n{combined}"
    );
    assert!(
        combined.contains("normal.value=fine"),
        "expected the sibling's contents in stdout.\ncombined output:\n{combined}"
    );
}
