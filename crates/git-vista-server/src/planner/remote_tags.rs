//! M2.21f (#240): executing [`GitOperation::PushTag`] and
//! [`GitOperation::DeleteRemoteTag`] — the two tag operations classified in
//! M2.21a (#235) but left `501` until now.
//!
//! Both run through the exact chokepoint [`super::push::exec_push`] uses:
//! [`crate::git_cmd::git_streamed_for`], the same askpass hardening,
//! credential redaction, cancellation latch and streamed progress. The one
//! real difference from a branch push is what there is to observe
//! afterwards: a tag has no local remote-tracking ref (tags fetch straight
//! into `refs/tags/`, never `refs/remotes/<remote>/tags/*`), so there is no
//! local ref to diff before/after the way [`super::push`] diffs
//! `refs/remotes/<remote>/<branch>`. `shape()`'s D5 reasoning
//! (planner.rs, the `DeleteRemoteTag`/`PushTag` arms) already says so for the
//! *plan*'s `expected_ref_changes`; this module is the execution side of the
//! same fact, so neither executor below diffs anything local — each reports
//! what git's own exit status and stderr say, and nothing more.
//!
//! Argv is exactly plan.rs's documented contract (its command table, and
//! each variant's own doc comment): `push_tag_argv` builds
//! `git push --progress <remote> refs/tags/<name>`,
//! `delete_remote_tag_argv` builds
//! `git push --progress <remote> --delete refs/tags/<name>`. Neither can
//! carry `--force` or `--tags` — there is no field in either operation that
//! could ask for one, so there is no branch of either argv builder that could
//! emit one either; `no_tag_remote_argv_can_carry_a_bare_force_or_a_bare_dash_dash_tags`
//! pins that over the whole input space, the tag-shaped twin of
//! `push::no_push_argv_can_carry_a_bare_force`.

use axum::http::StatusCode;

use git_vista_protocol::{plan_export, RemoteName, TagName};

use super::transfer::parse_progress;
use super::*;

/// `POST /api/push-tag`'s log-line name.
const PUSH_ENDPOINT: &str = "/api/push-tag";

/// `POST /api/delete-remote-tag`'s log-line name.
const DELETE_ENDPOINT: &str = "/api/delete-remote-tag";

// ---------------------------------------------------------------------------
// The argv
// ---------------------------------------------------------------------------
//
// `push_tag_argv` and `delete_remote_tag_argv` moved to
// `git_vista_protocol::plan_export` with M10 (#590), unchanged — including the
// full `refs/tags/<name>` path that stops git's refspec matching from choosing
// a same-named branch on the remote. Re-exported under the names this module's
// own tests already use.
// The suite below still names them bare; production calls them qualified, so
// the export's source scan can see which shared builder this module uses.
#[cfg(test)]
use git_vista_protocol::plan_export::{delete_remote_tag_argv, push_tag_argv};

// ---------------------------------------------------------------------------
// Failure classification — shared by both executors
// ---------------------------------------------------------------------------

/// Why `git push` (tag-shaped) came back non-zero. Server-internal, same
/// posture as [`super::push::PushFailure`] and for the same reason: these
/// endpoints answer `text/plain`, so the tag adds a sentence and never
/// replaces git's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagRemoteFailure {
    /// `PushTag` only: the remote already has a tag of this name pointing
    /// somewhere else. Tag refs are never fast-forwarded and `PushTag` has no
    /// field that could ask git to force through it (see its doc in
    /// plan.rs) — there is no lease here the way a branch push has one.
    AlreadyExists,
    AuthenticationFailed,
    RemoteUnreachable,
    RemoteRejected,
    Other,
}

impl TagRemoteFailure {
    fn status(self) -> StatusCode {
        match self {
            TagRemoteFailure::AlreadyExists => StatusCode::CONFLICT,
            TagRemoteFailure::AuthenticationFailed
            | TagRemoteFailure::RemoteUnreachable
            | TagRemoteFailure::RemoteRejected
            | TagRemoteFailure::Other => StatusCode::BAD_REQUEST,
        }
    }

    fn hint(self) -> Option<&'static str> {
        match self {
            TagRemoteFailure::AlreadyExists => Some(
                "The remote already has a tag of this name pointing somewhere else. \
                 git-vista never force-publishes a tag — delete the remote tag first \
                 if it should be replaced.",
            ),
            TagRemoteFailure::AuthenticationFailed => Some(
                "The remote refused this server's credentials. git-vista never \
                 prompts for a password — configure a credential helper or an SSH \
                 agent on the host.",
            ),
            TagRemoteFailure::RemoteUnreachable => Some("The remote could not be reached."),
            TagRemoteFailure::RemoteRejected | TagRemoteFailure::Other => None,
        }
    }
}

