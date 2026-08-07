//! `execute_plan` — the one MCP tool that can mutate (M2.23e, #249).
//!
//! Every other tool in this crate either reads (`tools.rs`) or builds a
//! reviewable [`Plan`] without running it (`plan_tools.rs`, #248). This tool
//! takes the exact `plan` object a `plan_*` tool call returned and POSTs it
//! to `POST /api/execute-plan`, which on the server side reaches
//! `planner::submit_plan_tracked` — the stage that re-validates a plan
//! against the *live* repository (operation hash, expiry, generation, every
//! precondition) before running anything. A tampered, expired, or stale plan
//! is refused there, in the server's own words, and this bridge passes that
//! refusal through unparaphrased: `tools::authed_post`'s only non-2xx path is
//! `POST {path} answered {status}: {body}`, and `String::from_utf8_lossy`
//! never rewrites content it can decode, so the server's exact wording (e.g.
//! `validate()`'s "This plan has expired — refresh and try again.") reaches
//! [`crate::tools::ToolError::Execution`] byte-for-byte.
//!
//! # Idempotency, derived rather than transported ad hoc
//!
//! Every other tracked write in this codebase mints its idempotency key from
//! a live user action (a tap of Commit, a click of Push). This tool has no
//! such moment — a client hands it a `plan` object and nothing else — so the
//! key is instead **derived deterministically from the plan itself**:
//! `mcp-{operation_hash}-{issued_at}`. Both fields already live on [`Plan`]
//! and are round-tripped unmodified by the caller. Two calls carrying
//! byte-identical `plan` JSON (the retry case) always produce the identical
//! key, so the server's `admit()` replays the recorded result instead of
//! running git a second time; two independently built plans for the "same"
//! operation almost always differ in `issued_at`, so both execute, as
//! intended.
//!
//! `operation_hash` is 64 lowercase hex and `issued_at` is a plain integer —
//! both already validated by `Plan`'s own `Deserialize` — so the constructed
//! string is provably charset-safe for
//! [`git_vista_protocol::IDEMPOTENCY_HEADER`] (`[A-Za-z0-9_-]`) without
//! needing a new hashing dependency: no `sha2` (not a workspace dependency)
//! and no `std::collections::hash_map::DefaultHasher` (randomized per-process
//! by default, which would break retry-safety across a bridge restart). A
//! useful corollary, provable from the construction: since the key
//! **embeds** the hash, two submissions sharing a key necessarily share
//! `operation_hash`, so `operations::Admission::Conflict` ("key reused for a
//! different operation") can never trigger via this client — only `Fresh` or
//! `Existing`.
//!
//! # Why a second `PostFn` type instead of widening the existing one
//!
//! [`crate::tools::PostFn`] (`FnMut(&str, &[u8], &str, &str) -> …`) is the
//! shape all ~23 of `plan_tools.rs`'s tool tests construct closures against.
//! Widening it to five arguments to carry a key would ripple into every one
//! of those closures for a capability none of them need (`/api/plan` never
//! sends a key). So this module defines its own [`PostFnIdempotent`] purely
//! as its own test-injection seam, and production code calls the existing,
//! **unmodified** [`crate::tools::authed_post`] through a thin adapter
//! closure that closes over the derived key — reusing `authed_post`'s proven
//! lazy-auth / retry-once-on-401 / give-up-on-second-401 logic unchanged, so
//! a 401 mid-submission automatically resends the same captured key on
//! retry, for free.

use git_vista_protocol::Plan;

use crate::auth::{self, Session};
use crate::http::{self, HttpResponse};
use crate::tools::ToolError;

/// The one endpoint this tool talks to.
pub(crate) const EXECUTE_ENDPOINT: &str = "/api/execute-plan";

/// [`crate::tools::PostFn`]'s sibling for this tool's own test-injection
/// seam: `(path, body, cookie, csrf, idempotency_key) -> response`. See the
/// module doc for why this is a second type rather than a widened
/// [`crate::tools::PostFn`].
pub(crate) type PostFnIdempotent<'a> =
    dyn FnMut(&str, &[u8], &str, &str, &str) -> Result<HttpResponse, String> + 'a;

