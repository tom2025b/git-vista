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

use axum::http::StatusCode;

use git_vista_protocol::{
    plan_export, FetchError, FetchFailureKind, FetchSuccess, RemoteName, RemoteRefUpdate,
};

use super::transfer::{diff_refs, parse_progress, remote_tracking_refs};
use super::*;
use crate::handlers::AppEntry;

/// The endpoint name in log lines, matching every other executor here.
const ENDPOINT: &str = "/api/fetch";

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

    // --- credential helper blocked by this server's own sandbox --------
    // Checked *before* the generic authentication markers below, and it has
    // to be: this host's configured HTTPS credential helper
    // (`credential.helper = !/usr/bin/gh auth git-credential`, global
    // `~/.gitconfig`) crashes on startup when the sandbox's Network tier
    // withholds `~/.config/gh` (`sandbox::DEFAULT_SECRET_EXCLUDES` — it
    // holds `hosts.yml`, the OAuth token, with no file-level carve-out for
    // the rest of the directory). Reproduced end to end (#325 lane 4,
    // 2026-08-05): a real `git ls-remote` against a live 401 challenge, with
    // this exact broken helper wired in, produces git falling back to its
    // own generic line —
    //   fatal: could not read Username for '...': No such device or address
    // — on the *same* stderr as gh's own crash. That generic fallback
    // already matches the "could not read username" marker in the
    // authentication block below, which would classify this
    // `AuthenticationFailed` and tell the user to "configure a credential
    // helper" — advice that is false here, since one is configured. `gh`'s
    // crash text is the more specific, and more true, signal, so it is
    // checked first and wins.
    //
    // Two independent markers, checked together: `gh`'s own Cobra-wrapper
    // crash text (stable across the version checked, `2.63.2`, but a future
    // release could reword it), and the underlying mechanism — the excluded
    // directory's name next to a permission refusal — which stays true
    // regardless of `gh`'s wording as long as the sandbox is what is doing
    // the excluding.
    if s.contains("failed to create root command")
        || (s.contains(".config/gh") && s.contains("permission denied"))
    {
        return FetchFailureKind::CredentialHelperBlocked;
    }

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
    let fetch_argv = plan_export::fetch_argv(remote);
    let fetch_args: Vec<&str> = fetch_argv.iter().map(String::as_str).collect();
    let run = Box::pin(crate::git_cmd::git_streamed_for(
        repo,
        &fetch_args,
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
///
/// # These entries are a deduplication key. Do not collapse them.
///
/// #329 read as "one action should be one journal line" and an attempt was
/// made to replace this loop with a single summary entry carrying the count.
/// It was reverted in `0a7ba777`, because these entries are quietly doing a
/// *second* job nobody had written down: `assemble_feed`'s attribution step
/// drops a reflog line when a journal entry matches it on kind, resulting oid
/// and moment. One fetch of 94 refs writes 94 reflog lines, and it is these 94
/// per-ref entries — each carrying a `new_oid` — that suppress them. A summary
/// entry has no `new_oid`, matches nothing, and lets all 94 reflog lines back
/// in: the "fix" took the feed from 94 rows to 95. The feed-level fold
/// (`git_vista_core::activity::fold_ref_update_bursts`) is where that noise is
/// collapsed, and it covers terminal fetches, which have no journal entry at
/// all.
///
/// # The per-ref cost was the whole of #485, and it is fixed here
///
/// Until #485 every iteration of this loop reached `journal::append`, which
/// called `capture_refs` — a full ref read of the repository, embedded into
/// that one line. A fetch of N refs therefore performed N full ref reads and
/// wrote N lines whose size itself grew with N. Measured 2026-08-25: 94 refs
/// cost 527 KiB and 1.1 s, 500 refs cost 14 MiB and 27.6 s — and it is awaited
/// before the endpoint responds, so it was the user's fetch latency.
///
/// The entries stay one per ref, because of the dedup key above. What
/// collapsed is the *capture*: the whole batch goes to
/// [`crate::handlers::journal_app_events`] in one call, and
/// `journal::append_all` reads the refs once, stores them once, and has the
/// batch's other lines reference that one snapshot (ADR 0080). One
/// `spawn_blocking` hop and one file open now serve the whole batch too.
///
/// See `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`.
async fn journal_updates(
    repo: &Path,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
    verb: &str,
) {
    if updated.is_empty() {
        return;
    }
    let entries: Vec<AppEntry> = updated
        .iter()
        .map(|update| {
            let short_name = update
                .ref_name
                .strip_prefix("refs/remotes/")
                .unwrap_or(&update.ref_name);
            AppEntry {
                kind: ActivityKind::Fetch,
                ref_name: Some(update.ref_name.clone()),
                // Observed, not read back through `Obs::from_read`: these come
                // from the before/after listings this module took itself, so
                // `None` genuinely means the ref did not exist. `Obs::Unknown`
                // — "git could not be read" — cannot arise from a diff of two
                // listings this module is already holding, which is why these
                // go straight to `Option` instead of through the `Obs` that
                // [`journal_unobserved`], whose tips really are unknown, needs.
                old_oid: update.old_oid.clone(),
                new_oid: update.new_oid.clone(),
                summary: format!("{verb} ‘{short_name}’ from {}", remote.as_str()),
            }
        })
        .collect();
    let repo = repo.to_path_buf();
    let _ =
        tokio::task::spawn_blocking(move || crate::handlers::journal_app_events(&repo, entries))
            .await;
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
///
/// # The all-`None` shape of this entry is load-bearing (#486, ADR 0081)
///
/// No `ref_name` *and* no oid — `Obs::Unknown` flattens to `None` — is what
/// `git_vista_core::activity::admits_it_could_not_read_the_refs` recognises,
/// and this is the only writer of that shape at `ActivityKind::Fetch`. It is
/// how the feed keeps this admission out of `fold_ref_update_bursts`: the
/// fetch succeeded, so git logged every ref it moved, and this entry carries
/// no `new_oid` with which to suppress those reflog lines. Before #486 it
/// folded in with them, and four moved refs rendered as "fetch — 5 refs
/// updated" with the admission gone.
///
/// **Giving this entry a `ref_name` or a placeholder oid would silently
/// restore that defect.** If it ever has to name something, add an explicit
/// discriminator to `ActivityEvent` and change the core's predicate in the
/// same commit.
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
    use git_vista_core::activity::{ActivitySource, RefsAtEvent};

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

    /// #325 lane 4: the exact combined stderr a real `git ls-remote` produced
    /// against a live HTTP `401` challenge, with `credential.helper = !gh
    /// auth git-credential` wired in and `gh`'s own config unreadable
    /// (reproduced by denying `~/.config/gh/config.yml`, the same errno
    /// shape — `EACCES` — the sandbox's Landlock policy produces for an
    /// excluded path). Pinned verbatim rather than paraphrased, so this test
    /// fails if `gh`'s crash wording ever drifts far enough to stop matching.
    ///
    /// Also proves the ordering claim in `classify_failure`'s own comment:
    /// this same string contains "could not read username" (the generic
    /// auth marker) *and* "failed to create root command" (the specific
    /// one) — without the specific check running first, this would silently
    /// classify as [`FetchFailureKind::AuthenticationFailed`] instead, which
    /// is why that ordering is asserted here rather than merely described.
    #[test]
    fn a_sandboxed_gh_credential_helper_crash_classifies_distinctly_from_generic_auth() {
        let stderr = "failed to create root command: failed to read configuration: open \
                       /home/tom/.config/gh/config.yml: permission denied\n\n\
                       fatal: could not read Username for 'http://127.0.0.1:19191': No such \
                       device or address";
        assert!(
            stderr
                .to_ascii_lowercase()
                .contains("could not read username"),
            "fixture must still trip the generic auth marker, or this test \
             is not proving the ordering it claims to"
        );
        assert_eq!(
            classify_failure(stderr),
            FetchFailureKind::CredentialHelperBlocked
        );
    }

    /// The mechanism-based marker (directory name + "permission denied")
    /// catches the same failure even if `gh`'s wrapper wording changes —
    /// tested independently of the Cobra preamble so a future `gh` version
    /// bumping "failed to create root command" does not silently fall back
    /// to `AuthenticationFailed` with no test noticing.
    #[test]
    fn the_config_gh_permission_denied_marker_is_independent_of_ghs_wrapper_text() {
        assert_eq!(
            classify_failure(
                "some future gh: open /home/tom/.config/gh/config.yml: permission denied"
            ),
            FetchFailureKind::CredentialHelperBlocked
        );
    }
    // -----------------------------------------------------------------------
    // #485 — what one fetch costs the journal (ADR 0080)
    // -----------------------------------------------------------------------

    /// The canonical seeded repository (ADR 0076): a real `.git` directory
    /// and one commit, so journaling engages and `capture_refs` has something
    /// to observe. From the fixture catalogue rather than a `git init` spawned
    /// here — this module's process spawns are the planner's git argv, and
    /// `argv_boundary` is right to hold that line.
    fn journalling_repo() -> git_vista_fixtures::Fixture {
        git_vista_fixtures::seeded()
    }

    /// **#485 at this module's own boundary.** One fetch that moved N refs
    /// still journals N entries — the dedup key the `0a7ba777` revert exists
    /// to protect — but pays for **one** ref read and stamps **one** moment.
    ///
    /// The two counts are asserted together because the two costs were the
    /// same loop. Restoring the per-ref `journal_app_event` call gives 12
    /// captures, and gives each entry its own `now_secs()` reading — the
    /// drift that inflated the folded count past ~170 refs.
    ///
    /// Read back through `journal::read_all`, i.e. through the parser
    /// `/api/activity` uses, rather than by inspecting what this function was
    /// handed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_fetch_journals_every_ref_at_one_moment_under_one_capture() {
        let (_dir, repo) = journalling_repo();
        let updated: Vec<RemoteRefUpdate> = (0..12)
            .map(|i| RemoteRefUpdate {
                ref_name: format!("refs/remotes/origin/b{i}"),
                old_oid: None,
                new_oid: Some(format!("{i:040x}")),
            })
            .collect();

        journal_updates(
            &repo,
            &RemoteName::new("origin").expect("a valid remote name"),
            &updated,
            "fetched",
        )
        .await;

        let read = crate::journal::read_all(&repo);
        assert_eq!(
            read.len(),
            12,
            "one entry per moved ref, still — the entries are the feed's \
             dedup key against git's own reflog lines: {read:#?}"
        );
        let moments: std::collections::BTreeSet<i64> = read.iter().map(|e| e.time).collect();
        assert_eq!(
            moments.len(),
            1,
            "one fetch is one moment; {} distinct timestamps means the \
             entries are drifting apart again",
            moments.len()
        );
        let captures = read
            .iter()
            .filter(|e| matches!(e.refs, Some(RefsAtEvent::Captured { .. })))
            .count();
        assert_eq!(
            captures, 1,
            "12 moved refs must cost ONE full ref read, not {captures}"
        );
        assert_eq!(
            read.iter()
                .filter(|e| matches!(e.refs, Some(RefsAtEvent::InBatch { .. })))
                .count(),
            11,
            "and the other eleven must say where their capture is, rather \
             than going silent"
        );
        // The entries themselves are unchanged: still per-ref, still named.
        assert!(read
            .iter()
            .all(|e| e.kind == ActivityKind::Fetch && e.source == ActivitySource::App));
        assert!(
            read.iter()
                .any(|e| e.ref_name.as_deref() == Some("refs/remotes/origin/b11")),
            "the last ref of the batch must still have its own entry"
        );
    }

    /// The paired negative: nothing moved, nothing journaled — and in
    /// particular no ref read. An empty batch that still captured would put
    /// the cost back on every up-to-date fetch, which is the common case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fetch_that_moved_nothing_writes_no_journal_at_all() {
        let (_dir, repo) = journalling_repo();

        journal_updates(
            &repo,
            &RemoteName::new("origin").expect("a valid remote name"),
            &[],
            "fetched",
        )
        .await;

        assert!(
            crate::journal::read_all(&repo).is_empty(),
            "an up-to-date fetch leaves no trace"
        );
    }
}
