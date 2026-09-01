//! The #246 dependency-graph proof, applied to the extracted session crate
//! (M10.01, #456): `git-vista-session`'s own dependency graph never reaches
//! `git-vista-server` — and therefore never reaches any write handler in it.
//!
//! This crate is what every non-browser client (`git-vista-mcp`, `gv-tui`,
//! whatever comes next) links to authenticate, so the read-only-by-construction
//! property those crates each prove for themselves would be hollow if the one
//! crate they all share could quietly grow a server edge. Same mechanism as
//! `git-vista-mcp/tests/no_write_dependency.rs`, and the fuller argument for
//! why a compile-time graph walk beats a router test lives in that file's
//! module doc — it transplants here unchanged.
//!
//! # Why walk `cargo metadata`'s resolved graph, not just grep `Cargo.toml`
//!
//! A manifest grep would catch a *direct* dependency but not a transitive
//! one. `cargo metadata`'s `resolve.nodes` graph is the same data Cargo
//! itself uses to decide what gets linked, so walking it transitively checks
//! the actual built graph, not a hand-maintained proxy for it.
#[test]
fn git_vista_session_never_depends_on_git_vista_server_even_transitively() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let manifest_path = format!("{manifest_dir}/Cargo.toml");

    // `--filter-platform` keeps this deterministic in CI: without it, cargo
    // resolves the graph for EVERY platform, including Windows-only crates a
    // Linux runner never downloads, and `--offline` then fails on cache
    // state rather than on the code. The dependency this test guards against
    // is not platform-gated in any manifest, so the host graph is the graph
    // that matters. (The full incident write-up lives in git-vista-mcp's
    // copy of this test.)
    let host_triple = host_triple();
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--filter-platform",
            &host_triple,
            "--manifest-path",
            &manifest_path,
        ])
        .output()
        .expect("could not run `cargo metadata` — is cargo on PATH?");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata did not print valid JSON");

    let packages = metadata["packages"]
        .as_array()
        .expect("metadata had no `packages` array");
    let name_of: std::collections::HashMap<&str, &str> = packages
        .iter()
        .map(|p| {
            (
                p["id"].as_str().expect("package id was not a string"),
                p["name"].as_str().expect("package name was not a string"),
            )
        })
        .collect();

    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("metadata had no `resolve.nodes` array — is Cargo.lock present?");
    let node_by_id: std::collections::HashMap<&str, &serde_json::Value> = nodes
        .iter()
        .map(|n| (n["id"].as_str().expect("node id was not a string"), n))
        .collect();

    let session_id = packages
        .iter()
        .find(|p| p["name"] == "git-vista-session")
        .expect("git-vista-session was not in its own `cargo metadata` output")["id"]
        .as_str()
        .expect("git-vista-session's id was not a string")
        .to_string();

    // Breadth-first over the whole resolved graph reachable from this
    // package — every dependency edge of every kind (normal, build, dev),
    // because a reachability proof that only checked normal deps would miss
    // a dev- or build-dependency edge just as easily.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    queue.push_back(session_id.as_str());
    seen.insert(session_id.as_str());

    while let Some(id) = queue.pop_front() {
        let Some(node) = node_by_id.get(id) else {
            continue;
        };
        let deps = node["dependencies"]
            .as_array()
            .expect("resolve node had no `dependencies` array");
        for dep in deps {
            let dep_id = dep.as_str().expect("dependency id was not a string");
            if seen.insert(dep_id) {
                queue.push_back(dep_id);
            }
        }
    }

    let reached_names: Vec<&str> = seen
        .iter()
        .filter_map(|id| name_of.get(id).copied())
        .collect();

    assert!(
        !reached_names.contains(&"git-vista-server"),
        "git-vista-session's dependency graph now reaches git-vista-server — \
         that makes every write handler in it reachable code from EVERY \
         non-browser client that links this crate (git-vista-mcp, gv-tui). \
         The whole premise of the extraction (#456) is that the shared \
         session boundary stays read-only by construction; whatever edit \
         introduced this dependency needs to be re-reasoned about, not waved \
         through."
    );

    // A sanity floor so this test can't pass by accident (e.g. `cargo
    // metadata` returning an empty graph on a misconfigured manifest path):
    // the crate's own known, deliberate dependencies must actually be found.
    assert!(
        reached_names.contains(&"git-vista-protocol"),
        "sanity check failed: git-vista-protocol was not found in the \
         resolved graph at all — the graph walk above is not exercising \
         anything"
    );
    assert!(
        reached_names.contains(&"serde_json"),
        "sanity check failed: serde_json was not found in the resolved graph \
         at all — the graph walk above is not exercising anything"
    );
}

/// The host's target triple, asked of `rustc` rather than assembled from
/// `std::env::consts` — those give arch and OS separately and reassembling
/// them into a triple is guesswork that differs from what cargo expects
/// (`gnu` vs `musl`, `pc` vs `unknown`). `rustc -vV` prints the
/// authoritative one.
fn host_triple() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = std::process::Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("could not run `rustc -vV` — is rustc on PATH?");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .expect("`rustc -vV` printed no `host:` line")
        .trim()
        .to_string()
}
