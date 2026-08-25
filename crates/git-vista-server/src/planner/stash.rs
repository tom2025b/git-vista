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

/// What an apply is allowed to claim, given git's exit status and a conflict
/// scan that may itself have failed (M3.24 #77, #494, ADR 0078).
///
/// # Why this is a separate function
///
/// Three of the six (status, scan) combinations below cannot be produced by
/// driving real git. `(succeeded, Blocked)` needs an apply git calls
/// successful while leaving unmerged index entries behind, and no invocation
/// found on git 2.43.0 does that — eight shapes were tried (ADR 0078
/// § "What was measured"). Both `Err` rows need a repository broken enough
/// that `git ls-files` errors while still being healthy enough to build and
/// execute a plan. The executor this replaced recorded that as an untestable
/// gap and left those arms unproven.
///
/// The other three are reachable and are driven end-to-end in
/// `planner::contract_suite`: a clean apply, a content conflict (exit 1 with
/// `UU`), and a plain refusal with a clean index (exit 1, an untracked
/// collision).
///
/// Splitting the *decision* out from the *doing* turns the gap into ordinary
/// unit tests: the combinations are enumerable even where the world that
/// produces them is not constructible. All six are pinned in the unit tests
/// below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ApplyVerdict {
    /// git succeeded and the scan confirmed nothing is unmerged. The **only**
    /// verdict that claims the apply is complete.
    Applied,
    /// git succeeded but the scan found unmerged paths. The acceptance
    /// criterion is about exactly this: the response must not read as complete
    /// while conflicted paths remain.
    AppliedWithConflicts(git_vista_protocol::conflict::Continuation),
    /// git failed, and the scan named what it left behind. A refused apply is
    /// **not** proof that nothing was applied — exit 1 with `UU` in the index
    /// is git's ordinary behaviour for a content conflict, and the markers are
    /// already in the tree (ADR 0077 § Context).
    FailedWithConflicts(git_vista_protocol::conflict::Continuation),
    /// git failed and nothing is unmerged — a plain refusal, such as an
    /// untracked file in the stash colliding with one already on disk. git's
    /// own stderr is the best thing to say, so the scan adds nothing here and
    /// must not be allowed to imply conflicts that do not exist.
    Failed,
    /// The scan could not run, so this cannot know whether conflicts remain.
    /// Reporting success here would be the green-that-means-I-did-not-look
    /// failure the conflict model was built against.
    Unverifiable(String),
}

/// The decision table. Pure, total, and unit-tested in both directions.
///
/// The asymmetry between the two `Err` rows is deliberate: on the **failure**
/// path git has already told us the operation did not succeed, and its stderr
/// is a better message than "the scan broke", so a broken scan only costs the
/// conflict detail. On the **success** path the scan is the only thing
/// standing between a green response and an unread working tree, so a broken
/// scan withdraws the claim entirely.
pub(super) fn apply_verdict(
    succeeded: bool,
    scan: Result<git_vista_protocol::conflict::Continuation, String>,
) -> ApplyVerdict {
    match (succeeded, scan) {
        (true, Ok(c)) if c.may_continue() => ApplyVerdict::Applied,
        (true, Ok(c)) => ApplyVerdict::AppliedWithConflicts(c),
        (true, Err(why)) => ApplyVerdict::Unverifiable(why),
        (false, Ok(c)) if c.may_continue() => ApplyVerdict::Failed,
        (false, Ok(c)) => ApplyVerdict::FailedWithConflicts(c),
        (false, Err(_)) => ApplyVerdict::Failed,
    }
}

