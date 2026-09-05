//! The branch-lifecycle executors — create, checkout, merge, delete, rebase,
//! restore, and reset — plus [`IntegrationCaller`], the one enum that names
//! who asked for an integration.
//!
//! # Why this is its own module
//!
//! Everything here moves a **branch ref** (or HEAD between branches) and
//! nothing else: no commit objects are built, no index surgery, no worktree
//! content writes beyond what git itself does. The cluster is held together
//! by two shared pieces nothing outside it uses: [`run_branch_cmd`], the one
//! error posture for `git <verb> <ref>` (checkout/merge/delete all speak
//! through it), and [`IntegrationCaller`], which is how [`exec_merge`] and
//! [`exec_rebase`] serve two masters — the direct `/api/merge`/`/api/rebase`
//! endpoints and [`super::pull`]'s integration half (M2.20d #230, ADR 0044)
//! — without duplicating either executor.
//!
//! [`super::strategy_word`] stays in `planner.rs`: [`super::pull`] needs it
//! for sentences of its own, so the parent owns it rather than one sibling
//! importing prose helpers from another.

use std::path::Path;

use axum::http::StatusCode;

use git_vista_protocol::{
    plan_export, BranchName, CommitOid, MergeStrategy, RefName, WorktreeName,
};

use git_vista_core::activity::ActivityKind;

use crate::git_cmd::rev_parse;
use crate::sandbox::NetworkNeed;

use super::{
    couldnt_run, git_argv, journal_app_event, read_head_branch_blocking,
    remove_from_snapshot_blocking, run_git_argv, short, stderr_or, stderr_stdout_or, strategy_word,
    worktree_dirty, Obs, Observed,
};