/// The `execute_plan` half of `tools/list`. Appended after the `plan_*`
/// surface by [`crate::tools::tool_catalog`] — the catalog's LAST entry,
/// and the only one that mutates.
pub(crate) fn execute_tool_catalog() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "name": "execute_plan",
        "description": "Submit a plan built by a plan_* tool for execution (POST \
                        /api/execute-plan). Pass the exact `plan` object a plan_* tool \
                        call returned, byte-identical — the server re-validates its \
                        operation hash, expiry, generation and every precondition \
                        against the live repository before running anything, and \
                        refuses (with its own explanation) if the plan was tampered \
                        with, has expired, or the repository moved underneath it. \
                        This is the only tool in this bridge that can mutate a \
                        repository.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "plan": {
                    "type": "object",
                    "description": "The exact `plan` object returned by a prior plan_* \
                                    tool call. Resubmit it byte-identical to retry safely \
                                    — the idempotency key this tool sends is derived from \
                                    the plan's own operation_hash and issued_at, so an \
                                    identical resubmission replays the original result \
                                    instead of running git twice."
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }
    })]
}

/// `mcp-{operation_hash}-{issued_at}` — see the module doc for why this
/// specific, deterministic construction and not a random or hashed one.
fn idempotency_key_for(plan: &Plan) -> String {
    format!("mcp-{}-{}", plan.operation_hash.as_str(), plan.issued_at.0)
}

/// Run the `execute_plan` tool: parse the given `plan` argument, derive its
/// idempotency key, and `POST` it to [`EXECUTE_ENDPOINT`]. `None` when `name`
/// is not this tool at all (so `tools::call_tool`'s dispatcher can fall
/// through to its own unknown-tool handling).
///
/// Production passes an adapter over [`crate::tools::authed_post`] (see
/// [`call_execute_tool_live`]); tests inject a capturing closure directly, so
/// the path/body/key of a call can be asserted without a server.
pub(crate) fn call_execute_tool(
    name: &str,
    args: &serde_json::Value,
    session: &mut Option<Session>,
    post: &mut PostFnIdempotent<'_>,
    authenticate: &mut dyn FnMut() -> Result<Session, String>,
) -> Option<Result<serde_json::Value, ToolError>> {
    if name != "execute_plan" {
        return None;
    }
    let plan_value = match args.get("plan") {
        Some(v) => v,
        None => {
            return Some(Err(ToolError::Execution(
                "missing required argument `plan`".to_string(),
            )))
        }
    };
    let plan: Plan = match serde_json::from_value(plan_value.clone()) {
        Ok(p) => p,
        Err(e) => {
            return Some(Err(ToolError::Execution(format!(
                "`plan` is not a valid Plan: {e}"
            ))))
        }
    };
    let key = idempotency_key_for(&plan);
    let body = match serde_json::to_vec(&plan) {
        Ok(b) => b,
        Err(e) => {
            return Some(Err(ToolError::Execution(format!(
                "could not encode the plan: {e}"
            ))))
        }
    };

    let mut adapter =
        |path: &str, body: &[u8], cookie: &str, csrf: &str| post(path, body, cookie, csrf, &key);
    let raw =
        crate::tools::authed_post(EXECUTE_ENDPOINT, &body, session, &mut adapter, authenticate);
    Some(match raw {
        Ok(bytes) => Ok(serde_json::Value::String(
            String::from_utf8_lossy(&bytes).into_owned(),
        )),
        Err(e) => Err(ToolError::Execution(e)),
    })
}