/// Render the conflicted and unreadable paths a [`Continuation`] carries, as
/// the trailing block of a response body.
///
/// [`Continuation`]: git_vista_protocol::conflict::Continuation
fn conflict_detail(c: &git_vista_protocol::conflict::Continuation) -> String {
    match c {
        git_vista_protocol::conflict::Continuation::Blocked {
            unresolved,
            unreadable,
        } => {
            let mut lines = String::new();
            if !unresolved.is_empty() {
                lines.push_str(&format!("\n\nConflicted:\n  {}", unresolved.join("\n  ")));
            }
            if !unreadable.is_empty() {
                lines.push_str(&format!(
                    "\n\nCould not be read (resolve these by hand):\n  {}",
                    unreadable.join("\n  ")
                ));
            }
            lines
        }
        git_vista_protocol::conflict::Continuation::Clear => String::new(),
    }
}

/// Turn a verdict into the status and body a caller sees. Pure, so the exact
/// wording of every outcome — including the ones real git will not produce —
/// is unit-testable without a repository or a sandbox (#494, ADR 0078).
///
/// `git_said` is git's own stderr, already defaulted by [`stderr_or`]. It is
/// used only on the two failure verdicts, where git has the better message.
///
/// # The status mirrors git's exit status. The verdict rides in the body.
///
/// **2xx exactly when `git stash apply` succeeded**, whatever the conflict
/// scan then found. This is not a stylistic choice; two things downstream read
/// the status and neither can read the body.
///
/// 1. **`ApplyOutcome`, and ADR 0077 D6.** The frontend's
///    `api::stash::apply_stash_request` derives its whole outcome from
///    `resp.ok()` — any non-2xx becomes `ApplyOutcome::Refused`. `drop_gate`
///    then sets `PopVerdict::Conflicted { apply_refusal }` from it, and D6
///    turns that into one of two different sentences: `None` →
///    *"The changes were applied but left conflicts"*, `Some(_)` →
///    *"Applying the stash hit conflicts"*. A 409 on
///    [`ApplyVerdict::AppliedWithConflicts`] would make the *only* case D6's
///    `None` branch exists for unreachable, and the UI would tell a user their
///    apply was refused while their changes sat in the tree.
/// 2. **The durable operation row.** `operations::apply_terminal` maps
///    `status.is_success()` to `Succeeded` or `Failed` with nothing in
///    between. A 409 here would record `Failed` for an apply git performed —
///    "the record says only `Failed`, indistinguishable from nothing
///    happened", which is the exact single-row limit that kept `PopStash` out
///    of the enum. `Succeeded` is honest for an apply in a way it never was
///    for a pop: apply's contract is *restore the changes and keep the entry*,
///    and a conflicted apply did both. Nothing is lost either way.
///
/// So no verdict is demoted to 4xx for what the *scan* found. The
/// "not complete" claim lives in the body, which is the only channel that can
/// carry a three-way distinction, and it is asserted by
/// [`tests::exactly_one_verdict_reports_the_apply_as_complete`].
pub(super) fn render_apply(
    verdict: &ApplyVerdict,
    git_said: &str,
    entry: &StashSelector,
) -> (StatusCode, String) {
    match verdict {
        ApplyVerdict::Applied => (
            StatusCode::OK,
            format!("Applied {entry}. It is still in your stash list."),
        ),

        // 2xx: git succeeded. See this function's doc comment — a 4xx here
        // makes ADR 0077 D6's "applied but left conflicts" sentence
        // unreachable and records `Failed` for an apply that happened.
        ApplyVerdict::AppliedWithConflicts(c) => (
            StatusCode::OK,
            format!(
                "Applying {entry} left conflicts, so it is NOT complete.{}\n\n\
                 The stash entry was not removed — it is still in the list. Resolve \
                 the paths above before treating the apply as done.",
                conflict_detail(c)
            ),
        ),

        // Still a 400, exactly as before this change: git refused, and the
        // frontend's composed pop (ADR 0077) reads the status to decide
        // whether its drop may run. What is new is the path list after it.
        ApplyVerdict::FailedWithConflicts(c) => (
            StatusCode::BAD_REQUEST,
            format!(
                "{git_said}{}\n\nThe stash entry was not removed — it is still in the \
                 list. The paths above are conflicted in your working tree: the apply \
                 got far enough to write them, so this is not a no-op you can ignore.",
                conflict_detail(c)
            ),
        ),

        // A conflicting apply leaves the entry in place — that is git's own
        // behaviour and it is the right one, so the message says so rather
        // than leaving the user wondering whether their stash survived.
        ApplyVerdict::Failed => (
            StatusCode::BAD_REQUEST,
            format!(
                "{git_said}

The stash entry was not removed — it is still in the list."
            ),
        ),

        // 2xx for the same reason: git succeeded, so `ApplyOutcome::Applied` is
        // the true reading, and the frontend's `AppliedUnverified` verdict —
        // "applied, scan unavailable, drop withheld" — is only reachable from
        // it. A 4xx would produce `RefusedUnverified`, whose sentence is
        // "whether anything reached the tree is genuinely unknown", and it is
        // not unknown: git said it applied. The drop is gated client-side on
        // the client's own successful scan, so a 2xx here authorises nothing.
        ApplyVerdict::Unverifiable(why) => (
            StatusCode::OK,
            format!(
                "Applying {entry} ran and git reported success, but the conflict state \
                 could not be read afterwards — {why}. This is NOT complete: run \
                 `git status` before treating it as done, because this server could not \
                 make the check that would say so."
            ),
        ),
    }
}

