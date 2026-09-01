//! The commit-writing executors — commit on HEAD, the branch-stub empty
//! commit, and amend (#72, M2.19; M2.19a #222, M2.19b #223, #323).
//!
//! # Why this is its own module
//!
//! These are the only three executors whose git spawn can run repository
//! hooks (`pre-commit`, `prepare-commit-msg`, `commit-msg`), so they are the
//! only callers of [`super::run_git_hooked`] and the only code that must
//! answer the question a killed hooked spawn leaves behind — "did the commit
//! land before the timeout?" ([`check_head_after_hook_timeout`]). They also
//! share one failure vocabulary: the pure classifiers
//! ([`classify_commit_failure`], [`classify_amend_failure`]), the probes that
//! feed them ([`signing_requested`], [`rejectable_hook_present`]), and the
//! refusal-body constructors that turn a kind into the typed
//! `CommitError`/`AmendCommitError` contract. Nothing else in the planner
//! consumes any of it, so the cluster moves as one piece.
//!
//! The timeout bound itself (`HOOKED_GIT_TIMEOUT`, its test-only override,
//! [`super::hooked_git_timeout`] and [`super::run_git_hooked`]) stays in
//! `planner.rs` beside the other spawn helpers: it is spawn infrastructure,
//! not commit vocabulary, and `hook_timeout_suite` reaches the override
//! through `planner`'s own namespace.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use git_vista_protocol::{
    plan_export, AmendCommitError, AmendCommitSuccess, AmendFailureKind, BranchName, CommitError,
    CommitFailureKind, CommitMessage, CommitOid,
};

use git_vista_core::activity::ActivityKind;

use crate::git_cmd::rev_parse;
use crate::sandbox::NetworkNeed;

use super::{
    couldnt_run, hooked_git_timeout, journal_app_event, read_head_branch_blocking, run_git,
    run_git_hooked, short, stderr_or, stderr_stdout_or, Obs, Observed,
};

/// What the bounded post-kill `rev-parse HEAD` read — performed by both
/// `/api/commit` and `/api/amend-commit`'s timeout arms — found, verified
/// rather than assumed. #72 (M2.19); mirrors [`run_signed_tag`](super::tag_exec::run_signed_tag)'s own
/// post-kill check on a killed `git tag -s`.
///
/// The kill races git's own ref write: a `pre-commit`/`prepare-commit-msg`/
/// `commit-msg` hang means no commit exists, but a `post-commit` hang means
/// one landed *before* the hook that hung. Three outcomes, not two — the
/// verification read can itself fail to answer in time, and that is a
/// distinct fact from "nothing changed", not a fallback to it.
enum HookTimeoutHeadCheck {
    /// HEAD reads back exactly where it was before the spawn — no commit
    /// landed.
    Unchanged,
    /// HEAD moved to this oid — a commit exists despite the kill.
    Moved(String),
    /// The verification read itself timed out, or could not be run.
    Unknown,
}

