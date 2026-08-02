//! M2.20c (#229): executing [`GitOperation::FetchRemote`] — the first
//! long-running network operation this server actually runs.
//!
//! #227 (ADR 0039) fixed the typed vocabulary and #228 (ADR 0036) fixed the
//! Network-tier exec harness. What was left is the part that only shows up
//! once a command can take a minute: the user cannot see what it is doing,
//! and cannot stop it. Three things follow, and each of them is a decision
//! recorded in ADR 0043 rather than an implementation detail:
//!
//! 1. **Progress is streamed into the lifecycle that already exists.** The
//!    operation registry (M1.08) already publishes an `OperationStatus`
//!    through a `watch` channel that `GET /api/operations/{id}/events` turns
//!    into SSE. A fetch does not need a second mechanism — it needs the
//!    existing one to carry a payload finer than "executing". So this module
//!    parses git's own `--progress` records and reports them as
//!    [`TransferProgress`] on that same channel. No new endpoint, no new
//!    transport, no polling loop.
//!
//! 2. **Cancellation kills the child, and is proven to.** A cancel that only
//!    stopped *waiting* would leave `git fetch` running against the remote
//!    and writing objects — the exact shape of lie this repository keeps
//!    finding in green tests. `git_cmd::git_streamed_for` SIGKILLs the child
//!    and this module reports the outcome as observed, never inferred.
//!
//! 3. **What happened to the repository is observed, not read out of git's
//!    prose.** `refs/remotes/<remote>/*` is listed before and after; the
//!    difference *is* the answer to "did anything move?". That is what makes
//!    a cancelled fetch's terminal message trustworthy under any locale, and
//!    it is the same lesson #284 wrote down for branch deletion.
//!
//! # What is deliberately not here
//!
//! No `--prune`, no `--tags`, no refspec, no depth. `FetchRemote` carries a
//! remote and nothing else (#227), and a flag with nowhere to land in the
//! typed operation would be a flag the plan's reviewer never sees and the
//! plan's hash never binds.

use std::collections::BTreeMap;

use axum::http::StatusCode;

use git_vista_protocol::{
    FetchError, FetchFailureKind, FetchSuccess, RemoteName, RemoteRefUpdate, TransferPhase,
    TransferProgress,
};

use super::*;

/// The endpoint name in log lines, matching every other executor here.
const ENDPOINT: &str = "/api/fetch";

// ---------------------------------------------------------------------------
// Progress parsing
// ---------------------------------------------------------------------------

/// Parse one of git's `--progress` records into a [`TransferProgress`].
///
/// The records this recognises, verified byte-for-byte against git 2.43.0's
/// own output (see the tests below, which are built from a captured real
/// fetch):
///
/// ```text
/// remote: Enumerating objects: 121, done.
/// remote: Counting objects:  37% (45/121)
/// remote: Compressing objects: 100% (120/120), done.
/// Receiving objects:  66% (80/120), 174.40 KiB | 14.53 MiB/s
/// Resolving deltas: 100% (39/39), completed with 1 local object.
/// ```
///
/// `None` for anything else — including git's `From <url>` header, its
/// `a1b2c3..d4e5f6  main -> origin/main` summary lines, and every warning or
/// error. That is deliberate: this function's job is progress, and a record
/// it does not understand must not be turned into a fabricated phase. The
/// error path has its own reader ([`classify_failure`]) and the ref outcome
/// has its own observation.
///
/// # Locale
///
/// These phase names are gettext-translated: under `LC_ALL=de_DE` git prints
/// `Objekte empfangen`, and the `remote:`-prefixed three come from the
/// *remote's* locale, not this host's. Unrecognised records simply produce no
/// progress, so a non-English pair degrades to "no progress bar", never to a
/// wrong one. `SandboxedCommand` exposes no `env` setter by construction
/// (#228's C10 hazard #1), so this cannot be closed by forcing `LC_ALL=C`
/// here; ADR 0043 records that as an accepted, reported gap.
pub(super) fn parse_progress(record: &str) -> Option<TransferProgress> {
    let record = record.strip_prefix("remote:").unwrap_or(record).trim();
    let (phase, rest) = [
        ("Enumerating objects:", TransferPhase::Enumerating),
        ("Counting objects:", TransferPhase::Counting),
        ("Compressing objects:", TransferPhase::Compressing),
        ("Receiving objects:", TransferPhase::Receiving),
        ("Resolving deltas:", TransferPhase::Resolving),
    ]
    .into_iter()
    .find_map(|(needle, phase)| record.strip_prefix(needle).map(|rest| (phase, rest.trim())))?;

    // `Enumerating` reports a bare running count and no percentage; every
    // other phase reports `N% (a/b)`.
    let percent = rest
        .split('%')
        .next()
        .filter(|_| rest.contains('%'))
        .and_then(|p| p.trim().parse::<u8>().ok())
        .filter(|p| *p <= 100);

    let (objects, total_objects) = match rest.split_once('(') {
        Some((_, after)) => {
            let inside = after.split(')').next().unwrap_or("");
            match inside.split_once('/') {
                Some((done, total)) => (
                    done.trim().parse::<u64>().ok(),
                    total.trim().parse::<u64>().ok(),
                ),
                None => (None, None),
            }
        }
        // `Enumerating objects: 121, done.` — the count is the first token.
        None => (
            rest.split(&[',', ' '][..])
                .next()
                .and_then(|n| n.trim().parse::<u64>().ok()),
            None,
        ),
    };

    Some(TransferProgress {
        phase,
        percent,
        objects,
        total_objects,
    })
}

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