/// Production's [`call_execute_tool`], with the real HTTP client and
/// authenticator wired in. Kept separate so the injectable form above has no
/// production caller passing anything unusual — same split as
/// `plan_tools::call_plan_tool_live`.
pub(crate) fn call_execute_tool_live(
    name: &str,
    args: &serde_json::Value,
    session: &mut Option<Session>,
) -> Option<Result<serde_json::Value, ToolError>> {
    call_execute_tool(
        name,
        args,
        session,
        &mut |path, body, cookie, csrf, key| {
            http::post_json_idempotent(path, body, Some(cookie), Some(csrf), key)
        },
        &mut auth::authenticate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        GenerationToken, GitOperation, OperationHash, RecoveryStrategy, RepositoryToken, RiskLevel,
        UnixSeconds, WorktreeToken,
    };

    fn session(cookie: &str) -> Session {
        Session {
            cookie: cookie.to_string(),
            csrf: "csrf".to_string(),
        }
    }

    fn resp(status: u16, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn a_plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("7").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_753_300_000),
            expires_at: UnixSeconds(1_753_300_300),
            risk: RiskLevel::Safe,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        }
    }

    #[test]
    fn the_execute_tool_catalog_has_exactly_one_closed_write_tool() {
        let cat = execute_tool_catalog();
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0]["name"], "execute_plan");
        assert_eq!(
            cat[0]["inputSchema"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            cat[0]["inputSchema"]["required"],
            serde_json::json!(["plan"])
        );
    }

    /// The retry case: two calls carrying byte-identical `plan` JSON must
    /// derive the identical idempotency key, or the server's `admit()` can
    /// never recognise the second as a replay of the first.
    #[test]
    fn identical_plans_derive_the_identical_idempotency_key() {
        let a = idempotency_key_for(&a_plan());
        let b = idempotency_key_for(&a_plan());
        assert_eq!(a, b);
        assert!(a.starts_with("mcp-"));
        assert!(
            a.contains(&"a".repeat(64)),
            "must embed operation_hash: {a}"
        );
        assert!(a.contains("1753300000"), "must embed issued_at: {a}");
    }

    /// The independent-plans case: a different `issued_at` (the ordinary case
    /// when the same operation is planned twice) must derive a different key,
    /// or two genuinely separate approvals would collide into one execution.
    #[test]
    fn plans_with_different_issued_at_derive_different_keys() {
        let mut later = a_plan();
        later.issued_at = UnixSeconds(1_753_300_001);
        assert_ne!(idempotency_key_for(&a_plan()), idempotency_key_for(&later));
    }

    /// Every byte of the derived key must be in `IdempotencyKey`'s own
    /// charset (`[A-Za-z0-9_-]`) — this is the module doc's "provably
    /// charset-safe" claim, checked rather than merely asserted in prose.
    #[test]
    fn the_derived_key_is_idempotency_key_charset_safe() {
        let key = idempotency_key_for(&a_plan());
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "key contains a byte outside IdempotencyKey's charset: {key}"
        );
        assert!(key.len() <= git_vista_protocol::MAX_IDEMPOTENCY_KEY_LEN);
    }

    #[test]
    fn call_execute_tool_ignores_every_other_tool_name() {
        let mut none = None;
        assert!(call_execute_tool(
            "plan_stage_all",
            &serde_json::json!({}),
            &mut none,
            &mut |_, _, _, _, _| panic!("must never POST for a name it doesn't own"),
            &mut || panic!("must never authenticate for a name it doesn't own"),
        )
        .is_none());
    }

    #[test]
    fn a_missing_plan_argument_is_refused_before_authenticating() {
        let mut none = None;
        let result = call_execute_tool(
            "execute_plan",
            &serde_json::json!({}),
            &mut none,
            &mut |_, _, _, _, _| panic!("must never POST without a plan"),
            &mut || panic!("must never authenticate without a plan"),
        );
        match result {
            Some(Err(ToolError::Execution(msg))) => assert!(msg.contains("plan")),
            other => panic!("expected a local Execution refusal, got {other:?}"),
        }
        assert!(none.is_none());
    }

    #[test]
    fn a_malformed_plan_argument_is_refused_before_authenticating() {
        let mut none = None;
        let result = call_execute_tool(
            "execute_plan",
            &serde_json::json!({ "plan": { "not": "a real plan" } }),
            &mut none,
            &mut |_, _, _, _, _| panic!("must never POST a plan that failed to parse"),
            &mut || panic!("must never authenticate for a plan that failed to parse"),
        );
        match result {
            Some(Err(ToolError::Execution(msg))) => assert!(msg.contains("not a valid Plan")),
            other => panic!("expected a local Execution refusal, got {other:?}"),
        }
        assert!(none.is_none());
    }

    /// The seam this whole tool exists for: the plan reaches
    /// `EXECUTE_ENDPOINT` with the key [`idempotency_key_for`] derives,
    /// carrying the session's cookie and CSRF token exactly like every other
    /// tracked write.
    #[test]
    fn a_successful_submission_posts_the_plan_to_execute_endpoint_with_its_derived_key() {
        let plan = a_plan();
        let expected_key = idempotency_key_for(&plan);
        let mut sess = Some(session("gv_session=live"));
        let mut seen: Vec<(String, String, String, String)> = Vec::new();
        let result = call_execute_tool(
            "execute_plan",
            &serde_json::json!({ "plan": plan }),
            &mut sess,
            &mut |path, body, cookie, csrf, key| {
                seen.push((
                    path.to_string(),
                    cookie.to_string(),
                    csrf.to_string(),
                    key.to_string(),
                ));
                assert!(!body.is_empty());
                Ok(resp(200, b"queued"))
            },
            &mut || panic!("a live session must not re-authenticate"),
        )
        .unwrap();
        assert_eq!(
            result.unwrap(),
            serde_json::Value::String("queued".to_string())
        );
        assert_eq!(
            seen,
            [(
                EXECUTE_ENDPOINT.to_string(),
                "gv_session=live".to_string(),
                "csrf".to_string(),
                expected_key,
            )]
        );
    }

    /// The idempotency key must survive a 401-triggered re-authentication and
    /// retry unchanged — `authed_post`'s own retry logic resends whatever the
    /// adapter closure captured, and this pins that the capture is the key,
    /// not something that could drift between the two POSTs.
    #[test]
    fn a_401_reauthenticates_and_retries_with_the_same_derived_key() {
        let plan = a_plan();
        let expected_key = idempotency_key_for(&plan);
        let mut sess = Some(session("gv_session=stale"));
        let mut keys_seen = Vec::new();
        let result = call_execute_tool(
            "execute_plan",
            &serde_json::json!({ "plan": plan }),
            &mut sess,
            &mut |_, _, cookie, _, key| {
                keys_seen.push(key.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"queued"))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert!(result.is_ok());
        assert_eq!(keys_seen, [expected_key.clone(), expected_key]);
    }

    /// The server's own refusal text (tampered / expired / stale plan) must
    /// reach the tool's `Execution` error unparaphrased — this is
    /// `tools::authed_post`'s existing guarantee, exercised here through this
    /// tool's own call path rather than assumed.
    #[test]
    fn a_non_2xx_response_is_forwarded_verbatim_as_an_execution_error() {
        let plan = a_plan();
        let mut sess = Some(session("gv_session=live"));
        let result = call_execute_tool(
            "execute_plan",
            &serde_json::json!({ "plan": plan }),
            &mut sess,
            &mut |_, _, _, _, _| {
                Ok(resp(
                    409,
                    r#"{"code":"conflict","message":"This plan has expired — refresh and try again."}"#
                        .as_bytes(),
                ))
            },
            &mut || panic!("a live session must not re-authenticate on a non-401"),
        )
        .unwrap();
        match result {
            Err(ToolError::Execution(msg)) => {
                assert!(msg.contains("This plan has expired"), "{msg}");
            }
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[test]
    fn call_execute_tool_live_reaches_the_real_http_and_auth_wiring() {
        // Not a network test — merely proves the production wrapper actually
        // dispatches to `call_execute_tool` for its one tool name (the same
        // "the arm exists" proof `tools.rs`'s
        // `every_plan_tool_is_reachable_through_call_tools_dispatcher` gives
        // the plan surface) and returns `None` for anything else, without
        // ever touching the network (a missing `plan` argument refuses
        // locally before any HTTP call is attempted).
        let mut none = None;
        assert!(call_execute_tool_live("get_status", &serde_json::json!({}), &mut none).is_none());
        let refused = call_execute_tool_live("execute_plan", &serde_json::json!({}), &mut none);
        assert!(matches!(refused, Some(Err(ToolError::Execution(_)))));
        assert!(none.is_none());
    }
}