/// Read HEAD back through [`crate::git_cmd::git_output_bounded`] — **never**
/// a plain, unbounded [`run_git`] — and compare it against `old`, the tip
/// observed before the killed spawn.
///
/// Bounded, on purpose, and not merely for symmetry: this runs on a
/// repository that just proved a git child can block past
/// [`HOOKED_GIT_TIMEOUT`](super::HOOKED_GIT_TIMEOUT), still inside the coordinator guard
/// [`plan_and_execute_in`](super::plan_and_execute_in) holds across `execute()` — an unbounded recovery
/// read here would hold that guard forever, undoing the entire point of the
/// bound above. Same reasoning [`run_signed_tag`](super::tag_exec::run_signed_tag)'s inline comment spells out
/// for the signing path; the same bound is reused rather than a second
/// constant, because the property is "the whole function returns within
/// [`HOOKED_GIT_TIMEOUT`](super::HOOKED_GIT_TIMEOUT)", not "each half does".
async fn check_head_after_hook_timeout(
    repo: &Path,
    need: NetworkNeed,
    old: &Obs<String>,
) -> HookTimeoutHeadCheck {
    match crate::git_cmd::git_output_bounded(
        repo,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        need,
        hooked_git_timeout(),
    )
    .await
    {
        Ok(crate::git_cmd::BoundedOutput::Completed(out)) => {
            let new = if out.status.success() {
                Obs::Known(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                // Still no commit — unborn HEAD, the same shape a fresh
                // repository's first-commit read maps to elsewhere.
                Obs::Absent
            };
            if old.same_observation(&new) {
                HookTimeoutHeadCheck::Unchanged
            } else {
                match new {
                    Obs::Known(head) => HookTimeoutHeadCheck::Moved(head),
                    Obs::Absent | Obs::Unknown => HookTimeoutHeadCheck::Unknown,
                }
            }
        }
        Ok(crate::git_cmd::BoundedOutput::TimedOut) | Err(_) => HookTimeoutHeadCheck::Unknown,
    }
}

/// The prose for a hooked spawn's timeout arm — what actually happened
/// (`HOOKED_GIT_TIMEOUT`'s value, named so the first hook that legitimately
/// needs longer is self-diagnosing), then exactly what [`HookTimeoutHeadCheck`]
/// found, never a guess. #72 (M2.19), §6 of the design doc.
///
/// **Deliberately does not name a hook as the cause.** The design's §6b
/// wants that sentence conditioned on `rejectable_hook_present`, but that
/// probe runs an unbounded [`run_git`] (`rev-parse --git-path hooks`, then a
/// filesystem stat) — calling it from this arm, still inside the coordinator
/// guard, on a repository that just proved a git child can block, would
/// reintroduce the exact bug this timeout exists to close. The message says
/// only what was actually observed: that git did not finish and was stopped,
/// and — from the bounded HEAD check — whether anything landed.
fn hook_timeout_message(check: &HookTimeoutHeadCheck) -> String {
    // The *actual* bound this timeout ran under, which is `HOOKED_GIT_TIMEOUT`
    // in production and a test's shrunk override under `hook_timeout_suite` —
    // never the bare constant, or a test run under a sub-second override
    // would print a message describing a bound nothing actually used.
    // `{:?}` (not `.as_secs()`) so a sub-second test bound reads as "400ms"
    // rather than truncating to "0 seconds".
    let bound = hooked_git_timeout();
    let tail = match check {
        HookTimeoutHeadCheck::Unchanged => "No commit was created — HEAD is unchanged.".to_string(),
        HookTimeoutHeadCheck::Moved(head) => format!(
            "A commit was created before the stop landed (now at {}) — inspect `git log -1` \
             before trusting it; its hooks did not finish.",
            short(head)
        ),
        HookTimeoutHeadCheck::Unknown => "Couldn't confirm whether a commit was created — the \
             check itself didn't finish in time. Run `git log -1` on the server to be sure."
            .to_string(),
    };
    format!(
        "git didn't finish within {bound:?} and was stopped so this repository wouldn't stay \
         locked. {tail}"
    )
}

/// `git commit [--allow-empty] -m <message>` on HEAD (`/api/commit`).
pub(super) async fn exec_commit_on_head(
    repo: &Path,
    need: NetworkNeed,
    message: &CommitMessage,
    allow_empty: bool,
    observed: &Observed,
) -> (StatusCode, String) {
    // The pre-commit tip, captured for the journal before git moves anything.
    // `Obs::Absent` on an unborn HEAD (first commit) — journaled as a
    // creation-like event with no old state, which is exactly what it is.
    let old = observed.head_tip.clone();

    let argv = plan_export::commit_on_head_argv(message, allow_empty);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();

    let output = match run_git_hooked(repo, need, &args).await {
        Ok(crate::git_cmd::BoundedOutput::Completed(o)) => o,
        Ok(crate::git_cmd::BoundedOutput::TimedOut) => {
            eprintln!(
                "git-vista: /api/commit did not finish within {:?} and was killed",
                hooked_git_timeout()
            );
            let check = check_head_after_hook_timeout(repo, need, &old).await;
            return commit_refusal_body(
                CommitFailureKind::HookTimedOut,
                &hook_timeout_message(&check),
            );
        }
        Err(e) => return couldnt_run("/api/commit", &e),
    };
    if output.status.success() {
        println!("[/api/commit] created commit (allow_empty={allow_empty})");
        let new = Obs::from_read(rev_parse(repo, "HEAD").await);
        // The branch the commit landed on; "HEAD" when detached.
        let branch = read_head_branch_blocking(repo)
            .await
            .unwrap_or_else(|| "HEAD".into());
        let summary = message
            .as_str()
            .lines()
            .next()
            .unwrap_or(message.as_str())
            .to_string();
        journal_app_event(repo, ActivityKind::Commit, Some(branch), old, new, summary).await;
        (StatusCode::OK, "Created commit.".to_string())
    } else {
        // #72 (M2.19): typed classification ([`classify_commit_failure`]),
        // same posture [`exec_amend_commit`] already takes — `kind` for the
        // client to branch on, `message` always git's own words (prefer
        // stderr, fall back to stdout: "nothing to commit, working tree
        // clean" goes to *stdout* with a non-zero exit).
        let kind = classify_commit_failure(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            signing_requested(repo, need).await,
            rejectable_hook_present(repo, need).await,
        );
        let msg = stderr_stdout_or(&output, "git commit failed.");
        commit_refusal_body(kind, &msg)
    }
}

/// The branch-stub path of `/api/commit`: `git commit-tree` on the branch
/// tip's own tree (an empty commit by construction), then a compare-and-swap
/// `git update-ref` from `expected_tip`. HEAD, index and working tree are
/// untouched throughout.
pub(super) async fn exec_empty_commit_on_branch(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    message: &CommitMessage,
    expected_tip: &CommitOid,
) -> (StatusCode, String) {
    let refname = format!("refs/heads/{branch}");
    let tip = expected_tip.as_str();

    // Write the commit object: the parent's own tree, so nothing changes.
    let output = match run_git(
        repo,
        need,
        &[
            "commit-tree",
            &format!("{tip}^{{tree}}"),
            "-p",
            tip,
            "-m",
            message.as_str(),
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git commit-tree: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = stderr_or(&output, "git commit-tree failed.");
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }
    let new = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "git commit-tree returned no commit id.".to_string(),
        );
    }

    // Advance the ref — compare-and-swap on the expected tip, with a reflog
    // line in git's own "commit (empty): …" shape so the activity feed reads
    // it like any other commit.
    let summary = message
        .as_str()
        .lines()
        .next()
        .unwrap_or(message.as_str())
        .to_string();
    let output = match run_git(
        repo,
        need,
        &[
            "update-ref",
            "-m",
            &format!("commit (empty): {summary}"),
            refname.as_str(),
            new.as_str(),
            tip,
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git update-ref: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            format!("‘{branch}’ has moved since this was offered — refresh and try again.")
        } else {
            msg
        };
        eprintln!("git-vista: /api/commit (on ‘{branch}’) failed: {msg}");
        return (StatusCode::CONFLICT, msg);
    }

    println!("[/api/commit] created empty commit on '{branch}' ({new})");
    journal_app_event(
        repo,
        ActivityKind::Commit,
        Some(branch.as_str().to_string()),
        // Both known first-hand: `tip` is the CAS pin this operation was built
        // on, `new` is `commit-tree`'s own stdout. Neither is a read that
        // could have come back unknown.
        Obs::Known(tip.to_string()),
        Obs::Known(new),
        summary,
    )
    .await;
    (StatusCode::OK, "Created commit.".to_string())
}

