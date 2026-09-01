//! The #246 dependency-graph proof, applied to the terminal UI (M10.01,
//! #456's acceptance criterion): `gv-tui`'s own dependency graph never
//! reaches `git-vista-server` — and therefore never reaches any write
//! handler in it. The TUI drives git-vista through the same HTTP API (and
//! later the same planner funnel) the browser uses; linking the server crate
//! would make every write handler reachable code in a terminal binary and
//! quietly dissolve that funnel.
//!
//! Same mechanism as `git-vista-mcp/tests/no_write_dependency.rs`; the fuller
//! argument for why a compile-time graph walk beats a router test lives in
//! that file's module doc and transplants here unchanged. A manifest grep
//! would catch a direct dependency but not a transitive one; `cargo
//! metadata`'s `resolve.nodes` graph is the same data Cargo itself uses to
//! decide what gets linked.
#[test]
fn gv_tui_never_depends_on_git_vista_server_even_transitively() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let manifest_path = format!("{manifest_dir}/Cargo.toml");

    // `--filter-platform` keeps this deterministic in CI: without it, cargo
    // resolves the graph for EVERY platform, including Windows-only crates a
    // Linux runner never downloads, and `--offline` then fails on cache
    // state rather than on the code. (Incident write-up: git-vista-mcp's
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

    let tui_id = packages
        .iter()
        .find(|p| p["name"] == "gv-tui")
        .expect("gv-tui was not in its own `cargo metadata` output")["id"]
        .as_str()
        .expect("gv-tui's id was not a string")
        .to_string();

    // Breadth-first over the whole resolved graph reachable from this
    // package — every dependency edge of every kind (normal, build, dev),
    // because a reachability proof that only checked normal deps would miss
    // a dev- or build-dependency edge just as easily.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    queue.push_back(tui_id.as_str());
    seen.insert(tui_id.as_str());

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
        "gv-tui's dependency graph now reaches git-vista-server — that makes \
         every write handler in it (handlers::branch, handlers::commit, \
         handlers::select, …) reachable code from the terminal binary. The \
         TUI is a client of the HTTP API and the planner funnel, never a \
         second host of the handlers; whatever edit introduced this \
         dependency needs to be re-reasoned about, not waved through."
    );

    // A sanity floor so this test can't pass by accident: the crate's own
    // known, deliberate dependencies must actually be found.
    assert!(
        reached_names.contains(&"git-vista-session"),
        "sanity check failed: git-vista-session was not found in the \
         resolved graph at all — the graph walk above is not exercising \
         anything"
    );
    assert!(
        reached_names.contains(&"git-vista-protocol"),
        "sanity check failed: git-vista-protocol was not found in the \
         resolved graph at all — the graph walk above is not exercising \
         anything"
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
