//! The bisect executors — start, mark (good/bad/skip) and reset — plus
//! [`discover`], which reads git's own on-disk bisect state fresh every
//! time it is asked, never mirrors it (M5.34, #87, ADR 0121).
//!
//! # Why this is its own module
//!
//! Like [`super::sequence_exec`], this owns a category of git state this
//! app does not create and must not re-derive from anything but git
//! itself: `.git/BISECT_START` (in progress? — resolved per-worktree via
//! `git rev-parse --git-path`, so — unlike the app's own journal,
//! `journal::state_dir`'s own doc comment — this does NOT skip linked
//! worktrees), `.git/BISECT_LOG` (ordered history, parsed as the replay
//! script `git bisect replay` itself trusts), and `refs/bisect/*` (the
//! current bad/good/skip set, read via `git for-each-ref`'s own
//! machine-readable format, never git's prose). See ADR 0121 for what was
//! verified empirically in a scratch repo before any of this was written —
//! most importantly that `git bisect good|bad` **exits 1** on the step
//! that finds the culprit, so [`discover`]'s candidate-range computation is
//! what decides "finished", never the exit code and never git's printed
//! sentence.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use git_vista_protocol::plan::BisectVerdict;
use git_vista_protocol::plan_export::{bisect_mark_argv, bisect_reset_argv, bisect_start_argv};
use git_vista_protocol::CommitOid;

use git_vista_core::activity::ActivityKind;

use crate::git_cmd::{git_output, rev_parse};
use crate::sandbox::NetworkNeed;

use super::{couldnt_run, journal_app_event, run_git_argv, short, stderr_stdout_or, Obs, Observed};

// ---------------------------------------------------------------------------
// Discovery — read git's own state, never mirror it
// ---------------------------------------------------------------------------

/// One decision this session made, in the order git ran it — one line of
/// `.git/BISECT_LOG`'s command script (ADR 0121 §1). Comment lines
/// (`# bad: [...] subject`) are git's own human-readable annotation of the
/// command that follows and are not parsed here; the command line is
/// authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BisectLogStep {
    /// `"start"` | `"good"` | `"bad"` | `"skip"`.
    pub verb: String,
    /// The oids/refs the command named, in the order given.
    pub args: Vec<String>,
}

/// Git's own bisect state, read fresh — never cached, never mirrored
/// (ADR 0121 §1). Every field comes from `.git/BISECT_START`,
/// `.git/BISECT_LOG` or `refs/bisect/*`; there is no field here this app
/// invented state for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BisectStatus {
    pub in_progress: bool,
    /// What `HEAD` resolves to right now — the current candidate while a
    /// bisect is in progress. `None` when git could not be read.
    pub current: Option<String>,
    /// `.git/BISECT_START`'s content: the branch or commit `BisectReset`
    /// returns to.
    pub started_from: Option<String>,
    pub bad: Option<String>,
    pub good: Vec<String>,
    pub skipped: Vec<String>,
    /// Every `start`/`good`/`bad`/`skip` this session ran, in order.
    pub history: Vec<BisectLogStep>,
    /// The candidate range has narrowed to exactly one commit (`bad`
    /// itself) — computed from `git rev-list`, never from git's printed
    /// sentence or `git bisect`'s exit code (ADR 0121 §2).
    pub finished: bool,
}

/// Resolve `name` to its real on-disk path for THIS worktree via `git
/// rev-parse --git-path` — correct for a linked worktree too, since bisect
/// state is per-worktree (verified: `--git-path BISECT_START` from a linked
/// worktree resolves to `.git/worktrees/<name>/BISECT_START`, not the main
/// worktree's). This is what lets `discover` work in every worktree, unlike
/// the app's own journal.
async fn git_path(repo: &Path, name: &str) -> Option<PathBuf> {
    let out = git_output(repo, &["rev-parse", "--git-path", name])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        return None;
    }
    let p = PathBuf::from(rel);
    Some(if p.is_absolute() { p } else { repo.join(p) })
}

async fn read_git_file(repo: &Path, name: &str) -> Option<String> {
    let path = git_path(repo, name).await?;
    tokio::fs::read_to_string(path).await.ok()
}

/// Parse `.git/BISECT_LOG`'s command lines — the format `git bisect replay`
/// itself trusts. `#`-prefixed comment lines are git's own annotation and
/// are skipped; a command line always starts `git bisect <verb> …`.
fn parse_bisect_log(text: &str) -> Vec<BisectLogStep> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("git bisect ")?;
            let mut parts = rest.split_whitespace();
            let verb = parts.next()?.to_string();
            let args = parts.map(|s| s.trim_matches('\'').to_string()).collect();
            Some(BisectLogStep { verb, args })
        })
        .collect()
}