/// `git commit --amend [--allow-empty] -m <message>` (`/api/amend-commit`,
/// M2.19b #223, ADR 0040): rewrite the checked-out branch's tip commit in
/// place — the first history-rewriting execution in this vocabulary, so every
/// step here is defensive by design.
///
/// The order of operations is deliberate:
///
///  1. **Detached-HEAD refusal.** Amend targets the checked-out *branch* (the
///     variant's doc comment: there is no "amend some other commit"
///     primitive), and the plan's `ResetRef` recovery needs a branch ref to
///     reset — on detached HEAD `shape` degrades recovery to `NotNeeded`,
///     which would be a lie the moment a rewrite actually happened. Refuse
///     rather than run with no recovery story.
///  2. **The compare-and-swap.** The executor-level guard, mirroring
///     `exec_empty_commit_on_branch`'s CAS and `exec_reset_branch`'s: the tip
///     observed at plan-build time must equal the operation's `expected_tip`.
///     This is the leg that catches a request whose `expected_tip` was stale
///     *from the start* — `enforce_fresh` re-verifies only preconditions that
///     held at build time, so a failed-at-build `RefAt` flows through to
///     exactly this refusal (a 400: the client's picture of the repository is
///     wrong, which is a request problem, not a race — races are the gate's
///     409s). D5: an `Absent` observation (unborn HEAD — nothing to amend)
///     refuses here too, and an `Unknown` one never reaches this function at
///     all (`enforce_fresh` refuses unreadable observations with a 500).
///  3. **The published-history flag**, read *before* the rewrite while the
///     amended-away commit is still the tip. Advisory, never blocking — the
///     user may be amending published history knowingly, and the pre-flight
///     ceremony belongs to the client (M2.19d); ADR 0040 records why.
///  4. `git commit --amend`, through the sealed chokepoint like every other
///     mutation (hooks — when the sandbox's `HookMode` runs them at all —
///     execute as children of this one spawn; there is no separate hook
///     path to bypass, which `argv_boundary`'s spawn-site census pins).
///  5. On failure, the typed classification ([`classify_amend_failure`]);
///     on success, the journal event (old tip → new tip, `ActivityKind::Amend`)
///     whose oid pair is what makes the amend visible in `/api/activity` and
///     undoable via its reset-back hint. The durable `ResetRef` recovery ref
///     is not written here: the tracked pipeline writes it for every
///     operation from the plan's own `recovery` (see
///     `plan_and_execute_tracked`), which `shape` pins to
///     `ResetRef { <branch>, expected_tip }` for this operation.
pub(super) async fn exec_amend_commit(
    repo: &Path,
    need: NetworkNeed,
    message: &CommitMessage,
    expected_tip: &CommitOid,
    allow_empty: bool,
    observed: &Observed,
) -> (StatusCode, String) {
    let Some(branch) = observed.head_branch.clone() else {
        return amend_refusal(
            AmendFailureKind::Other,
            "Amending requires a checked-out branch — HEAD is detached. \
             Check out a branch and try again.",
        );
    };
    match observed.head_tip.known().map(String::as_str) {
        Some(tip) if tip == expected_tip.as_str() => {}
        Some(_) => {
            return amend_refusal(
                AmendFailureKind::StaleTip,
                "HEAD has moved since this amend was reviewed — refresh and try again.",
            )
        }
        None => {
            return amend_refusal(
                AmendFailureKind::StaleTip,
                "There is no commit here to amend — refresh and try again.",
            )
        }
    }

    let published = amended_commit_is_published(repo, expected_tip).await;

    let argv = plan_export::amend_commit_argv(message, allow_empty);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = match run_git_hooked(repo, need, &args).await {
        Ok(crate::git_cmd::BoundedOutput::Completed(o)) => o,
        Ok(crate::git_cmd::BoundedOutput::TimedOut) => {
            eprintln!(
                "git-vista: /api/amend-commit did not finish within {:?} and was killed",
                hooked_git_timeout()
            );
            // The CAS check above already proved `observed.head_tip` equals
            // `expected_tip`, so that's the pre-spawn tip to verify against.
            let old = Obs::Known(expected_tip.as_str().to_string());
            let check = check_head_after_hook_timeout(repo, need, &old).await;
            return amend_refusal(
                AmendFailureKind::HookTimedOut,
                &hook_timeout_message(&check),
            );
        }
        Err(e) => return couldnt_run("/api/amend-commit", &e),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let kind = classify_amend_failure(
            &stderr,
            signing_requested(repo, need).await,
            rejectable_hook_present(repo, need).await,
        );
        // Amend shares `git commit`'s quirk: some refusals go to stdout.
        let msg = stderr_stdout_or(&output, "git commit --amend failed.");
        return amend_refusal(kind, &msg);
    }

    let new = Obs::from_read(rev_parse(repo, "HEAD").await);
    let summary = message
        .as_str()
        .lines()
        .next()
        .unwrap_or(message.as_str())
        .to_string();
    println!(
        "[/api/amend-commit] amended tip of '{branch}' ({} → {})",
        short(expected_tip.as_str()),
        new.known().map(|o| short(o)).unwrap_or("unknown"),
    );
    journal_app_event(
        repo,
        ActivityKind::Amend,
        Some(branch),
        // The pre-amend tip is the CAS pin this operation was built on — an
        // exact value, not a read. The new tip is a post-mutation read that
        // can honestly be `Unknown` (D5), in which case the journal notes it
        // and no undo is offered.
        Obs::Known(expected_tip.as_str().to_string()),
        new.clone(),
        summary,
    )
    .await;
    let body = AmendCommitSuccess {
        message: "Amended commit.".to_string(),
        old_tip: expected_tip.as_str().to_string(),
        new_tip: new.known().cloned(),
        amended_published_commit: published,
    };
    (
        StatusCode::OK,
        serde_json::to_string(&body).expect("AmendCommitSuccess serialization cannot fail"),
    )
}

