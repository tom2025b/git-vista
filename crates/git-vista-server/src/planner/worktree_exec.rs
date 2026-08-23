//! The worktree-destroying executors — reset a test repo to its seed
//! (M2.16 #206), discard tracked changes, and delete untracked files
//! (#219, hardened by the #71 audit and #284).
//!
//! # Why this is its own module
//!
//! Everything here destroys worktree state that **no git ref protects** —
//! the one category of operation this server cannot undo — so these
//! executors share a posture nothing else needs: never trust one exit code,
//! re-observe the worktree afterwards and report exactly what was proven
//! ([`DiscardOutcome`], [`DeleteOutcome`] and their observers), because
//! git's multi-path pathspecs are not atomic and a partial failure must
//! never be folded into success.
//!
//! The path-state gates the executors call — `verify_path_states`,
//! `symlink_containment_guard`, `classify_path_states`, `PathKind` — stay in
//! `planner.rs` on purpose: `shape` re-verifies path states at plan time and
//! [`super::conflict_exec`] guards its content write with the same symlink
//! containment check, so the parent owns them rather than one sibling
//! importing safety gates from another.

use std::collections::HashMap;
use std::path::Path;

use axum::http::StatusCode;

use git_vista_core::activity::ActivityKind;
use git_vista_core::seed::reset_plan;

use git_vista_protocol::WorktreePath;

use crate::git_cmd::{git_ok, rev_parse};
use crate::journal;
use crate::sandbox::NetworkNeed;

use super::{
    classify_path_states, couldnt_run, journal_app_event, journal_clear_blocking, read_seed,
    run_git, stderr_or, symlink_containment_guard, verify_path_states, Obs, PathKind,
};

/// Reset a *test repo* to its recorded seed (`/api/reset-test-repo`): move
/// every seeded branch back to its recorded tip, check out the seeded HEAD
/// branch, force the worktree clean, DELETE branches the seed doesn't know —
/// allowed nowhere else in git-vista — and wipe the app journal (its events
/// describe history that no longer exists). Hard-gated: only a repo
/// explicitly opted in with `gv --seed <path>` has seed files.
pub(super) async fn exec_reset_test_repo(repo: &Path, need: NetworkNeed) -> (StatusCode, String) {
    // `need` is threaded for the same reason every other `exec_*` threads it —
    // so the declared value reaches `policy_for` — but this operation's git
    // steps go through `git_cmd::git_ok`, which declares `Local` at its own
    // seam (see its comment). Consume it explicitly so a future edit that adds
    // a `run_git` step here has the right value already in scope.
    let _ = need;
    let seed = match read_seed(repo) {
        None => {
            return (
                StatusCode::NOT_FOUND,
                "This repo has no recorded seed — it isn't marked as a test repo. \
                 Run `gv --seed <path>` once on the server machine to record its reset point."
                    .to_string(),
            )
        }
        Some(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The recorded seed is corrupt ({e}) — re-record it with `gv --seed`."),
            )
        }
        Some(Ok(seed)) => seed,
    };

    // Objects first, verification second: unbundle is best-effort (idempotent,
    // cheap), then every seeded tip must resolve or the reset refuses to start —
    // never a half-restore.
    if let Some(dir) = journal::state_dir(repo) {
        let bundle = dir.join("seed.bundle");
        if bundle.exists() {
            let _ = git_ok(repo, &["bundle", "unbundle", &bundle.display().to_string()]).await;
        }
    }
    for r in &seed.refs {
        match rev_parse(repo, &format!("{}^{{commit}}", r.oid)).await {
            Ok(Some(_)) => {}
            // git ran and could not find the object: a real, reportable
            // problem with the seed.
            Ok(None) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "Seed commit {} for ‘{}’ no longer exists in this repo — \
                         re-record the seed with `gv --seed`.",
                        &r.oid[..7],
                        r.name
                    ),
                )
            }
            // D5: git never ran. Telling the operator to re-record a seed that
            // is probably intact would send them to destroy the one recovery
            // point this endpoint restores from. Refuse without a diagnosis.
            Err(e) => {
                return couldnt_run(
                    "/api/reset-test-repo",
                    &format!("couldn't verify seed commit for ‘{}’: {e}", r.name),
                )
            }
        }
    }

    // What the repo looks like NOW, then the pure plan of moves + deletions.
    let current_refs = match run_git(
        repo,
        need,
        &[
            "for-each-ref",
            "refs/heads",
            "--format=%(objectname) %(refname:short)",
        ],
    )
    .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                l.split_once(' ')
                    .map(|(oid, name)| (name.to_string(), oid.to_string()))
            })
            .collect::<Vec<_>>(),
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't list the repo's current branches.".to_string(),
            )
        }
    };
    let plan = reset_plan(&seed, &current_refs);

    // Apply, in an order where each step makes the next valid: refs back first
    // (so the seed HEAD branch exists at the right tip), then a forced checkout
    // + hard reset + clean (so HEAD is off any branch about to be deleted and
    // the worktree matches the seed exactly), then the deletions.
    for r in &plan.update {
        if let Err(e) = git_ok(
            repo,
            &["update-ref", &format!("refs/heads/{}", r.name), &r.oid],
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped while restoring ‘{}’: {e}", r.name),
            );
        }
    }
    for step in [
        &["checkout", "-f", seed.head.as_str()] as &[&str],
        &["reset", "--hard"],
        &["clean", "-fd"],
    ] {
        if let Err(e) = git_ok(repo, step).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Reset stopped at `git {}`: {e}", step.join(" ")),
            );
        }
    }
    let mut deleted = 0;
    for name in &plan.delete {
        // The ONLY place git-vista deletes a branch: a seeded test repo, inside
        // an explicit reset, for branches created after the seed was recorded.
        match git_ok(repo, &["branch", "-D", name]).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Reset stopped deleting test branch ‘{name}’: {e}"),
                )
            }
        }
    }
    // The journal now describes history that no longer exists (dead undo
    // targets included) — wipe it with the snapshot; both regenerate.
    journal_clear_blocking(repo).await;

    let msg = format!(
        "Reset to seed: {} branch(es) restored, {} deleted, HEAD → ‘{}’, working tree clean.",
        plan.update.len(),
        deleted,
        seed.head
    );
    println!("[/api/reset-test-repo] {msg}");
    (StatusCode::OK, msg)
}

