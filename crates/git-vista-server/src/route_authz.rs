//! #67 (M1.14): a structural gate that forces every route registered in
//! `main.rs` to carry an explicit authorization classification.
//!
//! `security.rs` already enforces authorization correctly at *runtime* — it
//! has tests proving a read needs a session, a write needs both session and
//! CSRF, and a bad Host/Origin is refused before anything else runs. That
//! work is not duplicated here. The gap this file closes is structural:
//! nothing notices when a *new* route is added. Someone writes
//! `.route("/api/new-thing", post(handler))` in `main.rs`, forgets to put it
//! behind the write layer, and every existing test still passes — because
//! every existing test exercises a route that already existed. The failure
//! is silent, and silent is how it ships.
//!
//! Same shape as [`crate::argv_boundary`]: a source scan, not a runtime
//! probe, asserting a structural fact about what `main.rs` actually
//! registers rather than what it is supposed to. Three layers:
//!
//!  1. Extract every `(path, method)` pair `api_router` registers, by
//!     text-scanning its own function body — no parser dependency, same
//!     posture as `argv_boundary`'s spawn-site scan.
//!  2. Check that set against [`ROUTE_AUTHZ`], the explicit classification
//!     table, in both directions: nothing registered is unclassified
//!     (catches a forgotten route), and nothing classified has quietly been
//!     deleted from `main.rs` (catches a rotten table).
//!  3. Pin the `Authz::Unauthenticated` allowlist to an exact, commented set
//!     — see [`EXPECTED_UNAUTHENTICATED`] — so a route landing there without
//!     a human explicitly arguing for it fails the build.

use axum::http::Method;
use std::path::Path;

/// The authorization a route must sit behind, per `security.rs`'s own
/// `require_auth` (see its module doc and the `session_exempt` check inside
/// it): `Unauthenticated` routes are exempt from the session requirement
/// entirely; `SessionRequired` reads need a live session; `SessionAndCsrf`
/// writes need a live session *and* a matching CSRF header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authz {
    Unauthenticated,
    SessionRequired,
    SessionAndCsrf,
}

