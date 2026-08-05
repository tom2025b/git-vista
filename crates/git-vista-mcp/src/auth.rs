//! Bootstrap-token authentication against the running git-vista-server —
//! exactly the way `gv` authenticates (M2.23a, #245).
//!
//! The flow mirrors the SPA's: read the `0600` one-time token, exchange it via
//! `POST /api/session` for an `HttpOnly` session cookie plus a CSRF token, and
//! reuse both for every later request. The token is **single-use and
//! self-replacing** — the server mints a fresh one into the same file the
//! moment one is spent — so this crate consuming a token never locks a human
//! out; it only rotates the file.
//!
//! # Where the secret is allowed to live
//!
//! The token is read from disk once and held in a local only long enough to be
//! sent; the session cookie and CSRF token live in [`Session`] fields in
//! memory. None of the three ever lands in argv, an environment variable, or
//! any file this crate writes — #245's acceptance criterion, load-bearing
//! because argv and env are world-visible on this box (`/proc/<pid>/cmdline`,
//! `/proc/<pid>/environ`) in ways a `0600` file is not.

use std::path::PathBuf;

use crate::http::{self, HttpResponse};

/// `$XDG_STATE_HOME/git-vista`, or `~/.local/state/git-vista` when unset —
/// **a deliberate mirror of `git-vista-server`'s `state::state_dir()`**, not an
/// import: #245 keeps this crate from linking the server crate, and the
/// resolution is three lines of stable XDG convention. If the server's ever
/// changes, the live integration test fails on the handshake, which is the
/// alarm we want.
fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir().join("git-vista-state"));
    base.join("git-vista")
}

/// Where the server writes the one-time bootstrap token, `0600`. Mirrors
/// `state::bootstrap_token_path()`.
pub fn bootstrap_token_path() -> PathBuf {
    state_dir().join("bootstrap.token")
}

/// An authenticated session: the cookie pair the server set, and the CSRF
/// token writes must echo. Memory only.
///
/// `Debug` is implemented by hand and **redacts both secrets** — a derived
/// impl would put the live cookie one `format!("{session:?}")` away from
/// landing in a tool-error string on stdout, and this crate's whole promise
/// is that the secrets live in memory and nowhere else.
#[derive(Clone)]
pub struct Session {
    /// The `gv_session=<id>` pair, exactly as it must appear in a `Cookie`
    /// header. Extracted from the exchange response's `Set-Cookie`.
    pub cookie: String,
    /// The per-session CSRF token (`x-git-vista-csrf` on state-changing
    /// requests). #245 carried it unused, ahead of need; #246's
    /// `select_repository` tool is the first consumer (`/api/select` sits
    /// behind the full write auth gate even though it's non-mutating — see
    /// `tools::select_repository`'s doc comment), and the write slices
    /// (#248/#249) reuse it after.
    pub csrf: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Destructured rather than field-accessed on purpose: a field added to
        // `Session` later is a **compile error right here** until somebody
        // decides how (or whether) it may be printed. Without this, a new
        // secret-bearing field would simply be absent from the rendering —
        // which looks safe — or, worse, someone would "fix" the omission by
        // deriving `Debug`. `tests::debugging_a_session_prints_neither_secret`
        // pins the behaviour; this pins that the impl stays in step with the
        // struct.
        let Session { cookie: _, csrf: _ } = self;
        f.debug_struct("Session")
            .field("cookie", &"gv_session=<redacted>")
            .field("csrf", &"<redacted>")
            .finish()
    }
}

