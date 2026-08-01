//! The #245 acceptance test: the full handshake against a REAL running
//! git-vista-server — read token → POST /api/session → authenticated tool
//! call — not a mock.
//!
//! # Why `#[ignore]` (the `sandbox::clone_live` precedent)
//!
//! The server's port is a compile-time constant (8080, loopback-only, no env
//! override — a deliberate security posture), so a test cannot spawn a
//! private instance beside a running one, and CI has no server at all. Like
//! `clone_live`, this test is therefore ignored by default and run explicitly
//! on the dev box, where the real server is a systemd service:
//!
//! ```text
//! cargo test -p git-vista-mcp --test live_handshake -- --ignored
//! ```
//!
//! Consuming the bootstrap token here is safe by design: it is single-use and
//! self-replacing — the server mints a fresh one into the same file the moment
//! one is spent — so a human's next `gv --token` link still works. The only
//! side effect is rotation.
//!
//! Every test below this point is a pure read with no side effect on the
//! shared server — **except `select_repository_round_trips_against_the_real_catalog`**,
//! which changes the server's live current-selection state and does not
//! restore it (review finding, #246: no read endpoint exposes the
//! previously-selected worktree/mode to restore *to*, so a faithful
//! save-and-restore isn't cleanly achievable without new server API — out of
//! this test's scope). Running the whole file with `--ignored` off-hours
//! will leave whatever repository/mode that one test selected as the
//! server's current selection afterward.

use std::process::{Command, Stdio};

/// Drive the compiled bridge binary exactly the way an MCP client does:
/// stdio, one JSON-RPC message per line. This is deliberately end-to-end —
/// binary, framing, auth, HTTP, server — because the unit tests already cover
/// each piece in isolation and the gap #260 taught is always *between* the
/// proven pieces.
#[test]
#[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored"]
fn the_full_handshake_lists_the_same_catalog_the_http_api_returns() {
    use std::io::{BufRead, BufReader, Write};

    // Baseline leg first (clone_live's paired-baseline pattern): fetch the
    // catalog over plain HTTP ourselves. If THIS fails, the server or token
    // is the problem, not the MCP bridge — the test says which leg died.
    let session = git_vista_mcp_test_support::authenticate_for_test()
        .expect("baseline: could not authenticate directly — is the server running?");
    let baseline = git_vista_mcp_test_support::get_catalog(&session)
        .expect("baseline: GET /api/catalog failed against the live server");
    assert!(
        baseline.is_array(),
        "baseline: /api/catalog did not return an array"
    );

    // Bridge leg: spawn the real binary, speak real MCP at it.
    let exe = env!("CARGO_BIN_EXE_git-vista-mcp");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("could not spawn the bridge binary");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut lines = stdout.lines();

    let mut send = |msg: &str| {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };

    send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let init: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "git-vista-mcp");

    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_repositories","arguments":{}}}"#,
    );
    let call: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();

    assert_eq!(
        call["result"]["isError"], false,
        "the bridge's tool call failed while the direct HTTP baseline succeeded \
         moments earlier — so this is the bridge, not the server: {}",
        call["result"]["content"][0]["text"]
    );
    let via_bridge: serde_json::Value =
        serde_json::from_str(call["result"]["content"][0]["text"].as_str().unwrap())
            .expect("the bridge's catalog payload was not JSON");

    // The acceptance criterion verbatim: the tool returns the same catalog
    // JSON GET /api/catalog would return directly. Full JSON-value equality,
    // order included — deterministic because the server serves the catalog in
    // stable registration order. The catalog CAN legitimately move between
    // the two legs (this is the live server; another client may clone or
    // select mid-test), so on mismatch the baseline is fetched once more:
    // "catalog moved" must read as exactly that, never as "bridge broken".
    if via_bridge != baseline {
        let refreshed = git_vista_mcp_test_support::get_catalog(&session)
            .expect("re-baseline: GET /api/catalog failed");
        assert_eq!(
            via_bridge, refreshed,
            "the bridge's catalog differs from the direct HTTP catalog even \
             after re-fetching the baseline — this is the bridge, not a \
             mid-test catalog change"
        );
    }

    drop(stdin); // close its stdin so the loop exits...
    let status = child.wait().expect("bridge did not exit");
    assert!(status.success(), "the bridge exited non-zero");
}

