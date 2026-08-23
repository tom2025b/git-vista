//! The sequencer executors — cherry-pick, the two-step revert, and the
//! continue/skip/abort verbs that drive a conflicted sequence (M4.28 #81,
//! #327 defect B).
//!
//! # Why this is its own module
//!
//! These are the executors that can leave the repository **mid-sequence** on
//! purpose — a conflicted cherry-pick or revert parks in the sequencer for a
//! human to finish — so they own the machinery of that state and nothing
//! else does: [`sequence_in_progress`] (which sequence are we in?), the
//! [`SequenceVerb`]s that move it, the mainline check shared by both
//! multi-parent forms ([`refuse_mainline_mismatch`] over [`parent_count`]),
//! and the classification of a failed revert's own words
//! ([`looks_like_revert_conflict`]). Merge/rebase conflicts are a different
//! animal — they park in MERGE_HEAD/rebase state, not the sequencer — and
//! stay with [`super::branch_exec`].

use std::path::Path;

use axum::http::StatusCode;

use git_vista_protocol::CommitOid;

use git_vista_core::activity::ActivityKind;

use crate::git_cmd::rev_parse;
use crate::sandbox::NetworkNeed;

use super::{
    couldnt_run, git, journal_app_event, read_head_branch_blocking, run_git, short, stderr_or, Obs,
    Observed,
};

/// Which way a sequence is being driven (M4.28, #81).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SequenceVerb {
    Continue,
    Skip,
    Abort,
}

impl SequenceVerb {
    fn flag(self) -> &'static str {
        match self {
            SequenceVerb::Continue => "--continue",
            SequenceVerb::Skip => "--skip",
            SequenceVerb::Abort => "--abort",
        }
    }
}

/// Which sequence, if any, the repository is in the middle of.
///
/// Read from git's own markers rather than taken from the caller. A caller
/// that guessed wrong would run the wrong verb's continue, and there is no
/// reason to invite that when the repository already knows.
///
/// `None` means neither is in progress — a refusal, not a fallback.
async fn sequence_in_progress(repo: &Path) -> Option<&'static str> {
    let git_dir = repo.join(".git");
    // Order matters only for the impossible case of both existing at once,
    // which git does not produce; cherry-pick first is arbitrary and harmless.
    for (marker, verb) in [
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        if tokio::fs::try_exists(git_dir.join(marker))
            .await
            .unwrap_or(false)
        {
            return Some(verb);
        }
    }
    None
}

