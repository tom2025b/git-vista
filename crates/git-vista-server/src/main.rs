//! Native HTTP backend for git-vista.
//!
//! git-vista is browser-first: the user runs it in Safari on an iPad, which can't
//! read a git repo itself. This server runs the native git reader
//! ([`git_vista_git::walk_history`]) + the pure layout ([`git_vista_core::layout`])
//! and serves, on a single origin:
//!   - `GET /api/commits` — the laid-out [`Graph`] as JSON, and
//!   - everything else    — the wasm SPA bundle Trunk builds into the frontend's
//!     `dist/` directory.
//! The frontend just `fetch`es `/api/commits` (same origin, no CORS).

use std::net::SocketAddr;
use std::path::Path;

use axum::{http::StatusCode, routing::get, Json, Router};
use git_vista_core::layout;
use git_vista_core::model::Graph;
use git_vista_git::walk_history;
use tower_http::services::ServeDir;

// Hardcoded for now (Phase 4); made configurable in a later phase.
const REPO_PATH: &str = "/home/tom/projects/git-vista";
// Upper bound on how much history to walk; plenty for now.
const HISTORY_LIMIT: usize = 5_000;
// The wasm bundle Trunk emits next to the frontend crate. Resolved at compile
// time relative to this crate so the server runs from any working directory.
const DIST_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../git-vista/dist");
// Bound on all interfaces so the iPad can reach it over the LAN.
const ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8080);

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/api/commits", get(commits))
        // Anything that isn't the API is served from the built SPA bundle.
        .fallback_service(ServeDir::new(DIST_DIR).append_index_html_on_directories(true));

    println!("git-vista: serving {REPO_PATH}\n  open http://{ADDR}/ (use this machine's LAN IP from the iPad)");
    let listener = tokio::net::TcpListener::bind(ADDR)
        .await
        .expect("bind TCP listener");
    axum::serve(listener, app).await.expect("run server");
}

/// Walk the (hardcoded) repository and return its laid-out graph as JSON.
async fn commits() -> Result<Json<Graph>, (StatusCode, String)> {
    let history = walk_history(Path::new(REPO_PATH), HISTORY_LIMIT)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(layout::layout(history)))
}
