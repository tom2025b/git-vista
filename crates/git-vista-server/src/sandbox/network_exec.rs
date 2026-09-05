//! #228 (M2.20b): the shared Network-tier exec harness — the one execution
//! path every fetch/pull/push spawn is meant to go through, so askpass
//! hardening and output redaction are enforced structurally rather than
//! re-derived at each of the three call sites.
//!
//! [`network_command`] is wired into production at `git_cmd.rs`'s single
//! spawn chokepoint (`sandboxed`): any call declaring
//! [`NetworkNeed::Remote`] gets this module's forced askpass hardening on
//! the way in and [`redact_output`]'s redaction on the way out, rather than
//! this module owning a second, parallel policy-build-and-spawn path of its
//! own. `exec_push` (`planner.rs`) is the one production caller today —
//! wiring `exec_fetch`/`exec_pull` on, once #227 adds them, is "declare
//! `NetworkNeed::Remote`", not "remember to call this module" — the
//! chokepoint they already go through is what enforces it.
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
//! **Post-M13.01 update:** the above is still why this module never forces
//! `credential.helper=` *off*. [`network_command_with_credential`] does the
//! opposite — it *appends* Git-Vista's own helper, never clearing whatever
//! the operator's config already declares — for the narrow case #582
//! measured: the operator's own helper executes under this sandbox but
//! cannot reach its token store, so a token Git-Vista holds itself has
//! nowhere to go without one. See that function's doc for the design.
//!
//! # The one thing this harness could not pin, before #582
//!
//! The M1.13 finding's own reproduction of "fails fast and cleanly" pins the
//! exact string `could not read Username for '<url>': terminal prompts
//! disabled` — which requires `GIT_TERMINAL_PROMPT=0` in the child's
//! environment. Before M13.01, [`spawn::SandboxedCommand`] exposed no `env`
//! method in production at all (see its module doc, C10 hazard #1), and
//! adding one to force this single variable was judged not worth reopening
//! that hazard for a message string alone, so it stayed unpinned.
//!
//! #582 has since added exactly one such exception —
//! [`spawn::SandboxedCommand::credential_env`], narrow by construction (see
//! its doc for why it does not carry the hazard the exclusion was about) —
//! but it sets [`spawn::CREDENTIAL_TOKEN_VAR`] specifically, not
//! `GIT_TERMINAL_PROMPT`. Widening it to a second variable for a message
//! string alone would still be a scope decision on its own, so this
//! byte-exact string remains unpinned; the *behavioural* measurement below
//! is what stands.
//!
//! Measured instead (see `network_tier_https_auth_failure_is_fast_and_never_prompts`
//! below): with no `core.askpass`, no credential helper that succeeds, and no
//! controlling terminal — which is every real deployment of this server,
//! since it is a headless network daemon with no tty of its own — git tries
//! to open `/dev/tty` directly (it does this regardless of `GIT_TERMINAL_PROMPT`
//! when that variable is unset) and fails immediately with `could not read
//! Username for '<url>': No such device or address`. That is the same
//! *behaviour* the pinned message promises — fast, clean, no hang, no
//! interactive fallback — just not the same *bytes*.

use std::path::Path;
use std::process::Output;

use super::{spawn, Policy};

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

/// The literal appended as `-c credential.helper=<this>` when
/// [`network_command_with_credential`] is given a token. Contains
/// [`spawn::CREDENTIAL_TOKEN_VAR`]'s **name**, never a value — the value
/// lives only in the child's environment, set separately via
/// [`spawn::SandboxedCommand::credential_env`]. This is the whole of #582's
/// "argv carries the variable's name, never its value."
///
/// `printf`, not `echo`: `echo`'s handling of `-n` and backslash escapes is
/// shell-dependent, `printf`'s is not. The `[ "$1" = get ]` guard matters
/// because git also invokes a configured helper with `store` and `erase` (to
/// report outcome) — without the guard those calls would print a stray
/// `password=` line to a channel nothing is reading it from, on every
/// successful or failed auth, not just when a credential is actually wanted.
fn credential_helper_config() -> String {
    format!(
        "!f() {{ [ \"$1\" = get ] && printf 'username=x-access-token\\npassword=%s\\n' \"${}\"; }}; f",
        spawn::CREDENTIAL_TOKEN_VAR
    )
}

