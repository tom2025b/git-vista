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

use std::path::Path;
use std::process::{Output, Stdio};

use axum::http::StatusCode;
use tokio::io::AsyncReadExt;

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
/// five production call sites are read endpoints (`/api/diff`'s three `diff`
/// reads and `/api/file`'s two `show` reads), none of which can reach a remote.
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

/// Resolve `rev` to a full commit id in `repo`, or `None` if it doesn't
/// resolve. Used by the journal hooks to capture a ref's tip before/after an
/// operation — e.g. a branch's tip *before* deleting it, which is the one
/// piece of state git itself throws away (the branch's reflog dies with it)
/// and exactly what "Restore branch" later needs.
pub(crate) async fn rev_parse(repo: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    // Local (D3): resolving a rev reads the object database, never a remote.
    let output = sandboxed(
        repo,
        &["rev-parse", "--verify", "--quiet", &spec],
        crate::sandbox::NetworkNeed::Local,
    )
    .ok()?
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Whether `ancestor` is an ancestor of (or equal to) `rev` — `git merge-base
/// --is-ancestor` exits 0 exactly then. "HEAD already contains the base tip" is
/// the definition of "a rebase onto that base would change nothing".
pub(crate) async fn is_ancestor(repo: &Path, ancestor: &str, rev: &str) -> bool {
    // Local (D3): `merge-base` walks the local object graph.
    let Ok(cmd) = sandboxed(
        repo,
        &["merge-base", "--is-ancestor", ancestor, rev],
        crate::sandbox::NetworkNeed::Local,
    ) else {
        return false;
    };
    cmd.output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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
pub(crate) async fn git_ref_exists(repo: &Path, refname: &str) -> bool {
    // Local (D3): a ref existence check reads `.git`, never a remote.
    let Ok(cmd) = sandboxed(
        repo,
        &["rev-parse", "--verify", "--quiet", refname],
        crate::sandbox::NetworkNeed::Local,
    ) else {
        return false;
    };
    cmd.output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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
}
