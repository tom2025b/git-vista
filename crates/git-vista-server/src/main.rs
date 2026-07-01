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

use axum::response::IntoResponse;
use axum::{
    http::{header, HeaderValue, StatusCode},
    routing::{get, post},
    Json, Router,
};
use git_vista_core::layout;
use git_vista_core::model::{
    BranchRequest, CommitSummary, CreateBranchRequest, CreateCommitRequest, GitRef, RefKind,
};
use git_vista_git::{read_refs, walk_history};
use tower::Layer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

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

    let app = Router::new()
        .route("/api/commits", get(commits))
        // Issue #18: create a branch at a commit (shells out to `git branch`).
        .route("/api/branch", post(create_branch))
        // Issue #33: create a commit on top of HEAD (shells out to `git commit`).
        .route("/api/commit", post(create_commit))
        // Issue #33 follow-up: the live checked-out branch, resolved fresh on every
        // request so the merge dialog shows the true target even without a Refresh.
        .route("/api/head-branch", get(head_branch))
        // Issue #33 follow-up: branch operations, each shelling out to git.
        .route("/api/merge", post(merge_branch))
        .route("/api/push", post(push_branch))
        .route("/api/delete-branch", post(delete_branch))
        // Anything that isn't the API is served from the built SPA bundle.
        .fallback_service(spa);

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
///
/// Sent `Cache-Control: no-store` so the browser never caches the graph: the repo
/// changes underneath us (new commits, new/switched branches) between launches,
/// and iOS Safari's on-disk cache otherwise persists a stale graph across app —
/// and even device — restarts, making freshly created branches never appear.
async fn commits() -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = repo_path();
    let history = walk_history(repo, HISTORY_LIMIT).map_err(|e| {
        eprintln!("git-vista: /api/commits failed reading history: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let refs = read_refs(repo).map_err(|e| {
        eprintln!("git-vista: /api/commits failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // Log, to the local terminal, exactly which branches this read found. This is
    // the diagnostic for issue #16's "a new branch doesn't show up": if the branch
    // you just made is listed here but not on the iPad, the graph is being read
    // fine and the browser is showing a cached copy; if it's missing here, the
    // problem is the repo being served (wrong path) or a ref the walk couldn't read.
    log_commits_summary(repo, &history, &refs);
    // The checked-out branch owns its line, so a branch just created from its tip
    // is the one drawn as a new stub line (not the trunk). See `layout_with_refs`.
    let head_branch = git_vista_git::read_head_branch(repo);
    let mut graph = layout::layout_with_refs(history, refs, head_branch.as_deref());
    // Tell the UI exactly which repo path this graph came from, so the header can
    // show it. If the page ever displays a different repo than the terminal is
    // serving, this makes the mismatch visible instead of a mystery.
    graph.repo_label = Some(repo.display().to_string());
    // Attach the GitHub web base (if this repo has a github.com origin) so the UI
    // can link commits and refs. None => the frontend renders plain-text labels.
    graph.repo_url = git_vista_git::github_web_base(repo);
    // Mark which commits are on the remote, so the UI only links pushed objects —
    // an unpushed commit/ref would 404 on GitHub. Only worth computing when we
    // have a web base to link to; on failure we leave it empty (nothing linked).
    if graph.repo_url.is_some() {
        if let Ok(remote) = git_vista_git::read_remote_commits(repo, HISTORY_LIMIT) {
            graph.remote_commits = remote.into_iter().collect();
        }
    }
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(graph)))
}

/// Create a branch in the served repository at a given commit (Issue #18).
///
/// B3 from the design discussion: shell out to `git branch <name> <commit>` rather
/// than write the ref ourselves. git does the heavy lifting — it validates the ref
/// name, refuses a name that already exists, resolves the start-point, and reports
/// a clear message on stderr — which we forward verbatim to the UI on failure.
///
/// Args are passed as separate argv entries (never a shell line), so a crafted
/// name/commit can't inject a command. We additionally reject an empty name and
/// one starting with `-` so it can't be read as a git option.
async fn create_branch(Json(req): Json<CreateBranchRequest>) -> (StatusCode, String) {
    let name = req.name.trim();
    let commit = req.commit.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "Branch name can't be empty.".to_string());
    }
    if name.starts_with('-') {
        return (StatusCode::BAD_REQUEST, "Branch name can't start with '-'.".to_string());
    }

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path())
        .arg("branch")
        .arg(name)
        .arg(commit)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/branch couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[/api/branch] created branch '{name}' at {commit}");
        (StatusCode::OK, format!("Created branch '{name}'."))
    } else {
        // git already explains the failure (name exists, bad name, unknown commit,
        // …) on stderr; surface that so the UI can show the real reason.
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() {
            "git branch failed.".to_string()
        } else {
            msg
        };
        eprintln!("git-vista: /api/branch failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Create a commit on top of the current HEAD in the served repository (Issue #33).
///
/// Same B3 posture as [`create_branch`]: shell out to `git commit` rather than
/// build the commit ourselves. git validates the tree state, refuses an empty
/// commit unless `--allow-empty` is passed, and reports a clear message on stderr
/// (e.g. "nothing to commit") which we forward verbatim to the UI on failure.
///
/// The UI only offers this on the HEAD tip, so a plain `git commit` lands exactly
/// where the user clicked — no ref moving here. Args are separate argv entries
/// (never a shell line); the message is the value of `-m`, so even a message
/// starting with `-` can't be read as an option. An empty message is rejected.
async fn create_commit(Json(req): Json<CreateCommitRequest>) -> (StatusCode, String) {
    let message = req.message.trim();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Commit message can't be empty.".to_string(),
        );
    }

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo_path()).arg("commit");
    if req.allow_empty {
        cmd.arg("--allow-empty");
    }
    cmd.arg("-m").arg(message);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/commit couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[/api/commit] created commit (allow_empty={})", req.allow_empty);
        (StatusCode::OK, "Created commit.".to_string())
    } else {
        // git explains the failure, but "nothing to commit, working tree clean"
        // goes to *stdout* (not stderr) with a non-zero exit — so prefer stderr,
        // fall back to stdout, and only then to a generic line.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git commit failed.".to_string()
            } else {
                stdout
            }
        };
        eprintln!("git-vista: /api/commit failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// The currently checked-out branch, resolved fresh (Issue #33 follow-up). The
/// merge dialog fetches this the moment the user clicks "Merge", so it names the
/// real target even if the graph on screen is a stale snapshot from before a branch
/// switch. `null` => detached HEAD. Sent `no-store` so it's never served from cache.
async fn head_branch() -> impl IntoResponse {
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    (no_store, Json(git_vista_git::read_head_branch(repo_path())))
}

/// Merge a branch into the currently checked-out branch (Issue #33 follow-up):
/// `git merge --no-edit <branch>`. `--no-edit` takes git's default merge message
/// (the server has no editor). A merge lands in whatever HEAD points at, so the UI
/// labels this with the current branch and never switches branches itself.
async fn merge_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    run_branch_op(
        "/api/merge",
        &req.branch,
        &["merge", "--no-edit"],
        format!("merged '{}' into HEAD", req.branch.trim()),
    )
    .await
}

