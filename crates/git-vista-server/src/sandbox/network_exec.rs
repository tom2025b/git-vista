//! #228 (M2.20b): the shared Network-tier exec harness — the one execution
//! path every fetch/pull/push spawn is meant to go through, so askpass
//! hardening and output redaction are enforced structurally rather than
//! re-derived at each of the three call sites.
//!
//! [`network_command`] is wired into production at `git_cmd.rs`'s single
//! spawn chokepoint (`sandboxed`): any call declaring
//! [`NetworkNeed::Remote`] gets this module's forced executable hardening on
//! the way in and [`redact_output`]'s redaction on the way out, rather than
//! this module owning a second, parallel policy-build-and-spawn path of its
//! own. Fetch, pull and push all declare `NetworkNeed::Remote`; the chokepoint
//! they already go through is what enforces the boundary.
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
//! # Repository-named executables are not operator configuration
//!
//! #755 measured the transport commands themselves, with every ordinary hook
//! disabled. A repository-local `core.sshCommand`, `core.gitProxy`,
//! `credential.helper`, `core.fsmonitor`, `remote.<name>.uploadpack` or
//! `remote.<name>.receivepack` still ran an arbitrary marker program. The same
//! experiment found `remote.<name>.vcs` and URL-selected remote helpers, and
//! found that `protocol.ext.allow=always` turns an `ext::` URL into an
//! arbitrary shell-command selector.
//!
//! [`FORCED_NETWORK_ARGS`] therefore supplies safe server-authored values for
//! the fixed-name keys, and [`compose_network_args`] supplies Git's explicit
//! `--upload-pack=git-upload-pack` / `--receive-pack=git-receive-pack` options
//! for transport commands. Those options, unlike a hard-coded
//! `remote.origin.*` config override, cover every validated remote name. The
//! `ext` protocol is forced off. Command-line config and explicit transport
//! options outrank `.git/config`, which is writable inside this tier's
//! repository grant.
//!
//! This deliberately breaks repository or global configuration that chooses
//! a credential helper, an fsmonitor hook, a custom SSH command, or a custom
//! upload/receive-pack command for a Network-tier spawn. Ordinary OpenSSH
//! configuration (`~/.ssh/config`, keys,
//! jump hosts and the inherited agent) still works through the server-authored
//! `ssh` command. HTTPS authentication now needs a credential supplied by
//! Git-Vista's own [`network_command_with_credential`] helper; tokenless
//! fetch/push and clone no longer fall back to an operator credential helper.
//!
//! #779 closes native Git proxies and unapproved installed remote helpers with
//! fixed `GIT_ALLOW_PROTOCOL=http:https:ssh:git:file` and `GIT_PROXY_COMMAND=`
//! values at spawn time. The allowlist outranks even specific repository
//! protocol rules, including after URL rewriting and for `remote.<name>.vcs`.
//! The empty proxy environment override bypasses all `core.gitProxy` entries,
//! unlike a later config reset, while preserving direct `git://` connections.
//! Proxy commands and custom helpers (including `ext`) are unavailable even
//! when global or repository config enables them.
//!
//! Clone transfer also forces an empty `init.templateDir` and removes inherited
//! `GIT_TEMPLATE_DIR`. Both are required: the environment variable has higher
//! precedence than command-line config. Without them, `clone --no-checkout`
//! can copy operator-selected hooks and config into the destination, and the
//! later checkout deliberately reads that repository-local state. The same
//! argv pins replace `core.alternateRefsCommand` with the inert standard
//! `true` program before connectivity checks can consult it. Git documents
//! `gc.recentObjectsHook` as multi-valued, so appending a safe value would not
//! clear earlier hooks; `maintenance.auto=false` disables the only production
//! Network consumer instead, the automatic maintenance run after fetch.
//!
//! `PATH`, `GIT_EXEC_PATH`, SSH and askpass environment selectors still come
//! from the operator's parent environment; executable lookup remains trusted
//! for ordinary Network commands. Clone checkout is narrower: its LFS driver
//! is an absolute path from `sandbox::lfs`'s reviewed candidate list.
//! HTTP(S) uses Git's installed standard helpers. Both transport environment
//! values are server-authored and replace inherited values. Checkout also
//! receives them after its environment is cleared. Its separate config-scope
//! policy disables system and global Git configuration, because LFS
//! custom-transfer/extension and general filter commands are executable
//! surfaces that the protocol policy cannot constrain. #831 restores only a
//! fixed LFS driver through [`lfs_checkout_command`]. That command also makes
//! Git LFS rewrite any direct `http://` object-action href to the fixed HTTPS
//! refusal sink on a port the checkout policy denies. This covers URLs chosen
//! in a batch response, not merely HTTPS-to-HTTP redirects.
//!
//! [`redact_output`] remains defence in depth for diagnostics emitted by the
//! selected transport. A [`CredentialedCommand`] additionally knows and
//! removes the exact token it supplied, so a bare-token diagnostic cannot
//! leave that value even though it is not URL-shaped (#680, ADR 0128).
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

use super::{lfs, spawn, Policy};

const REDACTED_CREDENTIAL: &[u8] = b"[REDACTED CREDENTIAL]";

/// Prepended to every Network-tier spawn's args, ahead of the subcommand.
/// Every value is server-authored and outranks repository config.
///
/// Positioned first, not last: git's `-c` flags must precede the subcommand,
/// and every caller in this crate already passes `args` as `[subcommand,
/// …]` (see `run_git`/`run_branch_cmd` in `planner.rs`), so there is no
/// legitimate later occurrence of the same key for this to lose a
/// last-one-wins race against. `args` here is always server-authored, never
/// raw request data — if a future caller ever needs to pass its own `-c`
/// flags, it must not repeat one of these keys ahead of the subcommand.
const FORCED_NETWORK_ARGS: &[&str] = &[
    "-c",
    spawn::EMPTY_INIT_TEMPLATE_CONFIG,
    "-c",
    "core.askpass=",
    "-c",
    "credential.helper=",
    "-c",
    "core.sshCommand=ssh",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.alternateRefsCommand=true",
    "-c",
    "maintenance.auto=false",
];

/// Compose the fixed config pins and the transport-side executable pin.
///
/// `remote.<name>.uploadpack` / `receivepack` cannot be pinned with one fixed
/// config key because `<name>` is request-selected. Git's explicit transport
/// options are the stronger spelling and avoid teaching this security boundary
/// how to parse a remote name out of each command shape. They must follow the
/// subcommand, while every `-c` above must precede it.
fn compose_network_args<'a>(
    args: &'a [&'a str],
    credential_helper: Option<&'a str>,
) -> Vec<&'a str> {
    let mut full: Vec<&str> = FORCED_NETWORK_ARGS.to_vec();
    if let Some(helper) = credential_helper {
        full.extend_from_slice(&["-c", helper]);
    }

    let Some((subcommand, rest)) = args.split_first() else {
        return full;
    };
    full.push(subcommand);
    match *subcommand {
        "fetch" | "pull" | "ls-remote" | "clone" => full.push("--upload-pack=git-upload-pack"),
        "push" => full.push("--receive-pack=git-receive-pack"),
        _ => {}
    }
    full.extend_from_slice(rest);
    full
}

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
    let full = compose_network_args(args, None);
    spawn::command_async(policy, repo, &full).with_network_transport_policy()
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

/// A sealed credential-bearing command whose only production completion path
/// redacts the exact value it supplied before returning captured output (#680).
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
/// # Clears first, then supplies exactly one server helper
///
/// [`FORCED_NETWORK_ARGS`]'s empty `credential.helper=` resets the helper
/// chain after repository/global config has been read. This function then
/// adds Git-Vista's non-empty helper after that reset. The credential-bearing
/// child therefore runs exactly the server-authored helper and cannot hand the
/// token environment to a repository-authored helper first.
///
/// # `token: None` changes nothing
///
/// When `token` is `None` this is byte-identical to [`network_command`] —
/// no server-authored non-empty helper and no environment variable set — so every
/// existing Remote-tier caller (`exec_push`, and everywhere else this
/// crate's `git_cmd::sandboxed()` routes `NetworkNeed::Remote`) is
/// unaffected until it deliberately opts in.
pub(crate) struct CredentialedCommand {
    command: spawn::SandboxedCommand,
    token: Option<Vec<u8>>,
}

impl CredentialedCommand {
    pub(crate) fn kill_on_drop(mut self, kill: bool) -> Self {
        self.command = self.command.kill_on_drop(kill);
        self
    }

    /// Run the command and return only output with URL userinfo, URL queries,
    /// and the exact credential removed. The raw bytes never leave this value,
    /// so a caller cannot remember one redaction rule and forget the others.
    pub(crate) async fn output(self) -> std::io::Result<Output> {
        let output = self.command.output().await?;
        Ok(redact_output_with_credential(output, self.token.as_deref()))
    }

    #[cfg(test)]
    pub(crate) fn pinned_env_for_test<K, V>(mut self, profile: &[(K, V)]) -> Self
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        self.command = self.command.pinned_env_for_test(profile);
        self
    }

    #[cfg(test)]
    fn credential_env_for_test(&self) -> Option<String> {
        self.command.credential_env_for_test()
    }
}

