//! Thin `git -C <repo> …` command wrappers shared across the route handlers.
//!
//! Split out of `main.rs`: every handler that shells out to git for a small,
//! reusable step goes through one of these. They deliberately do nothing clever —
//! run git, interpret the exit status/output — so the B3 posture (git does the
//! work, we forward its own error text) stays consistent everywhere. All are
//! `pub(crate)`; the handlers in `crate::handlers` are their only callers.
//!
//! The read side is *bounded*: `git_stdout_capped` streams git's stdout into a
//! buffer that never reserves more than the caller's cap, drains stderr
//! concurrently under its own 64 KiB cap, and kills + reaps the child the moment
//! the cap is full. A repository with a 5 GiB blob or a pathological diff can
//! therefore cost the server a bounded allocation and a bounded lifetime instead
//! of whatever git felt like printing (M1.10, #63).
//!
//! `git_cat_file_batch` is the same posture for a request that needs up to two
//! answers from one commit's tree (a spec, and its `^` parent-fallback): one
//! `git cat-file --batch` process is held open across both, and the type each
//! answer resolves to is read from the protocol's own header field — before
//! any content byte is read — rather than from a second spawn (#221).

use std::path::Path;
use std::process::{Output, Stdio};

use axum::http::StatusCode;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// The fail-safe ceiling behind the uncapped [`git_stdout`] wrapper: a caller
/// that does not think about size still cannot make git allocate more than this.
const DEFAULT_GIT_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// How much of git's stderr we keep for the error message. The drain always runs
/// to EOF — an undrained stderr pipe deadlocks a chatty git — but only the first
/// 64 KiB is retained; nothing past that would ever reach a useful error string.
const STDERR_CAPTURE_CAP: usize = 64 * 1024;

/// Scratch read size. Independent of the retained buffer: this bounds one
/// `read()` syscall, never the allocation the cap governs.
const READ_CHUNK: usize = 64 * 1024;

/// Outcome of a bounded read: the retained bytes, and whether git had more to
/// say once the cap was full. `truncated` is a *byte-level* fact established by
/// the reader; nothing downstream should re-derive it from a decoded length.
struct Capped {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Read `reader` into a buffer that never reserves more than `cap` bytes.
///
/// Growth is geometric but clamped to the cap, so a 40-byte answer costs one
/// [`READ_CHUNK`]-derived step (128 KiB) rather than the 8 MiB the cap would
/// allow, and an endless answer costs exactly the cap — never more, whatever
/// git decides to print. When the cap fills exactly, one probe byte distinguishes
/// "the output *was* `cap` bytes" from "there is more" — without it those two
/// cases are indistinguishable and every exactly-cap-sized read would lie.
async fn read_to_cap<R>(mut reader: R, cap: usize) -> std::io::Result<Capped>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK.min(cap.max(1))];
    while bytes.len() < cap {
        let want = (cap - bytes.len()).min(chunk.len());
        let read = reader.read(&mut chunk[..want]).await?;
        if read == 0 {
            return Ok(Capped {
                bytes,
                truncated: false,
            });
        }
        let needed = bytes.len() + read;
        if bytes.capacity() < needed {
            let target = bytes.capacity().max(READ_CHUNK).saturating_mul(2).min(cap);
            bytes.reserve_exact(target.max(needed) - bytes.len());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let mut probe = [0u8; 1];
    let truncated = reader.read(&mut probe).await? != 0;
    Ok(Capped { bytes, truncated })
}

/// Drain a child's stderr to EOF, retaining at most [`STDERR_CAPTURE_CAP`].
/// Draining is not optional: git blocks writing a long error when nobody reads
/// the pipe, and that deadlock is indistinguishable from a slow repository.
async fn drain_stderr(mut stderr: tokio::process::ChildStderr) -> Vec<u8> {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                if kept.len() < STDERR_CAPTURE_CAP {
                    let room = STDERR_CAPTURE_CAP - kept.len();
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    }
}

/// "We could not run/read git at all" — the spawn/IO failure shape the handlers
/// have always returned.
fn io_error(endpoint: &str, e: std::io::Error) -> (StatusCode, String) {
    eprintln!("git-vista: {endpoint} couldn't run git: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Couldn't run git: {e}"),
    )
}