/// The one constructor for `/api/amend-commit`'s 400 contract, wherever the
/// refusal is made: the handler's own request-shape rejections and
/// [`exec_amend_commit`]'s classified git outcomes both build the typed
/// [`AmendCommitError`] JSON through this function, into the same
/// `(StatusCode, String)` prose channel [`plan_and_execute`](super::plan_and_execute)
/// and every other operation's executor return.
///
/// #323 is why the body is JSON in a `String` at all, and why that used to
/// look like a trap: `String` implements `IntoResponse` as `text/plain`, so a
/// `(StatusCode, String)` carrying hand-serialized JSON reads as plain text at
/// the wire. `middleware::rewrap_error` answers that by sniffing the *bytes* —
/// a body that parses as a JSON object is relabeled `application/json` and
/// passed through instead of being escaped into an `ApiError.message` — which
/// is the same one mechanism [`commit_refusal_body`] and
/// [`sign_refusal_body`](super::tag_exec::sign_refusal_body) already rely on
/// with no route-local help.
///
/// This function used to be one of a pair — a `Response`-returning
/// `amend_refusal` for the handler and a `(StatusCode, String)`
/// `amend_refusal_body` for the executor, re-labeled at the route by
/// `handlers::commit::amend_route_response`. That layer existed because
/// `rewrap_error` discarded any body over its 64 KiB buffering cap, so an
/// oversized refusal (a hook printing a large rejection) survived only on the
/// route that bypassed the sniff. #336 fixed that in `rewrap_error` itself —
/// it now forwards an over-cap body instead of collecting it — so the second
/// mechanism covered nothing the first did not, and ADR 0084 records collapsing
/// it into this one.
pub(crate) fn amend_refusal(kind: AmendFailureKind, message: &str) -> (StatusCode, String) {
    eprintln!("git-vista: /api/amend-commit refused ({kind:?}): {message}");
    (
        StatusCode::BAD_REQUEST,
        serde_json::to_string(&AmendCommitError {
            kind,
            message: message.to_string(),
        })
        .expect("AmendCommitError serialization cannot fail"),
    )
}

