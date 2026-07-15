//! Native HTTP backend for git-vista.
//!
//! git-vista is browser-first: the user runs it in Safari on an iPad, which can't
//! read a git repo itself. This server runs the native git reader
//! ([`git_vista_git::walk_history`]) + the pure layout ([`git_vista_core::layout`])
//! and serves, on a single origin:
//!   - `GET /api/commits` — the laid-out [`Graph`] as JSON, and
//!   - everything else    — the wasm SPA bundle Trunk builds into the frontend's
//!     `dist/` directory.
//!
//! The frontend just `fetch`es `/api/commits` (same origin, no CORS).
//!
//! # Module layout
//!
//! This crate root keeps only process wiring: the [`main`] entry point (which
//! assembles the router and starts the server) and the startup banner. The rest
//! was split out of what was a single large `main.rs`, by concern:
//!
//!   * [`state`]    — the process-wide "which repo, and is it writable?" state,
//!     the config constants, and the read-only write guard.
//!   * [`git_cmd`]  — the thin `git -C <repo> …` command wrappers the handlers share.
//!   * [`handlers`] — the `/api/*` route handlers, one submodule per concern.
//!   * [`activity`] / [`journal`] — the Activity Log / Contextual Undo backend
//!     (`GET /api/activity` and the on-disk journal it reads).
//!
//! The split is move-only: every handler and helper kept its behaviour, and the
//! router below reads exactly as it did when all of this lived in one file.

use std::io::ErrorKind;
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};

// The activity feed (journal + reflogs + snapshots) — the server-side half of
// the Activity Log / Contextual Undo feature. `journal` owns the on-disk state
// under `.git/git-vista/`; `activity` owns `GET /api/activity`.
mod activity;
mod git_cmd;
mod handlers;
mod journal;
// The versioned-API-contract layer (M1.02, #102): protocol negotiation, the
// request id, the structured error envelope, and the contract response headers.
mod middleware;
mod state;

use axum::response::IntoResponse;
use axum::{
    http::{header, HeaderValue, StatusCode},
    routing::{get, post},
    Router,
};
use tower::Layer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use handlers::branch::{
    checkout_branch, create_branch, delete_branch, force_delete_branch, merge_branch, push_branch,
};
use handlers::clone::clone_repo;
use handlers::commit::{create_commit, stage_all, unstage_all};
use handlers::protocol::protocol_info;
use handlers::read::{
    commit_detail, commit_diff, commits, file_at_commit, head_branch, worktree_status,
};
use handlers::rebase::{rebase, rebase_status};
use handlers::reset::reset_test_repo;
use state::{bind_addr, clones_root, current, set_current, DEFAULT_REPO, DIST_DIR, PORT};