/// "git ran and refused" — forward git's own stderr as the reason (B3 posture),
/// falling back to a generic line only when git said nothing at all.
fn git_error(endpoint: &str, stderr: &[u8]) -> (StatusCode, String) {
    let msg = String::from_utf8_lossy(stderr).trim().to_string();
    let msg = if msg.is_empty() {
        "git failed.".to_string()
    } else {
        msg
    };
    eprintln!("git-vista: {endpoint} failed: {msg}");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// Run `git -C <repo> <args…>` and return at most `cap` bytes of its stdout plus
/// whether it was cut short.
///
/// Bytes, not `String` — paths in `-z` listings aren't guaranteed UTF-8, and the
/// parsers handle that themselves. A cap hit is a *success*: the child is killed
/// and reaped, and its (killed) exit status is deliberately not reinterpreted as
/// a git error. Only the below-cap path inspects the exit status.
/// Build a `git -C <repo> <args…>` command that runs **through the M1.13b
/// sandbox** (#66, Task 5/6).
///
/// `args` is taken here, not appended by the caller, and that is the whole
/// point. Until Task 5 this function passed an **empty** slice to
/// `command_async` and handed back a bare `Command` that each caller then
/// appended the real subcommand to — so the argv `sandbox_argv` classified was
/// never the argv that ran (C10 hazard #1). The returned
/// [`SandboxedCommand`](crate::sandbox::spawn::SandboxedCommand) has no `arg`,
/// `args` or `env` method, so the classified argv is now the executed argv by
/// construction rather than by convention.
///
/// This is the single seam that makes the sandbox load-bearing: every git the
/// server runs goes through here, and `argv_boundary.rs` proves nothing else in
/// the crate spawns git directly. A policy that cannot be built (a missing shim,
/// an unset `$HOME`, a `.git` that fails D2's resolution/managed-root check) is
/// a hard error rather than a silent fall-back to unsandboxed git — an
/// unsandboxed spawn is exactly what this exists to prevent.
///
/// # `read_only` is derived here; `NetworkNeed` is **declared** by the caller
///
/// D2 (#66, Task 7) made `sandbox::policy_for` take `read_only` and `need`.
/// `read_only` is still recovered right here — it comes from
/// `state::read_only_for_path`, a catalog lookup keyed on `repo` itself. Every
/// caller already resolved `repo` from either the catalog
/// (`resolve_repo`/`resolve_target`) or a path with no catalog entry at all (an
/// unregistered test fixture, a not-yet-registered clone destination, a
/// degraded-mode selection) — the lookup returns `false` for the latter, which
/// is the same "no restriction recorded" answer those paths already got before
/// D2, so nothing already working changes shape.
///
/// `need` is different, and Task 8/D3 is what changed it. It used to be
/// recovered here too, from `network_need(args)` — the argv classifier that
/// function's own doc comment describes as "a fail-closed fallback, not the
/// authoritative dispatch". That was harmless only because `policy_for`
/// discarded it. Now that `need` picks the tier, the authority moves to the
/// caller: `declared` is passed in, and `network_need(args)` is demoted to a
/// cross-check (`sandbox::reconcile_need`) that may only tighten, never widen.
///
/// See [`git_output`] and [`git_stdout_capped`] for where each of this crate's
/// callers gets its declaration from.
/// **git could not be run at all.**
///
/// The third answer that the three predicate-shaped helpers below
/// ([`rev_parse`], [`is_ancestor`], [`git_ref_exists`]) used to have no way to
/// give. Their old `Option`/`bool` returns had exactly two states — "git ran
/// and the thing is there" and "git ran and it is not" — so a sandbox policy
/// that could not be built, or a spawn that failed, was laundered into the
/// *second* one. A missing shim then reached the user as "no such branch", and
/// a failed read of a ref reached the staleness gate as a fact about the
/// repository.
///
/// D5 (#66, Task 19) gives that case its own value. It is deliberately opaque
/// — callers do not branch on *why* git could not run, only on *that* it could
/// not — and it carries the underlying reason for the log line and the 500 body.
///
/// Two distinct failures are folded in here on purpose, because every caller
/// treats them identically: the sandbox policy failing to build (a missing
/// shim, an unset `$HOME`, a `.git` geometry D2 refuses) and the spawn/wait
/// itself failing. Both mean the same thing to a gate — *we did not observe
/// anything, so we may not act as though we did.*
#[derive(Debug, Clone)]
pub(crate) struct ExecUnavailable {
    why: String,
}

impl ExecUnavailable {
    pub(crate) fn new(why: impl Into<String>) -> Self {
        Self { why: why.into() }
    }
}

impl std::fmt::Display for ExecUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for ExecUnavailable {}

/// # `NetworkNeed::Remote` goes through the #228 askpass-hardening harness
///
/// A `Remote`-declared spawn is built via
/// [`crate::sandbox::network_exec::network_command`], not the bare
/// `spawn::command_async` every other tier uses — that is the one place
/// `-c core.askpass=` gets spliced ahead of `args`, closing the M1.13
/// finding-I5 `core.askpass` RCE gap on every Network-tier spawn this
/// chokepoint composes (`network_exec.rs`'s module doc has the full
/// reasoning). This is the single seam every git the server runs goes
/// through, so wiring the hardening in here — rather than in each caller —
/// is what makes "every fetch/pull/push exec function" get it by
/// construction instead of by each call site remembering to ask for it.
fn sandboxed(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
) -> Result<crate::sandbox::spawn::SandboxedCommand, String> {
    let read_only = crate::state::read_only_for_path(repo);
    let need = crate::sandbox::reconcile_need(declared, args);
    let policy = crate::sandbox::policy_for(repo, read_only, need).map_err(|e| e.to_string())?;
    Ok(if need == crate::sandbox::NetworkNeed::Remote {
        crate::sandbox::network_exec::network_command(&policy, repo, args)
    } else {
        crate::sandbox::spawn::command_async(&policy, repo, args)
    })
}

/// [`sandboxed`] plus **one** extra read-write grant.
///
/// # Two callers, and why each needs this
///
/// `git worktree add` (M11.04, #549, ADR 0118) and `git worktree remove`
/// (M11.05, #550, ADR 0120) are the two spawns whose target is outside the
/// repository they run in — a new desk's directory, or a sibling's — which
/// `policy_for` never grants: it grants the served repository (and its
/// commondir, for a linked worktree) and the fixed system trees, nothing
/// else. Without the extra grant, neither spawn could write where it needs
/// to even after every other check has passed.
///
/// # It composes `policy_for` rather than replacing it — the inverse of clone
///
/// `policy_for_clone` is an *independent* constructor precisely because clone
/// has no repository to look a trust flag up for, and must never be able to
/// reach `Tier::Unsandboxed`. Both worktree-add and worktree-remove are the
/// opposite case: each has a repository, and each must inherit **exactly**
/// that repository's tier, trust state, hook mode and secret excludes —
/// anything else would mean an operation on a repository ran under a policy
/// that repository never earned. So this takes the policy the operation
/// would otherwise have had and adds one grant to it, changing nothing else.
///
/// # `grant` is never request-derived, for either caller
///
/// `AddWorktree`'s grant is always `state::worktrees_root()` — a constant of
/// the installation, resolved from the environment at startup, never
/// assembled from anything a client sent; the client supplies a
/// [`WorktreeName`](git_vista_protocol::WorktreeName), which cannot hold a
/// separator, a `..`, a leading dot or an absolute path, and the server joins
/// it to that root itself. `RemoveWorktree`'s grant is the canonical path a
/// **fresh worktree census** just resolved for a client-submitted opaque id
/// — proven, at the moment this runs, to be `Serviceable::Yes` and therefore
/// already inside this application's own allowed roots, exactly like every
/// path this server already admits into the catalog. Neither caller ever
/// lets a request choose `grant` directly; if either did, this function would
/// be a way to hand any directory on the filesystem to a git spawn — so each
/// call site is its own whole safety argument, pinned as such rather than
/// left to inspection.
///
/// # Refuses to combine a grant with the network tier
///
/// A grant plus an open socket is a strictly larger surface than either
/// alone, and no caller needs both.
fn sandboxed_with_grant(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
    grant: &Path,
) -> Result<crate::sandbox::spawn::SandboxedCommand, String> {
    let read_only = crate::state::read_only_for_path(repo);
    let need = crate::sandbox::reconcile_need(declared, args);
    if need == crate::sandbox::NetworkNeed::Remote {
        return Err(
            "a command that needs an extra filesystem grant may not also reach the network"
                .to_string(),
        );
    }
    let mut policy =
        crate::sandbox::policy_for(repo, read_only, need).map_err(|e| e.to_string())?;
    policy.rw_trees.push(grant.to_path_buf());
    Ok(crate::sandbox::spawn::command_async(&policy, repo, args))
}

/// [`git_output`] for `git worktree add` (M11.04, #549), whose destination is
/// always the managed worktrees root. See [`sandboxed_with_grant`] for why
/// the grant is safe and why it is not a general-purpose escape hatch.
pub(crate) async fn git_output_in_managed_root(
    repo: &Path,
    args: &[&str],
    grant: &Path,
) -> std::io::Result<Output> {
    sandboxed_with_grant(repo, args, crate::sandbox::NetworkNeed::Local, grant)
        .map_err(std::io::Error::other)?
        .output()
        .await
}

/// [`git_output`] for `git worktree remove` (M11.05, #550) against a resolved
/// sibling. See [`sandboxed_with_grant`] for why the grant is safe and why it
/// is not a general-purpose escape hatch.
pub(crate) async fn git_output_with_extra_grant(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
    grant: &Path,
) -> std::io::Result<Output> {
    sandboxed_with_grant(repo, args, declared, grant)
        .map_err(std::io::Error::other)?
        .output()
        .await
}

/// Run `git -C <repo> <args…>` through the sealed launcher and collect its
/// full [`Output`] — the one path from "a module needs git's `Output`" to
/// `sandboxed` above, for callers that want status/stdout/stderr together
/// rather than the capped-stdout or bool/Option shapes the other helpers in
/// this file return (#66, Task 6).
///
/// Folding "the sandbox policy couldn't be built" into the same `io::Error`
/// as "the spawn itself failed" is a real conflation — it is exactly the one
/// `docs/sandbox/tier-dispatch-revised-design.md`'s D5 exists to fix, by
/// giving execution-unavailable its own value instead of erasing it into a
/// generic IO failure. This helper does not do that work; it takes the
/// erased shape on purpose. It is still fail-safe: every caller today already
/// maps an `io::Error` from git to the same 500 it would map a "policy
/// unavailable" error to, so the two failures land on the same response
/// either way. And it is strictly better than what it replaces — the raw
/// `Command::new("git")` spawns this collapses into itself ran with no
/// sandbox at all.
/// # Declares `NetworkNeed::Local` (D3)
///
/// Not a fallback and not a guess: this arity exists for the callers that have
/// **no typed `GitOperation`** in scope, and every one of them is a local
/// command by inspection —
///
/// * `coordinator::absolute_git_dir` — `rev-parse --absolute-git-dir`;
/// * `durable::write_recovery_ref` — `update-ref <ref> <oid>`;
/// * `handlers::read::worktree_status` — `status --porcelain=v2 --branch`.
///
/// None of them reaches a remote, so `Local` is the truthful declaration rather
/// than a conservative default. The planner, which *does* have a typed
/// operation, does not use this arity: it goes through [`git_output_for`] with
/// the need `sandbox::network_need_for_operation` derived from the operation
/// itself. If a future caller of this function ever needs a remote, it must use
/// `git_output_for` — declaring `Remote` explicitly — because the cross-check
/// in `sandboxed` will otherwise fire on it, which is the intended way to find
/// out.
pub(crate) async fn git_output(repo: &Path, args: &[&str]) -> std::io::Result<Output> {
    git_output_for(repo, args, crate::sandbox::NetworkNeed::Local).await
}

/// [`git_output`] with the network need stated explicitly — the arity the
/// planner uses, where the declaration comes from the typed `GitOperation`
/// being executed rather than from this file's knowledge of its callers.
///
/// `Remote`-declared calls get their captured `Output` passed through
/// [`crate::sandbox::network_exec::redact_output`] before it reaches the
/// caller — the redaction half of #228's deliverable, applied at the same
/// chokepoint [`sandboxed`] applies the askpass hardening at, so nothing
/// downstream (a response body, a log line, a journal record built from this
/// `Output`) can see an unredacted secret without this function's caller
/// deliberately reaching around it.
pub(crate) async fn git_output_for(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
) -> std::io::Result<Output> {
    let cmd = sandboxed(repo, args, declared).map_err(std::io::Error::other)?;
    let output = cmd.output().await?;
    Ok(redact_if_remote(output, declared))
}

/// What [`git_output_bounded`] returns: either the spawn completed inside its
/// wall-clock budget (an ordinary [`Output`], success or failure, exactly as
/// [`git_output_for`] would hand back), or the budget elapsed first and the
/// child was killed before it ever produced one.
///
/// A dedicated enum rather than `Option<Output>` on purpose: `None` reads as
/// "there is no output", which is true either way at the type level but
/// invites a caller to treat a timeout as just another kind of empty
/// response. `TimedOut` is a distinct fact — the process was still running
/// and was stopped — and callers that must tell it apart from "git ran and
/// failed" (a mutation guard that needs to know how long it was really held,
/// a typed wire reason that must not claim git ran to completion) get that
/// for free from the match instead of reconstructing it from an absence.
pub(crate) enum BoundedOutput {
    Completed(Output),
    TimedOut,
}

/// [`git_output_for`] with a wall-clock bound and a severed stdin — for
/// spawns that may shell out to something whose own waiting is not under
/// this server's control, the way `git tag -s` shells out to `gpg` and,
/// through it, potentially to `gpg-agent`.
///
/// Two things beyond the bound itself: `.stdin(Stdio::null())` closes the one
/// fd `git_output_for`'s plain `cmd.output()` leaves at its default
/// (`Stdio::inherit()`, tokio's own default when nothing sets it) — so a
/// child that tries to read a prompt from stdin gets immediate EOF rather
/// than whatever the server process's own stdin happens to be. `.kill_on_drop(true)`
/// is what makes the timeout actually a timeout rather than a detach:
/// dropping the `cmd.output()` future when [`tokio::time::timeout`] elapses
/// sends a `SIGKILL` instead of leaving the process running unobserved.
///
/// **What that SIGKILL actually reaches is more indirect than "the child",
/// singular.** [`sandboxed`] wraps every spawn in `bwrap`, so the process
/// `kill_on_drop` directly signals is **`bwrap`**, not git — captured by
/// strace, not assumed. git, and gpg beneath it, are grandchildren inside
/// the sandbox's own PID namespace. Whether killing `bwrap` reaps that whole
/// tree — rather than leaving git/gpg orphaned and still running — is a
/// property of the sandbox *tier*, not of `kill_on_drop` itself:
/// [`crate::sandbox::lifecycle::strict_reaps_a_double_forked_setsid_orphan_that_the_network_tier_does_not`]
/// measures, via a control/subject pair, that the Strict tier's PID
/// namespace reaps exactly this shape of orphan (a double-forked,
/// `setsid`-detached grandchild) on supervisor kill — and that the Network
/// tier does **not**. `run_signed_tag`'s `CreateTag` path always resolves to
/// `NetworkNeed::Local`, which maps to `Tier::Strict` for the untrusted-repo
/// case that test proves reaping for. The one case that test does not cover:
/// an **operator-trusted** repository resolves to `Tier::Unsandboxed`
/// (`sandbox::tier_for`'s `(true, _)` arm) with no sandbox at all, where this
/// reasoning is void and only the timeout — not the reaping — bounds
/// anything. See `run_signed_tag`'s own doc comment for why that path is
/// unreachable in production today.
///
/// Callers that need to know whether a killed child's partial work left a
/// trace behind (a half-written ref, say) must check for it themselves —
/// killing a process does not undo what it already did before the signal
/// landed.
pub(crate) async fn git_output_bounded(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
    limit: std::time::Duration,
) -> std::io::Result<BoundedOutput> {
    let cmd = sandboxed(repo, args, declared)
        .map_err(std::io::Error::other)?
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(result) => Ok(BoundedOutput::Completed(redact_if_remote(
            result?, declared,
        ))),
        Err(_elapsed) => Ok(BoundedOutput::TimedOut),
    }
}

/// The redact-or-pass-through decision [`git_output_for`] and
/// [`git_output_with_stdin`] both make, pulled out as a pure function so it
/// is directly unit-testable against a hand-built `Output` — no process
/// spawn, no fake `git` on `PATH`, nothing that needs a sandbox policy or a
/// substituted environment. That matters here specifically: `git_output_for`
/// itself takes no env/stdio configuration (production deliberately runs
/// with the server's real environment — see [`sandboxed`]'s doc), so a test
/// that wanted to drive a fake `git` through the *whole* function would have
/// to mutate `PATH` process-wide, which races every other test in this
/// binary under `cargo test`'s default parallel-threads-one-process model
/// (`sandbox::argv::SSH_AUTH_SOCK_LOCK` documents the identical hazard for a
/// different variable). Splitting the decision out avoids needing that at
/// all: the two callers are then thin enough to trust by inspection, and
/// this function carries the actual test coverage.
fn redact_if_remote(output: Output, declared: crate::sandbox::NetworkNeed) -> Output {
    if declared == crate::sandbox::NetworkNeed::Remote {
        crate::sandbox::network_exec::redact_output(output)
    } else {
        output
    }
}

/// [`git_output_for`] with `input` written to the child's stdin first — the
/// arity `git apply --cached` needs (M2.17b, #213): the patch travels as
/// process input, never as an argv element or a temp file. Same sealed
/// launcher; stdin is written in full and closed (dropped) before the output
/// is collected, the same write-then-close discipline
/// [`git_cat_file_batch`]'s protocol uses, minus the interleaved reads a
/// one-shot filter like `apply` doesn't need. `output()` on the wait side
/// drains stdout/stderr concurrently, so a chatty child cannot wedge.
pub(crate) async fn git_output_with_stdin(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
    input: &[u8],
) -> std::io::Result<Output> {
    use tokio::io::AsyncWriteExt;
    let mut child = sandboxed(repo, args, declared)
        .map_err(std::io::Error::other)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(std::io::Error::other("git stdin was not piped"));
    };
    // The write runs concurrently with the output collection: if the child
    // ever produced output faster than it drained stdin, a sequential
    // write-then-wait could deadlock on full pipe buffers. `apply` is quiet,
    // but the shape should not depend on that.
    let bytes = input.to_vec();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    });
    let output = child.wait_with_output().await;
    let _ = writer.await;
    // Same redaction posture as `git_output_for` — see [`redact_if_remote`].
    // No current caller declares `Remote` here (`stage_selection`'s `apply
    // --cached` is always `Local`), but the arity is generic over
    // `declared`, so this keeps that promise true rather than leaving it
    // true only by accident.
    output.map(|o| redact_if_remote(o, declared))
}

/// What a [`git_streamed_for`] run ended as.
///
/// `Cancelled` is an **observed** outcome, never an inference: it is set only
/// on the path where this module itself called `kill()`. The exit status of a
/// SIGKILLed child is indistinguishable from several ordinary failures, so a
/// caller that had to guess from `output.status` would guess wrong.
pub(crate) struct StreamedRun {
    /// Everything the child said, redacted exactly as [`git_output_for`]
    /// redacts it. `stderr` is capped at [`STDERR_CAPTURE_CAP`] like every
    /// other reader here — a cancelled fetch can have printed a great deal
    /// of progress by the time it stops.
    pub(crate) output: Output,
    /// True when this function terminated the child because the cancel
    /// signal fired.
    pub(crate) cancelled: bool,
}

