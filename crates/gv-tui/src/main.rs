//! gv-tui — git-vista's terminal UI. This slice (M10.01, #456) is the crate
//! skeleton: authenticate against the running `git-vista-server` and print
//! one read from it. No rendering, no writes, no keybindings — deliberately.
//!
//! # What this slice exists to prove
//!
//! That the session boundary works from a terminal process. The hard part of
//! a non-browser client is auth, and it was already solved for the MCP
//! bridge (M2.23a, #245); #456 moved that solution into `git-vista-session`
//! and this binary consumes it — same one-time token, same
//! `POST /api/session` exchange, same in-memory-only cookie + CSRF
//! discipline.
//!
//! Consuming the bootstrap token here never locks a human out: the token is
//! single-use and **self-replacing** — the server mints a fresh one into the
//! same `0600` file the moment one is spent (see `git_vista_session::auth`'s
//! module doc, which this crate mirrors rather than re-deciding).
//!
//! # The one read: `GET /api/catalog`
//!
//! The issue offers "`/api/status` or equivalent". The catalog is the
//! equivalent this slice reads, for the same reason `git-vista-mcp`'s first
//! tool was `list_repositories`: it answers on a fresh server regardless of
//! whether any repository has been selected yet, so the only thing this
//! slice's success depends on is the thing it exists to prove — the session
//! boundary. `/api/status` additionally depends on the server's selection
//! state, which is the working-tree pane's concern (#459), not auth's.
//!
//! # Failure posture
//!
//! Every failure — token file missing, token already spent by a race, server
//! down, non-200 answer, malformed body — is a clear one-line message on
//! stderr and a non-zero exit. Never a panic, never a silent hang
//! (`git-vista-session`'s socket timeouts bound a dead peer). A one-shot
//! process re-authenticates on its next run, so the 401-retry loop
//! `git-vista-mcp`'s `authed_fetch` carries for its long-lived bridge is
//! deliberately absent here; it lifts into `git-vista-session` when the
//! first persistent-pane slice (#457) needs it.

use git_vista_protocol::{RepositoryDescriptor, RepositoryKind};
use git_vista_session::{auth, http};

const CATALOG_PATH: &str = "/api/catalog";

