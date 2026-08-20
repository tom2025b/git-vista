//! The `gv-sandbox` shim execs and never forks. Split out of `argv_boundary.rs`
//! because it proves a property of one specific binary (`src/bin/gv-sandbox/main.rs`),
//! not the crate-wide spawn census the parent module's allowlist covers.
//!
//! **This file is scanned too, and is not exempt.** The parent's spawn-site
//! scan (`every_process_spawn_site_is_allowlisted_and_spawns_only_git`) walks
//! every `.rs` file under `src/`, including this one, and its by-name
//! exemption from the literal-`git` check names only `src/argv_boundary.rs` —
//! not this path. This file already assembles the needle it looks for at
//! runtime (`Command` + `::new(`) rather than spelling it out, the same
//! discipline `argv_boundary.rs` and `sandbox/compat.rs` apply to their own
//! source; keep doing that in any comment added here too, or a prose mention
//! reads as a new, unreviewed spawn site.

use std::path::Path;

use super::code_only;

/// The shim `exec`s and **never forks**.
///
/// `gv-sandbox`'s module doc has asserted this in prose since it was written —
/// "this file must contain `.exec()` and must not contain `.spawn()`,
/// `.output()` or `.status()`" — and named this file as the place that proves
/// it. Nothing did. That prose was the entire guarantee, which is the shape of
/// claim this milestone has been burned by five times.
///
/// It matters because the shim's containment is *inherited through the exec*.
/// Landlock and seccomp are applied to the shim's own process and survive
/// `execve`; they would equally be inherited by a forked child, but a fork
/// gives the shim a second life — it stays resident as a parent, with an
/// argv it has already validated, in a process that could then exec something
/// else. `execve` is what makes the validation final: after it, there is no
/// gv-sandbox process left to run anything, only git wearing its restrictions.
///
/// Scanned on [`code_only`] output rather than raw source, so the module doc's
/// own mention of `.spawn()` is not counted as a use of it — the mistake that
/// makes a prose-driven scan report the file it is quoting.
#[test]
fn the_shim_execs_and_never_forks() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/gv-sandbox/main.rs");
    let src = std::fs::read_to_string(&path).expect("readable gv-sandbox main.rs");
    let code = code_only(&src);

    let spawn = ["Command", "::new("].concat();
    let spawn_git = ["Command", "::new(\"git\")"].concat();

    // Exactly one command is built, and it names `git` literally. The literal
    // is checked against the raw source because `code_only` blanks the contents
    // of string literals — the count is checked against code so a comment
    // quoting the pattern cannot inflate it.
    assert_eq!(
        code.matches(&spawn).count(),
        1,
        "the shim must construct exactly one Command; a second one is a second \
         thing it could exec"
    );
    assert_eq!(
        src.matches(&spawn_git).count(),
        1,
        "the shim's one Command must name `git` literally"
    );

    // It replaces its own image.
    let exec = [".exec", "()"].concat();
    assert_eq!(
        code.matches(&exec).count(),
        1,
        "the shim must `{exec}` exactly once — that call is what makes the \
         validated argv final"
    );
    assert!(
        code.contains("use std::os::unix::process::CommandExt"),
        "`{exec}` comes from `CommandExt`; if that import is gone, the call \
         above is not the exec this test thinks it is"
    );

    // It never becomes a parent.
    for (needle, why) in [
        (
            [".spawn", "()"].concat(),
            "forks a child and leaves the shim resident as its parent",
        ),
        (
            [".output", "()"].concat(),
            "forks a child and waits on it; the shim never waits on anything",
        ),
        (
            [".status", "()"].concat(),
            "forks a child and waits on it; the shim never waits on anything",
        ),
        (
            ["fork", "("].concat(),
            "duplicates the shim process outright",
        ),
        (
            ["daemon", "("].concat(),
            "forks and detaches — the shim must not outlive its exec",
        ),
    ] {
        assert_eq!(
            code.matches(needle.as_str()).count(),
            0,
            "gv-sandbox/main.rs: `{needle}` {why}. The shim applies Landlock and \
             seccomp to *itself* and then becomes git; anything that keeps it \
             alive as a parent keeps a validated-argv process around to exec \
             again."
        );
    }
}
