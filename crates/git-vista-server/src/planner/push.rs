//! M2.20e (#231, ADR 0045): executing [`GitOperation::PushBranch`] — the one
//! operation in this server that can make another party's commits unreachable.
//!
//! #227 (ADR 0039) fixed the typed vocabulary, #228 (ADR 0036) the Network-tier
//! exec harness, #229 (ADR 0043) the streaming spawn with live progress and a
//! cancel that kills the child. This module is the last of the remote trio, and
//! everything in it follows from four rules:
//!
//! 1. **A bare force cannot be constructed here.** [`ForcePublish`] has two
//!    variants and no third, so the type already makes `git push --force`
//!    unsayable in a *plan*. [`push_argv`] carries that through to the *argv*:
//!    it is the only place in this server that builds a push command line, its
//!    `match` over `ForcePublish` has no wildcard arm, and the sole arm that
//!    emits any force flag emits `--force-with-lease=<branch>:<oid>`. A third
//!    variant would fail to compile rather than fall through to "no flag" (or,
//!    worse, to a flag someone added later). `no_push_argv_can_carry_a_bare_force`
//!    pins the property over the whole input space.
//!
//! 2. **The lease is checked twice, by two different parties, against two
//!    different things.** [`verify_lease`] compares the reviewed
//!    `expected_remote_tip` against this repository's *local*
//!    `refs/remotes/<remote>/<branch>` before anything is spawned — that is the
//!    tip the user actually reviewed, and a stale or client-forged one is
//!    refused `409` here rather than handed to git unchecked. Git then compares
//!    the same value against what the remote **advertises during the push**,
//!    which is the only check that can see the real remote. Neither subsumes the
//!    other: ours refuses early with an actionable sentence and never opens a
//!    socket; git's is the one that is authoritative about the remote. See ADR
//!    0045 D2.
//!
//! 3. **What happened is observed, not read out of git's prose.**
//!    `refs/remotes/<remote>/*` is listed before and after, and the difference
//!    *is* the answer to "did the remote move?" — git updates that ref only
//!    when the remote reported the update accepted. The same
//!    [`super::transfer`] helpers a fetch uses, for the same reason: git's
//!    summary lines are gettext-translated and version-dependent, two listings
//!    are not.
//!
//! 4. **One spawn, through #229's runner.** `git_cmd::git_streamed_for` — the
//!    same askpass hardening, the same credential redaction, the same
//!    cancellation latch, the same `\r`-split record stream feeding
//!    [`super::transfer::parse_progress`]. A second spawn here would be a second
//!    place for a credential to leak from, and the first one to drift.
//!
//! # What a cancelled push can and cannot claim
//!
//! A cancelled *fetch* can say honestly that nothing arrived, because the only
//! machine involved is this one. A cancelled push cannot say the mirror image.
//! git updates `refs/remotes/<remote>/<branch>` **after** the remote reports the
//! ref accepted, so a SIGKILL landing in that window leaves the remote changed
//! and this repository unaware. So the terminal message for a cancel states what
//! was observed (the local remote-tracking ref did or did not move) and
//! explicitly declines to conclude anything about the remote from it. That is
//! the D5 posture applied to the one place where the thing being observed is not
//! on this host.

use axum::http::StatusCode;

use git_vista_protocol::plan_export;
use git_vista_protocol::{ForcePublish, RemoteName, RemoteRefUpdate};

use super::transfer::{diff_refs, parse_progress, remote_tracking_refs};
use super::*;

/// The endpoint name in log lines, matching every other executor here.
const ENDPOINT: &str = "/api/push";

// ---------------------------------------------------------------------------
// The argv
// ---------------------------------------------------------------------------
//
// `push_argv` moved to `git_vista_protocol::plan_export` with M10 (#590), so
// the plan export prints the command this module runs rather than a second
// reconstruction of it. Everything that made it worth pulling out of the
// executor in M2.20e is unchanged and travelled with it: the exhaustive,
// wildcard-free `match` over `ForcePublish` from which no unguarded `--force`
// is reachable, and the fixed flag order its tests describe.
//
// It now sits beside `ForcePublish` itself — the type whose design is what
// makes an unguarded force unsayable in the first place — and the
// force-construction tripwire moved with it (see
// `contract_suite::only_one_place_builds_a_push_argv_and_it_can_only_build_a_leased_force`).

// ---------------------------------------------------------------------------
// The lease, checked before anything is spawned
// ---------------------------------------------------------------------------

/// The outcome of the pre-flight lease check.
enum Lease {
    /// No lease was asked for, or the reviewed tip still matches the local
    /// remote-tracking ref. Either way, spawning is allowed.
    Ok,
    /// Refuse, with this status and message.
    Refuse(StatusCode, String),
}