/// Whether `tip` is reachable from any remote-tracking ref — the
/// published-history guard's question (#223). Three-state on purpose:
/// `Some(true)`/`Some(false)` are the walk's real answer, `None` is "the walk
/// failed", which the response must not collapse into `false` (a
/// shared-history warning that silently reads unknown as unpublished fails
/// open — the exact `Obs` lesson, applied to the wire).
///
/// Reuses [`git_vista_git::remote_membership`] — the shared remote walk
/// `handlers::read` already uses twice for its own on-remote flags — rather
/// than the capped [`git_vista_git::read_remote_commits`] the activity feed
/// uses. The issue named the capped helper, but the cap is wrong for *this*
/// question: `read_remote_commits` keeps only the newest `HISTORY_LIMIT`
/// remote commits, and the tip being amended is routinely deep below that in
/// remote terms — this repository's own workflow (branches preserved forever
/// after merging) makes "amend the tip of a branch merged into origin/main
/// long ago" an ordinary case, and a capped walk would answer `false` for
/// exactly the shared commit the flag exists to warn about. A false negative
/// is the dangerous direction for a defense-in-depth flag, so the exact,
/// stop-when-found membership walk is the right shared helper; nothing is
/// re-implemented (ADR 0040 records the substitution).
async fn amended_commit_is_published(repo: &Path, tip: &CommitOid) -> Option<bool> {
    let repo = repo.to_path_buf();
    let requested: std::collections::HashSet<git_vista_core::model::Oid> =
        std::iter::once(git_vista_core::model::Oid(tip.as_str().to_string())).collect();
    tokio::task::spawn_blocking(
        move || match git_vista_git::remote_membership(&repo, &requested) {
            Ok(found) => Some(!found.is_empty()),
            Err(e) => {
                eprintln!(
                    "git-vista: /api/amend-commit couldn't check remote reachability \
                     (reporting it as unknown, not as unpublished): {e}"
                );
                None
            }
        },
    )
    .await
    .unwrap_or(None)
}

