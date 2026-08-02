#!/usr/bin/env python3
"""Disposable MCP server driving the git-vista testbed on 127.0.0.1:8082.

Stdlib only. Never touches port 8080 — the constant below is the only place
a port can come from, and any base_url containing ':8080' is refused outright.

See design-docs/2026-08-01-temp-test-mcp-plan.md for the approved plan.
Delete this whole tmp-mcp/ folder and `claude mcp remove` it when M2 is done.
"""

import http.cookies
import json
import re
import sys
import urllib.error
import urllib.request

DEFAULT_BASE_URL = "http://127.0.0.1:8082"
FORBIDDEN_PORT = ":8080"
PROTOCOL_VERSION = "4"  # git_vista_protocol::PROTOCOL_VERSION — bump if that bumps

# Mirrors crates/git-vista-server/src/main.rs's route table, grepped 2026-08-01.
ROUTES = [
    ("GET", "/api/frame"),
    ("GET", "/api/commits"),
    ("GET", "/api/protocol"),
    ("GET", "/api/catalog"),
    ("GET", "/api/session"),
    ("POST", "/api/session"),
    ("DELETE", "/api/session"),
    ("GET", "/api/commit/{id}"),
    ("GET", "/api/diff/{id}"),
    ("GET", "/api/file/{id}/{*path}"),
    ("GET", "/api/head-branch"),
    ("GET", "/api/status"),
    ("GET", "/api/status/v2"),
    ("GET", "/api/activity"),
    ("GET", "/api/undoables/{id}"),
    ("GET", "/api/rebase-status"),
    ("POST", "/api/clone"),
    ("POST", "/api/delete-clone"),
    ("POST", "/api/select"),
    ("POST", "/api/rescan"),
    ("POST", "/api/branch"),
    ("POST", "/api/commit"),
    ("POST", "/api/stage"),
    ("POST", "/api/unstage"),
    ("POST", "/api/undo"),
    ("POST", "/api/merge"),
    ("POST", "/api/push"),
    ("POST", "/api/delete-branch"),
    ("POST", "/api/checkout"),
    ("POST", "/api/force-delete-branch"),
    ("POST", "/api/rebase"),
    ("POST", "/api/reset-test-repo"),
]

# Mutated by the login tool. One session at a time — this is a disposable
# single-user dev tool, not a multi-tenant server.
STATE = {"base_url": DEFAULT_BASE_URL, "cookie": None, "csrf": None}


def guard_base_url(base_url):
    if FORBIDDEN_PORT in base_url:
        raise ValueError(
            f"refusing to target {base_url!r} — port 8080 is the live server, "
            "never touched by this tool. Point at the testbed instead."
        )


def http_call(method, path, body=None):
    base_url = STATE["base_url"]
    guard_base_url(base_url)
    url = base_url.rstrip("/") + path
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("x-git-vista-protocol", PROTOCOL_VERSION)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if STATE["cookie"]:
        req.add_header("Cookie", f"gv_session={STATE['cookie']}")
    if STATE["csrf"] and method not in ("GET", "HEAD"):
        req.add_header("x-git-vista-csrf", STATE["csrf"])
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            status = resp.status
            raw = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        status = e.code
        raw = e.read().decode("utf-8", errors="replace")
    try:
        parsed = json.loads(raw)
        return status, parsed
    except json.JSONDecodeError:
        return status, raw


def tool_login(args):
    base_url = args.get("base_url", DEFAULT_BASE_URL)
    guard_base_url(base_url)
    STATE["base_url"] = base_url
    token = args["token"]
    url = base_url.rstrip("/") + "/api/session"
    req = urllib.request.Request(
        url,
        data=json.dumps({"token": token}).encode("utf-8"),
        method="POST",
        headers={
            "Content-Type": "application/json",
            "x-git-vista-protocol": PROTOCOL_VERSION,
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            status = resp.status
            set_cookie = resp.headers.get("Set-Cookie", "")
            raw = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        return {"status": e.code, "error": e.read().decode("utf-8", errors="replace")}
    match = re.search(r"gv_session=([^;]+)", set_cookie)
    if not match:
        return {"status": status, "error": "no session cookie in response", "body": raw}
    STATE["cookie"] = match.group(1)
    body = json.loads(raw)
    STATE["csrf"] = body.get("csrf")
    return {"status": status, "authenticated": body.get("authenticated"), "base_url": base_url}


def tool_request(args):
    method = args["method"].upper()
    path = args["path"]
    body = args.get("body")
    status, payload = http_call(method, path, body)
    return {"status": status, "body": payload}


def tool_routes(_args):
    return {"base_url": STATE["base_url"], "routes": [f"{m} {p}" for m, p in ROUTES]}


def tool_tail_log(args):
    lines = args.get("lines", 60)
    log_path = args.get("log_path", "/tmp/testbed-8082.log")
    try:
        with open(log_path, "r", errors="replace") as f:
            content = f.readlines()
    except OSError as e:
        return {"error": str(e)}
    return {"log_path": log_path, "lines": content[-lines:]}


TOOLS = {
    "gv_login": {
        "fn": tool_login,
        "description": (
            "Exchange a bootstrap token for a session cookie + CSRF token against "
            "the testbed. Call this first. base_url defaults to 127.0.0.1:8082 and "
            "any :8080 value is refused."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "token": {"type": "string", "description": "bootstrap token from ./dev signin"},
                "base_url": {"type": "string", "description": "defaults to http://127.0.0.1:8082"},
            },
            "required": ["token"],
        },
    },
    "gv_request": {
        "fn": tool_request,
        "description": (
            "Make an authenticated HTTP call against the testbed's real API using "
            "the session from gv_login. See gv_routes for valid (method, path) "
            "pairs and crates/git-vista-protocol/src/dto.rs for request/response "
            "body shapes."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "method": {"type": "string", "enum": ["GET", "POST", "DELETE"]},
                "path": {"type": "string", "description": "e.g. /api/commits"},
                "body": {"type": "object", "description": "JSON body for POST/DELETE calls"},
            },
            "required": ["method", "path"],
        },
    },
    "gv_routes": {
        "fn": tool_routes,
        "description": "List the real (method, path) route table this testbed serves.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    "gv_tail_log": {
        "fn": tool_tail_log,
        "description": "Read-only tail of the testbed's own server log.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "lines": {"type": "integer", "description": "default 60"},
                "log_path": {"type": "string", "description": "default /tmp/testbed-8082.log"},
            },
        },
    },
}


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def handle(msg):
    method = msg.get("method")
    msg_id = msg.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "gv-test-mcp", "version": "0.0.1-disposable"},
            },
        })
    elif method == "notifications/initialized":
        pass  # no response required
    elif method == "tools/list":
        tools = [
            {"name": name, "description": t["description"], "inputSchema": t["inputSchema"]}
            for name, t in TOOLS.items()
        ]
        send({"jsonrpc": "2.0", "id": msg_id, "result": {"tools": tools}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        tool = TOOLS.get(name)
        if tool is None:
            send({
                "jsonrpc": "2.0", "id": msg_id,
                "result": {"content": [{"type": "text", "text": f"unknown tool {name!r}"}], "isError": True},
            })
            return
        try:
            result = tool["fn"](args)
            text = json.dumps(result, indent=2)
            send({"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": text}], "isError": False}})
        except Exception as e:  # noqa: BLE001 - report to caller, keep server alive
            send({
                "jsonrpc": "2.0", "id": msg_id,
                "result": {"content": [{"type": "text", "text": f"{type(e).__name__}: {e}"}], "isError": True},
            })
    elif msg_id is not None:
        send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": f"unknown method {method!r}"}})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