/// Spawn `git -C <repo> <args…>` and hand each **stderr record** to `on_line`
/// *as it arrives*, killing the child if `cancel` fires (M2.20c, #229).
///
/// # Why a separate arity rather than a flag on `git_output_for`
///
/// `git_output_for` collects: nothing downstream of it can see a byte until
/// the process has exited. That is right for every git the server ran before
/// now — they finish in milliseconds — and useless for the one that does not.
/// A fetch of a large repository is a minute of silence followed by an answer,
/// and both halves of this slice (live progress, and a cancel that lands
/// mid-transfer) need the child *while it is still running*. Rather than
/// widen the collecting helper with a callback and a kill switch nobody else
/// wants, this is its own function — going through the same [`sandboxed`]
/// chokepoint, so it inherits #228's askpass hardening and the tier
/// classification identically, and applying the same [`redact_if_remote`] to
/// what it hands back.
///
/// # Records, not lines
///
/// git's `--progress` output separates updates with **carriage returns**, not
/// newlines (verified against git 2.43: one `\n`-terminated line can hold a
/// hundred `\r`-separated progress records). Splitting on `\n` alone would
/// deliver one enormous record at the end of each phase — i.e. no live
/// progress at all — so this splits on either.
///
/// Each record is redacted with [`crate::sandbox::network_exec::redact_url_userinfo`]
/// *before* `on_line` sees it, not only in the collected `output`: the live
/// path is a second sink for exactly the same secret shape, and a callback
/// that logs what it is given must not be the hole in #228's redaction.
///
/// # Cancellation
///
/// `cancel` is a `watch<bool>` that latches. When it fires this function
/// SIGKILLs the child and reaps it. **What it kills is the direct child** —
/// the sandbox shim, which has `exec`'d git into the same pid — so `git
/// fetch` itself dies. Any *grandchild* git started (an ssh transport, a
/// credential helper) is not in a process group this function owns and may
/// outlive the kill by however long it takes to notice its parent is gone;
/// that is a documented limitation (ADR 0043), not an oversight.
pub(crate) async fn git_streamed_for<F>(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
    mut on_line: F,
) -> std::io::Result<StreamedRun>
where
    F: FnMut(&str),
{
    let mut child = sandboxed(repo, args, declared)
        .map_err(std::io::Error::other)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If this future is dropped (the detached pipeline task was aborted),
        // the child must not be left running against a real remote.
        .kill_on_drop(true)
        .spawn()?;

    let Some(stderr) = child.stderr.take() else {
        return Err(std::io::Error::other("git stderr was not piped"));
    };
    // stdout is drained concurrently and kept: `git fetch` says little there,
    // but an undrained pipe is a deadlock waiting for a chattier caller.
    let stdout_task = child
        .stdout
        .take()
        .map(|out| tokio::spawn(async move { drain_stdout(out).await }));

    let mut reader = stderr;
    let mut buf = [0u8; READ_CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    let mut captured: Vec<u8> = Vec::new();
    let mut cancelled = false;

    loop {
        let read = tokio::select! {
            // Biased so a cancel that is already latched wins over a stream
            // that has bytes ready: without this, a fetch receiving objects
            // flat out could keep the read arm ready forever and starve the
            // cancel arm.
            biased;
            () = wait_for_cancel(&mut cancel) => {
                cancelled = true;
                let _ = child.start_kill();
                // Fall through to the wait below: the child must still be
                // reaped, and whatever it managed to say is still ours.
                break;
            }
            read = reader.read(&mut buf) => read?,
        };
        if read == 0 {
            break;
        }
        if captured.len() < STDERR_CAPTURE_CAP {
            let room = STDERR_CAPTURE_CAP - captured.len();
            captured.extend_from_slice(&buf[..read.min(room)]);
        }
        pending.extend_from_slice(&buf[..read]);
        emit_records(&mut pending, &mut on_line);
        // `captured` is capped; `pending` must be too. It normally holds one
        // partial record (a git progress record is tens of bytes), but a
        // remote that streams without ever emitting `\r` or `\n` would
        // otherwise grow it without bound — an unbounded allocation driven by
        // a peer, which is the shape every other reader in this file exists
        // to refuse. Flushing what we have as one record keeps the callback
        // fed and the buffer bounded.
        if pending.len() > MAX_PENDING_RECORD {
            let oversized = std::mem::take(&mut pending);
            emit_one(&oversized, &mut on_line);
        }
    }
    // Whatever is left is a partial record; hand it over rather than drop it —
    // a killed child's last words are exactly the interesting ones.
    if !pending.is_empty() {
        let tail = std::mem::take(&mut pending);
        emit_one(&tail, &mut on_line);
    }

    let status = child.wait().await?;

    // After a kill, **nothing more is read**, and that is a correctness point
    // rather than a shortcut. The pipes' write ends are inherited by whatever
    // the child had spawned — a transport helper, a credential helper, an
    // `upload-pack` on the far side of a local remote — and those are
    // processes this function does not own and cannot reap (see the doc
    // comment's grandchild note). Reading to EOF would therefore block until
    // *they* exit, which is exactly the wait the operator just asked to end:
    // a cancel that hangs for as long as the fetch would have taken is not a
    // cancel. Whatever was captured before the kill is the child's last words,
    // and that is what the caller gets.
    let stdout = match stdout_task {
        Some(task) if cancelled => {
            task.abort();
            Vec::new()
        }
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    };

    Ok(StreamedRun {
        output: redact_if_remote(
            Output {
                status,
                stdout,
                stderr: captured,
            },
            declared,
        ),
        cancelled,
    })
}

/// Resolve the moment `cancel` is (or becomes) set, and **never** otherwise.
///
/// The "never" half is load-bearing and easy to get wrong: this sits in a
/// `select!` arm, so a future that resolved immediately when there is nothing
/// to wait on would win the race every iteration and starve the read arm
/// forever. Both no-cancellation cases — no receiver at all (an untracked
/// pipeline run) and every sender dropped — therefore park on
/// `pending()` rather than returning.
async fn wait_for_cancel(cancel: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(rx) = cancel else {
        return std::future::pending().await;
    };
    // Scoped so the `Ref` guard is dropped before any await below.
    if *rx.borrow_and_update() {
        return;
    }
    if rx.wait_for(|c| *c).await.is_err() {
        std::future::pending().await
    }
}

/// Longest run of bytes [`git_streamed_for`] will hold waiting for a record
/// delimiter before flushing it anyway. Generously past any real git progress
/// record (tens of bytes) and past its longest ordinary message.
const MAX_PENDING_RECORD: usize = 64 * 1024;

/// Split `pending` on `\r` / `\n` and hand each complete record to `on_line`,
/// leaving any trailing partial record in the buffer.
fn emit_records<F: FnMut(&str)>(pending: &mut Vec<u8>, on_line: &mut F) {
    let mut start = 0usize;
    for i in 0..pending.len() {
        if pending[i] == b'\r' || pending[i] == b'\n' {
            emit_one(&pending[start..i], on_line);
            start = i + 1;
        }
    }
    if start > 0 {
        pending.drain(..start);
    }
}

/// One record, redacted, skipped if it is empty after trimming.
fn emit_one<F: FnMut(&str)>(record: &[u8], on_line: &mut F) {
    let text = String::from_utf8_lossy(record);
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    on_line(&crate::sandbox::network_exec::redact_url_userinfo(text));
}

/// Drain a child's stdout to EOF under the same cap as [`drain_stderr`].
async fn drain_stdout(mut stdout: tokio::process::ChildStdout) -> Vec<u8> {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                if kept.len() < STDERR_CAPTURE_CAP {
                    let room = STDERR_CAPTURE_CAP - kept.len();
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    }
}

/// Declares `NetworkNeed::Local` for the same reason [`git_output`] does: no
/// production call site can reach a remote. There are **eight**, counted by
/// grepping `git_stdout_capped(` across `crates/`, then discarding this
/// declaration itself and the six `#[cfg(test)]` hits (`content_suite.rs` and
/// the four cap tests at the foot of this file). The "three" this comment used
/// to claim was four short *before* #546 added the eighth:
///
/// * three in `handlers::read::commit_diff_for_repo` — `/api/diff`'s
///   `--name-status`, `--numstat` and `--patch` reads;
/// * `handlers::read::worktree_status_v2_for_repo` — `/api/status/v2`'s
///   `status --porcelain=v2 --branch -z`;
/// * `handlers::read::staging_diff_for_repo` — `/api/staging/diff`;
/// * `handlers::read::spec_diff_for_repo` — `/api/diff/spec`;
/// * [`git_stdout`] below, the fail-safe wrapper that pins
///   [`DEFAULT_GIT_STDOUT_CAP`] for a caller that has not reasoned about size
///   (itself `#[allow(dead_code)]` — it has no callers of its own yet);
/// * `worktree_census`'s `worktree list --porcelain` (M11.01, #546), which
///   has no route on it yet either.
///
/// `/api/file` used to be two more (`cat-file -t` then `git show`) until #221
/// folded them into [`git_cat_file_batch`]'s single held-open process.
///
/// Only the first three are load-bearing for `bounded_read.rs`'s structural
/// scan, which extracts `commit_diff_for_repo`/`file_at_commit_for_repo`'s
/// bodies alone and is deliberately blind to the rest of the file.
pub(crate) async fn git_stdout_capped(
    repo: &Path,
    args: &[String],
    endpoint: &str,
    cap: usize,
) -> Result<(Vec<u8>, bool), (StatusCode, String)> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let child = sandboxed(repo, &borrowed, crate::sandbox::NetworkNeed::Local)
        .map_err(|e| io_error(endpoint, std::io::Error::other(e)))?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| io_error(endpoint, e))?;

    collect_child_stdout_capped(child, endpoint, cap).await
}

/// The owned-child half of [`git_stdout_capped`], split out so the process
/// lifetime is testable without going through the spawn configuration.
///
/// This future **owns** `child` end to end. That is the whole point: dropping
/// the future (an aborted task, a disconnected client, a `select!` losing a
/// race) drops the `kill_on_drop(true)` handle, so git dies with it rather than
/// running on against a reader nobody is waiting for. Tests hand it a
/// pre-spawned `git cat-file --batch` whose stdin writer stays open, which no
/// spawn-and-read helper could ever produce.
async fn collect_child_stdout_capped(
    mut child: tokio::process::Child,
    endpoint: &str,
    cap: usize,
) -> Result<(Vec<u8>, bool), (StatusCode, String)> {
    let Some(stdout) = child.stdout.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdout was not piped"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stderr was not piped"),
        ));
    };
    // Start the stderr drain *before* reading stdout, so neither pipe can wedge
    // the other.
    let stderr_task = tokio::spawn(drain_stderr(stderr));

    let capped = match read_to_cap(stdout, cap).await {
        Ok(capped) => capped,
        Err(e) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            return Err(io_error(endpoint, e));
        }
    };
    match capped {
        Capped {
            bytes,
            truncated: true,
        } => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            Ok((bytes, true))
        }
        Capped {
            bytes,
            truncated: false,
        } => {
            let status = child.wait().await.map_err(|e| io_error(endpoint, e))?;
            let stderr = stderr_task.await.unwrap_or_default();
            if status.success() {
                Ok((bytes, false))
            } else {
                Err(git_error(endpoint, &stderr))
            }
        }
    }
}

/// [`git_stdout_capped`], but the child's **stderr comes back too** (M5.33,
/// #86 review).
///
/// # Why this exists rather than a second invocation
///
/// Some of git's answers are on stderr, not stdout: `--follow`'s
/// `"exhaustive rename detection was skipped due to too many files"` is the
/// one this was added for. The first version of that code ran the command
/// twice — once through `git_stdout_capped` for the output, once through
/// `git_output` to recover the warning — which was wrong three ways at once.
/// It doubled the work; the replay went through a helper with **no cap and no
/// `kill_on_drop`**, so a "bounded, cancellable" read had an unbounded
/// uncancellable twin; and two runs of a history walk are two chances to
/// disagree, so a warning could be attributed to output it did not come from.
///
/// One child, both streams, the same cap and the same `kill_on_drop` as its
/// stdout-only sibling. `stderr` is bounded by [`STDERR_CAPTURE_CAP`] exactly
/// as every other reader here bounds it.
pub(crate) async fn git_stdout_stderr_capped(
    repo: &Path,
    args: &[String],
    endpoint: &str,
    cap: usize,
) -> Result<(Vec<u8>, Vec<u8>, bool), (StatusCode, String)> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = sandboxed(repo, &borrowed, crate::sandbox::NetworkNeed::Local)
        .map_err(|e| io_error(endpoint, std::io::Error::other(e)))?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| io_error(endpoint, e))?;

    let Some(stdout) = child.stdout.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdout was not piped"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stderr was not piped"),
        ));
    };
    // Same ordering as `collect_child_stdout_capped`: drain stderr first, so
    // neither pipe can wedge the other.
    let stderr_task = tokio::spawn(drain_stderr(stderr));

    let capped = match read_to_cap(stdout, cap).await {
        Ok(capped) => capped,
        Err(e) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            return Err(io_error(endpoint, e));
        }
    };
    if capped.truncated {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let errs = stderr_task.await.unwrap_or_default();
        return Ok((capped.bytes, errs, true));
    }
    let status = child.wait().await.map_err(|e| io_error(endpoint, e))?;
    let errs = stderr_task.await.unwrap_or_default();
    if status.success() {
        Ok((capped.bytes, errs, false))
    } else {
        Err(git_error(endpoint, &errs))
    }
}