/// Classify a failed fetch from git's stderr into the wire taxonomy.
///
/// # Why this is a heuristic, and why it is still worth having
///
/// The exit status of `git fetch` is 128 for essentially every fatal error,
/// so it carries no classification at all. Nothing else about a failed fetch
/// is observable — there is no repository state to read, because the whole
/// point is that nothing happened. So the *only* source is git's stderr,
/// which is gettext-translated and version-dependent.
///
/// This is the same trade `classify_amend_failure` already makes, with the
/// same discipline: a documented marker set, [`FetchFailureKind::Other`] for
/// anything unmatched, and **git's own words forwarded verbatim in every
/// case**. The tag is an addition to git's message, never a replacement for
/// it, so a mis-tag costs a less specific hint and never a wrong explanation.
///
/// Ordering matters and is deliberate: authentication is checked before
/// rejection because a `403`/`401` from an HTTPS remote produces both an auth
/// marker and an access-denied one, and "your credentials didn't work" is the
/// actionable half.
pub(super) fn classify_failure(stderr: &str) -> FetchFailureKind {
    // Case-folded once: git varies capitalisation across transports
    // (`Authentication failed` from HTTP, `Permission denied` from SSH).
    let s = stderr.to_ascii_lowercase();

    // --- authentication -------------------------------------------------
    // HTTP: `fatal: Authentication failed for 'https://…'`.
    // HTTP with askpass forced off (#228): `could not read Username for …`.
    // SSH:  `git@host: Permission denied (publickey).`
    for marker in [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied (publickey",
        "invalid username or password",
    ] {
        if s.contains(marker) {
            return FetchFailureKind::AuthenticationFailed;
        }
    }

    // --- unreachable ----------------------------------------------------
    // Transport never got an answer: nothing about the remote repository is
    // known, so this is distinct from a remote that answered "no".
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
            return FetchFailureKind::RemoteUnreachable;
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
        "upload-pack: not our ref",
        "couldn't find remote ref",
    ] {
        if s.contains(marker) {
            return FetchFailureKind::RemoteRejected;
        }
    }

    FetchFailureKind::Other
}

// ---------------------------------------------------------------------------
// Observing what the fetch did
// ---------------------------------------------------------------------------

/// Every `refs/remotes/<remote>/*` ref and the object it points at.
///
/// `Err` is "we could not observe", which is a refusal reason and never
/// silently an empty map — a fetch whose before-state is unknown cannot
/// honestly answer "did anything move?" afterwards, and that answer is the
/// whole contract of a cancelled fetch (D5's posture: we did not observe
/// anything, so we may not act as though we did).
async fn remote_tracking_refs(
    repo: &Path,
    need: NetworkNeed,
    remote: &RemoteName,
) -> Result<BTreeMap<String, String>, String> {
    let prefix = format!("refs/remotes/{}/", remote.as_str());
    let output = run_git(
        repo,
        need,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    )
    .await
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(stderr_or(&output, "git for-each-ref failed."));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.trim().split_once(' ')?;
            Some((name.to_string(), oid.to_string()))
        })
        .collect())
}