/// Whether this repository's own config asks for commit signing
/// (`commit.gpgsign`, normalized through `--type=bool`). A probe for
/// [`classify_amend_failure`]'s ssh-format leg — locale-independent, unlike
/// stderr. Unset, unreadable, or git-couldn't-run all answer `false`: a
/// classification probe must never invent a claim it could not read.
async fn signing_requested(repo: &Path, need: NetworkNeed) -> bool {
    match run_git(
        repo,
        need,
        &["config", "--type=bool", "--get", "commit.gpgsign"],
    )
    .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "true",
        _ => false,
    }
}

/// Whether a hook that can reject `git commit --amend` (`pre-commit`,
/// `prepare-commit-msg`, `commit-msg`) exists — executable — in the
/// **effective** hooks directory.
///
/// "Effective" is the load-bearing word: the directory is asked of git
/// *through the same sealed chokepoint the amend itself ran through*
/// (`rev-parse --git-path hooks`), so when the sandbox policy is
/// `HookMode::Blocked` — which injects `-c core.hooksPath=<server-owned
/// empty dir>` into every spawn, shim and unsandboxed tier alike — this
/// probe sees that same empty directory and answers `false`. A repository
/// whose hooks cannot run can never have a failure classified as a hook
/// rejection, with no separate policy plumbing to drift out of sync.
async fn rejectable_hook_present(repo: &Path, need: NetworkNeed) -> bool {
    let hooks_dir = match run_git(repo, need, &["rev-parse", "--git-path", "hooks"]).await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    if hooks_dir.is_empty() {
        return false;
    }
    // `--git-path` answers relative to the repository when it answers
    // relatively at all (the spawn runs `git -C <repo>`).
    let dir = {
        let p = PathBuf::from(&hooks_dir);
        if p.is_absolute() {
            p
        } else {
            repo.join(p)
        }
    };
    ["pre-commit", "prepare-commit-msg", "commit-msg"]
        .iter()
        .any(|hook| {
            std::fs::metadata(dir.join(hook))
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                })
                .unwrap_or(false)
        })
}

