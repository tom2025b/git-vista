//! The stash drawer's write path (M3.24, #77).
//!
//! # Why this is its own module, and it is not organisational taste
//!
//! `git stash push` carries the literal `"push"` in its argv. `planner.rs` is
//! forbidden from naming `push` as a git subcommand at all — the guard
//! `only_planner_push_builds_a_push_argv_and_it_can_only_build_a_leased_force`
//! asserts it by scanning the source — because push-argv building was moved to
//! `planner/push.rs` so that exactly one `match` over `ForcePublish` can decide
//! whether a force is leased (#231, ADR 0045 D1).
//!
//! A stash push is not a network push and cannot force anything. But the guard
//! is a *source scan*, and a scan that had to distinguish `["push", ..]` from
//! `["stash", "push", ..]` would be a scan someone could talk into accepting
//! the wrong one. Weakening it to admit this file would trade a proof for a
//! convenience. Moving here costs nothing and keeps the guard absolute.
//!
//! # The one safety property in this file
//!
//! [`stash_entry_still_at`] is it. Everything else is argv construction.

use std::path::Path;

use axum::http::StatusCode;

use git_vista_protocol::{CommitOid, StashMessage, StashSelector};

use git_vista_core::activity::ActivityKind;

use crate::sandbox::NetworkNeed;

use super::{couldnt_run, journal_app_event, run_git, stderr_or, Obs};

/// Resolve a stash selector to the oid it names **right now**, and refuse
/// unless that matches what the plan was built against (M3.24, #77).
///
/// # This function is the entire safety of the stash write path
///
/// A selector is an index into a reflog, and the reflog renumbers on every
/// drop: `stash@{1}` names a different commit before and after `stash@{0}`
/// goes. So a plan built seconds ago against `stash@{1}` may now address
/// someone else's work. The oid cannot be used as the address instead — `git
/// stash drop <oid>` is not a command, and one commit can occupy two slots —
/// so the only safe shape is **selector as address, oid as witness**, checked
/// here immediately before the mutation runs.
///
/// Three outcomes, and the third is the one that matters:
///
/// | outcome | meaning |
/// |---|---|
/// | `Ok(())` | the selector still names `expected` — proceed |
/// | `Err(409)` | it names something else, or nothing: the drawer moved |
/// | `Err(500)` | the resolve itself failed — we do not know, so we do not act |
///
/// The last row is not a formality. Returning "matches" on an unreadable
/// repository would let a destructive operation run against an unread value,
/// which is the defect class this milestone exists to remove.
pub(super) async fn stash_entry_still_at(
    repo: &Path,
    need: NetworkNeed,
    endpoint: &str,
    entry: &StashSelector,
    expected: &CommitOid,
) -> Result<(), (StatusCode, String)> {
    let output = match run_git(
        repo,
        need,
        &["rev-parse", "--verify", "--quiet", entry.as_str()],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return Err(couldnt_run(endpoint, &e)),
    };
    if !output.status.success() {
        // git's documented "this ref does not resolve" is exit 1 with nothing
        // on stderr. Anything else — a broken ref store, exit 128 — is a
        // failed CHECK, and a failed check is never evidence of absence.
        let code = output.status.code();
        if code == Some(1) && output.stderr.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                format!("{entry} no longer exists — the stash list changed. Reload and try again."),
            ));
        }
        let msg = stderr_or(&output, "git rev-parse on the stash entry failed.");
        eprintln!("git-vista: {endpoint} could not resolve {entry}: {msg}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not read the stash list, so {entry} was not touched: {msg}"),
        ));
    }
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if actual != expected.as_str() {
        eprintln!(
            "git-vista: {endpoint} refused: {entry} is {actual}, plan expected {}",
            expected.as_str()
        );
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{entry} now holds a different stash than when this was planned —                  the list moved underneath it. Reload and try again."
            ),
        ));
    }
    Ok(())
}

