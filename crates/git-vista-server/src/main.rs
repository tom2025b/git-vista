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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::response::IntoResponse;
use axum::{
    extract::Path as AxumPath,
    http::{header, HeaderValue, StatusCode},
    routing::{get, post},
    Json, Router,
};
use git_vista_core::layout;
use git_vista_core::model::{
    validate_clone_url, BranchRequest, CloneRequest, CommitSummary, CreateBranchRequest,
    CreateCommitRequest, GitRef, RefKind,
};
use git_vista_core::status::parse_porcelain_v2;
use git_vista_git::{read_commit, read_refs, walk_history, RepoError};
use tower::Layer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

// Which repository to visualise *initially*. Taken from the first CLI argument
// (`git-vista-server <path>`), falling back to the current working directory (`.`,
// canonicalised at startup) when none is given. The `gv` launcher always passes
// the directory you run it from, so this default only bites when the server is run
// directly with no argument. This is only the starting repo — Phase 12 lets the
// user switch to a cloned URL at runtime.
const DEFAULT_REPO: &str = ".";

/// The repository the server is currently serving. Mutable at runtime (Phase 12):
/// starts at the CLI-arg repo (`read_only: false`, the user's own working repo),
/// and `POST /api/clone` swaps it for a throwaway clone (`read_only: true`).
struct Current {
    path: PathBuf,
    /// True for a cloned URL: a view-only snapshot, so the write endpoints refuse.
    read_only: bool,
}

static CURRENT: OnceLock<RwLock<Current>> = OnceLock::new();

/// Snapshot the current repo path and its read-only flag. Clones out of the lock
/// immediately so no guard is ever held across an `.await`.
fn current() -> (PathBuf, bool) {
    let g = CURRENT
        .get()
        .expect("CURRENT is set at startup")
        .read()
        .expect("CURRENT lock not poisoned");
    (g.path.clone(), g.read_only)
}

/// Point the server at a new repository (startup, or after a clone).
fn set_current(path: PathBuf, read_only: bool) {
    if let Some(lock) = CURRENT.get() {
        *lock.write().expect("CURRENT lock not poisoned") = Current { path, read_only };
    } else {
        CURRENT
            .set(RwLock::new(Current { path, read_only }))
            .unwrap_or_else(|_| unreachable!("CURRENT set once at startup"));
    }
}

/// Parent directory that holds every throwaway clone, under the OS temp dir. A
/// clone's temp dir is created here, and cleanup refuses to delete anything that
/// isn't under this root — so a bug can never `rm` a real repository.
fn clones_root() -> PathBuf {
    std::env::temp_dir().join("git-vista-clones")
}

/// Delete a previous clone's directory, best-effort. Guarded: only ever removes a
/// path under [`clones_root`], so it can't touch the user's own repo even if state
/// were somehow wrong.
fn cleanup_clone(path: &Path) {
    if path.starts_with(clones_root()) {
        if let Err(e) = std::fs::remove_dir_all(path) {
            eprintln!("git-vista: couldn't remove old clone {}: {e}", path.display());
        }
    }
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
            eprintln!("git-vista: couldn't clear old clones at {}: {e}", clones.display());
        }
    }

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
        // Phase 10: full detail for one commit, read on demand for the side panel.
        .route("/api/commit/{id}", get(commit_detail))
        // Activity/Undo feature, step 2: one commit's diff (file list + patch),
        // read on demand when the detail panel opens.
        .route("/api/diff/{id}", get(commit_diff))
        // Phase 12: clone a public URL into a temp dir and view it read-only.
        .route("/api/clone", post(clone_repo))
        // Issue #18: create a branch at a commit (shells out to `git branch`).
        .route("/api/branch", post(create_branch))
        // Issue #33: create a commit on top of HEAD (shells out to `git commit`).
        .route("/api/commit", post(create_commit))
        // Issue #33 follow-up: the live checked-out branch, resolved fresh on every
        // request so the merge dialog shows the true target even without a Refresh.
        .route("/api/head-branch", get(head_branch))
        // Working-tree status (Activity/Undo feature, step 1): branch, ahead/
        // behind, and the staged/unstaged/untracked/conflicted file lists —
        // resolved fresh per request, like `head_branch`.
        .route("/api/status", get(worktree_status))
        // Issue #33 follow-up: branch operations, each shelling out to git.
        .route("/api/merge", post(merge_branch))
        .route("/api/push", post(push_branch))
        .route("/api/delete-branch", post(delete_branch))
        // Issue #33 follow-up: force-delete an unmerged branch (`git branch -D`),
        // offered only after the safe `-d` above is refused; and rebase the
        // checked-out branch onto main (`git rebase`).
        .route("/api/force-delete-branch", post(force_delete_branch))
        .route("/api/rebase", post(rebase))
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
    println!("git-vista server — serving {}", current().0.display());
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
    let (repo, read_only) = current();
    let repo = repo.as_path();
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
    // A cloned URL is view-only: tell the UI to hide every write action.
    graph.read_only = read_only;
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

