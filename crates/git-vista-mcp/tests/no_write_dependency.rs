//! The #246 dependency-graph proof: `git-vista-mcp`'s own dependency graph
//! never reaches `git-vista-server` (and therefore never reaches any write
//! handler in it — `handlers::branch`, `handlers::commit`, `handlers::select`,
//! and friends), mirroring `git-vista-server/src/main.rs`'s
//! `the_lan_router_has_no_write_routes` in spirit but applied one layer
//! earlier, to *this* crate's dependency graph instead of a router's route
//! table.
//!
//! # Why this mechanism, and not a router test
//!
//! `the_lan_router_has_no_write_routes` proves a *route* is never
//! *registered* on one listener — a runtime, per-request check, because the
//! server links every handler either way and the safety property is "which
//! ones are wired up." That shape does not transplant to this crate: there
//! is no router here, and — more to the point — a router test could only
//! prove a request-time property, one dispatch at a time.
//!
//! What #246 actually asks ("no *code path* reaches a write handler") is a
//! **compile-time** property here, because this crate simply never links
//! `git-vista-server` at all (see `tools.rs`'s module doc for exactly which
//! DTOs it uses instead — `git-vista-protocol` and the pure `git-vista-core`,
//! never the server crate). If `git-vista-server` is absent from this
//! package's entire transitive dependency graph, no function call from
//! anywhere in this crate can name, let alone reach, a symbol that only
//! exists inside it — Rust's own linker enforces that, unconditionally, with
//! no dispatch-time branch to get wrong. That is strictly stronger than "the
//! route isn't registered": it is "the write handler is not reachable code in
//! this binary at all."
//!
//! # Why walk `cargo metadata`'s resolved graph, not just grep `Cargo.toml`
//!
//! Reading this crate's own `Cargo.toml` and checking it doesn't name
//! `git-vista-server` would catch a *direct* dependency, but not a
//! transitive one — e.g. if a future edit added a path dependency on some
//! other crate that itself gained a dependency on `git-vista-server`, a
//! `Cargo.toml`-only check would stay green while the property it claims to
//! prove had quietly become false. `cargo metadata`'s `resolve.nodes` graph
//! is the same data Cargo itself uses to decide what gets linked, so walking
//! it transitively is checking the actual built graph, not a hand-maintained
//! proxy for it.
#[test]
fn git_vista_mcp_never_depends_on_git_vista_server_even_transitively() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let manifest_path = format!("{manifest_dir}/Cargo.toml");

    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
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

    // Package id -> package name, so the final assertion reads by name, not
    // by cargo's opaque id string.
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

    let mcp_id = packages
        .iter()
        .find(|p| p["name"] == "git-vista-mcp")
        .expect("git-vista-mcp was not in its own `cargo metadata` output")["id"]
        .as_str()
        .expect("git-vista-mcp's id was not a string")
        .to_string();

    // Breadth-first over the *whole* resolved graph reachable from this
    // package — every dependency edge cargo actually resolved, of every
    // kind (normal, build, dev), because a write-handler reachability proof
    // that only checked normal deps would miss a dev-dependency or
    // build-dependency edge just as easily.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    queue.push_back(mcp_id.as_str());
    seen.insert(mcp_id.as_str());

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
        "git-vista-mcp's dependency graph now reaches git-vista-server — that \
         makes every write handler in it (handlers::branch, handlers::commit, \
         handlers::select, …) reachable code from this crate. #246's whole \
         premise is that this bridge is read-only by construction, not by \
         discipline; whatever edit introduced this dependency needs to be \
         re-reasoned about, not waved through."
    );

    // A sanity floor so this test can't pass by accident (e.g. `cargo
    // metadata` returning an empty graph on a misconfigured manifest path):
    // the crate's own known, deliberate dependencies must actually be found.
    assert!(
        reached_names.contains(&"git-vista-protocol"),
        "sanity check failed: git-vista-protocol was not found in the resolved \
         graph at all — the graph walk above is not exercising anything"
    );
    assert!(
        reached_names.contains(&"git-vista-core"),
        "sanity check failed: git-vista-core was not found in the resolved \
         graph at all — the graph walk above is not exercising anything"
    );
}