/// Read the current bootstrap token and exchange it for a session.
///
/// Errors are strings meant for the MCP client's eyes (they become tool-call
/// errors); none of them ever embeds the token itself.
pub fn authenticate() -> Result<Session, String> {
    let path = bootstrap_token_path();
    let token = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "could not read the bootstrap token at {}: {e}. Is git-vista-server running? \
             (it writes a fresh token there on every start)",
            path.display()
        )
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!(
            "the bootstrap token file at {} is empty — the server may be mid-rotation; retry",
            path.display()
        ));
    }

    let body = serde_json::to_vec(&git_vista_protocol::SessionRequest {
        token: token.to_string(),
    })
    .map_err(|e| format!("could not encode the session request: {e}"));
    let body = body?;

    let resp = http::post_json("/api/session", &body, None, None)?;
    if resp.status != 200 {
        return Err(format!(
            "POST /api/session answered {} — the token may have expired (the server \
             refreshes it periodically; re-run, or restart via its service if this persists): {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }

    let info: git_vista_protocol::SessionInfo = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("could not parse the session response: {e}"))?;
    let csrf = info
        .csrf
        .ok_or("the server authenticated us but returned no CSRF token — protocol drift?")?;
    let cookie = session_cookie_pair(&resp)
        .ok_or("the server authenticated us but set no gv_session cookie — protocol drift?")?;

    Ok(Session { cookie, csrf })
}

/// Pull the `gv_session=<value>` pair out of the exchange response's
/// `Set-Cookie` headers, dropping the attributes (`HttpOnly; SameSite=Strict;
/// Path=/` and friends) that belong to the response, not to our next request.
/// Every `set-cookie` header is scanned, not just the first — today the
/// server sets exactly one, but ordering must never be able to break auth.
fn session_cookie_pair(resp: &HttpResponse) -> Option<String> {
    resp.headers
        .iter()
        .filter(|(n, _)| n == "set-cookie")
        .find_map(|(_, v)| {
            let pair = v.split(';').next()?.trim();
            (pair.starts_with("gv_session=") && pair.len() > "gv_session=".len())
                .then(|| pair.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The XDG mirror must prefer `$XDG_STATE_HOME` and fall back to
    /// `~/.local/state` — the same resolution the server uses. Env-var tests
    /// mutate process state, so both cases run in one test to avoid a race
    /// between parallel test threads.
    #[test]
    fn token_path_follows_the_servers_xdg_resolution() {
        // Env mutations are restored on every exit path so a future
        // env-reading test can't inherit this one's fake HOME and fail
        // nondeterministically by thread schedule.
        let orig_xdg = std::env::var_os("XDG_STATE_HOME");
        let orig_home = std::env::var_os("HOME");

        // Explicit XDG_STATE_HOME wins.
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-test-state");
        let with_xdg = bootstrap_token_path();
        // Empty XDG_STATE_HOME falls back to ~/.local/state, like the server's
        // `.filter(|p| !p.as_os_str().is_empty())`.
        std::env::set_var("XDG_STATE_HOME", "");
        std::env::set_var("HOME", "/tmp/home-test");
        let with_home = bootstrap_token_path();

        match orig_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match orig_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            with_xdg,
            PathBuf::from("/tmp/xdg-test-state/git-vista/bootstrap.token")
        );
        assert_eq!(
            with_home,
            PathBuf::from("/tmp/home-test/.local/state/git-vista/bootstrap.token")
        );
    }

    /// Every `.rs` file in this crate's `src/`, read from disk at test time.
    ///
    /// This used to be a hand-written `include_str!` list — and #248 added
    /// `plan_tools.rs` (1887 lines, a third of the crate) without updating it,
    /// so the guard below kept passing green while silently covering none of
    /// the file most likely to grow next. Enumerating the directory means the
    /// next file cannot be forgotten the same way; nothing here needs editing
    /// when one is added.
    fn crate_sources() -> Vec<(String, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {dir:?}: {e}"))
            .map(|entry| entry.expect("a readable directory entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .map(|p| {
                let name = p
                    .file_name()
                    .expect("a file, so it has a name")
                    .to_string_lossy()
                    .into_owned();
                let body =
                    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {p:?}: {e}"));
                (name, body)
            })
            .collect();
        sources.sort();
        sources
    }

    /// Anti-vacuity for the guard below: a directory read that found nothing
    /// (a moved `src/`, a wrong `CARGO_MANIFEST_DIR`, a filter that stopped
    /// matching) would make the scan pass by scanning zero bytes. The names
    /// here are a **floor**, not the list — a new file is picked up
    /// automatically and needs no edit.
    #[test]
    fn the_source_census_really_sees_every_file_in_the_crate() {
        let sources = crate_sources();
        let names: Vec<&str> = sources.iter().map(|(n, _)| n.as_str()).collect();
        for expected in ["auth.rs", "http.rs", "main.rs", "plan_tools.rs", "tools.rs"] {
            assert!(
                names.contains(&expected),
                "the source census missed {expected}: {names:?}"
            );
        }
        for (name, body) in &sources {
            assert!(
                body.len() > 500,
                "{name} was read as only {} bytes — the census is scanning nothing",
                body.len()
            );
        }
    }

    /// The #245 acceptance criterion — the token never lands in argv, env, or
    /// any file this crate writes — held structurally, not just by prose: the
    /// production half of every source file must stay free of the APIs that
    /// could violate it. A future slice adding one fails this named test and
    /// forces conscious review instead of slipping through.
    #[test]
    fn production_code_never_writes_files_env_or_spawns_processes() {
        let sources = crate_sources();
        let forbidden = [
            "fs::write",
            "File::create",
            "OpenOptions",
            "env::set_var",
            "Command::new",
        ];
        for (name, src) in &sources {
            let production = src.split("#[cfg(test)]").next().unwrap();
            for needle in forbidden {
                assert!(
                    !production.contains(needle),
                    "{name}: production code now contains `{needle}` — the #245 \
                     token-hygiene criterion needs re-review before this lands"
                );
            }
        }
    }

    /// The hand-written [`Session`] `Debug` impl is the only thing standing
    /// between the live session cookie and a `format!("{session:?}")` in some
    /// future error string — `tools.rs` already folds server error bodies into
    /// `ToolError::Execution(String)`, which the MCP host may log or show.
    /// The impl carried a doc comment saying exactly that, and nothing pinned
    /// it: replacing the two redactions with `&self.cookie` / `&self.csrf`
    /// left all 47 tests green.
    #[test]
    fn debugging_a_session_prints_neither_the_cookie_nor_the_csrf_token() {
        const COOKIE_SECRET: &str = "gv_session=CookieSecret0123456789";
        const CSRF_SECRET: &str = "CsrfSecret9876543210";
        let session = Session {
            cookie: COOKIE_SECRET.to_string(),
            csrf: CSRF_SECRET.to_string(),
        };
        let rendered = format!("{session:?}");

        assert!(
            !rendered.contains("CookieSecret0123456789"),
            "the live session cookie reached a Debug rendering: {rendered}"
        );
        assert!(
            !rendered.contains("CsrfSecret9876543210"),
            "the CSRF token reached a Debug rendering: {rendered}"
        );
        // Redacted, not merely absent: an impl that printed nothing at all
        // would pass the two assertions above while making every debug log
        // useless. Both fields must still be named, both must say so.
        assert!(rendered.contains("Session"), "{rendered}");
        assert!(rendered.contains("cookie"), "{rendered}");
        assert!(rendered.contains("csrf"), "{rendered}");
        assert_eq!(
            rendered.matches("<redacted>").count(),
            2,
            "both secrets must be redacted, not one: {rendered}"
        );

        // Anti-vacuity: these sentinels ARE Debug-visible strings, so their
        // absence above is this impl's doing and not an artefact of values
        // that could never have printed in the first place.
        let leaky = format!("{:?}", (&session.cookie, &session.csrf));
        assert!(
            leaky.contains("CookieSecret0123456789") && leaky.contains("CsrfSecret9876543210"),
            "the test's own sentinels do not survive Debug: {leaky}"
        );
    }

    #[test]
    fn the_session_cookie_pair_is_extracted_without_its_attributes() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![(
                "set-cookie".into(),
                "gv_session=abc123; HttpOnly; SameSite=Strict; Path=/".into(),
            )],
            body: Vec::new(),
        };
        assert_eq!(
            session_cookie_pair(&resp).as_deref(),
            Some("gv_session=abc123")
        );
    }

    #[test]
    fn a_response_without_the_gv_cookie_yields_none_rather_than_junk() {
        let none = HttpResponse {
            status: 200,
            headers: vec![("set-cookie".into(), "other=1; Path=/".into())],
            body: Vec::new(),
        };
        assert_eq!(session_cookie_pair(&none), None);
        let empty = HttpResponse {
            status: 200,
            headers: vec![("set-cookie".into(), "gv_session=".into())],
            body: Vec::new(),
        };
        assert_eq!(session_cookie_pair(&empty), None);
    }
}