/// Full detail for one commit (Phase 10 — the detail panel): the whole message
/// body plus the author and committer signatures, looked up by hex id in the
/// current repo. A read, so it works on read-only clones too. A bad or unknown id
/// is a `404`; any other read failure a `500`. Sent `no-store` like the graph.
async fn commit_detail(
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = current().0;
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/commit/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(detail)))
}

/// Upper bound on the patch text returned by `/api/diff/{id}`. A huge commit
/// (vendored deps, generated files) can carry a multi-megabyte patch that would
/// choke the iPad both in transfer and in rendering; past this, the patch is
/// cut at a line boundary and flagged `truncated` so the panel says so.
const DIFF_PATCH_CAP: usize = 200_000;

/// One commit's diff (Activity/Undo feature, step 2): the per-file change list
/// and the unified patch, for the detail panel's Changes section.
///
/// The commit is first resolved via gix ([`read_commit`]) — validating the id
/// and yielding the parent count — then git itself produces the diff (same B3
/// posture as everywhere: git's diff engine handles renames, binaries and
/// merges; we only parse its machine-readable listings, in core, where that's
/// unit-tested). Three reads of the same diff:
///
///   * `--name-status -z` — the file list (order + change kinds),
///   * `--numstat -z`     — per-file added/deleted counts folded into it,
///   * `--patch`          — the unified text, capped at [`DIFF_PATCH_CAP`].
///
/// An ordinary commit uses `git show` (its diff vs the parent; a root commit
/// diffs against the empty tree). A merge commit is diffed against its *first
/// parent* instead — `git show` on a merge prints the usually-empty combined
/// diff, while "what did this merge bring in?" is exactly the first-parent
/// diff — and the response says so (`against_first_parent`).
async fn commit_diff(
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = current().0;
    // Belt-and-braces before the id goes anywhere near argv: real ids are hex.
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/diff/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let against_first_parent = detail.parents.len() >= 2;

    // The base args for the three reads: `show <id>` for ordinary commits,
    // `diff <id>^1 <id>` for merges. `--format=` silences show's commit header
    // so only the diff comes back; harmless on `diff`... which doesn't take it,
    // so the merge arm simply omits it.
    let first_parent = format!("{id}^1");
    let base: Vec<&str> = if against_first_parent {
        vec!["diff", first_parent.as_str(), id.as_str()]
    } else {
        vec!["show", "--format=", id.as_str()]
    };
    let with = |extra: &[&str]| -> Vec<String> {
        // Diff options go *before* the revisions so git never reads a
        // revision as an option's value.
        let mut args = vec![base[0].to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.extend(base[1..].iter().map(|s| s.to_string()));
        args
    };

    let name_status = git_stdout(&repo, &with(&["--name-status", "-z"]), "/api/diff").await?;
    let numstat = git_stdout(&repo, &with(&["--numstat", "-z"]), "/api/diff").await?;
    let patch_bytes =
        git_stdout(&repo, &with(&["--patch", "--no-color"]), "/api/diff").await?;

    let mut files = git_vista_core::diff::parse_name_status_z(&name_status);
    git_vista_core::diff::fold_numstat_z(&numstat, &mut files);

    // Cap the patch at a line boundary so the panel never gets half a line.
    let mut patch = String::from_utf8_lossy(&patch_bytes).into_owned();
    let truncated = patch.len() > DIFF_PATCH_CAP;
    if truncated {
        let cut = patch[..DIFF_PATCH_CAP].rfind('\n').unwrap_or(DIFF_PATCH_CAP);
        patch.truncate(cut);
    }

    let diff = git_vista_core::diff::CommitDiff {
        id: detail.id.0,
        files,
        patch,
        truncated,
        against_first_parent,
    };
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(diff)))
}

