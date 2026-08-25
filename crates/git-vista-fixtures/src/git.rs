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
