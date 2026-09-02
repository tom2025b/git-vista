//! The index-staging executors — stage-all, unstage-all, and the line/hunk
//! selection path (#214, M2.17c).
//!
//! # Why this is its own module
//!
//! These are the only executors whose subject is the **index alone**: nothing
//! here creates a commit, moves a ref, or touches worktree file content.
//! Stage-all and unstage-all are one-command inverses; the selection path is
//! the odd one out in the whole planner — it feeds a client-built patch to
//! `git apply --cached` on stdin rather than pointing git at repository
//! state, which is why it carries its own freshness token
//! (`expected_diff_generation`) instead of the ref-based preconditions every
//! other executor leans on. `hunk_staging_suite` drives
//! [`exec_stage_selection`] directly.

use std::path::Path;

use axum::http::StatusCode;

use crate::sandbox::NetworkNeed;

use git_vista_protocol::plan_export;

use super::{couldnt_run, run_git, run_git_argv, stderr_or};

/// `git add -A` (`/api/stage`).
pub(super) async fn exec_stage_all(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    let output = match run_git_argv(repo, need, &plan_export::stage_all_argv()).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stage", &e),
    };
    if output.status.success() {
        println!("[/api/stage] staged all changes (git add -A)");
        (StatusCode::OK, "Staged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git add failed.");
        eprintln!("git-vista: /api/stage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git apply --cached` of a built selection, then pathspec staging of the
/// whole-file part (M2.17b, #213; `/api/staging/apply`).
///
/// Order is deliberate: the patch leg runs first because it is the leg that
/// can fail (a hunk that no longer applies), and `git apply` is atomic — it
/// refuses the whole patch rather than applying half. The pathspec leg
/// (`git add --` / `git reset -q HEAD --`) after it is near-infallible, so
/// a failure almost always leaves the index wholly untouched. The residual
/// window — patch applied, pathspec then failing — is reported exactly as
/// what happened rather than papered over; the working tree is untouched in
/// every outcome, which is what makes this Safe-risk.
pub(super) async fn exec_stage_selection(
    repo: &Path,
    need: NetworkNeed,
    direction: git_vista_protocol::StageDirection,
    expected_diff_generation: &git_vista_protocol::GenerationToken,
    patch: &str,
    whole_files: &[String],
) -> (StatusCode, String) {
    use git_vista_protocol::StageDirection;
    // The gate, re-run INSIDE the coordinator lock (the handler's ran
    // outside it): re-mint the diff-v1 token and refuse if the base diff
    // moved between gate and execution. Without this, a concurrent write in
    // that window could shift file content and `git apply` would still
    // apply mid-file hunks at drifted offsets — silently staging content
    // the user never previewed.
    match crate::handlers::read::staging_diff_for_repo(repo, direction).await {
        Ok(live) => {
            if let Err(refused) = crate::staging::require_current_selection_token(
                expected_diff_generation,
                &live.generation,
            ) {
                return refused;
            }
        }
        Err(e) => return e,
    }
    let mut done: Vec<String> = Vec::new();
    if !patch.is_empty() {
        // `--recount`: a safety net, not a correctness dependency. The hunk
        // header counts this server builds (`patch_build::append_hunk` for a
        // whole hunk, `append_sub_hunk` for #214's line-level sub-hunks) are
        // computed from the exact lines being emitted, so they are already
        // supposed to be right. `--recount` tells `git apply` to ignore the
        // `@@ -a,b +c,d @@` counts entirely and derive them itself from the
        // body — cheap insurance against an off-by-one in that hand-computed
        // arithmetic (most exposed in `append_sub_hunk`'s three-way
        // context/added/removed bookkeeping) turning into a hard "patch does
        // not apply" or, worse, a hunk applying at the wrong offset. Harmless
        // when the counts already agree with the body, which is every case
        // today.
        let args: &[&str] = match direction {
            StageDirection::Stage => &["apply", "--cached", "--whitespace=nowarn", "--recount"],
            StageDirection::Unstage => &[
                "apply",
                "--cached",
                "--reverse",
                "--whitespace=nowarn",
                "--recount",
            ],
        };
        let output =
            match crate::git_cmd::git_output_with_stdin(repo, args, need, patch.as_bytes()).await {
                Ok(o) => o,
                Err(e) => return couldnt_run("/api/staging/apply", &e),
            };
        if !output.status.success() {
            let mut msg = stderr_or(&output, "git apply failed.");
            // A replacement character in the patch means the file is not
            // valid UTF-8 — the lossy read can never byte-match the blob,
            // so git's "does not apply" is misleading without this.
            if patch.contains('\u{fffd}') {
                msg.push_str(
                    " (the file does not appear to be valid UTF-8 — hunk \
                     staging cannot address it; stage the entire file instead)",
                );
            }
            eprintln!("git-vista: /api/staging/apply patch leg failed: {msg}");
            // Nothing staged: apply is atomic, and the pathspec leg never ran.
            return (StatusCode::BAD_REQUEST, msg);
        }
        done.push("applied the selected hunks".to_string());
    }
    if !whole_files.is_empty() {
        // `git reset -q HEAD -- <path>` exits 0 even when the pathspec
        // matches nothing — a silent false success on a write surface. The
        // stage leg needs no twin check: `git add` of a nonexistent path
        // fails loudly on its own.
        if matches!(direction, StageDirection::Unstage) {
            let mut check: Vec<&str> = vec!["diff", "--cached", "--name-only", "-z", "--"];
            check.extend(whole_files.iter().map(String::as_str));
            let listed = match run_git(repo, need, &check).await {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                Ok(o) => {
                    let msg = stderr_or(&o, "pathspec check failed.");
                    return (StatusCode::BAD_REQUEST, msg);
                }
                Err(e) => return couldnt_run("/api/staging/apply", &e),
            };
            let matched: std::collections::HashSet<&str> =
                listed.split('\0').filter(|p| !p.is_empty()).collect();
            if let Some(missing) = whole_files.iter().find(|p| !matched.contains(p.as_str())) {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("nothing is staged at {missing}, so there is nothing to unstage"),
                );
            }
        }
        let mut args: Vec<&str> = match direction {
            StageDirection::Stage => vec!["add", "--"],
            StageDirection::Unstage => vec!["reset", "-q", "HEAD", "--"],
        };
        args.extend(whole_files.iter().map(String::as_str));
        let output = match run_git(repo, need, &args).await {
            Ok(o) => o,
            Err(e) => return couldnt_run("/api/staging/apply", &e),
        };
        if !output.status.success() {
            let msg = stderr_or(&output, "pathspec staging failed.");
            eprintln!("git-vista: /api/staging/apply pathspec leg failed: {msg}");
            let and_yet = if done.is_empty() {
                String::new()
            } else {
                // The residual non-atomic window, reported as fact.
                " The selected hunks were already applied to the index; \
                 the whole-file part was not."
                    .to_string()
            };
            return (StatusCode::BAD_REQUEST, format!("{msg}{and_yet}"));
        }
        done.push(format!("staged {} file(s) whole", whole_files.len()));
    }
    let verb = match direction {
        StageDirection::Stage => "Staged selection",
        StageDirection::Unstage => "Unstaged selection",
    };
    println!("[/api/staging/apply] {verb}: {}", done.join(", "));
    (StatusCode::OK, format!("{verb}."))
}

/// `git reset -q HEAD` (`/api/unstage`) — the exact inverse of stage-all; the
/// working tree keeps every edit, so nothing is lost.
pub(super) async fn exec_unstage_all(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    let output = match run_git_argv(repo, need, &plan_export::unstage_all_argv()).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/unstage", &e),
    };
    if output.status.success() {
        println!("[/api/unstage] unstaged all changes (git reset -q HEAD)");
        (StatusCode::OK, "Unstaged changes.".to_string())
    } else {
        let msg = stderr_or(&output, "git reset failed.");
        eprintln!("git-vista: /api/unstage failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}