/// Re-verify a [`ForcePublish::WithLease`]'s reviewed tip against the live
/// `refs/remotes/<remote>/<branch>`, immediately before the push.
///
/// # Why this exists when `enforce_fresh` already checks a `RefAt`
///
/// `build_plan` turns the lease into a [`Precondition::RefAt`] on the tracking
/// ref (M2.20a), and [`super::enforce_fresh`] re-verifies it — **but only if it
/// held at build time**, by design: a precondition that already failed when the
/// plan was built is deliberately left to the executor's own guard so refusal
/// wording stays per-operation. For every other operation "the executor's own
/// guard" is git refusing. For a lease push it is *this function*, and the
/// difference matters: a client that submits a tip which never matched (a stale
/// cached value, or a forged one) would otherwise sail past the staleness gate
/// and have its unverified oid handed straight to `--force-with-lease`, where
/// git's answer is a `! [rejected] … (stale info)` on stderr after a socket was
/// opened. Refusing here is earlier, quieter, and says something the user can
/// act on.
///
/// # What it is *not*
///
/// It is not the authoritative check, and it must not be mistaken for one. The
/// remote-tracking ref is a local cache of what this repository last saw; the
/// remote may have moved since, and only git's own lease comparison against the
/// advertised ref can see that. Both run. See ADR 0045 D2.
///
/// # Failure directions
///
/// `Ok(None)` — the tracking ref does not exist — refuses. A lease names a tip
/// that must still be there, and "the ref you leased against is gone" is not a
/// reason to force-publish; it is a reason to look. `Err` — git could not be run
/// — refuses with a 500, never silently: D5's rule that an unread ref is
/// evidence about nothing.
async fn verify_lease(
    repo: &Path,
    branch: &BranchName,
    remote: &RemoteName,
    force: &ForcePublish,
) -> Lease {
    let expected = match force {
        ForcePublish::None => return Lease::Ok,
        ForcePublish::WithLease {
            expected_remote_tip,
        } => expected_remote_tip,
    };
    let tracking = format!("refs/remotes/{}/{}", remote.as_str(), branch.as_str());
    match rev_parse_ref_unpeeled(repo, &tracking).await {
        Ok(Some(live)) if live == expected.as_str() => Lease::Ok,
        Ok(Some(live)) => Lease::Refuse(
            StatusCode::CONFLICT,
            format!(
                "‘{tracking}’ is at {live}, but this force-publish was approved against \
                 {} — refusing to push. Fetch first and review the plan again; the \
                 commits now on the remote would otherwise be the ones discarded.",
                expected.as_str()
            ),
        ),
        Ok(None) => Lease::Refuse(
            StatusCode::CONFLICT,
            format!(
                "‘{tracking}’ no longer exists, so the tip this force-publish was \
                 approved against ({}) cannot be confirmed — refusing to push.",
                expected.as_str()
            ),
        ),
        Err(e) => Lease::Refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "couldn't read ‘{tracking}’, so this force-publish's lease cannot be \
                 verified: {e}"
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

/// Why a push git actually ran came back non-zero.
///
/// Server-internal rather than a wire enum: `/api/push` answers `text/plain`
/// today and the frontend renders that body to the user verbatim, so a typed
/// JSON body here would put raw JSON on an iPad screen. The classification is
/// still load-bearing — it picks the status code and the actionable sentence
/// appended to git's own words — and M2.20g (#232), which is where the push UI
/// is designed, is the slice that can afford to promote it to a wire type
/// together with the client that parses it. ADR 0045 records that as a
/// deliberate deferral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushFailure {
    /// The **lease lost**: git compared `expected_remote_tip` against what the
    /// remote advertised and they differed. The remote is untouched. This is
    /// the check `verify_lease` cannot make, and the reason both exist.
    LeaseStale,
    /// The remote refused a non-fast-forward and no force was asked for.
    NonFastForward,
    /// The remote demanded credentials this server could not supply.
    AuthenticationFailed,
    /// The remote could not be reached at all.
    RemoteUnreachable,
    /// The remote answered and refused — no such repository, access denied, a
    /// pre-receive hook declining.
    RemoteRejected,
    /// Everything else, reported with git's own words.
    Other,
}

impl PushFailure {
    /// The status this failure deserves.
    ///
    /// `409` for the two "the remote moved under you" cases, because they are
    /// the same shape as every other staleness refusal in this server and the
    /// remedy is identical (fetch, look again, resubmit). `400` for the rest,
    /// matching `/api/fetch`.
    fn status(self) -> StatusCode {
        match self {
            PushFailure::LeaseStale | PushFailure::NonFastForward => StatusCode::CONFLICT,
            PushFailure::AuthenticationFailed
            | PushFailure::RemoteUnreachable
            | PushFailure::RemoteRejected
            | PushFailure::Other => StatusCode::BAD_REQUEST,
        }
    }

    /// The sentence appended to git's own words, or nothing when git's words
    /// are already the whole story.
    fn hint(self) -> Option<&'static str> {
        match self {
            PushFailure::LeaseStale => Some(
                "The remote moved after this force-publish was approved, so the lease \
                 refused it and nothing on the remote was changed. Fetch and review \
                 again.",
            ),
            PushFailure::NonFastForward => Some(
                "The remote has commits this branch doesn't. Pull them first, or \
                 approve a force-publish, which pins the exact remote tip it may \
                 replace.",
            ),
            PushFailure::AuthenticationFailed => Some(
                "The remote refused this server's credentials. git-vista never prompts \
                 for a password — configure a credential helper or an SSH agent on the \
                 host.",
            ),
            PushFailure::RemoteUnreachable => Some("The remote could not be reached."),
            PushFailure::RemoteRejected | PushFailure::Other => None,
        }
    }
}