/// [`network_command`], plus Git-Vista's own credential helper when `token`
/// is `Some` (M13.01, #582) — the mechanism the module doc's final section
/// named as "an architectural decision that belongs in its own ADR" (ADR
/// 0122, #587) rather than a unilateral widening of [`spawn::SandboxedCommand`].
///
/// # The measurement this answers
///
/// `sandbox::clone_live::a_private_https_fetch_completes_through_the_production_clone_policy`
/// found that the sanctioned path — an operator's own configured
/// `credential.helper`, reached via the config parity every Network-tier
/// spawn already has — **executes** under this sandbox but cannot read its
/// own token store (`gh`'s config, `~/.git-credentials`, a keyring socket):
/// none of those are under a grant this sandbox gives out, and widening one
/// in per-helper, open-ended, and closes nothing this project has not just
/// finished hardening. Supplying Git-Vista's own token to Git-Vista's own
/// helper needs no such grant: the helper this function forces reads
/// exactly one environment variable and touches no filesystem at all.
///
/// # Never clears, only appends
///
/// The forced `-c credential.helper=` is **not** the empty-value form
/// [`FORCED_NETWORK_ARGS`] uses for `core.askpass` (which *disables* a
/// config-level entry) — a non-empty `credential.helper` value is added to
/// whatever chain the operator's own config already declares. Git tries
/// configured helpers in the order they are defined, config-file entries
/// before `-c` overrides, and moves to the next helper whenever one answers
/// nothing for `get` — so this one runs *last*, as the fallback for exactly
/// the case an operator's own helper cannot handle under this sandbox, and
/// never shadows a host credential path that happens to work.
///
/// # `token: None` changes nothing
///
/// When `token` is `None` this is byte-identical to [`network_command`] —
/// no `-c credential.helper=`, no environment variable set — so every
/// existing Remote-tier caller (`exec_push`, and everywhere else this
/// crate's `git_cmd::sandboxed()` routes `NetworkNeed::Remote`) is
/// unaffected until it deliberately opts in.
pub(crate) fn network_command_with_credential(
    policy: &Policy,
    repo: &Path,
    args: &[&str],
    token: Option<&str>,
) -> spawn::SandboxedCommand {
    let Some(token) = token else {
        return network_command(policy, repo, args);
    };
    let helper_config = format!("credential.helper={}", credential_helper_config());
    let mut full: Vec<&str> = FORCED_NETWORK_ARGS.to_vec();
    full.push("-c");
    full.push(&helper_config);
    full.extend_from_slice(args);
    spawn::command_async(policy, repo, &full).credential_env(token)
}