/// Classify a failed tag push/delete from git's stderr. Same discipline as
/// [`super::push::classify_failure`]: stderr only, gettext-translated and
/// version-dependent, so a documented marker set with an `Other` fallback,
/// and git's own words forwarded verbatim regardless of the classification.
///
/// Markers verified against git 2.43.0's real output (captured against a
/// live bare remote, not retyped from memory): a re-push of an unchanged tag
/// is `Everything up-to-date` (exit 0, not a failure); a re-push of a tag
/// that now points elsewhere is
/// `! [rejected] v1.0.0 -> v1.0.0 (already exists)`; deleting an already-gone
/// remote tag is exit 0 with a `warning: deleting a non-existent ref` — git
/// does not refuse it, so `AlreadyExists`' delete-side twin does not exist as
/// a failure case at all.
fn classify_tag_remote_failure(stderr: &str) -> TagRemoteFailure {
    let s = stderr.to_ascii_lowercase();

    if s.contains("already exists") {
        return TagRemoteFailure::AlreadyExists;
    }
    for marker in [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied (publickey",
        "invalid username or password",
    ] {
        if s.contains(marker) {
            return TagRemoteFailure::AuthenticationFailed;
        }
    }
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
            return TagRemoteFailure::RemoteUnreachable;
        }
    }
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
            return TagRemoteFailure::RemoteRejected;
        }
    }
    TagRemoteFailure::Other
}

// ---------------------------------------------------------------------------
// The executors
// ---------------------------------------------------------------------------

/// `git push --progress <remote> refs/tags/<name>` (`POST /api/push-tag`).
///
/// The cancel latch is read before the spawn (a cancel that lands while
/// queued behind the repository guard must not then publish anyway) and
/// handed into [`crate::git_cmd::git_streamed_for`], so a cancel mid-transfer
/// SIGKILLs the child exactly as it does for a branch push. That is *not* the
/// same as this operation advertising cancellation to an operator:
/// [`super::honours_cancellation`]'s pinned census stays at the three
/// object-transfer operations it names today, deliberately — widening it is
/// its own edit, with its own test update, not a side effect of this one.
pub(super) async fn exec_push_tag(
    repo: &Path,
    need: NetworkNeed,
    name: &TagName,
    remote: &RemoteName,
) -> (StatusCode, String) {
    debug_assert_eq!(
        need,
        NetworkNeed::Remote,
        "a tag push reaches a remote; if it arrives here declared Local, \
         `network_need_for_operation` is wrong and the sandbox will \
         (correctly) deny the connect"
    );

    let cancel = crate::operations::cancel_signal();
    if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
        return (
            StatusCode::CONFLICT,
            format!(
                "The push of tag ‘{name}’ to ‘{remote}’ was cancelled before it \
                 started — nothing was sent."
            ),
        );
    }

    let argv = plan_export::push_tag_argv(name, remote);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
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
        Err(e) => return couldnt_run(PUSH_ENDPOINT, &e),
    };

    if run.cancelled {
        let message = format!(
            "The push of tag ‘{name}’ to ‘{remote}’ was cancelled. A push killed \
             mid-flight can still have reached the remote before the signal did — \
             fetch and check the remote's tags to be sure."
        );
        journal_app_event(
            repo,
            ActivityKind::Push,
            Some(format!("refs/tags/{name}")),
            Obs::Unknown,
            Obs::Unknown,
            message.clone(),
        )
        .await;
        eprintln!("git-vista: {PUSH_ENDPOINT} cancelled: {message}");
        return (StatusCode::CONFLICT, message);
    }

    let stderr = String::from_utf8_lossy(&run.output.stderr).into_owned();
    if !run.output.status.success() {
        let kind = classify_tag_remote_failure(&stderr);
        let git_said = stderr_stdout_or(&run.output, "git push failed.");
        let message = match kind.hint() {
            Some(hint) => format!("{git_said}\n\n{hint}"),
            None => git_said,
        };
        eprintln!("git-vista: {PUSH_ENDPOINT} failed ({kind:?}): {message}");
        journal_app_event(
            repo,
            ActivityKind::Push,
            Some(format!("refs/tags/{name}")),
            Obs::Unknown,
            Obs::Unknown,
            format!("failed to push tag ‘{name}’ to ‘{remote}’: {message}"),
        )
        .await;
        return (kind.status(), message);
    }

    // The tag's own oid, read back rather than assumed — a successful push
    // never moves the local ref, but reading it (instead of reusing whatever
    // the plan observed) is what makes this a fact about the repository right
    // now rather than about the moment the plan was built.
    let new = Obs::from_read(rev_parse_ref_unpeeled(repo, &format!("refs/tags/{name}")).await);
    journal_app_event(
        repo,
        ActivityKind::Push,
        Some(format!("refs/tags/{name}")),
        Obs::Unknown,
        new,
        format!("pushed tag ‘{name}’ to ‘{remote}’"),
    )
    .await;
    let message = format!("Pushed tag ‘{name}’ to ‘{remote}’.");
    println!("[{PUSH_ENDPOINT}] {message}");
    (StatusCode::OK, message)
}

