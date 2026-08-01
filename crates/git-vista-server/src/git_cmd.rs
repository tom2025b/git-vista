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

fn sandboxed(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
) -> Result<crate::sandbox::spawn::SandboxedCommand, String> {
    let read_only = crate::state::read_only_for_path(repo);
    let need = crate::sandbox::reconcile_need(declared, args);
    let policy = crate::sandbox::policy_for(repo, read_only, need).map_err(|e| e.to_string())?;
    Ok(crate::sandbox::spawn::command_async(&policy, repo, args))
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
pub(crate) async fn git_output_for(
    repo: &Path,
    args: &[&str],
    declared: crate::sandbox::NetworkNeed,
) -> std::io::Result<Output> {
    let cmd = sandboxed(repo, args, declared).map_err(std::io::Error::other)?;
    cmd.output().await
}

/// Declares `NetworkNeed::Local` for the same reason [`git_output`] does: all
/// three production call sites are `/api/diff`'s `diff` reads
/// (`--name-status`, `--numstat`, `--patch`), none of which can reach a
/// remote. `/api/file` used to be two more (`cat-file -t` then `git show`)
/// until #221 folded them into [`git_cat_file_batch`]'s single held-open
/// process.
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

    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

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

    /// A fresh repository on branch `main` with one committed file.
    fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        (dir, repo)
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