/// Drive an in-progress cherry-pick or revert forward, past, or backward
/// (M4.28, #81).
///
/// # Refuses when nothing is in progress
///
/// Git's own message for that case is terse and, worse, `--abort` on a clean
/// repository can succeed while doing nothing — so a caller could be told an
/// abort "worked" when there was never anything to abort. The refusal here is
/// what makes the answer mean something.
///
/// # Continue and skip re-read the conflict state; abort does not
///
/// A `--continue` that leaves conflicts behind has not finished, for the same
/// reason a stash pop that leaves conflicts has not. Abort is different: it is
/// *supposed* to end with a clean tree, so the honest check after it is simply
/// whether it succeeded.
pub(super) async fn exec_sequence(
    repo: &Path,
    need: NetworkNeed,
    verb: SequenceVerb,
) -> (StatusCode, String) {
    let Some(kind) = sequence_in_progress(repo).await else {
        return (
            StatusCode::CONFLICT,
            format!(
                "There is no cherry-pick or revert in progress, so there is nothing to \
                 {}. If you expected one, it may have already finished or been \
                 aborted.",
                match verb {
                    SequenceVerb::Continue => "continue",
                    SequenceVerb::Skip => "skip",
                    SequenceVerb::Abort => "abort",
                }
            ),
        );
    };

    let output = match run_git(repo, need, &[kind, verb.flag()]).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/sequence", &e),
    };

    if verb == SequenceVerb::Abort {
        return if output.status.success() {
            println!("[/api/sequence] aborted the {kind}");
            (
                StatusCode::OK,
                format!(
                    "Aborted the {kind}. The repository is back where the sequence \
                     started, and any conflict resolutions made during it are gone."
                ),
            )
        } else {
            let msg = stderr_or(&output, "git could not abort the sequence.");
            eprintln!("git-vista: /api/sequence abort failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        };
    }

    let continuation = crate::conflicts::continuation(repo).await;
    match (output.status.success(), continuation) {
        (true, Ok(c)) if c.may_continue() => {
            println!("[/api/sequence] {kind} {}", verb.flag());
            let done = sequence_in_progress(repo).await.is_none();
            (
                StatusCode::OK,
                if done {
                    format!("The {kind} is complete.")
                } else {
                    format!("Moved the {kind} on. More commits remain in the sequence.")
                },
            )
        }

        (_, Err(why)) => (
            StatusCode::BAD_REQUEST,
            format!(
                "git {kind} {} ran, but the conflict state could not be read afterwards \
                 — {why}. Check `git status` before continuing.",
                verb.flag()
            ),
        ),

        (_, Ok(c)) => {
            let detail = match &c {
                git_vista_protocol::conflict::Continuation::Blocked { unresolved, .. } => {
                    if unresolved.is_empty() {
                        String::new()
                    } else {
                        format!("\n\nStill conflicted:\n  {}", unresolved.join("\n  "))
                    }
                }
                git_vista_protocol::conflict::Continuation::Clear => String::new(),
            };
            (
                StatusCode::CONFLICT,
                format!(
                    "The {kind} still has conflicts, so it did not move on.{detail}\n\n\
                     Resolve them and try again, or abort to unwind the whole sequence."
                ),
            )
        }
    }
}

/// `git cherry-pick [-m <mainline>] <commit>` (M4.28, #81).
///
/// # A conflict is a pause, not a failure
///
/// The revert path next door `--abort`s on conflict, and that was correct when
/// it was written: there was nowhere to send a conflict. M4.31 (#84) changed
/// that. So a conflicting cherry-pick is left IN PLACE, with the sequencer
/// state intact, and reported with the conflicted paths named — because the
/// resolution path now exists and aborting would throw away work the user can
/// finish.
///
/// That is the difference #81 depends on #84 for.
pub(super) async fn exec_cherry_pick(
    repo: &Path,
    need: NetworkNeed,
    commit: &CommitOid,
    mainline: Option<std::num::NonZeroU8>,
) -> (StatusCode, String) {
    let commit = commit.as_str();

    if let Some(refusal) =
        refuse_mainline_mismatch(repo, need, commit, mainline, "cherry-picking").await
    {
        return refusal;
    }

    let mainline_flag = mainline.map(|m| m.get().to_string());
    let mut argv: Vec<&str> = vec!["cherry-pick"];
    if let Some(m) = mainline_flag.as_deref() {
        argv.push("-m");
        argv.push(m);
    }
    argv.push(commit);

    let output = match run_git(repo, need, &argv).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/cherry-pick", &e),
    };

    // Asked in both branches for the same reason pop does: a cherry-pick git
    // called successful while leaving conflicted paths would be the case this
    // check exists for.
    let continuation = crate::conflicts::continuation(repo).await;

    match (output.status.success(), continuation) {
        (true, Ok(c)) if c.may_continue() => {
            println!("[/api/cherry-pick] picked {}", short(commit));
            (
                StatusCode::OK,
                format!("Cherry-picked {} onto this branch.", short(commit)),
            )
        }

        // Same posture as pop and branch-from-stash: never claim completion on
        // a check that could not be made.
        (_, Err(why)) => (
            StatusCode::BAD_REQUEST,
            format!(
                "git cherry-pick ran, but the conflict state could not be read \
                 afterwards — {why}. Check `git status` before continuing."
            ),
        ),

        (_, Ok(c)) => {
            let detail = match &c {
                git_vista_protocol::conflict::Continuation::Blocked { unresolved, .. } => {
                    if unresolved.is_empty() {
                        String::new()
                    } else {
                        format!("\n\nConflicted:\n  {}", unresolved.join("\n  "))
                    }
                }
                git_vista_protocol::conflict::Continuation::Clear => String::new(),
            };
            eprintln!("[/api/cherry-pick] {} left conflicts", short(commit));
            (
                StatusCode::CONFLICT,
                format!(
                    "Cherry-picking {} left conflicts, so it is NOT complete.{detail}\n\n\
                     The cherry-pick is still in progress — resolve the paths above and \
                     it can continue, or abort it to unwind.",
                    short(commit)
                ),
            )
        }
    }
}