/// What the worktree says happened to each requested tracked path,
/// determined by re-classifying every one of them against a **fresh** `git
/// status` taken immediately after `git checkout HEAD --` returns —
/// regardless of its exit code.
///
/// **Why this exists at all (#71 audit, the reproduced bug).** `git
/// checkout HEAD -- p1 p2 p3` is NOT atomic across a multi-path pathspec:
/// verified empirically, a permission error on a *later* path in the
/// batch still lets an *earlier* one revert before the process exits
/// non-zero. Before this fix, a non-zero exit short-circuited straight to a
/// `BAD_REQUEST` with git's own stderr and **no journal entry at all** — so
/// the one path that really did revert went completely unrecorded. This is
/// the discard-side twin of [`DeleteOutcome`]/[`observe_deletion`]: same
/// reasoning (trust the worktree, not one exit code; report exactly what
/// was proven), different question (tracked-and-dirty-or-not here,
/// present-on-disk-or-not there) because that is what each command's
/// pathspec atomicity failure actually leaves behind.
#[derive(Debug, PartialEq, Eq)]
struct DiscardOutcome<'a> {
    /// No longer classified [`PathKind::TrackedDirty`] after the attempt:
    /// this operation actually reverted it.
    reverted: Vec<&'a str>,
    /// Still [`PathKind::TrackedDirty`] after the attempt: `git checkout`
    /// did not reach it — an aborted multi-path batch, most likely.
    still_dirty: Vec<&'a str>,
}

