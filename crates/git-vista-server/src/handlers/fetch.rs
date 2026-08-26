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

    /// **The refusal at the wire** (#336): through a real `Router`, the real
    /// `api_contract` middleware and the real handler, a `/api/fetch` refusal
    /// reaches the client as a **bare** [`FetchError`] — not dug out of an
    /// `ApiError` envelope's `message` field.
    ///
    /// The test above proves what `validate_remote` *returns*; nothing proved
    /// what a client *receives*, and those are different questions. This route
    /// hand-serializes its typed DTO into the shared `(StatusCode, String)`
    /// channel, which axum stamps `text/plain`, so the only thing that keeps
    /// the DTO intact on the wire is `middleware::rewrap_error`'s byte sniff
    /// (#323) — one general mechanism with no route-local layer behind it
    /// (`/api/amend-commit` had one until #336; ADR 0084). Remove or narrow
    /// that sniff and this goes red at `bare()` below, which is the point: the
    /// incidental coverage is now deliberate.
    ///
    /// The second leg is what stops the first from being satisfiable by a
    /// middleware that labeled *everything* JSON: the read-only refusal on the
    /// same route is prose, and must still arrive as a proper envelope.
    #[tokio::test]
    async fn a_refusal_reaches_the_client_as_a_bare_fetch_error_through_a_real_router() {
        crate::state::with_isolated_test_current(async {
            use axum::body::{to_bytes, Body};
            use axum::http::{header, Request};
            use axum::routing::post;
            use axum::Router;
            use git_vista_protocol::{ApiError, RepoMode, PROTOCOL_HEADER, PROTOCOL_VERSION};
            use tower::ServiceExt;

            let router = Router::new()
                .route("/api/fetch", post(fetch_remote))
                .layer(axum::middleware::from_fn(crate::middleware::api_contract));

            async fn post_body(
                router: &Router,
                body: &'static str,
            ) -> (StatusCode, String, String) {
                let resp = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/fetch")
                            .header(header::CONTENT_TYPE, "application/json")
                            .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = resp.status();
                let content_type = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
                (
                    status,
                    content_type,
                    String::from_utf8_lossy(&bytes).into_owned(),
                )
            }

            /// The endpoint's own error body — parsed directly, **not** dug out
            /// of an `ApiError` envelope.
            fn bare(raw: &str) -> FetchError {
                serde_json::from_str(raw)
                    .unwrap_or_else(|e| panic!("body was not a bare FetchError: {e}\nbody={raw}"))
            }

            // Leg 1: the shape gate's typed refusal. `Active` so the read-only
            // gate (which runs first on this route) lets the request through to
            // it; the path need not be a real repository, because a request
            // refused here never reaches the planner.
            let dir = tempfile::tempdir().unwrap();
            crate::state::set_current(dir.path(), RepoMode::Active);
            let (status, content_type, raw) =
                post_body(&router, r#"{"remote":"--upload-pack=/bin/sh"}"#).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
            assert!(
                content_type.starts_with("application/json"),
                "a typed refusal must reach the client labeled JSON: {content_type:?}"
            );
            let error = bare(&raw);
            assert_eq!(error.kind, FetchFailureKind::Other, "{raw}");
            assert!(
                error.updated_refs.is_empty(),
                "a request refused before planning cannot have moved a ref"
            );
            assert!(
                !raw.contains("\\\"kind\\\""),
                "the DTO was escaped into an outer envelope's string field — \
                 double-encoded: {raw}"
            );

            // Leg 2: the paired negative. The read-only refusal on this same
            // route is prose, and must arrive as a proper `ApiError` envelope —
            // so leg 1 cannot be satisfied by labeling every body JSON.
            crate::state::set_current(dir.path(), RepoMode::Visualize);
            let (status, _, raw) = post_body(&router, r#"{"remote":"origin"}"#).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
            let enveloped: ApiError = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("a prose refusal must still be enveloped ({e}): {raw}"));
            assert!(
                enveloped.error.message.contains("Visualize"),
                "the envelope must carry what the server said: {}",
                enveloped.error.message
            );
        })
        .await;
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
