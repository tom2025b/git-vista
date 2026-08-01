//! The MCP tool surface (M2.23a, #245): exactly one read tool in this slice.
//!
//! `list_repositories` calls `GET /api/catalog` with the stored session cookie
//! and returns the same JSON the SPA's picker consumes — proven equivalent by
//! the live integration test, not asserted here.

use crate::auth::{self, Session};
use crate::http::{self, HttpResponse};

/// Why a tool call failed — a **protocol** failure (the client asked for a
/// tool that doesn't exist: JSON-RPC `-32602`) versus an **execution** failure
/// of a real tool (auth, HTTP, parse: MCP's `isError` result). The MCP spec
/// separates these, and this dispatcher is the template the rest of the #153
/// chain copies, so the taxonomy is typed from the first slice.
#[derive(Debug)]
pub enum ToolError {
    /// No such tool. The name goes back to the client in a `-32602` error.
    Unknown(String),
    /// A known tool ran and failed; the message is for the client's eyes.
    Execution(String),
}

/// The catalog of tools this bridge advertises to `tools/list`. One entry for
/// now; the read-surface slice (#246) grows it.
pub fn tool_catalog() -> serde_json::Value {
    serde_json::json!([{
        "name": "list_repositories",
        "description": "List every repository and clone the running git-vista server \
                        knows about, exactly as its own picker sees them (GET /api/catalog).",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
    }])
}

/// Run a tool by name. `session` is authenticated lazily on first use and
/// re-established once on a 401 — the server rotates sessions on restart, and
/// the bridge may well outlive one server process.
pub fn call_tool(
    name: &str,
    session: &mut Option<Session>,
) -> Result<serde_json::Value, ToolError> {
    match name {
        "list_repositories" => {
            let body = authed_fetch(
                "/api/catalog",
                session,
                &mut |path, cookie| http::get(path, Some(cookie)),
                &mut auth::authenticate,
            )
            .map_err(ToolError::Execution)?;
            serde_json::from_slice(&body).map_err(|e| {
                ToolError::Execution(format!("the catalog response was not JSON: {e}"))
            })
        }
        other => Err(ToolError::Unknown(other.to_string())),
    }
}

/// GET with the session cookie, authenticating on demand and retrying exactly
/// once on 401 with a fresh session (covers a server restart mid-bridge).
///
/// Generic over the fetch and auth closures so the three legs — lazy first
/// auth, 401 → re-auth → retry with the NEW cookie, 401 → 401 giving up — are
/// unit-testable without a server. Production passes `http::get` and
/// `auth::authenticate`.
fn authed_fetch(
    path: &str,
    session: &mut Option<Session>,
    fetch: &mut dyn FnMut(&str, &str) -> Result<HttpResponse, String>,
    auth: &mut dyn FnMut() -> Result<Session, String>,
) -> Result<Vec<u8>, String> {
    if session.is_none() {
        *session = Some(auth()?);
    }
    let cookie = session.as_ref().expect("just set").cookie.clone();
    let resp = fetch(path, &cookie)?;
    if resp.status == 401 {
        *session = Some(auth()?);
        let cookie = session.as_ref().expect("just set").cookie.clone();
        let retry = fetch(path, &cookie)?;
        if retry.status != 200 {
            return Err(format!(
                "GET {path} answered {} even after re-authenticating: {}",
                retry.status,
                String::from_utf8_lossy(&retry.body)
            ));
        }
        return Ok(retry.body);
    }
    if resp.status != 200 {
        return Err(format!(
            "GET {path} answered {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }
    Ok(resp.body)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_tool_catalog_lists_exactly_the_shipped_tool() {
        let cat = tool_catalog();
        let names: Vec<&str> = cat
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_repositories"]);
    }

    #[test]
    fn an_unknown_tool_is_refused_by_name_without_authenticating() {
        let mut none = None;
        match call_tool("drop_tables", &mut none) {
            Err(ToolError::Unknown(name)) => assert_eq!(name, "drop_tables"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        // Crucially: refusing an unknown tool never attempted to authenticate
        // — no session was created for a request that will never be sent.
        assert!(none.is_none());
    }

    #[test]
    fn the_first_call_authenticates_lazily_and_sends_that_cookie() {
        let mut sess = None;
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                Ok(resp(200, b"ok"))
            },
            &mut || Ok(session("gv_session=first")),
        )
        .unwrap();
        assert_eq!(body, b"ok");
        assert_eq!(seen, ["gv_session=first"]);
        assert_eq!(sess.unwrap().cookie, "gv_session=first");
    }

    #[test]
    fn a_401_reauthenticates_once_and_retries_with_the_new_cookie() {
        // The trap this test exists for: a retry that resends the STALE
        // cookie would loop 401 forever in production while looking
        // superficially like a retry.
        let mut sess = Some(session("gv_session=stale"));
        let mut seen = Vec::new();
        let body = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, cookie| {
                seen.push(cookie.to_string());
                if cookie == "gv_session=stale" {
                    Ok(resp(401, b""))
                } else {
                    Ok(resp(200, b"fresh"))
                }
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap();
        assert_eq!(body, b"fresh");
        assert_eq!(seen, ["gv_session=stale", "gv_session=fresh"]);
    }

    #[test]
    fn a_second_401_gives_up_rather_than_retrying_forever() {
        let mut sess = Some(session("gv_session=stale"));
        let mut fetches = 0;
        let err = authed_fetch(
            "/x",
            &mut sess,
            &mut |_, _| {
                fetches += 1;
                Ok(resp(401, b"no"))
            },
            &mut || Ok(session("gv_session=fresh")),
        )
        .unwrap_err();
        assert_eq!(fetches, 2, "exactly one retry, never a loop");
        assert!(err.contains("even after re-authenticating"));
    }
}
