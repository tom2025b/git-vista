//! `POST /api/fetch` (M2.20c, #229): fetch from a configured remote.
//!
//! The handler is deliberately thin, like every other write handler since
//! M1.06b (#143): validate the request shape, build one typed
//! [`GitOperation::FetchRemote`], hand it to the planner. Everything that
//! makes a fetch different from a branch delete — live progress, cancellation,
//! failure classification — lives in `planner::fetch`, behind the same
//! build → validate → execute pipeline, so a fetch cannot skip the staleness
//! gate or the per-repository mutation guard by virtue of being slow.
//!
//! # Why a remote *name* and not a URL
//!
//! [`FetchRequest`] carries a name. A request that could carry a URL would let
//! any authenticated client point this server — and whatever credential
//! helper or SSH agent the host offers it — at a host of the client's
//! choosing. That is the same class of hazard as a request naming a
//! repository path, which `docs/adr/0002-versioned-api-contract.md` already
//! refuses for the same reason.
//!
//! **Two separate things enforce it, and both are needed** (ADR 0047 — the
//! original version of this comment claimed the second one alone did, and it
//! did not, which is the hole `planner::remote_boundary_suite` was written
//! against):
//!
//! 1. [`RemoteName`]'s validator refuses every URL and path shape, so a
//!    URL-shaped value cannot be constructed from wire input at all. This one
//!    is type-level and reaches every consumer of the type, including pull,
//!    push and the tag operations.
//! 2. The plan's `Precondition::RemoteConfigured` requires the name to exist
//!    in the repository's own configuration, and `planner::enforce_fresh`
//!    now **refuses** when it does not hold rather than skipping it (see
//!    `planner::refuses_when_unmet_at_build`). This one catches what no
//!    string rule can: a perfectly well-formed name the repository has simply
//!    never configured, which `git fetch` resolves as a *path* relative to
//!    the worktree.

use axum::http::StatusCode;
use axum::Json;

use git_vista_protocol::{FetchFailureKind, FetchRequest, GitOperation, RemoteName};

use crate::planner;
use crate::state::reject_if_read_only;

/// `POST /api/fetch` — `git fetch --progress <remote>` via
/// [`GitOperation::FetchRemote`].
///
/// Every refusal this handler makes itself is a [`FetchError`] body, the same
/// contract `/api/amend-commit` makes: a client can parse a 400 from this
/// route as that one type without inspecting which layer produced it.
///
/// [`FetchError`]: git_vista_protocol::FetchError
pub(crate) async fn fetch_remote(Json(req): Json<FetchRequest>) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let remote = match validate_remote(&req.remote) {
        Ok(remote) => remote,
        Err(refused) => return refused,
    };
    planner::plan_and_execute(GitOperation::FetchRemote { remote }).await
}

/// The request-shape gate, split from the handler so it is testable without
/// the process-global selection `reject_if_read_only` reads (`state::CURRENT`
/// is set once per process and owned by `state`'s own test).
///
/// The same two checks every other name-taking endpoint applies — non-empty,
/// not option-shaped — before [`RemoteName`]'s own validation. The
/// option-shaped check matters more here than usual: this name becomes an
/// argv element in `git fetch --progress <remote>`, and a leading `-` would
/// be read by git as a flag.
///
/// The trim is why this exists at all rather than calling [`RemoteName::new`]
/// directly: `RemoteName` refuses whitespace (a name is a token, and a stored
/// `" origin "` would never match the config), so a request whose field a UI
/// padded has to be trimmed *before* the type sees it.
pub(crate) fn validate_remote(raw: &str) -> Result<RemoteName, (StatusCode, String)> {
    let remote = raw.trim();
    if remote.is_empty() {
        return Err(refusal("Remote name can't be empty."));
    }
    if remote.starts_with('-') {
        return Err(refusal("Remote name can't start with '-'."));
    }
    // Unreachable after the two checks above; kept total rather than panic.
    RemoteName::new(remote).map_err(|e| refusal(&e.to_string()))
}

/// A request-shape refusal in this endpoint's own error contract.
fn refusal(message: &str) -> (StatusCode, String) {
    eprintln!("git-vista: /api/fetch refused: {message}");
    (
        StatusCode::BAD_REQUEST,
        planner::fetch_error_body(FetchFailureKind::Other, message.to_string(), Vec::new()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::FetchError;

    /// Every refusal the shape gate makes is parseable as the endpoint's one
    /// error type — the property a client depends on, asserted by parsing
    /// rather than by string-matching the body.
    #[test]
    fn a_malformed_remote_is_refused_as_the_endpoints_error_type() {
        for bad in ["", "   ", "-force-looking", "--upload-pack=/bin/sh"] {
            let (status, body) = validate_remote(bad).expect_err("must be refused");
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {bad:?}");
            let parsed: FetchError =
                serde_json::from_str(&body).expect("every /api/fetch refusal is a FetchError");
            assert_eq!(parsed.kind, FetchFailureKind::Other);
            assert!(
                parsed.updated_refs.is_empty(),
                "a request refused before planning cannot have moved a ref"
            );
        }
    }

    /// The paired positive: ordinary remote names pass, and pass *as
    /// themselves* (trimmed, not rewritten). Without this, a gate that
    /// refused every input would satisfy the test above while accepting
    /// nothing.
    #[test]
    fn an_ordinary_remote_name_passes_the_shape_gate_unchanged() {
        for good in ["origin", "  origin  ", "upstream", "fork2"] {
            let remote = validate_remote(good).expect("an ordinary remote name must pass");
            assert_eq!(remote.as_str(), good.trim());
        }
    }
}