/// Every route `api_router` registers, and the authorization it must sit
/// behind. Adding a route to `main.rs` without adding it here fails the
/// build — deliberately. Classifying a new route is a security decision,
/// and this table is where a human is forced to make it.
///
/// Ordered to match `main.rs`'s own registration order, so a diff between
/// the two reads the same way top to bottom.
const ROUTE_AUTHZ: &[(&str, Method, Authz)] = &[
    // -- always registered (both the loopback and LAN routers build these) --
    ("/api/frame", Method::GET, Authz::SessionRequired),
    ("/api/commits", Method::GET, Authz::SessionRequired),
    // Negotiation happens before a session can exist — this is how a client
    // learns which protocol version to speak in the first place.
    ("/api/protocol", Method::GET, Authz::Unauthenticated),
    ("/api/catalog", Method::GET, Authz::SessionRequired),
    // Checking whether a session exists cannot itself require one.
    ("/api/session", Method::GET, Authz::Unauthenticated),
    // Establishing a session (bootstrap token → cookie) is how one comes to
    // exist; it cannot require the thing it creates.
    ("/api/session", Method::POST, Authz::Unauthenticated),
    // Revoking a session is state-changing and is NOT in `security.rs`'s
    // `session_exempt` check (only GET/POST on this path are) — it needs a
    // live session and CSRF like any other write, which is the right call:
    // an unauthenticated client should not be able to end someone else's
    // session, and CSRF matters here exactly as much as for any other write.
    ("/api/session", Method::DELETE, Authz::SessionAndCsrf),
    ("/api/commit/{id}", Method::GET, Authz::SessionRequired),
    ("/api/diff/{id}", Method::GET, Authz::SessionRequired),
    (
        "/api/file/{id}/{*path}",
        Method::GET,
        Authz::SessionRequired,
    ),
    ("/api/head-branch", Method::GET, Authz::SessionRequired),
    ("/api/status", Method::GET, Authz::SessionRequired),
    // #68c: the generation-tagged WorktreeStatus DTO — same read posture as
    // the v1 endpoint immediately above.
    ("/api/status/v2", Method::GET, Authz::SessionRequired),
    // M2.21b (#236): the tag listing. A GET (no CSRF surface) and *not*
    // full_routes-gated: unlike `/api/staging/diff` above it discloses only
    // committed, published history — the same class of fact `/api/frame`'s ref
    // badges already carry — never working-tree contents, which is the line
    // ADR 0005 draws for what the LAN router may see.
    ("/api/tags", Method::GET, Authz::SessionRequired),
    ("/api/activity", Method::GET, Authz::SessionRequired),
    ("/api/undoables/{id}", Method::GET, Authz::SessionRequired),
    ("/api/rebase-status", Method::GET, Authz::SessionRequired),
    // -- registered only when `full_routes` is set (ADR 0005: never built at
    //    all on the LAN router, not merely gated) --
    ("/api/clone", Method::POST, Authz::SessionAndCsrf),
    // #263: a read of a clone attempt's outcome, same posture as the
    // `/api/operations/{id}` read below it — a GET, so no CSRF surface.
    (
        "/api/clone-status/{key}",
        Method::GET,
        Authz::SessionRequired,
    ),
    ("/api/delete-clone", Method::POST, Authz::SessionAndCsrf),
    ("/api/select", Method::POST, Authz::SessionAndCsrf),
    ("/api/rescan", Method::POST, Authz::SessionAndCsrf),
    ("/api/branch", Method::POST, Authz::SessionAndCsrf),
    ("/api/commit", Method::POST, Authz::SessionAndCsrf),
    // M2.19b (#223): amend rewrites the tip commit — a Destructive git write,
    // so the full write posture like every other mutation here.
    ("/api/amend-commit", Method::POST, Authz::SessionAndCsrf),
    ("/api/stage", Method::POST, Authz::SessionAndCsrf),
    // Staging selections (M2.17b, #213). The diff read is GET (no CSRF
    // surface) but still full_routes-only — it feeds the write surface and
    // shows uncommitted worktree contents, so the LAN router never sees it.
    ("/api/staging/diff", Method::GET, Authz::SessionRequired),
    ("/api/staging/preview", Method::POST, Authz::SessionAndCsrf),
    ("/api/staging/apply", Method::POST, Authz::SessionAndCsrf),
    ("/api/unstage", Method::POST, Authz::SessionAndCsrf),
    ("/api/undo", Method::POST, Authz::SessionAndCsrf),
    ("/api/merge", Method::POST, Authz::SessionAndCsrf),
    ("/api/push", Method::POST, Authz::SessionAndCsrf),
    // M2.20c (#229): fetch from a configured remote. A git write that opens a
    // socket with whatever credentials the host offers, which makes the CSRF
    // half of this classification load-bearing in a way it is not for a local
    // mutation: a cross-origin page that could trigger this would be making
    // *this server's* credentials talk to a remote.
    ("/api/fetch", Method::POST, Authz::SessionAndCsrf),
    // M2.20d (#230): pull is fetch's classification plus a local mutation —
    // it moves the checked-out branch and rewrites the working tree. Both
    // halves of the reasoning above apply, and the second one harder: a
    // cross-origin page that could trigger this would not merely make this
    // server's credentials talk to a remote, it would land whatever came back
    // on the user's branch.
    ("/api/pull", Method::POST, Authz::SessionAndCsrf),
    ("/api/delete-branch", Method::POST, Authz::SessionAndCsrf),
    ("/api/checkout", Method::POST, Authz::SessionAndCsrf),
    (
        "/api/force-delete-branch",
        Method::POST,
        Authz::SessionAndCsrf,
    ),
    ("/api/rebase", Method::POST, Authz::SessionAndCsrf),
    ("/api/reset-test-repo", Method::POST, Authz::SessionAndCsrf),
    // #219 (M2.18a): discard/delete of working-tree paths — destructive
    // writes, same posture as every other mutation above.
    (
        "/api/discard-tracked-paths",
        Method::POST,
        Authz::SessionAndCsrf,
    ),
    (
        "/api/delete-untracked-paths",
        Method::POST,
        Authz::SessionAndCsrf,
    ),
    // M2.23d (#248, ADR 0046): build a reviewable Plan and hand it back,
    // executing nothing. Classified `SessionAndCsrf` — the full write
    // posture — even though it mutates nothing: `security.rs`'s gate keys on
    // HTTP method, so a POST needs CSRF regardless, and the classification
    // should say what the route *is* rather than what it currently runs. A
    // plan carries an `OperationHash` that #249's submit stage accepts as
    // approval for exactly that mutation, so minting one is the front half of
    // a write and belongs at the write posture, on the loopback router only.
    ("/api/plan", Method::POST, Authz::SessionAndCsrf),
    ("/api/operations/{id}", Method::GET, Authz::SessionRequired),
    (
        "/api/operations/{id}/events",
        Method::GET,
        Authz::SessionRequired,
    ),
    // M2.20c (#229): cancelling a running operation changes what the server
    // does — it kills a child process — so it is a write, not a read of a
    // write's outcome like the two routes above it. Full write posture.
    (
        "/api/operations/{id}/cancel",
        Method::POST,
        Authz::SessionAndCsrf,
    ),
];

