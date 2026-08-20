//! The stash drawer's HTTP surface (M3.24, #77).
//!
//! Four endpoints: one read and three writes.
//!
//! | endpoint | operation |
//! |---|---|
//! | `GET  /api/stashes` | list the drawer — unconditionally safe |
//! | `POST /api/stash/push` | [`GitOperation::PushStash`] |
//! | `POST /api/stash/apply` | [`GitOperation::ApplyStash`] |
//! | `POST /api/stash/drop` | [`GitOperation::DropStash`] |
//!
//! **There is no `/api/stash/pop`.** Pop is apply-then-drop, and a single
//! operation row cannot tell the truth about the half-done state: apply
//! succeeds, drop fails, and the record says only `Failed` — indistinguishable
//! from "nothing happened" while the user's changes are actually in the tree.
//! Two independent operations produce two rows, and two rows can say "applied,
//! then the drop failed". See `GitOperation`'s comment on the absent
//! `PopStash` in `plan.rs`.
//!
//! # The selector/oid split, restated here because this is where clients meet it
//!
//! Every write takes `entry` (a positional `stash@{n}`) **and** `expected_oid`.
//! The selector is the address and is what reaches git; the oid is the witness
//! and is compare-and-swapped against a fresh resolve immediately before the
//! mutation runs. A client that sends only one of them cannot be served: an oid
//! alone is not a valid argument to `git stash drop`, and a selector alone
//! renumbers on every drop, so acting on it would eventually delete a stash
//! nobody chose.

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use git_vista_protocol::{CommitOid, GitOperation, StashMessage, StashSelector};

use crate::planner;
use crate::state::reject_if_read_only;

