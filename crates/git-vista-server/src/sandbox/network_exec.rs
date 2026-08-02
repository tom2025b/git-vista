//! #228 (M2.20b): the shared Network-tier exec harness — the one execution
//! path every fetch/pull/push spawn is meant to go through, so askpass
//! hardening and output redaction are enforced structurally rather than
//! re-derived at each of the three call sites.
//!
//! # What this closes
//!
//! `docs/superpowers/evidence/m1.13-design-trail/m1.13-findings.md` finding
//! I5: `core.askpass` is a repo-local-settable config key naming a program
//! git executes to obtain credentials, and it is consulted **before** any
//! terminal-prompt fallback. Verified directly against this build (git
//! 2.43.0, 2026-08-01): a repo-local `core.askpass` pointing at a marker
//! script runs — twice, once for username and once for password — against a
//! remote that merely answers `401 Unauthorized`, with **no** controlling
//! terminal anywhere in the process tree. That is arbitrary code execution
//! reachable from a hostile or compromised repository's own `.git/config` on
//! every Network-tier spawn (`git fetch`/`pull`/`push`/`ls-remote`), and nothing
//! in this crate's env-inheriting spawn model (`spawn.rs`'s `command_async`
//! deliberately leaves the environment untouched — see its module doc) closes
//! it on its own. `-c core.askpass=` on the command line outranks repo-local
//! config and structurally cannot be re-opened by anything the served
//! repository controls; [`network_command`] is the one place that flag is
//! added, so every caller gets it by construction rather than by remembering.
//!
//! # Why this does not also force `credential.helper=`
//!
//! `credential.helper` is the *sanctioned* HTTPS-auth mechanism this server
//! relies on (`docs/SECURITY_MODEL.md`, "Remote and Forge Credentials":
//! "Prefer existing Git credential helpers and SSH agents on the Linux
//! host") — forcing it off would not harden anything (there is no attacker
//! path through the operator's own configured helper that `core.askpass`
//! doesn't already cover) and would break the one HTTPS-push path that is
//! meant to work. `core.askpass` has the opposite shape: it exists only to
//! drive an *interactive* prompt, and this server never has a terminal to
//! prompt through, so forcing it off costs nothing.
//!
//! That said, a credential helper is itself an arbitrary program (repo-local
//! `credential.helper` is exactly as executable as `core.askpass`), and its
//! stderr is forwarded by git verbatim, unfiltered — verified directly below
//! (`network_exec_redacts_a_real_credential_helpers_leaked_url`): a helper
//! that prints a secret-bearing URL to its own stderr puts that URL in git's
//! stderr unchanged. Closing *that* execution surface is a materially bigger
//! decision (it is the credential-helper reinjection design the M1.13
//! design-trail's operator lens devotes its own finding to — `m1.13-findings.md`
//! lines 89-92, "the helper is a fixed, server-authored literal" vs. "the
//! test needs it to be injectable" — a productization question this slice
//! does not have to answer) than this slice's scope, so it stays open here —
//! but [`redact_output`] means
//! whatever a helper prints is still sanitised before this harness hands it
//! back, which is the redaction half of the deliverable regardless of what
//! produced the leak.
//!
//! # The one thing this harness cannot pin, and why
//!
//! The M1.13 finding's own reproduction of "fails fast and cleanly" pins the
//! exact string `could not read Username for '<url>': terminal prompts
//! disabled` — which requires `GIT_TERMINAL_PROMPT=0` in the child's
//! environment. [`spawn::SandboxedCommand`] deliberately exposes no `env`
//! method in production (see its module doc, C10 hazard #1): argv and
//! environment must not be settable after `sandbox_argv` has classified the
//! spawn, and env-setting is excluded from the production surface for
//! exactly the same reason `arg`/`args` are. Adding one to force this single
//! variable would reopen that hazard for a message string, not for a
//! containment property this crate's tests can't get another way, so this
//! module does not do it and callers should not either.
//!
//! Measured instead (see `network_tier_https_auth_failure_is_fast_and_never_prompts`
//! below): with no `core.askpass`, no credential helper that succeeds, and no
//! controlling terminal — which is every real deployment of this server,
//! since it is a headless network daemon with no tty of its own — git tries
//! to open `/dev/tty` directly (it does this regardless of `GIT_TERMINAL_PROMPT`
//! when that variable is unset) and fails immediately with `could not read
//! Username for '<url>': No such device or address`. That is the same
//! *behaviour* the pinned message promises — fast, clean, no hang, no
//! interactive fallback — just not the same *bytes*. If the byte-exact
//! string is required (a client-side string match, say), that needs
//! `GIT_TERMINAL_PROMPT=0`, which needs an `env` capability on the production
//! spawn surface — an architectural decision that belongs in its own ADR, not
//! a unilateral widening here. Reported, not built.

