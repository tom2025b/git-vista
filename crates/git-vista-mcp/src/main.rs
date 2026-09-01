//! git-vista-mcp — an MCP stdio bridge to the running git-vista-server
//! (M2.23a, #245; parent #153).
//!
//! An MCP client (Claude Code, Claude Desktop, anything speaking the protocol)
//! launches this binary and drives git-vista through it. This slice proves the
//! transport end to end — stdio framing, authentication, one real read tool —
//! and deliberately carries **no write capability**: the write path arrives
//! only after the planner split (#247) so that agents submit reviewable plans
//! through the same funnel the browser uses, never argv.
//!
//! # Still no write capability after M2.23d (#248)
//!
//! [`plan_tools`] added one `plan_<operation>` tool per exposed
//! `GitOperation` variant. Every one of them **builds a reviewable `Plan` and
//! returns it, executing nothing**: the only endpoint they can reach is
//! `POST /api/plan`, which on the server side reaches only
//! `planner::build_plan_only` — no mutation guard, no executor, no argv.
//! Submitting an approved plan was #249's `execute_plan` on its own endpoint;
//! the separation is the point of the funnel, and
//! `plan_tools::tests::every_plan_tool_posts_only_to_api_plan` is what keeps
//! it a fact rather than an intention.
//!
//! # Write capability, exactly once (M2.23e, #249)
//!
//! [`execute_tool`] adds the one tool that can mutate: `execute_plan`. It
//! takes the exact `plan` object a `plan_*` tool call returned and POSTs it
//! to `POST /api/execute-plan`, which on the server side reaches
//! `planner::submit_plan_tracked` — the stage that re-validates a plan
//! against the *live* repository (operation hash, expiry, generation, every
//! precondition) before running anything. A tampered, expired, or stale plan
//! is refused there, in the server's own words, and this bridge passes that
//! refusal through unparaphrased (see [`execute_tool`]'s module doc). A
//! retry of the identical `plan` object is replay-safe: the idempotency key
//! this tool sends is derived deterministically from the plan itself, not
//! randomly minted, so a second submission of the same plan reproduces the
//! same key and the server replays its recorded result instead of running
//! git twice. Auth is the same bootstrap-token exchange every other tool
//! here uses — no second path.
//!
//! # Transport choice, recorded per #245's scope
//!
//! Hand-rolled **newline-delimited JSON-RPC 2.0**, which is MCP's stdio
//! framing: one JSON-RPC message per line on stdin/stdout, nothing else on
//! stdout, logs to stderr. Evaluated pulling a Rust MCP SDK instead; none is
//! in the workspace today, the protocol surface this slice needs is four
//! methods (`initialize`, `notifications/initialized`, `tools/list`,
//! `tools/call`), and the reviewed-dependency discipline
//! (`docs/NATIVE_DEPENDENCIES.md`) prices an SDK's whole tree against ~150
//! lines of dispatch. Hand-rolled wins at this size; revisit when the tool
//! surface (#246–#249) makes the framing the bigger half of the crate.

mod execute_tool;
mod hygiene;
mod lesson;
mod plan_tools;
mod tools;

use git_vista_session::auth;

use std::io::{BufRead, Write};

/// The MCP protocol revision this bridge answers `initialize` with.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut session: Option<auth::Session> = None;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin closed — the client is gone; exit cleanly.
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = handle_line(&line, &mut session) else {
            continue; // a notification — nothing goes back
        };
        // One message per line, newline-terminated, flushed — the client
        // blocks on this framing. Serialization of a Value cannot fail; any
        // error here is stdout closing, which means the client is gone.
        let mut encoded = reply.to_string();
        encoded.push('\n');
        let mut out = stdout.lock();
        if out
            .write_all(encoded.as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            break; // stdout closed — same as stdin: client gone.
        }
    }
}