/// [`network_command`], plus Git-Vista's own credential helper when `token`
/// is `Some` (M13.01, #582). The distinct return type is ADR 0128's output
/// guarantee: a credential-bearing caller cannot recover raw captured bytes.
pub(crate) fn network_command_with_credential(
    policy: &Policy,
    repo: &Path,
    args: &[&str],
    token: Option<&str>,
) -> CredentialedCommand {
    let command = if let Some(token) = token {
        let helper_config = format!("credential.helper={}", credential_helper_config());
        let full = compose_network_args(args, Some(&helper_config));
        spawn::command_async(policy, repo, &full)
            .with_network_transport_policy()
            .credential_env(token)
    } else {
        network_command(policy, repo, args)
    };
    CredentialedCommand {
        command,
        token: token.map(|value| value.as_bytes().to_vec()),
    }
}

/// A sealed command for the phase after credential use has ended: clone's
/// untrusted checkout half (#702, #704).
///
/// # Why this is its own type, like [`CredentialedCommand`]
///
/// This used to return a bare [`spawn::SandboxedCommand`] whose environment had
/// three names removed, and clone's handler then had to remember
/// `.map(redact_output)` on every one of its completion paths. Both halves were
/// "correct if the next caller remembers", which is the structure ADR 0128
/// rejected for credentials and is exactly how #704 stayed open: the removal
/// list was complete for the names someone had listed.
///
/// So the guarantee lives in the value. The only way to obtain one of these is
/// [`network_command_without_credential`], which applies
/// [`spawn::UNTRUSTED_CHECKOUT_ENV_ALLOWLIST`] unconditionally; the only way to
/// run one is [`Self::output`], which redacts. There is no `spawn`, no `env`,
/// and no way to recover the inner command — a future call site cannot obtain
/// an untrusted-checkout launcher carrying a full environment, because that
/// value is not constructible.
///
/// What it keeps is TCP network access. System and global Git configuration
/// are disabled, and the checkout policy blocks hooks. Repository-local config
/// remains readable, but #827 prevents a fresh remote from supplying it. #723
/// also narrows one capability: this sealed path selects the checkout seccomp
/// profile that denies AF_UNIX while retaining the TCP-port-443 grant.
pub(crate) struct UntrustedCheckoutCommand(spawn::SandboxedCommand);

impl UntrustedCheckoutCommand {
    pub(crate) fn kill_on_drop(mut self, kill: bool) -> Self {
        self.0 = self.0.kill_on_drop(kill);
        self
    }

    /// Run the command and return output with URL userinfo and queries already
    /// removed.
    ///
    /// No Git-Vista credential can appear here — this child never received one
    /// — but the *remote URL* git echoes into its own diagnostics can still
    /// carry operator-supplied userinfo, while an LFS object-action URL can
    /// carry a time-limited query credential. This output reaches both the
    /// server log and the HTTP error body.
    pub(crate) async fn output(self) -> std::io::Result<Output> {
        self.0.output().await.map(redact_output)
    }
}

/// A Network-tier command for the phase after credential use has ended.
/// It preserves TCP and repository-local configuration while replacing the
/// child's environment with the allowlisted one
/// ([`spawn::UNTRUSTED_CHECKOUT_ENV_ALLOWLIST`]), disabling system and global
/// Git configuration, and selecting the checkout-only AF_UNIX denial.
///
/// The allowlist is applied here rather than left to the caller: this function
/// is the boundary, and a boundary a caller can decline to cross is not one.
///
/// It takes a [`crate::sandbox::CheckoutPolicy`], not a `Policy`, so the
/// transfer's SSH-granted policy cannot reach an untrusted checkout even by a
/// transposed argument — see that type's doc for the two ways a source-level
/// check of the same property was defeated.
pub(crate) fn network_command_without_credential(
    policy: &crate::sandbox::CheckoutPolicy,
    repo: &Path,
    args: &[&str],
) -> UntrustedCheckoutCommand {
    let full = compose_network_args(args, None);
    UntrustedCheckoutCommand(
        spawn::checkout_command_async(policy, repo, &full)
            .with_network_transport_policy()
            .with_untrusted_checkout_env(),
    )
}

/// The only fresh-clone materialisation command that receives an executable
/// filter. It starts from [`network_command_without_credential`]'s sealed
/// environment/config/seccomp boundary, then adds only #831's server-authored
/// LFS settings ahead of the checkout subcommand.
///
/// `clone_url` has already passed the clone handler's TLS-aware validation. It
/// is used only to pin `lfs.url` as data; no part of it can select the filter
/// executable, a transfer adapter, a hook, or a shell fragment. The config also
/// rejects a direct plaintext object-action href before the built-in adapter
/// can request it.
pub(crate) fn lfs_checkout_command(
    policy: &crate::sandbox::CheckoutPolicy,
    repo: &Path,
    clone_url: &str,
) -> UntrustedCheckoutCommand {
    let mut owned = lfs::checkout_config(clone_url);
    owned.extend(["checkout".to_string(), "-f".to_string()]);
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    network_command_without_credential(policy, repo, &refs)
}

#[cfg(test)]
pub(super) fn lfs_checkout_command_with_program(
    policy: &crate::sandbox::CheckoutPolicy,
    repo: &Path,
    clone_url: &str,
    program: &str,
) -> UntrustedCheckoutCommand {
    let mut owned = lfs::checkout_config_with_program(clone_url, program);
    owned.extend(["checkout".to_string(), "-f".to_string()]);
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    network_command_without_credential(policy, repo, &refs)
}

/// Strip `user[:pass]@` userinfo and the complete query from every
/// `<scheme>://…` URL substring found in `bytes`, leaving the scheme, host,
/// path, and any fragment intact —
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

            // Git LFS errors can echo a server-supplied object-action URL.
            // Those URLs commonly carry time-limited credentials in their
            // query even though their authority has no userinfo. Remove the
            // entire query rather than trying to maintain an inevitably
            // incomplete list of credential-shaped parameter names.
            let token_end = bytes[end..]
                .iter()
                .position(|b| b.is_ascii_whitespace())
                .map_or(n, |offset| end + offset);
            let fragment = bytes[end..token_end]
                .iter()
                .position(|b| *b == b'#')
                .map(|offset| end + offset);
            let query = bytes[end..fragment.unwrap_or(token_end)]
                .iter()
                .position(|b| *b == b'?')
                .map(|offset| end + offset);
            if let Some(query) = query {
                out.extend_from_slice(&bytes[end..query]);
                if let Some(fragment) = fragment {
                    out.extend_from_slice(&bytes[fragment..token_end]);
                }
                i = token_end;
                continue;
            }
            i = end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// [`redact_url_userinfo_bytes`] over a `&str`, including its query stripping,
/// for callers (and this file's own pure unit tests) that already have text
/// rather than raw process output.
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
/// reason), and this redaction is ASCII-anchored (`://`, `@`, `?`) so it has
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

fn redact_output_with_credential(output: Output, token: Option<&[u8]>) -> Output {
    let output = redact_output(output);
    let Some(token) = token.filter(|value| !value.is_empty()) else {
        return output;
    };
    Output {
        status: output.status,
        stdout: redact_literal(&output.stdout, token),
        stderr: redact_literal(&output.stderr, token),
    }
}

fn redact_literal(bytes: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut redacted = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(index) = rest
        .windows(secret.len())
        .position(|window| window == secret)
    {
        redacted.extend_from_slice(&rest[..index]);
        redacted.extend_from_slice(REDACTED_CREDENTIAL);
        rest = &rest[index + secret.len()..];
    }
    redacted.extend_from_slice(rest);
    redacted
}

fn redact_bytes(bytes: &[u8]) -> Vec<u8> {
    redact_plaintext_lfs_refusal_userinfo_bytes(&redact_url_userinfo_bytes(bytes))
}

