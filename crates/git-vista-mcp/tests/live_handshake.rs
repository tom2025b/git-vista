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

/// Direct-HTTP support for the baseline leg, compiled only for this test.
/// Lives here, not in src/, so the shipped binary carries no test surface.
mod git_vista_mcp_test_support {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    pub struct Session {
        pub cookie: String,
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
        let resp = raw("POST", "/api/session", Some(&body), None)?;
        let cookie = resp
            .1
            .iter()
            .find(|(n, _)| n == "set-cookie")
            .and_then(|(_, v)| v.split(';').next())
            .ok_or("no session cookie")?
            .to_string();
        Ok(Session { cookie })
    }

    pub fn get_catalog(s: &Session) -> Result<serde_json::Value, String> {
        let resp = raw("GET", "/api/catalog", None, Some(&s.cookie))?;
        serde_json::from_slice(&resp.2).map_err(|e| format!("{e}"))
    }

    #[allow(clippy::type_complexity)]
    fn raw(
        method: &str,
        path: &str,
        body: Option<&str>,
        cookie: Option<&str>,
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
