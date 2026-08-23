//! The conflict-resolution executors — take a whole side (M4.31 #84) and
//! write resolved content into the worktree (M4.31c #432, ADR 0069).
//!
//! # Why this is its own module
//!
//! These are the only executors that write **worktree file content** rather
//! than running a git mutation: their write leg is `tokio::fs`, not an argv,
//! and everything dangerous about them is path handling — the symlink and
//! containment refusals both re-checked here at execution time, not only at
//! plan time, because the worktree can change between the two. They share
//! that shape (and the `crate::conflicts` scan they both re-run inside the
//! coordinator guard) with each other and with nothing else in the planner.
//! [`super::symlink_containment_guard`] itself stays in `planner.rs` because
//! the discard/delete executors in [`super::worktree_exec`] guard with it
//! too.

use std::path::Path;

use axum::http::StatusCode;

use git_vista_protocol::{CommitOid, ContentResolutionRefused, GenerationToken, WorktreePath};

use crate::sandbox::NetworkNeed;

use super::{couldnt_run, run_git, stderr_or, symlink_containment_guard};

/// Resolve one conflicted path by taking a whole side, or by deleting it
/// (M4.31, #84).
///
/// # Re-reads the conflict before acting, and refuses on what it finds
///
/// `shape` records no `Precondition` for this operation, because "this path is
/// still conflicted with a readable chosen side" is not expressible in a
/// vocabulary built to compare refs and worktree cleanliness. Approximating it
/// with a precondition that checks something *else* would be worse than none:
/// the plan would display a guarantee it had not made.
///
/// So the check lives here, immediately before the write, and it is stricter
/// than a precondition could be — it re-runs the scan and asks the same
/// `refuses` the caller asked, so a side that became unreadable between plan
/// and execution stops the write rather than resolving to content nobody saw.
pub(super) async fn exec_resolve_conflict(
    repo: &Path,
    need: NetworkNeed,
    path: &WorktreePath,
    resolution: git_vista_protocol::conflict::Resolution,
) -> (StatusCode, String) {
    use git_vista_protocol::conflict::Resolution;

    let files = match crate::conflicts::scan(repo).await {
        Ok(f) => f,
        // A scan that failed must never fall through to the write. Refusing
        // here is the whole reason `scan` returns a Result rather than an
        // empty Vec.
        //
        // SURVIVED MUTATION, documented rather than hidden (M4.31, #84):
        // replacing this arm with `Err(_) => Vec::new()` leaves every test
        // green. The fall-through still refuses — the path is simply not found
        // in an empty list — so nothing is written and no data is lost. What
        // breaks is the *answer*: the caller is told "not conflicted" when the
        // truth is "the conflicts could not be read", which is the
        // I-did-not-look-reported-as-a-fact failure this crate is organised
        // against.
        //
        // Not covered by a test because forcing `git ls-files` to fail inside a
        // repository still healthy enough for the pipeline to build a plan is
        // fragile in every form tried. Stated here so the next reader knows the
        // gap is known rather than missed.
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("the conflicts could not be read, so nothing was resolved — {e}"),
            )
        }
    };

    let Some(file) = files.iter().find(|f| f.path == path.as_str()) else {
        return (
            StatusCode::CONFLICT,
            format!(
                "{} is not conflicted — it may have been resolved already, or the \
                 operation that produced the conflict may have ended",
                path.as_str()
            ),
        );
    };

    if let Some(refused) = file.refuses(resolution) {
        return (
            StatusCode::CONFLICT,
            match refused {
                git_vista_protocol::conflict::ResolutionRefused::SideAbsent { side } => format!(
                    "{} has no {side} side — to remove the file, ask for a deletion \
                     explicitly rather than taking a side that is not there",
                    path.as_str()
                ),
                git_vista_protocol::conflict::ResolutionRefused::SideUnreadable {
                    side,
                    reason,
                } => format!(
                    "{}'s {side} side could not be read, so it cannot be chosen — {reason}",
                    path.as_str()
                ),
            },
        );
    }

    // `--` before the path, always: it stops a path that begins with a dash
    // being read as an option. The newtype already rejects the worst shapes,
    // but the separator is what makes that irrelevant rather than load-bearing.
    let argv: Vec<&str> = match resolution {
        Resolution::TakeOurs => vec!["checkout", "--ours", "--", path.as_str()],
        Resolution::TakeTheirs => vec!["checkout", "--theirs", "--", path.as_str()],
        // `rm` clears the index entries and removes the file in one step;
        // `-f` because a conflicted path is by definition not "clean" and git
        // refuses without it.
        Resolution::TakeDeletion => vec!["rm", "-f", "--", path.as_str()],
    };

    let output = match run_git(repo, need, &argv).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/resolve-conflict", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git could not apply that resolution.");
        eprintln!("git-vista: /api/resolve-conflict failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }

    // A checkout writes the working tree but leaves the stage entries in
    // place; the path stays conflicted until it is staged. `rm` has already
    // done both, so it needs no second step — and running `add` on a path it
    // just deleted would fail.
    if !matches!(resolution, Resolution::TakeDeletion) {
        let add = match run_git(repo, need, &["add", "--", path.as_str()]).await {
            Ok(o) => o,
            Err(e) => return couldnt_run("/api/resolve-conflict", &e),
        };
        if !add.status.success() {
            let msg = stderr_or(&add, "git could not stage the resolved file.");
            eprintln!("git-vista: /api/resolve-conflict failed to stage: {msg}");
            return (StatusCode::BAD_REQUEST, msg);
        }
    }

    println!(
        "[/api/resolve-conflict] resolved {} by {:?}",
        path.as_str(),
        resolution
    );
    (StatusCode::OK, format!("Resolved {}.", path.as_str()))
}