/// Classify a failed `git commit --amend` into the typed
/// [`AmendFailureKind`] the wire carries (#223), so the frontend never
/// regex-sniffs stderr itself. Pure over its three inputs — the async probes
/// live in the callers — so every branch is unit-testable without a spawn.
///
/// What each leg rests on, and how it degrades (all verified empirically
/// against git 2.43 — see the paired tests):
///
///  * **Signing, gpg format:** git's canonical `gpg failed to sign the data`
///    line. Exact under the C/English locales; under a translated locale the
///    body text differs and this leg falls through — degrading toward
///    `Other`, which promises nothing (safe).
///  * **Signing, ssh format:** the leading error line names the key path and
///    varies, but `fatal: failed to write commit object` is common to every
///    failed-signer shape — meaningful as a signing signal only when the
///    repo's config actually requested signing, which is what the
///    locale-independent `signing_requested` probe supplies. Without that
///    guard, a genuine object-store write failure would masquerade as a
///    signing problem.
///  * **Hook rejection:** git prints **nothing of its own** when a hook
///    rejects a commit — a silently-failing `pre-commit` yields exit 1 with
///    empty stderr *and* stdout — so there is no positive marker to match,
///    only an inference: a rejectable hook exists (the effective-hooks-dir
///    probe), and stderr carries no `fatal:` (the prefix is hardcoded in
///    git's `die()`, never localized, so this guard is locale-proof) and not
///    the one known non-fatal refusal this argv can produce (the
///    would-become-empty advice, "You asked to amend the most recent
///    commit…"). Known residuals, accepted and safe-directional: a hook that
///    itself prints `fatal:` classifies as `Other` (right message, weaker
///    kind); under a non-English locale the would-become-empty text is
///    translated, so with a hook present that refusal classifies as
///    `HookRejected` (wrong kind, and the message shown is still git's own
///    correct advice).
///  * Everything else: [`AmendFailureKind::Other`], with git's words
///    forwarded untouched.
pub(super) fn classify_amend_failure(
    stderr: &str,
    signing_requested: bool,
    rejectable_hook_present: bool,
) -> AmendFailureKind {
    if stderr.contains("gpg failed to sign the data") {
        return AmendFailureKind::SigningFailed;
    }
    if signing_requested && stderr.contains("failed to write commit object") {
        return AmendFailureKind::SigningFailed;
    }
    if rejectable_hook_present
        && !stderr.contains("fatal:")
        && !stderr.contains("You asked to amend the most recent commit")
    {
        return AmendFailureKind::HookRejected;
    }
    AmendFailureKind::Other
}