/// #836 follow-up: git-lfs's `url.*.insteadOf` rewrite (`lfs.rs`'s
/// `PLAINTEXT_ACTION_REFUSAL_URL`) is a literal prefix substitution, not a
/// URL-aware one. Given a server-supplied plaintext action href
/// `http://TOKEN@host/object`, the rewritten string git-lfs actually emits
/// (and that its own stderr can echo back to a client and to this process's
/// logs) is `https://127.0.0.1:1/git-vista-refused-plaintext-lfs-action/TOKEN@host/object`
/// — the credential has moved from the URL's authority (where
/// [`redact_url_userinfo_bytes`] looks for it, immediately after `://`) into
/// the *path*. [`redact_url_userinfo_bytes`] correctly redacts the
/// `127.0.0.1:1` authority and then treats everything from the first `/`
/// onward as an opaque path, so it never re-examines `TOKEN@host` for a
/// second, path-embedded `userinfo@` shape. That gap is this function's
/// entire job: run only after the normal pass, scan for our own known
/// refusal-URL prefix (a fixed literal this crate controls, not a general
/// pattern), and apply the identical "redact up to the last `@` in this
/// run" rule to whatever follows it, up to the next `/`, `?`, `#`, or
/// whitespace.
fn redact_plaintext_lfs_refusal_userinfo_bytes(bytes: &[u8]) -> Vec<u8> {
    let prefix = crate::sandbox::lfs::PLAINTEXT_ACTION_REFUSAL_URL.as_bytes();
    let n = bytes.len();
    let is_authority_delim = |b: u8| b == b'/' || b == b'?' || b == b'#' || b.is_ascii_whitespace();

    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let starts_prefix = i + prefix.len() <= n && &bytes[i..i + prefix.len()] == prefix;
        if starts_prefix {
            out.extend_from_slice(prefix);
            let start = i + prefix.len();
            let mut end = start;
            while end < n && !is_authority_delim(bytes[end]) {
                end += 1;
            }
            let mut at = None;
            let mut k = start;
            while k < end {
                if bytes[k] == b'@' {
                    at = Some(k);
                }
                k += 1;
            }
            let keep_from = at.map_or(start, |a| a + 1);
            out.extend_from_slice(&bytes[keep_from..end]);
            i = end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A redacted view of the argv a Network-tier spawn ran with, for callers
/// that want to log or journal "ran: git <args…>" style diagnostics.
///
/// [`redact_output`] only ever sees a spawn's captured stdout/stderr —
/// `sandboxed`'s (`git_cmd.rs`) `args: &[&str]` parameter is a second sink
/// for exactly the same secret shape, since every SSH test in this file (and
/// every real caller) routinely passes the remote URL as one of `args`
/// (`&["push", &fixture.repo_url, …]`). Nothing upstream can redact args on
/// this module's behalf — a caller that logs `args` directly bypasses
/// [`redact_output`] entirely — so this gives that caller the same
/// [`redact_url_userinfo`] treatment as a first-class, explicit primitive
/// rather than leaving args logging to rediscover (or forget) the need for
/// it independently.
///
/// # #801: this had no caller, and that was the wrong state to leave it in
///
/// This function used to carry `#[allow(dead_code)]` and a comment promising
/// it was "wired in once a diagnostic/journal path logs the argv" — with
/// nothing tracking that wiring, and nothing stopping a future log line from
/// being added without it. It now has a real one:
/// [`super::reconcile_need`]'s D3 cross-check, the one place in this crate
/// that already formatted a Network-tier spawn's raw `args` into a
/// diagnostic message (a `debug_assert!` panic message and a release
/// `eprintln!`) — reached only when a caller has already mislabelled a
/// remote-shaped operation `Local`, but reachable, and unredacted until that
/// fix landed in the same commit as this comment. `argv_boundary`'s
/// `argv_redaction_boundary` module is the tripwire that keeps a *future*
/// site like it from reintroducing the same gap: it scans this crate's known
/// Network-tier/chokepoint sink functions for exactly this pattern — a
/// logging or panic macro that names `args` without routing it through this
/// function first.
pub(crate) fn redact_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| redact_url_userinfo(a)).collect()
}

#[cfg(test)]
mod tests {
    use super::super::shim_cli::{fixture, production_policy};
    use super::*;

    // --- pure argv shape ----------------------------------------------

    /// A shim-independent view of the shared fixed-selector list. The real
    /// spawned-argv test below remains the stronger production proof; this
    /// focused assertion can also run in a fresh failure-atlas clone, where
    /// Cargo has not installed the top-level `gv-sandbox` executable that the
    /// integration launcher needs.
    ///
    /// MUTATION  1: remove `init.templateDir=` while the environment half
    /// remains; the operator-global template route reopens.
    /// MUTATION 2: remove the alternate-refs replacement or automatic-
    /// maintenance denial while every older transport pin remains; an
    /// executable selector reopens.
    #[test]
    fn forced_network_args_pin_template_alternate_refs_and_maintenance() {
        let args = compose_network_args(&["fetch", "origin"], None);
        for required in [
            "init.templateDir=",
            "core.alternateRefsCommand=true",
            "maintenance.auto=false",
        ] {
            assert!(
                args.contains(&required),
                "the shared Network argv omitted fixed selector {required}"
            );
        }
    }

    /// `network_command`'s argv is exactly `command_async`'s own argv with
    /// [`FORCED_NETWORK_ARGS`] spliced in immediately after `-C <repo>`, then
    /// the transport option after the subcommand — mirrors `spawn.rs`'s
    /// `the_wrapper_argv_is_the_sandbox_argv_plus_the_repo_and_args`, one
    /// layer up.
    #[tokio::test]
    async fn network_command_pins_every_fixed_selector_and_the_transport_program() {
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

        assert_eq!(&args[..2], ["-C", repo.path().to_str().unwrap()]);
        assert_eq!(
            &args[2..],
            [
                "-c",
                "init.templateDir=",
                "-c",
                "core.askpass=",
                "-c",
                "credential.helper=",
                "-c",
                "core.sshCommand=ssh",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "core.alternateRefsCommand=true",
                "-c",
                "maintenance.auto=false",
                "push",
                "--receive-pack=git-receive-pack",
                "origin",
                "main",
            ]
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
    /// that.
    ///
    /// # What this test does NOT prove — corrected after review
    ///
    /// This comment used to claim the run "also proves the positive half:
    /// the helper really does receive the token, via the environment, so a
    /// 'withholds by never actually supplying it' non-fix cannot pass this
    /// test either." **That was false.** `pinned_env_for_test` does
    /// `env_clear()` and then applies its profile — which includes
    /// `CREDENTIAL_TOKEN_VAR` — so it wipes whatever `credential_env` set
    /// and supplies the canary itself. The env assertion below therefore
    /// checks the test's own profile, not the production supply path.
    ///
    /// Measured, not argued: deleting `.credential_env(token)` from
    /// `network_command_with_credential` left this test **green**
    /// (`failure-atlas` mutation 332, verdict `survived`) — the exact
    /// "structurally complete, semantically inert" shape the claim above
    /// said could not pass. Found by grok's read-only review of #668,
    /// 2026-09-05.
    ///
    /// The argv/kernel-cmdline half below is sound and is what this test is
    /// for. The supply half is pinned separately by
    /// [`the_composed_command_actually_carries_the_token_in_its_environment`],
    /// which reads the composed command instead of a spawned child, because
    /// the spawn's environment is exactly what this harness overwrites.
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
            env_value.as_bytes(),
            REDACTED_CREDENTIAL,
            "a credential-bearing command must not return the environment \
             value its child printed"
        );
        assert!(
            composed_argv.contains(spawn::CREDENTIAL_TOKEN_VAR),
            "the credential.helper config should name the variable BY NAME so \
             the helper knows where to read it, even though the value never \
             appears: {composed_argv}"
        );
    }

    /// The supply half, pinned where the spawn-based test cannot pin it:
    /// `network_command_with_credential` must actually put the token on the
    /// command's environment via `credential_env`.
    ///
    /// Separate from the `/proc/self/cmdline` test on purpose. That one
    /// spawns a real child under `pinned_env_for_test`, which `env_clear()`s
    /// and re-supplies `CREDENTIAL_TOKEN_VAR` from its own profile — so no
    /// assertion made on the *spawned* environment can distinguish "the
    /// composer supplied it" from "the test profile supplied it". Deleting
    /// `.credential_env(token)` left that test green (`failure-atlas`
    /// mutation 332, `survived`). This test is the one that goes red for
    /// that defect.
    ///
    /// The negative arm matters as much as the positive one: `None` must
    /// leave the variable unset, so this cannot pass by the composer
    /// setting it unconditionally.
    #[tokio::test]
    async fn the_composed_command_actually_carries_the_token_in_its_environment() {
        const CANARY: &str = "gv-test-supply-canary-9d41";

        let repo = fixture().await;
        let policy = production_policy(repo.path());

        let with_token = network_command_with_credential(
            &policy,
            repo.path(),
            &["ls-remote", "https://example.invalid/repo.git"],
            Some(CANARY),
        );
        assert_eq!(
            with_token.credential_env_for_test().as_deref(),
            Some(CANARY),
            "the composed command does not carry the token in its \
             environment — the helper would have nothing to read, and the \
             spawn-based test cannot catch this because its own pinned \
             profile re-supplies the variable"
        );

        let without_token = network_command_with_credential(
            &policy,
            repo.path(),
            &["ls-remote", "https://example.invalid/repo.git"],
            None,
        );
        assert_eq!(
            without_token.credential_env_for_test(),
            None,
            "a tokenless call must leave {} unset; setting it \
             unconditionally would make the positive assertion above \
             vacuous",
            spawn::CREDENTIAL_TOKEN_VAR
        );
    }

