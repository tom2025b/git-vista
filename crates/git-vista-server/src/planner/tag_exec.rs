//! The two **local** tag executors — create (plain, annotated, or signed) and
//! delete — plus the signed-tag execution path and its failure taxonomy
//! (M2.21d #238, M2.21e #239, ADR 0048).
//!
//! # Why this is its own module
//!
//! Signing is the one place the planner shells out to a program it does not
//! control (`gpg`, or an ssh signer) on a write path, so these executors own
//! machinery nothing else in the planner consumes: the bounded signing spawn
//! ([`run_signed_tag`] under [`SIGN_TIMEOUT`]), the closed classification of
//! its failures ([`classify_sign_failure`], with the [`gpg_on_path`] probe),
//! and the typed `SignTagError` refusal contract. The remote-reaching tag
//! executors are a different shape entirely and live in
//! [`super::remote_tags`] — see that module's doc.
//!
//! # A source-scanning test anchors in THIS file
//!
//! `tag_signing_suite`'s
//! `the_timed_out_arms_recovery_read_uses_the_bounded_primitive` reads this
//! file's source and locates [`run_signed_tag`]'s body between the literal
//! anchors `async fn run_signed_tag(` and [`classify_sign_failure`]'s first
//! doc line, to prove the `TimedOut` arm's recovery read still calls the
//! bounded primitive. Renaming either item, or moving one of them out of
//! this file, moves that test's anchors — keep them together here.

use std::path::Path;
use std::process::Output;

use axum::http::StatusCode;

use git_vista_protocol::{CommitOid, SignTagError, SignTagFailureKind, TagAnnotation, TagName};

use git_vista_core::activity::ActivityKind;

use crate::sandbox::NetworkNeed;

use super::{
    couldnt_run, journal_app_event, rev_parse_ref_unpeeled, run_git, short, stderr_or, Obs,
    Observed,
};

/// The argv for one [`GitOperation::CreateTag`](git_vista_protocol::GitOperation::CreateTag) — pulled out of
/// [`exec_create_tag`] as a pure function so the **no-editor guarantee** can
/// be asserted over the exact bytes that reach `execve`, without a repository,
/// a spawn, or an environment.
///
/// Two properties this function exists to make checkable, both of which are
/// the whole of ADR 0048's create half:
///
///  * **`-m <message>` is present whenever `-a` or `-s` is.** `git tag -a`
///    (or `-s`, which implies `-a`) with no message writes `.git/TAG_EDITMSG`
///    and launches `core.editor`; on a headless server there is no editor and
///    nobody to type into one, so that process either dies on a
///    `true`-shaped editor or waits forever. There is no `--no-edit` on
///    `git tag` to close this after the fact — the only defence is never to
///    build the argv that asks for it.
///  * **`--edit` is never present.** It would re-open the editor even with
///    `-m` given.
///
/// The type system already makes the bad case unrepresentable — an annotated
/// tag *is* a [`TagAnnotation`], which cannot exist without a non-empty
/// [`TagMessage`] — so this function has no failure mode to encode. That is
/// the point: the guarantee is structural, and this is where it is visible.
/// It holds for the signed arm too: [`TagAnnotation::sign`] lives *inside*
/// the same struct, so a signed request is still, unconditionally, a request
/// with a message.
///
/// `-a` is passed explicitly for the unsigned annotated case even though
/// `-m` alone would imply it, so the argv says which kind of tag it is
/// building rather than relying on a git implication a reader would have to
/// know. The signed case asks for `-s` instead of `-a` — `-s` already
/// implies `-a` (git accepts both together, but that would just repeat the
/// same implication this function otherwise avoids relying on), so the two
/// are mutually exclusive here, not additive.
///
/// # No gpg flags belong in this argv, ever
///
/// It is tempting to think a `-c gpg.program="gpg --batch --pinentry-mode
/// cancel"`-shaped override could force gpg to fail fast instead of prompting.
/// It cannot: git execs `gpg.program` directly (`use_shell` is never set in
/// git's own `gpg-interface.c`), so a program string containing spaces execs
/// a binary *literally named* `"gpg --batch --pinentry-mode cancel"`, which
/// does not exist, and the tag creation fails for the wrong reason before
/// gpg ever runs. The only way to inject gpg-side flags would be a wrapper
/// executable, which needs its own exec grant from the sandbox — out of
/// scope for #239, and exactly the kind of "fix the sandbox" move this
/// slice's issue explicitly forbids. This is also why the bounded timeout in
/// [`exec_create_tag`]'s signed arm is load-bearing rather than decorative:
/// with no gpg-side flag reachable at all, non-interactivity rests entirely
/// on the sandbox's own denials (see that function's doc comment) plus this
/// bound as the backstop.
pub(super) fn create_tag_argv<'a>(
    name: &'a TagName,
    target: &'a CommitOid,
    annotation: Option<&'a TagAnnotation>,
) -> Vec<&'a str> {
    match annotation {
        None => vec!["tag", name.as_str(), target.as_str()],
        Some(a) if a.sign => vec![
            "tag",
            "-s",
            "-m",
            a.message.as_str(),
            name.as_str(),
            target.as_str(),
        ],
        Some(a) => vec![
            "tag",
            "-a",
            "-m",
            a.message.as_str(),
            name.as_str(),
            target.as_str(),
        ],
    }
}

