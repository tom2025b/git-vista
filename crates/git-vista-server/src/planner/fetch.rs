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

use git_vista_protocol::{FetchError, FetchFailureKind, FetchSuccess, RemoteName, RemoteRefUpdate};

use super::transfer::{diff_refs, parse_progress, remote_tracking_refs};
use super::*;

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

/// Journal **one** entry per fetch operation, naming how many remote-tracking
/// refs moved — not one entry per ref.
///
/// # #329: this used to be one entry per ref, and that was the bug
///
/// A single `git fetch` against an active upstream can move dozens of
/// remote-tracking refs at once (measured: 94, on the first fetch of a repo
/// the owner actually uses). Journaling one `ActivityKind::Fetch` per ref
/// buried the one event a reader cared about among 94 that all say "a fetch
/// happened" — the feed became noise exactly when a fetch did the most work.
///
/// The fix matches the granularity `exec_fetch`'s own response already
/// settled on: `POST /api/fetch` never told the client "origin/main moved
/// from X to Y" for each ref — it says "Fetched from ‘origin’: 94
/// remote-tracking refs updated." (see `exec_fetch`'s `message`). One
/// journal entry at that same granularity is what lets the feed say the same
/// thing the endpoint already says, instead of reconstructing it 94 times.
///
/// `ref_name`/`old_oid`/`new_oid` are `None` on the journaled entry — not
/// `Obs::Unknown` (that means "git could not be read"; here git was read
/// fine, there is just no *one* ref this event is about) but `Obs::Absent`,
/// same as `journal_app_event` already uses to mean "nothing here to name".
/// This is a deliberate, scope-bounded drop of detail, not an oversight:
/// `ActivityEvent` (`git_vista_core::activity`) has exactly one `ref_name`
/// and one `old_oid`/`new_oid` pair — a schema shaped for "one ref moved",
/// shared with every journal line already on disk. Widening it to carry a
/// list of refs is a `git-vista-core` change, outside this fix's file set
/// (journal.rs + this file), and arguably shouldn't happen here anyway: the
/// per-ref detail is not lost, it is available for the moment it happens in
/// `FetchSuccess::updated_refs` (the response body `exec_fetch` already
/// returns) — a future drill-down UI has a source for it that isn't "94 rows
/// in the activity feed". Stuffing 94 ref names into one `summary` string
/// would just move the noise from "94 rows" to "1 unreadable row".
///
/// Nothing is journaled when nothing moved, so an up-to-date fetch leaves no
/// trace — the same posture `exec_checkout` takes towards a no-op checkout.
///
/// # Known gap this fix does not close (outside journal.rs/fetch.rs)
///
/// `assemble_feed` (`git_vista_core::activity`) also reads git's own
/// per-ref reflogs for every remote-tracking branch and de-duplicates a
/// reflog line against a journal entry **only when their `new_oid`s match
/// exactly**. The 94 per-ref journal entries this fix removes were, in
/// effect, doing double duty: they were also the mechanism that made the 94
/// per-ref reflog lines a `git fetch` writes disappear from the feed. A
/// single aggregate journal entry has no one `new_oid`, so it cannot match
/// any of them. Left alone, `assemble_feed` would show the 94
/// `ActivitySource::External` reflog rows *plus* this one
/// `ActivitySource::App` summary row — 95 rows, not 1. Fixing that needs a
/// change in `assemble_feed` (`crates/git-vista-core/src/activity.rs`),
/// which is not in this lane's file set; flagging it rather than reaching
/// for it.
async fn journal_updates(
    repo: &Path,
    remote: &RemoteName,
    updated: &[RemoteRefUpdate],
    verb: &str,
) {
    if updated.is_empty() {
        return;
    }
    journal_app_event(
        repo,
        ActivityKind::Fetch,
        None,
        Obs::Absent,
        Obs::Absent,
        format!(
            "{verb} {} remote-tracking ref{} from {}",
            updated.len(),
            if updated.len() == 1 { "" } else { "s" },
            remote.as_str()
        ),
    )
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
    use git_vista_core::activity::ActivitySource;

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
    // #329: journal_updates journals ONE event per fetch, not one per ref.
    //
    // These exercise `journal_updates` directly against a real `.git` dir
    // (`crate::journal` requires one) rather than through the full
    // `POST /api/fetch` pipeline — that pipeline, and the response-shape
    // assertions for it, live in `fetch_suite.rs`, a sibling module outside
    // this fix's file set. What belongs here is proof of the one contract
    // this file changed: N observed ref updates in → exactly one journal
    // entry out, naming N. A test asserting only "an event was written"
    // would stay green under the very bug this fixes (the old per-ref
    // `journal_updates` also wrote "an event", 94 of them) — see this
    // module's other doc comments for that measurement — so every assertion
    // below is about the *count* and what the surviving entry *names*, never
    // merely its existence.
    // -----------------------------------------------------------------------

    /// A repo with a real `.git` directory, since `crate::journal::state_dir`
    /// deliberately requires one (mirrors `journal.rs`'s own `repo()` test
    /// helper — duplicated rather than shared because the two `#[cfg(test)]`
    /// modules don't share a `dev-dependencies`-only test-support crate).
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git runs")
            .success());
        dir
    }

    fn update(short_name: &str, old: Option<&str>, new: &str) -> RemoteRefUpdate {
        RemoteRefUpdate {
            ref_name: format!("refs/remotes/origin/{short_name}"),
            old_oid: old.map(str::to_string),
            new_oid: Some(new.to_string()),
        }
    }

    /// The headline property #329 asks for: a fetch that moved N refs
    /// journals **exactly one** `Fetch` event, and that event's payload
    /// *names* N — not merely "an event exists". Mutating the mechanism this
    /// protects (e.g. reverting `journal_updates` to loop-and-append one
    /// entry per ref) must turn this red; asserting only `len() >= 1` or
    /// only that a `Fetch`-kind entry is present would not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fetch_that_moves_many_refs_journals_one_event_naming_the_count() {
        let dir = repo();
        let remote = RemoteName::new("origin").expect("valid remote name");
        // 94 — the exact count measured in the bug report, not a round
        // number, so this test fails the same way the field report did if
        // the fix regresses.
        let updated: Vec<RemoteRefUpdate> = (0..94)
            .map(|i| update(&format!("b{i}"), None, &format!("{i:040x}")))
            .collect();

        journal_updates(dir.path(), &remote, &updated, "fetched").await;

        let entries = journal::read_all(dir.path());
        let fetches: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == ActivityKind::Fetch)
            .collect();
        assert_eq!(
            fetches.len(),
            1,
            "94 moved refs must journal exactly one Fetch entry, not one per \
             ref: {fetches:?}"
        );
        let entry = fetches[0];
        assert!(
            entry.summary.contains("94"),
            "the entry must name how many refs moved, at the same \
             granularity `exec_fetch`'s own response message uses: {}",
            entry.summary
        );
        assert!(
            entry.summary.contains(remote.as_str()),
            "the entry must still say which remote: {}",
            entry.summary
        );
        // No single ref_name/oid pair could honestly describe a 94-ref
        // aggregate — see `journal_updates`'s doc comment for why this is
        // `Obs::Absent`, not a fabricated "the first ref" or "the last ref".
        assert_eq!(entry.ref_name, None, "{entry:?}");
        assert_eq!(entry.old_oid, None, "{entry:?}");
        assert_eq!(entry.new_oid, None, "{entry:?}");
        assert_eq!(entry.source, ActivitySource::App, "{entry:?}");
    }

    /// The singular case still reads naturally ("1 remote-tracking ref", not
    /// "1 remote-tracking refs") and still journals exactly one entry — the
    /// aggregate isn't a special case that only kicks in above some count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fetch_that_moves_one_ref_journals_one_event_without_pluralizing() {
        let dir = repo();
        let remote = RemoteName::new("origin").expect("valid remote name");
        let updated = vec![update("main", Some(&"0".repeat(40)), &"a".repeat(40))];

        journal_updates(dir.path(), &remote, &updated, "fetched").await;

        let entries = journal::read_all(dir.path());
        let fetches: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == ActivityKind::Fetch)
            .collect();
        assert_eq!(fetches.len(), 1, "{fetches:?}");
        assert!(
            fetches[0].summary.contains("1 remote-tracking ref "),
            "singular ref count must not pluralize: {}",
            fetches[0].summary
        );
    }

    /// The paired negative, at this same direct-call layer: nothing moved →
    /// nothing journaled. Without this, a `journal_updates` that always wrote
    /// an entry (even an empty-summary one) would still pass the two tests
    /// above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fetch_that_moves_nothing_journals_nothing() {
        let dir = repo();
        let remote = RemoteName::new("origin").expect("valid remote name");

        journal_updates(dir.path(), &remote, &[], "fetched").await;

        assert!(
            journal::read_all(dir.path()).is_empty(),
            "an up-to-date fetch must leave no trace in the journal"
        );
    }
}