/// A block/line/manual-edit resolution (M4.31c, #432, ADR 0069):
/// `/api/resolve-conflict-content`.
///
/// The re-check ADR 0069 specifies, in order, every gate refusing before
/// anything is written:
///
/// 1. still conflicted at this path
/// 2. eligible for text resolution — [`ConflictedFile::all_sides_readable`]
///    then [`ConflictedFile::text_resolvable`]
/// 3. the live stage OID triple equals `expected_stages` exactly
/// 4. the live marker file, re-hashed, mints the same `conflict-v1:` token as
///    `expected_source`
///
/// Only then: write `content` to the worktree file, `git add -- path`, and
/// report the write/add half-state honestly — there is no atomicity
/// equivalent to `git apply`'s here (ADR 0069's "what does NOT transfer").
pub(super) async fn exec_resolve_conflict_content(
    repo: &Path,
    need: NetworkNeed,
    path: &WorktreePath,
    expected_stages: &[Option<CommitOid>; 3],
    expected_source: &GenerationToken,
    content: String,
) -> (StatusCode, String) {
    use git_vista_protocol::conflict::Stage;

    const OP: &str = "/api/resolve-conflict-content";

    let refuse = |r: ContentResolutionRefused| (StatusCode::CONFLICT, r.describe(path.as_str()));

    // Gate 1: still conflicted. Same failure-must-not-fall-through reasoning
    // `exec_resolve_conflict` documents for its own scan — a scan error must
    // never read as "not conflicted".
    let files = match crate::conflicts::scan(repo).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("the conflicts could not be read, so nothing was resolved — {e}"),
            )
        }
    };
    let Some(file) = files.iter().find(|f| f.path == path.as_str()) else {
        return refuse(ContentResolutionRefused::NoLongerConflicted);
    };

    // Gate 2: eligible. Readability first — a caller must not be offered a
    // resolution UI over a side nobody has seen (`all_sides_readable`'s own
    // doc), and that check is more fundamental than text-resolvability: an
    // unreadable BASE does not make `text_resolvable()` false (it only checks
    // ours/theirs), so it needs its own gate here.
    if !file.all_sides_readable() {
        return refuse(ContentResolutionRefused::NotTextResolvable {
            reason: "one side of this file could not be read".to_string(),
        });
    }
    if !file.text_resolvable() {
        let reason = match &file.not_text_resolvable {
            Some(git_vista_protocol::NotTextResolvable::Binary { .. }) => {
                "at least one side is binary".to_string()
            }
            Some(git_vista_protocol::NotTextResolvable::Deletion { .. }) => {
                "one side deleted this file — there is no second side to merge lines against"
                    .to_string()
            }
            Some(git_vista_protocol::NotTextResolvable::Rename { .. }) => {
                "the two sides do not agree on the path".to_string()
            }
            None => "this file is not eligible for a text resolution".to_string(),
        };
        return refuse(ContentResolutionRefused::NotTextResolvable { reason });
    }

    // Gate 3: the three-way picture is still the one the user resolved
    // against. `Stage::Present.oid` is the live OID; `Absent`/`Unreadable`
    // have none — and `Unreadable` cannot reach here, gate 2 already refused
    // it, but the match stays exhaustive rather than assuming that.
    let live_oid = |stage: &Stage| -> Option<CommitOid> {
        match stage {
            Stage::Present { oid, .. } => Some(oid.clone()),
            Stage::Absent {} | Stage::Unreadable { .. } => None,
        }
    };
    let live_stages = [
        live_oid(&file.base),
        live_oid(&file.ours),
        live_oid(&file.theirs),
    ];
    if &live_stages != expected_stages {
        return refuse(ContentResolutionRefused::StagesMoved);
    }

    // Gate 4: the served document itself. Re-read the LIVE marker file — the
    // one input no repository-level generation can see — and re-mint the
    // token from it. Reuses the same containment-checked reader the GET
    // endpoint serves through, so a symlink that changed the read target
    // between gate and write cannot desync gate 4 from the write below.
    let resolved = match crate::handlers::conflicts::resolve_worktree_read_path(repo, path).await {
        Ok(p) => p,
        Err((status, _)) if status == StatusCode::NOT_FOUND => {
            return refuse(ContentResolutionRefused::NoLongerConflicted);
        }
        Err((status, msg)) => return (status, msg),
    };
    let (marker_bytes, _truncated) = match crate::handlers::conflicts::read_bounded_worktree_file(
        &resolved,
        crate::handlers::read::FILE_CONTENT_CAP,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("git-vista: {OP} couldn't re-read '{}': {e}", path.as_str());
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't re-read '{}': {e}", path.as_str()),
            );
        }
    };
    let live_source =
        match crate::conflicts::conflict_source_token(repo, path.as_str(), &marker_bytes).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("git-vista: {OP} couldn't mint conflict-v1: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, e);
            }
        };
    if &live_source != expected_source {
        return refuse(ContentResolutionRefused::SourceMoved);
    }

    // The symlink guard `handlers::conflicts` doc comment promises for write
    // endpoints, run here rather than in the handler: gates 1-4 above prove
    // the path is real and current immediately before this, so this is the
    // narrowest possible window between containment-check and write.
    if let Err(refused) = symlink_containment_guard(repo, std::slice::from_ref(path), OP).await {
        return refused;
    }

    // A SYMLINK AT THE CONFLICTED PATH IS REFUSED OUTRIGHT, and this is not
    // belt-and-braces over the guard above — the guard refuses symlinks that
    // ESCAPE the worktree and directories, never an in-worktree symlink (see
    // its own doc comment). One that stays inside is the dangerous case here,
    // because the two legs below resolve it differently:
    //
    //   * `tokio::fs::write` follows the link and writes the TARGET file
    //   * `git add -- <path>` stages the link OBJECT (its target string)
    //
    // so the content would land in an unrelated tracked file while the
    // conflicted path staged something else entirely, and both half-state
    // messages below ("now holds your resolved content") would be false.
    // `conflicts::scan` cannot catch this: it reads the index, never the
    // worktree's file type.
    //
    // `symlink_metadata` on the JOINED path, deliberately not the
    // canonicalised `resolved` — canonicalising is what erases the distinction
    // this check exists to see.
    let joined = repo.join(path.as_str());
    match tokio::fs::symlink_metadata(&joined).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "'{}' is a symbolic link, not a regular file — refusing to resolve it, \
                     because writing would follow the link and stage something else",
                    path.as_str()
                ),
            );
        }
        Ok(_) => {}
        // Vanished between gate 4 and here. Report it rather than creating a
        // file at a path the conflict scan no longer describes.
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "'{}' could not be inspected before writing ({e}) — nothing was changed",
                    path.as_str()
                ),
            );
        }
    }

    // The write. No atomicity equivalent to `git apply` exists for
    // write-then-add (ADR 0069) — a failure on either leg is reported exactly
    // as what happened, never papered over.
    //
    // Writes `joined`, NOT the canonicalised `resolved`, so the bytes land at
    // exactly the path `git add` stages one call below. They can only differ
    // through a symlink, which the check above has just refused — writing the
    // same path both legs name makes that agreement structural rather than
    // something a future edit could quietly break.
    if let Err(e) = tokio::fs::write(&joined, content.as_bytes()).await {
        eprintln!("git-vista: {OP} couldn't write '{}': {e}", path.as_str());
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "couldn't write '{}': {e} — the file was NOT changed",
                path.as_str()
            ),
        );
    }

    let add = match run_git(repo, need, &["add", "--", path.as_str()]).await {
        Ok(o) => o,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "the file was written but staging it failed to even run: {e} — \
                     '{}' now holds your resolved content, unstaged; run 'git add' \
                     yourself to finish",
                    path.as_str()
                ),
            )
        }
    };
    if !add.status.success() {
        let msg = stderr_or(&add, "git could not stage the resolved file.");
        eprintln!("git-vista: {OP} wrote the file but staging failed: {msg}");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "the file was written but not staged — {msg} — '{}' now holds your \
                 resolved content, unstaged; run 'git add' yourself to finish",
                path.as_str()
            ),
        );
    }

    println!("[{OP}] resolved {} with content", path.as_str());
    (StatusCode::OK, format!("Resolved {}.", path.as_str()))
}