fn main() -> std::process::ExitCode {
    match run(&mut auth::authenticate, &mut |path, cookie| {
        http::get(path, Some(cookie))
    }) {
        Ok(report) => {
            println!("{report}");
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("gv-tui: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Authenticate, read the catalog, render a one-screen report.
///
/// Generic over the auth and fetch closures so every arm — auth refused,
/// non-200 answer, malformed body, success — is unit-testable without a
/// server, the same seam shape `git-vista-mcp`'s `authed_fetch` uses.
/// Production passes `auth::authenticate` and `http::get`.
fn run(
    auth: &mut dyn FnMut() -> Result<auth::Session, String>,
    fetch: &mut dyn FnMut(&str, &str) -> Result<http::HttpResponse, String>,
) -> Result<String, String> {
    let session = auth()?;
    let resp = fetch(CATALOG_PATH, &session.cookie)?;
    if resp.status != 200 {
        return Err(format!(
            "GET {CATALOG_PATH} answered {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        ));
    }
    let catalog: Vec<RepositoryDescriptor> = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("{CATALOG_PATH} did not return a valid catalog: {e}"))?;
    Ok(render_catalog(&catalog))
}

/// The human-readable report for one catalog. Pure, so tests pin the exact
/// rendering with no session in sight.
fn render_catalog(catalog: &[RepositoryDescriptor]) -> String {
    let mut out = format!(
        "authenticated to git-vista-server — {} repositor{} in the catalog",
        catalog.len(),
        if catalog.len() == 1 { "y" } else { "ies" }
    );
    for repo in catalog {
        let kind = match repo.kind {
            RepositoryKind::Bare => "bare",
            RepositoryKind::MainWorktree => "main worktree",
            RepositoryKind::LinkedWorktree => "linked worktree",
        };
        let read_only = if repo.read_only { ", read-only" } else { "" };
        out.push_str(&format!("\n  {} ({kind}{read_only})", repo.name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_session::http::HttpResponse;

    fn test_session() -> auth::Session {
        auth::Session {
            cookie: "gv_session=test-cookie".into(),
            csrf: "test-csrf".into(),
        }
    }

    /// The catalog exactly as the server serializes it — a hand-written wire
    /// literal, NOT produced by serializing the DTO, because a literal is
    /// what pins the `snake_case` kind encoding and optional-field omission
    /// against drift. (Serializing `RepositoryDescriptor` here would assert
    /// `parse(serialize(x)) == x`, which serde guarantees vacuously.)
    const WIRE_CATALOG: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"demo","kind":"main_worktree","read_only":false},
      {"repository":"r2","worktree":"w2","name":"mirror","kind":"bare","read_only":true}
    ]"#;

    #[test]
    fn an_auth_failure_surfaces_and_nothing_is_fetched() {
        let mut fetched = false;
        let err = run(
            &mut || Err("could not read the bootstrap token at /nope: gone".into()),
            &mut |_, _| {
                fetched = true;
                Err("unreachable".into())
            },
        )
        .unwrap_err();
        assert!(err.contains("bootstrap token"), "{err}");
        assert!(
            !fetched,
            "a fetch went out with no session — the auth gate is not in front"
        );
    }

    #[test]
    fn a_non_200_answer_names_the_status_and_the_servers_own_words() {
        let err = run(&mut || Ok(test_session()), &mut |_, _| {
            Ok(HttpResponse {
                status: 503,
                headers: vec![],
                body: b"catalog rebuilding".to_vec(),
            })
        })
        .unwrap_err();
        assert!(err.contains("503"), "{err}");
        assert!(err.contains("catalog rebuilding"), "{err}");
    }

    #[test]
    fn a_malformed_body_is_a_clear_error_never_a_panic() {
        let err = run(&mut || Ok(test_session()), &mut |_, _| {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: b"<html>definitely not the catalog</html>".to_vec(),
            })
        })
        .unwrap_err();
        assert!(err.contains(CATALOG_PATH), "{err}");
    }

    #[test]
    fn a_catalog_read_fetches_with_the_session_cookie_and_reports_every_entry() {
        let mut seen = None;
        let report = run(&mut || Ok(test_session()), &mut |path, cookie| {
            seen = Some((path.to_string(), cookie.to_string()));
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: WIRE_CATALOG.as_bytes().to_vec(),
            })
        })
        .unwrap();
        assert_eq!(
            seen,
            Some((
                CATALOG_PATH.to_string(),
                "gv_session=test-cookie".to_string()
            )),
            "the read must go to the catalog, carrying the session cookie"
        );
        assert!(report.contains("2 repositories"), "{report}");
        assert!(report.contains("demo (main worktree)"), "{report}");
        assert!(report.contains("mirror (bare, read-only)"), "{report}");
    }

    // -----------------------------------------------------------------------
    // The #245 token-hygiene census, carried by every crate that holds a live
    // Session. Same mechanism as git-vista-session's (where the history of
    // why-a-directory-census is recorded); the floor list names THIS crate's
    // files.
    // -----------------------------------------------------------------------

    /// Every `.rs` file in this crate's `src/`, read from disk at test time,
    /// so a file added later cannot be forgotten the way a hand-written
    /// `include_str!` list once forgot `plan_tools.rs` in git-vista-mcp.
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
    /// would make the scan pass by scanning zero bytes. The names here are a
    /// **floor**, not the list — a new file is picked up automatically.
    #[test]
    fn the_source_census_really_sees_every_file_in_the_crate() {
        let sources = crate_sources();
        let names: Vec<&str> = sources.iter().map(|(n, _)| n.as_str()).collect();
        let expected = "main.rs";
        assert!(
            names.contains(&expected),
            "the source census missed {expected}: {names:?}"
        );
        for (name, body) in &sources {
            assert!(
                body.len() > 500,
                "{name} was read as only {} bytes — the census is scanning nothing",
                body.len()
            );
        }
    }

    /// The #245 acceptance criterion — the token never lands in argv, env, or
    /// any file this crate writes — held structurally: the production half of
    /// every source file must stay free of the APIs that could violate it. A
    /// future slice adding one fails this named test and forces conscious
    /// review instead of slipping through.
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
}
