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
use std::process::Stdio;

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
pub(crate) async fn git_stdout_capped(
    repo: &Path,
    args: &[String],
    endpoint: &str,
    cap: usize,
) -> Result<(Vec<u8>, bool), (StatusCode, String)> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
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
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{rev}^{{commit}}"))
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
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", ancestor, rev])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run one `git -C <repo> <args…>` for the reset, mapping any failure to git's
/// own stderr so the response can say which exact step refused and why.
pub(crate) async fn git_ok(repo: &Path, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
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
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg(refname)
        .output()
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
