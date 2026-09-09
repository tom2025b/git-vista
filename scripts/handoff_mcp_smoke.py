#!/usr/bin/env python3
"""Smoke-test the installed handoff-mcp against its live config and real files.

Requires Python 3.11+ and handoff-mcp on PATH. Run:
    python3 scripts/handoff_mcp_smoke.py
    python3 scripts/handoff_mcp_smoke.py --binary /path/to/handoff-mcp --repo /path/to/repo

Config: ~/.config/handoff-mcp/config.toml (or $XDG_CONFIG_HOME/handoff-mcp/config.toml)
    repos = ["/home/tom/projects/Git-Vista"]

The fresh subprocess loads the current config. Existing MCP sessions need a
restart to see config changes. This calls only ho_list, with the configured DB;
it does not claim handoffs or write the board export. Exit 1 on empty results,
missing files, protocol errors, or a server timeout. Signed: codex.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib


def require(condition, message):
    if not condition:
        raise ValueError(message)


def smoke(binary, repo):
    root = repo.resolve(strict=True) / "design-docs" / "handoffs"
    require(root.is_dir(), f"handoff directory missing: {root}")
    require(any(root.rglob("*.md")), f"no real handoff markdown under {root}")
    xdg_config = Path(os.environ.get("XDG_CONFIG_HOME", ""))
    config_root = xdg_config if xdg_config.is_absolute() else Path.home() / ".config"
    config = config_root / "handoff-mcp" / "config.toml"
    with config.open("rb") as stream:
        settings = tomllib.load(stream)
    require(str(repo.resolve()) in [str(Path(p).expanduser().resolve())
                                  for p in settings.get("repos", [])],
            f"{repo} is absent from repos in {config}")

    # Same Discover lifecycle and per-request metadata as handoff-mcp's
    # tests/protocol_purity.rs; requests may receive replies in any order.
    meta = {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}}
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "server/discover",
         "params": {"_meta": meta}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
         "params": {"name": "ho_list", "arguments": {}, "_meta": meta}},
    ]
    child = subprocess.run(
        [binary], input="".join(json.dumps(r) + "\n" for r in requests),
        text=True, capture_output=True, timeout=15,
    )
    require(child.returncode == 0,
            f"server exited {child.returncode}: {child.stderr.strip()}")
    replies = {}
    for line in child.stdout.splitlines():
        reply = json.loads(line)
        require(isinstance(reply, dict), "expected JSON-RPC response object")
        request_id = reply.get("id")
        require(reply.get("jsonrpc") == "2.0" and request_id in (1, 2),
                f"unexpected protocol response: {reply}")
        require(request_id not in replies, f"duplicate response ID: {request_id}")
        require("error" not in reply and isinstance(reply.get("result"), dict),
                f"request {request_id} failed: {reply}")
        replies[request_id] = reply["result"]
    require(set(replies) == {1, 2}, f"missing replies; received IDs: {list(replies)}")
    require("2026-07-28" in replies[1].get("supportedVersions", []),
            "server does not support the Discover protocol used by this test")
    result = replies[2]
    require(not result.get("isError", False), f"ho_list tool failed: {result}")
    content = result.get("content", [])
    require(len(content) == 1 and content[0].get("type") == "text",
            f"expected one ho_list text result: {result}")
    rows = json.loads(content[0]["text"])
    require(isinstance(rows, list) and rows, "ho_list returned no handoffs")
    found = []
    for row in rows:
        require(isinstance(row, dict) and all(isinstance(row.get(k), str)
                for k in ("id", "path", "state")), f"invalid handoff row: {row}")
        path = Path(row["path"])
        require(path.is_absolute(), f"handoff path is not absolute: {row}")
        require(row["state"] in {"open", "claimed", "blocked", "done"},
                f"unknown handoff state: {row}")
        if path.resolve().is_relative_to(root):
            require(path.is_file() and path.suffix == ".md", f"missing handoff: {path}")
            require(row["id"].startswith(repo.resolve().name + "/"),
                    f"handoff ID does not name the expected repo: {row}")
            found.append(row)
    require(found, f"ho_list returned no real handoffs under {root}")
    print(f"PASS: ho_list returned {len(found)} real {repo.name} handoffs "
          f"({len(rows)} total), config: {config}")
    print(json.dumps(found, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="handoff-mcp")
    parser.add_argument("--repo", type=Path, default=Path.home() / "projects" / "Git-Vista")
    args = parser.parse_args()
    try:
        smoke(args.binary, args.repo)
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