/// Partition `requested` by the live classification `live` reports right
/// now — pure and synchronous so it is unit-testable without a real git
/// process, same split as [`observe_deletion`]/[`present_paths`] on the
/// delete side.
fn observe_discard<'a>(
    live: &HashMap<String, PathKind>,
    requested: &[&'a str],
) -> DiscardOutcome<'a> {
    let mut outcome = DiscardOutcome {
        reverted: Vec::new(),
        still_dirty: Vec::new(),
    };
    for p in requested.iter().copied() {
        match live.get(p).copied().unwrap_or(PathKind::Other) {
            PathKind::TrackedDirty => outcome.still_dirty.push(p),
            _ => outcome.reverted.push(p),
        }
    }
    outcome
}

impl DiscardOutcome<'_> {
    /// The whole client-facing outcome — status, response body, journal
    /// line — derived from what the worktree proved, not from git's exit
    /// code alone. `git_err`, when `git checkout` itself failed, is folded
    /// into the message so the user still sees git's own explanation.
    ///
    /// Names the exact paths in both the success and partial cases (#71
    /// audit item 3) — the durable journal already carries the full
    /// `requested` list via [`observe_discard`]'s caller, so this is
    /// exposing data already in scope, not new storage.
    fn report(&self, git_err: Option<&str>) -> (StatusCode, String, String) {
        if self.still_dirty.is_empty() {
            let count = self.reverted.len();
            let s = if count == 1 { "" } else { "s" };
            let names = self.reverted.join(", ");
            let journal = format!(
                "discarded uncommitted changes to {count} tracked path{s} ({names}) — \
                 recoverable only for content staged before this ran, and only until \
                 git gc; a worktree-only edit is gone"
            );
            let body = format!(
                "Discarded uncommitted changes to {count} tracked path{s}: {names}. \
                 Recoverable only for content that was staged before this ran, and \
                 only until the next git gc — a worktree-only edit is gone."
            );
            return (StatusCode::OK, body, journal);
        }
        // Partial (or total) failure: refusing now cannot re-revert what
        // already failed, so what this can still do is name exactly what
        // happened to every requested path instead of a count — or worse,
        // silence — that does not match reality.
        let still_dirty_list = self.still_dirty.join(", ");
        let still_dirty_verb = if self.still_dirty.len() == 1 {
            "was"
        } else {
            "were"
        };
        let reverted_note = if self.reverted.is_empty() {
            "nothing was reverted".to_string()
        } else {
            format!(
                "{} {} reverted — recoverable only for content staged before this ran, \
                 and only until git gc",
                self.reverted.join(", "),
                if self.reverted.len() == 1 {
                    "was"
                } else {
                    "were"
                },
            )
        };
        let reason = git_err
            .map(|e| format!(" git said: {e}"))
            .unwrap_or_default();
        let msg = format!(
            "Partial result: {reverted_note}, but {still_dirty_list} {still_dirty_verb} \
             not — nothing further was applied to {still_dirty_list}; re-check its \
             status before retrying.{reason}"
        );
        let journal = format!("discard-tracked-paths partial result — {msg}");
        (StatusCode::CONFLICT, msg, journal)
    }
}