/// Classify a failed `git commit` (on HEAD) into the typed
/// [`CommitFailureKind`] the wire carries (#72, M2.19), so the frontend
/// never regex-sniffs stderr itself. Pure over its inputs — the async
/// probes ([`signing_requested`], [`rejectable_hook_present`]) live in
/// [`exec_commit_on_head`] — so every branch is unit-testable without a
/// spawn.
///
/// **Order is load-bearing**, and exists to resolve one genuine ambiguity:
/// a silently-rejecting hook (empty stderr, non-zero exit — see
/// [`classify_amend_failure`]'s doc for the same fact on the amend path)
/// and a signing agent the sandbox stopped before it could write anything
/// (also empty stderr — see [`classify_sign_failure`](super::tag_exec::classify_sign_failure)'s doc) are
/// indistinguishable from stderr alone when both preconditions hold at
/// once. This function resolves it structurally rather than guessing:
///
///  1. **Positive GnuPG status-fd evidence first, unconditionally.** A
///     `[GNUPG:]` line is proof a signing attempt actually ran — and in
///     git's own commit sequence, hooks run *before* the object is written
///     and signed, so a positive status line can never be a hook's doing.
///     Reads the same protocol [`classify_sign_failure`](super::tag_exec::classify_sign_failure) does (verified
///     empirically against a real git 2.43 `git commit` with
///     `commit.gpgsign=true`: the identical `[GNUPG:] FAILURE sign 17` /
///     `INV_SGNR` lines land in stderr as for `git tag -s` — this function
///     duplicates that parse rather than calling `classify_sign_failure`
///     directly, since that function's own empty-stderr fallback assumes
///     no hook-shaped alternative explanation exists, which is false here;
///     see its doc comment).
///  2. **The ssh-format signing marker next**, guarded on
///     `signing_requested` exactly like [`classify_amend_failure`]'s own
///     ssh-format leg: `failed to write commit object` with no status-fd
///     protocol at all (ssh signing doesn't speak it) — verified against a
///     real bogus-signing-key ssh commit.
///  3. **The hook-rejection heuristic third** — a rejectable hook exists
///     and stderr carries no `fatal:` — so an ambiguous *empty* stderr with
///     both a hook present and signing requested is attributed to the
///     hook, not the signer: it is the earlier stage in git's own sequence,
///     and blaming a signer that structurally can't be reached yet would
///     send the user to fix a configuration that was never consulted.
///  4. **Only then** the sandboxed-signing-agent fallback: signing was
///     requested, stderr is empty, and no hook explains it either. This is
///     the shape [`classify_sign_failure`](super::tag_exec::classify_sign_failure)'s own doc names as the
///     production case under this server's sandbox (gpg stopped before it
///     could run its protocol engine at all).
///  5. Nothing staged: `git commit`'s own "nothing to commit" family of
///     messages, which git prints to **stdout** (not stderr) with a
///     non-zero exit — checked ahead of everything above, since an empty
///     working tree can never be a signing or hook problem no matter what
///     else is configured. Three shapes measured against a real git 2.43:
///     `"nothing to commit, working tree clean"` (nothing changed at all),
///     `"no changes added to commit"` (tracked changes exist, unstaged),
///     and `"nothing added to commit but untracked files present"` (only
///     untracked files) — gettext-translated under a non-English locale, so
///     a translated repository falls through to `Other`, the safe
///     direction, same residual style as every other heuristic here.
///  6. Everything else: [`CommitFailureKind::Other`], with git's words
///     forwarded untouched — never swallowed, unlike
///     [`SignTagFailureKind::Other`](git_vista_protocol::SignTagFailureKind::Other)'s canned "see the server log" (#72
///     asked explicitly that the unknown arm lose no information).
pub(super) fn classify_commit_failure(
    stdout: &str,
    stderr: &str,
    signing_requested: bool,
    rejectable_hook_present: bool,
) -> CommitFailureKind {
    if stdout.contains("nothing to commit")
        || stdout.contains("no changes added to commit")
        || stdout.contains("nothing added to commit")
    {
        return CommitFailureKind::NothingStaged;
    }

    // Positive GnuPG status-fd evidence is checked unconditionally — like
    // `classify_amend_failure`'s exact-string gpg marker, a `[GNUPG:]` line
    // can only be produced by a real signing attempt, so it is decisive
    // even if the `signing_requested` probe itself somehow disagreed.
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[GNUPG:] FAILURE ") {
            return match rest
                .split_whitespace()
                .last()
                .and_then(|s| s.parse::<i64>().ok())
                .map(|code| (code as u64) & 0xFFFF)
            {
                Some(17) => CommitFailureKind::SigningKeyMissing,
                Some(77) | Some(78) => CommitFailureKind::SigningAgentUnavailable,
                Some(257..=281) => CommitFailureKind::SigningAgentUnavailable,
                _ => CommitFailureKind::Other,
            };
        }
        if line.starts_with("[GNUPG:] INV_SGNR") {
            return CommitFailureKind::SigningKeyMissing;
        }
    }
    // The ssh-format marker, unlike the GnuPG protocol above, is generic
    // enough ("failed to write commit object") to also be a plain
    // object-write/disk failure — so it needs the `signing_requested` guard
    // `classify_amend_failure`'s own ssh-format leg already relies on.
    if signing_requested && stderr.contains("failed to write commit object") {
        return CommitFailureKind::SigningAgentUnavailable;
    }

    if rejectable_hook_present && !stderr.contains("fatal:") {
        return CommitFailureKind::HookRejected;
    }

    if signing_requested && stderr.trim().is_empty() {
        return CommitFailureKind::SigningAgentUnavailable;
    }

    CommitFailureKind::Other
}

/// Build a failed `POST /api/commit` refusal's `(StatusCode, String)` — the
/// typed [`CommitError`] JSON, serialized into the same shared prose
/// channel [`plan_and_execute`](super::plan_and_execute) and its executors return everywhere else.
/// Needs no route-local relabeling layer: `middleware::rewrap_error`'s #323
/// fix already recognises a JSON *object* body on any route and passes it
/// through with `application/json` set rather than re-wrapping it as escaped
/// text — the same posture [`amend_refusal`] and
/// [`sign_refusal_body`](super::tag_exec::sign_refusal_body) take, the first of
/// them since #336 collapsed the one exception (ADR 0084).
pub(super) fn commit_refusal_body(kind: CommitFailureKind, message: &str) -> (StatusCode, String) {
    eprintln!("git-vista: /api/commit refused ({kind:?}): {message}");
    (
        StatusCode::BAD_REQUEST,
        serde_json::to_string(&CommitError {
            kind,
            message: message.to_string(),
        })
        .expect("CommitError serialization cannot fail"),
    )
}