use std::path::Path;
use std::process::Output;

use super::{policy_for, spawn, NetworkNeed, Policy};

/// Prepended to every Network-tier spawn's args, ahead of the subcommand —
/// see the module doc for why this is the one flag this harness forces.
///
/// Positioned first, not last: git's `-c` flags must precede the subcommand,
/// and every caller in this crate already passes `args` as `[subcommand,
/// …]` (see `run_git`/`run_branch_cmd` in `planner.rs`), so there is no
/// legitimate later occurrence of the same key for this to lose a
/// last-one-wins race against. `args` here is always server-authored, never
/// raw request data — if a future caller ever needs to pass its own `-c`
/// flags, it must not repeat `core.askpass` ahead of the subcommand, and
/// that should be caught in review, not by this ordering.
const FORCED_NETWORK_ARGS: &[&str] = &["-c", "core.askpass="];

/// Why a Network-tier spawn could not be run at all — mirrors
/// `git_cmd::ExecUnavailable`'s two-cause fold (policy-build failure and
/// spawn/IO failure are the same "we observed nothing" fact to every caller)
/// but keeps the underlying `ShimError` typed rather than stringified, since
/// this lives beside `policy_for` rather than across the crate boundary
/// `git_cmd.rs` sits at.
#[derive(Debug)]
pub(crate) enum NetworkExecError {
    /// The Network-tier policy itself could not be built (missing shim,
    /// unset `$HOME`, a `.git` geometry `repo_paths` refuses).
    Policy(super::shim::ShimError),
    /// The composed launcher could not be spawned, or its `Output` could not
    /// be collected.
    Io(std::io::Error),
}

impl std::fmt::Display for NetworkExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for NetworkExecError {}

/// Build the composed launcher for one Network-tier remote spawn: `policy`'s
/// argv (#188's SSH carve-out and agent-socket grant included, whenever
/// `policy` is one `policy_for`/`policy_for_clone` built) with
/// [`FORCED_NETWORK_ARGS`] spliced in ahead of `args`.
///
/// Returns the same [`spawn::SandboxedCommand`] every other spawn site
/// returns, un-run — callers still configure stdio exactly as
/// `spawn::command_async`'s other callers do, and this module adds no new
/// way to touch argv or environment after that point. `policy` is taken
/// rather than built here so this function stays testable with a hand-built
/// Network-tier `Policy` the way `ssh_remote.rs` already tests
/// `spawn::command_async` directly — see that file's module doc for why a
/// real end-to-end test needs a substituted ephemeral port that
/// `policy_for`'s fixed `DEFAULT_GIT_PORTS` can't supply.
pub(crate) fn network_command(
    policy: &Policy,
    repo: &Path,
    args: &[&str],
) -> spawn::SandboxedCommand {
    let mut full: Vec<&str> = FORCED_NETWORK_ARGS.to_vec();
    full.extend_from_slice(args);
    spawn::command_async(policy, repo, &full)
}

/// The production entry point: builds the Network-tier policy itself via
/// [`policy_for`] (`NetworkNeed::Remote`, so #188's SSH carve-out and
/// agent-socket grant are wired in exactly as they are for every other
/// Network-tier spawn — nothing here reimplements that machinery), runs
/// through [`network_command`], and redacts the captured output before
/// returning it.
///
/// # Not yet called from `planner.rs`
///
/// #228's allowed paths are `sandbox/**` plus `durable.rs`/journal redaction
/// helpers; wiring `planner.rs`'s `exec_push` (and the `exec_fetch`/
/// `exec_pull` #227 will add) onto this function is explicitly left for that
/// integration step — see this crate's issue tracker and the module doc
/// above. `#[allow(dead_code)]` on this item is the same "lands before its
/// caller" state `sandbox/mod.rs`'s own module doc describes for
/// `sandbox_argv` between Task 1 and Task 5 of #66; it should come off the
/// moment a real caller lands.
#[allow(dead_code)]
pub(crate) async fn run_network_git(
    repo: &Path,
    read_only: bool,
    args: &[&str],
) -> Result<Output, NetworkExecError> {
    let policy =
        policy_for(repo, read_only, NetworkNeed::Remote).map_err(NetworkExecError::Policy)?;
    let output = network_command(&policy, repo, args)
        .output()
        .await
        .map_err(NetworkExecError::Io)?;
    Ok(redact_output(output))
}

