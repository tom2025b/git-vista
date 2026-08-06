//! M2.20d (#230, ADR 0044): executing [`GitOperation::PullBranch`] — a fetch
//! the caller did not choose the outcome of, plus an integration the caller
//! **did**.
//!
//! #227 (ADR 0039) fixed the typed vocabulary, including the deliberate
//! absence of any `MergeStrategy::Auto`; #229 (ADR 0043) built the fetch
//! executor. This module is the join, and every decision in it falls out of
//! two rules:
//!
//! 1. **There is one `git fetch` in this server.** The fetch half is
//!    [`super::fetch::run_fetch`] — the same spawn, the same
//!    `--progress` parsing onto the operation's own channel, the same
//!    cancellation latch, the same before/after observation of
//!    `refs/remotes/<remote>/*`, and the same #228 Network-tier hardening
//!    (`-c core.askpass=`, credential redaction). A second spawn here would
//!    be a second place for a credential to leak from, and the first one to
//!    drift.
//!
//! 2. **The integration is the caller's stated choice, executed by the code
//!    that already does it.** `strategy` reaches this module already typed and
//!    already bound by the plan's `OperationHash`; the arm it selects calls
//!    `exec_merge` or `exec_rebase` — the live executors behind `/api/merge`
//!    and `/api/rebase` — rather than re-deriving `git merge`/`git rebase`.
//!    Nothing here reads `pull.rebase`, and nothing here has a fallback to
//!    read it into. **Reusing those executors includes reusing their sandbox
//!    tier**: the integration half declares [`INTEGRATION_NEED`]
//!    (`NetworkNeed::Local`, so `Tier::Strict`), never the pull operation's own
//!    `Remote`, because `need` is what picks the tier and hooks run in every
//!    tier. See that constant's doc for the escalation that would otherwise be
//!    reachable through this endpoint alone.
//!
//! # A conflict is an outcome, not a server error
//!
//! The interesting failure of a pull is not a network failure — that is
//! fetch's taxonomy, forwarded. It is the integration that half-applies and
//! stops. This module treats that as a first-class classified result:
//!
//! * the integration is **aborted** (`git merge --abort` / `git rebase
//!   --abort`) so a browser-only user is never left mid-merge with no shell,
//!   which is the posture `exec_rebase` has taken since long before pull
//!   existed;
//! * whether the abort *worked* is then **observed** — the checked-out
//!   branch's tip is re-read and `git ls-files --unmerged` is listed — never
//!   inferred from the abort command's exit status;
//! * and the answer reaches the client as a typed
//!   [`PullFailureKind::Conflict`] (restored — choose again) or
//!   [`PullFailureKind::ConflictLeftInProgress`] (not restored — a human is
//!   needed), at `409`, never a `500`.
//!
//! Classifying a failure *as* a conflict is a documented stderr heuristic with
//! an `Other` fallback, exactly like `fetch::classify_failure` and for exactly
//! the same reason (git's exit status carries no classification, and its prose
//! is gettext-translated). The half that must not be a guess — "is the working
//! tree usable?" — is the observed half.

use axum::http::StatusCode;

use git_vista_protocol::{
    FetchFailureKind, MergeStrategy, PullError, PullFailureKind, PullSuccess, RemoteRefUpdate,
};

use super::fetch::FetchStep;
use super::*;

/// The endpoint name in log lines, matching every other executor here.
const ENDPOINT: &str = "/api/pull";