/// Refuse when a `mainline` and a commit's actual shape disagree (M4.28, #81).
///
/// Shared by revert and cherry-pick because both take `-m`, both refuse in the
/// same two directions, and a second copy of this reasoning would be a second
/// place for it to drift. `verb` is the word the message uses so each caller
/// reads naturally.
///
/// # The messages name the DECISION, not the flag
///
/// Git's own refusal — "commit <sha> is a merge but no -m option was given" —
/// is accurate and nearly useless to someone who has not done this before: it
/// names an option, when what the user is missing is a choice. So these say
/// which choice, and what the usual answer is.
///
/// # An unreadable parent count falls through
///
/// `None` from [`parent_count`] means this server does not know whether the
/// commit is a merge. Refusing there would block a legitimate operation on a
/// read WE failed to make, so the caller proceeds and lets git decide — worst
/// case the user gets git's terser wording, which is where they were before.
async fn refuse_mainline_mismatch(
    repo: &Path,
    need: NetworkNeed,
    commit: &str,
    mainline: Option<std::num::NonZeroU8>,
    verb: &str,
) -> Option<(StatusCode, String)> {
    match (parent_count(repo, need, commit).await, mainline) {
        (Some(n), None) if n > 1 => Some((
            StatusCode::BAD_REQUEST,
            format!(
                "{} is a merge commit, so {verb} it needs one more answer: which side \
                 of the merge is the history you are keeping?\n\nThat side is kept; \
                 everything the other side brought in is what changes. Usually the \
                 answer is the branch you were on when you merged, which is parent 1.",
                short(commit)
            ),
        )),
        // `n <= 1`, not `n == 1`: a ROOT commit has ZERO parents and is just as
        // much "not a merge" as an ordinary one. Written as == 1 first, which
        // sent a root commit down the parent-does-not-exist arm and produced
        // "has 0 parents, so parent 1 does not exist" — true, and the wrong
        // explanation. Caught by a test, not by review.
        (Some(n), Some(_)) if n <= 1 => Some((
            StatusCode::BAD_REQUEST,
            format!(
                "{} is not a merge commit — it has {n} parent(s), so there is no side \
                 to choose.",
                short(commit)
            ),
        )),
        (Some(n), Some(m)) if usize::from(m.get()) > n => Some((
            StatusCode::BAD_REQUEST,
            format!(
                "{} has {n} parents, so parent {m} does not exist.",
                short(commit)
            ),
        )),
        _ => None,
    }
}

/// How many parents `commit` has, or `None` if the read failed.
///
/// `None` is deliberately NOT "one parent". A failed read means this server
/// does not know whether the commit is a merge, and the caller falls through
/// to let git decide rather than refusing on a check it could not make —
/// refusing there would block a legitimate revert on our own failure.
///
/// Local (D3): reading a commit header walks the object database.
async fn parent_count(repo: &Path, need: NetworkNeed, commit: &str) -> Option<usize> {
    let out = run_git(repo, need, &["rev-list", "--parents", "-n", "1", commit])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `rev-list --parents -n 1` prints "<commit> <parent>..." on one line, so
    // the parent count is the field count minus the commit itself.
    let line = String::from_utf8_lossy(&out.stdout);
    let fields = line.split_whitespace().count();
    fields.checked_sub(1)
}