/// `git push --progress <remote> --delete refs/tags/<name>`
/// (`POST /api/delete-remote-tag`). Same shape as [`exec_push_tag`]; see its
/// doc for the cancel-latch and no-local-ref-to-diff reasoning, both of which
/// apply here unchanged.
pub(super) async fn exec_delete_remote_tag(
    repo: &Path,
    need: NetworkNeed,
    name: &TagName,
    remote: &RemoteName,
) -> (StatusCode, String) {
    debug_assert_eq!(
        need,
        NetworkNeed::Remote,
        "a remote tag delete reaches a remote; if it arrives here declared \
         Local, `network_need_for_operation` is wrong and the sandbox will \
         (correctly) deny the connect"
    );

    let cancel = crate::operations::cancel_signal();
    if cancel.as_ref().is_some_and(|rx| *rx.borrow()) {
        return (
            StatusCode::CONFLICT,
            format!(
                "The delete of tag ‘{name}’ on ‘{remote}’ was cancelled before it \
                 started — nothing was sent."
            ),
        );
    }

    let argv = plan_export::delete_remote_tag_argv(name, remote);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
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
        Err(e) => return couldnt_run(DELETE_ENDPOINT, &e),
    };

    if run.cancelled {
        let message = format!(
            "The delete of tag ‘{name}’ on ‘{remote}’ was cancelled. A delete \
             killed mid-flight can still have reached the remote before the \
             signal did — fetch and check the remote's tags to be sure."
        );
        journal_app_event(
            repo,
            ActivityKind::Other,
            Some(format!("refs/tags/{name}")),
            Obs::Unknown,
            Obs::Unknown,
            message.clone(),
        )
        .await;
        eprintln!("git-vista: {DELETE_ENDPOINT} cancelled: {message}");
        return (StatusCode::CONFLICT, message);
    }

    let stderr = String::from_utf8_lossy(&run.output.stderr).into_owned();
    if !run.output.status.success() {
        let kind = classify_tag_remote_failure(&stderr);
        let git_said = stderr_stdout_or(&run.output, "git push --delete failed.");
        let message = match kind.hint() {
            Some(hint) => format!("{git_said}\n\n{hint}"),
            None => git_said,
        };
        eprintln!("git-vista: {DELETE_ENDPOINT} failed ({kind:?}): {message}");
        journal_app_event(
            repo,
            ActivityKind::Other,
            Some(format!("refs/tags/{name}")),
            Obs::Unknown,
            Obs::Unknown,
            format!("failed to delete tag ‘{name}’ on ‘{remote}’: {message}"),
        )
        .await;
        return (kind.status(), message);
    }

    // Deleting an already-gone remote tag is not a failure git reports (exit
    // 0, a stderr warning) — see `classify_tag_remote_failure`'s doc — so
    // "the remote no longer has it" is reported uniformly on every success
    // rather than branching this message on stderr content nothing here
    // tests.
    journal_app_event(
        repo,
        ActivityKind::Other,
        Some(format!("refs/tags/{name}")),
        Obs::Unknown,
        Obs::Absent,
        format!("deleted tag ‘{name}’ on ‘{remote}’"),
    )
    .await;
    let message = format!("Deleted tag ‘{name}’ on ‘{remote}’.");
    println!("[{DELETE_ENDPOINT}] {message}");
    (StatusCode::OK, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str) -> TagName {
        TagName::new(name).unwrap()
    }

    fn remote(name: &str) -> RemoteName {
        RemoteName::new(name).unwrap()
    }

    /// **The invariant this slice exists to keep, tag-shaped**: neither argv
    /// this server can build for a remote tag operation carries a bare force
    /// of any spelling, and neither ever asks git to publish every local tag
    /// — `PushTag` names exactly the one tag it carries (see its doc in
    /// plan.rs), never `--tags`.
    #[test]
    fn no_tag_remote_argv_can_carry_a_bare_force_or_a_bare_dash_dash_tags() {
        for (n, r) in [("v1.0.0", "origin"), ("release/2026-08", "upstream")] {
            for argv in [
                push_tag_argv(&tag(n), &remote(r)),
                delete_remote_tag_argv(&tag(n), &remote(r)),
            ] {
                for arg in &argv {
                    assert_ne!(arg, "--force", "bare force in {argv:?}");
                    assert_ne!(arg, "-f", "bare force in {argv:?}");
                    assert_ne!(
                        arg, "--tags",
                        "must never publish every local tag: {argv:?}"
                    );
                    assert!(
                        !arg.starts_with("--force"),
                        "no force flag of any shape belongs in a tag-remote argv: {argv:?}"
                    );
                }
                assert_eq!(argv[0], "push", "{argv:?}");
            }
        }
    }

    /// The full `refs/tags/` path names every argv, `--progress` asks git for
    /// progress on both, and only the delete argv carries `--delete`.
    #[test]
    fn both_argvs_name_the_full_refs_tags_path_and_ask_for_progress() {
        let push = push_tag_argv(&tag("v1.0.0"), &remote("origin"));
        assert!(push.contains(&"refs/tags/v1.0.0".to_string()), "{push:?}");
        assert!(push.contains(&"--progress".to_string()), "{push:?}");
        assert!(!push.contains(&"--delete".to_string()), "{push:?}");

        let delete = delete_remote_tag_argv(&tag("v1.0.0"), &remote("origin"));
        assert!(
            delete.contains(&"refs/tags/v1.0.0".to_string()),
            "{delete:?}"
        );
        assert!(delete.contains(&"--progress".to_string()), "{delete:?}");
        assert!(delete.contains(&"--delete".to_string()), "{delete:?}");
    }

    /// Markers captured against real git 2.43.0 output (see
    /// `classify_tag_remote_failure`'s doc for the exact commands run).
    #[test]
    fn classification_names_the_actionable_cause() {
        for (stderr, expected) in [
            (
                " ! [rejected]        v1.0.0 -> v1.0.0 (already exists)\n\
                 error: failed to push some refs to '../remote.git'\n\
                 hint: Updates were rejected because the tag already exists in the \
                 remote.",
                TagRemoteFailure::AlreadyExists,
            ),
            (
                "fatal: Authentication failed for 'https://example.invalid/r.git/'",
                TagRemoteFailure::AuthenticationFailed,
            ),
            (
                "git@github.com: Permission denied (publickey).\r\nfatal: Could not \
                 read from remote repository.",
                TagRemoteFailure::AuthenticationFailed,
            ),
            (
                "fatal: unable to access 'https://example.invalid/r.git/': Could not \
                 resolve host: example.invalid",
                TagRemoteFailure::RemoteUnreachable,
            ),
            (
                "remote: error: hook declined to update refs/tags/v1.0.0\nTo origin",
                TagRemoteFailure::RemoteRejected,
            ),
        ] {
            assert_eq!(
                classify_tag_remote_failure(stderr),
                expected,
                "for {stderr:?}"
            );
        }
    }

    /// The load-bearing negative: an unrecognised failure lands in `Other`
    /// rather than the nearest-looking box, and `Other` adds no hint — a
    /// fabricated remedy is worse than none.
    #[test]
    fn an_unrecognised_failure_is_other_rather_than_a_guess() {
        for stderr in ["fatal: early EOF", "", "Everything up-to-date"] {
            assert_eq!(
                classify_tag_remote_failure(stderr),
                TagRemoteFailure::Other,
                "for {stderr:?}"
            );
        }
        assert_eq!(TagRemoteFailure::Other.hint(), None);
    }

    #[test]
    fn each_failure_kind_gets_the_status_its_remedy_implies() {
        for (kind, status) in [
            (TagRemoteFailure::AlreadyExists, StatusCode::CONFLICT),
            (
                TagRemoteFailure::AuthenticationFailed,
                StatusCode::BAD_REQUEST,
            ),
            (TagRemoteFailure::RemoteUnreachable, StatusCode::BAD_REQUEST),
            (TagRemoteFailure::RemoteRejected, StatusCode::BAD_REQUEST),
            (TagRemoteFailure::Other, StatusCode::BAD_REQUEST),
        ] {
            assert_eq!(kind.status(), status, "for {kind:?}");
        }
    }
}