/// `git checkout -- <paths>` (`/api/discard-tracked-paths`, #219): discard
/// uncommitted changes to already-tracked paths, restoring each to its
/// checked-out (index, else HEAD) version. See
/// [`GitOperation::DiscardTrackedPaths`](git_vista_protocol::GitOperation::DiscardTrackedPaths)'s doc comment for the exact,
/// qualified recovery story this response/journal text spells out — this is
/// destructive, and only *sometimes* undoable outside git-vista.
pub(super) async fn exec_discard_tracked_paths(
    repo: &Path,
    need: NetworkNeed,
    paths: &[WorktreePath],
) -> (StatusCode, String) {
    if let Err(refused) = symlink_containment_guard(repo, paths, "/api/discard-tracked-paths").await
    {
        return refused;
    }
    if let Err(refused) = verify_path_states(
        repo,
        need,
        paths,
        PathKind::TrackedDirty,
        "/api/discard-tracked-paths",
    )
    .await
    {
        return refused;
    }
    // `git checkout HEAD -- <paths>`, not the bare `git checkout -- <paths>`
    // the issue's own shorthand suggested: bare `checkout --` only resets
    // the worktree to the INDEX, so a path whose only difference is staged
    // (index != HEAD, worktree == index) is a silent no-op — verified
    // empirically before this fix landed (review finding: it returned 200
    // and journaled "discarded" while the git command changed nothing).
    // `checkout HEAD --` resets both index and worktree to HEAD, discarding
    // staged and unstaged changes alike, which is what "discard uncommitted
    // changes" means to a caller regardless of staging state — and the
    // staged blob (if any) still survives as a dangling object until the
    // next `git gc`, confirmed with `git fsck --unreachable`, so the
    // recovery-story text below stays true either way.
    let mut args: Vec<&str> = vec!["checkout", "HEAD", "--"];
    args.extend(paths.iter().map(WorktreePath::as_str));
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/discard-tracked-paths", &e),
    };
    let git_err = if output.status.success() {
        None
    } else {
        Some(stderr_or(&output, "git checkout failed."))
    };

    // #71 audit items 1/2: never trust the exit code alone. `git checkout
    // HEAD -- <paths>` is not atomic across a multi-path pathspec (this
    // module's doc comment above has the empirical proof), so re-classify
    // every requested path against a fresh `git status` and report exactly
    // what the worktree proves — whether the process exited 0 or not.
    let requested: Vec<&str> = paths.iter().map(WorktreePath::as_str).collect();
    let live_status = run_git(repo, need, &["status", "--porcelain=v2", "-z"]).await;
    let (status, body, summary) = match live_status {
        Ok(o) if o.status.success() => {
            let live = classify_path_states(&git_vista_protocol::parse_porcelain_v2_z(&o.stdout));
            let outcome = observe_discard(&live, &requested);
            outcome.report(git_err.as_deref())
        }
        _ => {
            // Could not re-verify what actually happened — fail safe by
            // saying so plainly rather than trusting the checkout's exit
            // code alone (exactly what item 2 exists to stop) or guessing.
            let requested_list = requested.join(", ");
            let msg = match &git_err {
                Some(e) => format!(
                    "git checkout failed ({e}) and the worktree could not be \
                     re-verified afterwards, so which of {requested_list} actually \
                     reverted is unknown — check status manually before retrying."
                ),
                None => format!(
                    "git checkout exited successfully but the worktree could not be \
                     re-verified afterwards, so whether {requested_list} actually \
                     reverted is unconfirmed."
                ),
            };
            let journal = format!("discard-tracked-paths could not be verified — {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, msg, journal)
        }
    };
    if status == StatusCode::OK {
        println!("[/api/discard-tracked-paths] {summary}");
    } else {
        eprintln!("git-vista: /api/discard-tracked-paths partial/failed: {summary}");
    }
    journal_app_event(
        repo,
        ActivityKind::Other,
        None,
        Obs::Absent,
        Obs::Absent,
        summary,
    )
    .await;
    (status, body)
}