/// The #246 baseline-then-bridge cases: one per new read tool, extending the
/// #245 pattern above rather than replacing it. Each fetches the same
/// endpoint two ways — direct HTTP (the oracle) and through the compiled
/// bridge binary over stdio MCP — and asserts the JSON matches. See the
/// module doc above (and `tools.rs`) for why `get_commit_detail` and
/// `get_commit_diff` are separate tools, and why `get_graph` returns exactly
/// one page.
///
/// `#[ignore]` for the same reason as the #245 case: needs the real
/// `git-vista-server` on `127.0.0.1:8080`. **Never run these with
/// `--ignored`** against the box's live server — that server is serving a
/// real iPad session; these are written and ready for a human to run
/// explicitly, later, off-hours.
mod read_tools {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    use super::git_vista_mcp_test_support as support;

    /// Spawn the bridge, complete `initialize`/`notifications/initialized`,
    /// and return a live stdin/stdout pair ready for `tools/call`. Shared by
    /// every case below so each test is just "call one tool, compare."
    struct Bridge {
        stdin: std::process::ChildStdin,
        lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
        child: std::process::Child,
    }

    impl Bridge {
        fn spawn() -> Self {
            let exe = env!("CARGO_BIN_EXE_git-vista-mcp");
            let mut child = Command::new(exe)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("could not spawn the bridge binary");
            let mut stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());
            let mut lines = stdout.lines();

            let send = |stdin: &mut std::process::ChildStdin, msg: &str| {
                stdin.write_all(msg.as_bytes()).unwrap();
                stdin.write_all(b"\n").unwrap();
                stdin.flush().unwrap();
            };
            send(
                &mut stdin,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            );
            let _init = lines.next().unwrap().unwrap();
            send(
                &mut stdin,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            );
            Bridge {
                stdin,
                lines,
                child,
            }
        }

