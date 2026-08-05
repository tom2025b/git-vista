//! `POST /api/pull` (M2.20d, #230): fetch from a configured remote and
//! integrate one of its branches, using the strategy the caller named.
//!
//! Thin like every other write handler since M1.06b (#143): validate the
//! request shape, build one typed [`GitOperation::PullBranch`], hand it to the
//! planner. The fetch/merge/rebase machinery lives in `planner::pull`.
//!
//! # This handler owns its own body parsing, and that is the endpoint
//!
//! Every other write handler takes `Json<T>` and lets axum reject a malformed
//! body. That would be wrong here for one specific reason: axum's rejection is
//! a `422` whose body is a bare sentence about serde, and the single most
//! important refusal this endpoint makes — a request that named **no
//! integration strategy** — would arrive at the client as an unparseable
//! deserialization complaint rather than as this endpoint's own error type.
//!
//! #230 exists because `git pull` picks merge-or-rebase from `pull.rebase`
//! config when nobody says, so two people can get two different histories and
//! neither reviewed which. Refusing that is the feature. A feature's refusal
//! has to be legible: a `400`, carrying [`PullFailureKind::StrategyRequired`],
//! naming both legal values.
//!
//! So the body arrives as [`Bytes`] and [`parse_request`] deserializes it.
//! That also puts the wire-shape gate **before** the read-only check, which is
//! deliberate and is what makes the mandate testable through a real router: a
//! body that names no strategy is refused identically whether or not a
//! repository happens to be open in Active mode, because it is a statement
//! about the request, not about the repository.

use axum::body::Bytes;
use axum::http::StatusCode;

use git_vista_protocol::{
    BranchName, GitOperation, MergeStrategy, PullFailureKind, PullRequest, RemoteName,
};
use serde::Deserialize;

use crate::planner;
use crate::state::reject_if_read_only;

/// `POST /api/pull` — `git fetch <remote>` then `git merge`/`git rebase`
/// `<remote>/<branch>`, via [`GitOperation::PullBranch`].
///
/// Every refusal this handler makes is a [`PullError`] body, the same contract
/// `/api/fetch` and `/api/amend-commit` make: a client can parse any non-2xx
/// from this route as that one type without inspecting which layer produced
/// it.
///
/// # Order: the whole wire gate, then the mode gate
///
/// Both request-shape checks run **before** [`reject_if_read_only`], which is
/// the reverse of `/api/fetch`'s order and is deliberate. A malformed request
/// is a statement about the request, not about the repository, so its refusal
/// must not depend on which repository happens to be open — and a Visualize
/// session that sends one learns nothing about the repository from being told
/// its remote name is empty.
///
/// What that buys is testability of the thing #230 exists for: the mandate can
/// be driven through a real router with no process-global selection set, so
/// `the_strategy_mandate_is_a_400_through_a_real_router` proves the `400` at
/// the HTTP layer rather than by calling a helper. Nothing is weakened —
/// [`crate::planner::plan_and_execute`] applies the same gate again before any
/// operation executes, and `contract_suite` pins that it does.
///
/// [`PullError`]: git_vista_protocol::PullError
pub(crate) async fn pull_branch(body: Bytes) -> (StatusCode, String) {
    let req = match parse_request(&body) {
        Ok(req) => req,
        Err(refused) => return refused,
    };
    let (remote, branch) = match validate_names(&req) {
        Ok(pair) => pair,
        Err(refused) => return refused,
    };
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    planner::plan_and_execute(GitOperation::PullBranch {
        remote,
        branch,
        strategy: req.strategy,
    })
    .await
}

/// The same body with `strategy` made optional — used **only** to tell one
/// refusal from another after [`PullRequest`] has already refused the bytes.
///
/// Why a probe rather than matching on serde's error text: "did the client
/// omit the strategy?" has to be answered *structurally*, or the endpoint's
/// headline refusal would rest on a string comparison against a library's
/// prose. Re-deserializing with the one field relaxed answers it exactly —
/// if the body parses now and the field is `None`, the one thing wrong with
/// the request is the thing #230 exists to require.
///
/// `deny_unknown_fields` is kept, so `{"remote":…,"branch":…,"strategy":"auto"}`
/// and a body with a stray key both fail the probe too and fall through to the
/// generic refusal — the probe can only ever *narrow* a refusal, never widen
/// what is accepted. Nothing constructs a [`GitOperation`] from it.
///
/// An explicit `"strategy": null` reads as `None` here and therefore gets the
/// [`PullFailureKind::StrategyRequired`] refusal rather than the generic one.
/// That is the intended reading: `null` is JSON for "no value", the client did
/// not choose, and the actionable message is the one that names the two legal
/// values. `PullRequest` still refuses the body — nothing is defaulted; the
/// probe only decides which of two refusals the client reads.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrategyProbe {
    #[allow(dead_code)]
    remote: String,
    #[allow(dead_code)]
    branch: String,
    #[serde(default)]
    strategy: Option<MergeStrategy>,
}