/// Two-step revert (`/api/undo`) — the history-preserving undo; a failed
/// revert is auto-aborted (like `/api/rebase`) so a browser-only user is
/// never left mid-revert.
///
/// Why two steps and not `git revert --no-edit <commit>`: whenever the
/// revert's diff against HEAD is empty — an empty commit, a commit whose
/// changes are not present in HEAD (orphan lineage, hit live by the owner
/// with `84570fe`), or a change already reverted — the single-step form
/// fails with "nothing to commit, working tree clean" (#308). The flag that
/// fixes it, `revert --allow-empty`, landed in git 2.45; this box runs
/// 2.43. `revert --no-commit` + `commit --allow-empty --no-edit` expresses
/// the same intent on 2.43, and `--no-edit` reuses the message `--no-commit`
/// staged, so the resulting commit is byte-for-byte what the single-step
/// form produces on newer git.
///
/// Cleanup is `git revert --abort` at EITHER failure point, and that is a
/// verified fact rather than an assumption: git clears `REVERT_HEAD` only on
/// a SUCCESSFUL commit, so a step-2 failure (rejecting hook, signing
/// failure) leaves exactly the same sequencer state a conflicted step-1
/// does, and `--abort` restores the pre-revert tree identically in both
/// cases. Confirmed empirically on git 2.43.0 with a rejecting pre-commit
/// hook before this code was written.
///
/// # #327 defect B: a conflict here is an outcome, not a wire dump
///
/// Since #325's `undoables` precheck (`activity::revert_would_conflict`)
/// this arm should be rare in practice — most conflicting reverts are no
/// longer offered in the first place. It still has to be handled well,
/// because the precheck is advisory, not a lock: the repository can move
/// between the `GET /api/undoables` that offered the action and the
/// `POST /api/undo` that runs it (a concurrent push, another undo, a
/// terminal command), and a root commit's revert skips the precheck
/// entirely (see that function's doc comment).
///
/// Before this fix, step 1's failure — including a genuine conflict —
/// forwarded git's raw stderr verbatim at a generic `400`, indistinguishable
/// from every other kind of revert failure and never reaching the user as
/// words a browser-only client could act on (the operations status strip
/// showed the dump, but nothing said "this is fine, try something else").
/// [`looks_like_revert_conflict`] classifies it the same way ADR 0044
/// classifies a failed pull's integration half: a conflict is reported at
/// `409` with a sentence explaining what happened and that nothing was
/// changed, everything else still forwards git's own words verbatim exactly
/// as before (this repo's established posture for "git's call, not ours" —
/// see `exec_create_branch`'s doc comment).
pub(super) async fn exec_revert(
    repo: &Path,
    need: NetworkNeed,
    commit: &CommitOid,
    mainline: Option<std::num::NonZeroU8>,
    observed: &Observed,
) -> (StatusCode, String) {
    let commit = commit.as_str();

    if let Some(refusal) = refuse_mainline_mismatch(repo, need, commit, mainline, "undoing").await {
        return refusal;
    }

    let mainline_flag = mainline.map(|m| m.get().to_string());
    let mut argv: Vec<&str> = vec!["revert", "--no-commit"];
    if let Some(m) = mainline_flag.as_deref() {
        argv.push("-m");
        argv.push(m);
    }
    argv.push(commit);

    // Step 1: compute the revert into the index without committing.
    if let Err(msg) = git(repo, need, &argv).await {
        // A conflicted (or otherwise failed) --no-commit leaves sequencer
        // state (REVERT_HEAD) and possibly conflict markers; --abort is
        // git's own cleanup for exactly that. Harmless when no revert is
        // in progress.
        let _ = git(repo, need, &["revert", "--abort"]).await;
        eprintln!("git-vista: /api/undo revert (compute) failed (aborted): {msg}");
        return revert_step1_failure_response(commit, &msg);
    }

    // Step 2: finish the revert as its own commit, explicitly allowing an
    // empty one — the step the single command cannot express on git < 2.45.
    match git(repo, need, &["commit", "--allow-empty", "--no-edit"]).await {
        Ok(()) => {
            println!("[/api/undo] reverted {}", short(commit));
            let new = Obs::from_read(rev_parse(repo, "HEAD").await);
            let branch = read_head_branch_blocking(repo)
                .await
                .unwrap_or_else(|| "HEAD".into());
            journal_app_event(
                repo,
                ActivityKind::Revert,
                Some(branch),
                observed.head_tip.clone(),
                new,
                format!("reverted {}", short(commit)),
            )
            .await;
            (StatusCode::OK, format!("Reverted {}.", short(commit)))
        }
        Err(msg) => {
            // Computed but not committed (hook/signing/other). REVERT_HEAD
            // is still set — git only clears it on a successful commit — so
            // --abort restores the pre-revert tree here exactly as it does
            // for a conflicted step 1.
            let _ = git(repo, need, &["revert", "--abort"]).await;
            eprintln!("git-vista: /api/undo revert (commit) failed (aborted): {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// Build the response for a failed revert step 1 (#327 defect B): a `409`
/// with a composed, actionable sentence when git's own words describe a
/// conflict, a `400` forwarding git's words verbatim for everything else —
/// unchanged from this function's behavior before this fix.
///
/// The `409` is deliberate, not incidental: it reuses
/// [`git_vista_protocol::ErrorCode::Conflict`] — the same generic code
/// [`middleware::rewrap_error`](crate::middleware) already assigns any `409`
/// this server sends — so any current or future consumer that switches on
/// the wire envelope's `code` (never its message text, by that type's own
/// contract) already tells this failure apart from an ordinary refusal,
/// with no protocol change. What this function cannot do — because
/// `UndoAction`/`Undoable` live in `git_vista_core`, outside this fix's
/// file set — is thread a *named* `RevertFailureKind` the way
/// [`git_vista_protocol::PullFailureKind`] threads a pull's; see this
/// change's notes for that follow-up.
fn revert_step1_failure_response(commit: &str, git_said: &str) -> (StatusCode, String) {
    if looks_like_revert_conflict(git_said) {
        (
            StatusCode::CONFLICT,
            format!(
                "Reverting {} conflicts with changes made since — something later \
                 in the history still depends on what it changed. Nothing was \
                 applied and the repository is unchanged; the revert was cancelled \
                 automatically. To do it anyway, check out the commit yourself, \
                 revert it there, resolve the conflict by hand, and commit — or \
                 leave the history as it is.\n\nGit's own explanation:\n{git_said}",
                short(commit),
            ),
        )
    } else {
        (StatusCode::BAD_REQUEST, git_said.to_string())
    }
}

/// Whether git's own words for a failed `git revert --no-commit` describe a
/// *conflict*, as opposed to a refusal that never touched the working tree
/// (e.g. a dirty tree: "Your local changes … would be overwritten by
/// merge").
///
/// Same trade `pull::looks_like_conflict` documents (this module's own
/// `pull` submodule, private, so not linkable from here), applied to
/// revert's own vocabulary instead of merge/rebase's: git's exit
/// status carries no classification (`git revert` exits 1 for a conflict and
/// 128 for most other refusals, but nothing here should lean on that
/// distinction being stable — see the tests), and its prose is
/// gettext-translated and version-dependent. So this is a documented marker
/// with `false` (⇒ the raw message forwarded verbatim, exactly as before
/// this fix) as the safe fallback: a mis-tag costs a less specific hint,
/// never a wrong explanation.
///
/// Marker verified against **the exact string from the owner's own session
/// log this fix addresses** (#327) — not a paraphrase — and against git
/// 2.43.0's actual `git revert --no-commit` stderr for both the shape that
/// text came from and the two-hint form the sequencer uses when a revert is
/// left mid-flight (see the tests for both). Every case observed puts the
/// word "conflict" in the hint text git prints right after the summary
/// line, which is what this checks for:
///
/// ```text
/// error: could not revert f993ba6... LangChain - Company Research Agent
/// hint: after resolving the conflicts, mark the corrected paths
/// hint: with 'git add <paths>' or 'git rm <paths>'
/// ```
pub(super) fn looks_like_revert_conflict(text: &str) -> bool {
    text.to_ascii_lowercase().contains("conflict")
}