    /// `token: None` must be byte-identical to plain [`network_command`] — no
    /// additional non-empty helper and no environment variable — so every
    /// existing Remote-tier caller that has no token to offer gets exactly the
    /// base harness's deliberate helper reset and nothing credential-bearing.
    #[tokio::test]
    async fn no_token_is_byte_identical_to_plain_network_command() {
        let repo = fixture().await;
        let policy = production_policy(repo.path());
        let dumper = which_dumper(repo.path());
        let profile = [
            ("PATH", dumper.clone()),
            ("HOME", std::env::var("HOME").unwrap()),
        ];

        let with_none =
            network_command_with_credential(&policy, repo.path(), &["ls-remote", "origin"], None)
                .pinned_env_for_test(&profile)
                .output()
                .await
                .expect("fake git runs (None leg)");
        let plain = network_command(&policy, repo.path(), &["ls-remote", "origin"])
            .pinned_env_for_test(&profile)
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

    /// Git-Vista's own helper is the **non-empty** form, deliberately composed
    /// after [`FORCED_NETWORK_ARGS`]'s empty reset. It must never accidentally
    /// become a second empty value or the credential-bearing command would
    /// clear the chain and provide no way to consume its token.
    #[test]
    fn the_forced_credential_helper_value_is_never_empty() {
        let cfg = credential_helper_config();
        assert!(
            !cfg.is_empty(),
            "an empty server credential.helper value leaves the supplied \
             token with no helper able to consume it"
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
    fn redact_url_userinfo_strips_the_complete_query_and_keeps_a_fragment() {
        assert_eq!(
            redact_url_userinfo(
                "download https://objects.invalid/a.bin?token=hunter2&sig=secret#retry"
            ),
            "download https://objects.invalid/a.bin#retry"
        );
    }

    #[test]
    fn redact_url_userinfo_strips_queries_from_several_urls_in_one_string() {
        assert_eq!(
            redact_url_userinfo(
                "first https://a.invalid/x?X-Amz-Signature=one second http://b.invalid:443/y?token=two"
            ),
            "first https://a.invalid/x second http://b.invalid:443/y"
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

    /// #836 follow-up (daybreak review, 2026-09-11): the plaintext-LFS
    /// refusal rewrite (`lfs.rs`'s `PLAINTEXT_ACTION_REFUSAL_URL`) is a
    /// literal `insteadOf` prefix substitution, so a server-supplied action
    /// href `http://TOKEN@host/object` becomes
    /// `https://127.0.0.1:1/git-vista-refused-plaintext-lfs-action/TOKEN@host/object`
    /// in git-lfs's own stderr — the credential has moved from the URL
    /// authority into the path. Without this second pass, that stderr
    /// reaches both the HTTP response body (`clone.rs`'s
    /// `clone_execution_failure`) and the server log verbatim.
    #[test]
    fn redact_bytes_closes_the_path_embedded_credential_in_a_plaintext_lfs_refusal() {
        let secret = "hunter2";
        let refused = format!(
            "{}{secret}@host.example/objects/deadbeef",
            crate::sandbox::lfs::PLAINTEXT_ACTION_REFUSAL_URL
        );
        let buf = format!(
            "error: external filter failed\nfatal: unable to access '{refused}': fetch failed"
        );

        let redacted = redact_bytes(buf.as_bytes());
        let redacted_text = String::from_utf8_lossy(&redacted);

        assert!(
            !redacted_text.contains(secret),
            "path-embedded credential survived redaction: {redacted_text}"
        );
        // The refusal URL's own fixed prefix must survive — this is a
        // targeted scrub of what follows it, not a blunt drop of the whole
        // line, so the refusal reason stays diagnosable.
        assert!(
            redacted_text.contains(crate::sandbox::lfs::PLAINTEXT_ACTION_REFUSAL_URL),
            "redaction over-scrubbed the refusal marker itself: {redacted_text}"
        );
        assert!(
            redacted_text.contains("host.example/objects/deadbeef"),
            "redaction dropped non-secret path content it shouldn't have: {redacted_text}"
        );
    }

    /// Paired negative: proves the secret really was present in the raw
    /// buffer at the path position this test targets — the same discipline
    /// as `unredacted_text_still_contains_the_literal_secret` above, applied
    /// to the path-embedded shape specifically.
    #[test]
    fn unredacted_text_still_contains_the_path_embedded_lfs_refusal_secret() {
        let secret = "hunter2";
        let refused = format!(
            "{}{secret}@host.example/objects/deadbeef",
            crate::sandbox::lfs::PLAINTEXT_ACTION_REFUSAL_URL
        );
        assert!(refused.contains(secret));
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

    #[test]
    fn redact_output_removes_lfs_action_query_credentials_from_both_streams() {
        let raw = Output {
            status: std::process::ExitStatus::default(),
            stdout: b"download https://objects.invalid/a?token=stdout-secret".to_vec(),
            stderr: b"LFS: GET http://objects.invalid:443/a?sig=stderr-secret failed".to_vec(),
        };
        let redacted = redact_output(raw);
        assert_eq!(
            redacted.stdout,
            b"download https://objects.invalid/a".to_vec()
        );
        assert_eq!(
            redacted.stderr,
            b"LFS: GET http://objects.invalid:443/a failed".to_vec()
        );
    }

    #[tokio::test]
    async fn untrusted_checkout_output_never_exposes_an_lfs_action_query() {
        let repo = fixture().await;
        let bin_dir = repo.path().join("query-redaction-bin");
        std::fs::create_dir(&bin_dir).expect("create fake binary directory");
        let git = bin_dir.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\nprintf '%s\\n' 'LFS: GET https://objects.invalid/a?token=checkout-secret failed' >&2\nexit 1\n",
        )
        .expect("write fake git");
        let mut permissions = std::fs::metadata(&git)
            .expect("fake git metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&git, permissions).expect("make fake git executable");

        let policy = super::super::policy_for_clone_checkout(repo.path())
            .expect("checkout policy must build");
        let command = super::super::test_env::with_env(
            &[
                ("PATH", Some(bin_dir.as_os_str())),
                ("HOME", Some(repo.path().as_os_str())),
            ],
            || network_command_without_credential(&policy, repo.path(), &["status"]),
        );
        let output = command.output().await.expect("fake git runs");
        assert!(!output.status.success(), "fixture must retain its failure");
        assert_eq!(
            output.stderr,
            b"LFS: GET https://objects.invalid/a failed\n".to_vec(),
            "the sealed checkout command must redact before returning captured output"
        );
    }

    #[test]
    fn credential_redaction_removes_a_bare_token_from_both_output_streams() {
        let token = b"clone-hook-canary";
        let raw = Output {
            status: std::process::ExitStatus::default(),
            stdout: b"progress clone-hook-canary\xff".to_vec(),
            stderr: b"fatal: clone-hook-canary".to_vec(),
        };

        let redacted = redact_output_with_credential(raw, Some(token));

        assert!(!redacted.stdout.windows(token.len()).any(|w| w == token));
        assert!(!redacted.stderr.windows(token.len()).any(|w| w == token));
        assert!(redacted.stdout.contains(&0xff));
        assert!(redacted
            .stderr
            .windows(REDACTED_CREDENTIAL.len())
            .any(|w| w == REDACTED_CREDENTIAL));
    }
}

/// Run a direct git command for sandbox test-fixture construction.
///
/// Keeping the spawn here preserves the crate's reviewed process boundary:
/// `argv_boundary` already audits this test-only file, while the composed
/// checkout proof remains free of new spawn sites.
#[cfg(test)]
pub(super) fn run_fixture_git<I, S>(cwd: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = fixture_git_output(cwd, args);
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Output-returning sibling for measurements that need to inspect a fixture
/// command's status or bytes. It deliberately shares the reviewed test-only
/// spawn site above instead of creating another process boundary.
#[cfg(test)]
pub(super) fn fixture_git_output<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git starts")
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

    /// A tracked attribute can select any filter name, so the system and both
    /// operator-global config paths must be absent from the real checkout
    /// launcher. This is not an LFS-only test: three generic smudge drivers
    /// make the scope boundary observable without requiring git-lfs or a
    /// network service. `$HOME/.gitconfig` and
    /// `$XDG_CONFIG_HOME/git/config` are the production-shaped paths: those
    /// two environment names survive the checkout allowlist, while
    /// `GIT_CONFIG_GLOBAL` itself does not.
    ///
    /// The first checkout is the executing control. It uses the same compiled
    /// checkout policy, shim and reaper, but omits
    /// [`network_command_without_credential`]'s environment hardening; one
    /// driver comes from a synthetic system file and the other two from the
    /// real home and XDG global paths. The production launcher receives that
    /// identical environment through `pinned_env_for_test`.
    ///
    /// The control uses `checkout_command_async`, while production also adds
    /// `FORCED_NETWORK_ARGS` and the transport environment policy. Those pins
    /// do not suppress smudge filters; the two mutation arms below isolate the
    /// config-scope completion policy as the load-bearing difference.
    ///
    /// MUTATION 1 (remove the mechanism): omit
    /// `apply_checkout_git_config_policy` from the completion policies. All
    /// three markers execute in the production leg.
    /// MUTATION 2 (weaken the mechanism): retain `GIT_CONFIG_NOSYSTEM=1` but
    /// omit `GIT_CONFIG_GLOBAL=/dev/null`. The home and XDG markers execute.
    #[tokio::test]
    async fn clone_checkout_ignores_system_home_and_xdg_filter_commands() {
        let fixture = home_and_cwd();
        let system_marker = fixture.cwd.join("system-filter-ran");
        let home_marker = fixture.cwd.join("home-filter-ran");
        let xdg_marker = fixture.cwd.join("xdg-filter-ran");
        let system_filter = fixture.home.join("system-filter.sh");
        let home_filter = fixture.home.join("home-filter.sh");
        let xdg_filter = fixture.home.join("xdg-filter.sh");

        for (program, marker) in [
            (&system_filter, &system_marker),
            (&home_filter, &home_marker),
            (&xdg_filter, &xdg_marker),
        ] {
            std::fs::write(
                program,
                format!(
                    "#!/bin/sh\nprintf 'RAN\\n' > '{}'\nexec /bin/cat\n",
                    marker.display()
                ),
            )
            .expect("write filter program");
            let mut permissions = std::fs::metadata(program).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
            std::fs::set_permissions(program, permissions).unwrap();
        }

        let system_config = fixture.home.join("system.gitconfig");
        let home_config = fixture.home.join(".gitconfig");
        let xdg_home = fixture.home.join("xdg");
        let xdg_config = xdg_home.join("git/config");
        std::fs::create_dir_all(xdg_config.parent().unwrap()).expect("create XDG config dir");
        for (config, name, program) in [
            (&system_config, "gv782system", &system_filter),
            (&home_config, "gv782home", &home_filter),
            (&xdg_config, "gv782xdg", &xdg_filter),
        ] {
            std::fs::write(
                config,
                format!(
                    "[filter \"{name}\"]\n\tsmudge = {}\n\trequired = true\n",
                    program.display()
                ),
            )
            .expect("write filter config");
        }

        run(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&fixture.cwd),
            "init filter repo",
        );
        std::fs::write(
            fixture.cwd.join(".gitattributes"),
            "system-payload filter=gv782system\nhome-payload filter=gv782home\nxdg-payload filter=gv782xdg\n",
        )
        .expect("write filter attributes");
        for (path, contents) in [
            ("system-payload", "system payload\n"),
            ("home-payload", "home payload\n"),
            ("xdg-payload", "xdg payload\n"),
        ] {
            std::fs::write(fixture.cwd.join(path), contents).expect("write filtered payload");
        }
        run(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&fixture.cwd),
            "stage filter fixture",
        );
        run(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "filter fixture",
                ])
                .current_dir(&fixture.cwd),
            "commit filter fixture",
        );

        let env = vec![
            ("PATH", "/usr/bin:/bin".to_string()),
            ("HOME", fixture.home.to_string_lossy().into_owned()),
            ("XDG_CONFIG_HOME", xdg_home.to_string_lossy().into_owned()),
            (
                "GIT_CONFIG_SYSTEM",
                system_config.to_string_lossy().into_owned(),
            ),
        ];
        let policy = network_policy(&fixture.home, &fixture.cwd, 9418);
        let checkout = crate::sandbox::CheckoutPolicy(policy);

        for name in ["system-payload", "home-payload", "xdg-payload"] {
            std::fs::remove_file(fixture.cwd.join(name)).expect("remove payload before control");
        }
        let control = spawn::checkout_command_async(&checkout, &fixture.cwd, &["checkout", "-f"])
            .pinned_env_for_test(&env)
            .output()
            .await
            .expect("unhardened checkout starts");
        assert!(
            control.status.success(),
            "executing control checkout failed: {}",
            String::from_utf8_lossy(&control.stderr)
        );
        assert!(
            system_marker.exists() && home_marker.exists() && xdg_marker.exists(),
            "executing control did not reach all three filter config paths"
        );

        for path in [
            &system_marker,
            &home_marker,
            &xdg_marker,
            &fixture.cwd.join("system-payload"),
            &fixture.cwd.join("home-payload"),
            &fixture.cwd.join("xdg-payload"),
        ] {
            std::fs::remove_file(path).expect("reset fixture before hardened checkout");
        }

        let mut hardened =
            network_command_without_credential(&checkout, &fixture.cwd, &["checkout", "-f"]);
        hardened.0 = hardened.0.pinned_env_for_test(&env);
        let output = hardened.output().await.expect("hardened checkout starts");
        assert!(
            output.status.success(),
            "hardened checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !system_marker.exists() && !home_marker.exists() && !xdg_marker.exists(),
            "an operator-configured filter executed during hardened checkout"
        );
        for (path, contents) in [
            ("system-payload", b"system payload\n".as_slice()),
            ("home-payload", b"home payload\n".as_slice()),
            ("xdg-payload", b"xdg payload\n".as_slice()),
        ] {
            assert_eq!(std::fs::read(fixture.cwd.join(path)).unwrap(), contents);
        }
    }

    /// Clone transfer must not let an operator-selected template create the
    /// repository-local executable state that checkout intentionally keeps.
    /// The control runs the transfer without the shared Network hardening,
    /// then runs the real hardened checkout: the template's config selects a
    /// filter from the remote's `.gitattributes`, and its executable
    /// `post-checkout` hook also runs. That is the live cross-phase bypass this
    /// regression guards, not an inference from the files clone copied.
    ///
    /// The production transfer has two independent template controls because
    /// Git gives the environment variable higher precedence than config:
    /// completion removes inherited `GIT_TEMPLATE_DIR`, and
    /// [`FORCED_NETWORK_ARGS`] supplies `-c init.templateDir=` to override
    /// system/global config. The production checkout must then materialise the
    /// payload with neither marker present.
    ///
    /// MUTATION 1 (remove the environment half): stop removing
    /// `GIT_TEMPLATE_DIR`; the environment-selected template is copied and
    /// both markers run.
    /// MUTATION 2 (remove the config half): drop `-c init.templateDir=`; the
    /// same template selected by `$HOME/.gitconfig` is copied and both markers
    /// run.
    #[tokio::test]
    async fn clone_transfer_cannot_seed_checkout_filters_or_hooks_from_a_template() {
        let fixture = home_and_cwd();
        let source = fixture.cwd.join("source");
        let template = fixture.cwd.join("template");
        let control_dest = fixture.cwd.join("control-dest");
        let hardened_dest = fixture.cwd.join("hardened-dest");
        let filter_marker = fixture.cwd.join("template-filter-ran");
        let hook_marker = fixture.cwd.join("template-hook-ran");
        let filter = fixture.cwd.join("template-filter.sh");

        std::fs::create_dir_all(&source).expect("create source repo");
        run(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&source),
            "init template source",
        );
        std::fs::write(source.join(".gitattributes"), "payload filter=gv827\n")
            .expect("write template attack attributes");
        std::fs::write(source.join("payload"), "payload contents\n")
            .expect("write template attack payload");
        run(
            Command::new("git").args(["add", "."]).current_dir(&source),
            "stage template source",
        );
        run(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "template source",
                ])
                .current_dir(&source),
            "commit template source",
        );