/// `GET /api/stash/show?entry=stash@{N}` — the patch a stash entry holds
/// (M3.24 #77).
///
/// # The criterion: "stash content is inspectable before apply or drop"
///
/// Before this, the only way to learn what an entry contained was to apply it
/// and look — which is exactly the thing a user wants to avoid deciding
/// blindly, and it is irreversible in the drop case. A stash you cannot read
/// is a stash you cannot safely discard.
///
/// # A read, and only a read
///
/// `git stash show -p` resolves the entry and prints a diff. It writes
/// nothing, touches no index and no worktree, so this needs no plan and no
/// `GitOperation` — the same posture every other diff read in this server
/// takes.
///
/// The flag set matters and is the same one every diff read here uses:
/// `--no-color` so a `color.ui = always` config cannot inject escapes into
/// text rendered as-is, and `--no-textconv` because a repository's own
/// `.gitattributes` can bind a textconv filter that git would then *execute*
/// to render content.
pub(crate) async fn show_stash(
    axum::extract::Query(q): axum::extract::Query<ShowStashQuery>,
) -> (StatusCode, String) {
    let (repo, _read_only) = crate::state::current();

    let Ok(entry) = git_vista_protocol::StashSelector::new(&q.entry) else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "{} is not a stash selector — expected the stash@{{N}} form the \
                 stash list returns.",
                q.entry
            ),
        );
    };

    // `--` is not applicable here (the argument is a revision, not a path),
    // but the selector newtype has already refused anything that is not
    // `stash@{N}`, so no argument can be read as an option.
    let out = match crate::git_cmd::git_output(
        &repo,
        &[
            "stash",
            "show",
            "--patch",
            "--no-color",
            "--no-textconv",
            entry.as_str(),
        ],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't run git stash show: {e}"),
            )
        }
    };

    if !out.status.success() {
        // Most often a selector that no longer resolves, because every drop
        // renumbers the list. Say that, rather than passing git's wording
        // through and leaving the user to infer it.
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return (
            StatusCode::NOT_FOUND,
            format!(
                "{} could not be read — {msg}\n\nStash entries renumber on every \
                 drop, so a selector held from an earlier listing may now point \
                 somewhere else or nowhere. Re-read the list and try again.",
                entry
            ),
        );
    }

    (
        StatusCode::OK,
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Query for [`show_stash`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShowStashQuery {
    /// The `stash@{N}` selector, exactly as the list returned it.
    pub(crate) entry: String,
}

/// `GET /api/stashes` — the drawer, newest first.
///
/// A read, so it is not `full_routes`-gated and the LAN router sees it. An app
/// that can *show* the stash list is useful before any write path exists, which
/// is why the read shipped first.
pub(crate) async fn stash_list() -> (StatusCode, String) {
    let (repo, _read_only) = crate::state::current();
    match git_vista_git::stash::read_stashes(&repo) {
        Ok(entries) => {
            let body: Vec<serde_json::Value> = entries
                .iter()
                .map(|s| {
                    serde_json::json!({
                        // The selector a client must send back to act on this
                        // entry — built here rather than in the client, so
                        // the wire form has exactly one author.
                        "entry": format!("stash@{{{}}}", s.index),
                        "index": s.index,
                        "oid": s.oid.0,
                        "message": s.message,
                        "time": s.time,
                    })
                })
                .collect();
            match serde_json::to_string(&body) {
                Ok(json) => (StatusCode::OK, json),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not serialise the stash list: {e}"),
                ),
            }
        }
        // An unreadable drawer is an error, never an empty list. "No stashes"
        // and "could not look" authorise different things in the UI, and the
        // git crate already keeps them apart — this must not re-merge them.
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read the stash list: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub(crate) struct PushStashRequest {
    #[serde(default)]
    pub message: Option<String>,
    /// REQUIRED, no default. The acceptance criterion is that staged and
    /// untracked handling is *explicit*; a bool with a default is how a UI
    /// quietly stops asking.
    pub keep_index: bool,
    pub include_untracked: bool,
}

/// `POST /api/stash/push` — put the working tree in the drawer.
pub(crate) async fn push_stash(Json(req): Json<PushStashRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    // An absent message is fine (git writes its own "WIP on <branch>"); a
    // present-but-blank one is a client bug worth naming rather than silently
    // dropping, since the user typed something and it went nowhere.
    let message = match req.message.as_deref().map(str::trim) {
        None | Some("") if req.message.is_none() => None,
        Some("") => {
            return (
                StatusCode::BAD_REQUEST,
                "Stash message can't be blank — omit it entirely to let git write its own."
                    .to_string(),
            );
        }
        Some(m) => match StashMessage::new(m) {
            Ok(msg) => Some(msg),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
        },
        None => None,
    };
    planner::plan_and_execute(GitOperation::PushStash {
        message,
        keep_index: req.keep_index,
        include_untracked: req.include_untracked,
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct StashEntryRequest {
    /// Positional selector, `stash@{0}`. Validated to exactly that shape.
    pub entry: String,
    /// The oid the client believes that selector names. Compare-and-swapped
    /// server-side before the mutation runs.
    pub expected_oid: String,
}

/// Validate the pair both write endpoints share.
///
/// Both fields are required. A request carrying only one is refused rather
/// than half-honoured: the selector alone renumbers on every drop, and the oid
/// alone is not something `git stash drop` accepts.
fn parse_entry(
    req: &StashEntryRequest,
) -> Result<(StashSelector, CommitOid), (StatusCode, String)> {
    let entry = StashSelector::new(req.entry.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let oid = CommitOid::new(req.expected_oid.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok((entry, oid))
}

/// `POST /api/stash/apply` — restore a stash's changes, keeping the entry.
///
/// Gated on `CleanWorktree` by the plan builder. That is the load-bearing
/// decision of this slice: with a clean tree the abort path is `reset --hard`
/// plus `clean -fd`, and that is provably safe because there is nothing of the
/// user's to destroy.
pub(crate) async fn apply_stash(Json(req): Json<StashEntryRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let (entry, expected_oid) = match parse_entry(&req) {
        Ok(pair) => pair,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::ApplyStash {
        entry,
        expected_oid,
    })
    .await
}

/// `POST /api/stash/branch` (M3.24 #77) — the escape hatch for a stash that
/// will not apply where you are now.
///
/// Carries the branch name alongside the usual selector/oid pair. The name is
/// validated by [`BranchName`]'s newtype before a plan exists, so a malformed
/// name is refused without anything being consumed.
pub(crate) async fn branch_from_stash(
    Json(req): Json<BranchFromStashRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    // Reuses the shared selector/oid parse so this path cannot drift from
    // apply and drop on what a valid entry looks like.
    let (entry, expected_oid) = match parse_entry(&StashEntryRequest {
        entry: req.entry,
        expected_oid: req.expected_oid,
    }) {
        Ok(pair) => pair,
        Err(refusal) => return refusal,
    };
    let Ok(name) = git_vista_protocol::BranchName::new(&req.name) else {
        return (
            StatusCode::BAD_REQUEST,
            format!("{} is not a usable branch name.", req.name),
        );
    };
    crate::planner::plan_and_execute(GitOperation::BranchFromStash {
        name,
        entry,
        expected_oid,
    })
    .await
}

/// Body of [`branch_from_stash`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchFromStashRequest {
    pub(crate) name: String,
    pub(crate) entry: String,
    pub(crate) expected_oid: String,
}

/// `POST /api/stash/drop` — discard an entry.
///
/// `Destructive`, and the compare-and-swap in the executor is what stands
/// between this and dropping a stash the user never chose: every drop
/// renumbers the list, so a selector planned seconds ago may now address
/// someone else's work.
pub(crate) async fn drop_stash(Json(req): Json<StashEntryRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let (entry, expected_oid) = match parse_entry(&req) {
        Ok(pair) => pair,
        Err(rejected) => return rejected,
    };
    planner::plan_and_execute(GitOperation::DropStash {
        entry,
        expected_oid,
    })
    .await
}