/// Run `git -C <repo> <args…>` and return its stdout bytes, mapping both spawn
/// failures and non-zero exits to a 500 with git's own stderr as the reason.
/// Shared by the diff reads; deliberately bytes, not String — paths in `-z`
/// listings aren't guaranteed UTF-8, and the parsers handle that themselves.
async fn git_stdout(
    repo: &Path,
    args: &[String],
    endpoint: &str,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            eprintln!("git-vista: {endpoint} couldn't run git: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"))
        })?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git failed.".to_string() } else { msg };
        eprintln!("git-vista: {endpoint} failed: {msg}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
    }
    Ok(output.stdout)
}

/// Guard for the write endpoints (Phase 12): when the current repo is a read-only
/// clone, refuse the operation with `403` and a clear reason, since any change
/// would be thrown away with the clone. Returns `None` when writes are allowed.
fn reject_if_read_only() -> Option<(StatusCode, String)> {
    if current().1 {
        Some((
            StatusCode::FORBIDDEN,
            "This repository is a read-only clone opened from a URL. Open your own \
             repo to make changes."
                .to_string(),
        ))
    } else {
        None
    }
}

/// Clone a public repository from a pasted URL into a throwaway temp directory and
/// switch the server to viewing it, read-only (Phase 12).
///
/// Same B3 posture as the other git handlers: shell out to `git clone` and forward
/// git's own error text (bad host, repo not found, …) verbatim. The URL is
/// validated by [`validate_clone_url`] — only `http(s)://`/`git://`, so a pasted
/// SSH URL can't trigger a key prompt — and is passed as its own argv entry, never
/// a shell line. A full clone is made (history is bounded downstream by
/// `HISTORY_LIMIT`); the previous clone, if any, is deleted so at most one is kept.
async fn clone_repo(Json(req): Json<CloneRequest>) -> (StatusCode, String) {
    let url = match validate_clone_url(&req.url) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, e),
    };

    let root = clones_root();
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("git-vista: /api/clone couldn't create {}: {e}", root.display());
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't prepare temp dir: {e}"));
    }
    // Unique per-clone dir: monotonic counter + a timestamp, so concurrent or
    // rapid clones never collide.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dest = root.join(format!("clone-{stamp}-{n}"));

    println!("[/api/clone] cloning {url} → {}", dest.display());
    let output = match tokio::process::Command::new("git")
        .arg("clone")
        // `--` so the URL is never read as an option, even past validation.
        .arg("--")
        .arg(&url)
        .arg(&dest)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/clone couldn't run git: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"));
        }
    };

    if !output.status.success() {
        // git printed why (host down, repo not found, auth needed…) on stderr.
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git clone failed.".to_string() } else { msg };
        cleanup_clone(&dest); // remove the empty/partial dir git may have left
        eprintln!("git-vista: /api/clone failed: {msg}");
        return (StatusCode::BAD_REQUEST, msg);
    }

    // Switch to the fresh clone (read-only), then delete the previous one, if it
    // was itself a clone — so disk holds at most one clone at a time.
    let (old_path, old_read_only) = current();
    set_current(dest.clone(), true);
    if old_read_only {
        cleanup_clone(&old_path);
    }
    println!("[/api/clone] now viewing {}", dest.display());
    (StatusCode::OK, format!("Cloned {url}"))
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
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
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
        .arg(current().0)
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
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let message = req.message.trim();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Commit message can't be empty.".to_string(),
        );
    }

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(current().0).arg("commit");
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
    (no_store, Json(git_vista_git::read_head_branch(&current().0)))
}

