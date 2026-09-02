//! The argv boundary every fixture is built through.
//!
//! Every child process this crate spawns is literally `git` — never a shell,
//! never a helper binary. A fixture that needed `sh -c` to build itself would
//! be a fixture whose shape depended on the machine's shell, and the whole
//! point of the catalogue is that the same bytes land on disk everywhere.
//!
//! ## Why identity is passed per invocation
//!
//! Every commit here is made with `-c user.name=… -c user.email=…` on the
//! command line, and with `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` pointed
//! at `/dev/null`. A bare `git commit` reads identity, `commit.gpgsign`, hook
//! paths and template directories from the developer's own global config — so
//! a fixture built that way is a different repository on every machine, and on
//! a box with `commit.gpgsign = true` it does not build at all.
//!
//! The builders *also* write `user.name` and `user.email` into the fixture's
//! local config. That is not redundancy for its own sake: the suites that use
//! these fixtures go on to run their own `git commit` against the repository
//! afterwards, through their own helpers, which pass no identity. Removing the
//! local config would leave those follow-up commits with no author.

use std::path::Path;
use std::process::Command;

/// Who a fixture's commits are authored by.
#[derive(Clone, Copy, Debug)]
pub struct Ident {
    /// `user.name`.
    pub name: &'static str,
    /// `user.email`.
    pub email: &'static str,
}

/// The identity the Rust suites' fixtures are authored with.
///
/// `t <t@example.invalid>` is not arbitrary — it is what all twenty of the
/// hand-rolled `seeded_repo()` implementations this catalogue replaces already
/// used, and `.invalid` is the RFC 2606 TLD guaranteed never to resolve. Two
/// suites assert on the literal string, so it is part of the contract.
pub const CATALOGUE: Ident = Ident {
    name: "t",
    email: "t@example.invalid",
};

/// The identity the browser harness's fixtures are authored with.
///
/// Kept byte-identical to what `ci/browser/fixture.mjs` used before #448 moved
/// these builders into Rust: no spec asserts on it today, but a fixture whose
/// commits change author is a fixture whose rendered history changed, and this
/// migration is supposed to change nothing a spec can see.
pub const BROWSER: Ident = Ident {
    name: "Claude_Max",
    email: "262510778+tom2025b@users.noreply.github.com",
};

/// Config overrides prepended to every `git` invocation the builders make.
///
/// Signing is forced off in both forms: a developer with `commit.gpgsign` or
/// `tag.gpgsign` set globally would otherwise be prompted for a passphrase by
/// a unit test, or simply watch it fail.
///
/// # Auto-maintenance is off, and `GIT_CONFIG_GLOBAL=/dev/null` is not enough
///
/// Every `git commit` spawns `git maintenance run --auto --quiet --detach`
/// unless told otherwise. That janitor creates an empty
/// `objects/maintenance.lock`, then unlinks it when it finishes — asynchronously,
/// after the commit that spawned it has already returned.
///
/// Emptying the global and system config does NOT prevent this: `maintenance.auto`
/// and `gc.auto` default to on in git's own compiled-in defaults, with no file to
/// blank. The setting has to arrive on the command line, which is why it lives
/// here rather than in the environment beside `GIT_CONFIG_GLOBAL`.
///
/// This is #598. Tests that photograph `.git` before and after an operation caught
/// the lock in one snapshot and not the other, and failed on a file the code under
/// test never touched:
///
/// ```text
/// left: ["- objects/maintenance.lock len=0 hash=bd60acb658c79e45"]
/// right: []
/// ```
///
/// It reproduced in CI and not locally because the window is version-dependent:
/// git 2.55 (CI) defaults manual maintenance to the *geometric* strategy, which
/// does more work and holds the lock across `daemonize()`; git 2.53 (dev box)
/// still defaults to `gc`, whose window measured **~0.7 ms** — never lost in 80
/// natural runs, and only reproducible under an `LD_PRELOAD` that widened it.
///
/// A fixture must not race a background process it never asked for.
///
/// # `-c` here is only half of it
///
/// This vector reaches the commits **this module** makes. It does not reach a
/// caller's own bare `git commit`, which the module doc above says the suites
/// deliberately perform — and such a commit spawns the janitor exactly as any
/// other does (measured with `GIT_TRACE`). So [`init_as`] additionally writes
/// `maintenance.auto` and `gc.auto` into the repository's own config, which
/// every later commit reads whoever makes it.
///
/// The first version of this fix set only the `-c` pair and described this
/// function as "the one place every fixture git invocation passes through". A
/// fresh reader found the counter-example in `seeded.rs`. Both halves are
/// needed; neither is decoration.
fn ident_args(ident: Ident) -> Vec<String> {
    let Ident { name, email } = ident;
    vec![
        "-c".into(),
        format!("user.name={name}"),
        "-c".into(),
        format!("user.email={email}"),
        "-c".into(),
        "commit.gpgsign=false".into(),
        "-c".into(),
        "tag.gpgsign=false".into(),
        // #598 — see the note above. Both, deliberately: `maintenance.auto`
        // governs the modern janitor, `gc.auto` the legacy path some
        // subcommands still consult.
        "-c".into(),
        "maintenance.auto=false".into(),
        "-c".into(),
        "gc.auto=0".into(),
    ]
}