/// Dispatch one incoming line. `None` means "send nothing back"
/// (notifications, unparseable junk without an id).
fn handle_line(line: &str, session: &mut Option<auth::Session>) -> Option<serde_json::Value> {
    let msg: serde_json::Value = match serde_json::from_str(line) {
        Ok(m) => m,
        // A parse failure has no id to answer; JSON-RPC's parse-error reply
        // uses id null.
        Err(_) => return Some(error_reply(serde_json::Value::Null, -32700, "parse error")),
    };
    // Valid JSON that isn't a request object at all (a bare string, a batch
    // array — MCP 2024-11-05 has no batching, an object with no method) is
    // Invalid Request, not a notification to swallow.
    if !msg.is_object() || !msg.get("method").is_some_and(|m| m.is_string()) {
        return Some(error_reply(
            serde_json::Value::Null,
            -32600,
            "invalid request",
        ));
    }
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Notifications (no id) get no reply, per JSON-RPC 2.0.
    let id = id?;

    let reply = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "git-vista-mcp", "version": env!("CARGO_PKG_VERSION") }
        }),
        "ping" => serde_json::json!({}),
        "tools/list" => serde_json::json!({ "tools": tools::tool_catalog() }),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let empty = serde_json::json!({});
            let arguments = msg.pointer("/params/arguments").unwrap_or(&empty);
            return Some(match tools::call_tool(name, arguments, session) {
                Ok(value) => result_reply(
                    id,
                    serde_json::json!({
                        // MCP tool results are content blocks; the JSON payload
                        // travels as text, which every client renders.
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&value)
                                .unwrap_or_else(|_| value.to_string()),
                        }],
                        "isError": false
                    }),
                ),
                // The MCP error taxonomy, kept apart on purpose: asking for a
                // tool that doesn't exist (or omitting the name) is a PROTOCOL
                // error — JSON-RPC -32602 — while a real tool failing is an
                // EXECUTION result flagged isError, so clients show the
                // message instead of dropping the call.
                Err(tools::ToolError::Unknown(name)) => {
                    error_reply(id, -32602, &format!("Unknown tool: {name}"))
                }
                Err(tools::ToolError::Execution(e)) => result_reply(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": e }],
                        "isError": true
                    }),
                ),
            });
        }
        other => {
            return Some(error_reply(
                id,
                -32601,
                &format!("method not found: {other}"),
            ))
        }
    };
    Some(result_reply(id, reply))
}

fn result_reply(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": code, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(line: &str) -> Option<serde_json::Value> {
        let mut session = None;
        handle_line(line, &mut session)
    }

    #[test]
    fn initialize_advertises_tools_and_names_itself() {
        let r = dispatch(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["serverInfo"]["name"], "git-vista-mcp");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_names_list_repositories() {
        let r = dispatch(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(r["result"]["tools"][0]["name"], "list_repositories");
    }

    #[test]
    fn a_notification_gets_no_reply() {
        assert!(dispatch(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn an_unknown_method_is_a_method_not_found_error() {
        let r = dispatch(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn an_unknown_tool_is_a_protocol_error_not_an_execution_result() {
        // MCP separates "no such tool" (JSON-RPC -32602) from "a real tool
        // ran and failed" (isError result). This dispatcher is the template
        // the #153 chain copies, so the taxonomy is locked by test.
        let r =
            dispatch(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#)
                .unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert!(r["error"]["message"].as_str().unwrap().contains("nope"));
        assert!(r["result"].is_null());
    }

    #[test]
    fn unparseable_input_answers_a_parse_error_with_null_id() {
        let r = dispatch("this is not json").unwrap();
        assert_eq!(r["error"]["code"], -32700);
        assert!(r["id"].is_null());
    }

    #[test]
    fn valid_json_that_is_not_a_request_object_is_invalid_request() {
        for junk in [r#""just a string""#, r#"[1,2,3]"#, r#"{"id":9}"#] {
            let r = dispatch(junk).unwrap();
            assert_eq!(r["error"]["code"], -32600, "input: {junk}");
        }
    }
}