/// The working-tree status (Activity/Undo feature, step 1): the parsed output
/// of `git status --porcelain=v2 --branch`, resolved fresh on every request.
///
/// Shelling out to `git status` rather than assembling this from gix keeps the
/// B3 posture of the write endpoints — git itself decides what's staged /
/// modified / conflicted, including every corner case (renames, type changes,
/// sparse checkouts) — and the pure parser lives in core where it's unit-
/// tested. A read, so it works on read-only clones too. Sent `no-store` like
/// the other live reads: the answer changes with every edit in the worktree.
async fn worktree_status() -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = current().0;
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .await
        .map_err(|e| {
            eprintln!("git-vista: /api/status couldn't run git: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't run git: {e}"))
        })?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if msg.is_empty() { "git status failed.".to_string() } else { msg };
        eprintln!("git-vista: /api/status failed: {msg}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
    }
    let parsed = parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout));
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(parsed)))
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

/// Force-delete a branch (Issue #33 follow-up): `git branch -D <branch>`. The
/// uppercase `-D` drops a branch even when its commits aren't merged, discarding
/// any it alone holds. The UI only reaches here after the safe `git branch -d`
/// (see [`delete_branch`]) was refused for "not fully merged" and the user
/// confirmed the override, so this deliberately skips git's merge safety check.
async fn force_delete_branch(Json(req): Json<BranchRequest>) -> (StatusCode, String) {
    run_branch_op(
        "/api/force-delete-branch",
        &req.branch,
        &["branch", "-D"],
        format!("force-deleted branch '{}'", req.branch.trim()),
    )
    .await
}

/// Rebase the checked-out branch onto main (Issue #33 follow-up): `git rebase
/// <base>`. `<base>` is `origin/main` when that remote-tracking ref exists — the
/// usual feature-branch target, so you rebase onto the freshest pushed main — and
/// the local `main` otherwise. It acts on HEAD, so it takes no request body.
///
/// A failed rebase (almost always conflicts) would leave the repo mid-rebase,
/// which a browser-only user with no shell can't resolve — so on failure it runs
/// `git rebase --abort` to restore the pre-rebase state, then forwards git's own
/// error text so the UI can explain why it couldn't complete.
async fn rebase() -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let repo = current().0;
    let base = if git_ref_exists(&repo, "refs/remotes/origin/main").await {
        "origin/main"
    } else {
        "main"
    };

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("rebase")
        .arg(base)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git-vista: /api/rebase couldn't run git: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't run git: {e}"),
            );
        }
    };

    if output.status.success() {
        println!("[/api/rebase] rebased HEAD onto {base}");
        (StatusCode::OK, format!("Rebased onto {base}."))
    } else {
        // git explains conflicts on stderr (some notices go to stdout); prefer
        // stderr, fall back to stdout, then a generic line — matching the others.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                "git rebase failed.".to_string()
            } else {
                stdout
            }
        };
        // Best-effort: back out of the half-applied rebase so the working tree isn't
        // stuck mid-rebase. Harmless (exits non-zero, ignored) when none is running.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("rebase")
            .arg("--abort")
            .output()
            .await;
        eprintln!("git-vista: /api/rebase failed (aborted): {msg}");
        (StatusCode::BAD_REQUEST, msg)
    }
}

/// Whether `refname` resolves in `repo` (`git rev-parse --verify --quiet`): exit 0
/// when the ref exists, non-zero otherwise. Used to prefer `origin/main` over the
/// local `main` as a rebase base only when the remote-tracking ref is actually there.
async fn git_ref_exists(repo: &Path, refname: &str) -> bool {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg(refname)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let branch = branch.trim();
    if branch.is_empty() {
        return (StatusCode::BAD_REQUEST, "Branch name can't be empty.".to_string());
    }
    if branch.starts_with('-') {
        return (StatusCode::BAD_REQUEST, "Branch name can't start with '-'.".to_string());
    }

    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(current().0)
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