/// After `git clean -f -- <paths>` has run, ask the **filesystem** which of
/// `requested` is actually gone, and build an honest partial-result message
/// naming any that survived — `None` when every requested path really was
/// removed.
///
/// **Why the filesystem and not `git clean`'s stdout (#284).** Until #284
/// this parsed `git clean`'s own output for `Removing <path>`. That string is
/// passed through gettext in git's source, so it is translated whenever a
/// `git.mo` catalog is installed and `LANG`/`LC_MESSAGES` names a non-English
/// locale — and production spawns inherit the server's environment in full,
/// because `sandbox::spawn`'s `env_clear`/`env` are `#[cfg(test)]`-only *by
/// design* (argv and env cannot change after policy classification). Under
/// `LANG=fr_FR.UTF-8` with translations installed, three successfully deleted
/// files matched no prefix, all three looked un-deleted, and the endpoint
/// returned 409 telling the user their files had survived — after they were
/// irreversibly gone. That is the exact inversion of the property this
/// function exists to provide, so the parse is gone: a dirent that is still
/// there was not deleted, in every language.
///
/// **`symlink_metadata`, not `Path::exists`.** `exists()` follows the link, so
/// a *dangling* symlink — one whose target is already gone — reports as
/// absent while its dirent is still sitting in the worktree. `git clean` can
/// and does delete dangling symlinks, so both "clean removed it" and "clean
/// skipped it" would look identical to `exists()`, reintroducing a false
/// success in the narrow case. `symlink_metadata` stats the entry itself and
/// tells the two apart. (Same reason `symlink_containment_guard` uses it.)
///
/// **What this can still get wrong, stated plainly.** If something *else*
/// deleted a requested path in the same window, we cannot see that it was
/// not us. See [`DeleteOutcome`] for how much of that window the
/// before-snapshot closes and what is left.
///
/// A stat error other than "not found" (an unreadable parent directory, say)
/// counts as absent, deliberately: presence is the claim that has to be
/// *proved* here, and an error proves nothing.
///
/// Synchronous `stat` calls in an async fn, deliberately: one per requested
/// path, bounded by the request, on entries `symlink_containment_guard` and
/// `verify_path_states` stat'd microseconds earlier — not worth a
/// `spawn_blocking` hop and the join-error branch that comes with it.
fn present_paths<'a>(repo: &Path, requested: &[&'a str]) -> Vec<&'a str> {
    requested
        .iter()
        .copied()
        .filter(|p| std::fs::symlink_metadata(repo.join(p)).is_ok())
        .collect()
}

/// What the worktree says happened to each requested path, split three ways
/// by comparing a presence snapshot taken immediately *before* the `git
/// clean` spawn against one taken immediately after.
///
/// **Why three buckets and not two (#284, review finding).** The first cut of
/// this only looked at the worktree *after* the spawn, so "absent now" was
/// read as "we deleted it". That silently credits this operation with a
/// deletion it did not perform: `git clean -f -- a.txt b.txt` exits 0 and
/// says nothing when `b.txt` is already gone (verified directly against real
/// git), so a second git-vista tab, a shell `rm`, or an editor auto-clean
/// removing `b.txt` first produced a 200 reading "Deleted 2 untracked paths
/// permanently" — and, worse, a *journal* entry saying the same. The journal
/// is the durable record for the one operation in this vocabulary with no
/// undo of any kind; an entry claiming a destruction we did not cause is a
/// corrupt audit trail, not a rounding error.
///
/// **How much window this actually closes, stated plainly.** Not all of it.
/// [`verify_path_states`] already refuses a path that has vanished by the
/// time its `git status` runs (a missing path classifies as
/// [`PathKind::Other`], never [`PathKind::Untracked`]), so the exposure was
/// always the gap between that read and `git clean`'s own `unlink`. The
/// before-snapshot moves the near edge of that gap from "before a `git
/// status` subprocess spawn, a porcelain parse, and a `git clean` subprocess
/// spawn" to "after all of those, one `stat` before the spawn" — milliseconds
/// down to whatever elapses inside `git clean` itself. What remains is an
/// external deleter landing *inside* the child process's own run. Closing
/// that last sliver needs a repo-wide exclusive lock this endpoint does not
/// hold, which is a different and much larger decision; narrowing it by three
/// orders of magnitude costs one `stat` per requested path on entries that
/// were stat'd twice already.
///
/// **The bias is unchanged and deliberate.** A path still on disk is always
/// reported as a survivor, whoever put it there. This can still never claim a
/// destroyed file survived — the inversion #284 was filed about, and the only
/// failure direction that makes a user stop looking for data that is gone for
/// good.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DeleteOutcome<'a> {
    /// Present before the spawn, absent after: this operation removed it.
    /// The count the response and the journal are allowed to claim.
    pub(super) deleted: Vec<&'a str>,
    /// Absent before the spawn and absent after: gone for good, but not by
    /// our hand. Reported, never counted as ours.
    pub(super) already_gone: Vec<&'a str>,
    /// Still in the worktree: not deleted, whatever it points at. `git clean`
    /// silently skips a path that has become tracked since the pre-flight
    /// check (no error, exit 0), which is how this bucket normally fills.
    pub(super) survived: Vec<&'a str>,
}

