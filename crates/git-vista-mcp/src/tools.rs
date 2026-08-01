//! The MCP tool surface (M2.23a, #245): exactly one read tool in this slice.
//!
//! `list_repositories` calls `GET /api/catalog` with the stored session cookie
//! and returns the same JSON the SPA's picker consumes — proven equivalent by
//! the live integration test, not asserted here.

use crate::auth::{self, Session};
use crate::http;

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
pub fn call_tool(name: &str, session: &mut Option<Session>) -> Result<serde_json::Value, String> {
    match name {
        "list_repositories" => {
            let body = authed_get("/api/catalog", session)?;
            let parsed: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| format!("the catalog response was not JSON: {e}"))?;
            Ok(parsed)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// GET with the session cookie, authenticating on demand and retrying exactly
/// once on 401 with a fresh session (covers a server restart mid-bridge).
fn authed_get(path: &str, session: &mut Option<Session>) -> Result<Vec<u8>, String> {
    if session.is_none() {
        *session = Some(auth::authenticate()?);
    }
    let cookie = session.as_ref().expect("just set").cookie.clone();
    let resp = http::get(path, Some(&cookie))?;
    if resp.status == 401 {
        *session = Some(auth::authenticate()?);
        let cookie = session.as_ref().expect("just set").cookie.clone();
        let retry = http::get(path, Some(&cookie))?;
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
    fn an_unknown_tool_is_refused_by_name() {
        let mut none = None;
        let err = call_tool("drop_tables", &mut none).unwrap_err();
        assert!(err.contains("unknown tool"));
        // And crucially: refusing an unknown tool never attempted to
        // authenticate — no session was created for a request that will
        // never be sent.
        assert!(none.is_none());
    }
}