// ---------------------------------------------------------------------------
// `git cat-file --batch`: one spawn, up to two queries (#221)
//
// `/api/file/{id}/{*path}` used to run two spawns per request — `cat-file -t
// <spec>` to check the object's type, then (only when it was a blob) `git
// show <spec>` for its content — and, when the id's own tree missed, that
// whole pair again against `<id>^:<path>` for the parent-fallback. The batch
// protocol answers both questions (type *and* size) from one header line per
// query, and the process the query runs against stays open across the
// fallback, so the same two answers now cost at most one spawn and two
// stdin lines instead of up to four spawns.

/// Bound on one `cat-file --batch` response *header* line: `<oid> SP <type>
/// SP <size> LF` on a hit, or `<query> SP missing LF` on a miss, where
/// `<query>` echoes back whatever we wrote — including, on a miss, the full
/// requested path. Real hit headers are under 100 bytes; the miss case is the
/// one that grows with client input, so this is sized generously past
/// anything this server's own `{*path}` route segment realistically carries,
/// not tuned to the common case.
const BATCH_HEADER_CAP: usize = 16 * 1024;

/// The outcome of resolving one `<rev>:<path>` spec (falling back to
/// `<rev>^:<path>` when the first is missing) against a single, still-open
/// `git cat-file --batch` process.
#[derive(Debug)]
pub(crate) enum BatchFileRead {
    /// The winning spec named a blob. `bytes` holds its first
    /// `min(size, cap)` bytes; `truncated` is `true` exactly when the
    /// header's own `size` field exceeded `cap` — decided before a single
    /// content byte was read, never by reading past the cap and noticing.
    Blob { bytes: Vec<u8>, truncated: bool },
    /// The winning spec resolved, but not to a blob. `kind` is git's own
    /// type word (`tree`, `commit`, `tag`, …) taken verbatim from the
    /// header — never inferred from, or confused with, any content byte.
    NotABlob { kind: String },
}

/// Resolve `<id>:<path>` (falling back to `<id>^:<path>`) through exactly one
/// `git cat-file --batch` spawn, reading the content only when the winning
/// spec resolves to a blob (#221). Replaces the #168/#169 pair of spawns
/// (`cat-file -t` then `git show`) `file_at_commit_for_repo` used to run: the
/// type this endpoint must check before ever serving content now arrives as
/// the batch protocol's own header field, read and checked before the
/// content bytes that follow it in the very same stream — so "type resolved
/// before content, on every attempt including the fallback" (#168's security
/// property) is enforced by the order fields appear on the wire, not by which
/// of two separate child processes was allowed to run. See
/// [`batch_lookup_with_fallback`] and [`parse_batch_header`] for where that
/// property actually lives, in a form a unit test can drive without spawning
/// git at all.
///
/// `path` is refused before anything is spawned if it contains a `\n`: the
/// wire protocol is one query per stdin **line** (`<spec>\n`), so an embedded
/// newline would not be an illegal byte inside one argv element (as it was
/// for the old per-attempt `cat-file -t <spec>` spawn) — it would silently
/// become *two* query lines, and whatever the second line happened to
/// resolve to could be read back as though it answered the first.
pub(crate) async fn git_cat_file_batch(
    repo: &Path,
    id: &str,
    path: &str,
    cap: usize,
    endpoint: &str,
) -> Result<BatchFileRead, (StatusCode, String)> {
    if path.contains('\n') {
        return Err(io_error(
            endpoint,
            std::io::Error::other("path contains an embedded newline"),
        ));
    }

    let mut child = sandboxed(
        repo,
        &["cat-file", "--batch"],
        crate::sandbox::NetworkNeed::Local,
    )
    .map_err(|e| io_error(endpoint, std::io::Error::other(e)))?
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .map_err(|e| io_error(endpoint, e))?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdin was not piped"),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdout was not piped"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stderr was not piped"),
        ));
    };
    // Same ordering discipline as `collect_child_stdout_capped`: the drain
    // starts before anything reads stdout, so neither pipe can wedge the
    // other.
    let stderr_task = tokio::spawn(drain_stderr(stderr));
    let mut reader = tokio::io::BufReader::new(stdout);

    let outcome = batch_lookup_with_fallback(&mut stdin, &mut reader, id, path, cap).await;

    // This process exists for exactly one request's worth of queries (one or
    // two) and is never reused: always terminate it here, whatever the
    // outcome. `kill`/`wait` on an already-exited child (the fatal-crash
    // case below) are harmless no-ops.
    let _ = child.kill().await;
    let _ = child.wait().await;
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    match outcome {
        Ok(found) => Ok(found),
        Err(BatchLookupError::Io(e)) => Err(io_error(endpoint, e)),
        Err(BatchLookupError::ProcessEnded) => Err(git_error(endpoint, &stderr_bytes)),
        Err(BatchLookupError::Protocol(msg)) => Err(io_error(endpoint, std::io::Error::other(msg))),
        Err(BatchLookupError::BothMissing) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("'{path}' does not exist at {id} or its parent."),
        )),
    }
}

/// Resolve one blob **object id directly** — no `<rev>:<path>` spec, no `^`
/// parent fallback — through a dedicated `git cat-file --batch` spawn (#428).
///
/// This is deliberately not a thin wrapper around [`git_cat_file_batch`]: that
/// function's whole shape exists to build `<id>:<path>` and retry
/// `<id>^:<path>` when the first is missing, because a *file at a commit* can
/// legitimately have moved to the parent (a commit that deleted it). A blob
/// oid names one exact object with no such history to fall back into — a
/// conflict's stage entries are already-resolved blob ids from the index
/// (`ConflictedFile`'s `Stage::Present.oid`), not revisions to walk. Retrying
/// `<oid>^:` would ask git to treat the oid as a commit and silently resolve
/// to a *different, unrelated* object on a coincidental hit — the one thing
/// this endpoint must never do with a fixed identity the caller already
/// picked out of a scan.
///
/// `oid` is validated by the caller (`CommitOid::new`, 40 or 64 lowercase hex)
/// before this is reached — it is not re-checked here — so a bare oid can
/// never be read as `cat-file --batch` revision syntax
/// (`HEAD:secrets.txt`, `:0:path`, `@{u}`): the hex gate admits nothing the
/// batch protocol's `<spec>` grammar treats as anything but a plain object
/// name.
pub(crate) async fn git_cat_file_batch_oid(
    repo: &Path,
    oid: &str,
    cap: usize,
    endpoint: &str,
) -> Result<BatchFileRead, (StatusCode, String)> {
    let mut child = sandboxed(
        repo,
        &["cat-file", "--batch"],
        crate::sandbox::NetworkNeed::Local,
    )
    .map_err(|e| io_error(endpoint, std::io::Error::other(e)))?
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .map_err(|e| io_error(endpoint, e))?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdin was not piped"),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stdout was not piped"),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(io_error(
            endpoint,
            std::io::Error::other("git stderr was not piped"),
        ));
    };
    let stderr_task = tokio::spawn(drain_stderr(stderr));
    let mut reader = tokio::io::BufReader::new(stdout);

    let outcome = batch_query(&mut stdin, &mut reader, oid, cap).await;

    let _ = child.kill().await;
    let _ = child.wait().await;
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    match outcome {
        Ok(BatchQueryOutcome::Found(found)) => Ok(found),
        Ok(BatchQueryOutcome::Missing) => {
            Err((StatusCode::NOT_FOUND, format!("no such object: {oid}")))
        }
        Err(BatchLookupError::Io(e)) => Err(io_error(endpoint, e)),
        Err(BatchLookupError::ProcessEnded) => Err(git_error(endpoint, &stderr_bytes)),
        Err(BatchLookupError::Protocol(msg)) => Err(io_error(endpoint, std::io::Error::other(msg))),
        Err(BatchLookupError::BothMissing) => unreachable!(
            "batch_query never returns BothMissing — only batch_lookup_with_fallback's \
             two-query retry does"
        ),
    }
}

/// Why [`batch_lookup_with_fallback`] (and the [`batch_query`] it drives)
/// could not produce a [`BatchFileRead`].
#[derive(Debug)]
enum BatchLookupError {
    /// The batch process's stdout ended before a complete header line
    /// arrived — the signature of the child itself exiting (typically a
    /// fatal top-level git error, e.g. a path that resolves outside the
    /// repository, which `cat-file --batch` treats as fatal rather than as
    /// an ordinary per-query `missing`). The real reason lives in the
    /// stderr the caller already drained.
    ProcessEnded,
    /// A write or read against the child's pipes failed outright.
    Io(std::io::Error),
    /// A header line was read in full but matched neither of
    /// `cat-file --batch`'s two documented shapes. Not reachable through any
    /// input this server accepts today — a defensive backstop, not a case
    /// any test drives.
    Protocol(String),
    /// Both the direct spec and the `^` parent-fallback spec reported
    /// `missing`.
    BothMissing,
}

/// Resolve `<id>:<path>`, retrying `<id>^:<path>` on the same still-open
/// process when — and only when — the first spec is reported `missing`,
/// never on a type mismatch, which is a resolved, *existing* answer, not a
/// miss (the same distinction #168's two-spawn code drew between an `Err`
/// from `cat-file -t` and an `Ok(kind)` that simply wasn't `"blob"`: a
/// directory that replaced a file must be rejected as itself, never as a
/// route to the parent's file of the same name).
///
/// Generic over the reader/writer so this — the actual fallback decision —
/// is testable against a synthetic in-memory stream, with no real `git`
/// process anywhere in the test.
async fn batch_lookup_with_fallback<W, R>(
    stdin: &mut W,
    reader: &mut R,
    id: &str,
    path: &str,
    cap: usize,
) -> Result<BatchFileRead, BatchLookupError>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    match batch_query(stdin, reader, &format!("{id}:{path}"), cap).await? {
        BatchQueryOutcome::Found(found) => Ok(found),
        BatchQueryOutcome::Missing => {
            match batch_query(stdin, reader, &format!("{id}^:{path}"), cap).await? {
                BatchQueryOutcome::Found(found) => Ok(found),
                BatchQueryOutcome::Missing => Err(BatchLookupError::BothMissing),
            }
        }
    }
}

/// What one [`batch_query`] round-trip resolved to.
#[derive(Debug)]
enum BatchQueryOutcome {
    Found(BatchFileRead),
    Missing,
}