/// The before/after difference, as the wire type. Sorted by ref name (the
/// `BTreeMap` gives that for free), so two identical fetches report
/// identically.
fn diff_refs(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RemoteRefUpdate> {
    let mut out = Vec::new();
    for (name, new_oid) in after {
        match before.get(name) {
            Some(old) if old == new_oid => {}
            old => out.push(RemoteRefUpdate {
                ref_name: name.clone(),
                old_oid: old.cloned(),
                new_oid: Some(new_oid.clone()),
            }),
        }
    }
    for (name, old_oid) in before {
        if !after.contains_key(name) {
            out.push(RemoteRefUpdate {
                ref_name: name.clone(),
                old_oid: Some(old_oid.clone()),
                new_oid: None,
            });
        }
    }
    out.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));
    out
}

// ---------------------------------------------------------------------------
// The fetch step, as an outcome rather than a response
// ---------------------------------------------------------------------------

/// What one run of the fetch step did, before any endpoint has decided what
/// HTTP status it deserves.
///
/// This type exists because M2.20d (#230) needs the fetch step *without* the
/// `/api/fetch` response contract wrapped around it: a pull is a fetch plus an
/// integration, and its wire shape is [`PullSuccess`]/[`PullError`], not
/// [`FetchSuccess`]/[`FetchError`]. Splitting here rather than letting
/// `planner::pull` build its own spawn is the whole point — there is exactly
/// one place in this server that runs `git fetch`, so the streaming progress,
/// the cancellation latch, the before/after ref observation and #228's
/// Network-tier hardening cannot exist in a second, quietly-diverging copy.
///
/// Every variant carries `updated`: the observed ref diff, which is the
/// honest answer to "what landed?" in *all* of them, including the ones that
/// failed part-way. [`Self::Unobservable`] is the exception, and it is the
/// exception precisely because that answer is what could not be obtained.
pub(super) enum FetchStep {
    /// `git fetch` exited 0. `updated` may still be empty (already up to
    /// date), which is a success.
    Completed { updated: Vec<RemoteRefUpdate> },
    /// The operator cancelled. `output` is `None` when the cancel landed
    /// before anything was spawned.
    Cancelled {
        updated: Vec<RemoteRefUpdate>,
        output: Option<std::process::Output>,
    },
    /// `git fetch` ran and exited non-zero.
    Failed {
        kind: FetchFailureKind,
        message: String,
        updated: Vec<RemoteRefUpdate>,
    },
    /// git could not be run at all, or the ref listing that establishes the
    /// baseline failed — nothing was spawned, so nothing moved.
    CouldNotRun { why: String },
    /// `git fetch` ran to completion and the re-read that would say what it
    /// moved failed. The one outcome with no ref diff, and the reason
    /// [`journal_unobserved`] exists.
    Unobservable { why: String },
}

