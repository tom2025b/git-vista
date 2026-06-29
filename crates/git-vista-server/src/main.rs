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

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::{http::StatusCode, routing::get, Json, Router};
use git_vista_core::layout;
use git_vista_core::model::Graph;
use git_vista_git::{read_refs, walk_history};
use tower_http::services::ServeDir;

// Which repository to visualise. Taken from the first CLI argument
// (`git-vista-server <path>`), falling back to this checkout when none is given.
// The `gv` launcher passes the directory you run it from. Set once at startup.
const DEFAULT_REPO: &str = "/home/tom/projects/git-vista";
static REPO: OnceLock<PathBuf> = OnceLock::new();

/// The repository being served, resolved at startup. Panics only if called before
/// `main` sets it.
fn repo_path() -> &'static Path {
    REPO.get().expect("REPO is set at startup").as_path()
}
// Upper bound on how much history to walk; plenty for now.
const HISTORY_LIMIT: usize = 5_000;
// The wasm bundle Trunk emits next to the frontend crate. Resolved at compile
// time relative to this crate so the server runs from any working directory.
const DIST_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../git-vista/dist");
const PORT: u16 = 8080;
// Bound on all interfaces so the iPad can reach it over the LAN.
const ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT);

#[tokio::main]
async fn main() {
    // Resolve which repo to serve: first CLI arg, else the default checkout.
    // Canonicalise so relative paths (e.g. `.`) and the banner are absolute; if
    // that fails (path missing) keep the raw value so the error is reported.
    let raw = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REPO));
    let repo = raw.canonicalize().unwrap_or(raw);
    if !repo.join(".git").exists() {
        eprintln!("warning: {} doesn't look like a git repository (no .git).", repo.display());
        eprintln!("         /api/commits will error until it points at a real repo.\n");
    }
    REPO.set(repo).expect("REPO set once");

    // Warn early if the SPA hasn't been built — otherwise every page is a 404
    // and it looks like the server is broken.
    if !Path::new(DIST_DIR).exists() {
        eprintln!("warning: the web bundle isn't built yet ({DIST_DIR} is missing).");
        eprintln!("         run `(cd crates/git-vista && trunk build)` first, or pages will 404.\n");
    }

    let app = Router::new()
        .route("/api/commits", get(commits))
        // Anything that isn't the API is served from the built SPA bundle.
        .fallback_service(ServeDir::new(DIST_DIR).append_index_html_on_directories(true));

    let listener = match tokio::net::TcpListener::bind(ADDR).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: could not bind {ADDR}: {e}");
            if e.kind() == ErrorKind::AddrInUse {
                eprintln!("  Port {PORT} is already in use — another git-vista-server may be running.");
                eprintln!("  Stop it (e.g. `pkill -f git-vista-server`) and try again.");
            }
            std::process::exit(1);
        }
    };

    print_startup_banner();

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("error: server stopped: {e}");
        std::process::exit(1);
    }
}

/// Print where to open the app and how to fix the common "works locally but the
/// iPad can't reach it" situation.
fn print_startup_banner() {
    println!("git-vista server — serving {}", repo_path().display());
    println!("  • on this machine: http://localhost:{PORT}/");
    match lan_ip() {
        Some(ip) => println!("  • from the iPad:   http://{ip}:{PORT}/   (must be on the same Wi-Fi)"),
        None => println!(
            "  • from the iPad:   http://<this-machine-LAN-IP>:{PORT}/   (find it with `hostname -I`)"
        ),
    }
    println!();
    println!("Reaches localhost but NOT the iPad? It's almost always one of these:");
    println!("  1. Firewall (most common): the OS is blocking inbound :{PORT}. Allow it, e.g.");
    println!("       sudo ufw allow {PORT}/tcp        # or: sudo ufw status  to check");
    println!("  2. Different network: the iPad's Wi-Fi must share this machine's subnet");
    println!("     (the LAN IP above). Guest Wi-Fi or a separate AP won't reach it.");
    println!("  3. Router 'AP/client isolation' blocks device-to-device traffic — disable it.");
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

/// Walk the configured repository (see [`repo_path`]) and return its laid-out
/// graph as JSON, with branch/tag/HEAD refs attached for badging and per-branch
/// colouring.
async fn commits() -> Result<Json<Graph>, (StatusCode, String)> {
    let repo = repo_path();
    let history = walk_history(repo, HISTORY_LIMIT)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let refs = read_refs(repo).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(layout::layout_with_refs(history, refs)))
}