/// `git stash apply <selector>` (`/api/stash/apply`, M3.24 #77) — restore a
/// stash's changes, KEEPING the entry.
///
/// Guarded by `CleanWorktree`, which is the load-bearing decision of this
/// slice: with a clean tree, the abort path is `reset --hard` + `clean -fd`
/// and that is *provably* safe, because a clean tree has nothing of the
/// user's to destroy. Apply into a dirty tree would mean an abort could
/// discard work that was never in the stash.
///
/// # The conflict state is re-read on BOTH outcomes (#494, ADR 0078)
///
/// Not only on failure. Two independent reasons, and the second is the one
/// that bites today:
///
/// 1. A stash apply git called successful while leaving conflicted paths
///    behind would otherwise be reported as complete. No such invocation was
///    found on git 2.43.0 — this is a guarantee held against a future git or a
///    conflict shape not yet tried, not a bug being fixed. It is stated as a
///    property rather than left to the exit code because a proxy that happens
///    to agree is a weaker promise than a check that asks.
/// 2. **A refused apply is not proof that nothing was applied.** Exit 1 with
///    `UU` in the index is git's ordinary response to a content conflict, and
///    the markers are already in the working tree. Naming those paths is the
///    reachable half of this, and it is what lets a client stop scanning for
///    itself on the apply-only path (ADR 0077 D3).
///
/// # Three pieces, so the untestable parts are testable anyway
///
/// The decision ([`apply_verdict`]) and the wording ([`render_apply`]) are
/// pure and unit-tested, including the combinations no `git stash apply`
/// invocation on 2.43.0 produces. What is left here is spawning git, reading
/// the conflict state, and journalling — the part that genuinely needs a
/// repository, covered by the pipeline tests in `planner::contract_suite`.
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

    // Asked on BOTH outcomes, not only on failure — see the doc comment above
    // for the two reasons.
    let verdict = apply_verdict(
        output.status.success(),
        crate::conflicts::continuation(repo).await,
    );

    match &verdict {
        ApplyVerdict::Applied => {
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
        }
        ApplyVerdict::AppliedWithConflicts(_) => {
            eprintln!("[/api/stash/apply] {entry} reported success but left conflicts");
        }
        ApplyVerdict::FailedWithConflicts(_) => {
            eprintln!("[/api/stash/apply] {entry} was refused and left conflicts");
        }
        ApplyVerdict::Failed => {
            eprintln!("git-vista: /api/stash/apply failed for {entry}");
        }
        ApplyVerdict::Unverifiable(why) => {
            eprintln!("[/api/stash/apply] {entry}: conflict state unreadable — {why}");
        }
    }

    render_apply(
        &verdict,
        &stderr_or(&output, "git stash apply failed."),
        entry,
    )
}