/// Run `git fetch --progress <remote>` and report what it did.
///
/// The shape, in order, and why each step is where it is:
///
/// 1. **Observe the remote-tracking refs.** Before anything is spawned, so
///    the "did anything move?" question has a baseline. Failing to observe
///    refuses the fetch rather than running one whose outcome could not be
///    reported honestly.
/// 2. **Check the cancel latch before spawning.** A cancel that arrived while
///    the operation was queued behind the repository guard must not result in
///    a git process starting anyway.
/// 3. **Run, streaming.** Every stderr record goes through
///    [`parse_progress`]; recognised ones are published on the operation's
///    own channel.
/// 4. **Observe again, and diff.** This is the answer that reaches the
///    client, for success, failure and cancellation alike.
/// 5. **Journal** what moved. The generation bump is not done here:
///    `plan_and_execute_tracked` re-reads it after *every* operation and puts
///    it on the terminal record, so a fetch gets it by construction (and would
///    get it wrong if this module also did it).
///
/// `endpoint` is only ever a log label — `/api/fetch` or `/api/pull` — so the
/// two callers' stderr lines stay attributable. It never reaches an argv.
pub(super) async fn run_fetch(
    repo: &Path,
    need: NetworkNeed,
    remote: &RemoteName,
    endpoint: &str,
) -> FetchStep {
    debug_assert_eq!(
        need,
        NetworkNeed::Remote,
        "a fetch reaches a remote; if it arrives here declared Local, \
         `network_need_for_operation` is wrong and the sandbox will \
         (correctly) deny the connect"
    );

    let before = match remote_tracking_refs(repo, need, remote).await {
        Ok(refs) => refs,
        Err(why) => {
            return FetchStep::CouldNotRun {
                why: format!("couldn't list refs/remotes/{}: {why}", remote.as_str()),
            }
        }
    };

    let cancel = crate::operations::cancel_signal();
    if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
        // Nothing spawned, so nothing can have moved — but the answer is
        // still built from the same diff (of `before` against itself) rather
        // than from a hardcoded empty list, so there is exactly one place
        // that decides what "which refs moved" means.
        return FetchStep::Cancelled {
            updated: diff_refs(&before, &before),
            output: None,
        };
    }

    // `Box::pin`, and it is load-bearing rather than stylistic. Measured:
    // `git_streamed_for`'s future is ~66 KiB, which is most of the whole
    // planner pipeline's state machine — inlined, every caller that awaits it
    // (`run_fetch`, then `exec_fetch` or `exec_pull`, then `execute`, then
    // `plan_and_execute_in`) carries a copy in its own frame, and in a debug
    // build that overflows a 2 MiB test thread's stack. One allocation, on an
    // operation that is about to open a socket and receive a pack, is not a
    // cost worth measuring; the crash it prevents is.
    let run = Box::pin(crate::git_cmd::git_streamed_for(
        repo,
        &["fetch", "--progress", remote.as_str()],
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
        Err(e) => return FetchStep::CouldNotRun { why: e.to_string() },
    };

    let after = match remote_tracking_refs(repo, need, remote).await {
        Ok(refs) => refs,
        Err(why) => {
            // The only exit path reached *after* git fetch has already run
            // and *without* a ref diff to describe it. Every other exit
            // (success, failure, cancelled) journals what moved; returning
            // silently from here would leave the activity feed claiming
            // nothing happened while the repository on disk says otherwise —
            // the silent divergence the rest of this module observes refs to
            // avoid. So the fact of the unobserved run is journaled instead.
            journal_unobserved(repo, remote, &why).await;
            return FetchStep::Unobservable { why };
        }
    };
    let updated = diff_refs(&before, &after);

    if run.cancelled {
        journal_updates(repo, remote, &updated, "cancelled part-way").await;
        return FetchStep::Cancelled {
            updated,
            output: Some(run.output),
        };
    }

    let stderr = String::from_utf8_lossy(&run.output.stderr).into_owned();
    if !run.output.status.success() {
        let kind = classify_failure(&stderr);
        let message = stderr_stdout_or(&run.output, "git fetch failed.");
        eprintln!("git-vista: {endpoint} failed ({kind:?}): {message}");
        // A failed fetch can still have updated some refs before it died, and
        // the journal must record what actually landed either way.
        journal_updates(repo, remote, &updated, "failed part-way").await;
        return FetchStep::Failed {
            kind,
            message,
            updated,
        };
    }

    journal_updates(repo, remote, &updated, "fetched").await;
    FetchStep::Completed { updated }
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// `git fetch --progress <remote>` (`POST /api/fetch`) — [`run_fetch`] plus
/// this endpoint's own response contract, and nothing else.
pub(super) async fn exec_fetch(
    repo: &Path,
    need: NetworkNeed,
    remote: &RemoteName,
) -> (StatusCode, String) {
    match run_fetch(repo, need, remote, ENDPOINT).await {
        FetchStep::CouldNotRun { why } => couldnt_run(ENDPOINT, &why),
        FetchStep::Unobservable { why } => couldnt_run(
            ENDPOINT,
            &format!(
                "the fetch ran but refs/remotes/{} could not be re-read: {why}",
                remote.as_str()
            ),
        ),
        FetchStep::Cancelled { updated, output } => {
            cancelled_response(remote, updated, output.as_ref())
        }
        FetchStep::Failed {
            kind,
            message,
            updated,
        } => (StatusCode::BAD_REQUEST, error_body(kind, message, updated)),
        FetchStep::Completed { updated } => {
            let message = if updated.is_empty() {
                format!("Fetched from ‘{}’: already up to date.", remote.as_str())
            } else {
                format!(
                    "Fetched from ‘{}’: {} remote-tracking ref{} updated.",
                    remote.as_str(),
                    updated.len(),
                    if updated.len() == 1 { "" } else { "s" }
                )
            };
            println!("[{ENDPOINT}] {message}");
            (
                StatusCode::OK,
                serde_json::to_string(&FetchSuccess {
                    remote: remote.as_str().to_string(),
                    message,
                    updated_refs: updated,
                })
                .expect("FetchSuccess serialization cannot fail"),
            )
        }
    }
}

/// The cancelled terminal response.
///
/// `409` rather than a success code because the operation did not do what was
/// asked — the registry derives `OperationState::Failed` from a non-2xx, and
/// a cancelled fetch recorded as `Succeeded` would be exactly the wrong thing
/// for a reconnecting client to read. The *cancel request itself* succeeds
/// (`202` from `POST /api/operations/{id}/cancel`); this is the status of the
/// fetch it stopped, which is a different question.
///
/// The message states plainly which of the two cases happened, and it is
/// built from the observed ref diff — never from git's output, which after a
/// SIGKILL is a truncated progress line.
fn cancelled_response(
    remote: &RemoteName,
    updated: Vec<RemoteRefUpdate>,
    output: Option<&std::process::Output>,
) -> (StatusCode, String) {
    let message = if updated.is_empty() {
        format!(
            "The fetch from ‘{}’ was cancelled before any remote-tracking ref was updated.",
            remote.as_str()
        )
    } else {
        format!(
            "The fetch from ‘{}’ was cancelled after {} remote-tracking ref{} had already \
             been updated.",
            remote.as_str(),
            updated.len(),
            if updated.len() == 1 { "" } else { "s" }
        )
    };
    if let Some(output) = output {
        // Redacted already by the streaming runner; logged rather than
        // returned, since a killed child's last words are diagnostics, not an
        // explanation the user asked for.
        eprintln!(
            "git-vista: {ENDPOINT} cancelled; git's last output: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next_back()
                .unwrap_or("(none)")
        );
    }
    (
        StatusCode::CONFLICT,
        error_body(FetchFailureKind::Cancelled, message, updated),
    )
}

/// The one constructor for `POST /api/fetch`'s error contract, so every
/// non-2xx body this endpoint produces parses as [`FetchError`] — the same
/// guarantee `/api/amend-commit` makes through `amend_refusal`.
pub(crate) fn error_body(
    kind: FetchFailureKind,
    message: String,
    updated_refs: Vec<RemoteRefUpdate>,
) -> String {
    serde_json::to_string(&FetchError {
        kind,
        message,
        updated_refs,
    })
    .expect("FetchError serialization cannot fail")
}

/// Journal one entry per remote-tracking ref that actually moved.
///
/// Per ref rather than one summary entry: the activity feed is keyed on refs,
/// and `ActivityKind::Fetch` with a `ref_name` is what lets a later view say
/// "origin/main moved from X to Y" instead of "a fetch happened". Nothing is
/// journaled when nothing moved, so an up-to-date fetch leaves no trace —
/// the same posture `exec_checkout` takes towards a no-op checkout.
async fn journal_updates(
    repo: &Path,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
    verb: &str,
) {
    for update in updated {
        let short_name = update
            .ref_name
            .strip_prefix("refs/remotes/")
            .unwrap_or(&update.ref_name);
        journal_app_event(
            repo,
            ActivityKind::Fetch,
            Some(update.ref_name.clone()),
            // Observed, not read back through `Obs::from_read`: these come
            // from the before/after listings this module took itself, so
            // "absent" genuinely means the ref did not exist.
            match &update.old_oid {
                Some(oid) => Obs::Known(oid.clone()),
                None => Obs::Absent,
            },
            match &update.new_oid {
                Some(oid) => Obs::Known(oid.clone()),
                None => Obs::Absent,
            },
            format!("{verb} ‘{short_name}’ from {}", remote.as_str()),
        )
        .await;
    }
}

/// Journal the one fetch outcome this module cannot name: `git fetch` ran to
/// completion and the re-read of `refs/remotes/<remote>/*` that would say what
/// it moved failed.
///
/// **One entry, with no `ref_name`**, because the per-ref set is precisely
/// what is unknown — inventing a ref name here would be the fabrication
/// [`journal_updates`] avoids by only ever journaling refs it watched change.
///
/// **`Obs::Unknown` on both tips, not `Obs::Absent`** — D5's distinction, and
/// the reason it exists. `Absent` would assert the refs do not exist; the
/// truth is that git could not be read. `journal_app_event` turns `Unknown`
/// into an explicit "(tips unknown — git could not be read)" on the summary,
/// so a reader of the feed sees an admission rather than a gap.
///
/// `why` is already redacted: it comes back through `run_git` under
/// [`NetworkNeed::Remote`], which applies the same `redact_if_remote` every
/// other Network-tier output goes through (#228).
async fn journal_unobserved(repo: &Path, remote: &RemoteName, why: &str) {
    journal_app_event(
        repo,
        ActivityKind::Fetch,
        None,
        Obs::Unknown,
        Obs::Unknown,
        format!(
            "fetched from ‘{}’, but refs/remotes/{} could not be re-read afterwards, \
             so which remote-tracking refs moved is unknown: {why}",
            remote.as_str(),
            remote.as_str()
        ),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records captured verbatim from a real `git fetch --progress` (git
    /// 2.43.0) against a local remote, `\r`-split the way
    /// `git_cmd::emit_records` splits them.
    #[test]
    fn every_real_progress_record_shape_parses() {
        let cases: &[(&str, TransferProgress)] = &[
            (
                "remote: Enumerating objects: 121, done.",
                TransferProgress {
                    phase: TransferPhase::Enumerating,
                    percent: None,
                    objects: Some(121),
                    total_objects: None,
                },
            ),
            (
                "remote: Counting objects:  37% (45/121)",
                TransferProgress {
                    phase: TransferPhase::Counting,
                    percent: Some(37),
                    objects: Some(45),
                    total_objects: Some(121),
                },
            ),
            (
                "remote: Compressing objects: 100% (120/120), done.",
                TransferProgress {
                    phase: TransferPhase::Compressing,
                    percent: Some(100),
                    objects: Some(120),
                    total_objects: Some(120),
                },
            ),
            (
                "Receiving objects:  66% (80/120), 174.40 KiB | 14.53 MiB/s",
                TransferProgress {
                    phase: TransferPhase::Receiving,
                    percent: Some(66),
                    objects: Some(80),
                    total_objects: Some(120),
                },
            ),
            (
                "Resolving deltas: 100% (39/39), completed with 1 local object.",
                TransferProgress {
                    phase: TransferPhase::Resolving,
                    percent: Some(100),
                    objects: Some(39),
                    total_objects: Some(39),
                },
            ),
        ];
        for (record, expected) in cases {
            assert_eq!(
                parse_progress(record).as_ref(),
                Some(expected),
                "failed to parse {record:?}"
            );
        }
    }

    /// The paired negative: everything else a fetch prints must produce **no**
    /// progress. Without this, a parser that returned a default
    /// `TransferProgress` for any input would pass the test above and publish
    /// a fabricated phase for git's ref-summary lines.
    #[test]
    fn non_progress_records_produce_no_progress() {
        for record in [
            "From /tmp/upstream",
            "   fc81d61..43138c2  main       -> origin/main",
            " * [new branch]      feature    -> origin/feature",
            "fatal: Authentication failed for 'https://example.invalid/r.git/'",
            "remote: Total 120 (delta 39), reused 0 (delta 0), pack-reused 0",
            "warning: no common commits",
            "",
            "remote:",
        ] {
            assert_eq!(
                parse_progress(record),
                None,
                "{record:?} must not be read as progress"
            );
        }
    }

    /// A percentage git could not have printed is dropped rather than
    /// clamped: a bar drawn from a fabricated number is worse than no bar.
    #[test]
    fn an_impossible_percentage_is_dropped_not_clamped() {
        let p = parse_progress("Receiving objects: 250% (5/2)").unwrap();
        assert_eq!(p.percent, None);
        assert_eq!(p.objects, Some(5));
    }

    #[test]
    fn classification_names_the_actionable_cause() {
        for (stderr, expected) in [
            (
                "fatal: Authentication failed for 'https://example.invalid/r.git/'",
                FetchFailureKind::AuthenticationFailed,
            ),
            (
                "git@github.com: Permission denied (publickey).\r\nfatal: Could not read \
                 from remote repository.",
                FetchFailureKind::AuthenticationFailed,
            ),
            (
                "fatal: unable to access 'https://example.invalid/r.git/': Could not \
                 resolve host: example.invalid",
                FetchFailureKind::RemoteUnreachable,
            ),
            (
                "fatal: unable to access 'http://127.0.0.1:1/r.git/': Failed to connect \
                 to 127.0.0.1 port 1: Connection refused",
                FetchFailureKind::RemoteUnreachable,
            ),
            (
                "remote: Repository not found.\nfatal: repository \
                 'https://example.invalid/r.git/' not found",
                FetchFailureKind::RemoteRejected,
            ),
            (
                "fatal: '/nope' does not appear to be a git repository",
                FetchFailureKind::RemoteRejected,
            ),
        ] {
            assert_eq!(classify_failure(stderr), expected, "for {stderr:?}");
        }
    }

    /// The load-bearing negative: an unrecognised failure must land in
    /// `Other`, not in the nearest-looking box. A classifier that guessed
    /// would make the tag actively misleading, which is worse than absent.
    #[test]
    fn an_unrecognised_failure_is_other_rather_than_a_guess() {
        for stderr in [
            "fatal: early EOF",
            "error: RPC failed; curl 92 HTTP/2 stream 5 was not closed cleanly",
            "fatal: Objekte konnten nicht empfangen werden",
            "",
        ] {
            assert_eq!(
                classify_failure(stderr),
                FetchFailureKind::Other,
                "for {stderr:?}"
            );
        }
    }

    /// Auth outranks rejection when a remote's stderr carries both markers —
    /// "your credentials didn't work" is the actionable half of a 403.
    #[test]
    fn an_authenticated_403_classifies_as_auth_not_rejection() {
        assert_eq!(
            classify_failure(
                "remote: Invalid username or password.\nfatal: The requested URL \
                 returned error: 403"
            ),
            FetchFailureKind::AuthenticationFailed
        );
    }

    fn refs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_ref_diff_reports_moved_new_and_gone_refs_and_nothing_else() {
        let before = refs(&[
            ("refs/remotes/origin/main", "aaa"),
            ("refs/remotes/origin/stable", "bbb"),
            ("refs/remotes/origin/dropped", "ccc"),
        ]);
        let after = refs(&[
            ("refs/remotes/origin/main", "ddd"),
            ("refs/remotes/origin/stable", "bbb"),
            ("refs/remotes/origin/fresh", "eee"),
        ]);
        let diff = diff_refs(&before, &after);
        assert_eq!(
            diff,
            vec![
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/dropped".into(),
                    old_oid: Some("ccc".into()),
                    new_oid: None,
                },
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/fresh".into(),
                    old_oid: None,
                    new_oid: Some("eee".into()),
                },
                RemoteRefUpdate {
                    ref_name: "refs/remotes/origin/main".into(),
                    old_oid: Some("aaa".into()),
                    new_oid: Some("ddd".into()),
                },
            ],
            "an unchanged ref must not appear, and the order must be stable"
        );
    }

    #[test]
    fn an_unchanged_listing_diffs_to_nothing() {
        let same = refs(&[("refs/remotes/origin/main", "aaa")]);
        assert!(diff_refs(&same, &same).is_empty());
    }
}