/// Deserialize the request body, distinguishing the missing-strategy refusal
/// from every other malformed body.
fn parse_request(body: &[u8]) -> Result<PullRequest, (StatusCode, String)> {
    match serde_json::from_slice::<PullRequest>(body) {
        Ok(req) => Ok(req),
        Err(e) => Err(match serde_json::from_slice::<StrategyProbe>(body) {
            Ok(probe) if probe.strategy.is_none() => refusal(
                PullFailureKind::StrategyRequired,
                "A pull must say how to integrate what it fetches: send \
                 \"strategy\": \"merge\" (a merge commit when the histories \
                 diverged) or \"strategy\": \"rebase\" (replay your local \
                 commits on top). git-vista never chooses for you — the \
                 setting git would fall back to (pull.rebase) lives in a file \
                 this app doesn't show you.",
            ),
            _ => refusal(
                PullFailureKind::Other,
                &format!(
                    "That isn't a pull request this server can read: {e}. It needs \
                     exactly \"remote\", \"branch\" and \"strategy\" (\"merge\" or \
                     \"rebase\")."
                ),
            ),
        }),
    }
}

/// The name gates, split from the handler so they are testable without the
/// process-global selection `reject_if_read_only` reads (`state::CURRENT` is
/// set once per process and owned by `state`'s own test).
///
/// The same two checks every other name-taking endpoint applies — non-empty,
/// not option-shaped — before the newtypes' own validation. Both names become
/// argv elements (`git fetch <remote>`, and `<remote>/<branch>` as the ref the
/// integration runs against), so a leading `-` on either would be read by git
/// as a flag.
fn validate_names(req: &PullRequest) -> Result<(RemoteName, BranchName), (StatusCode, String)> {
    let remote = named(&req.remote, "Remote")?;
    let branch = named(&req.branch, "Branch")?;
    Ok((
        RemoteName::new(remote).map_err(|e| refusal(PullFailureKind::Other, &e.to_string()))?,
        BranchName::new(branch).map_err(|e| refusal(PullFailureKind::Other, &e.to_string()))?,
    ))
}

/// One trimmed, non-empty, non-option-shaped name, or this endpoint's refusal.
fn named<'a>(raw: &'a str, what: &str) -> Result<&'a str, (StatusCode, String)> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(refusal(
            PullFailureKind::Other,
            &format!("{what} name can't be empty."),
        ));
    }
    if name.starts_with('-') {
        return Err(refusal(
            PullFailureKind::Other,
            &format!("{what} name can't start with '-'."),
        ));
    }
    Ok(name)
}