        std::fs::create_dir_all(template.join("hooks")).expect("create template hooks");
        std::fs::write(
            &filter,
            format!(
                "#!/bin/sh\nprintf 'RAN\\n' > '{}'\nexec /bin/cat\n",
                filter_marker.display()
            ),
        )
        .expect("write template filter");
        let hook = template.join("hooks/post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf 'RAN\\n' > '{}'\n", hook_marker.display()),
        )
        .expect("write template hook");
        for program in [&filter, &hook] {
            let mut permissions = std::fs::metadata(program).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
            std::fs::set_permissions(program, permissions).unwrap();
        }
        std::fs::write(
            template.join("config"),
            format!(
                "[filter \"gv827\"]\n\tsmudge = {}\n\trequired = true\n",
                filter.display()
            ),
        )
        .expect("write template config");
        std::fs::write(
            fixture.home.join(".gitconfig"),
            format!("[init]\n\ttemplateDir = {}\n", template.display()),
        )
        .expect("write operator template config");

        let env = vec![
            ("PATH", "/usr/bin:/bin".to_string()),
            ("HOME", fixture.home.to_string_lossy().into_owned()),
            ("GIT_TEMPLATE_DIR", template.to_string_lossy().into_owned()),
        ];
        let transfer_policy = network_policy(&fixture.home, &fixture.cwd, 9418);
        let checkout =
            crate::sandbox::CheckoutPolicy(network_policy(&fixture.home, &fixture.cwd, 9418));
        let source_arg = source.to_string_lossy();
        let control_arg = control_dest.to_string_lossy();
        let control_transfer = spawn::command_async(
            &transfer_policy,
            &fixture.cwd,
            &[
                "clone",
                "--no-checkout",
                "--",
                source_arg.as_ref(),
                control_arg.as_ref(),
            ],
        )
        .pinned_env_for_test(&env)
        .output()
        .await
        .expect("unhardened transfer starts");
        assert!(
            control_transfer.status.success(),
            "unhardened transfer failed: {}",
            String::from_utf8_lossy(&control_transfer.stderr)
        );
        let mut control_checkout =
            network_command_without_credential(&checkout, &control_dest, &["checkout", "-f"]);
        control_checkout.0 = control_checkout.0.pinned_env_for_test(&env);
        let control_checkout = control_checkout
            .output()
            .await
            .expect("control checkout starts");
        assert!(
            control_checkout.status.success(),
            "control checkout failed: {}",
            String::from_utf8_lossy(&control_checkout.stderr)
        );
        assert!(
            filter_marker.exists() && hook_marker.exists(),
            "template-seeded repository config and hook must both execute in the control"
        );

        std::fs::remove_file(&filter_marker).expect("clear filter marker");
        std::fs::remove_file(&hook_marker).expect("clear hook marker");

        let hardened_arg = hardened_dest.to_string_lossy();
        let hardened_transfer = network_command_with_credential(
            &transfer_policy,
            &fixture.cwd,
            &[
                "clone",
                "--no-checkout",
                "--",
                source_arg.as_ref(),
                hardened_arg.as_ref(),
            ],
            None,
        )
        .pinned_env_for_test(&env)
        .output()
        .await
        .expect("hardened transfer starts");
        assert!(
            hardened_transfer.status.success(),
            "hardened transfer failed: {}",
            String::from_utf8_lossy(&hardened_transfer.stderr)
        );
        assert!(
            !hardened_dest.join(".git/hooks/post-checkout").exists(),
            "production transfer copied an operator-selected hook"
        );
        let cloned_config = std::fs::read_to_string(hardened_dest.join(".git/config"))
            .expect("read cloned repository config");
        assert!(
            !cloned_config.contains("gv827"),
            "production transfer copied operator-selected filter config"
        );

        let mut hardened_checkout =
            network_command_without_credential(&checkout, &hardened_dest, &["checkout", "-f"]);
        hardened_checkout.0 = hardened_checkout.0.pinned_env_for_test(&env);
        let hardened_checkout = hardened_checkout
            .output()
            .await
            .expect("hardened checkout starts");
        assert!(
            hardened_checkout.status.success(),
            "hardened checkout failed: {}",
            String::from_utf8_lossy(&hardened_checkout.stderr)
        );
        assert_eq!(
            std::fs::read(hardened_dest.join("payload")).unwrap(),
            b"payload contents\n"
        );
        assert!(
            !filter_marker.exists() && !hook_marker.exists(),
            "template-seeded executable state survived the production transfer"
        );
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
    /// captured process output rather than a hand-built `Output`: an
    /// unforced repo-local credential helper — a real subprocess, run by real
    /// git — prints a secret-bearing URL to its own stderr, which git forwards
    /// verbatim (measured directly, 2026-08-01, see this file's module doc).
    ///
    /// Paired positive/negative in one test, same captured bytes: the raw
    /// output is asserted to contain the secret first (the census would
    /// have found it), then the redacted output is asserted not to (proving
    /// the assertion below is capable of failing, not just of passing
    /// against text the secret was never in).
    #[tokio::test]
    async fn a_repo_credential_helper_is_blocked_and_its_unforced_output_is_redactable() {
        let server = Http401::start();
        let fixture = home_and_cwd();
        let repo = fixture.cwd.clone();
        run(
            Command::new("git").args(["init", "-q"]).current_dir(&repo),
            "git init",
        );

        let secret_url = "https://s3cr3t-token:hunter2@leaked-host.invalid/org/repo.git";
        let marker = repo.join("helper-ran");
        let helper = repo.join("helper.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho RAN >> {}\necho 'debug: tried {secret_url}' >&2\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write helper");
        let mut perm = std::fs::metadata(&helper).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&helper, perm).unwrap();
        let policy = network_policy(&fixture.home, &repo, server.port);
        let url = format!("http://127.0.0.1:{}/repo.git", server.port);
        let scoped_helper = format!("credential.{url}.helper");
        run(
            Command::new("git")
                .args(["config", &scoped_helper, helper.to_str().unwrap()])
                .current_dir(&repo),
            "git config URL-scoped credential helper",
        );

        let raw = spawn::command_async(&policy, &repo, &["ls-remote", &url])
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
        assert!(
            marker.exists(),
            "paired positive: the unforced repository helper must really have run"
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

        std::fs::remove_file(&marker).expect("remove unforced marker");
        let hardened = network_command(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("git runs through the Network harness");
        assert!(
            !hardened.status.success(),
            "the 401-only remote must not become reachable just because the \
             repository helper is suppressed"
        );
        assert!(
            !marker.exists(),
            "the repository credential helper executed through network_command; \
             stderr={}",
            String::from_utf8_lossy(&hardened.stderr)
        );
    }

    /// A repository can opt an `ext::` URL back into use with
    /// `protocol.ext.allow=always`; the URL then names a shell command
    /// directly. Prove both halves with the same repository and URL: plain
    /// Git really executes the marker, while the production Network launcher
    /// forces the protocol off and leaves no marker behind.
    #[tokio::test]
    async fn a_repo_enabled_ext_transport_never_executes_through_network_command() {
        let server = Http401::start();
        let fixture = home_and_cwd();
        let repo = fixture.cwd.clone();
        run(
            Command::new("git").args(["init", "-q"]).current_dir(&repo),
            "git init",
        );
        run(
            Command::new("git")
                .args(["config", "protocol.ext.allow", "always"])
                .current_dir(&repo),
            "git config protocol.ext.allow",
        );

        let marker = repo.join("ext-transport-ran");
        let program = repo.join("ext-transport.sh");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf 'RAN\\n' >> {}\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write ext transport program");
        let mut perm = std::fs::metadata(&program).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&program, perm).unwrap();

        let policy = network_policy(&fixture.home, &repo, server.port);
        let url = format!("ext::{} %S ignored", program.display());

        let unforced = spawn::command_async(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("plain git runs");
        assert!(
            !unforced.status.success(),
            "the marker transport exits 1, so the premise run must fail"
        );
        assert!(
            marker.exists(),
            "paired positive: repository-enabled ext transport did not run, so \
             the denial assertion below would be vacuous; stderr={}",
            String::from_utf8_lossy(&unforced.stderr)
        );

        std::fs::remove_file(&marker).expect("remove premise marker");
        let hardened = network_command(&policy, &repo, &["ls-remote", &url])
            .pinned_env_for_test(&hermetic_env(&fixture.home))
            .output()
            .await
            .expect("git runs through the Network harness");
        assert!(
            !hardened.status.success(),
            "an ext transport forced to `never` must be rejected"
        );
        assert!(
            !marker.exists(),
            "the repository-enabled ext transport executed through \
             network_command; stderr={}",
            String::from_utf8_lossy(&hardened.stderr)
        );
    }

    fn selector_repo(fixture: &HomeAndCwd) {
        run(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&fixture.cwd),
            "init selector repo",
        );
        // No hooks can account for the observed execution.
        std::fs::create_dir(fixture.cwd.join("no-hooks")).unwrap();
        selector_config(fixture, "core.hooksPath", "no-hooks");
    }

    fn selector_config(fixture: &HomeAndCwd, key: &str, value: &str) {
        run(
            Command::new("git")
                .args(["config", key, value])
                .current_dir(&fixture.cwd),
            "configure selector",
        );
    }

    fn selector_marker(repo: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let program = repo.join(name);
        let marker = repo.join("selector-ran");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf 'RAN\\n' >> '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&program, permissions).unwrap();
        (program, marker)
    }

    /// Run the same attack through every Network constructor and both public
    /// completion paths. The hostile test environment is installed AFTER the
    /// builder, so this also detects an allowlist erased by env replacement.
    async fn assert_selector_blocked(
        fixture: &HomeAndCwd,
        policy: &Policy,
        args: &[&str],
        env: &[(&str, String)],
        marker: &Path,
        protocol: &str,
    ) {
        for mode in [
            "output",
            "spawn",
            "credential-none",
            "credential-some",
            "checkout",
        ] {
            let out = match mode {
                "output" => network_command(policy, &fixture.cwd, args)
                    .pinned_env_for_test(env)
                    .output()
                    .await
                    .unwrap(),
                "spawn" => network_command(policy, &fixture.cwd, args)
                    .pinned_env_for_test(env)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
                    .wait_with_output()
                    .await
                    .unwrap(),
                "credential-none" | "credential-some" => network_command_with_credential(
                    policy,
                    &fixture.cwd,
                    args,
                    (mode == "credential-some").then_some("selector-test-token"),
                )
                .pinned_env_for_test(env)
                .output()
                .await
                .unwrap(),
                "checkout" => {
                    let checkout = crate::sandbox::CheckoutPolicy(policy.clone());
                    // Apply a hermetic profile to the already sealed command;
                    // unlike reapplying hardening here, this cannot mask a
                    // missing constructor pin in production.
                    let mut command =
                        network_command_without_credential(&checkout, &fixture.cwd, args);
                    command.0 = command.0.pinned_env_for_test(env);
                    command.output().await.unwrap()
                }
                _ => unreachable!(),
            };
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !marker.exists(),
                "{mode}: {protocol} marker executed; stderr={stderr}"
            );
            assert!(
                !out.status.success(),
                "{mode}: failing marker/remote unexpectedly succeeded"
            );
            if protocol != "git" {
                assert!(
                    stderr.contains(&format!("transport '{protocol}' not allowed")),
                    "{mode}: expected protocol rejection, got {stderr}"
                );
            } else {
                assert!(
                    stderr.contains("unable to connect to 127.0.0.1"),
                    "{mode}: expected direct connection after proxy suppression, got {stderr}"
                );
            }
        }
    }

    #[tokio::test]
    async fn native_git_proxy_selector_is_blocked() {
        let fixture = home_and_cwd();
        selector_repo(&fixture);
        let (program, marker) = selector_marker(&fixture.cwd, "hostile-proxy");
        selector_config(&fixture, "core.gitProxy", program.to_str().unwrap());
        selector_config(&fixture, "protocol.git.allow", "always");
        // Reserve a port without listening: direct connects fail immediately,
        // and no concurrent fixture can acquire it during either test leg.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        let policy = network_policy(&fixture.home, &fixture.cwd, port);
        let mut env = hermetic_env(&fixture.home);
        env.push(("GIT_ALLOW_PROTOCOL", "git:http:https:ssh:file".into()));
        let url = format!("git://127.0.0.1:{port}/repo");
        let args = ["ls-remote", &url];
        // The naive reset still executes the first matching repository proxy.
        let unforced = spawn::command_async(
            &policy,
            &fixture.cwd,
            &["-c", "core.gitProxy=none", args[0], args[1]],
        )
        .pinned_env_for_test(&env)
        .output()
        .await
        .unwrap();
        assert!(
            marker.exists(),
            "positive control: proxy did not run: {}",
            String::from_utf8_lossy(&unforced.stderr)
        );
        std::fs::remove_file(&marker).unwrap();
        assert_selector_blocked(&fixture, &policy, &args, &env, &marker, "git").await;
        env.push(("GIT_PROXY_COMMAND", program.to_string_lossy().into_owned()));
        assert_selector_blocked(&fixture, &policy, &args, &env, &marker, "git").await;
    }

    #[tokio::test]
    async fn installed_remote_helper_selectors_are_blocked() {
        for selector in ["vcs", "url", "insteadOf", "pushurl", "pushInsteadOf"] {
            let fixture = home_and_cwd();
            selector_repo(&fixture);
            let (_, marker) = selector_marker(&fixture.cwd, "git-remote-gv779");
            selector_config(&fixture, "protocol.gv779.allow", "always");
            selector_config(&fixture, "remote.named.url", "https://example.invalid/repo");
            match selector {
                "vcs" => selector_config(&fixture, "remote.named.vcs", "gv779"),
                "url" => selector_config(&fixture, "remote.named.url", "gv779::repo"),
                "insteadOf" => selector_config(
                    &fixture,
                    "url.gv779::.insteadOf",
                    "https://example.invalid/",
                ),
                "pushurl" => selector_config(&fixture, "remote.named.pushurl", "gv779::repo"),
                "pushInsteadOf" => selector_config(
                    &fixture,
                    "url.gv779::.pushInsteadOf",
                    "https://example.invalid/",
                ),
                _ => unreachable!(),
            }
            // Push needs a source ref before it will dispatch a transport.
            run(
                Command::new("git")
                    .args([
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.invalid",
                        "commit",
                        "-qm",
                        "fixture",
                        "--allow-empty",
                    ])
                    .current_dir(&fixture.cwd),
                "create push source",
            );
            let policy = network_policy(&fixture.home, &fixture.cwd, 9418);
            let env = vec![
                ("PATH", format!("{}:/usr/bin:/bin", fixture.cwd.display())),
                ("HOME", fixture.home.to_string_lossy().into_owned()),
                ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ];
            let args: &[&str] = if selector.starts_with("push") {
                &["push", "named", "HEAD:refs/heads/test"]
            } else {
                &["fetch", "named"]
            };
            // Specific repo policy beats even this command-line default.
            let mut naive = vec!["-c", "protocol.allow=never"];
            naive.extend_from_slice(args);
            let unforced = spawn::command_async(&policy, &fixture.cwd, &naive)
                .pinned_env_for_test(&env)
                .output()
                .await
                .unwrap();
            assert!(
                marker.exists(),
                "{selector} positive control: helper did not run: {}",
                String::from_utf8_lossy(&unforced.stderr)
            );
            std::fs::remove_file(&marker).unwrap();
            assert_selector_blocked(&fixture, &policy, args, &env, &marker, "gv779").await;
        }
    }

    /// #779's allowlist must also cover the *path* shape of a helper
    /// selector, not only the installed-name shape
    /// [`installed_remote_helper_selectors_are_blocked`] covers.
    ///
    /// Git builds a helper's program name by concatenation — `git-remote-` +
    /// the selector's value — and hands the result to `execvp`. POSIX gives
    /// `execvp` a second mode: any name containing a slash is used as a path
    /// directly, with no `PATH` search. So `vcs = ../r1` never asks `PATH`
    /// for an installed `git-remote-../r1`; it resolves `git-remote-../r1`
    /// from the child's cwd — the served worktree — and runs whatever the
    /// repository itself put there.
    ///
    /// "Ordinary tracked content, no install step and no operator
    /// involvement" is the actual claim, so the fixture below `git add`s and
    /// commits the helper rather than just writing it to disk and making an
    /// unrelated `--allow-empty` commit alongside it — a 2026-09-09 review of
    /// an earlier version of this test caught exactly that gap: the helper
    /// existed but was never shown to be something a repository could
    /// plausibly ship.
    ///
    /// That makes this a distinct reachability story from an installed
    /// helper, and it is the shape a separate 2026-09-09 review claimed was
    /// still open after #779. It is not: `GIT_ALLOW_PROTOCOL` rejects
    /// `../r1` as a transport name before Git ever reaches helper dispatch.
    /// This test pins that, because nothing else does — and the argument for
    /// why is subtle enough that re-deriving it from the code is expensive.
    /// (The other four selector routes named alongside `vcs` — `url`,
    /// `insteadOf`, `pushurl`, `pushInsteadOf` — do NOT share this
    /// reachability at all; see
    /// [`path_shaped_url_based_selectors_never_reach_helper_dispatch`] for
    /// why, and do not read this test as implying they do.)
    ///
    /// The positive control runs under Git's *default* protocol policy
    /// rather than a naive `protocol.allow=never`: a user-initiated fetch
    /// permits unknown helper protocols by default, so the marker really
    /// does execute when the pin is absent. A control that denied first
    /// would prove only that the denial worked, never that the attack was
    /// reachable.
    #[tokio::test]
    async fn worktree_relative_remote_helper_selector_is_blocked() {
        let fixture = home_and_cwd();
        selector_repo(&fixture);
        let helper_dir = fixture.cwd.join("git-remote-..");
        std::fs::create_dir(&helper_dir).unwrap();
        let (_, marker) = selector_marker(&helper_dir, "r1");
        selector_config(&fixture, "remote.named.url", "https://example.invalid/repo");
        selector_config(&fixture, "remote.named.vcs", "../r1");
        // The "ordinary tracked content" claim above is only true if the
        // helper is actually tracked: stage and commit it, rather than
        // leaving it as an untracked file on disk next to an unrelated
        // empty commit. (This test only fetches, so unlike
        // installed_remote_helper_selectors_are_blocked's commit, there is
        // no push source ref to supply here.)
        run(
            Command::new("git")
                .args(["add", "--", "git-remote-../r1"])
                .current_dir(&fixture.cwd),
            "stage hostile helper",
        );
        run(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "hostile relative remote helper, tracked in the repository",
                ])
                .current_dir(&fixture.cwd),
            "commit hostile helper",
        );
        let policy = network_policy(&fixture.home, &fixture.cwd, 9418);
        let env = vec![
            ("PATH", format!("{}:/usr/bin:/bin", fixture.cwd.display())),
            ("HOME", fixture.home.to_string_lossy().into_owned()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ];
        let args: &[&str] = &["fetch", "named"];
        // Positive control: DEFAULT protocol policy, no naive denial at all.
        // A user-initiated fetch allows unknown helper protocols by default,
        // so if this attack is real at all, the marker runs here.
        let unforced = spawn::command_async(&policy, &fixture.cwd, args)
            .pinned_env_for_test(&env)
            .output()
            .await
            .unwrap();
        assert!(
            marker.exists(),
            "positive control: helper did not run: {}",
            String::from_utf8_lossy(&unforced.stderr)
        );
        std::fs::remove_file(&marker).unwrap();
        assert_selector_blocked(&fixture, &policy, args, &env, &marker, "../r1").await;
    }

    /// Companion to [`worktree_relative_remote_helper_selector_is_blocked`]:
    /// checks whether `url`, `insteadOf`, `pushurl` and `pushInsteadOf` —
    /// named alongside `vcs` in
    /// [`installed_remote_helper_selectors_are_blocked`] — share `vcs`'s
    /// slash-into-`execvp` reachability, or whether that shape is unique to
    /// `vcs`. A 2026-09-09 review raised exactly this as an open question
    /// about an earlier version of this file: "path-shaped url/insteadOf/
    /// pushurl/pushInsteadOf selectors are not tested and could bypass while
    /// this stays green."
    ///
    /// It is unique to `vcs`, and this test is what actually establishes
    /// that rather than asserting it. All four routes here are parsed by Git
    /// as `<scheme>::<address>`, and Git validates `<scheme>` against
    /// `[A-Za-z0-9.+-]` *before* it will treat the value as a helper
    /// selector at all — verified directly against this build (git 2.53.0,
    /// plain command-line git, no sandbox, no `protocol.allow` override of
    /// any kind): a `/` in that position makes Git read the whole value as a
    /// literal (local) path instead and fail with "does not appear to be a
    /// git repository". `remote.<name>.vcs` has no such charset check — its
    /// value is handed to `execvp` verbatim — which is the actual reason it
    /// alone can carry a path.
    ///
    /// That means there is no "genuinely honoured when unhardened" positive
    /// control to write here the way
    /// [`worktree_relative_remote_helper_selector_is_blocked`] has one: the
    /// premise such a control would demonstrate (Git dispatches a
    /// slash-shaped value to a helper) does not hold for these four routes,
    /// so a control built on that premise could only ever pass by
    /// construction. This test proves the negative directly instead, across
    /// every Network completion path, with the hostile helper sitting
    /// exactly where it would need to be for the vulnerability to exist: if
    /// a future Git version ever loosened that charset, the helper would
    /// start running and this test would fail on `!marker.exists()`. It
    /// also asserts the rejection is never `GIT_ALLOW_PROTOCOL`'s — if the
    /// sandbox's allowlist had to intervene here, that alone would mean Git
    /// silently started dispatching a slash-shaped scheme, which is exactly
    /// the regression this guards against.
    #[tokio::test]
    async fn path_shaped_url_based_selectors_never_reach_helper_dispatch() {
        for selector in ["url", "insteadOf", "pushurl", "pushInsteadOf"] {
            let fixture = home_and_cwd();
            selector_repo(&fixture);
            let helper_dir = fixture.cwd.join("git-remote-..");
            std::fs::create_dir(&helper_dir).unwrap();
            let (_, marker) = selector_marker(&helper_dir, "r1");
            selector_config(&fixture, "remote.named.url", "https://example.invalid/repo");
            match selector {
                "url" => selector_config(&fixture, "remote.named.url", "../r1::repo"),
                "insteadOf" => selector_config(
                    &fixture,
                    "url.../r1::.insteadOf",
                    "https://example.invalid/",
                ),
                "pushurl" => selector_config(&fixture, "remote.named.pushurl", "../r1::repo"),
                "pushInsteadOf" => selector_config(
                    &fixture,
                    "url.../r1::.pushInsteadOf",
                    "https://example.invalid/",
                ),
                _ => unreachable!(),
            }
            // Push needs a source ref before it will dispatch a transport.
            run(
                Command::new("git")
                    .args([
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.invalid",
                        "commit",
                        "-qm",
                        "fixture",
                        "--allow-empty",
                    ])
                    .current_dir(&fixture.cwd),
                "create push source",
            );
            let policy = network_policy(&fixture.home, &fixture.cwd, 9418);
            let env = vec![
                ("PATH", format!("{}:/usr/bin:/bin", fixture.cwd.display())),
                ("HOME", fixture.home.to_string_lossy().into_owned()),
                ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ];
            let args: &[&str] = if selector.starts_with("push") {
                &["push", "named", "HEAD:refs/heads/test"]
            } else {
                &["fetch", "named"]
            };
            // Strongest available "unhardened" baseline: the same sandboxed
            // spawn substrate as every other case in this suite, but
            // WITHOUT `.with_network_transport_policy()` — no
            // `GIT_ALLOW_PROTOCOL` override at all, default Git protocol
            // policy. If Git dispatched this by path the way it does for
            // `vcs`, the marker would run right here.
            let unforced = spawn::command_async(&policy, &fixture.cwd, args)
                .pinned_env_for_test(&env)
                .output()
                .await
                .unwrap();
            let unforced_stderr = String::from_utf8_lossy(&unforced.stderr);
            assert!(
                !marker.exists(),
                "{selector} unhardened: helper ran anyway — Git DID dispatch a slash-shaped \
                 scheme by path, contradicting this test's premise: {unforced_stderr}"
            );
            assert!(
                !unforced.status.success(),
                "{selector} unhardened: unexpectedly succeeded"
            );
            assert!(
                !unforced_stderr.contains("not allowed"),
                "{selector} unhardened: got a protocol-allowlist rejection with no transport \
                 policy applied at all — that shouldn't be possible: {unforced_stderr}"
            );
            // The positive half of this control: prove the selector actually
            // fired, rather than treating "the marker didn't run" as proof by
            // itself. `insteadOf`/`pushInsteadOf` rewrite a URL only when
            // their base matches; a mistyped or non-matching rule silently
            // does nothing, and `remote.named.url`'s untouched
            // `https://example.invalid/repo` then fails for an unrelated
            // reason (DNS resolution) that would satisfy every assertion
            // above without the slash-shaped selector ever having been
            // interpreted at all. Verified directly against this build: all
            // four routes' effective value, when the selector genuinely
            // fires, is the literal string `../r1::repo` in Git's own
            // rejection message; a silently-ignored `insteadOf`/
            // `pushInsteadOf` produces a DNS-failure message with no such
            // substring instead.
            assert!(
                unforced_stderr.contains("../r1::repo"),
                "{selector} unhardened: the selector did not appear to fire at all — expected \
                 Git's own rejection to name the rewritten value `../r1::repo`, got: \
                 {unforced_stderr}"
            );

            assert_url_based_selector_never_reaches_helper(&fixture, &policy, args, &env, &marker)
                .await;
        }
    }

    /// Shared by
    /// [`path_shaped_url_based_selectors_never_reach_helper_dispatch`]: run
    /// the same slash-shaped selector through every Network constructor and
    /// completion path — the same coverage [`assert_selector_blocked`] gives
    /// the installed-name and `vcs` path-shaped cases — but assert the
    /// opposite failure signature: no marker, a failing status, and
    /// **never** the sandbox's `transport '<x>' not allowed` message. That
    /// message appearing here would mean `GIT_ALLOW_PROTOCOL` had to reject
    /// a dispatched helper, which would mean Git's own `<scheme>::` charset
    /// check had already been bypassed — the actual regression this guards
    /// against.
    async fn assert_url_based_selector_never_reaches_helper(
        fixture: &HomeAndCwd,
        policy: &Policy,
        args: &[&str],
        env: &[(&str, String)],
        marker: &Path,
    ) {
        for mode in [
            "output",
            "spawn",
            "credential-none",
            "credential-some",
            "checkout",
        ] {
            let out = match mode {
                "output" => network_command(policy, &fixture.cwd, args)
                    .pinned_env_for_test(env)
                    .output()
                    .await
                    .unwrap(),
                "spawn" => network_command(policy, &fixture.cwd, args)
                    .pinned_env_for_test(env)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
                    .wait_with_output()
                    .await
                    .unwrap(),
                "credential-none" | "credential-some" => network_command_with_credential(
                    policy,
                    &fixture.cwd,
                    args,
                    (mode == "credential-some").then_some("selector-test-token"),
                )
                .pinned_env_for_test(env)
                .output()
                .await
                .unwrap(),
                "checkout" => {
                    let checkout = crate::sandbox::CheckoutPolicy(policy.clone());
                    // Apply a hermetic profile to the already sealed command;
                    // unlike reapplying hardening here, this cannot mask a
                    // missing constructor pin in production.
                    let mut command =
                        network_command_without_credential(&checkout, &fixture.cwd, args);
                    command.0 = command.0.pinned_env_for_test(env);
                    command.output().await.unwrap()
                }
                _ => unreachable!(),
            };
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !marker.exists(),
                "{mode}: helper executed via a slash-shaped `<scheme>::` selector; \
                 stderr={stderr}"
            );
            assert!(
                !out.status.success(),
                "{mode}: failing remote unexpectedly succeeded"
            );
            assert!(
                !stderr.contains("not allowed"),
                "{mode}: sandbox protocol allowlist rejected this — meaning Git DID try to \
                 dispatch a slash-shaped scheme as a transport, contradicting this test's \
                 premise: {stderr}"
            );
            // Same positive-proof requirement as the unhardened control
            // above: without this, a silently-ignored `insteadOf`/
            // `pushInsteadOf` rule would pass every assertion above for the
            // wrong reason (an unrelated DNS failure against the untouched
            // URL), never having exercised the selector this test names.
            assert!(
                stderr.contains("../r1::repo"),
                "{mode}: the selector did not appear to fire at all — expected Git's own \
                 rejection to name the rewritten value `../r1::repo`, got: {stderr}"
            );
        }
    }
}