/// One query/answer round-trip on an already-open `cat-file --batch`
/// process: write `<spec>\n`, read its header, and — only when the header
/// says `blob` — read the content that immediately follows. Nothing here
/// spawns or terminates a process; it only speaks the protocol over whatever
/// reader/writer it is given, which is what makes it callable twice against
/// the same process (the fallback) and directly against a synthetic
/// in-memory stream in tests, with identical logic either way — the type
/// check has nowhere else to hide a shortcut.
async fn batch_query<W, R>(
    stdin: &mut W,
    reader: &mut R,
    spec: &str,
    cap: usize,
) -> Result<BatchQueryOutcome, BatchLookupError>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
{
    stdin
        .write_all(spec.as_bytes())
        .await
        .map_err(BatchLookupError::Io)?;
    stdin.write_all(b"\n").await.map_err(BatchLookupError::Io)?;
    stdin.flush().await.map_err(BatchLookupError::Io)?;

    let Some(line) = read_batch_header_line(reader, BATCH_HEADER_CAP)
        .await
        .map_err(BatchLookupError::Io)?
    else {
        return Err(BatchLookupError::ProcessEnded);
    };

    match parse_batch_header(&line).map_err(BatchLookupError::Protocol)? {
        BatchHeader::Missing => Ok(BatchQueryOutcome::Missing),
        BatchHeader::Found { kind, .. } if kind != "blob" => {
            Ok(BatchQueryOutcome::Found(BatchFileRead::NotABlob { kind }))
        }
        BatchHeader::Found { size, .. } => {
            let (bytes, truncated) = read_batch_content(reader, size, cap)
                .await
                .map_err(BatchLookupError::Io)?;
            Ok(BatchQueryOutcome::Found(BatchFileRead::Blob {
                bytes,
                truncated,
            }))
        }
    }
}

/// One parsed `cat-file --batch` response header — the pure, spawn-free unit
/// the #221 security property (type checked from the wire, before content,
/// on every attempt) is provable against directly (see the `parse_batch_*`
/// tests below).
#[derive(Debug, PartialEq, Eq)]
enum BatchHeader {
    /// `<oid> SP <type> SP <size> LF`. `size` is the exact byte length of the
    /// content that immediately follows in the stream, before its own
    /// trailing LF.
    Found { kind: String, size: usize },
    /// `<query> SP missing LF` — nothing is read after this line for this
    /// query.
    Missing,
}

/// Parse one already-read, LF-stripped `cat-file --batch` header line.
///
/// A `missing` line's echoed `<query>` is whatever the caller wrote — not
/// re-validated here, and not needed: the type/size shape is what
/// distinguishes the two grammars, not the query text. A "hit" line's type
/// field is always a fixed word (`blob`/`tree`/`commit`/`tag`) and its size
/// field is always decimal digits, so it can never itself end in the literal
/// six bytes `missing` preceded by a space — that suffix unambiguously means
/// the miss shape, whatever the echoed query contains (including a path
/// component that happens to spell "missing").
fn parse_batch_header(line: &[u8]) -> Result<BatchHeader, String> {
    if line.ends_with(b" missing") {
        return Ok(BatchHeader::Missing);
    }
    let text = std::str::from_utf8(line)
        .map_err(|_| format!("non-UTF-8 batch header ({} bytes)", line.len()))?;
    let mut fields = text.splitn(3, ' ');
    let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next()) else {
        return Err(format!("malformed batch header: {text:?}"));
    };
    if oid.is_empty() || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("malformed batch header oid: {text:?}"));
    }
    let size: usize = size
        .parse()
        .map_err(|_| format!("malformed batch header size: {text:?}"))?;
    Ok(BatchHeader::Found {
        kind: kind.to_string(),
        size,
    })
}

/// Read one `cat-file --batch` header line from `reader`, up to and
/// excluding its terminating LF, bounded to `cap` bytes.
///
/// `Ok(None)` means the stream ended before any LF arrived — indistinguishable
/// at this layer between "wrote nothing" and "wrote a partial line then
/// died", and the caller treats both the same way: stop trusting this
/// process's stdout and go read its exit status and stderr instead. An `Err`
/// means a line's worth of bytes was seen but exceeded `cap` without a
/// terminating LF — a distinct, and today unreachable through any input this
/// server accepts, failure from a clean EOF.
async fn read_batch_header_line<R>(reader: &mut R, cap: usize) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line: Vec<u8> = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(None);
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            if line.len() + pos > cap {
                reader.consume(pos + 1);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "batch header line exceeded the cap",
                ));
            }
            line.extend_from_slice(&buf[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(line));
        }
        let take = buf.len();
        if line.len() + take > cap {
            reader.consume(take);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "batch header line exceeded the cap",
            ));
        }
        line.extend_from_slice(buf);
        reader.consume(take);
    }
}

/// Read a blob's content, capped from the header's own `size` field before a
/// single content byte is read — never by streaming past a limit and
/// noticing (#221's requirement, and the same "refuse from the header"
/// posture the type check above already gets for free from the wire order).
///
/// `size <= cap`: reads the whole object, then its frame's own trailing LF
/// (best-effort — a failure here changes nothing about the content already
/// read, and this process is about to be killed regardless).
/// `size > cap`: reads exactly `cap` bytes — a genuine prefix of the object,
/// not an empty "refused" answer — and stops; the remaining bytes and the
/// frame's LF are deliberately left undrained, since the caller kills this
/// process immediately after and nothing will ever read them.
async fn read_batch_content<R>(
    reader: &mut R,
    size: usize,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if size > cap {
        let mut bytes = vec![0u8; cap];
        reader.read_exact(&mut bytes).await?;
        return Ok((bytes, true));
    }
    let mut bytes = vec![0u8; size];
    reader.read_exact(&mut bytes).await?;
    let mut lf = [0u8; 1];
    let _ = reader.read_exact(&mut lf).await;
    Ok((bytes, false))
}

/// Run `git -C <repo> <args…>` and return its stdout bytes, mapping both spawn
/// failures and non-zero exits to a 500 with git's own stderr as the reason.
/// Deliberately bytes, not String — paths in `-z` listings aren't guaranteed
/// UTF-8, and the parsers handle that themselves.
///
/// The truncation bit is discarded on purpose: this is the *fail-safe* shape for
/// a caller that has not reasoned about size, and it silently stops at
/// [`DEFAULT_GIT_STDOUT_CAP`] rather than letting a pathological repository
/// dictate the allocation. Anything that must know it was cut short — every diff
/// and file read does — calls [`git_stdout_capped`] with its own explicit cap.
#[allow(dead_code)] // D17: future callers fail safe at 8 MiB.
pub(crate) async fn git_stdout(
    repo: &Path,
    args: &[String],
    endpoint: &str,
) -> Result<Vec<u8>, (StatusCode, String)> {
    git_stdout_capped(repo, args, endpoint, DEFAULT_GIT_STDOUT_CAP)
        .await
        .map(|(bytes, _truncated)| bytes)
}