/// `git tag [-a|-s -m <message>] <name> <target>` (`/api/tag`).
///
/// Lightweight, annotated and signed are one operation with one argv builder
/// ([`create_tag_argv`]) rather than three executors: everything after the
/// argv — the journal entry, the success response — is identical for the
/// unsigned shapes, and the *plan* already told the reviewer which kind they
/// approved (an annotated create's [`RefState::Computed`](git_vista_protocol::RefState::Computed) after-state versus
/// a lightweight one's exact `At(target)`). Only the signed shape's *failure*
/// path diverges, because it is the one shape whose failure is not "git
/// refused a bad request" but "the environment could not do what was asked" —
/// see [`classify_sign_failure`]'s doc comment.
///
/// # Why signing is attempted at all, and why it is expected to fail here
///
/// M2.21d (#238) answered every `sign: true` request with a `501` before
/// building any argv. M2.21e (#239) replaces that with a real attempt,
/// because a refusal that never runs `git tag -s` can never tell a user
/// *why* signing does not work for them, and "why" is exactly what this
/// server can say precisely: its own sandbox is what closes the door. See
/// [`classify_sign_failure`] for the mechanism and [`SIGN_TIMEOUT`] for the
/// bound that makes a fast, honest failure the *only* outcome — never a
/// hang, and never raw gpg stderr reaching the client.
pub(super) async fn exec_create_tag(
    repo: &Path,
    need: NetworkNeed,
    name: &TagName,
    target: &CommitOid,
    annotation: Option<&TagAnnotation>,
) -> (StatusCode, String) {
    let annotated = annotation.is_some();
    let signed = annotation.is_some_and(|a| a.sign);
    let args = create_tag_argv(name, target, annotation);

    let output = if signed {
        match run_signed_tag(repo, need, &args, name).await {
            Ok(output) => output,
            Err(refusal) => return refusal,
        }
    } else {
        match run_git(repo, need, &args).await {
            Ok(o) => o,
            Err(e) => return couldnt_run("/api/tag", &e),
        }
    };

    if !output.status.success() {
        if signed {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let kind = classify_sign_failure(&stderr, gpg_on_path());
            // The raw stderr goes to the server log ONLY — never to the
            // client. This is the deliberate break from the B3
            // verbatim-forwarding posture the unsigned arm keeps just below:
            // gpg's own text is untranslated-nowhere, version-dependent
            // noise a browser-only user cannot act on, and forwarding it is
            // the issue's named second-worst outcome after a hang.
            eprintln!("git-vista: /api/tag signing failed ({kind:?}): {stderr}");
            return sign_refusal_body(kind, sign_failure_message(kind));
        }
        // B3 posture, same as `/api/branch`: git owns ref-name validation and
        // the "already exists" refusal, and its stderr is forwarded verbatim.
        let msg = stderr_or(&output, "git tag failed.");
        eprintln!("git-vista: /api/tag failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let kind = if annotated {
        "annotated"
    } else {
        "lightweight"
    };
    println!(
        "[/api/tag] created {kind}{} tag '{name}' at {}",
        if signed { " signed" } else { "" },
        short(target.as_str())
    );
    // The ref's resulting value, read back rather than assumed: for an
    // annotated tag it is the *tag object* git just wrote, an oid nothing
    // could have known at plan time (which is exactly why the plan's
    // after-state is `RefState::Computed`). `Obs::Unknown` if the read fails —
    // journalled as unknown, never silently as the target.
    let new = Obs::from_read(rev_parse_ref_unpeeled(repo, &format!("refs/tags/{name}")).await);
    journal_app_event(
        repo,
        // `git-vista-core`'s `ActivityKind` has no tag member; `Other` is the
        // honest existing bucket (the same one `/api/discard-tracked-paths`
        // uses) and the summary carries the detail. A `TagCreated` kind is a
        // core-crate widening, not this slice's.
        ActivityKind::Other,
        Some(format!("refs/tags/{name}")),
        Obs::Absent, // a created tag has no previous value, by definition
        new,
        format!("created {kind} tag ‘{name}’"),
    )
    .await;
    (StatusCode::OK, format!("Created {kind} tag '{name}'."))
}

/// The wall-clock ceiling on a signing spawn — the backstop behind every
/// argument in [`run_signed_tag`]'s doc comment for why it *shouldn't* be
/// needed. Ten seconds is generous for a local, keyless failure (which
/// resolves in well under a second — measured, not assumed, by
/// `a_signing_attempt_with_no_usable_key_fails_fast_with_a_typed_reason`
/// below) and still short enough that the per-worktree mutation guard
/// [`plan_and_execute_in`](super::plan_and_execute_in) holds across this call is never held for long.
pub(super) const SIGN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run a signed `git tag -s` under [`SIGN_TIMEOUT`], turning a bound-elapsed
/// timeout into the same typed refusal shape [`exec_create_tag`]'s own
/// failure branch builds for a completed-but-unsuccessful spawn — so this is
/// the one place that knows to check whether a killed child's ref write
/// landed anyway before deciding what to tell the client.
///
/// # This cannot hang — argued path by path
///
/// A signing `git tag -s` shells out to `gpg`, which in turn needs
/// `gpg-agent` — over an `AF_UNIX` socket — for anything that touches the
/// secret key: producing the signature itself, and, on a terminal, a
/// pinentry prompt for a passphrase. Every step that could block on input is
/// closed before this function's timeout is ever needed:
///
///  * **The secret key is unreachable, and this is the fast, common
///    failure.** `~/.gnupg` sits in `sandbox::DEFAULT_SECRET_EXCLUDES`, the
///    same Landlock exclude set that hides `~/.ssh` — so gpg's key lookup
///    fails before it gets anywhere near an agent, regardless of what the
///    server's real keyring holds. This is the modal case a client will see.
///  * **The agent is unreachable independently.** `CreateTag` declares
///    `NetworkNeed::Local`, which resolves to `Tier::Strict`
///    (`sandbox::policy_for`), and Strict denies `socket(2)`/`socketpair(2)`
///    for `AF_UNIX` outright (`seccomp_filter.rs`'s `af_unix_rule`, whose own
///    doc names `gpg-agent` as one of the sockets this closes). gpg's first
///    step toward an agent is exactly that syscall; the kernel answers
///    `EPERM` synchronously, so there is no file descriptor to block on and
///    no connection to wait for. An auto-launched `gpg-agent` inherits the
///    same filter across `fork`/`execve` and fails the identical way.
///  * **No editor can open.** `create_tag_argv` never omits `-m` when
///    signing — a [`TagAnnotation`] cannot exist without a non-empty
///    message — and `GIT_EDITOR=true` is set process-wide at startup
///    (`main.rs`), so there is no argv shape and no environment shape that
///    reaches `$EDITOR`.
///  * **No credential or terminal prompt from git itself.** `CreateTag` is
///    local, and `GIT_TERMINAL_PROMPT=0` (`main.rs`) closes that regardless.
///  * **Stdin cannot carry a prompt.** git pipes the tag payload to gpg on a
///    pipe it controls, never a terminal — and [`crate::git_cmd::git_output_bounded`],
///    which this function calls, additionally nulls the spawn's own stdin,
///    so even an inherited terminal fd cannot reach the child. `GPG_TTY` is
///    also cleared process-wide at startup (`main.rs`), so a stray ttyname
///    cannot route a pinentry attempt anywhere even if one somehow launched.
///
/// None of that is a proof that survives every future GnuPG version, host
/// configuration, or sandbox change — an internal retry loop's own bound
/// inside gpg-agent's startup code, for one, is not this codebase's to
/// guarantee. Nor does the AF_UNIX bullet above hold for every *tier*: it is
/// `sandbox::tier_for`'s `(false, NetworkNeed::Local) => Tier::Strict` arm —
/// an **operator-trusted** repository instead gets `(true, _) => Unsandboxed`
/// (no seccomp, no Landlock), where AF_UNIX is open and a graphical pinentry
/// keying off `DISPLAY`/`WAYLAND_DISPLAY` is not closed by the cleared
/// `GPG_TTY` or the nulled stdin above. That path is unreachable *today* —
/// `sandbox::trust::grant`, the only marker writer `is_trusted` can ever see,
/// is `#[cfg(test)]`-gated with no production caller — but it is one future
/// operator-trust handler away from existing, and the bullets above say
/// nothing about it. So: [`crate::git_cmd::git_output_bounded`] wraps the
/// spawn in [`SIGN_TIMEOUT`] with `kill_on_drop`, and that bound is the
/// **primary** guarantee this function makes, not a backstop behind the
/// bullets above — it is what makes "never hangs" true in Unsandboxed too,
/// where none of the sandbox-specific reasoning applies at all. The bullets
/// above explain why the *common* failure is fast and its reason nameable;
/// the timeout is what makes the property hold regardless of whether any of
/// them turn out to be wrong, or inapplicable, on a given repository.
async fn run_signed_tag(
    repo: &Path,
    need: NetworkNeed,
    args: &[&str],
    name: &TagName,
) -> Result<Output, (StatusCode, String)> {
    match crate::git_cmd::git_output_bounded(repo, args, need, SIGN_TIMEOUT).await {
        Err(e) => Err(couldnt_run("/api/tag", &e)),
        Ok(crate::git_cmd::BoundedOutput::Completed(output)) => Ok(output),
        Ok(crate::git_cmd::BoundedOutput::TimedOut) => {
            eprintln!(
                "git-vista: /api/tag signing on '{name}' did not finish within \
                 {SIGN_TIMEOUT:?} and was killed"
            );
            // The kill races git's own ref write, so the honest next step is
            // to look rather than assume either outcome. This read MUST use
            // git_output_bounded, not rev_parse_ref_unpeeled's plain
            // git_output: it runs on a repository that just proved a git
            // child can block past SIGN_TIMEOUT, and a bare
            // `tokio::time::timeout` around an unbounded spawn does not kill
            // that spawn — per git_output_bounded's own doc, dropping the
            // future without kill_on_drop detaches the child rather than
            // stopping it, leaving it running unobserved. Without a spawn
            // that is actually killed, a hung repo turns a bounded signing
            // failure into an unbounded recovery read — the mutation guard
            // this function runs under (coordinator::lock in execute()) is
            // still held here, so that would hold it forever, undoing the
            // entire point of SIGN_TIMEOUT above. Same bound, reused rather
            // than a second constant: the property is "the whole function
            // returns within SIGN_TIMEOUT", not "each half does".
            let ref_name = format!("refs/tags/{name}");
            let tail = match crate::git_cmd::git_output_bounded(
                repo,
                &["rev-parse", "--verify", "--quiet", &ref_name],
                need,
                SIGN_TIMEOUT,
            )
            .await
            {
                Ok(crate::git_cmd::BoundedOutput::Completed(out)) if out.status.success() => {
                    format!("Warning: refs/tags/{name} now exists — inspect it before trusting it.")
                }
                Ok(crate::git_cmd::BoundedOutput::Completed(_)) => {
                    "The tag was not created.".to_string()
                }
                Ok(crate::git_cmd::BoundedOutput::TimedOut) | Err(_) => format!(
                    "Couldn't confirm whether refs/tags/{name} was created — the check itself \
                     didn't finish in time. Run `git tag -l {name}` on the server to be sure."
                ),
            };
            Err(sign_refusal_body(
                SignTagFailureKind::TimedOut,
                &format!(
                    "Signing didn't finish within {} seconds and was stopped so this \
                     repository wouldn't stay locked. {tail}",
                    SIGN_TIMEOUT.as_secs()
                ),
            ))
        }
    }
}

/// Map a failed signing spawn's stderr onto the closed [`SignTagFailureKind`]
/// set, using GnuPG's own machine-readable `[GNUPG:] …` status-fd protocol —
/// never git's or gpg's human-facing prose, which is gettext-translated and
/// changes wording across versions. Git captures the whole status-fd stream
/// into its own stderr on a signing failure (`gpg-interface.c` invokes gpg
/// with `--status-fd=2`), so every line this function looks for is really
/// there to find.
///
/// `gpg_on_path` disambiguates the two cases with no `[GNUPG:]` line at all.
///
/// Measured directly (plain gpg 2.4.4, no sandbox, both a missing and a
/// permission-denied `GNUPGHOME`): every invocation that actually runs —
/// including ones that fail on a keydb resource error before ever reaching
/// key lookup — emits at least one `[GNUPG:] ERROR …` or `[GNUPG:] FAILURE …`
/// line. Git captures the whole status-fd stream into its own stderr on a
/// signing failure (`gpg-interface.c` invokes gpg with `--status-fd=2`), so
/// completely empty stderr from a gpg that IS on `PATH` means gpg was
/// prevented from running its protocol engine at all — plausibly stopped by
/// the sandbox before it could write anything, the same shape
/// [`AgentUnreachable`](SignTagFailureKind::AgentUnreachable) already names —
/// rather than "ran and failed for some unrecognised reason", which is what
/// [`Other`](SignTagFailureKind::Other) is for. Non-empty, unrecognised
/// stderr still falls to `Other`: that case really did produce output this
/// classifier could not place, and guessing beyond it would be exactly the
/// kind of assumption this codebase's standing caution warns against
/// ("measure sandbox behaviour; never assert it") — this fallback is not
/// verified against the real sandboxed spawn's empty-stderr shape, only
/// against gpg's own behaviour outside it, and is a best-effort narrowing on
/// that basis rather than a proven mapping.
///
/// The two GnuPG status codes this cares about (from libgpg-error's own
/// `err-codes.h.in`, not guessed): `17` is `GPG_ERR_NO_SECKEY`, and `77`/`78`
/// (`GPG_ERR_NO_AGENT`/`GPG_ERR_AGENT`) plus the `257..=281` libassuan IPC
/// range (which includes `259`, `ASS_CONNECT_FAILED`) all mean the same
/// thing from this server's own vantage point: the sandbox denied the
/// connection [`AgentUnreachable`](SignTagFailureKind::AgentUnreachable)
/// exists to name.
pub(super) fn classify_sign_failure(stderr: &str, gpg_on_path: bool) -> SignTagFailureKind {
    let mut saw_status_line = false;
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[GNUPG:] FAILURE ") {
            saw_status_line = true;
            if let Some(code) = rest
                .split_whitespace()
                .last()
                .and_then(|s| s.parse::<i64>().ok())
            {
                return match (code as u64) & 0xFFFF {
                    17 => SignTagFailureKind::NoSecretKey,
                    77 | 78 => SignTagFailureKind::AgentUnreachable,
                    257..=281 => SignTagFailureKind::AgentUnreachable,
                    _ => SignTagFailureKind::Other,
                };
            }
        } else if line.starts_with("[GNUPG:] INV_SGNR") {
            return SignTagFailureKind::NoSecretKey;
        } else if line.starts_with("[GNUPG:]") {
            saw_status_line = true;
        }
    }
    if !saw_status_line && !gpg_on_path {
        SignTagFailureKind::GpgNotInstalled
    } else if !saw_status_line && stderr.trim().is_empty() {
        SignTagFailureKind::AgentUnreachable
    } else {
        SignTagFailureKind::Other
    }
}