/// Build a `git` command rooted at `repo`, with identity supplied and the
/// developer's global and system config taken out of the picture.
fn command_as(ident: Ident, repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(ident_args(ident))
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn command(repo: &Path, args: &[&str]) -> Command {
    command_as(CATALOGUE, repo, args)
}

/// [`run`] with extra `-c <key>=<value>` overrides in front of the subcommand.
///
/// Needed because some settings are only honoured when they arrive on the
/// command line. `protocol.file.allow` is the case this exists for: git
/// deliberately does **not** read it from the repository's own config when
/// deciding whether a submodule may be cloned over a `file` transport — a
/// repository could otherwise authorise its own clone — so writing it with
/// `git config` has no effect and the clone fails with `transport 'file' not
/// allowed`. Passed as `-c` it reaches the child through
/// `GIT_CONFIG_PARAMETERS`, which is what the submodule helper consults.
pub fn run_configured(repo: &Path, config: &[&str], args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.args(ident_args(CATALOGUE));
    for kv in config {
        cmd.arg("-c").arg(kv);
    }
    let status = cmd
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// Run `git <args>` in `repo` and panic if it fails.
///
/// Fixtures assert rather than return a `Result` on purpose: a builder that
/// half-succeeded would hand a test a repository in a shape nobody wrote down,
/// and the test would then fail somewhere far away from the real cause.
pub fn run(repo: &Path, args: &[&str]) {
    let status = command(repo, args)
        .status()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// Run `git <args>` in `repo` with author and committer dates pinned.
///
/// Used by the shapes whose whole purpose is to be byte-identical across two
/// independent builds: commit oids hash the timestamps, so without this two
/// repositories built from the same instructions one second apart are different
/// repositories.
pub fn run_dated(repo: &Path, args: &[&str], date: &str) {
    let status = command(repo, args)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// Run `git <args>` in `repo` and return trimmed stdout, panicking on failure.
pub fn out(repo: &Path, args: &[&str]) -> String {
    let output = command(repo, args)
        .output()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(output.status.success(), "git {args:?} failed in {repo:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Run `git <args>` in `repo` and report only whether it succeeded.
///
/// The conflict shapes need this: `git merge` on a conflicted merge is
/// *supposed* to exit non-zero, and a builder that asserted on its status
/// would refuse to build the very shape it exists to build.
pub fn try_run(repo: &Path, args: &[&str]) -> bool {
    command(repo, args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `git init -b main` plus the local identity config, in a directory that is
/// created if it does not exist.
///
/// The local config is what lets a *caller* keep committing to the fixture
/// with its own bare `git commit` after the builder has returned — see the
/// module doc.
pub fn init(repo: &Path) {
    init_as(CATALOGUE, repo);
}

/// [`init`] under an explicit identity.
pub fn init_as(ident: Ident, repo: &Path) {
    std::fs::create_dir_all(repo).expect("create fixture repo directory");
    run_as(ident, repo, &["init", "-q", "-b", "main"]);
    run_as(ident, repo, &["config", "user.email", ident.email]);
    run_as(ident, repo, &["config", "user.name", ident.name]);
    // #598, and the reason this is LOCAL config rather than only `-c`: the
    // module doc above says callers "go on to run their own `git commit`
    // against the repository afterwards, through their own helpers, which pass
    // no identity". Such a commit gets none of `ident_args`, so a `-c` override
    // does not reach it and it spawns the janitor — measured with GIT_TRACE on
    // `seeded::tests::a_caller_can_commit_again_without_supplying_an_identity`.
    // Written into the repository, it covers every later commit by anyone.
    run_as(ident, repo, &["config", "maintenance.auto", "false"]);
    run_as(ident, repo, &["config", "gc.auto", "0"]);
}

/// [`run`] under an explicit identity.
pub fn run_as(ident: Ident, repo: &Path, args: &[&str]) {
    let status = command_as(ident, repo, args)
        .status()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// [`try_run`] under an explicit identity.
pub fn try_run_as(ident: Ident, repo: &Path, args: &[&str]) -> bool {
    command_as(ident, repo, args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// [`run_dated`] under an explicit identity.
pub fn run_dated_as(ident: Ident, repo: &Path, args: &[&str], date: &str) {
    let status = command_as(ident, repo, args)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {repo:?}");
}

/// [`out`] without trimming, under an explicit identity.
///
/// For output whose leading whitespace is *data*. `git status --porcelain` is
/// the case that matters: its first column is a space when a path is changed
/// but not staged, so trimming the output shifts every column of the first
/// line one to the left and silently turns " M file" into "M file" — an
/// unstaged edit read as a staged rename.
pub fn out_exact_as(ident: Ident, repo: &Path, args: &[&str]) -> String {
    let output = command_as(ident, repo, args)
        .output()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(output.status.success(), "git {args:?} failed in {repo:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// [`out`] under an explicit identity.
pub fn out_as(ident: Ident, repo: &Path, args: &[&str]) -> String {
    let output = command_as(ident, repo, args)
        .output()
        .unwrap_or_else(|e| panic!("could not spawn git {args:?} in {repo:?}: {e}"));
    assert!(output.status.success(), "git {args:?} failed in {repo:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Write `content` to `repo/name`, creating parent directories as needed.
pub fn write(repo: &Path, name: &str, content: &[u8]) {
    let path = repo.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture file parent");
    }
    std::fs::write(&path, content).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #598 — a fixture commit must not spawn git's background janitor.
    ///
    /// This is the **behavioural** test, and it exists because the first attempt
    /// at pinning #598 was a test that could not fail. Two attempts, in fact:
    ///
    /// 1. "assert no `objects/maintenance.lock` is left behind" — inert, because
    ///    the lock's lifetime measured **~0.7 ms** on this box, so the check
    ///    almost never catches it even with maintenance fully enabled.
    /// 2. "assert `git config --get maintenance.auto` is false" — inert, because
    ///    that reads *repository* config, which never holds a `-c` value.
    ///
    /// `GIT_TRACE=1` settles it directly: git prints the spawn, or it does not.
    /// No race, no snapshot, no reliance on a lock file still being on disk.
    ///
    /// # Two mutations, both real — but not both against THIS test
    ///
    /// 1. **Removes the mechanism** — delete the `-c maintenance.auto=false`
    ///    pair from [`ident_args`]. **This test does not catch it**: `init`
    ///    already routed through [`init_as`], which wrote the same setting
    ///    into the repository's own local config, so the commit below is
    ///    still protected by that second layer. An earlier version of this
    ///    comment claimed otherwise — a fresh reader's mutation run
    ///    disproved it; the two suppressors are confounded here on purpose,
    ///    because that is what a real fixture commit looks like. What
    ///    *does* redden on this mutation is the structural companion test
    ///    below (it reads [`ident_args`] directly), and, more importantly,
    ///    [`a_commit_with_no_local_maintenance_config_is_protected_by_dash_c_alone`] —
    ///    which exists specifically because nothing here isolated `-c` from
    ///    local config until that gap was found.
    /// 2. **Weakens it** — set `maintenance.auto=true` instead of `false`.
    ///    This one DOES redden this test: it changes what [`init_as`] itself
    ///    writes, so both layers see the weaker value at once and the spawn
    ///    happens.
    ///
    /// Note what is deliberately **not** offered as a mutation: dropping
    /// `-c gc.auto=0` while keeping `maintenance.auto=false`. Measured on git
    /// 2.53, that changes nothing — `maintenance.auto=false` alone already
    /// suppresses the spawn. A mutation that cannot change behaviour is not a
    /// weakening, and an earlier version of this test claimed it as one.
    #[test]
    fn a_real_fixture_commit_does_not_spawn_auto_maintenance() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path().join("repo");
        init(&repo);

        let output = command(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "trace maintenance"],
        )
        .env("GIT_TRACE", "1")
        .output()
        .expect("run a traced fixture commit");
        assert!(
            output.status.success(),
            "the traced fixture commit failed, so the assertions below say \
             nothing about maintenance: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let trace = String::from_utf8_lossy(&output.stderr);

        // Vacuity guard FIRST here, unlike the structural test below: a run that
        // produced no trace at all would satisfy the negative assertion for
        // entirely the wrong reason, and that is the exact failure mode this
        // test was written to escape.
        assert!(
            trace.contains("built-in: git commit"),
            "GIT_TRACE produced no commit event, so the negative assertion below \
             would pass by scanning nothing. Trace was: {trace}"
        );
        assert!(
            !trace.contains("maintenance run"),
            "a fixture commit spawned git's auto-maintenance. That janitor \
             creates an empty `objects/maintenance.lock` and unlinks it \
             asynchronously, so a test photographing .git before and after an \
             operation catches it in one snapshot and not the other and fails on \
             a file it never touched. That is #598. Trace was: {trace}"
        );
    }

    /// A commit routed through this module, in a repository [`init_as`] never
    /// built — so `-c maintenance.auto=false` is the ONLY suppressor in play,
    /// not confounded with the local config `init_as` also writes.
    ///
    /// The test above shares a fixture built by [`init`], so it cannot tell
    /// `-c` and local config apart: dropping `-c` from [`ident_args`] leaves
    /// local config still holding `false`, and that test stays green (a fresh
    /// reader's mutation run proved this — see the corrected comment above).
    /// A real repository with no local override exists in this codebase
    /// today: `divergent.rs::clone_onto` builds one with a bare `git clone`,
    /// which does not copy the source repository's custom config keys, and
    /// every subsequent commit against it goes through this module's `run`/
    /// `command`, never through [`init_as`] again. This test stands in for
    /// that shape directly, rather than depending on another module's fixture
    /// staying built the way it happens to be built today.
    #[test]
    fn a_commit_with_no_local_maintenance_config_is_protected_by_dash_c_alone() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create fixture repo directory");

        // Deliberately NOT `init_as`: identity only, no maintenance override
        // written to local config. This is the one difference from the test
        // above, and it is the whole point of this test.
        let identity_only = Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "init", "-q", "-b", "main"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("run git init");
        assert!(identity_only.success(), "git init failed");
        for (key, value) in [
            ("user.name", CATALOGUE.name),
            ("user.email", CATALOGUE.email),
        ] {
            let status = Command::new("git")
                .args(["-C", repo.to_str().unwrap(), "config", key, value])
                .status()
                .expect("run git config");
            assert!(status.success(), "git config {key} failed");
        }

        // Routed through THIS module, so `ident_args`'s `-c` applies — the
        // thing this test exists to isolate.
        let output = command(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "trace maintenance"],
        )
        .env("GIT_TRACE", "1")
        .output()
        .expect("run a traced commit with no local maintenance config");
        assert!(
            output.status.success(),
            "the traced commit failed, so the assertions below say nothing \
             about maintenance: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let trace = String::from_utf8_lossy(&output.stderr);
        assert!(
            trace.contains("built-in: git commit"),
            "GIT_TRACE produced no commit event, so the check below is vacuous: \
             {trace}"
        );
        assert!(
            !trace.contains("maintenance run"),
            "a commit with no local maintenance config spawned auto-maintenance \
             even with `-c maintenance.auto=false` on the command line — the \
             override this test exists to prove is load-bearing on its own \
             failed to suppress the spawn. Trace was: {trace}"
        );
    }

    /// A caller's own bare `git commit` — no `ident_args` — must not spawn it
    /// either.
    ///
    /// This is the gap a fresh reader found in the first version of this fix.
    /// The module doc says callers "go on to run their own `git commit` against
    /// the repository afterwards, through their own helpers, which pass no
    /// identity", and `seeded.rs` has a test doing exactly that. A `-c` override
    /// reaches none of those commits, so the first fix left the race open on
    /// every one of them while claiming `ident_args` was the single choke point.
    ///
    /// [`init_as`] therefore writes the setting into the repository's own config
    /// as well. This test drives the bare path — a raw `Command`, deliberately
    /// bypassing this module — so it fails if that local config is dropped.
    #[test]
    fn a_bare_caller_commit_does_not_spawn_auto_maintenance_either() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path().join("repo");
        init(&repo);
        write(&repo, "a.txt", b"one\n");
        // A seed commit through this module, so the follow-up below has a
        // tracked file to modify. `commit -am` stages tracked paths only, and a
        // repository with no commit has none.
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        write(&repo, "a.txt", b"two\n");

        // Deliberately NOT through this module: this is the caller's own shape.
        let output = Command::new("git")
            .args(["commit", "-q", "-am", "a caller's own follow-up"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TRACE", "1")
            .output()
            .expect("run a bare follow-up commit");
        assert!(
            output.status.success(),
            "the bare follow-up commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let trace = String::from_utf8_lossy(&output.stderr);
        assert!(
            trace.contains("built-in: git commit"),
            "GIT_TRACE produced no commit event, so the check below is vacuous: \
             {trace}"
        );
        assert!(
            !trace.contains("maintenance run"),
            "a caller's bare commit spawned auto-maintenance. `ident_args` \
             cannot reach this path — the setting has to be in the repository's \
             own config, written by `init_as`. See #598. Trace was: {trace}"
        );
    }

    /// The overrides are on the argument vector — a fast structural companion to
    /// the behavioural tests above.
    ///
    /// It earns its place by being instant and by naming the mechanism in its
    /// failure message; it is **not** the proof. The traced tests above are.
    #[test]
    fn every_fixture_git_call_carries_the_maintenance_overrides() {
        let args = ident_args(CATALOGUE);

        assert!(
            args.iter().any(|a| a == "maintenance.auto=false"),
            "fixture git invocations no longer pass `-c maintenance.auto=false`. \
             See #598, and see the traced test above for what actually breaks. \
             Args were: {args:?}"
        );

        // Vacuity guard, checked last: an empty vector would otherwise fail the
        // check above with a confusing message about maintenance.
        assert!(
            args.iter().any(|a| a.starts_with("user.email=")),
            "ident_args returned something that is not a git argument vector at \
             all — fix that before trusting the check above. Got: {args:?}"
        );
    }
}