/// Resolve `rev` to a full commit id in `repo`.
///
/// Three answers, not two (D5, #66 Task 19):
///
/// * `Ok(Some(id))` — git ran and `rev` resolves to `id`;
/// * `Ok(None)` — git ran and said `rev` does not resolve (`--verify --quiet`
///   exits non-zero for exactly that). **A fact about the repository.**
/// * `Err(ExecUnavailable)` — git did not run. Not a fact about anything.
///
/// The middle and last used to be the same `None`, and callers read it as the
/// middle one. Used by the journal hooks to capture a ref's tip before/after an
/// operation — e.g. a branch's tip *before* deleting it, which is the one
/// piece of state git itself throws away (the branch's reflog dies with it)
/// and exactly what "Restore branch" later needs.
pub(crate) async fn rev_parse(repo: &Path, rev: &str) -> Result<Option<String>, ExecUnavailable> {
    let spec = format!("{rev}^{{commit}}");
    // Local (D3): resolving a rev reads the object database, never a remote.
    let output = sandboxed(
        repo,
        &["rev-parse", "--verify", "--quiet", &spec],
        crate::sandbox::NetworkNeed::Local,
    )
    .map_err(ExecUnavailable::new)?
    .output()
    .await
    .map_err(|e| ExecUnavailable::new(format!("couldn't run git rev-parse: {e}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!id.is_empty()).then_some(id))
}

/// Whether `ancestor` is an ancestor of (or equal to) `rev` — `git merge-base
/// --is-ancestor` exits 0 exactly then. "HEAD already contains the base tip" is
/// the definition of "a rebase onto that base would change nothing".
/// `Err` when git did not run (D5): "we could not tell" must not read as
/// "no, it is not an ancestor", which is what the old bare `bool` said.
pub(crate) async fn is_ancestor(
    repo: &Path,
    ancestor: &str,
    rev: &str,
) -> Result<bool, ExecUnavailable> {
    // Local (D3): `merge-base` walks the local object graph.
    let out = sandboxed(
        repo,
        &["merge-base", "--is-ancestor", ancestor, rev],
        crate::sandbox::NetworkNeed::Local,
    )
    .map_err(ExecUnavailable::new)?
    .output()
    .await
    .map_err(|e| ExecUnavailable::new(format!("couldn't run git merge-base: {e}")))?;
    Ok(out.status.success())
}

/// Run one `git -C <repo> <args…>` for the reset, mapping any failure to git's
/// own stderr so the response can say which exact step refused and why.
pub(crate) async fn git_ok(repo: &Path, args: &[&str]) -> Result<(), String> {
    // Local (D3): every call site is a local step — the seed reset's
    // `checkout`/`reset`/`clean`/`branch -D`, `bundle unbundle`, and
    // `remote get-url`, which reads `.git/config` and opens no socket.
    let output = sandboxed(repo, args, crate::sandbox::NetworkNeed::Local)?
        .output()
        .await
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("`git {}` failed", args.join(" "))
    } else {
        stderr
    })
}

/// Whether `refname` resolves in `repo` (`git rev-parse --verify --quiet`): exit 0
/// when the ref exists, non-zero otherwise. Used to prefer `origin/main` over the
/// local `main` as a rebase base only when the remote-tracking ref is actually there.
/// `Err` when git did not run (D5): the old bare `bool` reported a missing
/// shim as "the ref is not there", which then silently picked a *different*
/// rebase base.
pub(crate) async fn git_ref_exists(repo: &Path, refname: &str) -> Result<bool, ExecUnavailable> {
    // Local (D3): a ref existence check reads `.git`, never a remote.
    let out = sandboxed(
        repo,
        &["rev-parse", "--verify", "--quiet", refname],
        crate::sandbox::NetworkNeed::Local,
    )
    .map_err(ExecUnavailable::new)?
    .output()
    .await
    .map_err(|e| ExecUnavailable::new(format!("couldn't run git rev-parse: {e}")))?;
    Ok(out.status.success())
}

/// A directory git genuinely cannot be run against, for the D5 tests.
///
/// **Nothing is mocked and no function under test is stubbed.** `.git` is a
/// regular file that is not a `gitdir:` pointer, which is a geometry
/// `sandbox::worktree::linked_worktree_dirs` refuses to classify; that becomes
/// `RepoPathsError::WorktreeGeometry`, which `sandbox::policy_for` returns as
/// an error rather than a policy, so [`sandboxed`] has no command to hand back
/// and no git process is ever spawned. That is a real production failure path
/// (a hostile or corrupt `.git`), reached the way production reaches it.
///
/// Deliberately *not* an env-var override of the shim path: `shim::RESOLVED`
/// is a process-wide `OnceLock`, so an override set by one test would leak
/// into every other test in the binary and could be pre-empted by whichever
/// test resolved the shim first.
#[cfg(test)]
pub(crate) fn unrunnable_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("hostile");
    std::fs::create_dir_all(&repo).expect("create fixture dir");
    std::fs::write(repo.join(".git"), "this is not a gitdir: pointer\n").expect("write .git");
    (dir, repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_fixtures::seeded as seeded_repo;

    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    /// Collect what [`emit_records`] delivers for one buffer, and what it
    /// leaves behind for the next read.
    fn split(bytes: &[u8]) -> (Vec<String>, Vec<u8>) {
        let mut pending = bytes.to_vec();
        let mut out = Vec::new();
        emit_records(&mut pending, &mut |r: &str| out.push(r.to_string()));
        (out, pending)
    }

    /// M2.20c (#229), the "records, not lines" claim: git separates progress
    /// updates with **carriage returns**, and the splitter must deliver each
    /// one as it arrives.
    ///
    /// The premise is asserted in the same test: this buffer — captured from
    /// a real `git fetch --progress` — contains exactly **one** `\n`, so a
    /// `\n`-only splitter would deliver one record at the end of the phase,
    /// i.e. no live progress at all. Three records out of a one-line buffer
    /// is the whole property.
    #[test]
    fn progress_records_are_split_on_carriage_returns_not_only_newlines() {
        let buf = b"remote: Counting objects:  10% (1/10)\rremote: Counting objects:  \
                    20% (2/10)\rremote: Counting objects: 100% (10/10), done.\n";
        assert_eq!(
            buf.iter().filter(|b| **b == b'\n').count(),
            1,
            "the fixture must be a single line, or this proves nothing"
        );
        let (records, pending) = split(buf);
        assert_eq!(records.len(), 3, "{records:?}");
        assert!(records[0].ends_with("(1/10)"), "{}", records[0]);
        assert!(records[2].ends_with("done."), "{}", records[2]);
        assert!(
            pending.is_empty(),
            "a fully-delimited buffer leaves nothing"
        );
    }

    /// A record that has not finished arriving is held, not delivered as a
    /// truncated one — and is delivered whole once its delimiter arrives.
    #[test]
    fn a_partial_record_is_held_until_its_delimiter_arrives() {
        let mut pending = b"Receiving objects:  66% (80/120)\rReceiving objec".to_vec();
        let mut out: Vec<String> = Vec::new();
        emit_records(&mut pending, &mut |r: &str| out.push(r.to_string()));
        assert_eq!(out, vec!["Receiving objects:  66% (80/120)".to_string()]);
        assert_eq!(pending, b"Receiving objec");

        pending.extend_from_slice(b"ts:  67% (81/120)\r");
        out.clear();
        emit_records(&mut pending, &mut |r: &str| out.push(r.to_string()));
        assert_eq!(out, vec!["Receiving objects:  67% (81/120)".to_string()]);
        assert!(pending.is_empty());
    }

    /// #228's redaction covers the **live** path too, not only the collected
    /// `Output` — the callback is a second sink for the same secret shape.
    ///
    /// Premise asserted: the raw record really does carry the literal secret,
    /// so the delivered record lacking it is a fact about redaction rather
    /// than about a fixture that never leaked.
    #[test]
    fn a_streamed_record_is_redacted_before_the_callback_sees_it() {
        const SECRET: &str = "hunter2-streamed";
        let raw = format!("remote: tried https://svc:{SECRET}@leaked.invalid/r.git\n");
        assert!(
            raw.contains(SECRET),
            "the fixture must leak when unredacted"
        );

        let (records, _) = split(raw.as_bytes());
        assert_eq!(records.len(), 1);
        assert!(
            !records[0].contains(SECRET),
            "a credential reached the live callback: {}",
            records[0]
        );
        assert!(
            records[0].contains("leaked.invalid"),
            "redaction must strip the userinfo and keep the rest: {}",
            records[0]
        );
    }

    /// Empty records — the trailing `\n` after a `\r`, or a bare `remote:` —
    /// are dropped rather than delivered as noise the parser would have to
    /// filter itself.
    #[test]
    fn empty_records_are_not_delivered() {
        let (records, _) = split(b"a\r\n\r\rb\n");
        assert_eq!(records, vec!["a".to_string(), "b".to_string()]);
    }

    /// `git <args…>` in `repo`; asserts success. Same shape as the planner
    /// suites' fixtures, duplicated because those helpers are private to
    /// `planner::contract_suite` and unreachable from here.
    fn run(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    /// `git <args…>` in `repo`, returning trimmed stdout; asserts success.
    fn out(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {repo:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Deterministic filler of exactly `len` bytes, built in fixed rows so the
    /// fixture never depends on a shell helper (`yes`/`dd`/`head` are banned by
    /// the argv boundary — every child in these tests is literally `git`).
    fn filler(len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            let row = format!("{:08} bounded-read fixture row\n", bytes.len());
            let take = row.len().min(len - bytes.len());
            bytes.extend_from_slice(&row.as_bytes()[..take]);
        }
        bytes
    }

    /// Write `content` into `repo`'s object database via literal
    /// `git hash-object -w`, returning the blob's object id.
    fn write_blob(repo: &Path, name: &str, content: &[u8]) -> String {
        std::fs::write(repo.join(name), content).unwrap();
        out(repo, &["hash-object", "-w", "--", name])
    }

    /// `git cat-file blob <oid>` — raw blob bytes on stdout, nothing added.
    fn cat_file_args(oid: &str) -> Vec<String> {
        vec!["cat-file".to_string(), "blob".to_string(), oid.to_string()]
    }

    /// A literal `git cat-file --batch` child with piped stdio and kill-on-drop.
    /// It reads object ids from stdin forever, so as long as the writer is held
    /// open it has no reason of its own to exit — which is exactly what makes it
    /// a witness for "who ended this process?".
    fn batch_child(repo: &Path) -> tokio::process::Child {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn git cat-file --batch")
    }

    /// Whether the kernel still has a process-table entry for `pid`.
    fn pid_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Poll `/proc/<pid>` until the entry is gone or `deadline` elapses. The
    /// assertion is deliberately about the *process*, not about a Rust task
    /// finishing: a task that stopped polling proves nothing about the git child
    /// it started.
    async fn wait_for_pid_gone(pid: u32, deadline: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if !pid_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        !pid_alive(pid)
    }

    /// A `PATH` containing nothing but a fake `git` that writes its argv
    /// (unit-separator-joined) to stdout and exits 0 — same technique
    /// `network_exec.rs`'s own argv-shape test uses, duplicated here (rather
    /// than shared) because that helper is private to that file's test
    /// module. Written inside `repo` (already rw-granted by the policy under
    /// test) since a path outside every grant a Network-tier policy makes
    /// cannot be exec'd at all under Landlock.
    fn fake_git_dumper(repo: &Path) -> String {
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

    /// The regression this file's own review found (#228 blocker): a
    /// `NetworkNeed::Remote`-declared spawn used to go through the same bare
    /// `spawn::command_async` every other tier uses, with no `-c
    /// core.askpass=` hardening — so `exec_push` (the one production caller
    /// today) never actually got the askpass RCE closure `network_exec.rs`
    /// builds, even though that harness existed and was fully tested in
    /// isolation. This proves the *wiring*, not just the underlying
    /// mechanism (already proven exhaustively by
    /// `network_exec::https_suite::repo_local_askpass_is_never_executed`):
    /// `sandboxed()` itself, called exactly the way `git_output_for` calls
    /// it, must route a `Remote`-declared spawn through the hardened
    /// launcher.
    #[tokio::test]
    async fn sandboxed_forces_askpass_hardening_for_remote_network_need() {
        let (_dir, repo) = seeded_repo();
        let dumper = fake_git_dumper(&repo);

        let cmd = sandboxed(
            &repo,
            &["ls-remote", "origin"],
            crate::sandbox::NetworkNeed::Remote,
        )
        .expect("policy builds for a Network-tier need");
        let out = cmd
            .pinned_env_for_test(&[("PATH", dumper), ("HOME", std::env::var("HOME").unwrap())])
            .output()
            .await
            .expect("fake git runs");
        let argv_line = String::from_utf8_lossy(&out.stdout);
        let args: Vec<&str> = argv_line
            .trim()
            .trim_end_matches('\u{1f}')
            .split('\u{1f}')
            .collect();
        assert!(
            args.windows(2).any(|w| w == ["-c", "core.askpass="]),
            "sandboxed() did not force askpass hardening for NetworkNeed::Remote; argv={args:?}"
        );
    }

    /// Paired negative: a `Local`-declared spawn (every other tier) must
    /// NOT carry the Network-tier forcing — proves the assertion above is
    /// actually discriminating on `need`, not just always true of every
    /// spawn this fixture produces.
    #[tokio::test]
    async fn sandboxed_does_not_force_askpass_hardening_for_local_network_need() {
        let (_dir, repo) = seeded_repo();
        let dumper = fake_git_dumper(&repo);

        let cmd = sandboxed(
            &repo,
            &["status", "--short"],
            crate::sandbox::NetworkNeed::Local,
        )
        .expect("policy builds for a Local need");
        let out = cmd
            .pinned_env_for_test(&[("PATH", dumper), ("HOME", std::env::var("HOME").unwrap())])
            .output()
            .await
            .expect("fake git runs");
        let argv_line = String::from_utf8_lossy(&out.stdout);
        let args: Vec<&str> = argv_line
            .trim()
            .trim_end_matches('\u{1f}')
            .split('\u{1f}')
            .collect();
        assert!(
            !args.windows(2).any(|w| w == ["-c", "core.askpass="]),
            "a Local-need spawn unexpectedly carried Network-tier askpass forcing; argv={args:?}"
        );
    }

    /// The redaction half of the same wiring: [`redact_if_remote`] — the
    /// exact decision `git_output_for` and `git_output_with_stdin` both
    /// delegate to — must strip URL userinfo from a `Remote`-declared
    /// `Output` and leave a `Local`-declared one byte-for-byte untouched.
    /// No process spawn needed: see `redact_if_remote`'s own doc for why a
    /// fake-`git`-on-`PATH` version of this test would have to mutate `PATH`
    /// process-wide and race every other test in this binary.
    #[test]
    fn redact_if_remote_redacts_only_when_declared_remote() {
        let raw = || Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: b"fatal: https://user:hunter2@host/repo.git unreachable".to_vec(),
        };

        let remote = redact_if_remote(raw(), crate::sandbox::NetworkNeed::Remote);
        let remote_stderr = String::from_utf8_lossy(&remote.stderr);
        assert!(
            !remote_stderr.contains("hunter2"),
            "Remote-declared Output was not redacted: {remote_stderr}"
        );

        // Paired negative: Local-declared output is untouched, AND proves
        // the raw fixture really does carry the secret un-redacted (so the
        // Remote assertion above is capable of failing, not vacuous).
        let local = redact_if_remote(raw(), crate::sandbox::NetworkNeed::Local);
        assert_eq!(
            local.stderr,
            raw().stderr,
            "Local-declared Output must pass through unchanged"
        );
        assert!(String::from_utf8_lossy(&local.stderr).contains("hunter2"));
    }

    /// The ordinary leg: a bound with real room in it lets a fast git command
    /// complete normally — [`BoundedOutput::Completed`], not `TimedOut`,
    /// carrying the same `Output` [`git_output_for`] would have produced.
    /// Runs through [`sandboxed`], the real production chokepoint — not a
    /// mock — exactly like every other test in this module.
    #[tokio::test]
    async fn git_output_bounded_completes_normally_with_a_generous_bound() {
        let (_dir, repo) = seeded_repo();
        let result = git_output_bounded(
            &repo,
            &["status", "--short"],
            crate::sandbox::NetworkNeed::Local,
            std::time::Duration::from_secs(20),
        )
        .await
        .expect("the bounded wrapper builds and runs a policy for a real repo");
        match result {
            BoundedOutput::Completed(out) => {
                assert!(
                    out.status.success(),
                    "stderr={}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            BoundedOutput::TimedOut => panic!("a 20s bound must not fire for `git status`"),
        }
    }

    /// The bound itself, exercised against the real sandboxed spawn path —
    /// not a synthetic hang, a real one: the budget is too small for even a
    /// fast `git status` on a tiny repository to complete inside it, so
    /// [`tokio::time::timeout`] elapses first and `git_output_bounded` must
    /// report [`BoundedOutput::TimedOut`] rather than the test itself
    /// hanging alongside a regression.
    ///
    /// This is the property M2.21e (#239) exists to prove: a bounded spawn
    /// that does not finish in time is reported, not waited on forever. The
    /// outer `tokio::time::timeout` here is the belt to `git_output_bounded`'s
    /// own suspenders — if the function under test ever stopped enforcing
    /// its bound, this test must still fail in bounded time rather than
    /// wedging the suite.
    #[tokio::test]
    async fn git_output_bounded_reports_timed_out_when_the_bound_is_too_tight() {
        let (_dir, repo) = seeded_repo();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            git_output_bounded(
                &repo,
                &["status", "--short"],
                crate::sandbox::NetworkNeed::Local,
                std::time::Duration::from_nanos(1),
            ),
        )
        .await
        .expect(
            "git_output_bounded must return within 15s on its own — it is not this test's \
             job to enforce that bound, only to fail loudly if it is ever missing",
        )
        .expect("the bounded wrapper builds a policy for a real repo");
        assert!(
            matches!(outcome, BoundedOutput::TimedOut),
            "a 1ns bound must elapse before any real git spawn can complete"
        );
    }

    #[tokio::test]
    async fn git_stdout_capped_retains_at_most_cap() {
        let (_dir, repo) = seeded_repo();
        let content = filler(100_000);
        let oid = write_blob(&repo, "big.txt", &content);

        let (bytes, truncated) = git_stdout_capped(&repo, &cat_file_args(&oid), "test", 4096)
            .await
            .unwrap();

        assert_eq!(bytes.len(), 4096);
        assert!(truncated);
        assert_eq!(bytes, content[..4096]);
    }

    #[tokio::test]
    async fn git_stdout_capped_distinguishes_exact_cap_from_over_cap() {
        let (_dir, repo) = seeded_repo();
        let exact = write_blob(&repo, "exact.txt", &filler(4096));
        let over = write_blob(&repo, "over.txt", &filler(4097));

        let (bytes, truncated) = git_stdout_capped(&repo, &cat_file_args(&exact), "test", 4096)
            .await
            .unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(
            !truncated,
            "output of exactly the cap is complete, not truncated"
        );

        let (bytes, truncated) = git_stdout_capped(&repo, &cat_file_args(&over), "test", 4096)
            .await
            .unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(truncated, "one byte past the cap must report truncation");
    }

    #[tokio::test]
    async fn git_stdout_capped_returns_small_output_verbatim() {
        let (_dir, repo) = seeded_repo();
        let content = b"a short blob, well under any cap\n";
        let oid = write_blob(&repo, "small.txt", content);

        let (bytes, truncated) =
            git_stdout_capped(&repo, &cat_file_args(&oid), "test", DEFAULT_GIT_STDOUT_CAP)
                .await
                .unwrap();

        assert_eq!(bytes, content.to_vec());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn git_stdout_capped_preserves_git_errors() {
        let (_dir, repo) = seeded_repo();
        let missing = "0".repeat(40);

        let (status, msg) = git_stdout_capped(
            &repo,
            &cat_file_args(&missing),
            "test",
            DEFAULT_GIT_STDOUT_CAP,
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            msg.contains(&missing),
            "git's own stderr must survive the capped read: {msg}"
        );
        assert_ne!(msg, "git failed.", "the empty-stderr fallback is not a fit");
    }

    // --- D5 (#66, Task 19): execution-unavailable is its own value ----------

    /// The fixture must actually be unrunnable, or every D5 test below is
    /// vacuous — a `Some`/`None` assertion that passes because git quietly
    /// worked. Pinned first, on its own, against a *control*: the same call
    /// against a real repository must succeed, so the `Err` cannot be blamed
    /// on the call itself being broken.
    #[tokio::test]
    async fn the_unrunnable_fixture_really_cannot_run_git() {
        let (_dir, repo) = seeded_repo();
        rev_parse(&repo, "HEAD")
            .await
            .expect("control: git runs in a real repository")
            .expect("control: HEAD resolves");

        let (_hostile_dir, hostile) = unrunnable_repo();
        let err = rev_parse(&hostile, "HEAD")
            .await
            .expect_err("a `.git` the policy cannot resolve must not spawn git");
        assert!(
            !err.to_string().is_empty(),
            "the reason must survive for the log line and the 500 body"
        );
    }

    /// The three-way split, on all three helpers: "git ran and it is not
    /// there" and "git could not run" are different values now. Before D5
    /// both were `None`/`false`.
    #[tokio::test]
    async fn absent_and_unavailable_are_different_answers() {
        let (_dir, repo) = seeded_repo();
        let (_hostile_dir, hostile) = unrunnable_repo();

        // rev_parse: a ref that genuinely does not exist.
        assert!(
            rev_parse(&repo, "refs/heads/no-such-branch")
                .await
                .expect("git ran")
                .is_none(),
            "a missing ref is Ok(None) — a fact"
        );
        assert!(rev_parse(&hostile, "refs/heads/no-such-branch")
            .await
            .is_err());

        // git_ref_exists: same ref, bool shape.
        assert!(
            !git_ref_exists(&repo, "refs/heads/no-such-branch")
                .await
                .expect("git ran"),
            "a missing ref is Ok(false) — a fact"
        );
        assert!(git_ref_exists(&hostile, "refs/heads/no-such-branch")
            .await
            .is_err());

        // is_ancestor: a real negative needs two real commits, so make one.
        run(&repo, &["checkout", "-q", "-b", "side"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        run(&repo, &["add", "b.txt"]);
        run(&repo, &["commit", "-q", "-m", "side"]);
        assert!(
            !is_ancestor(&repo, "side", "main").await.expect("git ran"),
            "`side` is not an ancestor of `main`: Ok(false), a fact"
        );
        assert!(
            is_ancestor(&repo, "main", "side").await.expect("git ran"),
            "and the positive direction still answers Ok(true)"
        );
        assert!(is_ancestor(&hostile, "main", "side").await.is_err());
    }

    /// Cancellation — a browser tab closing mid-diff — must reach the git
    /// process, not just the Rust future. The collector owns the child for its
    /// whole future, so dropping the future drops the kill-on-drop owner.
    #[tokio::test]
    async fn dropping_capped_read_kills_git_child() {
        let (_dir, repo) = seeded_repo();
        let mut child = batch_child(&repo);
        let stdin = child.stdin.take().expect("stdin is piped");
        let pid = child.id().expect("a freshly spawned child has a pid");

        let collector = tokio::spawn(collect_child_stdout_capped(child, "test", 4096));
        // Let the collector actually park on git's stdout before cancelling.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pid_alive(pid),
            "git {pid} should be running before the drop"
        );

        collector.abort();
        let _ = collector.await;

        // The writer is still open, so `cat-file --batch` has no EOF to exit on:
        // only the dropped owner's kill can have ended it.
        assert!(
            wait_for_pid_gone(pid, Duration::from_secs(10)).await,
            "git {pid} survived the dropped read future"
        );
        drop(stdin);
    }

    /// A cap hit must kill and reap git *while its input is still open*. If the
    /// collector waited for the producer's EOF instead, this would hang — hence
    /// the timeout: a regression has to fail, not wedge the suite forever.
    #[tokio::test]
    async fn capped_batch_kills_git_before_open_input_finishes() {
        let (_dir, repo) = seeded_repo();
        // Sized to overflow a 4 KiB cap many times over while still fitting in
        // the 64 KiB pipe buffer. That combination is deliberate: git finishes
        // writing and parks back on *stdin*, so it is not touching stdout when
        // the cap is reached. Closing our read end therefore cannot end it by
        // EPIPE, and the explicit kill is the only thing left that can — which
        // is precisely the behaviour under test.
        let oid = write_blob(&repo, "huge.txt", &filler(40 * 1024));
        let mut child = batch_child(&repo);
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let pid = child.id().expect("a freshly spawned child has a pid");

        stdin
            .write_all(format!("{oid}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        // Let git drain its whole answer into the pipe buffer before the read
        // starts. Overshooting this only weakens the test, never breaks it.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let (bytes, truncated) = tokio::time::timeout(
            Duration::from_secs(20),
            collect_child_stdout_capped(child, "test", 4096),
        )
        .await
        .expect("the capped read must not wait for the producer's EOF")
        .unwrap();

        assert_eq!(bytes.len(), 4096);
        assert!(truncated);
        assert!(
            wait_for_pid_gone(pid, Duration::from_secs(10)).await,
            "git {pid} outlived the cap hit"
        );
        // Only dropped now: the assertions above all held with the writer open.
        drop(stdin);
    }

    // --- #221: the stdin-framed batch parser, proved without spawning git ---
    //
    // These drive `parse_batch_header`, `read_batch_header_line`,
    // `read_batch_content`, `batch_query` and `batch_lookup_with_fallback`
    // directly against synthetic bytes — no real `cat-file --batch` process
    // anywhere below. That is the point: the #168 security property (type
    // resolved from the header, before any content byte is read, on every
    // attempt including the fallback) has to hold as a fact about this parser,
    // not merely as an emergent property of two integration tests against a
    // real repository.

    /// A synthetic `cat-file --batch` **hit** frame: `<oid> SP <type> SP
    /// <size> LF` followed by exactly `content.len()` bytes and the frame's
    /// own trailing LF — byte-for-byte the shape confirmed against real git
    /// (`git cat-file --batch`, git 2.43).
    fn hit_frame(oid: &str, kind: &str, content: &[u8]) -> Vec<u8> {
        let mut frame = format!("{oid} {kind} {}\n", content.len()).into_bytes();
        frame.extend_from_slice(content);
        frame.push(b'\n');
        frame
    }

    /// A synthetic `cat-file --batch` **miss** frame: `<query> SP missing LF`.
    fn miss_frame(query: &str) -> Vec<u8> {
        format!("{query} missing\n").into_bytes()
    }

    #[test]
    fn parse_batch_header_reads_a_hit_line() {
        let oid = "a".repeat(40);
        let header = parse_batch_header(format!("{oid} blob 12").as_bytes()).unwrap();
        assert_eq!(
            header,
            BatchHeader::Found {
                kind: "blob".to_string(),
                size: 12
            }
        );
    }

    #[test]
    fn parse_batch_header_reads_a_tree_and_commit_line() {
        let oid = "b".repeat(40);
        assert_eq!(
            parse_batch_header(format!("{oid} tree 68").as_bytes()).unwrap(),
            BatchHeader::Found {
                kind: "tree".to_string(),
                size: 68
            }
        );
        assert_eq!(
            parse_batch_header(format!("{oid} commit 147").as_bytes()).unwrap(),
            BatchHeader::Found {
                kind: "commit".to_string(),
                size: 147
            }
        );
    }

    #[test]
    fn parse_batch_header_reads_a_miss_line() {
        assert_eq!(
            parse_batch_header(b"deadbeef:some/path missing").unwrap(),
            BatchHeader::Missing
        );
    }

    /// The discriminator is the line's trailing shape, not a scan for the
    /// substring "missing" anywhere in it: a path that itself ends in the
    /// word "missing" produces a miss line that still ends " missing" (the
    /// literal git appends), and must still parse as a miss, not as a
    /// malformed hit.
    #[test]
    fn parse_batch_header_a_path_named_missing_is_still_a_clean_miss() {
        assert_eq!(
            parse_batch_header(b"cafef00d:sub/missing missing").unwrap(),
            BatchHeader::Missing
        );
    }

    #[test]
    fn parse_batch_header_rejects_garbage() {
        assert!(parse_batch_header(b"not a real header").is_err());
        assert!(parse_batch_header(b"").is_err());
        // Right shape, non-numeric size.
        let oid = "c".repeat(40);
        assert!(parse_batch_header(format!("{oid} blob not-a-number").as_bytes()).is_err());
    }

    #[tokio::test]
    async fn read_batch_header_line_reads_two_lines_off_one_stream_in_order() {
        let data = b"first line\nsecond line\n".to_vec();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let first = read_batch_header_line(&mut reader, 1024)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, b"first line");
        // The reader must be left positioned exactly after the LF — proving
        // `consume` was told the right amount, not "the whole chunk".
        let second = read_batch_header_line(&mut reader, 1024)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, b"second line");
    }

    #[tokio::test]
    async fn read_batch_header_line_returns_none_on_clean_eof() {
        let data: Vec<u8> = Vec::new();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        assert!(read_batch_header_line(&mut reader, 1024)
            .await
            .unwrap()
            .is_none());
    }

    /// Bytes arrived but the stream ended before their LF — the shape a
    /// fatally-crashed `cat-file --batch` can never actually produce (it
    /// writes nothing before dying), but the reader treats it identically to
    /// a totally empty stream regardless, rather than fabricating a header
    /// out of a partial line.
    #[tokio::test]
    async fn read_batch_header_line_treats_a_partial_line_at_eof_as_none() {
        let data = b"partial, no newline".to_vec();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        assert!(read_batch_header_line(&mut reader, 1024)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn read_batch_header_line_errors_when_a_line_exceeds_the_cap() {
        let data = b"way more than the cap allows\n".to_vec();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let err = read_batch_header_line(&mut reader, 4).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_batch_content_reads_two_frames_back_to_back() {
        // Two hit frames concatenated, exactly as they would arrive answering
        // the direct spec then the `^` fallback on one held-open process.
        let mut data = hit_frame(&"a".repeat(40), "blob", b"first\n");
        data.extend(hit_frame(&"b".repeat(40), "blob", b"second"));
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let h1 = read_batch_header_line(&mut reader, BATCH_HEADER_CAP)
            .await
            .unwrap()
            .unwrap();
        let (kind1, size1) = match parse_batch_header(&h1).unwrap() {
            BatchHeader::Found { kind, size } => (kind, size),
            BatchHeader::Missing => panic!("expected a hit"),
        };
        assert_eq!(kind1, "blob");
        let (bytes1, truncated1) = read_batch_content(&mut reader, size1, 1_000_000)
            .await
            .unwrap();
        assert_eq!(bytes1, b"first\n");
        assert!(!truncated1);

        let h2 = read_batch_header_line(&mut reader, BATCH_HEADER_CAP)
            .await
            .unwrap()
            .unwrap();
        let size2 = match parse_batch_header(&h2).unwrap() {
            BatchHeader::Found { size, .. } => size,
            BatchHeader::Missing => panic!("expected a hit"),
        };
        let (bytes2, truncated2) = read_batch_content(&mut reader, size2, 1_000_000)
            .await
            .unwrap();
        assert_eq!(bytes2, b"second");
        assert!(!truncated2);
    }

    /// The cap is enforced from the parsed `size`, not from how much of the
    /// stream actually gets read: even when the true object is far larger
    /// than the cap, exactly `cap` bytes come back and no more.
    #[tokio::test]
    async fn read_batch_content_caps_without_reading_past_it() {
        let full = vec![b'x'; 10_000];
        let data = hit_frame(&"d".repeat(40), "blob", &full);
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let header = read_batch_header_line(&mut reader, BATCH_HEADER_CAP)
            .await
            .unwrap()
            .unwrap();
        let size = match parse_batch_header(&header).unwrap() {
            BatchHeader::Found { size, .. } => size,
            BatchHeader::Missing => panic!("expected a hit"),
        };
        assert_eq!(size, 10_000);

        let (bytes, truncated) = read_batch_content(&mut reader, size, 128).await.unwrap();
        assert_eq!(bytes.len(), 128);
        assert_eq!(bytes, full[..128]);
        assert!(truncated);
    }

    /// The security property itself, isolated: a spec that resolves to a
    /// **tree** must come back as `NotABlob` without a single content byte
    /// being read. Proved structurally, not just by absence of a wrong
    /// answer — the synthetic stream contains *nothing* after the header, so
    /// if `batch_query` ever tried to read content for a non-blob, the read
    /// would hit EOF and this test would fail with an `Io` error instead of
    /// the expected `NotABlob`.
    #[tokio::test]
    async fn batch_query_rejects_a_tree_without_touching_any_content_byte() {
        let mut stdin: Vec<u8> = Vec::new();
        let header_only = format!("{} tree 68\n", "e".repeat(40)).into_bytes();
        let mut reader = tokio::io::BufReader::new(&header_only[..]);

        let outcome = batch_query(&mut stdin, &mut reader, "deadbeef:sub", 1_000_000)
            .await
            .expect("a resolved-but-wrong-type answer is not an error");
        match outcome {
            BatchQueryOutcome::Found(BatchFileRead::NotABlob { kind }) => {
                assert_eq!(kind, "tree");
            }
            other => panic!("expected NotABlob, got {other:?}"),
        }
        assert_eq!(
            stdin, b"deadbeef:sub\n",
            "exactly one query line is written"
        );
    }

    /// The same property for a **submodule gitlink** (`commit`-typed tree
    /// entry): rejected the same way, from the header alone.
    #[tokio::test]
    async fn batch_query_rejects_a_commit_type_without_touching_any_content_byte() {
        let mut stdin: Vec<u8> = Vec::new();
        let header_only = format!("{} commit 147\n", "f".repeat(40)).into_bytes();
        let mut reader = tokio::io::BufReader::new(&header_only[..]);

        let outcome = batch_query(&mut stdin, &mut reader, "deadbeef:vendor/lib", 1_000_000)
            .await
            .unwrap();
        match outcome {
            BatchQueryOutcome::Found(BatchFileRead::NotABlob { kind }) => {
                assert_eq!(kind, "commit");
            }
            other => panic!("expected NotABlob, got {other:?}"),
        }
    }

    /// A hit on the **first** attempt never writes or reads a second query —
    /// the fallback ladder must not fire on a resolved-but-wrong-type answer,
    /// only on a genuine `missing`.
    #[tokio::test]
    async fn fallback_does_not_fire_when_the_first_attempt_resolves_to_a_tree() {
        let mut stdin: Vec<u8> = Vec::new();
        let data = hit_frame(&"1".repeat(40), "tree", b"unused-tree-listing-bytes");
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let result = batch_lookup_with_fallback(&mut stdin, &mut reader, "deadbeef", "sub", 4096)
            .await
            .unwrap();
        match result {
            BatchFileRead::NotABlob { kind } => assert_eq!(kind, "tree"),
            other => panic!("expected NotABlob, got {other:?}"),
        }
        assert_eq!(
            stdin, b"deadbeef:sub\n",
            "a resolved (if wrong-typed) first answer must not trigger the ^ fallback"
        );
    }

    /// A `missing` first answer *does* trigger exactly one fallback query,
    /// against `<id>^:<path>`, on the same stream — and when that resolves to
    /// a blob, its content is what comes back.
    #[tokio::test]
    async fn fallback_fires_on_missing_and_reads_the_parents_blob() {
        let mut stdin: Vec<u8> = Vec::new();
        let mut data = miss_frame("deadbeef:sub/link.txt");
        data.extend(hit_frame(&"2".repeat(40), "blob", b"file.txt"));
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let result = batch_lookup_with_fallback(
            &mut stdin,
            &mut reader,
            "deadbeef",
            "sub/link.txt",
            1_000_000,
        )
        .await
        .unwrap();
        match result {
            BatchFileRead::Blob { bytes, truncated } => {
                assert_eq!(bytes, b"file.txt");
                assert!(!truncated);
            }
            other => panic!("expected Blob, got {other:?}"),
        }
        assert_eq!(
            stdin, b"deadbeef:sub/link.txt\ndeadbeef^:sub/link.txt\n",
            "exactly the direct spec then the ^ fallback, in order"
        );
    }

    /// The trap #168 exists to close, at the parser level: if the *parent's*
    /// answer is a tree, the fallback must reject it too — never silently
    /// serve the parent's tree listing as if it were file content.
    #[tokio::test]
    async fn fallback_rejects_a_tree_found_through_the_parent_too() {
        let mut stdin: Vec<u8> = Vec::new();
        let mut data = miss_frame("deadbeef:sub");
        data.extend(hit_frame(
            &"3".repeat(40),
            "tree",
            b"unused-tree-listing-bytes",
        ));
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let result = batch_lookup_with_fallback(&mut stdin, &mut reader, "deadbeef", "sub", 4096)
            .await
            .unwrap();
        match result {
            BatchFileRead::NotABlob { kind } => assert_eq!(kind, "tree"),
            other => panic!("expected NotABlob, got {other:?}"),
        }
    }

    /// Both the direct spec and the parent miss: a distinct terminal error,
    /// never a fabricated third attempt.
    #[tokio::test]
    async fn both_missing_is_a_distinct_terminal_outcome() {
        let mut stdin: Vec<u8> = Vec::new();
        let mut data = miss_frame("deadbeef:nope");
        data.extend(miss_frame("deadbeef^:nope"));
        let mut reader = tokio::io::BufReader::new(&data[..]);

        let err = batch_lookup_with_fallback(&mut stdin, &mut reader, "deadbeef", "nope", 4096)
            .await
            .unwrap_err();
        assert!(
            matches!(err, BatchLookupError::BothMissing),
            "expected BothMissing, got {err:?}"
        );
    }

    /// A batch child that dies before writing anything (the real shape of a
    /// fatal top-level error, e.g. a traversal outside the repository) reads
    /// back as a clean EOF — `ProcessEnded` — never as `missing`, so the
    /// caller knows to consult the process's exit status and stderr instead
    /// of trying a fallback against a process that no longer exists.
    #[tokio::test]
    async fn a_dead_process_before_any_header_is_process_ended_not_missing() {
        let mut stdin: Vec<u8> = Vec::new();
        let empty: Vec<u8> = Vec::new();
        let mut reader = tokio::io::BufReader::new(&empty[..]);

        let err =
            batch_lookup_with_fallback(&mut stdin, &mut reader, "deadbeef", "../secret.txt", 4096)
                .await
                .unwrap_err();
        assert!(
            matches!(err, BatchLookupError::ProcessEnded),
            "expected ProcessEnded, got {err:?}"
        );
        assert_eq!(
            stdin, b"deadbeef:../secret.txt\n",
            "exactly one query is written before the dead stream is discovered; \
             a dead process gets no fallback attempt"
        );
    }

    // --- #221 integration: the real `cat-file --batch` process ---------------

    #[tokio::test]
    async fn git_cat_file_batch_reads_a_blob_directly() {
        let (_dir, repo) = seeded_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let result = git_cat_file_batch(&repo, &id, "a.txt", 1_000_000, "test")
            .await
            .unwrap();
        match result {
            BatchFileRead::Blob { bytes, truncated } => {
                assert_eq!(bytes, b"a\n");
                assert!(!truncated);
            }
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_cat_file_batch_falls_back_to_the_parent_on_a_real_repo() {
        let (_dir, repo) = seeded_repo();
        let parent_id = out(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["rm", "-q", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "delete a.txt"]);
        let child_id = out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(child_id, parent_id);

        let result = git_cat_file_batch(&repo, &child_id, "a.txt", 1_000_000, "test")
            .await
            .unwrap();
        match result {
            BatchFileRead::Blob { bytes, .. } => assert_eq!(bytes, b"a\n"),
            other => panic!("expected the parent's blob, got {other:?}"),
        }
    }

    /// One spawn total, real process: the type check and the content read
    /// for a deleted-then-recreated-as-a-directory path both run against the
    /// same still-open `cat-file --batch`, and the rejection carries the
    /// real `kind`.
    #[tokio::test]
    async fn git_cat_file_batch_rejects_a_real_tree_through_the_fallback() {
        let (_dir, repo) = path_battery_git_cmd_fixture();
        let parent_id = out(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["rm", "-q", "-r", "sub"]);
        run(&repo, &["commit", "-q", "-m", "delete sub"]);
        let child_id = out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(child_id, parent_id);

        let result = git_cat_file_batch(&repo, &child_id, "sub", 1_000_000, "test")
            .await
            .unwrap();
        match result {
            BatchFileRead::NotABlob { kind } => assert_eq!(kind, "tree"),
            other => panic!("expected NotABlob(tree), got {other:?}"),
        }
    }

    /// A traversal path that escapes the repository crashes the real batch
    /// process fatally (confirmed against real git 2.43: `cat-file --batch`
    /// treats this as a top-level fatal error, not a per-query `missing`),
    /// and this must still surface as git's own "outside repository" message,
    /// not a generic failure.
    #[tokio::test]
    async fn git_cat_file_batch_surfaces_git_own_traversal_error() {
        let (_dir, repo) = seeded_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let (status, msg) = git_cat_file_batch(&repo, &id, "../secret.txt", 1_000_000, "test")
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            msg.contains("outside repository"),
            "git's own stderr must survive: {msg}"
        );
    }

    /// A `\n` inside `path` is refused before anything is spawned — the
    /// framing hazard: writing it verbatim into a `<spec>\n` stdin line would
    /// silently split into two protocol queries.
    #[tokio::test]
    async fn git_cat_file_batch_refuses_an_embedded_newline_before_spawning() {
        let (_dir, repo) = seeded_repo();
        let id = out(&repo, &["rev-parse", "HEAD"]);

        let (status, _msg) = git_cat_file_batch(&repo, &id, "a.txt\nsecret.txt", 1_000_000, "test")
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A repository shaped like `handlers/read.rs`'s own `path_battery_repo`
    /// fixture, duplicated here (rather than shared across crates) because
    /// that helper is private to its own test module.
    fn path_battery_git_cmd_fixture() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/file.txt"), "sub-file\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "path battery fixture"]);
        (dir, repo)
    }
}