/// Whether an executable named `gpg` exists anywhere on the server's own
/// `PATH` — a pure filesystem walk, never a spawn, so it cannot itself hang
/// or need a sandbox policy. Only consulted by [`classify_sign_failure`] when
/// there was no `[GNUPG:]` status line to read at all, which is the one
/// ambiguous case: "gpg was never invoked" and "gpg ran and failed before
/// printing anything" look identical from stderr alone.
fn gpg_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .iter()
        .any(|dir| dir.join("gpg").is_file())
}

/// The client-facing sentence for each [`SignTagFailureKind`] — closed-set
/// prose, never git's or gpg's own words. [`SignTagFailureKind::TimedOut`]'s
/// real message is built at its one call site in [`run_signed_tag`], where
/// the ref's post-kill state is known; the text here exists only so this
/// match stays exhaustive and is never actually shown.
fn sign_failure_message(kind: SignTagFailureKind) -> &'static str {
    match kind {
        SignTagFailureKind::NoSecretKey => {
            "gpg couldn't find a secret key to sign with — and on this server that is \
             expected: the sandbox that runs git deliberately hides ~/.gnupg (the same \
             boundary that hides ~/.ssh), so signing can't work here even with a \
             perfectly good key. This is not a problem with your configuration. The tag \
             was NOT created. Create it unsigned, or follow #74 for real signing support."
        }
        SignTagFailureKind::AgentUnreachable => {
            "Signing is blocked by this server's sandbox: local git operations have no \
             access to gpg-agent's socket, by design (the same boundary tracked for \
             ssh-agent under #188). gpg could not reach the agent and never will as the \
             sandbox is built today. This is not a problem with your keys or your gpg \
             setup. The tag was NOT created. Create it unsigned, or follow #74 for \
             signing support."
        }
        SignTagFailureKind::GpgNotInstalled => {
            "gpg isn't installed on the server, so git has nothing to sign with. The tag \
             was NOT created. Install GnuPG on the server, or create the tag unsigned."
        }
        SignTagFailureKind::Other => {
            "Signing failed for a reason outside the set this server recognises. The tag \
             was NOT created. The full detail is in the server log."
        }
        SignTagFailureKind::TimedOut => "Signing didn't finish in time and was stopped.",
    }
}