/// Compare the before-snapshot against the live worktree. `present_before`
/// comes from [`present_paths`] called immediately before the spawn.
pub(super) fn observe_deletion<'a>(
    repo: &Path,
    requested: &[&'a str],
    present_before: &[&'a str],
) -> DeleteOutcome<'a> {
    let present_after = present_paths(repo, requested);
    let mut outcome = DeleteOutcome {
        deleted: Vec::new(),
        already_gone: Vec::new(),
        survived: Vec::new(),
    };
    for p in requested.iter().copied() {
        if present_after.contains(&p) {
            outcome.survived.push(p);
        } else if present_before.contains(&p) {
            outcome.deleted.push(p);
        } else {
            outcome.already_gone.push(p);
        }
    }
    outcome
}

impl DeleteOutcome<'_> {
    /// The 409 body when some requested path is still on disk — `None` when
    /// nothing survived. Refusing now cannot un-delete what already went, so
    /// what this can still do is name exactly what happened instead of a
    /// count that does not match reality.
    pub(super) fn partial_refusal(&self) -> Option<String> {
        if self.survived.is_empty() {
            return None;
        }
        let survived_list = self.survived.join(", ");
        let survived_verb = if self.survived.len() == 1 {
            "was"
        } else {
            "were"
        };
        let destroyed = if self.deleted.is_empty() {
            "Partial result: nothing was deleted".to_string()
        } else {
            format!(
                "Partial result: {} {} deleted permanently",
                self.deleted.join(", "),
                if self.deleted.len() == 1 {
                    "was"
                } else {
                    "were"
                }
            )
        };
        let mut msg = format!(
            "{destroyed}, but {survived_list} {survived_verb} not — its state changed \
             (likely became tracked) in the instant between the pre-flight check and \
             this running. Nothing further was applied for {survived_list}; re-check \
             its status before retrying."
        );
        msg.push_str(&self.already_gone_note());
        Some(msg)
    }

    /// The whole client-facing outcome — status, response body, journal line
    /// — derived from nothing but what the worktree proved.
    ///
    /// **Why this composes the message instead of the executor (review
    /// finding).** The count started life as `paths.len()` in the executor,
    /// which is what defect 2 of #284 fixed for duplicates and what the
    /// before-snapshot fixes for foreign deletions. Both are the same mistake:
    /// counting what was *asked for* rather than what was *observed*. While
    /// that arithmetic lived inline in an `async fn` that does its own
    /// `stat`ing, no test could reach a state where the two counts differ, so
    /// reverting it to `paths.len()` passed the entire suite — a green test
    /// proving nothing. Owning it here makes the divergent case constructible
    /// (see `a_report_counts_only_what_this_operation_destroyed`) and leaves
    /// the executor a thin caller with no count of its own to get wrong.
    pub(super) fn report(&self) -> (StatusCode, String, String) {
        if let Some(msg) = self.partial_refusal() {
            let journal = format!("delete-untracked-paths partial result — {msg}");
            return (StatusCode::CONFLICT, msg, journal);
        }
        // `self.deleted.len()`, and no other number is in scope to reach for:
        // the count is the user's only record of what is gone for good.
        let count = self.deleted.len();
        let s = if count == 1 { "" } else { "s" };
        let note = self.already_gone_note();
        // Deliberately no "undo"/"restore"/"recover" anywhere in this text (a
        // regression test greps for exactly those words) — this is the one
        // operation in the vocabulary where saying so plainly is the honest
        // thing to say, not merely the cautious one.
        let journal = format!(
            "deleted {count} untracked path{s} permanently — never stored in git, no \
             way to bring the content back{note}"
        );
        let body = format!(
            "Deleted {count} untracked path{s} permanently. That content was never \
             stored in git, so there is no way to bring it back.{note}"
        );
        (StatusCode::OK, body, journal)
    }

    /// One sentence disclosing paths that were already gone before the spawn,
    /// empty when there were none. Kept separate so both the refusal body and
    /// the success body say the same thing about them.
    pub(super) fn already_gone_note(&self) -> String {
        if self.already_gone.is_empty() {
            return String::new();
        }
        let list = self.already_gone.join(", ");
        let verb = if self.already_gone.len() == 1 {
            "was"
        } else {
            "were"
        };
        format!(
            " {list} {verb} already gone before this ran, so {} not deleted by this \
             operation — something else outside Git-Vista removed {}.",
            if self.already_gone.len() == 1 {
                "it was"
            } else {
                "they were"
            },
            if self.already_gone.len() == 1 {
                "it"
            } else {
                "them"
            }
        )
    }
}