/// A request-shape refusal in this endpoint's own error contract.
///
/// `worktree_restored: true` on every one of them, and it is a fact rather
/// than an optimistic default: none of these refusals ran a single git
/// command, so the repository is untouched by construction.
fn refusal(kind: PullFailureKind, message: &str) -> (StatusCode, String) {
    eprintln!("git-vista: /api/pull refused ({kind:?}): {message}");
    (
        StatusCode::BAD_REQUEST,
        planner::pull_error_body(kind, message.to_string(), Vec::new(), true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::PullError;

    fn body(raw: &str) -> Result<PullRequest, (StatusCode, String)> {
        parse_request(raw.as_bytes())
    }

    /// **The endpoint's headline refusal**: a body that names no strategy is a
    /// `400` carrying `StrategyRequired`, and the message names both legal
    /// values so the client can act on it.
    ///
    /// Asserted by *parsing* the body as `PullError`, not by matching text —
    /// the property a client depends on is that this refusal has the same
    /// shape as every other one from this route.
    #[test]
    fn a_body_that_names_no_strategy_is_refused_as_strategy_required() {
        for missing in [
            r#"{"remote":"origin","branch":"main"}"#,
            r#"{"branch":"main","remote":"origin"}"#,
            // An explicit JSON `null` is "no strategy" too, and gets the same
            // actionable refusal rather than a generic malformed-body one. It
            // is still refused — `PullRequest` itself rejects it, and nothing
            // is defaulted; the probe only decides *which* refusal a client
            // reads.
            r#"{"remote":"origin","branch":"main","strategy":null}"#,
        ] {
            let (status, text) = body(missing).expect_err("must be refused");
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {missing}");
            let parsed: PullError =
                serde_json::from_str(&text).expect("every /api/pull refusal is a PullError");
            assert_eq!(
                parsed.kind,
                PullFailureKind::StrategyRequired,
                "a pull with no strategy must be refused *as* that, not as a \
                 generic malformed body — the client's whole remedy is to pick \
                 one: {text}"
            );
            assert!(
                parsed.message.contains("merge") && parsed.message.contains("rebase"),
                "the refusal must name both legal values: {}",
                parsed.message
            );
            assert!(
                parsed.worktree_restored,
                "a request refused before planning cannot have touched the worktree"
            );
            assert!(parsed.updated_refs.is_empty());
        }
    }

    /// The paired positive: the *same* parser accepts both strategies, and
    /// yields the value the body named.
    ///
    /// Without this, a `parse_request` that refused everything would satisfy
    /// the test above while accepting no pull at all — and a parser that
    /// mapped every body to `Merge` would satisfy it too, which is why the
    /// strategy is asserted per case rather than just "it parsed".
    #[test]
    fn a_body_that_names_a_strategy_parses_as_that_strategy() {
        for (raw, expected) in [
            (
                r#"{"remote":"origin","branch":"main","strategy":"merge"}"#,
                MergeStrategy::Merge,
            ),
            (
                r#"{"remote":"upstream","branch":"release/2026","strategy":"rebase"}"#,
                MergeStrategy::Rebase,
            ),
        ] {
            let req = body(raw).expect("a complete pull body must parse");
            assert_eq!(req.strategy, expected, "for {raw}");
        }
    }

    /// A body that names a strategy this server does not have is **not**
    /// `StrategyRequired` — the client did choose, it just chose something
    /// that does not exist, and telling it "you must choose" would send it
    /// round the same loop. It is also not silently coerced to either arm.
    #[test]
    fn an_unknown_strategy_is_refused_without_being_narrowed_or_coerced() {
        for raw in [
            r#"{"remote":"origin","branch":"main","strategy":"auto"}"#,
            r#"{"remote":"origin","branch":"main","strategy":"default"}"#,
            r#"{"remote":"origin","branch":"main","strategy":true}"#,
            r#"{"remote":"origin","branch":"main","strategy":["merge"]}"#,
        ] {
            let (status, text) = body(raw).expect_err("must be refused");
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {raw}");
            let parsed: PullError = serde_json::from_str(&text).unwrap();
            assert_eq!(
                parsed.kind,
                PullFailureKind::Other,
                "an unknown strategy is a malformed value, not an absent one: {raw}"
            );
        }
    }

    /// The probe may only narrow a refusal, never widen what is accepted: a
    /// body with a stray key is refused even though the probe would otherwise
    /// find `strategy` absent, and a body that is not an object at all is
    /// refused too.
    #[test]
    fn the_probe_never_admits_a_body_the_real_dto_refuses() {
        for raw in [
            // The trap the probe could have opened: no strategy *and* a
            // smuggled extra key. `deny_unknown_fields` on both types is what
            // closes it.
            r#"{"remote":"origin","branch":"main","force":true}"#,
            r#"{"remote":"origin","branch":"main","strategy":"merge","force":true}"#,
            r#"["git","pull","--rebase"]"#,
            r#"{"remote":"origin"}"#,
            "not json at all",
            "",
        ] {
            let (status, text) = body(raw).expect_err("must be refused");
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {raw:?}");
            serde_json::from_str::<PullError>(&text)
                .expect("every /api/pull refusal is a PullError");
        }
    }

    /// The name gates refuse what they must and pass what they should, in this
    /// endpoint's error contract.
    #[test]
    fn malformed_names_are_refused_as_the_endpoints_error_type() {
        for (remote, branch) in [
            ("", "main"),
            ("   ", "main"),
            ("-force-looking", "main"),
            ("origin", ""),
            ("origin", "--upload-pack=/bin/sh"),
        ] {
            let req = PullRequest {
                remote: remote.to_string(),
                branch: branch.to_string(),
                strategy: MergeStrategy::Merge,
            };
            let (status, text) = validate_names(&req).expect_err("must be refused");
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {remote:?}/{branch:?}");
            let parsed: PullError = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed.kind, PullFailureKind::Other);
        }
    }

    /// **The mandate, pinned at the HTTP layer** — through a real router, the
    /// real `api_contract` middleware, and the real handler, because that is
    /// where the `400` the issue asks for either exists or does not.
    ///
    /// Three legs, and the third is what makes the first two mean something:
    ///
    /// 1. a body with no `strategy` comes back **400**, and the endpoint's own
    ///    `PullError` (carrying `strategy_required`) survives to the client;
    /// 2. …and it is a `400`, not axum's `422` — which is exactly what this
    ///    endpoint would answer if it took `Json<PullRequest>` like every
    ///    other write handler, and is the whole reason it does not;
    /// 3. the *same* router, same headers, with `"strategy": "merge"` added,
    ///    gets **past** the strategy gate — proved by it failing on the
    ///    *next* gate instead (an empty remote), with a different `kind`. A
    ///    router that refused everything would satisfy legs 1 and 2.
    ///
    /// Leg 3 stops at the shape gate on purpose: going further would reach
    /// `reject_if_read_only`, which reads the set-once-per-process
    /// `state::CURRENT` that `state`'s own test owns.
    #[tokio::test]
    async fn the_strategy_mandate_is_a_400_through_a_real_router() {
        use axum::body::{to_bytes, Body};
        use axum::http::{header, Request};
        use axum::routing::post;
        use axum::Router;
        use git_vista_protocol::{PROTOCOL_HEADER, PROTOCOL_VERSION};
        use tower::ServiceExt;

        let router = Router::new()
            .route("/api/pull", post(pull_branch))
            .layer(axum::middleware::from_fn(crate::middleware::api_contract));

        async fn post_body(router: &Router, body: &'static str) -> (StatusCode, String) {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/pull")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }

        /// The endpoint's own error body — parsed directly, **not** dug out of
        /// an `ApiError` envelope.
        ///
        /// This mirrors `/api/amend-commit`'s contract exactly (and
        /// `/api/fetch`'s, which builds its `FetchError` the same way): the
        /// handler returns `(StatusCode, String)` with the `String` already a
        /// pre-serialized typed DTO, and `middleware::rewrap_error` (#323)
        /// recognises a JSON-object body and passes it through untouched
        /// instead of escaping it into a generic envelope's `message` field.
        /// Before #323's fix this test asserted the *bug* — a `PullError`
        /// double-encoded inside `ApiError.message` — as if it were the
        /// contract; the frontend never actually unwrapped an envelope here
        /// (`api.rs::receipt` hands the raw response text straight to
        /// `pull_summary`, which parses it as `PullError` directly), so that
        /// assertion was pinning behaviour nothing downstream relied on.
        fn inner(raw: &str) -> PullError {
            serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("body was not a bare PullError: {e}\nbody={raw}"))
        }

        // Leg 1 + 2.
        let (status, raw) = post_body(&router, r#"{"remote":"origin","branch":"main"}"#).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a pull with no strategy must be a 400. A 422 here means the \
             handler went back to `Json<PullRequest>` and the client now gets \
             a serde complaint instead of an instruction: {raw}"
        );
        let error = inner(&raw);
        assert_eq!(error.kind, PullFailureKind::StrategyRequired, "{raw}");
        assert!(
            error.message.contains("merge") && error.message.contains("rebase"),
            "{raw}"
        );

        // Leg 3: the paired positive — a named strategy gets past this gate.
        let (status, raw) = post_body(
            &router,
            r#"{"remote":"","branch":"main","strategy":"merge"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(
            inner(&raw).kind,
            PullFailureKind::Other,
            "a body that DID name a strategy must reach the next gate — if this \
             is still `strategy_required`, the router is refusing everything \
             and the legs above prove nothing: {raw}"
        );
    }

    /// The paired positive: ordinary names pass, and pass *as themselves*
    /// (trimmed, not rewritten). Without this, a gate that refused everything
    /// would satisfy the test above while accepting nothing.
    #[test]
    fn ordinary_names_pass_the_shape_gate_unchanged() {
        for (remote, branch) in [("origin", "main"), ("  upstream  ", " release/2026 ")] {
            let req = PullRequest {
                remote: remote.to_string(),
                branch: branch.to_string(),
                strategy: MergeStrategy::Rebase,
            };
            let (r, b) = validate_names(&req).expect("ordinary names must pass");
            assert_eq!(r.as_str(), remote.trim());
            assert_eq!(b.as_str(), branch.trim());
        }
    }
}