/// The total number of `(path, method)` pairs `api_router` should register
/// today. Pinned like `argv_boundary`'s "exactly four": a route silently
/// dropped by a `main.rs` refactor that this scanner's pattern-matching
/// doesn't recognise is exactly as much a regression as a route silently
/// added, and a bare membership check alone would miss the former.
const EXPECTED_ROUTE_COUNT: usize = 45;

/// The `Authz::Unauthenticated` allowlist, pinned to this exact set rather
/// than merely counted — each entry carries its own reason above in
/// [`ROUTE_AUTHZ`]. A route landing in this category is a security decision
/// serious enough that changing this constant is the explicit, visible act
/// of making it; it does not follow implicitly from the table growing.
const EXPECTED_UNAUTHENTICATED: &[(&str, Method)] = &[
    ("/api/protocol", Method::GET),
    ("/api/session", Method::GET),
    ("/api/session", Method::POST),
];

/// Blank `//` line comments (`//`, `///`, `//!` alike) to end-of-line,
/// leaving string literals — including their contents, where route paths
/// live — untouched. `main.rs`'s router section has no block comments and
/// no raw strings (the file's one raw string lives inside `#[cfg(test)] mod
/// tests`, well past where extraction below ever looks), so this narrower
/// version of `argv_boundary`'s `code_only` is enough here.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some((_, ch)) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }
        if ch == '/' && chars.peek().map(|&(_, c)| c) == Some('/') {
            for (_, c2) in chars.by_ref() {
                if c2 == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// The brace-balanced body of the one production `fn api_router(...)` in
/// `code` (already passed through [`strip_line_comments`]). Mirrors
/// `argv_boundary::production_body`: exactly one definition must exist,
/// found before `mod tests` starts, so a same-named test helper can't be
/// picked up instead and the scan can't go ambiguous.
fn api_router_body(code: &str) -> &str {
    let marker = "fn api_router(";
    let defs = code.matches(marker).count();
    assert_eq!(
        defs, 1,
        "expected exactly one `{marker}` definition in main.rs, found {defs}"
    );
    let at = code.find(marker).expect("counted above");
    let tests_at = code.find("mod tests").expect("main.rs has a test module");
    assert!(
        at < tests_at,
        "`{marker}` was found inside `mod tests`, not in production code"
    );

    let open = at
        + code[at..]
            .find('{')
            .expect("a function signature is followed by its body brace");
    let mut depth = 0usize;
    for (offset, ch) in code[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces while extracting `fn api_router`");
}

/// Every `(path, method)` pair registered by `.route(...)` calls inside
/// `body`. Handles both the ordinary one-method-per-call shape
/// (`.route("/x", get(h))`) and the one multi-method chain in this codebase
/// (`.route("/api/session", get(a).post(b).delete(c))`) — the same
/// balanced-parens technique `argv_boundary::production_body` uses to find
/// a function body, applied to each `.route(` call's own argument list.
fn extract_registered_routes(body: &str) -> Vec<(String, Method)> {
    let mut out = Vec::new();
    let needle = ".route(";
    let mut search_from = 0usize;
    while let Some(rel) = body[search_from..].find(needle) {
        let start = search_from + rel + needle.len();
        let mut depth = 1usize;
        let mut end = None;
        for (offset, ch) in body[start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.expect("unbalanced parens scanning a `.route(` call");
        let span = &body[start..end];

        let q1 = span
            .find('"')
            .unwrap_or_else(|| panic!("a `.route(` call names no path string: {span:?}"));
        let rest = &span[q1 + 1..];
        let q2 = rest
            .find('"')
            .unwrap_or_else(|| panic!("unterminated path string in a `.route(` call: {span:?}"));
        let path = rest[..q2].to_string();

        // Whole-word matches only: the char immediately before the match, if
        // any, must not be an identifier char — guards against a coincidental
        // substring hit (e.g. some future handler literally ending in
        // "target(" would otherwise look like a `get(` call).
        for (word, method) in [
            ("get", Method::GET),
            ("post", Method::POST),
            ("put", Method::PUT),
            ("patch", Method::PATCH),
            ("delete", Method::DELETE),
        ] {
            let word_paren = format!("{word}(");
            let mut from = 0usize;
            while let Some(rel2) = span[from..].find(word_paren.as_str()) {
                let idx = from + rel2;
                let boundary_ok = idx == 0 || {
                    let prev = span.as_bytes()[idx - 1];
                    !(prev.is_ascii_alphanumeric() || prev == b'_')
                };
                if boundary_ok {
                    out.push((path.clone(), method.clone()));
                }
                from = idx + word_paren.len();
            }
        }

        search_from = end + 1;
    }
    out
}

fn registered_routes() -> Vec<(String, Method)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&path).expect("readable main.rs");
    let code = strip_line_comments(&src);
    extract_registered_routes(api_router_body(&code))
}

/// Assertion 1 (the one that catches the actual regression class): every
/// route `main.rs` registers has an entry in [`ROUTE_AUTHZ`]. A route
/// missing from the table fails loudly, naming the exact route and what to
/// do about it — not `assertion failed: left == right`, which teaches
/// nobody anything at 2am.
///
/// The count pin catches the case a plain membership check would miss: a
/// route silently *dropped* from extraction by a `main.rs` shape this
/// scanner's patterns don't recognise, which would otherwise let the table
/// go stale without ever failing.
#[test]
fn every_registered_route_is_classified() {
    let routes = registered_routes();
    assert_eq!(
        routes.len(),
        EXPECTED_ROUTE_COUNT,
        "main.rs's api_router now registers {} (path, method) pairs, expected {} — \
         if you added a route, classify it in ROUTE_AUTHZ and bump EXPECTED_ROUTE_COUNT; \
         if this dropped, the scanner in route_authz.rs may no longer recognise a \
         `.route(...)` shape main.rs uses",
        routes.len(),
        EXPECTED_ROUTE_COUNT
    );

    for (path, method) in &routes {
        let classified = ROUTE_AUTHZ
            .iter()
            .any(|(p, m, _)| *p == path.as_str() && m == method);
        assert!(
            classified,
            "NEW ROUTE NOT CLASSIFIED: {method} {path} was added to main.rs's \
             api_router but has no entry in ROUTE_AUTHZ (crates/git-vista-server/src/route_authz.rs). \
             Classify it: add (\"{path}\", Method::{method}, Authz::<level>) to ROUTE_AUTHZ \
             with a comment explaining the choice. Reads need at least SessionRequired; \
             writes need SessionAndCsrf; Unauthenticated is reserved for the pinned \
             pre-session allowlist and needs its own argued entry in EXPECTED_UNAUTHENTICATED too."
        );
    }
}

/// Assertion 2 (the table must not rot): every entry in [`ROUTE_AUTHZ`]
/// still names a route `main.rs` actually registers. Otherwise the table
/// accumulates routes deleted years ago, and a table nobody trusts stops
/// being read.
#[test]
fn every_classified_route_still_exists_in_main_rs() {
    let routes = registered_routes();
    for (path, method, _authz) in ROUTE_AUTHZ {
        let still_registered = routes
            .iter()
            .any(|(p, m)| p.as_str() == *path && m == method);
        assert!(
            still_registered,
            "STALE TABLE ENTRY: ROUTE_AUTHZ classifies {method} {path} but main.rs's \
             api_router no longer registers it. Remove the entry from ROUTE_AUTHZ \
             (crates/git-vista-server/src/route_authz.rs) — a route that no longer exists \
             cannot be a live security decision."
        );
    }
}

/// Assertion 3: `Authz::Unauthenticated` is a short, pinned, individually
/// justified allowlist — not something that can grow by a route merely being
/// classified that way in [`ROUTE_AUTHZ`] without also updating
/// [`EXPECTED_UNAUTHENTICATED`], whose entries each carry a comment (see
/// `ROUTE_AUTHZ` above) arguing why that specific route is safe to reach
/// without a session.
#[test]
fn unauthenticated_routes_are_a_pinned_short_allowlist() {
    let actual: Vec<(&str, Method)> = ROUTE_AUTHZ
        .iter()
        .filter(|(_, _, authz)| *authz == Authz::Unauthenticated)
        .map(|(p, m, _)| (*p, m.clone()))
        .collect();

    assert_eq!(
        actual.len(),
        EXPECTED_UNAUTHENTICATED.len(),
        "ROUTE_AUTHZ now classifies {} routes as Unauthenticated, expected exactly {}. \
         A route landing on the pre-session allowlist is a security decision: update \
         EXPECTED_UNAUTHENTICATED in route_authz.rs with the same entry, plus a comment \
         on its ROUTE_AUTHZ row saying why it is safe to reach without a session.",
        actual.len(),
        EXPECTED_UNAUTHENTICATED.len()
    );
    for expected in EXPECTED_UNAUTHENTICATED {
        assert!(
            actual
                .iter()
                .any(|(p, m)| *p == expected.0 && *m == expected.1),
            "expected {} {} to be classified Unauthenticated in ROUTE_AUTHZ, but it isn't \
             (or was reclassified without updating EXPECTED_UNAUTHENTICATED)",
            expected.1,
            expected.0
        );
    }
}