/// The network need every **non-fetch** spawn in this module declares.
///
/// # Why a pull's second half is `Local` even though a pull is `Remote`
///
/// `network_need_for_operation(PullBranch)` is `NetworkNeed::Remote`, and it
/// has to be: the fetch half opens a socket. But that answer is about the
/// operation as a whole, and this module runs **two** kinds of command under
/// it. Threading the operation-level need into both was the obvious thing to
/// write and it is wrong, because `need` is not a label — it *chooses the
/// sandbox tier*:
///
/// ```text
/// tier_for(Remote, untrusted) => Tier::Network   // no bwrap, AF_INET allowed,
///                                                // DEFAULT_GIT_PORTS reachable
/// tier_for(Local,  untrusted) => Tier::Strict    // bwrap --unshare-net, --net-deny
/// ```
///
/// and `policy_for` sets `HookMode::Run` in **both**. So `git merge` /
/// `git rebase` spawned under the pull's own `Remote` need would run any
/// `post-merge`, `post-checkout` or `post-rewrite` hook the repository carries
/// with outbound TCP on 22/443/80/9418 — a capability the byte-identical
/// command is denied when the same user asks for it through `POST /api/merge`
/// or `POST /api/rebase`, which declare `NetworkNeed::Local` and land in
/// `Tier::Strict`. Two routes to the same git command must not differ in what
/// a hostile repository can do from inside it; the pull route being the
/// *wider* one is the direction that matters, because a pull is exactly the
/// operation an untrusted clone's hooks are waiting for.
///
/// The declaration is truthful, not merely conservative: `git merge`,
/// `git rebase`, their `--abort`s and `git ls-files --unmerged` reach no
/// remote. Everything the wire touches is behind [`super::fetch::run_fetch`],
/// which keeps the operation's `Remote` need and with it #228's askpass
/// hardening and credential redaction — this constant must never reach that
/// call, and `exec_pull` is the only place both appear.
///
/// `reconcile_need` is happy with it too: it only ever complains about the
/// *other* direction (declared `Local`, argv looks remote), and none of these
/// argvs starts with a `REMOTE_SUBCOMMANDS` token.
const INTEGRATION_NEED: NetworkNeed = NetworkNeed::Local;

// ---------------------------------------------------------------------------
// Naming the ref a pull integrates
// ---------------------------------------------------------------------------