/// Build a signed `/api/tag` refusal's `(StatusCode, String)` — the typed
/// [`SignTagError`] JSON, serialized into the same shared prose channel every
/// other operation's executor returns. #323 already taught `middleware`'s
/// `rewrap_error` to recognise a JSON *object* body and pass it through with
/// `application/json` set rather than re-wrapping it as escaped text inside
/// an `ApiError`, so this needs no `Response`-returning sibling the way
/// [`amend_refusal`](super::commit_exec::amend_refusal)/[`amend_refusal_body`](super::commit_exec::amend_refusal_body) split into two — one plain
/// function covers both this pipeline's callers and any future direct one.
fn sign_refusal_body(kind: SignTagFailureKind, message: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        serde_json::to_string(&SignTagError {
            kind,
            message: message.to_string(),
        })
        .expect("SignTagError serialization cannot fail"),
    )
}

/// `git tag -d <name>` (`/api/delete-tag`) — the **local** delete only;
/// nothing here reaches a remote (`NetworkNeed::Local`, ADR 0036).
///
/// `observed.branch_tip` is the tag ref's **unpeeled** pre-delete value, read
/// by [`observe_operation`](super::observe_operation) before anything was touched. It is the same value
/// the plan's compare-and-swap precondition pinned and the same value
/// [`RecoveryStrategy::RecreateTag`](git_vista_protocol::RecoveryStrategy::RecreateTag) carries, and it is what is journalled as
/// the old oid — so the one number a human or a recovery path needs is
/// recorded from the observation, not re-read after the ref is gone (there
/// would be nothing left to read: tag refs keep no reflog).
pub(super) async fn exec_delete_local_tag(
    repo: &Path,
    need: NetworkNeed,
    name: &TagName,
    observed: &Observed,
) -> (StatusCode, String) {
    let output = match run_git(repo, need, &["tag", "-d", name.as_str()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/delete-tag", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git tag -d failed.");
        eprintln!("git-vista: /api/delete-tag failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    println!("[/api/delete-tag] deleted tag '{name}'");
    journal_app_event(
        repo,
        ActivityKind::Other,
        Some(format!("refs/tags/{name}")),
        observed.branch_tip.clone(),
        Obs::Absent, // the tag is gone: its new value is a real absence
        format!("deleted tag ‘{name}’"),
    )
    .await;
    (StatusCode::OK, format!("Deleted tag '{name}'."))
}