/// `git branch <name> <at>` (`/api/branch`). B3 posture: git validates the
/// name, refuses a duplicate, and its stderr is forwarded verbatim on failure.
pub(super) async fn exec_create_branch(
    repo: &Path,
    need: NetworkNeed,
    name: &BranchName,
    at: &CommitOid,
) -> (StatusCode, String) {
    let output = match run_git_argv(repo, need, &plan_export::create_branch_argv(name, at)).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/branch", &e),
    };
    if output.status.success() {
        println!("[/api/branch] created branch '{name}' at {at}");
        // Journal the creation with the resolved tip (the user may have given
        // an abbreviated or symbolic start point).
        let tip = Obs::from_read(rev_parse(repo, name.as_str()).await);
        journal_app_event(
            repo,
            ActivityKind::BranchCreated,
            Some(name.as_str().to_string()),
            Obs::Absent, // a created branch has no previous tip, by definition
            tip,
            format!("created branch ‘{name}’"),
        )
        .await;
        (StatusCode::OK, format!("Created branch '{name}'."))
    } else {
        let msg = stderr_or(&output, "git branch failed.");
        eprintln!("git-vista: /api/branch failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// One `git <args…> <ref>` branch operation with the shared error posture
/// (stderr, then stdout, then a generic line) — the old `run_branch_op` core.
///
/// `target` is a [`RefName`] rather than a [`BranchName`] since M2.20d
/// (#230): four of the five callers pass a local branch (converted for free —
/// see `impl From<&BranchName> for RefName`), and the fifth is a pull's
/// integration half, whose target is the remote-tracking name `origin/main`
/// and never a local branch. Both newtypes carry the identical
/// non-empty/not-option-shaped gate, so nothing about what may reach an argv
/// changed; only the name of the thing being described did.
async fn run_branch_cmd(
    repo: &Path,
    need: NetworkNeed,
    endpoint: &str,
    argv: &[String],
    ok_msg: String,
) -> (StatusCode, String) {
    let output = match run_git_argv(repo, need, argv).await {
        Ok(o) => o,
        Err(e) => return couldnt_run(endpoint, &e),
    };
    if output.status.success() {
        println!("[{endpoint}] {ok_msg}");
        (StatusCode::OK, ok_msg)
    } else {
        let msg = stderr_stdout_or(&output, "git command failed.");
        eprintln!("git-vista: {endpoint} failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git checkout <branch>` (`/api/checkout`). Asking for the branch already
/// checked out is a no-op — git exits 0 — so journalling it would put a
/// phantom event in the Activity feed; a real switch is journaled and the
/// feed's dedup collapses git's own reflog copy into it.
pub(super) async fn exec_checkout(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    observed: &Observed,
) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        need,
        "/api/checkout",
        &plan_export::checkout_argv(branch),
        format!("checked out '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        if observed.head_branch.as_deref() == Some(branch.as_str()) {
            return (
                StatusCode::OK,
                format!("Already on ‘{branch}’ — it's the checked-out branch."),
            );
        }
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        journal_app_event(
            repo,
            ActivityKind::Checkout,
            Some(branch.as_str().to_string()),
            observed.head_tip.clone(),
            new,
            format!("checked out ‘{branch}’"),
        )
        .await;
    }
    resp
}

/// Why an integration ([`exec_merge`] / [`exec_rebase`]) is running — and
/// therefore what the activity feed must say happened (M2.20d, #230).
///
/// **The feed records the operation the user approved, not the git command
/// that implemented it.** A pull's second half runs `git merge` or
/// `git rebase`, but a user who pressed "Pull" never asked for a merge; a feed
/// showing `Fetch` + `Merge` for one approved `PullBranch` describes an
/// operation nobody submitted, and its undo hint would offer to undo half of
/// it. So the caller says who it is, and the one entry names the pull and the
/// strategy that ran.
///
/// A typed two-variant enum rather than a `bool` for ADR 0015's reason and one
/// more: the `Pull` arm has to *carry* the strategy, which a flag could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntegrationCaller {
    /// `POST /api/merge` or `POST /api/rebase` — the user asked for exactly
    /// this git operation, and the feed says exactly that. The wording every
    /// existing entry already has.
    Direct,
    /// The integration half of `POST /api/pull` (M2.20d, #230). One
    /// [`ActivityKind::Pull`] entry naming the strategy, replacing — never
    /// accompanying — the `Merge`/`Rebase` entry the `Direct` arm writes.
    Pull(MergeStrategy),
}

impl IntegrationCaller {
    /// The `(kind, summary)` this caller's journal entry carries, given the
    /// ref that was integrated and the branch it landed on.
    fn journal_as(
        self,
        target: &RefName,
        into: &str,
        direct: (ActivityKind, String),
    ) -> (ActivityKind, String) {
        match self {
            IntegrationCaller::Direct => direct,
            IntegrationCaller::Pull(strategy) => (
                ActivityKind::Pull,
                format!(
                    "pulled ‘{target}’ into ‘{into}’ ({} strategy)",
                    strategy_word(strategy)
                ),
            ),
        }
    }
}

/// `git merge --no-edit <ref>` into the checked-out branch (`/api/merge`, and
/// the merge half of `/api/pull`).
pub(super) async fn exec_merge(
    repo: &Path,
    need: NetworkNeed,
    target: &RefName,
    observed: &Observed,
    caller: IntegrationCaller,
) -> (StatusCode, String) {
    let resp = run_branch_cmd(
        repo,
        need,
        "/api/merge",
        &plan_export::merge_argv(target),
        format!("merged '{target}' into HEAD"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        // git exits 0 with "Already up to date." when the branch brings
        // nothing in — HEAD hasn't moved. That's no merge: journalling one
        // would put an event in the Activity feed that never happened.
        //
        // D5: `same_observation`, not `==`. Both sides are reads that can come
        // back `Unknown`, and `Unknown == Unknown` would report "already up to
        // date" — a statement about where HEAD is — on the strength of two
        // reads that never saw HEAD at all. `Obs` has no `PartialEq` precisely
        // so this line cannot be written the wrong way.
        if new.same_observation(&observed.head_tip) {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{target}’ has no commits the current branch doesn’t already have."),
            );
        }
        let into = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        let (kind, summary) = caller.journal_as(
            target,
            &into,
            (
                ActivityKind::Merge,
                format!("merged ‘{target}’ into ‘{into}’"),
            ),
        );
        journal_app_event(
            repo,
            kind,
            Some(into.clone()),
            observed.head_tip.clone(),
            new,
            summary,
        )
        .await;
    }
    resp
}

/// `git branch -d`/`-D <branch>` (`/api/delete-branch` /
/// `/api/force-delete-branch`). The tip was captured BEFORE the delete (git
/// removes the branch's reflog with the branch) — that journaled oid is
/// precisely what "Restore branch" replays, and after a force-delete it may be
/// the ONLY path back to the commits (until gc).
pub(super) async fn exec_delete(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    observed: &Observed,
    force: bool,
) -> (StatusCode, String) {
    let (endpoint, verb) = if force {
        ("/api/force-delete-branch", "force-deleted")
    } else {
        ("/api/delete-branch", "deleted")
    };
    let resp = run_branch_cmd(
        repo,
        need,
        endpoint,
        &plan_export::delete_branch_argv(branch, force),
        format!("{verb} branch '{branch}'"),
    )
    .await;
    if resp.0 == StatusCode::OK {
        journal_app_event(
            repo,
            ActivityKind::BranchDeleted,
            Some(branch.as_str().to_string()),
            observed.branch_tip.clone(),
            Obs::Absent, // the branch is gone: its new tip is a real absence
            format!("{verb} branch ‘{branch}’"),
        )
        .await;
        // Drop it from the snapshot now, so the feed's snapshot diff can't
        // also report this app deletion as an external one.
        remove_from_snapshot_blocking(repo, branch.as_str()).await;
    }
    resp
}

/// `git rebase <base>` of the checked-out branch (`/api/rebase`). A failed
/// rebase (almost always conflicts) is `--abort`ed so a browser-only user is
/// never left mid-rebase with no shell to fix it.
pub(super) async fn exec_rebase(
    repo: &Path,
    need: NetworkNeed,
    base: &RefName,
    observed: &Observed,
    caller: IntegrationCaller,
) -> (StatusCode, String) {
    let old = observed.head_tip.clone();
    let target = base;
    let base = base.as_str();

    let output = match run_git_argv(repo, need, &plan_export::rebase_argv(target)).await {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/rebase", &e),
    };
    if output.status.success() {
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        let branch = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        // git exits 0 without moving HEAD when the branch is already based on
        // the base — that's no rebase, and journalling one would put a phantom
        // event in the Activity feed. Say what (didn't) happen instead.
        //
        // D5: same reasoning as `exec_merge` — two unreadable tips are not
        // evidence that HEAD stayed put.
        if new.same_observation(&old) {
            return (
                StatusCode::OK,
                format!("Already up to date — ‘{branch}’ is already based on {base}."),
            );
        }
        println!("[/api/rebase] rebased HEAD onto {base}");
        let (kind, summary) = caller.journal_as(
            target,
            &branch,
            (
                ActivityKind::Rebase,
                format!("rebased ‘{branch}’ onto {base}"),
            ),
        );
        journal_app_event(repo, kind, Some(branch.clone()), old, new, summary).await;
        (StatusCode::OK, format!("Rebased onto {base}."))
    } else {
        let msg = stderr_stdout_or(&output, "git rebase failed.");
        // Best-effort: back out of the half-applied rebase so the working tree
        // isn't stuck mid-rebase. Harmless (exits non-zero, ignored) when none
        // is running.
        let _ = run_git_argv(repo, need, &plan_export::rebase_abort_argv()).await;
        eprintln!("git-vista: /api/rebase failed (aborted): {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// `git branch <name> <tip>` — re-create a deleted branch at its journaled tip
/// (`/api/undo`). The safe undo: `git branch` creates, never destroys, and
/// fails by itself if the name came back into use since the hint.
pub(super) async fn exec_restore_branch(
    repo: &Path,
    need: NetworkNeed,
    name: &BranchName,
    tip: &CommitOid,
) -> (StatusCode, String) {
    match git_argv(repo, need, &plan_export::create_branch_argv(name, tip)).await {
        Ok(()) => {
            println!(
                "[/api/undo] restored branch '{name}' at {}",
                short(tip.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::BranchCreated,
                Some(name.as_str().to_string()),
                Obs::Absent, // restored from nothing: there was no branch here
                Obs::Known(tip.as_str().to_string()),
                format!("restored branch ‘{name}’ at {}", short(tip.as_str())),
            )
            .await;
            (StatusCode::OK, format!("Restored branch ‘{name}’."))
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo restore failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// Move a branch back to `to` (`/api/undo`): `git reset --hard` when it's the
/// checked-out branch with a clean worktree, else `git branch -f` (no worktree
/// involved). `expected_tip` is compare-and-swap — a hint from a stale menu
/// can never reset away work that happened after it was shown.
pub(super) async fn exec_reset_branch(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    to: &CommitOid,
    expected_tip: &CommitOid,
    observed: &Observed,
) -> (StatusCode, String) {
    // Compare-and-swap: the hint was computed against `expected_tip`; if the
    // branch has moved since, this undo would discard newer work the user
    // never saw in the dialog — refuse instead.
    // D5: `Obs::Unknown` fails this check, the same as a mismatch would — a
    // compare-and-swap whose "compare" never read anything must not swap.
    if observed.branch_tip.known().map(String::as_str) != Some(expected_tip.as_str()) {
        return (
            StatusCode::CONFLICT,
            format!("‘{branch}’ has moved since this undo was offered — refresh and try again."),
        );
    }
    let checked_out = observed.head_branch.as_deref() == Some(branch.as_str());
    let result = if checked_out {
        // `git reset --hard` rewrites the working tree, so it runs only
        // against a fully clean one — never eat uncommitted work.
        match worktree_dirty(repo, need).await {
            Err(msg) => Err(msg),
            Ok(true) => {
                return (
                    StatusCode::CONFLICT,
                    "The working tree has uncommitted changes — commit them first \
                     so the undo can't destroy them."
                        .to_string(),
                );
            }
            Ok(false) => git_argv(repo, need, &plan_export::reset_hard_argv(to)).await,
        }
    } else {
        // Not checked out: move the ref alone, no worktree involved.
        git_argv(repo, need, &plan_export::move_branch_argv(branch, to)).await
    };
    match result {
        Ok(()) => {
            println!(
                "[/api/undo] reset branch '{branch}' to {}",
                short(to.as_str())
            );
            journal_app_event(
                repo,
                ActivityKind::Reset,
                Some(branch.as_str().to_string()),
                // Both are exact ids from the request, not reads.
                Obs::Known(expected_tip.as_str().to_string()),
                Obs::Known(to.as_str().to_string()),
                format!("undid — reset ‘{branch}’ to {}", short(to.as_str())),
            )
            .await;
            (
                StatusCode::OK,
                format!("Reset ‘{branch}’ to {}.", short(to.as_str())),
            )
        }
        Err(msg) => {
            eprintln!("git-vista: /api/undo reset failed: {msg}");
            (StatusCode::BAD_REQUEST, msg)
        }
    }
}

/// `git worktree add <worktrees_root>/<name> <branch>` (M11.04, #549, ADR 0118).
///
/// # The path is computed here, and that is the containment argument
///
/// Nothing in the request names a location. The client sends a
/// [`WorktreeName`] — a validated single path segment that cannot hold a
/// separator, a `..`, a leading dot or an absolute path — and this function
/// joins it to [`crate::state::worktrees_root`], which is a constant of the
/// installation. A single segment joined to a fixed root can only ever be a
/// direct child of that root, so "is the destination inside the fence?" is not
/// a question this code asks. It is a question the types already answered.
///
/// # It does NOT allow a new root, and that omission is load-bearing
///
/// The managed root is admitted to the allowed roots **once**, at startup,
/// exactly as the clones root is. Every child of it is therefore already
/// servable, and the new worktree is discoverable by the census and openable
/// by the drawer with no further grant. Adding `allow_repo_root(&dest)` here
/// would "work", would look like a fix if anyone ever thought it was needed,
/// and would grow the allowlist by one permanent entry per worktree created —
/// each outliving the directory it was added for. See ADR 0117 and ADR 0118:
/// this is the second place the same omission carries the fence, and it is
/// pinned by reading this function's body rather than by running it, because a
/// widening makes the app serve *more* and no behavioural test goes red.
///
/// # Every failure here is withheld-by-default, including git's own refusal
///
/// The destination path is the one thing the user did not choose and cannot
/// see, and three of this function's failure arms used to hand it to them
/// anyway: `create_dir_all`'s `io::Error`, the spawn error, and — the one
/// codex and grok both reproduced — git's stderr, relayed with only `.trim()`
/// applied, so `fatal: 'main' is already used by worktree at '/tmp/…'` shipped
/// in the HTTP body regardless of `GIT_VISTA_EXPOSE_PATHS`.
///
/// All three now go through [`crate::state::withheld_detail`]: this function
/// writes the client-safe sentence itself, git's own words are appended only
/// when the operator opted in, and the full text is logged either way. Same
/// control, same convention, and the same rule as ADR 0119 — a string that
/// arrived from git or from the OS is *detail*, not inspected first.
pub(super) async fn exec_add_worktree(
    repo: &Path,
    name: &WorktreeName,
    branch: &BranchName,
) -> (StatusCode, String) {
    let root = crate::state::worktrees_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        // `io::Error`'s own text carries no path here, but the rule is not
        // "inspect it and decide" — see this function's doc.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::state::withheld_detail(
                "/api/add-worktree",
                "Couldn't prepare the worktrees folder.",
                &format!("{}: {e}", root.display()),
            ),
        );
    }
    let dest = root.join(name.as_str());
    // Refuse rather than let git decide: `git worktree add` onto an existing
    // non-empty directory fails with a message about the path, and the path is
    // the one thing the user did not choose and cannot see. Saying it in terms
    // of the name they DID choose is the whole difference.
    if dest.exists() {
        return (
            StatusCode::CONFLICT,
            format!(
                "There is already a desk called ‘{name}’. Pick another name, or open \
                 the existing one from the worktree list."
            ),
        );
    }
    let Some(dest_str) = dest.to_str() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "The worktrees folder's path is not valid UTF-8.".to_string(),
        );
    };
    let args = ["worktree", "add", dest_str, branch.as_str()];
    // The one spawn in this server that writes outside its repository, through
    // the one helper that grants it — with the managed root, never anything
    // request-derived. See `git_cmd::sandboxed_with_grant`.
    let output = match crate::git_cmd::git_output_in_managed_root(repo, &args, &root).await {
        Ok(output) => output,
        // `e` is a spawn/sandbox error and routinely names the destination.
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::state::withheld_detail(
                    "/api/add-worktree",
                    "Couldn't run git to open the desk.",
                    &e.to_string(),
                ),
            )
        }
    };
    if !output.status.success() {
        // The confirmed leak. git's refusal for a branch already checked out
        // somewhere is `fatal: '<branch>' is already used by worktree at
        // '<abs path>'` — informative, and half of it is the server's layout.
        // The name and the branch are the client's own words, so the sentence
        // this composes stays useful with the path withheld.
        let why = String::from_utf8_lossy(&output.stderr);
        return (
            StatusCode::CONFLICT,
            crate::state::withheld_detail(
                "/api/add-worktree",
                &format!(
                    "git wouldn't open a desk called ‘{name}’ on ‘{branch}’. A branch \
                     can only be checked out at one desk at a time — the worktree list \
                     says which one has it."
                ),
                &why,
            ),
        );
    }
    (
        StatusCode::OK,
        format!("Opened a second desk called ‘{name}’ on ‘{branch}’."),
    )
}
