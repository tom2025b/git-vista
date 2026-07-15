//! Thin `git -C <repo> …` command wrappers shared across the route handlers.
//!
//! Split out of `main.rs`: every handler that shells out to git for a small,
//! reusable step goes through one of these. They deliberately do nothing clever —
//! run git, interpret the exit status/output — so the B3 posture (git does the
//! work, we forward its own error text) stays consistent everywhere. All are
//! `pub(crate)`; the handlers in `crate::handlers` are their only callers.

use std::path::Path;

use axum::http::StatusCode;

/// Run `git -C <repo> <args…>` and return its stdout bytes, mapping both spawn
/// failures and non-zero exits to a 500 with git's own stderr as the reason.
/// Shared by the diff reads; deliberately bytes, not String — paths in `-z`
/// listings aren't guaranteed UTF-8, and the parsers handle that themselves.
pub(crate) async fn git_stdout(
    repo: &Path,
    args: &[String],
    endpoint: &str,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            eprintln!("git-vista: {endpoint} couldn't run git: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            )
        })?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git failed.".to_string()
        } else {
            msg
        };
        eprintln!("git-vista: {endpoint} failed: {msg}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
    }
    Ok(output.stdout)
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