/// `<remote>/<branch>` — the remote-tracking name the integration half runs
/// against, e.g. `origin/main`.
///
/// A [`RefName`] and not a [`BranchName`], because it is not a local branch:
/// [`RefName`]'s own contract names `origin/main` as one of the three shapes
/// it exists for. The conversion cannot fail and the `expect` explains why
/// rather than hoping — both halves already passed
/// [`RefName`]'s identical `require_git_safe` gate (non-empty, not
/// option-shaped), so the join is non-empty and begins with `remote`'s first
/// byte, which is not `-`. Should that gate ever widen asymmetrically this
/// fails loudly at the one place instead of silently at every argv.
fn tracking_ref(remote: &RemoteName, branch: &BranchName) -> RefName {
    RefName::new(format!("{}/{}", remote.as_str(), branch.as_str())).expect(
        "RemoteName and BranchName already satisfy RefName's require_git_safe \
         gate, so their `/`-join does too",
    )
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Carry a failed fetch's classification across to the pull vocabulary.
///
/// A total match with no wildcard: a new [`FetchFailureKind`] cannot be added
/// without deciding what a pull calls it, which is the point of writing the
/// mapping out instead of deriving it. The two vocabularies are separate types
/// because a pull has failures a fetch cannot have (a conflict) and must not
/// borrow fetch's shape for them.
fn from_fetch_failure(kind: FetchFailureKind) -> PullFailureKind {
    match kind {
        FetchFailureKind::AuthenticationFailed => PullFailureKind::AuthenticationFailed,
        FetchFailureKind::RemoteUnreachable => PullFailureKind::RemoteUnreachable,
        FetchFailureKind::RemoteRejected => PullFailureKind::RemoteRejected,
        FetchFailureKind::CredentialHelperBlocked => PullFailureKind::CredentialHelperBlocked,
        FetchFailureKind::Cancelled => PullFailureKind::Cancelled,
        FetchFailureKind::Other => PullFailureKind::Other,
    }
}

/// Whether git's own words describe a failed integration as a *conflict*, as
/// opposed to a refusal that never touched the working tree.
///
/// # Why a heuristic here, when the rest of this module observes
///
/// `git merge` and `git rebase` both exit 1 for a conflict and 1 for "your
/// local changes would be overwritten" and 1 for "not something we can merge",
/// so the status carries nothing. The state *after* the abort carries nothing
/// either, because a successful abort erases exactly the evidence — that is
/// what an abort is. The only remaining source is stderr/stdout, which is
/// gettext-translated and version-dependent.
///
/// So this is the same trade [`super::fetch::classify_failure`] makes, with
/// the same discipline: a documented marker set, [`PullFailureKind::Other`]
/// for anything unmatched, and git's own words forwarded verbatim in every
/// case, so a mis-tag costs a less specific hint and never a wrong
/// explanation. Crucially it is **only** the advisory half: whether the
/// repository is usable afterwards is [`restored`], which observes.
///
/// Markers verified against git 2.43.0's English output:
///
/// ```text
/// CONFLICT (content): Merge conflict in a.txt
/// Automatic merge failed; fix conflicts and then commit the result.
/// error: could not apply 1a2b3c4… local work
/// error: Merging is not possible because you have unmerged files.
/// ```
fn looks_like_conflict(text: &str) -> bool {
    let s = text.to_ascii_lowercase();
    [
        "conflict",
        "automatic merge failed",
        "could not apply",
        "unmerged files",
        "fix conflicts",
    ]
    .iter()
    .any(|marker| s.contains(marker))
}

// ---------------------------------------------------------------------------
// Observing the repository after a failed integration
// ---------------------------------------------------------------------------

/// Paths git currently considers unmerged (`git ls-files --unmerged`).
///
/// `Err` is "we could not observe", never silently "there are none" — the same
/// posture `fetch::remote_tracking_refs` takes, and for the same reason: this
/// read is half the evidence for telling a user their working tree is fine.
///
/// Declares [`INTEGRATION_NEED`] rather than taking a need from its caller:
/// listing the index reaches no remote, and a parameter here would be one more
/// place the pull's `Remote` need could be threaded into a local spawn.
async fn unmerged_paths(repo: &Path) -> Result<usize, String> {
    let output = run_git(repo, INTEGRATION_NEED, &["ls-files", "--unmerged"])
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git ls-files --unmerged failed."));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().count())
}

/// Whether the repository is back at its pre-pull state: the checked-out
/// branch is at `before` **and** nothing is left unmerged.
///
/// Both halves are re-read from the repository. Neither is inferred from the
/// abort command's exit status, which is exactly the inference that would make
/// a green test meaningless — `git merge --abort` exits 0 having done nothing
/// useful in more than one real situation.
///
/// **A read that fails answers `false`.** That is not a guess in the other
/// direction: the field this feeds (`worktree_restored`) exists to let a
/// client tell a user "nothing happened, choose again", and saying that on the
/// strength of a read that never happened is precisely D5's failure mode. Not
/// being able to confirm the repository is fine is, for this field's purpose,
/// the same as it not being fine.
async fn restored(repo: &Path, before: &Obs<String>) -> bool {
    let after = Obs::from_read(rev_parse(repo, "HEAD").await);
    after.same_observation(before) && matches!(unmerged_paths(repo).await, Ok(0))
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// `git fetch --progress <remote>` then `git merge --no-edit <remote>/<branch>`
/// or `git rebase <remote>/<branch>` (`POST /api/pull`).
///
/// The shape, in order:
///
/// 1. **Fetch, through [`super::fetch::run_fetch`].** Every fetch-side outcome
///    — cancelled, failed, unobservable — ends the pull there, carrying the
///    refs that did move, because the integration must not run against a
///    remote-tracking ref whose state nobody could establish.
/// 2. **Re-read the cancel latch.** The transfer is where the time goes, so a
///    cancel very often lands as it finishes; starting a merge afterwards
///    would be the operation the operator just said to stop.
/// 3. **Observe the ref to integrate.** `refs/remotes/<remote>/<branch>`
///    either exists after the fetch or it does not, and that is a listing, not
///    an error message to parse.
/// 4. **Integrate, through the existing executor** for the caller's stated
///    strategy, under [`INTEGRATION_NEED`] — so the second half runs in the
///    same sandbox tier `POST /api/merge` and `POST /api/rebase` run it in,
///    and a pull is not a wider door into the same git command.
/// 5. **On failure, abort and observe**, per the module docs.
/// 6. **Report what moved**, with `advanced` taken from the branch tip before
///    and after — not from git's prose and not from the sub-executor's
///    sentence.
pub(super) async fn exec_pull(
    repo: &Path,
    need: NetworkNeed,
    remote: &RemoteName,
    branch: &BranchName,
    strategy: MergeStrategy,
    observed: &Observed,
) -> (StatusCode, String) {
    debug_assert_eq!(
        need,
        NetworkNeed::Remote,
        "a pull reaches a remote; if it arrives here declared Local, \
         `network_need_for_operation` is wrong and the sandbox will \
         (correctly) deny the connect"
    );

    // --- half one: the fetch ------------------------------------------------
    let updated = match super::fetch::run_fetch(repo, need, remote, ENDPOINT).await {
        FetchStep::CouldNotRun { why } => return couldnt_run(ENDPOINT, &why),
        FetchStep::Unobservable { why } => {
            return couldnt_run(
                ENDPOINT,
                &format!(
                    "the pull's fetch ran but refs/remotes/{} could not be re-read, so \
                     there is no state to integrate from: {why}",
                    remote.as_str()
                ),
            )
        }
        FetchStep::Cancelled { updated, .. } => {
            return cancelled(remote, branch, updated, "during the fetch")
        }
        FetchStep::Failed {
            kind,
            message,
            updated,
        } => {
            return refusal(
                StatusCode::BAD_REQUEST,
                from_fetch_failure(kind),
                message,
                updated,
                // The integration never started, so the checked-out branch is
                // exactly where it was. Stated as a fact about what did not
                // run, not as a read.
                true,
            );
        }
        FetchStep::Completed { updated } => updated,
    };

    // --- the gap between the halves ----------------------------------------
    // The latch is read once more here for the same reason `run_fetch` reads
    // it before spawning: the expensive half is over, a cancel very plausibly
    // landed while it finished, and the integration is a *local mutation* —
    // the one thing an operator who pressed Cancel most wants not to happen.
    //
    // **Honest note on coverage.** This line is defense in depth and is *not*
    // proven by a behavioural test: hitting it requires a cancel that lands in
    // the window after `git fetch` exits and before `exec_merge` spawns, and
    // every way to arrange that is a timing race (a cancel that lands a moment
    // earlier is caught by `git_streamed_for`, which is what
    // `a_cancelled_pull_does_not_integrate` exercises). Deleting it leaves the
    // suite green — verified by mutation, and recorded in ADR 0044 rather than
    // papered over. It stays because it is one cheap read and the failure it
    // prevents is a merge nobody asked for.
    if crate::operations::cancel_signal().is_some_and(|rx| *rx.borrow()) {
        return cancelled(remote, branch, updated, "before the integration started");
    }

    // --- what there is to integrate ----------------------------------------
    let target = tracking_ref(remote, branch);
    let tracking = format!("refs/remotes/{target}");
    match Obs::from_read(rev_parse(repo, &tracking).await) {
        Obs::Known(_) => {}
        Obs::Absent => {
            return refusal(
                StatusCode::BAD_REQUEST,
                PullFailureKind::NoSuchRemoteBranch,
                format!(
                    "The fetch from ‘{}’ succeeded, but it has no branch ‘{}’ — nothing \
                     to integrate.",
                    remote.as_str(),
                    branch.as_str()
                ),
                updated,
                true,
            )
        }
        Obs::Unknown => {
            return couldnt_run(
                ENDPOINT,
                &format!("the fetch succeeded but {tracking} could not be read"),
            )
        }
    }

    // The pre-integration tip. `observed.head_tip` is the value the plan was
    // built against and `enforce_fresh` re-verified under the repository
    // guard, and the fetch half cannot have moved it — a fetch writes only
    // `refs/remotes/*`. Reusing it rather than taking a fresh read keeps one
    // source of truth with `exec_merge`/`exec_rebase`, which compute their own
    // "already up to date" answers from this same value.
    let head_before = observed.head_tip.clone();
    if head_before.is_unknown() {
        // D5: with no pre-pull tip there is no honest `advanced`, and "did the
        // pull change anything?" is the question the response exists to
        // answer. Refuse rather than integrate and then guess.
        return couldnt_run(
            ENDPOINT,
            "the checked-out branch's tip could not be read, so a pull's effect on it \
             could not be reported",
        );
    }

    // --- half two: the integration, by the existing executors ---------------
    //
    // `INTEGRATION_NEED`, **not** `need`. See that constant's doc: `need`
    // picks the sandbox tier, and running `git merge`/`git rebase` under the
    // pull's operation-level `Remote` would hand this repository's hooks the
    // Network tier — outbound TCP the same command is denied through
    // `/api/merge` and `/api/rebase`. `need` is used exactly once in this
    // function, by the fetch above.
    let (status, git_said) = match strategy {
        MergeStrategy::Merge => {
            exec_merge(
                repo,
                INTEGRATION_NEED,
                &target,
                observed,
                IntegrationCaller::Pull(strategy),
            )
            .await
        }
        MergeStrategy::Rebase => {
            exec_rebase(
                repo,
                INTEGRATION_NEED,
                &target,
                observed,
                IntegrationCaller::Pull(strategy),
            )
            .await
        }
    };

    if status != StatusCode::OK {
        return integration_failed(
            repo,
            remote,
            branch,
            strategy,
            &head_before,
            git_said,
            updated,
        )
        .await;
    }

    // --- what the integration actually did ----------------------------------
    let head_after = Obs::from_read(rev_parse(repo, "HEAD").await);
    if head_after.is_unknown() {
        return couldnt_run(
            ENDPOINT,
            "the integration reported success but the checked-out branch's tip could \
             not be re-read, so what the pull did to it is unknown",
        );
    }
    let advanced = !head_after.same_observation(&head_before);

    let message = if advanced {
        format!(
            "Pulled ‘{}’ from ‘{}’ into the checked-out branch ({} strategy).",
            branch.as_str(),
            remote.as_str(),
            strategy_word(strategy)
        )
    } else {
        format!(
            "Already up to date — ‘{}’ on ‘{}’ has nothing the checked-out branch \
             doesn’t already have.",
            branch.as_str(),
            remote.as_str()
        )
    };
    println!("[{ENDPOINT}] {message}");
    (
        StatusCode::OK,
        serde_json::to_string(&PullSuccess {
            remote: remote.as_str().to_string(),
            branch: branch.as_str().to_string(),
            strategy,
            message,
            updated_refs: updated,
            advanced,
        })
        .expect("PullSuccess serialization cannot fail"),
    )
}

/// The failed-integration path: abort, observe, classify, report.
///
/// Split out so the abort and the observation that follows it are **one**
/// place rather than one per strategy — a pull whose merge arm forgot to abort
/// would be exactly the kind of asymmetry nobody notices until a user is stuck
/// mid-merge with no shell.
#[allow(clippy::too_many_arguments)]
async fn integration_failed(
    repo: &Path,
    remote: &RemoteName,
    branch: &BranchName,
    strategy: MergeStrategy,
    head_before: &Obs<String>,
    git_said: String,
    updated: Vec<RemoteRefUpdate>,
) -> (StatusCode, String) {
    // Back out of the half-applied integration so the working tree isn't stuck
    // mid-merge or mid-rebase. Best-effort and deliberately unconditional:
    // `exec_rebase` already ran its own abort, and a second one against a
    // repository with no rebase in progress exits non-zero and changes
    // nothing. Running it for both strategies from one place is what keeps the
    // merge arm from silently lacking the guarantee the rebase arm has had
    // since it was written.
    let abort = match strategy {
        MergeStrategy::Merge => ["merge", "--abort"],
        MergeStrategy::Rebase => ["rebase", "--abort"],
    };
    //
    // `INTEGRATION_NEED` for the same reason the integration itself uses it:
    // an abort is a local command, and it runs hooks of its own
    // (`post-checkout` on a `rebase --abort`).
    let _ = run_git(repo, INTEGRATION_NEED, &abort).await;

    // Whether that worked is observed, never assumed.
    let worktree_restored = restored(repo, head_before).await;

    // The advisory half. `looks_like_conflict` is the only heuristic in this
    // module and it never decides anything a user acts on alone: the state of
    // their working tree comes from `worktree_restored` above, and git's own
    // words are forwarded either way.
    let kind = match (looks_like_conflict(&git_said), worktree_restored) {
        (true, true) => PullFailureKind::Conflict,
        // Not restored is not restored, whatever git called it: the user's
        // working tree needs attention and no retry will help until it gets
        // some. See this variant's doc for why it is named for the cause that
        // produces it in practice while being defined by the state.
        (_, false) => PullFailureKind::ConflictLeftInProgress,
        (false, true) => PullFailureKind::Other,
    };

    let tail = if worktree_restored {
        format!(
            "The {} was aborted and the checked-out branch is back where it started; \
             the fetched commits are still here, so retrying with the other strategy \
             downloads nothing.",
            strategy_word(strategy)
        )
    } else {
        format!(
            "The {} could not be aborted cleanly — the working tree is still \
             mid-integration and needs to be resolved or aborted by hand.",
            strategy_word(strategy)
        )
    };
    refusal(
        StatusCode::CONFLICT,
        kind,
        format!(
            "Pulling ‘{}’ from ‘{}’ ({} strategy) failed: {git_said} {tail}",
            branch.as_str(),
            remote.as_str(),
            strategy_word(strategy)
        ),
        updated,
        worktree_restored,
    )
}

/// Build a refusal in this endpoint's error contract.
fn refusal(
    status: StatusCode,
    kind: PullFailureKind,
    message: String,
    updated_refs: Vec<RemoteRefUpdate>,
    worktree_restored: bool,
) -> (StatusCode, String) {
    if status != StatusCode::OK {
        eprintln!("git-vista: {ENDPOINT} refused ({kind:?}): {message}");
    }
    (
        status,
        error_body(kind, message, updated_refs, worktree_restored),
    )
}

/// The cancelled terminal response.
///
/// `409` rather than a success code, and for the same reason a cancelled fetch
/// is one: the operation did not do what was asked, and the registry derives
/// `OperationState::Failed` from a non-2xx. `when` says which of the two
/// cancellation points stopped it, because "we fetched but did not integrate"
/// and "we did not finish fetching" leave the repository in different places.
fn cancelled(
    remote: &RemoteName,
    branch: &BranchName,
    updated: Vec<RemoteRefUpdate>,
    when: &str,
) -> (StatusCode, String) {
    let moved = if updated.is_empty() {
        "no remote-tracking ref had been updated".to_string()
    } else {
        format!(
            "{} remote-tracking ref{} had already been updated",
            updated.len(),
            if updated.len() == 1 { "" } else { "s" }
        )
    };
    refusal(
        StatusCode::CONFLICT,
        PullFailureKind::Cancelled,
        format!(
            "The pull of ‘{}’ from ‘{}’ was cancelled {when}: {moved}, and the \
             checked-out branch was not changed.",
            branch.as_str(),
            remote.as_str()
        ),
        updated,
        true,
    )
}

/// The one constructor for `POST /api/pull`'s error contract, so every non-2xx
/// body this endpoint produces parses as [`PullError`] — the same guarantee
/// `/api/fetch` and `/api/amend-commit` make.
pub(crate) fn error_body(
    kind: PullFailureKind,
    message: String,
    updated_refs: Vec<RemoteRefUpdate>,
    worktree_restored: bool,
) -> String {
    serde_json::to_string(&PullError {
        kind,
        message,
        updated_refs,
        worktree_restored,
    })
    .expect("PullError serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fetch → pull failure mapping, pinned as a **literal table**.
    ///
    /// Written out rather than asserted by calling `from_fetch_failure` on
    /// both sides, which would prove only that the function agrees with
    /// itself. Every fetch kind appears exactly once, so a new one cannot be
    /// added without deciding — visibly, here — what a pull calls it.
    #[test]
    fn every_fetch_failure_carries_across_to_its_named_pull_failure() {
        let table = [
            (
                FetchFailureKind::AuthenticationFailed,
                PullFailureKind::AuthenticationFailed,
            ),
            (
                FetchFailureKind::RemoteUnreachable,
                PullFailureKind::RemoteUnreachable,
            ),
            (
                FetchFailureKind::RemoteRejected,
                PullFailureKind::RemoteRejected,
            ),
            (
                FetchFailureKind::CredentialHelperBlocked,
                PullFailureKind::CredentialHelperBlocked,
            ),
            (FetchFailureKind::Cancelled, PullFailureKind::Cancelled),
            (FetchFailureKind::Other, PullFailureKind::Other),
        ];
        for (fetch, pull) in table {
            assert_eq!(from_fetch_failure(fetch), pull, "for {fetch:?}");
        }
        // The census: the whole fetch vocabulary is covered, so a seventh
        // variant fails the exhaustive `match` in `from_fetch_failure` at
        // compile time *and* leaves this count stale.
        assert_eq!(
            table.len(),
            6,
            "FetchFailureKind grew — decide what a pull calls the new one"
        );
        // No pull-only kind may be produced by this mapping: `Conflict` and
        // friends describe an integration a fetch never has.
        for (_, pull) in table {
            assert!(
                !matches!(
                    pull,
                    PullFailureKind::Conflict
                        | PullFailureKind::ConflictLeftInProgress
                        | PullFailureKind::NoSuchRemoteBranch
                        | PullFailureKind::StrategyRequired
                ),
                "a fetch failure must never be reported as an integration \
                 failure: {pull:?}"
            );
        }
    }

    /// The conflict markers, against git 2.43.0's real output.
    #[test]
    fn a_real_conflict_is_recognised_from_gits_own_words() {
        for text in [
            "Auto-merging a.txt\nCONFLICT (content): Merge conflict in a.txt\n\
             Automatic merge failed; fix conflicts and then commit the result.",
            "CONFLICT (add/add): Merge conflict in shared.txt",
            "error: could not apply 1a2b3c4… local work",
            "error: Merging is not possible because you have unmerged files.",
        ] {
            assert!(looks_like_conflict(text), "should be a conflict: {text:?}");
        }
    }

    /// **The load-bearing negative.** Failures that are *not* conflicts must
    /// not be tagged as one, or the tag carries no information and sends a
    /// user hunting for conflict markers that do not exist.
    ///
    /// Without this leg, a `looks_like_conflict` that returned `true`
    /// unconditionally would pass the test above.
    #[test]
    fn a_failure_that_is_not_a_conflict_is_not_called_one() {
        for text in [
            "error: Your local changes to the following files would be \
             overwritten by merge:\n\ta.txt\nPlease commit your changes or \
             stash them before you merge.\nAborting",
            "error: The following untracked working tree files would be \
             overwritten by merge:\n\tshared.txt",
            "merge: origin/nope - not something we can merge",
            "fatal: It seems that there is already a rebase-merge directory",
            "",
        ] {
            assert!(
                !looks_like_conflict(text),
                "should not be read as a conflict: {text:?}"
            );
        }
    }

    /// The remote-tracking name a pull integrates, for the odd-but-legal names
    /// the newtypes actually admit — including the one shape that would break
    /// a naive join.
    #[test]
    fn the_integration_target_is_the_remote_tracking_name() {
        for (remote, branch, expected) in [
            ("origin", "main", "origin/main"),
            ("upstream", "release/2026-08", "upstream/release/2026-08"),
            ("fork2", "feature/x", "fork2/feature/x"),
        ] {
            let target = tracking_ref(
                &RemoteName::new(remote).unwrap(),
                &BranchName::new(branch).unwrap(),
            );
            assert_eq!(target.as_str(), expected);
        }
    }

    /// The join can never produce an option-shaped ref, because neither half
    /// can start with `-` — the gate that keeps a name from being read by git
    /// as a flag. Asserted on the constructor rather than assumed: this is the
    /// premise `tracking_ref`'s `expect` rests on.
    #[test]
    fn neither_half_of_the_integration_target_can_be_option_shaped() {
        assert!(RemoteName::new("-oProxyCommand=id").is_err());
        assert!(BranchName::new("--upload-pack=/bin/sh").is_err());
        assert!(RemoteName::new("").is_err());
        assert!(BranchName::new("").is_err());
    }
}
