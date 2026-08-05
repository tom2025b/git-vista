//! A deliberately minimal HTTP/1.1 client over `TcpStream` (M2.23a, #245).
//!
//! # Why hand-rolled
//!
//! This crate talks to exactly one server — git-vista-server, loopback-only on
//! its fixed port — and calls two kinds of endpoint, both of which answer
//! small JSON bodies with an explicit `Content-Length` (axum always sets it
//! for these routes). A full HTTP client crate (reqwest, hyper-as-client)
//! would add a dependency tree that `docs/NATIVE_DEPENDENCIES.md`'s review
//! discipline would then carry forever, for capabilities (TLS, redirects,
//! pooling, proxies, chunked bodies) that a loopback JSON API never exercises.
//! ~120 lines of `std` beats that trade. If this crate ever needs to speak to
//! anything that is not this one loopback server, revisit — that is the line
//! where a real client earns its place.
//!
//! # Wire posture (mirrors the SPA's, per `security.rs`)
//!
//! - `Host: 127.0.0.1:<port>` — passes `HostPolicy::loopback`, which refuses
//!   anything that is not a loopback name literal (DNS-rebinding defence).
//! - **No `Origin` header, ever** — `security.rs` documents the same-origin
//!   exemption for requests that don't carry one; a non-browser client is
//!   exactly that case.
//! - `x-git-vista-protocol: <version>` on every request — the server refuses
//!   unversioned requests.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use git_vista_protocol::{PROTOCOL_HEADER, PROTOCOL_VERSION};

/// The server's fixed loopback endpoint. Mirrors `state::PORT` (8080), which
/// is a compile-time constant on the server side by design (loopback-only,
/// no env override) — so a mirror here is stable, not fragile.
const SERVER: &str = "127.0.0.1:8080";

/// A per-read-syscall bound (`SO_RCVTIMEO`), honestly named: `read_to_end`
/// restarts this clock on every chunk, so it bounds a *dead* peer, not a
/// slow-dripping one. Acceptable here because the peer is this box's own
/// loopback server (a slow-drip adversary is not in the threat model) and the
/// MCP client above the bridge carries its own tool-call timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A parsed response: status, lower-cased header pairs, raw body bytes.
///
/// `Debug` is implemented by hand and shows status, header *names* and body
/// length only: responses from `/api/session` carry the live session cookie
/// in `set-cookie`, and a derived impl would put it one `{resp:?}` away from
/// stdout. Same reasoning as [`crate::auth::Session`]'s redacted impl.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Destructured for the same reason as `auth::Session`'s impl: a field
        // added here later is a compile error in this function until somebody
        // decides whether it may be printed.
        let HttpResponse {
            status: _,
            headers: _,
            body: _,
        } = self;
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl HttpResponse {
    /// First header with this (already lower-case) name, if any.
    #[allow(dead_code)] // exercised by tests today; the write slices (#248/#249) read it next
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// `GET <path>` with the standing headers plus an optional session cookie.
pub fn get(path: &str, cookie: Option<&str>) -> Result<HttpResponse, String> {
    request("GET", path, None, cookie, None)
}

/// `POST <path>` with a JSON body, optional cookie, optional CSRF token.
pub fn post_json(
    path: &str,
    body: &[u8],
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> Result<HttpResponse, String> {
    request("POST", path, Some(body), cookie, csrf)
}

fn request(
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> Result<HttpResponse, String> {
    let mut stream = TcpStream::connect(SERVER)
        .map_err(|e| format!("could not connect to git-vista-server at {SERVER}: {e}"))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {SERVER}\r\n\
         {PROTOCOL_HEADER}: {PROTOCOL_VERSION}\r\n\
         Connection: close\r\n"
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

    stream
        .write_all(req.as_bytes())
        .and_then(|()| body.map_or(Ok(()), |b| stream.write_all(b)))
        .map_err(|e| format!("could not send the request to {SERVER}: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("could not read the response from {SERVER}: {e}"))?;
    parse_response(&raw)
}

/// Parse a full HTTP/1.1 response held in memory. `Connection: close` plus
/// `read_to_end` means the body is complete when this runs; `Content-Length`
/// (always present on the routes this crate calls) bounds it exactly, and is
/// honoured when present so trailing bytes can never leak into the body.
fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed HTTP response: no header terminator")?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "malformed HTTP response: non-UTF-8 headers")?;
    let mut lines = head.split("\r\n");

    let status_line = lines.next().ok_or("malformed HTTP response: empty")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed HTTP status line: {status_line:?}"))?;

    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (name, value) = l.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();

    let body_start = header_end + 4;
    let mut body = raw[body_start..].to_vec();
    if let Some(len) = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        if body.len() < len {
            return Err(format!(
                "truncated HTTP body: Content-Length {len}, got {}",
                body.len()
            ));
        }
        body.truncate(len);
    }

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_parses_into_status_headers_and_exact_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.body, b"{}");
    }

    #[test]
    fn content_length_bounds_the_body_even_with_trailing_bytes() {
        // `Connection: close` + read_to_end can pick up stray bytes on some
        // stacks; the declared length must win.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}garbage";
        assert_eq!(parse_response(raw).unwrap().body, b"{}");
    }

    #[test]
    fn a_truncated_body_is_an_error_not_a_short_read() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}";
        assert!(parse_response(raw).unwrap_err().contains("truncated"));
    }

    #[test]
    fn header_lookup_is_case_normalised() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\nSet-Cookie: gv_session=x\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 401);
        assert_eq!(r.header("set-cookie"), Some("gv_session=x"));
    }

    #[test]
    fn a_missing_header_terminator_is_refused() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    /// `/api/session`'s response carries the live session cookie in a
    /// `set-cookie` header and the CSRF token in its body, so this struct's
    /// hand-written `Debug` shows header *names* and a body *length* only.
    /// Nothing pinned that; a derived impl (or one field added back) would put
    /// both secrets one `{resp:?}` away from an error string on stdout.
    #[test]
    fn debugging_a_response_shows_no_header_value_and_no_body_bytes() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![(
                "set-cookie".into(),
                "gv_session=CookieSecret0123456789; HttpOnly".into(),
            )],
            body: br#"{"csrf":"CsrfSecret9876543210"}"#.to_vec(),
        };
        let rendered = format!("{resp:?}");
        assert!(
            !rendered.contains("CookieSecret0123456789"),
            "a set-cookie value reached a Debug rendering: {rendered}"
        );
        assert!(
            !rendered.contains("CsrfSecret9876543210"),
            "the response body reached a Debug rendering: {rendered}"
        );
        // Still useful, not merely empty: status, the header's NAME, and the
        // body's length are exactly what a debug line is for.
        assert!(rendered.contains("200"), "{rendered}");
        assert!(rendered.contains("set-cookie"), "{rendered}");
        assert!(
            rendered.contains(&resp.body.len().to_string()),
            "{rendered}"
        );
        // Anti-vacuity: the sentinels are ordinary Debug-visible strings.
        let leaky = format!("{:?}", (&resp.headers, String::from_utf8_lossy(&resp.body)));
        assert!(
            leaky.contains("CookieSecret0123456789") && leaky.contains("CsrfSecret9876543210"),
            "the test's own sentinels do not survive Debug: {leaky}"
        );
    }
}