/// Push a branch to `origin` (Issue #33 follow-up): `git push origin <branch>`.
/// A non-origin remote (or none) makes git error; that text is forwarded to the UI.
async fn push_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    run_branch_op(
        "/api/push",
        &req.branch,
        &["push", "origin"],
        format!("pushed '{}' to origin", req.branch.trim()),
    )
    .await
}

/// Delete a branch (Issue #33 follow-up): `git branch -d <branch>`. The lowercase
/// `-d` is the *safe* delete — git refuses to drop a branch whose commits aren't
/// merged, forwarding "not fully merged" to the UI. The UI also confirms first, so
/// deletion takes both a click-through and a merged branch.
async fn delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    run_branch_op(
        "/api/delete-branch",
        &req.branch,
        &["branch", "-d"],
        format!("deleted branch '{}'", req.branch.trim()),
    )
    .await
}

/// Shared runner for the branch-operation endpoints (merge/push/delete). Validates
/// `branch` (non-empty, not an option), then runs `git -C <repo> <args…> <branch>`
/// with the branch as its own final argv entry — so a crafted name is a git
/// argument, never a shell command. On failure it forwards git's own stderr
/// (falling back to stdout, then a generic line), matching `create_commit`'s posture.
async fn run_branch_op(
    endpoint: &str,
    branch: &str,
    args: &[&str],
    ok_msg: String,
) -> (StatusCode, String) {
    let branch = branch.trim();
    if branch.is_empty() {
        return (StatusCode::BAD_REQUEST, "Branch name can't be empty.".to_string());
    }
    if branch.starts_with('-') {
        return (StatusCode::BAD_REQUEST, "Branch name can't start with '-'.".to_string());
    }

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path())
        .args(args)
        .arg(branch)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: {endpoint} couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[{endpoint}] {ok_msg}");
        (StatusCode::OK, ok_msg)
    } else {
        // git explains most failures on stderr, but some (e.g. an up-to-date merge)
        // print to stdout with a non-zero exit — so prefer stderr, fall back to stdout.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git command failed.".to_string()
            } else {
                stdout
            }
        };
        eprintln!("git-vista: {endpoint} failed: {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Print, to the local terminal, a one-line summary of what a `/api/commits` read
/// found: the repo served, the commit count, and — crucially — the local branch
/// names. It's the fastest answer to "I made a branch and it isn't showing":
/// reload the page and look here. If the branch is in this list, the server sees
/// it and the browser is caching a stale graph; if it's absent, the server is
/// reading the wrong repo or couldn't read the ref (see any warnings above).
fn log_commits_summary(repo: &Path, history: &[CommitSummary], refs: &[GitRef]) {
    let mut local = Vec::new();
    let (mut remote, mut tags, mut has_head) = (0usize, 0usize, false);
    for r in refs {
        match r.kind {
            RefKind::Branch => local.push(r.name.as_str()),
            RefKind::RemoteBranch => remote += 1,
            RefKind::Tag => tags += 1,
            RefKind::Head => has_head = true,
        }
    }
    println!(
        "[/api/commits] {} — {} commit(s); {} local branch(es) [{}]; {remote} remote, {tags} tag(s){}",
        repo.display(),
        history.len(),
        local.len(),
        local.join(", "),
        if has_head { "; HEAD" } else { "" },
    );
}