        /// One `tools/call`, returning the parsed `result.content[0].text`
        /// JSON on success — panics with the tool's own error text
        /// otherwise, so a failing assertion in a test using this shows
        /// *why* the bridge failed, not just that it did.
        fn call(&mut self, id: u64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
            let msg = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            });
            self.stdin.write_all(msg.to_string().as_bytes()).unwrap();
            self.stdin.write_all(b"\n").unwrap();
            self.stdin.flush().unwrap();
            let line = self.lines.next().unwrap().unwrap();
            let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                reply["result"]["isError"], false,
                "{name} failed: {}",
                reply["result"]["content"][0]["text"]
            );
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("{name}'s payload was not JSON: {e}"))
        }

        fn finish(mut self) {
            drop(self.stdin);
            let status = self.child.wait().expect("bridge did not exit");
            assert!(status.success(), "the bridge exited non-zero");
        }
    }

    #[test]
    #[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored"]
    fn get_graph_matches_frame_and_first_commits_page() {
        let session = support::authenticate_for_test()
            .expect("baseline: could not authenticate — is the server running?");
        let frame_baseline =
            support::get_json(&session, "/api/frame").expect("baseline: GET /api/frame failed");
        let page_baseline =
            support::get_json(&session, "/api/commits").expect("baseline: GET /api/commits failed");

        let mut bridge = Bridge::spawn();
        let via_bridge = bridge.call(2, "get_graph", serde_json::json!({}));
        bridge.finish();

        assert_eq!(via_bridge["frame"], frame_baseline, "frame half mismatched");
        assert_eq!(via_bridge["page"], page_baseline, "page half mismatched");
    }

    #[test]
    #[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored"]
    fn get_commit_detail_and_get_commit_diff_match_their_direct_endpoints() {
        let session = support::authenticate_for_test()
            .expect("baseline: could not authenticate — is the server running?");
        let page =
            support::get_json(&session, "/api/commits").expect("baseline: GET /api/commits failed");
        let id = page["rows"][0]["commit"]["id"]
            .as_str()
            .expect("baseline: /api/commits returned no rows to pick a commit id from")
            .to_string();

        let detail_baseline = support::get_json(&session, &format!("/api/commit/{id}"))
            .expect("baseline: GET /api/commit/{id} failed");
        let diff_baseline = support::get_json(&session, &format!("/api/diff/{id}"))
            .expect("baseline: GET /api/diff/{id} failed");

        let mut bridge = Bridge::spawn();
        let detail_via_bridge =
            bridge.call(2, "get_commit_detail", serde_json::json!({ "id": id }));
        let diff_via_bridge = bridge.call(3, "get_commit_diff", serde_json::json!({ "id": id }));
        bridge.finish();

        assert_eq!(detail_via_bridge, detail_baseline);
        assert_eq!(diff_via_bridge, diff_baseline);
    }

    #[test]
    #[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored"]
    fn get_status_matches_the_v2_endpoint_not_v1() {
        let session = support::authenticate_for_test()
            .expect("baseline: could not authenticate — is the server running?");
        let v2_baseline = support::get_json(&session, "/api/status/v2")
            .expect("baseline: GET /api/status/v2 failed");
        // The v1 shape the tool must NOT match — asserted distinct so a
        // regression that wires get_status to v1 by mistake fails loudly
        // rather than passing by coincidence (v1 and v2 do share some field
        // names).
        let v1_baseline = support::get_json(&session, "/api/status")
            .expect("baseline: GET /api/status (v1) failed");

        let mut bridge = Bridge::spawn();
        let via_bridge = bridge.call(2, "get_status", serde_json::json!({}));
        bridge.finish();

        assert_eq!(via_bridge, v2_baseline);
        assert!(
            via_bridge.get("generation").is_some(),
            "get_status must return the generation-tagged v2 shape"
        );
        assert_ne!(
            via_bridge, v1_baseline,
            "get_status returned the legacy v1 shape, not v2"
        );
    }

    #[test]
    #[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored"]
    fn get_activity_matches_the_direct_endpoint() {
        let session = support::authenticate_for_test()
            .expect("baseline: could not authenticate — is the server running?");
        let baseline = support::get_json(&session, "/api/activity")
            .expect("baseline: GET /api/activity failed");

        let mut bridge = Bridge::spawn();
        let via_bridge = bridge.call(2, "get_activity", serde_json::json!({}));
        bridge.finish();

        assert_eq!(via_bridge, baseline);
    }

    #[test]
    #[ignore = "needs the real git-vista-server running on 127.0.0.1:8080; run with --ignored. \
                STATE-CHANGING (unlike this file's other tests): moves the server's live \
                current selection to the catalog's first repository in visualize mode, and \
                does not restore whatever was selected before — see the module doc."]
    fn select_repository_round_trips_against_the_real_catalog() {
        let session = support::authenticate_for_test()
            .expect("baseline: could not authenticate — is the server running?");
        let catalog =
            support::get_json(&session, "/api/catalog").expect("baseline: GET /api/catalog failed");
        let entry = catalog
            .as_array()
            .and_then(|a| a.first())
            .expect("baseline: /api/catalog returned no repositories to select");
        let worktree = entry["worktree_id"]
            .as_str()
            .or_else(|| entry["id"].as_str())
            .expect("baseline: catalog entry had no worktree/id field to select by")
            .to_string();

        let baseline_body = support::post_text(
            &session,
            "/api/select",
            &serde_json::json!({ "worktree": worktree, "mode": "visualize" }),
        )
        .expect("baseline: POST /api/select failed");

        let mut bridge = Bridge::spawn();
        let via_bridge = bridge.call(
            2,
            "select_repository",
            serde_json::json!({ "worktree": worktree, "mode": "visualize" }),
        );
        bridge.finish();

        // Both legs select the same repository in the same mode, so both
        // get the server's same confirmation text back.
        assert_eq!(via_bridge.as_str(), Some(baseline_body.as_str()));
    }
}