/// Classify a failed push from git's stderr.
///
/// The same trade [`super::fetch::classify_failure`] documents, with the same
/// discipline: git's exit status is 1 or 128 for essentially everything, so
/// stderr is the only source; it is gettext-translated and version-dependent;
/// so there is a documented marker set, an [`PushFailure::Other`] fallback, and
/// **git's own words are forwarded verbatim in every case**. The tag adds a
/// sentence, it never replaces git's.
///
/// Ordering is deliberate. `stale info` is checked first because a rejected
/// lease also prints `failed to push some refs`, and "your lease lost" is the
/// actionable half. Authentication is checked before rejection for the reason
/// fetch's classifier gives (a 403 carries both markers).
///
/// Markers verified against git 2.43.0's English output:
///
/// ```text
///  ! [rejected]        main -> main (stale info)
///  ! [rejected]        main -> main (non-fast-forward)
///  ! [rejected]        main -> main (fetch first)
/// error: failed to push some refs to './up.git'
/// remote: error: hook declined to update refs/heads/main
/// ```
fn classify_failure(stderr: &str) -> PushFailure {
    let s = stderr.to_ascii_lowercase();

    // --- the lease lost -------------------------------------------------
    // git's word for it is literally "stale info", printed on the rejection
    // line for the ref whose lease did not hold.
    if s.contains("stale info") {
        return PushFailure::LeaseStale;
    }

    // --- authentication -------------------------------------------------
    for marker in [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied (publickey",
        "invalid username or password",
    ] {
        if s.contains(marker) {
            return PushFailure::AuthenticationFailed;
        }
    }

    // --- unreachable ----------------------------------------------------
    for marker in [
        "could not resolve host",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "no route to host",
        "operation timed out",
        "connection reset by peer",
        "unable to connect",
        "failed to connect to",
        "connection closed by remote host",
    ] {
        if s.contains(marker) {
            return PushFailure::RemoteUnreachable;
        }
    }

    // --- the remote had something we don't ------------------------------
    for marker in ["non-fast-forward", "fetch first", "behind its remote"] {
        if s.contains(marker) {
            return PushFailure::NonFastForward;
        }
    }

    // --- the remote answered, and said no -------------------------------
    for marker in [
        "repository not found",
        "does not appear to be a git repository",
        "access denied",
        "the requested url returned error: 403",
        "the requested url returned error: 404",
        "remote: forbidden",
        "service not enabled",
        "hook declined",
        "pre-receive hook declined",
        "deny current branch",
        "denycurrentbranch",
    ] {
        if s.contains(marker) {
            return PushFailure::RemoteRejected;
        }
    }

    PushFailure::Other
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// `git push --progress [--set-upstream] [--force-with-lease=…] <remote>
/// <branch>` (`POST /api/push`).
///
/// The shape, in order, and why each step is where it is:
///
/// 1. **Verify the lease** ([`verify_lease`]) — before a socket exists, so a
///    stale or forged tip costs nothing and reaches no remote.
/// 2. **Observe `refs/remotes/<remote>/*`**, so "did the remote move?" has a
///    baseline. Failing to observe refuses rather than pushing blind.
/// 3. **Read the cancel latch**, so an operation cancelled while queued behind
///    the repository guard does not then publish anyway.
/// 4. **Run, streaming**, through #229's runner.
/// 5. **Observe again, and diff.** That diff is the answer reported for
///    success, failure and cancellation alike.
/// 6. **Journal what moved**, naming the mode that ran.
pub(super) async fn exec_push(
    repo: &Path,
    need: NetworkNeed,
    branch: &BranchName,
    remote: &RemoteName,
    set_upstream: bool,
    force: &ForcePublish,
) -> (StatusCode, String) {
    debug_assert_eq!(
        need,
        NetworkNeed::Remote,
        "a push reaches a remote; if it arrives here declared Local, \
         `network_need_for_operation` is wrong and the sandbox will \
         (correctly) deny the connect"
    );

    // --- 1. the lease, before anything is spawned ---------------------------
    if let Lease::Refuse(status, why) = verify_lease(repo, branch, remote, force).await {
        eprintln!("git-vista: {ENDPOINT} refused: {why}");
        return (status, why);
    }

    // --- 2. the baseline ----------------------------------------------------
    let before = match remote_tracking_refs(repo, need, remote).await {
        Ok(refs) => refs,
        Err(why) => {
            return couldnt_run(
                ENDPOINT,
                &format!("couldn't list refs/remotes/{}: {why}", remote.as_str()),
            )
        }
    };

    // --- 3. a cancel that landed while queued -------------------------------
    let cancel = crate::operations::cancel_signal();
    if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
        return (
            StatusCode::CONFLICT,
            format!(
                "The push of ‘{}’ to ‘{}’ was cancelled before it started — nothing \
                 was sent.",
                branch.as_str(),
                remote.as_str()
            ),
        );
    }

    // --- 4. the spawn -------------------------------------------------------
    let argv = plan_export::push_argv(branch, remote, set_upstream, force);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    // `Box::pin` for the reason `planner::fetch` documents: `git_streamed_for`'s
    // future is large enough that inlining it into every caller's frame
    // overflows a debug-build test thread's stack.
    let run = Box::pin(crate::git_cmd::git_streamed_for(
        repo,
        &argv_ref,
        need,
        cancel,
        |record| {
            if let Some(progress) = parse_progress(record) {
                crate::operations::progress(progress);
            }
        },
    ))
    .await;
    let run = match run {
        Ok(run) => run,
        Err(e) => return couldnt_run(ENDPOINT, &e),
    };

    // --- 5. what the repository says happened -------------------------------
    let after = match remote_tracking_refs(repo, need, remote).await {
        Ok(refs) => refs,
        Err(why) => {
            journal_unobserved(repo, branch, remote, &why).await;
            return couldnt_run(
                ENDPOINT,
                &format!(
                    "the push ran but refs/remotes/{} could not be re-read, so what it \
                     did to the remote is unknown: {why}",
                    remote.as_str()
                ),
            );
        }
    };
    let updated = diff_refs(&before, &after);

    if run.cancelled {
        journal_updates(repo, branch, remote, &updated, force, set_upstream, true).await;
        return cancelled_response(branch, remote, &updated);
    }

    let stderr = String::from_utf8_lossy(&run.output.stderr).into_owned();
    if !run.output.status.success() {
        let kind = classify_failure(&stderr);
        let git_said = stderr_stdout_or(&run.output, "git push failed.");
        let message = match kind.hint() {
            Some(hint) => format!("{git_said}\n\n{hint}"),
            None => git_said,
        };
        eprintln!("git-vista: {ENDPOINT} failed ({kind:?}): {message}");
        // A failed push can still have moved something (one ref of several
        // accepted, or the lease losing after an earlier ref landed), and the
        // journal records what actually landed either way.
        journal_updates(repo, branch, remote, &updated, force, set_upstream, false).await;
        return (kind.status(), message);
    }

    // --- 6. success ---------------------------------------------------------
    journal_updates(repo, branch, remote, &updated, force, set_upstream, false).await;
    let message = success_message(repo, branch, remote, &updated, force, set_upstream).await;
    println!("[{ENDPOINT}] {message}");
    (StatusCode::OK, message)
}

/// The terminal response for a cancelled push.
///
/// `409` for the reason a cancelled fetch is one: the operation did not do what
/// was asked, and the registry derives `OperationState::Failed` from a non-2xx.
///
/// The wording is the careful part. git updates `refs/remotes/<remote>/<branch>`
/// only *after* the remote reports the ref accepted, so an empty diff means
/// "this repository never saw an acceptance" — which is **not** the same as "the
/// remote is unchanged". A push killed in the window between the remote
/// committing the update and git recording it locally leaves exactly that gap.
/// Saying "nothing was pushed" would be a claim about a machine this server
/// stopped talking to mid-sentence.
fn cancelled_response(
    branch: &BranchName,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
) -> (StatusCode, String) {
    let message = if updated.is_empty() {
        format!(
            "The push of ‘{}’ to ‘{}’ was cancelled. No remote-tracking ref was \
             updated here, which means this repository never saw the remote accept \
             it — but a push cancelled mid-flight can still have landed. Fetch to \
             see where the remote actually is.",
            branch.as_str(),
            remote.as_str()
        )
    } else {
        format!(
            "The push of ‘{}’ to ‘{}’ was cancelled after {} remote-tracking ref{} \
             had already been updated, so that much was accepted by the remote.",
            branch.as_str(),
            remote.as_str(),
            updated.len(),
            if updated.len() == 1 { "" } else { "s" }
        )
    };
    eprintln!("git-vista: {ENDPOINT} cancelled: {message}");
    (StatusCode::CONFLICT, message)
}

/// The success sentence, built from what was observed rather than from what was
/// requested.
///
/// `--set-upstream` is the part that has to be *checked*: the flag says what was
/// asked for, and this reads `<branch>@{upstream}` back afterwards, so the
/// sentence claims an upstream only when git actually recorded one. A message
/// that echoed the request would say "upstream set" for a git that quietly did
/// not.
async fn success_message(
    repo: &Path,
    branch: &BranchName,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
    force: &ForcePublish,
    set_upstream: bool,
) -> String {
    let head = if updated.is_empty() {
        format!(
            "Already up to date — ‘{}’ on ‘{}’ already has everything ‘{}’ does.",
            branch.as_str(),
            remote.as_str(),
            branch.as_str()
        )
    } else {
        match force {
            ForcePublish::None => format!("Pushed ‘{}’ to ‘{}’.", branch.as_str(), remote.as_str()),
            ForcePublish::WithLease { .. } => format!(
                "Force-published ‘{}’ to ‘{}’ under a lease{}.",
                branch.as_str(),
                remote.as_str(),
                match replaced_tip(updated, branch, remote) {
                    Some(old) => format!(", replacing {}", short(&old)),
                    None => String::new(),
                }
            ),
        }
    };
    if !set_upstream {
        return head;
    }
    match upstream_of(repo, branch).await {
        Obs::Known(upstream) => format!("{head} Upstream set to ‘{upstream}’."),
        // git said there is no upstream, or could not be asked. Either way the
        // honest sentence is the same: do not claim one was recorded.
        Obs::Absent | Obs::Unknown => format!(
            "{head} The push asked to set the upstream, but ‘{}’ still has none \
             recorded.",
            branch.as_str()
        ),
    }
}

/// `<branch>@{upstream}`, as git resolves it — the observation behind the
/// "upstream set" half of a success message.
///
/// D5's three states: `Known` is a recorded upstream, `Absent` is git running
/// and reporting none, `Unknown` is git not running. The caller collapses the
/// last two, and says so where it does.
/// `pub(super)` for one reason worth stating: its test lives in
/// [`super::push_suite`], not here. It needs a real repository, building one
/// needs a raw process spawn, and `argv_boundary`'s tripwire (rightly) refuses
/// to allowlist a *production* module as a spawn site — an allowlist entry
/// outlives the fixture that earned it. The tripwire scans source text, so even
/// naming the constructor in this comment would trip it, which is the sort of
/// bluntness a tripwire is allowed to have.
pub(super) async fn upstream_of(repo: &Path, branch: &BranchName) -> Obs<String> {
    // Local (D3): resolving an upstream reads config and refs, never a remote.
    //
    // No `--quiet`: verified against git 2.43.0, `rev-parse --abbrev-ref
    // --quiet <b>@{upstream}` still exits 128 for an unresolvable upstream but
    // **echoes the spec back on stdout** while doing it, so a reader that
    // trusted stdout would report the upstream of `main` as the literal string
    // `main@{upstream}`. Without the flag, a failure leaves stdout empty and
    // says why on stderr.
    let spec = format!("{}@{{upstream}}", branch.as_str());
    let output = match crate::git_cmd::git_output(repo, &["rev-parse", "--abbrev-ref", &spec]).await
    {
        Ok(output) => output,
        Err(_) => return Obs::Unknown,
    };
    if !output.status.success() {
        return Obs::Absent;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        Obs::Absent
    } else {
        Obs::Known(name)
    }
}

/// The oid this push replaced on `refs/remotes/<remote>/<branch>`, taken from
/// the observed diff — never from the reviewed lease value.
///
/// The distinction matters for the journal: the lease says what the user
/// *approved* replacing, this says what was *actually* replaced. They agree in
/// the ordinary case, and when they do not, the observed one is the one worth
/// recording.
fn replaced_tip(
    updated: &[RemoteRefUpdate],
    branch: &BranchName,
    remote: &RemoteName,
) -> Option<String> {
    let want = format!("refs/remotes/{}/{}", remote.as_str(), branch.as_str());
    updated
        .iter()
        .find(|u| u.ref_name == want)
        .and_then(|u| u.old_oid.clone())
}

/// The first 8 characters of an oid, for a sentence a human reads.
fn short(oid: &str) -> String {
    oid.chars().take(8).collect()
}

/// Journal one entry per remote-tracking ref this push actually moved.
///
/// **Per ref, and only for refs that moved** — the same posture
/// `planner::fetch` takes, and for the same reason: the feed is keyed on refs,
/// and an entry written on entry to the executor would make every push look like
/// a change, including the ones that pushed nothing.
///
/// The summary names **which mode ran**, because "pushed", "pushed with
/// `--set-upstream`" and "force-published over a tip that is now unreachable"
/// are three different events and a feed that spells them the same way cannot be
/// audited after the fact — which is precisely what a reader wants from the feed
/// on the day a force-publish is the thing they are trying to understand.
#[allow(clippy::too_many_arguments)]
async fn journal_updates(
    repo: &Path,
    branch: &BranchName,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
    force: &ForcePublish,
    set_upstream: bool,
    cancelled: bool,
) {
    for update in updated {
        let short_name = update
            .ref_name
            .strip_prefix("refs/remotes/")
            .unwrap_or(&update.ref_name);
        let verb = match (force, &update.old_oid) {
            (ForcePublish::None, _) => "pushed".to_string(),
            (ForcePublish::WithLease { .. }, Some(old)) => {
                format!("force-published (lease) over {}", short(old))
            }
            // A lease push that *created* the remote ref: nothing was replaced,
            // and saying "over <nothing>" would invent a casualty.
            (ForcePublish::WithLease { .. }, None) => "force-published (lease)".to_string(),
        };
        let upstream = if set_upstream {
            " with --set-upstream"
        } else {
            ""
        };
        let tail = if cancelled {
            " (the push was then cancelled)"
        } else {
            ""
        };
        journal_app_event(
            repo,
            ActivityKind::Push,
            Some(update.ref_name.clone()),
            // Observed, taken from this module's own before/after listings, so
            // "absent" genuinely means the ref did not exist.
            match &update.old_oid {
                Some(oid) => Obs::Known(oid.clone()),
                None => Obs::Absent,
            },
            match &update.new_oid {
                Some(oid) => Obs::Known(oid.clone()),
                None => Obs::Absent,
            },
            format!(
                "{verb} ‘{}’ to {}{upstream} — ‘{short_name}’{tail}",
                branch.as_str(),
                remote.as_str()
            ),
        )
        .await;
    }
}

/// The one push outcome with no ref diff to describe: `git push` ran and the
/// re-read of `refs/remotes/<remote>/*` failed.
///
/// One entry, no `ref_name`, `Obs::Unknown` on both tips — `planner::fetch`'s
/// `journal_unobserved` for the same reason it exists there: returning silently
/// would leave the feed claiming nothing happened while a remote may have been
/// changed. For a push the stakes are higher than for a fetch, because the thing
/// that may have changed is not on this machine and no later local read will
/// reveal it.
async fn journal_unobserved(repo: &Path, branch: &BranchName, remote: &RemoteName, why: &str) {
    journal_app_event(
        repo,
        ActivityKind::Push,
        None,
        Obs::Unknown,
        Obs::Unknown,
        format!(
            "pushed ‘{}’ to {}, but refs/remotes/{} could not be re-read afterwards, \
             so what reached the remote is unknown: {why}",
            branch.as_str(),
            remote.as_str(),
            remote.as_str()
        ),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::CommitOid;

    fn branch(name: &str) -> BranchName {
        BranchName::new(name).unwrap()
    }

    fn remote(name: &str) -> RemoteName {
        RemoteName::new(name).unwrap()
    }

    fn lease(oid: &str) -> ForcePublish {
        ForcePublish::WithLease {
            expected_remote_tip: CommitOid::new(oid).unwrap(),
        }
    }

    /// Every [`ForcePublish`] variant, named — an exhaustive `match` with no
    /// wildcard, so a third variant fails to compile here and the census below
    /// cannot silently stop covering the whole space.
    fn variant_name(force: &ForcePublish) -> &'static str {
        match force {
            ForcePublish::None => "none",
            ForcePublish::WithLease { .. } => "with_lease",
        }
    }

    /// The whole `ForcePublish` space, with the census that keeps it whole.
    fn every_force_mode() -> Vec<ForcePublish> {
        let all = vec![ForcePublish::None, lease(&"4".repeat(40))];
        let mut names: Vec<&str> = all.iter().map(variant_name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            2,
            "ForcePublish grew a variant — add a sample here, or every argv \
             assertion below silently stops covering it"
        );
        all
    }

    /// **The invariant this whole slice exists to keep**: no argv this server
    /// can build carries an unguarded force.
    ///
    /// Asserted over the entire input space — both force modes, both upstream
    /// settings, several branch and remote names — because "the code doesn't
    /// currently write `--force`" is a fact about today's source and this is
    /// meant to be a fact about the function.
    ///
    /// Note what is *not* asserted: that the string `--force` never appears. It
    /// does, as the prefix of `--force-with-lease=`. The property is stronger and
    /// more precise: **any** element beginning `--force` must begin
    /// `--force-with-lease=`, and the short `-f` must never appear at all.
    #[test]
    fn no_push_argv_can_carry_a_bare_force() {
        for force in every_force_mode() {
            for set_upstream in [true, false] {
                for (b, r) in [("main", "origin"), ("release/2026-08", "upstream")] {
                    let argv = plan_export::push_argv(&branch(b), &remote(r), set_upstream, &force);
                    for arg in &argv {
                        assert_ne!(arg, "--force", "bare force in {argv:?}");
                        assert_ne!(arg, "-f", "bare force in {argv:?}");
                        assert_ne!(arg, "--force-if-includes", "{argv:?}");
                        assert!(
                            !arg.starts_with("--force="),
                            "an `--force=<x>` form is still a bare force: {argv:?}"
                        );
                        if arg.starts_with("--force") {
                            assert!(
                                arg.starts_with("--force-with-lease="),
                                "the only force flag this server may build is the \
                                 leased one: {argv:?}"
                            );
                        }
                    }
                    // The argv is a *push*, and the last two elements are the
                    // remote and the branch — so nothing above passed by
                    // accident on an argv that was not a push at all.
                    assert_eq!(argv[0], "push", "{argv:?}");
                    assert_eq!(argv[argv.len() - 2], r, "{argv:?}");
                    assert_eq!(argv[argv.len() - 1], b, "{argv:?}");
                }
            }
        }
    }

    /// The paired positive and negative for the lease flag: a lease push builds
    /// exactly one `--force-with-lease=<branch>:<tip>`, and a fast-forward push
    /// builds **no** force flag at all.
    ///
    /// Without the negative leg, an implementation that emitted the lease flag
    /// unconditionally (with some placeholder oid) would satisfy
    /// `no_push_argv_can_carry_a_bare_force` — and would force-publish every
    /// ordinary push.
    #[test]
    fn only_a_lease_push_builds_a_force_flag_and_it_names_the_reviewed_tip() {
        let tip = "4".repeat(40);
        let leased = plan_export::push_argv(&branch("main"), &remote("origin"), false, &lease(&tip));
        let leased_flags: Vec<&String> =
            leased.iter().filter(|a| a.starts_with("--force")).collect();
        assert_eq!(
            leased_flags,
            vec![&format!("--force-with-lease=main:{tip}")],
            "the lease must name the remote-side ref and the reviewed tip: {leased:?}"
        );

        let plain = plan_export::push_argv(
            &branch("main"),
            &remote("origin"),
            false,
            &ForcePublish::None,
        );
        assert!(
            !plain.iter().any(|a| a.contains("force")),
            "a fast-forward push must build no force flag whatsoever: {plain:?}"
        );
    }

    /// `--set-upstream` appears when and only when it was asked for, and it is
    /// the long form (the short `-u` is the same flag, but one spelling means
    /// one thing to grep for).
    #[test]
    fn set_upstream_is_present_exactly_when_requested() {
        let with = plan_export::push_argv(
            &branch("main"),
            &remote("origin"),
            true,
            &ForcePublish::None,
        );
        assert!(with.iter().any(|a| a == "--set-upstream"), "{with:?}");
        let without = plan_export::push_argv(
            &branch("main"),
            &remote("origin"),
            false,
            &ForcePublish::None,
        );
        assert!(
            !without.iter().any(|a| a == "--set-upstream" || a == "-u"),
            "{without:?}"
        );
        // And the flag never leaks into the fast-forward argv's shape: the
        // difference between the two is exactly one element.
        assert_eq!(with.len(), without.len() + 1);
    }

    /// Every push argv streams progress, or the operation reports nothing but
    /// `executing` for the whole transfer — the gap #229 closed for fetch and
    /// this slice closes for push.
    #[test]
    fn every_push_argv_asks_git_for_progress() {
        for force in every_force_mode() {
            for set_upstream in [true, false] {
                let argv = plan_export::push_argv(&branch("main"), &remote("origin"), set_upstream, &force);
                assert!(
                    argv.iter().any(|a| a == "--progress"),
                    "no --progress in {argv:?}"
                );
            }
        }
    }

    /// The classifier names the actionable cause, against git 2.43.0's real
    /// output.
    #[test]
    fn classification_names_the_actionable_cause() {
        for (stderr, expected) in [
            (
                " ! [rejected]        main -> main (stale info)\nerror: failed to \
                 push some refs to './up.git'",
                PushFailure::LeaseStale,
            ),
            (
                " ! [rejected]        main -> main (non-fast-forward)\nerror: failed \
                 to push some refs to 'origin'",
                PushFailure::NonFastForward,
            ),
            (
                " ! [rejected]        main -> main (fetch first)\nhint: Updates were \
                 rejected because the remote contains work that you do not have",
                PushFailure::NonFastForward,
            ),
            (
                "fatal: Authentication failed for 'https://example.invalid/r.git/'",
                PushFailure::AuthenticationFailed,
            ),
            (
                "git@github.com: Permission denied (publickey).\r\nfatal: Could not \
                 read from remote repository.",
                PushFailure::AuthenticationFailed,
            ),
            (
                "fatal: unable to access 'https://example.invalid/r.git/': Could not \
                 resolve host: example.invalid",
                PushFailure::RemoteUnreachable,
            ),
            (
                "remote: error: hook declined to update refs/heads/main\nTo origin",
                PushFailure::RemoteRejected,
            ),
            (
                "fatal: '/nope' does not appear to be a git repository",
                PushFailure::RemoteRejected,
            ),
        ] {
            assert_eq!(classify_failure(stderr), expected, "for {stderr:?}");
        }
    }

    /// **The ordering that matters.** A rejected lease prints *both* `stale
    /// info` and `failed to push some refs`, and on some transports an
    /// authentication failure prints a rejection marker too. The lease must win,
    /// because "your lease lost and the remote is untouched" is the sentence a
    /// user can act on.
    #[test]
    fn a_rejected_lease_outranks_the_generic_push_failure() {
        assert_eq!(
            classify_failure(
                " ! [rejected]        main -> main (stale info)\n\
                 error: failed to push some refs to 'https://example.invalid/r.git'\n\
                 hint: Updates were rejected because the remote contains work"
            ),
            PushFailure::LeaseStale,
            "a lease that lost must not be reported as a generic non-fast-forward"
        );
    }

    /// The load-bearing negative: an unrecognised failure lands in `Other`
    /// rather than the nearest-looking box, and `Other` adds no hint — a
    /// fabricated remedy is worse than none.
    #[test]
    fn an_unrecognised_failure_is_other_rather_than_a_guess() {
        for stderr in [
            "fatal: early EOF",
            "error: RPC failed; curl 92 HTTP/2 stream 5 was not closed cleanly",
            "fatal: Objekte konnten nicht gesendet werden",
            "",
        ] {
            assert_eq!(
                classify_failure(stderr),
                PushFailure::Other,
                "for {stderr:?}"
            );
        }
        assert_eq!(PushFailure::Other.hint(), None);
    }

    /// A successful push's own output must not be classified as anything — the
    /// classifier only ever runs on a non-zero exit, and this pins that its
    /// markers are not so loose that ordinary success text trips them.
    #[test]
    fn ordinary_push_output_matches_no_failure_marker() {
        for stderr in [
            "Enumerating objects: 15, done.",
            "To ./up.git\n * [new branch]      main -> main",
            "To ./up.git\n   fc81d61..43138c2  main -> main",
            "branch 'main' set up to track 'origin/main'.",
            "Everything up-to-date",
        ] {
            assert_eq!(
                classify_failure(stderr),
                PushFailure::Other,
                "success output must not match a specific failure marker: {stderr:?}"
            );
        }
    }

    /// The two "the remote moved under you" failures are `409`s and everything
    /// else is a `400` — asserted as a literal table rather than by calling
    /// `status()` on both sides.
    #[test]
    fn each_failure_kind_gets_the_status_its_remedy_implies() {
        for (kind, status) in [
            (PushFailure::LeaseStale, StatusCode::CONFLICT),
            (PushFailure::NonFastForward, StatusCode::CONFLICT),
            (PushFailure::AuthenticationFailed, StatusCode::BAD_REQUEST),
            (PushFailure::RemoteUnreachable, StatusCode::BAD_REQUEST),
            (PushFailure::RemoteRejected, StatusCode::BAD_REQUEST),
            (PushFailure::Other, StatusCode::BAD_REQUEST),
        ] {
            assert_eq!(kind.status(), status, "for {kind:?}");
        }
    }

    /// One moved ref, for the cancellation-wording test below.
    fn moved(name: &str) -> RemoteRefUpdate {
        RemoteRefUpdate {
            ref_name: name.into(),
            old_oid: Some("aaa".into()),
            new_oid: Some("bbb".into()),
        }
    }

    /// A cancelled push's sentence is chosen by **what was observed**, and the
    /// two branches say opposite things.
    ///
    /// Both legs are load-bearing and neither stands alone. An implementation
    /// stuck on the empty-diff sentence would tell a user whose force-publish
    /// *did* land that this repository never saw the remote accept it — on the
    /// one operation whose effect is on another machine, where no later local
    /// read can correct the record. One stuck on the non-empty sentence would
    /// claim the remote accepted refs nothing here ever saw. So each leg
    /// asserts the other branch's discriminating phrase is **absent**, not just
    /// that its own is present.
    ///
    /// The count and its plural are asserted too — this sentence is read by a
    /// human deciding whether to go looking at the remote, and "after 2
    /// remote-tracking ref had already been updated" is the shape a dropped
    /// plural arm produces.
    #[test]
    fn a_cancelled_push_says_which_of_the_two_things_it_observed() {
        let (b, r) = (branch("main"), remote("origin"));

        let (status, nothing_seen) = cancelled_response(&b, &r, &[]);
        assert_eq!(status, StatusCode::CONFLICT, "{nothing_seen}");
        assert!(
            nothing_seen.contains("never saw the remote accept"),
            "{nothing_seen}"
        );
        assert!(
            nothing_seen.contains("Fetch to see where the remote actually is"),
            "an unobserved cancel must send the reader to the only place that \
             can answer: {nothing_seen}"
        );
        assert!(
            !nothing_seen.contains("had already been updated"),
            "an empty diff must not claim the remote accepted anything: \
             {nothing_seen}"
        );

        let (status, one) = cancelled_response(&b, &r, &[moved("refs/remotes/origin/main")]);
        assert_eq!(status, StatusCode::CONFLICT, "{one}");
        assert!(
            one.contains("after 1 remote-tracking ref had already been updated"),
            "{one}"
        );
        assert!(
            one.contains("accepted by the remote"),
            "a ref that moved here moved because the remote accepted it, and \
             the sentence is what tells the user their push partly landed: {one}"
        );
        assert!(
            !one.contains("never saw the remote accept"),
            "a diff that did move must not be reported as one that did not: {one}"
        );

        let (_, two) = cancelled_response(
            &b,
            &r,
            &[
                moved("refs/remotes/origin/main"),
                moved("refs/remotes/origin/side"),
            ],
        );
        assert!(
            two.contains("after 2 remote-tracking refs had already been updated"),
            "the count and its plural are both read by a human: {two}"
        );
    }

    /// The replaced tip comes from the observed diff, and only for the branch
    /// this push named — a sibling ref moving in the same push must not be
    /// reported as the one that was overwritten.
    #[test]
    fn the_replaced_tip_is_the_observed_one_for_this_branch_only() {
        let updated = vec![
            RemoteRefUpdate {
                ref_name: "refs/remotes/origin/other".into(),
                old_oid: Some("bbb".into()),
                new_oid: Some("ccc".into()),
            },
            RemoteRefUpdate {
                ref_name: "refs/remotes/origin/main".into(),
                old_oid: Some("aaa".into()),
                new_oid: Some("ddd".into()),
            },
        ];
        assert_eq!(
            replaced_tip(&updated, &branch("main"), &remote("origin")),
            Some("aaa".to_string())
        );
        assert_eq!(
            replaced_tip(&updated, &branch("absent"), &remote("origin")),
            None
        );
        // A ref that was *created* replaced nothing, and must not borrow a
        // sibling's old oid.
        let created = vec![RemoteRefUpdate {
            ref_name: "refs/remotes/origin/main".into(),
            old_oid: None,
            new_oid: Some("ddd".into()),
        }];
        assert_eq!(
            replaced_tip(&created, &branch("main"), &remote("origin")),
            None
        );
    }
}