/// `git clean -f -- <paths>` (`/api/delete-untracked-paths`, #219): delete
/// untracked paths from the working tree outright. **No journal-backed undo
/// exists for this at all** — an untracked path was never written to git's
/// object database, so there is nothing anywhere in this repository to reset
/// back to. See [`GitOperation::DeleteUntrackedPaths`](git_vista_protocol::GitOperation::DeleteUntrackedPaths)'s doc comment.
pub(super) async fn exec_delete_untracked_paths(
    repo: &Path,
    need: NetworkNeed,
    paths: &[WorktreePath],
) -> (StatusCode, String) {
    if let Err(refused) =
        symlink_containment_guard(repo, paths, "/api/delete-untracked-paths").await
    {
        return refused;
    }
    if let Err(refused) = verify_path_states(
        repo,
        need,
        paths,
        PathKind::Untracked,
        "/api/delete-untracked-paths",
    )
    .await
    {
        return refused;
    }
    let mut args: Vec<&str> = vec!["clean", "-f", "--"];
    args.extend(paths.iter().map(WorktreePath::as_str));
    // Snapshot presence as late as possible — the very last thing before the
    // spawn — so what this operation is credited with destroying is what it
    // actually destroyed, not merely what is missing afterwards. See
    // [`DeleteOutcome`] for the window this does and does not close.
    let requested: Vec<&str> = paths.iter().map(WorktreePath::as_str).collect();
    let present_before = present_paths(repo, &requested);
    let output = match run_git(repo, need, &args).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/delete-untracked-paths", &e),
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git clean failed.");
        eprintln!("git-vista: /api/delete-untracked-paths failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }

    // The TOCTOU this closes (review finding, empirically demonstrated): a
    // path can become tracked in the gap between `verify_path_states`'s read
    // and this exact `git clean` call — a concurrent `git add`, an IDE
    // auto-stage, a second git-vista tab. `git clean -f -- p1 p2 p3` is NOT
    // atomic across a multi-path pathspec: it silently SKIPS a path that's
    // since become tracked (no error, exit 0) while still deleting the
    // rest of the batch — verified directly against real git. Locking out
    // the whole race window needs a repo-wide exclusive lock this endpoint
    // doesn't hold; what's tractable and load-bearing without one is never
    // reporting success that isn't true: every requested path is re-stat'd
    // before this returns 200, and one still on disk was not deleted
    // (`observe_deletion`; #284 replaced an English-only parse of `git
    // clean`'s stdout with that check — see [`DeleteOutcome`]'s doc comment,
    // which also covers why the *before* snapshot above is needed to avoid
    // the mirror-image dishonesty of crediting ourselves with someone else's
    // deletion). The timing race itself isn't something a permanent test can
    // trigger deterministically, but the honesty property this exists for
    // doesn't depend on how a mismatch arose.
    //
    // Everything client-facing past this point is [`DeleteOutcome::report`]'s
    // — this executor deliberately keeps no count of its own to get wrong.
    let outcome = observe_deletion(repo, &requested, &present_before);
    let (status, body, summary) = outcome.report();
    if status == StatusCode::CONFLICT {
        eprintln!("git-vista: /api/delete-untracked-paths partial: {body}");
    } else {
        println!("[/api/delete-untracked-paths] {summary}");
    }
    journal_app_event(
        repo,
        ActivityKind::Other,
        None,
        Obs::Absent,
        Obs::Absent,
        summary,
    )
    .await;
    (status, body)
}
