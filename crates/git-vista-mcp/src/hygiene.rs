//! The #245 token-hygiene census, kept in this crate after #456 moved
//! `auth.rs` and `http.rs` (with their own copy of this census) into
//! `git-vista-session`.
//!
//! Why it stays here too: every remaining file in this crate still handles
//! the live `git_vista_session::auth::Session` — the tools fold server error
//! bodies into strings an MCP host may log or show — so the structural
//! guarantee ("the production half of every source file is free of the APIs
//! that could put a secret into argv, the environment, or a file") must keep
//! covering THIS crate's sources, not just the extracted ones. The guard
//! belongs to every crate that holds a secret, not to one file location.
//!
//! The mechanism (a directory census at test time rather than a hand-written
//! file list, with a >500-byte anti-vacuity floor) and the history of why
//! (#248 once added `plan_tools.rs` — a third of the crate — without the old
//! hand-written list noticing) are documented on `git-vista-session`'s copy,
//! which is the original moved verbatim. This module exists so the census
//! has a compiled home in a binary crate whose other modules all have bigger
//! jobs; it contains tests only.

#[cfg(test)]
mod tests {
    /// Every `.rs` file in this crate's `src/`, read from disk at test time,
    /// so a file added later cannot be forgotten the way a hand-written
    /// `include_str!` list once forgot `plan_tools.rs`.
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
    /// automatically and needs no edit. `auth.rs`/`http.rs` left this floor
    /// when #456 moved them to `git-vista-session`, whose own census now
    /// names them.
    #[test]
    fn the_source_census_really_sees_every_file_in_the_crate() {
        let sources = crate_sources();
        let names: Vec<&str> = sources.iter().map(|(n, _)| n.as_str()).collect();
        for expected in [
            "execute_tool.rs",
            "hygiene.rs",
            "lesson.rs",
            "main.rs",
            "plan_tools.rs",
            "tools.rs",
        ] {
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

    /// The #245 acceptance criterion — no secret ever lands in argv, env, or
    /// any file this crate writes — held structurally, not just by prose:
    /// the production half of every source file must stay free of the APIs
    /// that could violate it. A future slice adding one fails this named
    /// test and forces conscious review instead of slipping through.
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