/// Enumerate `refs/bisect/*` via `git for-each-ref`'s own machine-readable
/// format — never git's prose (ADR 0037). Failure (git unavailable) reads
/// as "nothing observed", the same posture [`super::census_for`] takes for
/// a failed census: absence is not the same claim as an empty set, but
/// there is no room in this shape to carry that distinction further, and a
/// caller finding no bisect in progress falls back to the `BISECT_START`
/// check regardless.
async fn refs_bisect(repo: &Path) -> (Option<String>, Vec<String>, Vec<String>) {
    let out = match git_output(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/bisect",
        ],
    )
    .await
    {
        Ok(o) if o.status.success() => o,
        _ => return (None, Vec::new(), Vec::new()),
    };
    let mut bad = None;
    let mut good = Vec::new();
    let mut skipped = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((refname, oid)) = line.rsplit_once(' ') else {
            continue;
        };
        if refname == "refs/bisect/bad" {
            bad = Some(oid.to_string());
        } else if refname.starts_with("refs/bisect/good-") {
            good.push(oid.to_string());
        } else if refname.starts_with("refs/bisect/skip-") {
            skipped.push(oid.to_string());
        }
    }
    (bad, good, skipped)
}

/// Has the candidate range narrowed to exactly one commit? `git rev-list
/// --count <bad> ^<good...>` — never git's printed "is the first bad
/// commit", never `git bisect`'s exit code (ADR 0121 §2, verified: that
/// command exits 1 on exactly this step). Skip commits are deliberately
/// left IN the range — they were never resolved as good, so excluding them
/// would under-count and report "finished" too early.
async fn is_finished(repo: &Path, bad: &str, good: &[String]) -> bool {
    let mut args: Vec<String> = vec![
        "rev-list".to_string(),
        "--count".to_string(),
        bad.to_string(),
    ];
    args.extend(good.iter().map(|g| format!("^{g}")));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match git_output(repo, &arg_refs).await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u32>()
            .map(|n| n == 1)
            .unwrap_or(false),
        _ => false,
    }
}

/// Read git's own bisect state fresh. Called by `GET /api/bisect/status`
/// and by every executor in this module after it runs, to decide what
/// happened — never cached, never mirrored (ADR 0121 §1).
pub(crate) async fn discover(repo: &Path) -> BisectStatus {
    let started_from = read_git_file(repo, "BISECT_START")
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let in_progress = started_from.is_some();
    if !in_progress {
        return BisectStatus::default();
    }
    let current = rev_parse(repo, "HEAD").await.ok().flatten();
    let (bad, good, skipped) = refs_bisect(repo).await;
    let history = read_git_file(repo, "BISECT_LOG")
        .await
        .map(|s| parse_bisect_log(&s))
        .unwrap_or_default();
    let finished = match &bad {
        Some(b) => is_finished(repo, b, &good).await,
        None => false,
    };
    BisectStatus {
        in_progress,
        current,
        started_from,
        bad,
        good,
        skipped,
        history,
        finished,
    }
}

// ---------------------------------------------------------------------------
// Notes — app-only metadata, outside the GitOperation vocabulary (ADR 0121 §5)
// ---------------------------------------------------------------------------

/// A free-text note never moves a ref or touches the index, so it does not
/// go through the planner — see `GitOperation`'s doc comment on why every
/// *mutation* does, and ADR 0121 §5 on why a note is not one. Stored at the
/// per-worktree private path `git rev-path --git-path
/// git-vista-bisect-notes.json` — the same worktree-correct resolution
/// [`discover`] uses, since notes are scoped to the bisect session running
/// in THIS worktree, not shared with the repository's other worktrees.
const NOTES_FILE: &str = "git-vista-bisect-notes.json";