#[tokio::main]
async fn main() {
    // No git child may ever sit waiting on input this headless server can't
    // provide: these are inherited by every `git` the handlers spawn, making
    // git fail fast instead of hanging a request forever on a credential
    // prompt (push/clone against a private remote) or an editor. A request
    // that never completes surfaces on the iPad as the same opaque
    // "Load failed" a dead server does.
    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    std::env::set_var("GIT_EDITOR", "true");

    // Resolve which repo to serve: first CLI arg, else the default checkout.
    // Canonicalise so relative paths (e.g. `.`) and the banner are absolute; if
    // that fails (path missing) keep the raw value so the error is reported.
    let raw = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REPO));
    let repo = raw.canonicalize().unwrap_or(raw);
    if !repo.join(".git").exists() {
        eprintln!(
            "warning: {} doesn't look like a git repository (no .git).",
            repo.display()
        );
        eprintln!("         /api/commits will error until it points at a real repo.\n");
    }
    // The CLI-arg repo is the user's own working repo, so it's writable.
    set_current(repo, false);

    // Phase 13: clear any throwaway clones left behind by a previous run. The `gv`
    // launcher SIGKILLs the old server on restart, so its last Phase 12 clone was
    // never cleaned up and would otherwise pile up under the temp dir across runs.
    // Nothing is being served from there yet at startup, so removing the whole
    // clones root is safe; the next clone recreates it.
    let clones = clones_root();
    if clones.exists() {
        if let Err(e) = std::fs::remove_dir_all(&clones) {
            eprintln!(
                "git-vista: couldn't clear old clones at {}: {e}",
                clones.display()
            );
        }
    }

    // Warn early if the SPA hasn't been built — otherwise every page is a 404
    // and it looks like the server is broken.
    if !Path::new(DIST_DIR).exists() {
        eprintln!("warning: the web bundle isn't built yet ({DIST_DIR} is missing).");
        eprintln!(
            "         run `(cd crates/git-vista && trunk build)` first, or pages will 404.\n"
        );
    }

    // Serve the SPA bundle with `Cache-Control: no-cache` so the browser always
    // revalidates index.html (and thus picks up a freshly built wasm hash) instead
    // of running a stale, cached frontend — the cache layered on top of the live
    // git data we already keep uncacheable below. The layer wraps only the static
    // fallback, so it never overrides the API's stronger `no-store`.
    let spa = SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    )
    .layer(ServeDir::new(DIST_DIR).append_index_html_on_directories(true));

    // Every `/api/*` route lives on this sub-router so the M1.02 contract layer
    // (protocol negotiation, request id, structured errors, response headers)
    // wraps them all — and only them, never the static SPA below.
    let api = Router::new()
        // The one unversioned endpoint: a client hits it to learn the protocol
        // before it can be required to speak it (so it's exempt from the header
        // check inside the contract layer).
        .route("/api/protocol", get(protocol_info))
        .route("/api/commits", get(commits))
        // Phase 10: full detail for one commit, read on demand for the side panel.
        .route("/api/commit/{id}", get(commit_detail))
        // Activity/Undo feature, step 2: one commit's diff (file list + patch),
        // read on demand when the detail panel opens. `?full=1` lifts the patch
        // cap for the full-screen diff viewer.
        .route("/api/diff/{id}", get(commit_diff))
        // Full file viewer: one file's whole content at one commit (`git show
        // <id>:<path>`), read on demand when a file in the diff list is tapped.
        .route("/api/file/{id}/{*path}", get(file_at_commit))
        // Phase 12: clone a public URL into a temp dir and view it read-only.
        .route("/api/clone", post(clone_repo))
        // Issue #18: create a branch at a commit (shells out to `git branch`).
        .route("/api/branch", post(create_branch))
        // Issue #33: create a commit on top of HEAD (shells out to `git commit`).
        .route("/api/commit", post(create_commit))
        // Stage the working tree (`git add -A`) so the UI can stage, then commit.
        .route("/api/stage", post(stage_all))
        // …and unstage it again (`git reset HEAD`) — the exact inverse, offered
        // by the menu while anything is staged.
        .route("/api/unstage", post(unstage_all))
        // Issue #33 follow-up: the live checked-out branch, resolved fresh on every
        // request so the merge dialog shows the true target even without a Refresh.
        .route("/api/head-branch", get(head_branch))
        // Working-tree status (Activity/Undo feature, step 1): branch, ahead/
        // behind, and the staged/unstaged/untracked/conflicted file lists —
        // resolved fresh per request, like `head_branch`.
        .route("/api/status", get(worktree_status))
        // Activity/Undo feature, step 3: the chronological event feed —
        // journal + reflogs + snapshot diffs, folded and attributed.
        .route("/api/activity", get(activity::activity_feed))
        // Activity/Undo feature, step 5: the undo actions for one commit,
        // computed live; and the endpoint that executes one of them.
        .route("/api/undoables/{id}", get(activity::undoables))
        .route("/api/undo", post(activity::undo))
        // Issue #33 follow-up: branch operations, each shelling out to git.
        .route("/api/merge", post(merge_branch))
        .route("/api/push", post(push_branch))
        .route("/api/delete-branch", post(delete_branch))
        // iPad-testing follow-up: switch HEAD to a branch (`git checkout`).
        .route("/api/checkout", post(checkout_branch))
        // Issue #33 follow-up: force-delete an unmerged branch (`git branch -D`),
        // offered only after the safe `-d` above is refused; and rebase the
        // checked-out branch onto main (`git rebase`).
        .route("/api/force-delete-branch", post(force_delete_branch))
        .route("/api/rebase", post(rebase))
        // Whether "Rebase onto main" would do anything right now — the menu
        // disables the item (with the reason) when it wouldn't.
        .route("/api/rebase-status", get(rebase_status))
        // iPad-testing follow-up: restore a seeded *test repo* to its recorded
        // state (gated on the seed files `gv --seed` writes).
        .route("/api/reset-test-repo", post(reset_test_repo))
        // Inner: a panicking handler becomes a 500 with the panic text (not a
        // reset connection) *before* the contract layer sees it, so that 500 is
        // rewrapped into the structured error envelope like any other failure.
        .layer(CatchPanicLayer::custom(panic_to_response))
        // Outer: the M1.02 versioned-API contract — protocol negotiation, request
        // id, the consistent error envelope, and the response headers.
        .layer(axum::middleware::from_fn(middleware::api_contract));

    let app = Router::new()
        .merge(api)
        // Anything that isn't the API is served from the built SPA bundle.
        .fallback_service(spa)
        // Global backstop for the static SPA / fallback. The `/api` space has its
        // own inner catch above (so API panics are already enveloped); this keeps
        // any panic outside it from tearing down the connection — which iPad
        // Safari would report as the unexplained "Load failed".
        .layer(CatchPanicLayer::custom(panic_to_response));

    let addr = match bind_addr() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: could not bind {addr}: {e}");
            if e.kind() == ErrorKind::AddrInUse {
                eprintln!(
                    "  Port {PORT} is already in use — another git-vista-server may be running."
                );
                eprintln!("  Stop it (e.g. `pkill -f git-vista-server`) and try again.");
            }
            std::process::exit(1);
        }
    };

    print_startup_banner(addr);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("error: server stopped: {e}");
        std::process::exit(1);
    }
}