/// Strip `user[:pass]@` userinfo from every `<scheme>://…` URL substring
/// found in `bytes`, leaving the scheme, host and path intact —
/// `docs/SECURITY_MODEL.md`'s "Remote and Forge Credentials" bullet: "Redact
/// URL userinfo … from logs and operation records."
///
/// No URL-parsing crate: `bytes` is not itself a URL, it is arbitrary bytes
/// (git's stderr, a credential helper's own diagnostic output) that may
/// contain zero, one, or several URLs anywhere inside it, so parsing the
/// whole buffer as one URL does not apply. This scans for every `://`
/// occurrence that is immediately preceded by scheme characters
/// (`[A-Za-z0-9+.-]`), takes that URL's authority as the run up to the next
/// `/`, `?`, `#`, ASCII whitespace, or end of buffer — treating an *embedded*
/// `://` found during that scan as still part of the authority rather than a
/// second delimiter (see the inner comment below: a password containing
/// `://` must not be able to truncate the scan before the real userinfo
/// delimiter) — and, only when that authority contains an `@`, drops
/// everything up to and including the *last* `@` in it (the userinfo
/// delimiter; a password can itself contain `@`, which is why this is
/// "last", not "first").
///
/// Operates on raw bytes, not `char`s or `&str`: every delimiter this
/// function looks for (`:`, `/`, `?`, `#`, `@`, ASCII whitespace, and the
/// scheme-char class) is a single ASCII byte, and a UTF-8 continuation or
/// lead byte for any multi-byte code point is always `>= 0x80` — it can
/// never equal an ASCII byte value. So this never needs to know whether
/// `bytes` is valid UTF-8 at all: it cannot misread a multi-byte sequence as
/// one of these delimiters, and every position it slices at sits immediately
/// after a single-byte ASCII delimiter (always its own whole code point,
/// never a continuation byte) or at a buffer boundary — both of which are
/// valid UTF-8 char boundaries whenever the surrounding bytes are valid
/// UTF-8. This is what lets [`redact_bytes`] redact a buffer that carries
/// one stray non-UTF-8 byte (git's stdout is not guaranteed valid UTF-8 — a
/// path component can be any byte the filesystem allows) without falling
/// back to leaving the *entire* buffer unredacted just because one byte in
/// it doesn't decode: the invalid byte is simply never matched as a
/// delimiter, and passes through unchanged like any other non-ASCII byte.
fn redact_url_userinfo_bytes(bytes: &[u8]) -> Vec<u8> {
    let n = bytes.len();
    let is_scheme_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.';
    let is_authority_delim = |b: u8| b == b'/' || b == b'?' || b == b'#' || b.is_ascii_whitespace();

    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let starts_scheme_sep =
            i + 2 < n && bytes[i] == b':' && bytes[i + 1] == b'/' && bytes[i + 2] == b'/';
        if starts_scheme_sep && i > 0 && is_scheme_byte(bytes[i - 1]) {
            // Authority = the run after "://" up to the next path/query/
            // fragment/whitespace delimiter, or the end of the buffer — with
            // one carve-out: a `/` that is itself the first half of another
            // "://"-shaped run (checked by looking one byte back for `:` and
            // one byte forward for `/`) is treated as still-inside-the-
            // authority rather than the terminator. Without this, a
            // credential value that happens to contain the literal text
            // "://" (a plausible crafted/reused token) truncates the scan
            // before the real userinfo `@`, and the whole URL — credential
            // included — passes through unredacted. Skipping both bytes of
            // the embedded separator when this fires keeps the scan moving
            // forward rather than looping on the same position.
            let mut end = i + 3;
            loop {
                if end >= n {
                    break;
                }
                let b = bytes[end];
                if b == b'/' && end + 1 < n && bytes[end + 1] == b'/' && bytes[end - 1] == b':' {
                    end += 2;
                    continue;
                }
                if is_authority_delim(b) {
                    break;
                }
                end += 1;
            }
            // Last '@' inside the authority, if any.
            let mut at = None;
            let mut k = i + 3;
            while k < end {
                if bytes[k] == b'@' {
                    at = Some(k);
                }
                k += 1;
            }
            out.extend_from_slice(b"://");
            let keep_from = at.map_or(i + 3, |a| a + 1);
            out.extend_from_slice(&bytes[keep_from..end]);
            i = end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// [`redact_url_userinfo_bytes`] over a `&str`, for callers (and this file's
/// own pure unit tests) that already have text rather than raw process
/// output.
///
/// The round-trip through `String::from_utf8` cannot fail for `str` input:
/// see [`redact_url_userinfo_bytes`]'s doc for why every slice boundary it
/// chooses is a valid UTF-8 char boundary whenever the input bytes are.
pub(crate) fn redact_url_userinfo(text: &str) -> String {
    String::from_utf8(redact_url_userinfo_bytes(text.as_bytes()))
        .expect("redact_url_userinfo_bytes preserves UTF-8 validity for str input")
}

/// [`redact_url_userinfo_bytes`] applied to both halves of a spawn's
/// captured output — the one place this harness's callers get sanitisation
/// "for free" regardless of which of them eventually reaches a response, a
/// log line, or a journal record built from this `Output`.
///
/// Works directly on the raw bytes, with no UTF-8 validity check or
/// fallback: git's stdout in particular can carry non-UTF-8 path bytes
/// (`git_cmd.rs`'s own byte-not-`String` convention exists for the same
/// reason), and this redaction is ASCII-anchored (`://`, `@`) so it has
/// nothing to find *in* a non-UTF-8 byte and nothing to corrupt by leaving
/// it untouched — see [`redact_url_userinfo_bytes`]'s doc. Earlier revisions
/// of this function validated the whole buffer as UTF-8 first and skipped
/// redaction entirely on any decode failure; that meant a single stray
/// non-ASCII byte anywhere in a buffer — trivially producible by a hostile
/// credential helper — suppressed redaction of an otherwise-plain-ASCII
/// secret URL elsewhere in the *same* buffer. Operating on bytes directly
/// removes the whole-buffer-or-nothing failure mode: every byte is either
/// part of a matched delimiter or copied through untouched, independent of
/// what else is in the buffer.
pub(crate) fn redact_output(output: Output) -> Output {
    Output {
        status: output.status,
        stdout: redact_bytes(&output.stdout),
        stderr: redact_bytes(&output.stderr),
    }
}

fn redact_bytes(bytes: &[u8]) -> Vec<u8> {
    redact_url_userinfo_bytes(bytes)
}

/// A redacted view of the argv a Network-tier spawn ran with, for callers
/// that want to log or journal "ran: git <args…>" style diagnostics.
///
/// [`redact_output`] only ever sees a spawn's captured stdout/stderr —
/// `run_network_git`'s own `args: &[&str]` parameter is a second sink for
/// exactly the same secret shape, since every SSH test in this file (and
/// every real caller) routinely passes the remote URL as one of `args`
/// (`&["push", &fixture.repo_url, …]`). Nothing upstream can redact args on
/// this module's behalf — a caller that logs `args` directly bypasses
/// [`redact_output`] entirely — so this gives that caller the same
/// [`redact_url_userinfo`] treatment as a first-class, explicit primitive
/// rather than leaving args logging to rediscover (or forget) the need for
/// it independently.
#[allow(dead_code)] // no caller yet — see this file's module doc; wired in
                    // once a diagnostic/journal path logs the argv.
pub(crate) fn redact_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| redact_url_userinfo(a)).collect()
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

    // --- network_command_with_credential (M13.01, #582) -----------------

    /// A `PATH` containing nothing but a fake `git` that writes its own argv
    /// (as [`which_dumper`] does) **and then also dumps `/proc/self/cmdline`**
    /// (NUL-joined, so a value containing spaces or the record separator
    /// survives) and the raw value of `GIT_VISTA_CREDENTIAL_TOKEN` from its
    /// own environment — each on its own line, in a fixed order, so a test
    /// can assert on all three views of "what this process actually saw"
    /// from one spawn: the argv this crate composed, the argv the *kernel*
    /// recorded for the process (`/proc/self/cmdline` is what #582's
    /// acceptance criterion names explicitly — the OS-level view, not this
    /// crate's own bookkeeping of what it intended to pass), and the
    /// environment.
    fn credential_probe_dumper(repo: &Path) -> String {
        let dir = repo.join("fake-bin-cred");
        std::fs::create_dir_all(&dir).expect("mkdir fake-bin-cred");
        let bin = dir.join("git");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\n'\n\
             tr '\\0' '\\037' < /proc/self/cmdline; printf '\\n'\n\
             printf '%s\\n' \"$GIT_VISTA_CREDENTIAL_TOKEN\"\n",
        )
        .expect("write credential-probe fake git");
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&bin, perm).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// **The acceptance criterion, driven end to end through a real spawn**:
    /// "the token appears in neither `/proc/PID/cmdline` … — asserted, not
    /// reasoned." This does not read the argv this module *composed* and
    /// trust it — it spawns a real child, has that child read its own
    /// `/proc/self/cmdline` from the kernel, and greps the canary out of
    /// that. It also proves the positive half in the same run: the helper
    /// really does receive the token, via the environment, so a
    /// "withholds by never actually supplying it" non-fix cannot pass this
    /// test either (that is the shape #665's paired-positive lesson is
    /// about, applied here before the fact rather than after a review
    /// catches it).
    #[tokio::test]
    async fn a_supplied_token_reaches_the_helpers_environment_and_never_the_processs_own_argv() {
        const CANARY: &str = "gv-test-canary-token-should-never-appear-in-argv-8f2c";

        let repo = fixture().await;
        let policy = production_policy(repo.path());
        let dumper = credential_probe_dumper(repo.path());

        let cmd = network_command_with_credential(
            &policy,
            repo.path(),
            &["ls-remote", "https://example.invalid/repo.git"],
            Some(CANARY),
        )
        .pinned_env_for_test(&[
            ("PATH", dumper),
            ("HOME", std::env::var("HOME").unwrap()),
            (spawn::CREDENTIAL_TOKEN_VAR, CANARY.to_string()),
        ]);
        let out = cmd.output().await.expect("fake git runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let composed_argv = lines.next().expect("argv line");
        let kernel_cmdline = lines.next().expect("/proc/self/cmdline line");
        let env_value = lines.next().expect("env value line");

        assert!(
            !composed_argv.contains(CANARY),
            "the argv this crate composed contains the token: {composed_argv}"
        );
        assert!(
            !kernel_cmdline.contains(CANARY),
            "the KERNEL's own record of this process's argv contains the token — \
             this is the exact acceptance criterion #582 states, and it is not \
             satisfied by this crate's argv looking clean if the OS view \
             disagrees: {kernel_cmdline}"
        );
        assert_eq!(
            env_value, CANARY,
            "the helper's environment does not carry the token at all — a \
             fix that merely withholds it everywhere would pass the two \
             assertions above without doing anything useful"
        );
        assert!(
            composed_argv.contains(spawn::CREDENTIAL_TOKEN_VAR),
            "the credential.helper config should name the variable BY NAME so \
             the helper knows where to read it, even though the value never \
             appears: {composed_argv}"
        );
    }

    /// `token: None` must be byte-identical to plain [`network_command`] — no
    /// `-c credential.helper=`, no environment variable — so every existing
    /// Remote-tier caller that has no token to offer is provably unaffected,
    /// not merely "probably fine because nothing broke in review."
    #[tokio::test]
    async fn no_token_is_byte_identical_to_plain_network_command() {
        let repo = fixture().await;
        let policy = production_policy(repo.path());
        let dumper = which_dumper(repo.path());
        let hermetic = |c: spawn::SandboxedCommand| {
            c.pinned_env_for_test(&[
                ("PATH", dumper.clone()),
                ("HOME", std::env::var("HOME").unwrap()),
            ])
        };

        let with_none = hermetic(network_command_with_credential(
            &policy,
            repo.path(),
            &["ls-remote", "origin"],
            None,
        ))
        .output()
        .await
        .expect("fake git runs (None leg)");
        let plain = hermetic(network_command(
            &policy,
            repo.path(),
            &["ls-remote", "origin"],
        ))
        .output()
        .await
        .expect("fake git runs (plain leg)");

        assert_eq!(
            with_none.stdout, plain.stdout,
            "network_command_with_credential(.., None) must compose the exact \
             same argv as network_command — a caller with nothing to offer \
             must see zero behavioural difference"
        );
    }

    /// The forced flag is the **non-empty** form. [`FORCED_NETWORK_ARGS`]'s
    /// `-c core.askpass=` is empty on purpose, to *clear* a config-level
    /// entry; this one must never accidentally take that shape, because an
    /// empty `credential.helper=` value clears every helper the operator's
    /// own config already declares — the opposite of "append a fallback".
    #[test]
    fn the_forced_credential_helper_value_is_never_empty() {
        let cfg = credential_helper_config();
        assert!(
            !cfg.is_empty(),
            "an empty credential.helper value clears the operator's own \
             configured helpers instead of falling back after them"
        );
        assert!(
            cfg.contains(spawn::CREDENTIAL_TOKEN_VAR),
            "the helper must name the variable it reads, or nothing tells a \
             reader (or a future edit) where the token is expected to come \
             from: {cfg}"
        );
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

    /// A password value that itself contains the literal text `://` (a
    /// plausible crafted/reused token) must not defeat redaction — the
    /// blocker this file's own review found: the authority-end scan used to
    /// stop at the *first* `/` it saw, which for this input is the first
    /// slash of the embedded `://`, well before the real userinfo `@`.
    #[test]
    fn redact_url_userinfo_strips_a_password_containing_a_scheme_separator() {
        assert_eq!(
            redact_url_userinfo("https://user:pa://hunter2ss@host/repo.git"),
            "https://host/repo.git"
        );
    }

    /// Paired negative for the case above: proves the assertion is capable
    /// of failing — the OLD (pre-fix) algorithm returned this exact input
    /// byte-for-byte unchanged, secret and all.
    #[test]
    fn unredacted_password_containing_a_scheme_separator_still_leaks() {
        let secret = "hunter2ss";
        let s = format!("https://user:pa://{secret}@host/repo.git");
        assert!(
            s.contains(secret),
            "test setup: secret must be present pre-redaction"
        );
        assert!(
            !redact_url_userinfo(&s).contains(secret),
            "redaction must remove it even though the password contains '://'; got: {}",
            redact_url_userinfo(&s)
        );
    }

    /// The other blocker this file's own review found: `redact_bytes` used
    /// to validate the *entire* buffer as UTF-8 before redacting anything,
    /// so one stray non-UTF-8 byte anywhere in a buffer — trivially
    /// producible by a hostile credential helper — suppressed redaction of
    /// an otherwise-plain-ASCII secret URL elsewhere in that same buffer.
    /// This plants a secret, then one invalid UTF-8 byte, then asserts the
    /// secret is still gone from the redacted output.
    #[test]
    fn redact_bytes_still_redacts_a_secret_when_the_buffer_also_has_an_invalid_utf8_byte() {
        let mut buf = b"debug: tried https://user:hunter2@host/repo.git".to_vec();
        buf.push(0xFF); // not valid UTF-8 on its own or as a continuation here
        buf.extend_from_slice(b" -- trailing text after the bad byte");

        let redacted = redact_bytes(&buf);
        assert!(
            !redacted
                .windows(8)
                .any(|w| w == b"hunter2@" || w == b"hunter2\xff"),
            "secret survived redaction: {}",
            String::from_utf8_lossy(&redacted)
        );
        assert!(
            !String::from_utf8_lossy(&redacted).contains("hunter2"),
            "secret survived redaction (lossy view): {}",
            String::from_utf8_lossy(&redacted)
        );
        // The invalid byte itself is preserved (passed through), not
        // dropped or lossily replaced — same "don't corrupt binary output"
        // posture the module doc commits to.
        assert!(redacted.contains(&0xFF));
    }

    /// Paired negative: without redaction, the secret is present in the raw
    /// buffer (proves the setup actually contains what the test above
    /// claims), AND — this is the specific regression — the OLD whole-buffer
    /// UTF-8 gate would have returned `buf` completely unchanged the moment
    /// `std::str::from_utf8` hit the 0xFF byte, secret included.
    #[test]
    fn unredacted_buffer_with_a_trailing_invalid_byte_still_contains_the_secret() {
        let mut buf = b"debug: tried https://user:hunter2@host/repo.git".to_vec();
        buf.push(0xFF);
        assert!(
            std::str::from_utf8(&buf).is_err(),
            "test setup: buffer must be invalid UTF-8"
        );
        assert!(String::from_utf8_lossy(&buf).contains("hunter2"));
    }

    #[test]
    fn redact_args_strips_userinfo_from_every_arg_that_has_it() {
        let args = [
            "push",
            "https://user:tok@host/repo.git",
            "HEAD:refs/heads/main",
        ];
        let redacted = redact_args(&args);
        assert_eq!(
            redacted,
            vec![
                "push".to_string(),
                "https://host/repo.git".to_string(),
                "HEAD:refs/heads/main".to_string(),
            ]
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
    ///
    /// # A single unreproduced failure, investigated and left open
    ///
    /// An adversarial review of this PR reported one observed failure of
    /// this exact test under real concurrent load, with `hardened.stderr`
    /// reading "Authentication failed for '\<url\>'" — a message shape that,
    /// if genuine, would mean the marker script actually ran despite the
    /// forcing. Investigated in the same review round: 65 runs total (15
    /// `cargo test` iterations of this test under a concurrently-running
    /// `cargo test --workspace` plus CPU/IO stress, and 50 more via a
    /// standalone bash reproduction of the same two-phase check under the
    /// same stress) produced zero repeats of a hardened-phase bypass. A
    /// source read found no mechanism that could cause one: `-c
    /// core.askpass=` is a command-line override, which git's own config
    /// precedence always ranks above repo-local `.git/config` regardless of
    /// read order or timing (not a race this crate's code arbitrates), and
    /// the one known process-wide-env-mutation hazard in this crate's own
    /// tests (`sandbox::argv::SSH_AUTH_SOCK_LOCK`'s doc) does not touch
    /// `core.askpass`/`GIT_ASKPASS`/`SSH_ASKPASS` anywhere in this codebase.
    /// Left open rather than "fixed" with an unverified change: there is
    /// nothing concrete to change, and a speculative retry-tolerant rewrite
    /// of a security-load-bearing test would hide a real flake if one exists
    /// rather than catch it. If this reproduces again, capture the full
    /// process environment and `ps` tree at the moment of failure, not just
    /// the stderr string.
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