pub(crate) async fn read_notes(repo: &Path) -> std::collections::BTreeMap<String, String> {
    let Some(text) = read_git_file(repo, NOTES_FILE).await else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub(crate) async fn write_note(repo: &Path, commit: &str, note: &str) -> Result<(), String> {
    let Some(path) = git_path(repo, NOTES_FILE).await else {
        return Err("could not resolve this worktree's git directory".to_string());
    };
    let mut notes = read_notes(repo).await;
    if note.is_empty() {
        notes.remove(commit);
    } else {
        notes.insert(commit.to_string(), note.to_string());
    }
    let text = serde_json::to_string_pretty(&notes).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, text)
        .await
        .map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

/// Cleared when a bisect ends — notes are scoped to the session they were
/// written during (ADR 0121 §5).
async fn clear_notes(repo: &Path) {
    if let Some(path) = git_path(repo, NOTES_FILE).await {
        let _ = tokio::fs::remove_file(path).await;
    }
}

// ---------------------------------------------------------------------------
// The executors
// ---------------------------------------------------------------------------

/// `git bisect start <bad> <good...>` (M5.34, #87).
pub(super) async fn exec_start(
    repo: &Path,
    need: NetworkNeed,
    bad: &CommitOid,
    good: &[CommitOid],
    observed: &Observed,
) -> (StatusCode, String) {
    if discover(repo).await.in_progress {
        return (
            StatusCode::CONFLICT,
            "A bisect is already in progress. End it (reset) before starting a new one."
                .to_string(),
        );
    }
    if good.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "A bisect needs at least one known-good commit to compute a candidate range."
                .to_string(),
        );
    }
    let output = match run_git_argv(repo, need, &bisect_start_argv(bad, good)).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/bisect/start", &e),
    };
    // `git bisect start` writes BISECT_START/refs before attempting the
    // first checkout, so a failed checkout (dirty worktree — verified,
    // ADR 0121 §1) can still leave a real session behind. Discover state
    // after the call regardless of whether the checkout itself succeeded.
    let status = discover(repo).await;
    if !status.in_progress {
        return (
            StatusCode::BAD_REQUEST,
            stderr_stdout_or(&output, "git could not start the bisect."),
        );
    }
    let new = Obs::from_read(rev_parse(repo, "HEAD").await);
    journal_app_event(
        repo,
        ActivityKind::Bisect,
        None,
        observed.head_tip.clone(),
        new,
        format!(
            "bisect: started — bad {}, good {}",
            short(bad.as_str()),
            good.iter()
                .map(|o| short(o.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .await;
    if output.status.success() {
        (StatusCode::OK, "Bisect started.".to_string())
    } else {
        (
            StatusCode::CONFLICT,
            stderr_stdout_or(
                &output,
                "The bisect session started, but checking out the first candidate failed \
                 — the working tree may have local changes in the way.",
            ),
        )
    }
}

/// `git bisect good|bad|skip` on the current candidate (M5.34, #87). No
/// commit field — see [`git_vista_protocol::plan::GitOperation::BisectMark`]'s
/// doc comment: the candidate is whatever `HEAD` already is.
pub(super) async fn exec_mark(
    repo: &Path,
    need: NetworkNeed,
    verdict: BisectVerdict,
    observed: &Observed,
) -> (StatusCode, String) {
    if !discover(repo).await.in_progress {
        return (
            StatusCode::CONFLICT,
            "There is no bisect in progress, so there is nothing to mark. If you expected \
             one, it may have already finished or been reset."
                .to_string(),
        );
    }
    let output = match run_git_argv(repo, need, &bisect_mark_argv(verdict)).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/bisect/mark", &e),
    };
    let new = Obs::from_read(rev_parse(repo, "HEAD").await);
    let word = match verdict {
        BisectVerdict::Good => "good",
        BisectVerdict::Bad => "bad",
        BisectVerdict::Skip => "skip",
    };
    journal_app_event(
        repo,
        ActivityKind::Bisect,
        None,
        observed.head_tip.clone(),
        new,
        format!("bisect: marked {word}"),
    )
    .await;
    // `git bisect bad|good` EXITS 1 on the step that finds the culprit —
    // verified empirically, not assumed (ADR 0121 §1). Exit code alone
    // cannot distinguish "found it" from "the command failed"; `discover`'s
    // candidate-range computation is what decides, never the code or git's
    // printed sentence.
    let status = discover(repo).await;
    if status.finished {
        return (
            StatusCode::OK,
            match &status.bad {
                Some(oid) => {
                    format!("Bisect complete — {} is the first bad commit.", short(oid))
                }
                None => "Bisect complete.".to_string(),
            },
        );
    }
    if output.status.success() {
        (StatusCode::OK, format!("Marked {word}. Bisect continues."))
    } else {
        (
            StatusCode::BAD_REQUEST,
            stderr_stdout_or(&output, "git could not record that verdict."),
        )
    }
}

/// `git bisect reset` (M5.34, #87). Always offered, never destructive:
/// nothing here is destroyed, only returned to the state `BisectStart`
/// found the repository in — see `RecoveryStrategy::BisectReset`'s doc
/// comment.
pub(super) async fn exec_reset(
    repo: &Path,
    need: NetworkNeed,
    observed: &Observed,
) -> (StatusCode, String) {
    if !discover(repo).await.in_progress {
        return (
            StatusCode::OK,
            "There was no bisect in progress — nothing to reset.".to_string(),
        );
    }
    let output = match run_git_argv(repo, need, &bisect_reset_argv()).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/bisect/reset", &e),
    };
    if !output.status.success() {
        return (
            StatusCode::BAD_REQUEST,
            stderr_stdout_or(&output, "git could not reset the bisect."),
        );
    }
    let new = Obs::from_read(rev_parse(repo, "HEAD").await);
    journal_app_event(
        repo,
        ActivityKind::Bisect,
        None,
        observed.head_tip.clone(),
        new,
        "bisect: reset".to_string(),
    )
    .await;
    clear_notes(repo).await;
    (
        StatusCode::OK,
        "Bisect ended. The repository is back where it started.".to_string(),
    )
}