/// `git stash push [--keep-index] [--include-untracked] [-m <message>]`
/// (`/api/stash/push`, M3.24 #77).
///
/// No precondition: a dirty tree is this operation's whole input, and git
/// refuses an empty stash itself rather than creating a useless entry.
pub(super) async fn exec_push_stash(
    repo: &Path,
    need: NetworkNeed,
    message: Option<&StashMessage>,
    keep_index: bool,
    include_untracked: bool,
) -> (StatusCode, String) {
    let mut args: Vec<&str> = vec!["stash", "push"];
    if keep_index {
        args.push("--keep-index");
    }
    if include_untracked {
        args.push("--include-untracked");
    }
    if let Some(m) = message {
        args.push("-m");
        args.push(m.as_str());
    }
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stash/push", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git stash push failed.");
        eprintln!("git-vista: /api/stash/push failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    // git says "No local changes to save" on stdout and still exits 0. That is
    // a successful command that stashed nothing, and reporting it as a stash
    // would leave the user looking for a drawer entry that was never created.
    let said = String::from_utf8_lossy(&output.stdout);
    if said.contains("No local changes to save") {
        println!("[/api/stash/push] nothing to stash");
        return (
            StatusCode::OK,
            "Nothing to stash — the working tree is already clean.".to_string(),
        );
    }
    println!("[/api/stash/push] stashed the working tree");
    journal_app_event(
        repo,
        ActivityKind::Other,
        Some("refs/stash".to_string()),
        Obs::Absent,
        Obs::Absent,
        match message {
            Some(m) => format!("stashed changes ‘{m}’"),
            None => "stashed changes".to_string(),
        },
    )
    .await;
    (StatusCode::OK, "Stashed your changes.".to_string())
}

/// `git stash apply <selector>` (`/api/stash/apply`, M3.24 #77) — restore a
/// stash's changes, KEEPING the entry.
///
/// Guarded by `CleanWorktree`, which is the load-bearing decision of this
/// slice: with a clean tree, the abort path is `reset --hard` + `clean -fd`
/// and that is *provably* safe, because a clean tree has nothing of the
/// user's to destroy. Apply into a dirty tree would mean an abort could
/// discard work that was never in the stash.
pub(super) async fn exec_apply_stash(
    repo: &Path,
    need: NetworkNeed,
    entry: &StashSelector,
    expected_oid: &CommitOid,
) -> (StatusCode, String) {
    if let Err(refusal) =
        stash_entry_still_at(repo, need, "/api/stash/apply", entry, expected_oid).await
    {
        return refusal;
    }
    let output = match run_git(repo, need, &["stash", "apply", entry.as_str()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stash/apply", &e),
    };
    if !output.status.success() {
        // A conflicting apply leaves the entry in place — that is git's own
        // behaviour and it is the right one, so the message says so rather
        // than leaving the user wondering whether their stash survived.
        let msg = stderr_or(&output, "git stash apply failed.");
        eprintln!("git-vista: /api/stash/apply failed: {msg}");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "{msg}

The stash entry was not removed — it is still in the list."
            ),
        );
    }
    println!("[/api/stash/apply] applied {entry}");
    journal_app_event(
        repo,
        ActivityKind::Other,
        Some("refs/stash".to_string()),
        Obs::Absent,
        Obs::Absent,
        format!("applied stash {entry}"),
    )
    .await;
    (
        StatusCode::OK,
        format!("Applied {entry}. It is still in your stash list."),
    )
}

/// `git stash drop <selector>` (`/api/stash/drop`, M3.24 #77).
///
/// `Destructive` on the same reasoning `ForceDeleteBranch` is: the commit
/// becomes unreachable. It is recoverable — `RecreateStashEntry` plus the
/// durable recovery pin keep the object alive past gc — but `RiskLevel` is
/// about what can be lost, not about whether an undo was built.
pub(super) async fn exec_drop_stash(
    repo: &Path,
    need: NetworkNeed,
    entry: &StashSelector,
    expected_oid: &CommitOid,
) -> (StatusCode, String) {
    // The re-resolve matters most here. Every drop renumbers the list, so a
    // stale selector on this path deletes a stash the user never chose.
    if let Err(refusal) =
        stash_entry_still_at(repo, need, "/api/stash/drop", entry, expected_oid).await
    {
        return refusal;
    }
    let output = match run_git(repo, need, &["stash", "drop", entry.as_str()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stash/drop", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git stash drop failed.");
        eprintln!("git-vista: /api/stash/drop failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    println!(
        "[/api/stash/drop] dropped {entry} (was {})",
        expected_oid.as_str()
    );
    journal_app_event(
        repo,
        ActivityKind::Other,
        Some("refs/stash".to_string()),
        Obs::Absent,
        Obs::Absent,
        format!("dropped stash {entry}"),
    )
    .await;
    (
        StatusCode::OK,
        format!("Dropped {entry}. You can undo this from the history."),
    )
}