/// Direct-HTTP support for the baseline leg, compiled only for this test.
/// Lives here, not in src/, so the shipped binary carries no test surface.
mod git_vista_mcp_test_support {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    pub struct Session {
        pub cookie: String,
        pub csrf: String,
    }

    pub fn authenticate_for_test() -> Result<Session, String> {
        let path = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
            })
            .ok_or("no HOME")?
            .join("git-vista/bootstrap.token");
        let token = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
        // Real JSON encoding, not string interpolation: today's tokens are
        // lowercase hex, but the baseline leg must never silently send
        // malformed JSON if the token format ever changes.
        let body = serde_json::json!({ "token": token.trim() }).to_string();
        let resp = raw("POST", "/api/session", Some(&body), None, None)?;
        let cookie = resp
            .1
            .iter()
            .find(|(n, _)| n == "set-cookie")
            .and_then(|(_, v)| v.split(';').next())
            .ok_or("no session cookie")?
            .to_string();
        let info: serde_json::Value =
            serde_json::from_slice(&resp.2).map_err(|e| format!("{e}"))?;
        let csrf = info
            .get("csrf")
            .and_then(|c| c.as_str())
            .ok_or("session response carried no csrf token")?
            .to_string();
        Ok(Session { cookie, csrf })
    }

    pub fn get_catalog(s: &Session) -> Result<serde_json::Value, String> {
        get_json(s, "/api/catalog")
    }

    /// `GET path` with the session cookie, parsed as JSON — the shared
    /// baseline-leg fetch every #246 read case above uses.
    pub fn get_json(s: &Session, path: &str) -> Result<serde_json::Value, String> {
        let resp = raw("GET", path, None, Some(&s.cookie), None)?;
        if resp.0 != 200 {
            return Err(format!(
                "GET {path} answered {}: {}",
                resp.0,
                String::from_utf8_lossy(&resp.2)
            ));
        }
        serde_json::from_slice(&resp.2).map_err(|e| format!("{e}"))
    }

    /// `POST path` with the session cookie AND csrf token, returning the raw
    /// response text (not JSON — `/api/select`'s own response is plain
    /// confirmation text, same as `tools::select_repository` documents).
    pub fn post_text(s: &Session, path: &str, body: &serde_json::Value) -> Result<String, String> {
        let resp = raw(
            "POST",
            path,
            Some(&body.to_string()),
            Some(&s.cookie),
            Some(&s.csrf),
        )?;
        if resp.0 != 200 {
            return Err(format!(
                "POST {path} answered {}: {}",
                resp.0,
                String::from_utf8_lossy(&resp.2)
            ));
        }
        Ok(String::from_utf8_lossy(&resp.2).into_owned())
    }

    #[allow(clippy::type_complexity)]
    fn raw(
        method: &str,
        path: &str,
        body: Option<&str>,
        cookie: Option<&str>,
        csrf: Option<&str>,
    ) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
        let mut s = TcpStream::connect("127.0.0.1:8080").map_err(|e| format!("{e}"))?;
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n{}: {}\r\nConnection: close\r\n",
            git_vista_protocol::PROTOCOL_HEADER,
            git_vista_protocol::PROTOCOL_VERSION
        );
        if let Some(c) = cookie {
            req.push_str(&format!("Cookie: {c}\r\n"));
        }
        if let Some(t) = csrf {
            req.push_str(&format!("{}: {t}\r\n", git_vista_protocol::CSRF_HEADER));
        }
        if let Some(b) = body {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).map_err(|e| format!("{e}"))?;
        if let Some(b) = body {
            s.write_all(b.as_bytes()).map_err(|e| format!("{e}"))?;
        }
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).map_err(|e| format!("{e}"))?;
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("malformed")?;
        let head = String::from_utf8_lossy(&raw[..end]).to_string();
        let mut lines = head.split("\r\n");
        let status: u16 = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .ok_or("bad status")?;
        let headers = lines
            .filter_map(|l| {
                let (n, v) = l.split_once(':')?;
                Some((n.trim().to_ascii_lowercase(), v.trim().to_string()))
            })
            .collect();
        Ok((status, headers, raw[end + 4..].to_vec()))
    }
}