/// `git stash branch <name> <selector>` (`/api/stash/branch`, M3.24 #77).
///
/// # The escape hatch, and why it usually works when pop does not
///
/// Git creates the branch at the stash's **original base commit** and applies
/// there, so the changes land in the context they were written in. A stash
/// that conflicts on pop generally goes in cleanly this way, which makes this
/// the answer to the one stash situation users actually panic about: "it
/// won't come back".
///
/// # It still checks, because "usually" is not "always"
///
/// If the base commit is itself gone, or the working tree is not what the
/// precondition believed, the apply can still conflict. Git then leaves the
/// entry in place — the branch exists and is checked out, but the stash
/// survives. Same posture as pop: the conflict state is re-read afterwards and
/// the response says plainly that the work is not finished, rather than
/// returning a success whose only clue is git's stderr.
pub(super) async fn exec_branch_from_stash(
    repo: &Path,
    need: NetworkNeed,
    name: &git_vista_protocol::BranchName,
    entry: &StashSelector,
    expected_oid: &CommitOid,
) -> (StatusCode, String) {
    if let Err(refusal) =
        stash_entry_still_at(repo, need, "/api/stash/branch", entry, expected_oid).await
    {
        return refusal;
    }

    let output = match run_git(
        repo,
        need,
        &["stash", "branch", name.as_str(), entry.as_str()],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return couldnt_run("/api/stash/branch", &e),
    };

    let continuation = crate::conflicts::continuation(repo).await;

    match (output.status.success(), continuation) {
        (true, Ok(c)) if c.may_continue() => {
            println!("[/api/stash/branch] {entry} became branch {name}");
            journal_app_event(
                repo,
                ActivityKind::Other,
                Some(format!("refs/heads/{name}")),
                Obs::Absent,
                Obs::Absent,
                format!("created branch {name} from stash {entry}"),
            )
            .await;
            (
                StatusCode::OK,
                format!(
                    "Created {name} from {entry} and checked it out. The stash entry \
                     has been removed."
                ),
            )
        }

        // Same reasoning as pop: this cannot claim the work finished on a
        // check it could not make.
        (_, Err(why)) => (
            StatusCode::BAD_REQUEST,
            format!(
                "git stash branch ran, but the conflict state could not be read \
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
            eprintln!("[/api/stash/branch] {entry} left conflicts on {name}");
            (
                StatusCode::CONFLICT,
                format!(
                    "Creating {name} from {entry} left conflicts, so it is NOT \
                     complete.{detail}\n\nThe stash entry was not removed — it is \
                     still in the list."
                ),
            )
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{apply_verdict, conflict_detail, ApplyVerdict, StashSelector, StatusCode};
    use git_vista_protocol::conflict::Continuation;

    fn blocked() -> Continuation {
        Continuation::Blocked {
            unresolved: vec!["a.txt".to_string()],
            unreadable: vec![],
        }
    }

    /// All six (exit status, scan) combinations, including the four real git
    /// will not produce (#494, ADR 0078).
    ///
    /// Mutations applied to [`apply_verdict`] and observed red, each alone:
    ///
    /// | # | mutation | kind |
    /// |---|---|---|
    /// | V1 | collapse to `(true, Ok(_)) => Applied` — success stops asking | removed |
    /// | V2 | `(true, Err(_)) => Applied` — "the scan is best-effort" | weakened |
    /// | V3 | collapse to `(false, Ok(c)) => FailedWithConflicts` | weakened |
    /// | V4 | collapse to `(false, Ok(_)) => Failed` — failure stops asking | removed |
    /// | V5 | `(false, Err(why)) => Unverifiable` — a broken scan buries git's stderr | weakened |
    ///
    /// Five kills, spanning both directions on both paths. V2 and V5 also take
    /// [`a_broken_scan_withdraws_a_success_but_not_a_failure`] with them, which
    /// is the point of stating that asymmetry as its own test.
    #[test]
    fn every_combination_of_exit_status_and_scan_has_a_pinned_verdict() {
        // git succeeded and the tree is clean — the only claim of completion.
        assert_eq!(
            apply_verdict(true, Ok(Continuation::Clear)),
            ApplyVerdict::Applied
        );

        // git succeeded but conflicts remain. THE criterion: not complete.
        // Unreachable through real git on 2.43.0, which is precisely why it is
        // pinned here rather than left to a pipeline test that cannot exist.
        assert_eq!(
            apply_verdict(true, Ok(blocked())),
            ApplyVerdict::AppliedWithConflicts(blocked())
        );

        // git succeeded and the scan did not. The claim is withdrawn, not
        // softened: an unread tree may not be reported as applied.
        assert_eq!(
            apply_verdict(true, Err("ls-files exploded".to_string())),
            ApplyVerdict::Unverifiable("ls-files exploded".to_string())
        );

        // git failed and left conflicts — the reachable case. A refused apply
        // is not proof that nothing was applied.
        assert_eq!(
            apply_verdict(false, Ok(blocked())),
            ApplyVerdict::FailedWithConflicts(blocked())
        );

        // git failed and left nothing unmerged. Also reachable: an untracked
        // file in the stash colliding with one on disk exits 1 with a clean
        // index. This must NOT claim conflicts.
        assert_eq!(
            apply_verdict(false, Ok(Continuation::Clear)),
            ApplyVerdict::Failed
        );

        // git failed and the scan failed. git's stderr is still the best thing
        // to say, so a broken scan costs only the conflict detail here — it
        // does not turn a plain failure into a mystery.
        assert_eq!(
            apply_verdict(false, Err("ls-files exploded".to_string())),
            ApplyVerdict::Failed
        );
    }

    /// The two `Err` rows differ on purpose, and the difference is the point:
    /// a broken scan withdraws a *success* claim but not a *failure* message.
    /// Asserted separately so that collapsing them into one arm — the obvious
    /// "simplification" — goes red with a message saying why it is wrong.
    ///
    /// Killed two ways, both run: **V2** `(true, Err(_)) => Applied` (the
    /// success half stops being withdrawn) and **V5**
    /// `(false, Err(why)) => Unverifiable` (the failure half starts being
    /// withdrawn). One collapses the arms upward, the other downward.
    #[test]
    fn a_broken_scan_withdraws_a_success_but_not_a_failure() {
        let on_success = apply_verdict(true, Err("boom".to_string()));
        let on_failure = apply_verdict(false, Err("boom".to_string()));
        assert!(
            matches!(on_success, ApplyVerdict::Unverifiable(_)),
            "a scan that could not run must withdraw a claimed success, got {on_success:?}"
        );
        assert_eq!(
            on_failure,
            ApplyVerdict::Failed,
            "git already said this failed; a broken scan must not replace its stderr \
             with a report about the scan"
        );
    }

    fn sel() -> StashSelector {
        StashSelector::new("stash@{0}").unwrap()
    }

    fn render(v: ApplyVerdict) -> (StatusCode, String) {
        super::render_apply(&v, "CONFLICT (content): Merge conflict in a.txt", &sel())
    }

    /// **Exactly one verdict may claim the apply is complete.**
    ///
    /// This is the acceptance criterion of #494 stated as a property over the
    /// whole outcome space rather than as a check on one path, which is the
    /// form that survives someone adding a sixth verdict later.
    ///
    /// The claim is read from the **body alone**, deliberately. Three of the
    /// five verdicts are 2xx (see [`render_apply`]'s doc comment on why the
    /// status mirrors git's exit status), so a status-shaped test of this
    /// property would now pass three verdicts and prove nothing.
    ///
    /// Killed two ways, both run:
    ///
    /// - **R1, removed:** give `AppliedWithConflicts` the `Applied` body.
    ///   Two verdicts then claim completion.
    /// - **R2, weakened:** soften its wording to
    ///   `"Applied {entry}, with conflicts"`, dropping the disclaimer while
    ///   keeping everything else. R2 survived an earlier, narrower version of
    ///   this test that matched a fixed prefix; the comma defeated it. It is
    ///   why the check is now "opens by asserting the apply happened **and**
    ///   takes it back nowhere".
    #[test]
    fn exactly_one_verdict_reports_the_apply_as_complete() {
        /// A body claims completion when it opens by saying the apply happened
        /// and disclaims it nowhere.
        fn claims_complete(body: &str) -> bool {
            body.starts_with("Applied ")
                && !body.contains("NOT complete")
                && !body.contains("could not be read afterwards")
        }

        let all = [
            ApplyVerdict::Applied,
            ApplyVerdict::AppliedWithConflicts(blocked()),
            ApplyVerdict::FailedWithConflicts(blocked()),
            ApplyVerdict::Failed,
            ApplyVerdict::Unverifiable("boom".to_string()),
        ];
        let claiming: Vec<_> = all
            .iter()
            .map(|v| render(v.clone()))
            .filter(|(_, body)| claims_complete(body))
            .collect();
        assert_eq!(
            claiming.len(),
            1,
            "exactly one of five verdicts may read as a completed apply, got {claiming:?}"
        );
        assert!(
            claims_complete(&render(ApplyVerdict::Applied).1),
            "and it must be `Applied`"
        );
    }

    /// **The status mirrors git's exit status — never what the scan found.**
    ///
    /// Two things downstream read the status and cannot read the body, and
    /// both break if a scan result is allowed to demote a successful apply.
    /// `api::stash::apply_stash_request` derives `ApplyOutcome` from
    /// `resp.ok()`, so a 4xx on a succeeded apply makes ADR 0077 D6's
    /// *"The changes were applied but left conflicts"* sentence unreachable;
    /// and `operations::apply_terminal` maps `is_success()` to
    /// `Succeeded`/`Failed`, so it would record `Failed` for an apply that
    /// happened — the single-row ambiguity that kept `PopStash` out of the
    /// enum in the first place.
    ///
    /// Both of those live in crates this one cannot see, which is exactly why
    /// the coupling is pinned here instead of being left to a reviewer.
    ///
    /// Killed two ways, both run:
    ///
    /// - **S1, removed:** `AppliedWithConflicts` returns
    ///   `StatusCode::CONFLICT` — the shape this branch shipped before review
    ///   caught it, and the more "correct-looking" status.
    /// - **S2, weakened:** `Unverifiable` returns `StatusCode::BAD_REQUEST`,
    ///   the conservative-seeming choice, which produces the frontend's
    ///   `RefusedUnverified` — *"whether anything reached the tree is
    ///   genuinely unknown"* — when git plainly said it applied.
    #[test]
    fn the_status_mirrors_gits_exit_status_not_the_scan() {
        for v in [
            ApplyVerdict::Applied,
            ApplyVerdict::AppliedWithConflicts(blocked()),
            ApplyVerdict::Unverifiable("boom".to_string()),
        ] {
            let (status, body) = render(v.clone());
            assert!(
                status.is_success(),
                "git succeeded, so this must stay 2xx or ADR 0077 D6's \
                 'applied but left conflicts' sentence becomes unreachable and the \
                 operation row records Failed for an apply that happened — {v:?} gave \
                 {status}: {body}"
            );
        }
        for v in [
            ApplyVerdict::Failed,
            ApplyVerdict::FailedWithConflicts(blocked()),
        ] {
            let (status, body) = render(v.clone());
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "git failed, so this must stay 4xx — {v:?}: {body}"
            );
        }

        // The obligation that BUYS the 2xx above. A successful status on an
        // apply that is not finished is only defensible while the body says
        // so, because the body is the sole channel carrying that distinction
        // once the status has been spent on git's exit code.
        for v in [
            ApplyVerdict::AppliedWithConflicts(blocked()),
            ApplyVerdict::Unverifiable("boom".to_string()),
        ] {
            let (status, body) = render(v.clone());
            assert!(status.is_success(), "{v:?}");
            assert!(
                body.contains("NOT complete"),
                "a 2xx that is not a finished apply must disclaim in the body — \
                 it is the only place left to say it — {v:?}: {body}"
            );
        }
    }

    /// The two verdicts that carry conflicts must NAME them, and the two that
    /// do not must not imply them.
    ///
    /// Killed two ways, both run:
    ///
    /// - **R3, removed:** drop `conflict_detail(c)` from `FailedWithConflicts`;
    ///   the paths vanish from the body.
    /// - **C3, weakened:** keep the call but stop rendering the `unresolved`
    ///   list inside it, so the block survives with only its unreadable half.
    ///
    /// The negative half of this test — that `Failed` and `Unverifiable` carry
    /// no conflict block — is what the deleted `exec_pop_stash` got wrong: its
    /// `(_, Ok(c))` arm caught every non-clean outcome, so a plain refusal with
    /// a clear index produced *"left conflicts"* above an empty list. **C1**
    /// (render a heading even for `Clear`) reproduces that defect and is caught
    /// by [`a_clear_continuation_renders_no_detail_block`].
    #[test]
    fn only_the_conflict_carrying_verdicts_name_paths() {
        for v in [
            ApplyVerdict::AppliedWithConflicts(blocked()),
            ApplyVerdict::FailedWithConflicts(blocked()),
        ] {
            let (_, body) = render(v);
            assert!(
                body.contains("Conflicted:\n  a.txt"),
                "a conflict-carrying verdict must name its paths: {body}"
            );
        }
        for v in [
            ApplyVerdict::Applied,
            ApplyVerdict::Failed,
            ApplyVerdict::Unverifiable("boom".to_string()),
        ] {
            let (_, body) = render(v);
            assert!(
                !body.contains("Conflicted:"),
                "a verdict with no conflicts must not carry a conflict block: {body}"
            );
        }
    }

    /// A refusal keeps its 400 and keeps git's own stderr — the status the
    /// frontend's composed pop reads to gate its drop (ADR 0077) must not have
    /// moved, and git's message is better than any this could write.
    ///
    /// Killed two ways, both run:
    ///
    /// - **R4, removed:** drop `{git_said}` from the `FailedWithConflicts`
    ///   body, losing git's own words.
    /// - **R5, weakened:** return `StatusCode::CONFLICT` from
    ///   `FailedWithConflicts` — which reads as the *more* correct status and
    ///   is the tempting change, but silently moves what the frontend's
    ///   composed pop gates its drop on (ADR 0077).
    #[test]
    fn a_refused_apply_keeps_its_status_and_gits_own_words() {
        for v in [
            ApplyVerdict::Failed,
            ApplyVerdict::FailedWithConflicts(blocked()),
        ] {
            let (status, body) = render(v);
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "a refusal stays a 400 — the composed pop's gate reads it: {body}"
            );
            assert!(
                body.contains("CONFLICT (content): Merge conflict in a.txt"),
                "git's own stderr must survive into the body: {body}"
            );
            assert!(
                body.contains("not removed"),
                "and the user must be told their stash survived: {body}"
            );
        }
    }

    /// `Clear` must render nothing. A detail block that appended an empty
    /// "Conflicted:" heading would put a conflict-shaped section into a
    /// response about a tree with no conflicts.
    ///
    /// Killed two ways, both run: **C1, removed** — `Clear` returns
    /// `"\n\nConflicted:\n  (none)"`, reproducing the deleted
    /// `exec_pop_stash`'s defect exactly; **C4, weakened** — `Clear` returns
    /// `"\n\n"`, blank padding that looks harmless and still appends a
    /// section separator to a response that has no section.
    #[test]
    fn a_clear_continuation_renders_no_detail_block() {
        assert_eq!(conflict_detail(&Continuation::Clear), "");
    }

    /// Unreadable paths are rendered under their own heading, not folded in
    /// with the resolvable ones — the user cannot fix them by choosing a side.
    ///
    /// Killed two ways, both run: **R6, weakened** — label the unreadable
    /// block `"Conflicted:"` so the two collapse into one heading; **C2,
    /// removed** — drop the unreadable block entirely, so a path nobody can
    /// resolve is simply never mentioned.
    #[test]
    fn unreadable_paths_are_named_separately_from_conflicted_ones() {
        let detail = conflict_detail(&Continuation::Blocked {
            unresolved: vec!["a.txt".to_string()],
            unreadable: vec!["blob.bin".to_string()],
        });
        assert!(detail.contains("Conflicted:\n  a.txt"), "{detail}");
        assert!(
            detail.contains("Could not be read (resolve these by hand):\n  blob.bin"),
            "{detail}"
        );
    }

    /// The premise the `(false, Clear)` arm rests on, **executed** rather than
    /// asserted in a comment (#508).
    ///
    /// [`apply_verdict`]'s own table has claimed since #494 that this shape is
    /// reachable — *"an untracked file in the stash colliding with one on disk
    /// exits 1 with a clean index"*. It was right, and nothing ran it. So the
    /// claim sat in a comment in THIS crate while the frontend, in another
    /// crate, read the same `(false, Clear)` pair and rendered *"Your working
    /// tree was left untouched"* over a file git had just rewritten.
    ///
    /// What this pins is **git's** behaviour, not ours: git can write a tracked
    /// file and then fail, leaving nothing unmerged. Every verdict that
    /// declines to describe the working tree after a refused apply depends on
    /// that being true. If some future git stops doing it, this says so loudly
    /// instead of letting the caution look superstitious.
    #[test]
    fn a_failed_apply_can_change_a_tracked_file_and_leave_the_index_clean() {
        use git_vista_fixtures::git;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        git::init(repo);

        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git::run(repo, &["add", "tracked.txt"]);
        git::run(repo, &["commit", "-q", "-m", "seed"]);

        // The stash carries BOTH a tracked edit and an untracked file.
        std::fs::write(repo.join("tracked.txt"), "from-stash\n").unwrap();
        std::fs::write(repo.join("collision.txt"), "from-stash\n").unwrap();
        git::run(
            repo,
            &["stash", "push", "--include-untracked", "-m", "both"],
        );

        // Now put a DIFFERENT file in the untracked one's way.
        std::fs::write(repo.join("collision.txt"), "already-here\n").unwrap();

        let applied = git::try_run(repo, &["stash", "apply", "stash@{0}"]);

        assert!(
            !applied,
            "git must refuse: the stash's untracked file collides with one on disk"
        );
        assert_eq!(
            git::out(repo, &["ls-files", "-u"]).trim(),
            "",
            "and it leaves NOTHING unmerged — which is why a clear conflict scan \
             cannot be read as 'nothing happened'"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "from-stash\n",
            "yet the tracked file WAS rewritten by the failed apply. A verdict \
             calling this 'untouched' would be lying about the user's data"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("collision.txt")).unwrap(),
            "already-here\n",
            "the colliding file is the one git refused to overwrite"
        );
        assert!(
            !git::out(repo, &["stash", "list"]).trim().is_empty(),
            "and the entry survives, so a pop composed on top of this must not drop it"
        );
    }
}