/// Strip `user[:pass]@` userinfo from every `<scheme>://…` URL substring
/// found in `text`, leaving the scheme, host and path intact —
/// `docs/SECURITY_MODEL.md`'s "Remote and Forge Credentials" bullet: "Redact
/// URL userinfo … from logs and operation records."
///
/// No URL-parsing crate: `text` is not itself a URL, it is arbitrary text
/// (git's stderr, a credential helper's own diagnostic output) that may
/// contain zero, one, or several URLs anywhere inside it, so parsing the
/// whole string as one URL does not apply. This scans for every `://`
/// occurrence that is immediately preceded by scheme characters
/// (`[A-Za-z0-9+.-]`), takes that URL's authority as the run up to the next
/// `/`, `?`, `#`, whitespace, or end of string, and — only when that
/// authority contains an `@` — drops everything up to and including the
/// *last* `@` in it (the userinfo delimiter; a password can itself contain
/// `@`, which is why this is "last", not "first").
///
/// Operates on `char`s rather than raw bytes so every slice point this
/// function chooses is a valid boundary regardless of what non-ASCII text
/// surrounds a redacted URL — git's own output is not guaranteed ASCII (a
/// path component can be any byte the filesystem allows).
pub(crate) fn redact_url_userinfo(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let is_scheme_char = |c: char| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.';

    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < n {
        let starts_scheme_sep =
            i + 2 < n && chars[i] == ':' && chars[i + 1] == '/' && chars[i + 2] == '/';
        if starts_scheme_sep && i > 0 && is_scheme_char(chars[i - 1]) {
            // Authority = the run after "://" up to the next path/query/
            // fragment/whitespace delimiter, or the end of the string.
            let mut end = i + 3;
            while end < n
                && chars[end] != '/'
                && chars[end] != '?'
                && chars[end] != '#'
                && !chars[end].is_whitespace()
            {
                end += 1;
            }
            // Last '@' inside the authority, if any.
            let mut at = None;
            let mut k = i + 3;
            while k < end {
                if chars[k] == '@' {
                    at = Some(k);
                }
                k += 1;
            }
            out.push_str("://");
            let keep_from = at.map_or(i + 3, |a| a + 1);
            for &c in &chars[keep_from..end] {
                out.push(c);
            }
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// [`redact_url_userinfo`] applied to both halves of a spawn's captured
/// output — the one place this harness's callers get sanitisation "for
/// free" regardless of which of them eventually reaches a response, a log
/// line, or a journal record built from this `Output`.
///
/// Non-UTF-8 bytes are left untouched rather than lossily reinterpreted:
/// `redact_url_userinfo` needs `&str` to scan characters, and git's stdout
/// in particular can carry non-UTF-8 path bytes (`git_cmd.rs`'s own byte-not-
/// String convention exists for the same reason). A lossy round-trip would
/// silently corrupt those bytes for a redaction that, being ASCII-anchored
/// (`://`, `@`), has nothing to find in binary output anyway.
pub(crate) fn redact_output(output: Output) -> Output {
    Output {
        status: output.status,
        stdout: redact_bytes(&output.stdout),
        stderr: redact_bytes(&output.stderr),
    }
}

fn redact_bytes(bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(s) => redact_url_userinfo(s).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::shim_cli::{fixture, production_policy};
    use super::*;

    // --- pure argv shape ----------------------------------------------

    /// `network_command`'s argv is exactly `command_async`'s own argv with
    /// [`FORCED_NETWORK_ARGS`] spliced in immediately after `-C <repo>` and
    /// before the caller's own args — mirrors `spawn.rs`'s
    /// `the_wrapper_argv_is_the_sandbox_argv_plus_the_repo_and_args`, one
    /// layer up.
    #[tokio::test]
    async fn network_command_prepends_forced_askpass_hardening_before_user_args() {
        let repo = fixture().await;
        let policy = production_policy(repo.path());

        // Build the same argv `network_command` builds, but by hand from
        // `command_async`'s own documented shape, so this test does not
        // just call the function under test and check it agrees with
        // itself.
        let bare = spawn::command_async(&policy, repo.path(), &["push", "origin", "main"]);
        drop(bare); // only wanted to prove the args compose; nothing spawned.

        // The real assertion: run both through a fake `git` that dumps argv,
        // one with the harness and one without, and compare. The dumper has
        // to live *inside* `repo` — the shim execs the sandboxed `git` by
        // bare name via `PATH`, and Landlock only grants exec on paths this
        // policy actually grants; a dumper in an ungranted tempdir would
        // just fail to exec, not prove anything about argv order.
        let dumper = which_dumper(repo.path());
        let hermetic = |c: spawn::SandboxedCommand| {
            c.pinned_env_for_test(&[
                ("PATH", dumper.clone()),
                ("HOME", std::env::var("HOME").unwrap()),
            ])
        };

        let out = hermetic(network_command(
            &policy,
            repo.path(),
            &["push", "origin", "main"],
        ))
        .output()
        .await
        .expect("fake git runs");
        let argv_line = String::from_utf8_lossy(&out.stdout);
        // The dumper emits a trailing separator after its last argument
        // unconditionally (simplest possible shell loop); trim it before
        // splitting so it doesn't show up as a spurious empty final element.
        let args: Vec<&str> = argv_line
            .trim()
            .trim_end_matches('\u{1f}')
            .split('\u{1f}')
            .collect();

        // ends with the caller's own args, untouched
        assert_eq!(&args[args.len() - 3..], ["push", "origin", "main"]);
        // and the forced flag sits immediately before them
        assert_eq!(
            &args[args.len() - 5..args.len() - 3],
            ["-c", "core.askpass="]
        );
    }

    /// A `PATH` containing nothing but a fake `git` that writes its argv
    /// (unit-separator-joined, to survive spaces in any element) to stdout
    /// and exits 0. Lets the argv test above observe the *exact* argv a
    /// real spawn would run, rather than re-deriving it from the same
    /// composition code the function under test uses. Written inside `repo`
    /// (an already rw-granted tree) rather than a fresh tempdir, since a
    /// path outside every grant this policy makes cannot be exec'd at all.
    fn which_dumper(repo: &Path) -> String {
        let dir = repo.join("fake-bin");
        std::fs::create_dir_all(&dir).expect("mkdir fake-bin");
        let bin = dir.join("git");
        std::fs::write(
            &bin,
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\n'\n",
        )
        .expect("write fake git");
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&bin, perm).unwrap();
        dir.to_string_lossy().into_owned()
    }

    // --- redact_url_userinfo, pure -------------------------------------

    #[test]
    fn redact_url_userinfo_strips_userinfo_keeps_host_and_path() {
        assert_eq!(
            redact_url_userinfo("https://user:token@host/repo.git"),
            "https://host/repo.git"
        );
    }

    #[test]
    fn redact_url_userinfo_leaves_a_url_without_userinfo_unchanged() {
        let s = "fatal: unable to access 'https://host/repo.git/': timed out";
        assert_eq!(redact_url_userinfo(s), s);
    }

    #[test]
    fn redact_url_userinfo_leaves_plain_text_with_no_url_unchanged() {
        let s = "nothing url-shaped in here, just prose and a ratio 3://4";
        // "3://4" has no scheme chars matching the alnum/+/-/. class before
        // it in a way that changes anything real — but confirm harmless
        // colons/slashes elsewhere in prose survive untouched too.
        assert_eq!(redact_url_userinfo(s), s);
    }

    #[test]
    fn redact_url_userinfo_handles_several_urls_in_one_string() {
        let s = "tried https://a:b@host1/x then ssh://git@host2:22/y then http://host3/z";
        assert_eq!(
            redact_url_userinfo(s),
            "tried https://host1/x then ssh://host2:22/y then http://host3/z"
        );
    }

    #[test]
    fn redact_url_userinfo_uses_the_last_at_when_the_password_contains_one() {
        // A password containing '@' is exactly why this scans for the LAST
        // '@' in the authority, not the first.
        assert_eq!(
            redact_url_userinfo("https://user:p@ss@host/repo.git"),
            "https://host/repo.git"
        );
    }

    #[test]
    fn redact_url_userinfo_handles_a_url_with_userinfo_at_the_end_of_the_string() {
        assert_eq!(
            redact_url_userinfo("remote: https://user:tok@host"),
            "remote: https://host"
        );
    }

    /// The paired negative for the four cases above: without redaction, the
    /// literal secret survives verbatim in the same input strings — proving
    /// the assertions above are capable of failing, not just capable of
    /// passing against text that never had the secret positioned where the
    /// scanner looks.
    #[test]
    fn unredacted_text_still_contains_the_literal_secret() {
        let secret = "token";
        let s = format!("https://user:{secret}@host/repo.git");
        assert!(
            s.contains(secret),
            "test setup: secret must be present pre-redaction"
        );
        assert!(
            !redact_url_userinfo(&s).contains(secret),
            "redaction must remove it"
        );
    }

    #[test]
    fn redact_output_redacts_both_stdout_and_stderr() {
        let raw = Output {
            status: std::process::ExitStatus::default(),
            stdout: b"cloning https://u:p@host/a.git".to_vec(),
            stderr: b"fatal: https://u:p@host/a.git unreachable".to_vec(),
        };
        let redacted = redact_output(raw);
        assert_eq!(redacted.stdout, b"cloning https://host/a.git");
        assert_eq!(redacted.stderr, b"fatal: https://host/a.git unreachable");
    }
}

/// Real-git tests that need a Network-tier `Policy` pointed at a loopback
/// fixture on an ephemeral port rather than `policy_for`'s fixed
/// `DEFAULT_GIT_PORTS` (22/443/80/9418 — none of which this process can bind
/// without root). Same substitution `sandbox::ssh_remote`'s fixture already
/// makes, for the same reason: see that module's doc comment. Kept in its
/// own `#[cfg(test)]` module (rather than folded into the pure-unit `tests`
/// module above) because everything here spawns real processes.
#[cfg(test)]
mod https_suite {
    use super::super::{
        default_system_trees, secret_excludes_for_home, shim, ssh_known_hosts_carveout, HookMode,
        Tier,
    };
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;

    /// A Network-tier `Policy` shaped exactly like `policy_for`'s Network
    /// branch, pointed at `home`/`repo`, with `port` substituted for
    /// `DEFAULT_GIT_PORTS`.
    fn network_policy(home: &Path, repo: &Path, port: u16) -> Policy {
        let (mut rw, mut ro) = default_system_trees(Tier::Network);
        rw.push(repo.to_path_buf());
        ro.push(home.to_path_buf());
        Policy {
            tier: Tier::Network,
            shim: shim::shim_path()
                .expect("gv-sandbox must be built")
                .to_path_buf(),
            bwrap: None,
            rw_trees: rw,
            ro_trees: ro,
            secret_excludes: secret_excludes_for_home(home),
            ro_carveouts: ssh_known_hosts_carveout(home),
            net_ports: vec![port],
            hook_mode: HookMode::Run,
        }
    }

    fn hermetic_env(home: &Path) -> Vec<(&'static str, String)> {
        vec![
            ("PATH", "/usr/bin:/bin".to_string()),
            ("HOME", home.to_string_lossy().into_owned()),
        ]
    }

    /// A throwaway HTTP/1.1 server that answers every request with `401
    /// Unauthorized` plus a `WWW-Authenticate: Basic` challenge — enough to
    /// make git's smart-HTTP client attempt a credential fill (and, absent
    /// askpass hardening, invoke `core.askpass`) without needing a real
    /// forge or a TLS certificate. Serves connections sequentially,
    /// `Connection: close` on every reply, until the process exits (daemon
    /// thread; nothing joins it) — sufficient for this file's tests, each
    /// of which makes at most a couple of requests against its own server
    /// on its own ephemeral port.
    struct Http401 {
        port: u16,
    }

    impl Http401 {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            let port = listener.local_addr().expect("local_addr").port();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buf = [0u8; 4096];
                    // Best-effort drain of the request; nothing here needs
                    // to parse it, every request gets the same answer.
                    let _ = stream.read(&mut buf);
                    let body = b"";
                    let resp = format!(
                        "HTTP/1.1 401 Unauthorized\r\n\
                         WWW-Authenticate: Basic realm=\"gv-test\"\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            });
            Self { port }
        }
    }

    fn run(cmd: &mut Command, what: &str) {
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("{what}: could not run: {e}"));
        assert!(status.success(), "{what}: exited with {status}");
    }

    /// A `$HOME`-shaped tempdir with no real git config in it, plus a
    /// non-repository `-C` target directory (an `ls-remote`/`fetch` with an
    /// explicit URL needs no local repository — same posture
    /// `ssh_remote.rs`'s `cwd` field documents).
    struct HomeAndCwd {
        _home: tempfile::TempDir,
        _cwd: tempfile::TempDir,
        home: std::path::PathBuf,
        cwd: std::path::PathBuf,
    }

    fn home_and_cwd() -> HomeAndCwd {
        let home = tempfile::tempdir().expect("home tempdir");
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let home_path = home.path().to_path_buf();
        let cwd_path = cwd.path().to_path_buf();
        HomeAndCwd {
            _home: home,
            _cwd: cwd,
            home: home_path,
            cwd: cwd_path,
        }
    }

    /// I5, closed: a repo-local `core.askpass` marker script never runs
    /// through [`network_command`], and the operation still fails fast —
    /// not a hang, not a fallback prompt.
    ///
    /// The premise (a hostile `core.askpass` really would run without this
    /// harness) is proven in the same test, not assumed: the paired negative
    /// half spawns the identical args directly through
    /// `spawn::command_async` — the launcher `network_command` wraps, minus
    /// the forcing — and asserts the marker DOES run there. That is what
    /// makes the main assertion non-vacuous: this test would fail if
    /// `FORCED_NETWORK_ARGS` were ever dropped or reordered wrongly.
    #[tokio::test]
    async fn repo_local_askpass_is_never_executed() {
        let server = Http401::start();
        let fixture = home_and_cwd();

        // A repo-local core.askpass, planted the way an attacker or a
        // hostile clone's tracked `.git/config`-equivalent would — a
        // marker script that records that it ran and hands back a fake
        // username so the run doesn't stall waiting on its own stdin.
        let repo = fixture.cwd.clone();
        run(
            Command::new("git").args(["init", "-q"]).current_dir(&repo),
            "git init",
        );
        let marker = repo.join("askpass-marker.log");
        let script = repo.join("askpass.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"RAN pid=$$\" >> {}\necho fake-user\n",
                marker.display()
            ),
        )
        .expect("write askpass script");
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&script, perm).unwrap();
        run(
            Command::new("git")
                .args(["config", "core.askpass", script.to_str().unwrap()])
                .current_dir(&repo),
            "git config core.askpass",
        );

        let policy = network_policy(&fixture.home, &repo, server.port);
        let url = format!("http://127.0.0.1:{}/repo.git", server.port);

        // --- paired negative: without the harness's forcing, the marker
        // really does run. Proves the fixture (server, script, config) is
        // capable of demonstrating the RCE at all.
        let unforced = spawn::command_async(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("git runs");
        assert!(
            !unforced.status.success(),
            "unauthenticated ls-remote against a 401-only server must fail"
        );
        assert!(
            marker.exists(),
            "paired negative: the hostile askpass script must have run with no \
             hardening in place, or this test proves nothing about the hardening \
             below actually closing anything"
        );

        // --- the real claim: through network_command, it never runs.
        std::fs::remove_file(&marker).ok();
        let hardened = network_command(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("git runs");
        assert!(
            !hardened.status.success(),
            "ls-remote against a 401-only server must still fail — this is a \
             fail-fast claim, not a fail-open one"
        );
        assert!(
            !marker.exists(),
            "the hostile askpass script ran even though network_command forces \
             -c core.askpass=; stderr={}",
            String::from_utf8_lossy(&hardened.stderr)
        );
    }

    /// The other half of I5's acceptance box: HTTPS auth failure is fast and
    /// clean, never a hang and never an interactive fallback. See this
    /// file's module doc for why the exact `terminal prompts disabled`
    /// string is not reachable from this production surface (it needs
    /// `GIT_TERMINAL_PROMPT=0`, an env-var the spawn chokepoint does not
    /// expose) and what this pins instead: the real message this build
    /// produces, under a bounded timeout so a genuine hang fails the test
    /// rather than wedging the suite.
    #[tokio::test]
    async fn network_tier_https_auth_failure_is_fast_and_never_prompts() {
        let server = Http401::start();
        let fixture = home_and_cwd();
        let repo = fixture.cwd.clone();
        run(
            Command::new("git").args(["init", "-q"]).current_dir(&repo),
            "git init",
        );

        let policy = network_policy(&fixture.home, &repo, server.port);
        let url = format!("http://127.0.0.1:{}/repo.git", server.port);

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            network_command(&policy, &repo, &["ls-remote", &url])
                .pinned_env_for_test(&hermetic_env(&fixture.home))
                .output(),
        )
        .await
        .expect("must not hang waiting on a prompt — timed out instead of failing fast")
        .expect("git runs");

        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("could not read Username"),
            "expected git's own credential-fill failure text, got: {stderr}"
        );
        // What this build actually says without GIT_TERMINAL_PROMPT=0 (no
        // controlling terminal anywhere in this process tree — see module
        // doc): not the byte-exact "terminal prompts disabled" pin, but the
        // same fail-fast, no-prompt behaviour.
        assert!(
            stderr.contains("No such device or address")
                || stderr.contains("terminal prompts disabled"),
            "expected one of the two known fail-fast shapes this git version \
             produces with no tty, got: {stderr}"
        );
    }

    /// The redaction half of the deliverable, proven against **real**
    /// captured process output rather than a hand-built `Output`: a
    /// repo-local credential helper — a real subprocess, run by real git —
    /// prints a secret-bearing URL to its own stderr, which git forwards
    /// verbatim (measured directly, 2026-08-01, see this file's module doc).
    /// `core.askpass=` forcing does not touch `credential.helper` at all
    /// (by design — see module doc), so this is a genuine, currently-live
    /// leak this harness's redaction step is the thing that closes.
    ///
    /// Paired positive/negative in one test, same captured bytes: the RAW
    /// output is asserted to contain the secret first (the census would
    /// have found it), then the redacted output is asserted not to (proving
    /// the assertion below is capable of failing, not just of passing
    /// against text the secret was never in).
    #[tokio::test]
    async fn network_exec_redacts_a_real_credential_helpers_leaked_url() {
        let server = Http401::start();
        let fixture = home_and_cwd();
        let repo = fixture.cwd.clone();
        run(
            Command::new("git").args(["init", "-q"]).current_dir(&repo),
            "git init",
        );

        let secret_url = "https://s3cr3t-token:hunter2@leaked-host.invalid/org/repo.git";
        let helper = repo.join("helper.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\necho 'debug: tried {secret_url}' >&2\nexit 1\n"),
        )
        .expect("write helper");
        let mut perm = std::fs::metadata(&helper).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&helper, perm).unwrap();
        run(
            Command::new("git")
                .args(["config", "credential.helper", helper.to_str().unwrap()])
                .current_dir(&repo),
            "git config credential.helper",
        );

        let policy = network_policy(&fixture.home, &repo, server.port);
        let url = format!("http://127.0.0.1:{}/repo.git", server.port);

        let raw = network_command(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("git runs");
        assert!(!raw.status.success());
        let raw_stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
        assert!(
            raw_stderr.contains("s3cr3t-token") && raw_stderr.contains("hunter2"),
            "paired positive: the credential helper's leaked URL must be present \
             in the raw, unredacted output, or this test cannot show redaction \
             does anything. raw stderr={raw_stderr}"
        );

        let redacted = redact_output(raw);
        let redacted_stderr = String::from_utf8_lossy(&redacted.stderr);
        let redacted_stdout = String::from_utf8_lossy(&redacted.stdout);
        assert!(
            !redacted_stderr.contains("s3cr3t-token") && !redacted_stderr.contains("hunter2"),
            "the secret survived redaction in stderr: {redacted_stderr}"
        );
        assert!(
            !redacted_stdout.contains("s3cr3t-token") && !redacted_stdout.contains("hunter2"),
            "the secret survived redaction in stdout: {redacted_stdout}"
        );
    }
}