/// Turn a caught handler panic into a `500` carrying the panic text, instead of a
/// reset connection (which iPad Safari reports as an opaque "Load failed"). Used
/// by both `CatchPanicLayer`s; on the `/api` router the contract layer then
/// rewraps this into the structured error envelope like any other failure.
fn panic_to_response(panic: Box<dyn std::any::Any + Send>) -> axum::response::Response {
    let msg = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("(no panic message)");
    eprintln!("git-vista: handler panicked: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("git-vista server bug — a handler panicked: {msg}"),
    )
        .into_response()
}

/// Print the local/SSH path by default and isolate the legacy LAN guidance to an
/// explicit non-loopback bind.
fn print_startup_banner(addr: std::net::SocketAddr) {
    println!("git-vista server — serving {}", current().0.display());
    println!("  • on this machine: http://localhost:{PORT}/");
    if addr.ip().is_loopback() {
        println!("  • from the iPad: use an SSH local port forward to 127.0.0.1:{PORT}");
        println!("    example: ssh -N -L {PORT}:127.0.0.1:{PORT} <linux-host>");
    } else {
        let display_ip = lan_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "<this-machine-LAN-IP>".to_string());
        println!("  • from the iPad: http://{display_ip}:{PORT}/");
        println!();
        println!("WARNING: LAN mode has no authentication or HTTPS.");
        println!("Anyone or any webpage that can reach this port may invoke Git operations.");
        println!("Use only on a trusted personal LAN; prefer the default SSH-tunnel mode.");
    }
    println!();
}

/// Best-effort: this machine's primary LAN IPv4. Connecting a UDP socket sends no
/// packets; it just makes the OS pick the outbound interface, whose local address
/// is the IP other devices on the LAN use to reach us. Returns `None` if offline.
fn lan_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|addr| addr.ip())
}
