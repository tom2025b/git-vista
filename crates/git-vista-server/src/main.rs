//! Native HTTP backend for git-vista.
//!
//! git-vista is browser-first: the user runs it in Safari on an iPad, which can't
//! read a git repo itself. This server runs the native git reader
//! ([`git_vista_git::walk_history`]) + the pure layout ([`git_vista_core::layout`])
//! and serves, on a single origin:
//!   - `GET /api/frame` — the once-per-view refs/branch-colours envelope,
//!   - `GET /api/commits` — one cursor-signed [`Page`](handlers::read::Page) of
//!     laid-out history rows/edges/stubs (protocol v4, stateless paging), and
//!   - everything else    — the wasm SPA bundle Trunk builds into the frontend's
//!     `dist/` directory.
//!
//! The frontend `fetch`es `/api/frame` once per view and `/api/commits` per
//! page (same origin, no CORS).
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
use std::path::{Path, PathBuf};

// The activity feed (journal + reflogs + snapshots) — the server-side half of
// the Activity Log / Contextual Undo feature. `journal` owns the on-disk state
// under `.git/git-vista/`; `activity` owns `GET /api/activity`.
mod activity;
// M4.31 (#84): the conflict model and the scan that fills it. Contract and
// scanner first, endpoint later — the same staging `build_plan_only` uses, and
// the same reason: the vocabulary and the index reads get reviewed before any
// route exposes them. `allow(dead_code)` off the test build only, so the day a
// handler wires this up the attribute stops applying on its own rather than
// hiding a genuinely dead function.
#[cfg_attr(not(test), allow(dead_code))]
mod conflicts;
// The server-owned repository catalog (M1.03): opaque repository/worktree ids,
// allowed-root enforcement, and the only path→id resolution in the server.
#[cfg(test)]
mod argv_boundary;
mod catalog;
// M1.07 (#60): the per-repository guard every app mutation acquires, plus the
// external-git busy check. Serialization lives here and nowhere else.
mod coordinator;
// M1.09 (#62): the SQLite operation journal (survives a restart) and the
// private git recovery refs a completed mutation's recovery strategy pins.
mod durable;
mod git_cmd;
// #581: the running git's version, established once for every feature whose
// floor is above the documented product floor of 2.32 — the graph preview
// (2.38, ADR 0099) and the revert offer (2.38, same plumbing).
mod git_version;
mod handlers;
// M1.10 Task 3 (#63): the paged-history snapshot (refs + HEAD + shallow), its
// `history-v1` generation token, and the Frame/Page representation validators.
// Task 4 wires these into the `/api/frame` and paged `/api/commits` handlers
// registered by `api_router` below, sharing the one `Arc<CursorCodec>` minted
// in `main`.
mod history;
mod journal;
// The versioned-API-contract layer (M1.02, #102): protocol negotiation, the
// request id, the structured error envelope, and the contract response headers.
mod middleware;
// M1.08 (#61): the operation registry — idempotency keys, operation ids, the
// lifecycle state machine, and the replayable terminal result every write is
// recorded under.
mod operations;
// The shared write planner (M1.06b, #143): every write handler builds a typed
// GitOperation and this module builds/validates/executes its reviewable Plan —
// the only place a mutating git argv is constructed.
mod planner;
// M10.08 (#576, ADR 0099): the graph preview — draw the repository as it
// *would* be after a Plan, writing nothing to it. Real git computes the answer
// in a throwaway object store inside the repository's own commondir, whose
// `objects/info/alternates` lets it read every object and write none of them
// back. Refuses (`Unsupported`/`Unavailable`) rather than modelling.
mod preview;
// Per-source-IP sign-in rate limiting for the LAN listener (ADR 0005, #122).
mod ratelimit;
// M3.25 (#78): the Recovery Center — a browsable history of this app's own
// operations over `durable`'s journal, each row's recovery classified live
// against the repository, and the one endpoint that executes such a recovery
// after re-deriving and matching it server-side. See
// docs/superpowers/specs/2026-08-18-m3-recovery-center.md.
mod recovery_center;
#[cfg(test)]
mod route_authz;
// M1.13b (#66): the git-process sandbox — the pure argv chokepoint, the tier
// enum, the gitdir validation, and the spawn wrappers every production spawn
// site goes through. The fused shim it launches is `src/bin/gv-sandbox.rs`.
mod sandbox;
// The loopback session + request-protection layer (M1.04, #57): Origin/Host/CSRF/
// content-type/method enforcement, the browser hardening headers, and the
// bootstrap-token → session-cookie exchange.
mod security;
mod session;
mod staging;
mod state;
// GitHub token resolution (#583, M13.02): keyring, then environment, then a
// gitignored local file — the fallback chain `state::credential_token`
// (M13.01, #582) was scaffolding for. See ADR 0122.
mod token_store;
// M12.02 (#552): native filesystem hints over the selected worktree's Git
// metadata. The authoritative sweep/feed lands in later M12 slices, so this
// tested module is intentionally staged before production wiring reaches it.
#[cfg_attr(not(test), allow(dead_code))]
mod watcher;
// M11.01 (#546): the read-only worktree census (`git worktree list
// --porcelain` resolved into `git_vista_protocol::WorktreeCensus`). Contract
// and query land first, staged the same way `conflicts` was — no route
// exposes this yet; that is M11.03's and the checkout-collision
// precondition's, not this issue's. See docs/superpowers/specs/
// m3.23-worktrees.md §1.
//
// The `allow(dead_code)` sits on the entry point `worktree_census` itself, not
// on this `mod` line — deliberately narrower than `conflicts` above. Its
// internal helpers (`correlate_missing_admin_dir`, `common_dir`,
// `is_null_oid`, `display_name`) are each called from within the module, so a
// future edit that orphans one should still trip the dead-code lint rather
// than being exempted by a blanket module-level attribute. (`conflicts`' own
// module-level attribute is a leftover from when *it* was staged this way; it
// has had real callers since — `conflicts::scan` from `planner/conflict_exec.rs`
// and `handlers/conflicts.rs`, `conflicts::continuation` from
// `planner/sequence_exec.rs` and `planner/stash.rs` — so the contrast is with
// where the attribute sits, not with whether that module is reached.)
mod worktree_census;
// M1.13b (#66): the single owner of TCP port 9418 in the test binary. Three
// tests across `sandbox::escape_suite` and `planner::contract_suite` need that
// one port (it is the only unprivileged entry in `DEFAULT_GIT_PORTS`, so the
// only port a Network-tier Landlock connect grant covers) and `cargo test` runs
// them concurrently in one process.
#[cfg(test)]
mod test_ports;
// #588: the selected repository belongs to the session, not the process. Its
// own suite because it drives two concurrent sessions through the real router
// rather than calling any one module's functions.
#[cfg(test)]
mod session_selection_suite;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::response::IntoResponse;
use axum::{
    http::{header, HeaderValue, StatusCode},
    routing::{get, post},
    Extension, Router,
};
use tower::Layer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use git_vista_protocol::{ListenerProfile, RepoMode, LISTENER_PROFILE_HEADER};
use handlers::branch::{
    checkout_branch, create_branch, delete_branch, force_delete_branch, merge_branch, push_branch,
};
use handlers::clone::{clone_repo, clone_status, delete_clone_repo};
use handlers::commit::{amend_commit, cherry_pick, create_commit, stage_all, unstage_all};
use handlers::conflicts::{
    blob_content, conflict_source, list_conflicts, resolve_conflict, resolve_conflict_content,
    worktree_file,
};
use handlers::discard::{delete_untracked_paths, discard_tracked_paths};
use handlers::fetch::fetch_remote;
use handlers::plan::{execute_plan, plan_operation};
use handlers::preview::preview_plan;
use handlers::protocol::protocol_info;
use handlers::pull::pull_branch;
use handlers::read::{
    commit_detail, commit_diff, commits, file_at_commit, frame, head_branch, spec_diff,
    worktree_status, worktree_status_v2,
};
use handlers::rebase::{rebase, rebase_status};
use handlers::reset::reset_test_repo;
use handlers::select::{rescan, select_repo};
use handlers::session::{create_session, revoke_session, session_status, SessionState};
use handlers::staging::{staging_apply, staging_diff, staging_preview};
use history::CursorCodec;
use ratelimit::SignInLimiter;
use security::{AuthState, HostPolicy};
use session::{SessionManager, BOOTSTRAP_REFRESH_INTERVAL};
use state::{
    bind_addr, bootstrap_token_path, current, lan_bind_addr, set_current, DEFAULT_REPO, DIST_DIR,
    PORT,
};

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
    // M2.21e (#239): a signed `git tag -s` shells out to gpg, which — if it
    // ever got far enough to try a curses pinentry, which it cannot today
    // (the Strict-tier sandbox denies gpg-agent's AF_UNIX socket outright;
    // see `seccomp_filter.rs`'s `af_unix_rule`) — would take the tty name to
    // attach from this variable. Clearing it process-wide is defense in
    // depth: without a `GPG_TTY`, the agent has no terminal to open a
    // pinentry prompt on and fails with an error instead, even in a future
    // where some carve-out reopens the socket this line does not depend on.
    std::env::remove_var("GPG_TTY");

    // M1.13b (#66) Task 9 — INV-13 / Global Constraint 15's boot gate. This
    // must run before ANYTHING in this process spawns a git process — the
    // whole point of the gate is that a host whose sandbox does not actually
    // compose never executes a byte of repository content, and
    // `durable::recover()` a little further down is exactly such a spawn (it
    // writes recovery refs via the sandboxed launcher). So this sits here,
    // ahead of every catalog registration and ahead of `durable::recover()`,
    // not merely ahead of the listener binds. There is no degrade: a verdict
    // other than `Contained` means no server, full stop (ADR 0029).
    if let Err(refusal) = sandbox::probe::run_at_startup().await {
        eprintln!("error: {refusal}");
        std::process::exit(1);
    }

    // #583 (M13.02): say which token-storage tier answered, masked, at boot —
    // never only silently. Prints one ordinary line when nothing is
    // configured; that is expected for a public repository and not a
    // warning.
    println!("{}", token_store::provenance_line());

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
    // The CLI-arg repo is the user's own working repo, so it opens Active. This
    // registers it in the catalog (M1.03) and makes it the default selection.
    set_current(&repo, RepoMode::Active);

    // ADR 0009: register every direct-child repo of the configured root, so the
    // picker can offer them. No root configured → exactly the old behavior.
    if let Some((registered, skipped)) = state::scan_repo_root() {
        println!("git-vista: repo root scan: {registered} registered, {skipped} skipped");
    }

    // ADR 0009 (list form): register each explicitly-named repository. Runs
    // alongside the root scan rather than instead of it — an operator may
    // reasonably want "everything in ~/work, plus these two elsewhere", and
    // registration is idempotent on identity, so a path named by both is
    // admitted once.
    let (listed, listed_skipped) = state::register_repo_list();
    if listed > 0 || listed_skipped > 0 {
        println!("git-vista: repo list: {listed} registered, {listed_skipped} skipped");
    }

    // ADR 0008: clones persist across runs. Re-register every clone surviving
    // under the clones root so the picker keeps offering it after a restart.
    let (clones_registered, _) = state::scan_clones_root();
    if clones_registered > 0 {
        println!("git-vista: {clones_registered} persistent clone(s) re-registered");
    }

    // ADR 0118 (M11.04, #549): the managed worktrees root is admitted to the
    // allowed roots HERE, by being scanned — that admission is what makes
    // "inside the fence by construction" true of every desk created under it.
    // Runs even when the root is empty, and even when it does not yet exist:
    // the scan creates it, and the point is the `allow_root`, not the count.
    let (desks_registered, _) = state::scan_worktrees_root();
    if desks_registered > 0 {
        println!("git-vista: {desks_registered} linked worktree(s) re-registered");
    }

    // M1.09 (#62): reload the durable operation journal. Anything left
    // non-terminal by a prior process is closed out as interrupted here (see
    // `durable`'s module docs for why that's the correct answer) before the
    // registry is repopulated, so `GET /api/operations/{id}` and idempotency
    // replay both keep working for operations admitted before this restart.
    let recovered = durable::recover().await;
    if !recovered.records.is_empty() {
        println!(
            "git-vista: {} operation(s) reloaded from the journal",
            recovered.records.len()
        );
    }
    // #509: rows written by a build that understood an operation this one
    // does not. Their keys are guarded against reuse; the rows themselves
    // surface in the Recovery Center's history.
    if !recovered.incompatible.is_empty() {
        println!(
            "git-vista: {} journal row(s) from an incompatible build; their keys are reserved",
            recovered.incompatible.len()
        );
    }
    operations::rehydrate(recovered.records, recovered.incompatible);

    // Warn early if the SPA hasn't been built — otherwise every page is a 404
    // and it looks like the server is broken.
    if !Path::new(DIST_DIR).exists() {
        eprintln!("warning: the web bundle isn't built yet ({DIST_DIR} is missing).");
        eprintln!(
            "         run `(cd crates/git-vista && trunk build)` first, or pages will 404.\n"
        );
    }

    // Resolve the fixed loopback address first. `bind_addr` rejects every
    // non-loopback override so neither a stale launcher nor a service file can
    // expose this plain-HTTP control surface.
    let addr = match bind_addr() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };

    // Reserve the port before creating the session manager. SessionManager
    // publishes a new bootstrap token as part of construction; doing that first
    // meant a second server that later failed with AddrInUse could overwrite the
    // token file belonging to the healthy live server. The operator would then
    // receive links that could only answer 401 until the live server restarted.
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("error: could not bind {addr}: {error}");
            if error.kind() == ErrorKind::AddrInUse {
                eprintln!(
                    "  Port {PORT} is already in use — another git-vista-server may be running."
                );
                eprintln!("  Run `gv doctor`, then stop it with its owning launcher/service.");
            }
            std::process::exit(1);
        }
    };

    // ADR 0005: resolve the optional second, LAN-facing listener. `gv` is
    // responsible for auto-detecting the LAN IP or requiring --lan-ip before
    // ever setting GIT_VISTA_LAN_IP, so a rejection here is a clean startup
    // error, matching the loopback bind_addr() error path above.
    let lan_addr = match lan_bind_addr() {
        None => None,
        Some(Ok(addr)) => Some(addr),
        Some(Err(error)) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };
    let lan_listener = match lan_addr {
        Some(addr) => match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                eprintln!("error: could not bind LAN listener {addr}: {error}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    // M1.04: mint the one-time bootstrap token (written 0600 for `gv` to read) and
    // build the shared session store. The auth layer and the session handlers both
    // hold this `Arc`; the Host/Origin policy is strict loopback-only.
    let sessions = Arc::new(SessionManager::new(Some(bootstrap_token_path())));
    // Keep the launcher-visible one-time link usable on a long-running server.
    // Each individual token still expires within one hour; this task replaces it
    // before that deadline and never exposes or logs the secret.
    let bootstrap_refresher = sessions.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(BOOTSTRAP_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            bootstrap_refresher.refresh_bootstrap_if_expiring();
        }
    });
    let loopback_session_state = SessionState {
        manager: sessions.clone(),
        via_lan: false,
        rate_limiter: None,
    };
    // M1.10 (#63): the one `Arc<CursorCodec>` shared by both listeners, so a
    // history cursor minted against the loopback router decodes on the LAN
    // router too (and vice versa) — two independently-minted codecs would
    // silently reject every cursor across listeners.
    let history_codec = Arc::new(CursorCodec::new());
    let loopback_app = build_app(
        loopback_session_state,
        HostPolicy::loopback(PORT),
        true,
        history_codec.clone(),
    );

    print_startup_banner(&bootstrap_token_path(), lan_addr);

    match lan_listener {
        Some(lan_listener) => {
            let lan_ip = lan_addr.expect("lan_listener implies lan_addr").ip();
            // ADR 0005: sign-in on the LAN listener is rate-limited per source
            // IP; the loopback listener above stays unlimited.
            let lan_session_state = SessionState {
                manager: sessions.clone(),
                via_lan: true,
                rate_limiter: Some(Arc::new(SignInLimiter::new())),
            };
            let lan_app = build_app(
                lan_session_state,
                HostPolicy::lan(lan_ip, PORT),
                false,
                history_codec.clone(),
            );
            let loopback_serve = axum::serve(
                listener,
                loopback_app.into_make_service_with_connect_info::<SocketAddr>(),
            );
            let lan_serve = axum::serve(
                lan_listener,
                lan_app.into_make_service_with_connect_info::<SocketAddr>(),
            );
            if let Err(e) = tokio::try_join!(loopback_serve, lan_serve) {
                eprintln!("error: server stopped: {e}");
                std::process::exit(1);
            }
        }
        None => {
            if let Err(e) = axum::serve(
                listener,
                loopback_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("error: server stopped: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// The `/api/*` route table plus its auth/contract layers, for one listener.
/// `full_routes` selects whether the write/select/rescan/clone endpoints are
/// registered at all: `true` for the loopback listener, `false` for the LAN
/// listener (ADR 0005) — those routes are never *built* on the LAN router, not
/// merely gated, so a mode-check regression can't reopen them. Kept separate
/// from [`build_app`] so a test can exercise route registration directly,
/// without the static-file fallback (and its `DIST_DIR` dependency) in the way.
fn api_router(
    session_state: SessionState,
    hosts: HostPolicy,
    full_routes: bool,
    codec: Arc<CursorCodec>,
) -> Router {
    // #589: declare the profile from the exact boolean that selects the route
    // table below.  It is not reconstructed from Host, peer address, or
    // `SessionState::via_lan`, any of which would create a second source of
    // truth about what this router can honour.
    let listener_profile = ListenerProfile::from_write_routes(full_routes);
    let auth_state = AuthState {
        manager: session_state.manager.clone(),
        hosts,
    };

    // Every `/api/*` route lives on this sub-router so the M1.02 contract layer
    // (protocol negotiation, request id, structured errors, response headers)
    // wraps them all — and only them, never the static SPA served alongside it.
    let mut api = Router::new()
        // M1.10 (#63): protocol v4's stateless paged history. `/api/frame` is
        // the once-per-view refs/branch-colours envelope; the paged
        // `/api/commits` replaces the old whole-graph read with one
        // cursor-signed window. Both read the one shared `Arc<CursorCodec>`
        // below, so a cursor minted on one listener decodes on the other.
        .route("/api/frame", get(frame))
        .route("/api/commits", get(commits))
        // The one unversioned endpoint: a client hits it to learn the protocol
        // before it can be required to speak it (so it's exempt from the header
        // check inside the contract layer).
        .route("/api/protocol", get(protocol_info))
        // M1.03: the capability report — which repositories are servable, each
        // addressed by an opaque id, with no filesystem paths by default.
        .route("/api/catalog", get(handlers::catalog::catalog_list))
        // M1.04: establish (POST, bootstrap→cookie), check (GET), or revoke
        // (DELETE) a session. GET/POST are exempt from the session gate — they are
        // how a session comes to exist — but never from the Host/Origin checks.
        .route(
            "/api/session",
            get(session_status)
                .post(create_session)
                .delete(revoke_session),
        )
        // Phase 10: full detail for one commit, read on demand for the side panel.
        .route("/api/commit/{id}", get(commit_detail))
        // Activity/Undo feature, step 2: one commit's diff (file list + patch),
        // read on demand when the detail panel opens. `?full=1` lifts the patch
        // cap for the full-screen diff viewer.
        .route("/api/diff/{id}", get(commit_diff))
        // Full file viewer: one file's whole content at one commit (`git show
        // <id>:<path>`), read on demand when a file in the diff list is tapped.
        .route("/api/file/{id}/{*path}", get(file_at_commit))
        // Issue #33 follow-up: the live checked-out branch, resolved fresh on every
        // request so the merge dialog shows the true target even without a Refresh.
        .route("/api/head-branch", get(head_branch))
        // Working-tree status (Activity/Undo feature, step 1): branch, ahead/
        // behind, and the staged/unstaged/untracked/conflicted file lists —
        // resolved fresh per request, like `head_branch`.
        .route("/api/status", get(worktree_status))
        // #68c: the generation-tagged WorktreeStatus DTO (#68a/#68b), additive
        // alongside the v1 shape above — not a replacement. See handlers::read
        // for why both exist side by side.
        .route("/api/status/v2", get(worktree_status_v2))
        // M2.21b (#236): every tag with the metadata the `/api/frame` ref
        // badges throw away — lightweight vs annotated, the peeled target, and
        // an annotated tag's own object, tagger and message. A read of
        // committed, published history like `/api/frame`, so it is registered
        // on the LAN router too; it never discloses working-tree state.
        .route("/api/tags", get(handlers::tags::tag_list))
        // M3.24 (#77): a read, so the LAN router sees it like every other
        // listing. Showing the drawer is useful before any write path exists.
        .route("/api/stashes", get(handlers::stash::stash_list))
        .route("/api/stash/show", get(handlers::stash::show_stash))
        // Activity/Undo feature, step 3: the chronological event feed —
        // journal + reflogs + snapshot diffs, folded and attributed.
        .route("/api/activity", get(activity::activity_feed))
        // Activity/Undo feature, step 5: the undo actions for one commit, computed live.
        .route("/api/undoables/{id}", get(activity::undoables))
        // Whether "Rebase onto main" would do anything right now — the menu
        // disables the item (with the reason) when it wouldn't.
        .route("/api/rebase-status", get(rebase_status));

    // ADR 0005: every write / repo-selection / clone endpoint is registered
    // only when full_routes is set — the LAN router never sees these routes
    // exist at all.
    if full_routes {
        api = api
            // Phase 12: clone a public URL into a temp dir and view it read-only.
            .route("/api/clone", post(clone_repo))
            // #263: what happened to a clone attempt admitted under an
            // idempotency key — the recovery channel for a client that lost
            // the `POST /api/clone` response above and wants to reconcile
            // without re-POSTing. Registered alongside `/api/clone`, same
            // reasoning as `/api/operations/{id}` below: this describes a
            // write's outcome, so the LAN router must never see it either
            // (ADR 0005).
            .route("/api/clone-status/{key}", get(clone_status))
            // ADR 0008: delete a persistent clone (catalog entry + directory),
            // guarded to paths that canonicalize inside the clones root.
            .route("/api/delete-clone", post(delete_clone_repo))
            // ADR 0007: pick the current repository + Visualize/Active mode by id.
            .route("/api/select", post(select_repo))
            // M11.03 (#548): switch to a linked worktree of the served
            // repository, addressed by the opaque id the census reports. A
            // second door rather than a widening of `/api/select` — see
            // `SelectWorktreeRequest`'s doc for the authority each one uses,
            // and why the fail-closed 404 above must keep meaning what it
            // means. `full_routes` only, like the census read it depends on.
            .route(
                "/api/select-worktree",
                post(handlers::select::select_discovered_worktree),
            )
            // M11.05 (#550): close a linked sibling desk, addressed by the
            // same opaque census id `/api/select-worktree` uses. Its own
            // route rather than the generic `/api/plan` seam — see
            // `handlers::worktrees`'s module doc.
            .route(
                "/api/remove-worktree",
                post(handlers::worktrees::remove_worktree),
            )
            // ADR 0009: re-scan the configured repo root without a restart.
            .route("/api/rescan", post(rescan))
            // Issue #18: create a branch at a commit (shells out to `git branch`).
            .route("/api/branch", post(create_branch))
            // Issue #33: create a commit on top of HEAD (shells out to `git commit`).
            .route("/api/commit", post(create_commit))
            // M2.19b (#223, ADR 0040): rewrite the tip commit in place
            // (`git commit --amend`, compare-and-swapped on the tip the
            // client reviewed). Its own route, deliberately — see
            // `handlers::commit::amend_commit` for why this must never be a
            // widened `/api/commit` body.
            .route("/api/amend-commit", post(amend_commit))
            // M10.09 (#596): cherry-pick one commit onto the checked-out
            // branch. A dedicated write route, not the generic `/api/plan` +
            // `/api/execute-plan` pair, so the frontend's write carries an
            // idempotency key and lands in the operations registry like every
            // other mutation — see `handlers::commit::cherry_pick`.
            .route("/api/cherry-pick", post(cherry_pick))
            // Stage the working tree (`git add -A`) so the UI can stage, then commit.
            .route("/api/stage", post(stage_all))
            // The staging-selection surface (M2.17b, #213). All three under
            // full_routes on purpose — the diff read exists only to feed the
            // write surface, and a LAN visualize session never sees
            // uncommitted worktree contents (ADR 0005).
            .route("/api/staging/diff", get(staging_diff))
            .route("/api/staging/preview", post(staging_preview))
            .route("/api/staging/apply", post(staging_apply))
            // Explicit source/target diffs (M2.16, #69) — the four DiffSpec
            // modes. POST because DiffSpec is an internally-tagged enum whose
            // variants carry different fields; a query string can only express
            // that by flattening it back into loose optional parameters, which
            // is the un-explicit shape the type exists to remove (same reason
            // /api/plan is a POST).
            //
            // Under full_routes with the staging reads, not beside
            // /api/diff/{id}: two of the four modes (WorktreeVsIndex,
            // IndexVsCommit) expose uncommitted worktree and index content,
            // which ADR 0005's LAN profile withholds. The commit-vs-commit
            // modes would be safe on the LAN listener, but gating by variant
            // would put a security boundary inside a match arm — where a later
            // variant inherits whichever side someone forgets to consider.
            .route("/api/diff/spec", post(spec_diff))
            // M4.31a (#428): inspect a conflict. All three GETs, full_routes
            // only — the decision recorded on the issue itself before this
            // landed. `/api/conflicts` reports the stage entries of an
            // in-progress merge (uncommitted index state by definition);
            // `/api/blob/{oid}` addresses conflict stage blobs, which are
            // **index** objects with no guarantee of being reachable from
            // any commit, so "it is just a blob" is not `/api/file`'s
            // guarantee; the worktree read is uncommitted by definition.
            // Deliberately NOT gated by "is this OID reachable from a
            // commit?" to make `/api/blob` LAN-safe — that is exactly the
            // by-variant gating `/api/diff/spec`'s own comment above rejects,
            // one call site over: the whole route is withheld instead.
            // M11.02 (#547): the worktree census. `full_routes` only, on the
            // same line ADR 0005 draws for the three routes below it — a
            // sibling's directory base name (and, when the operator opts in,
            // its absolute path) is filesystem shape, which is not something
            // the LAN router discloses. The frontend needs it to decline a
            // checkout git would refuse; the LAN router offers no checkout to
            // decline.
            .route("/api/worktrees", get(handlers::read::worktree_list))
            .route("/api/conflicts", get(list_conflicts))
            .route("/api/blob/{oid}", get(blob_content))
            .route("/api/worktree-file/{*path}", get(worktree_file))
            // M4.31c (#432), ADR 0069: the marker file plus the `conflict-v1:`
            // token pinning it — what a content resolution's editor seeds
            // from. Same uncommitted-content disclosure as the two routes
            // above; full_routes-only for the same reason.
            .route("/api/conflict-source/{*path}", get(conflict_source))
            // M4.31b (#429): resolve one conflicted path by taking a whole
            // side, or removing the file. A write, so the full posture — and
            // it goes through the planner like every other mutation (ADR
            // 0016), never straight to git.
            .route("/api/resolve-conflict", post(resolve_conflict))
            // M4.31c (#432), ADR 0069: a block/line/manual-edit resolution.
            .route(
                "/api/resolve-conflict-content",
                post(resolve_conflict_content),
            )
            // …and unstage it again (`git reset HEAD`) — the exact inverse, offered
            // by the menu while anything is staged.
            .route("/api/unstage", post(unstage_all))
            .route("/api/undo", post(activity::undo))
            // Issue #33 follow-up: branch operations, each shelling out to git.
            .route("/api/merge", post(merge_branch))
            .route("/api/push", post(push_branch))
            // M2.20c (#229, ADR 0043): fetch from a configured remote. The
            // first write here that can take a minute, so it is also the
            // first one whose progress is worth streaming and whose
            // cancellation has to actually kill a process — see
            // `planner::fetch` and the cancel route below.
            .route("/api/fetch", post(fetch_remote))
            // M2.20d (#230, ADR 0044): fetch and then integrate. Its own route
            // rather than a flag on `/api/fetch`, because it is a different
            // operation with a different risk (a fetch is additive; a pull
            // moves the checked-out branch) and — the reason that matters —
            // its request body carries the mandatory merge/rebase strategy a
            // fetch has no field for.
            .route("/api/pull", post(pull_branch))
            .route("/api/delete-branch", post(delete_branch))
            // M2.21d (#238, ADR 0048): the two **local** tag writes. Named to
            // match the `/api/branch` + `/api/delete-branch` pair beside them,
            // and full_routes-gated like every other write — unlike the
            // `GET /api/tags` listing above, which the LAN router does see.
            // M3.24 (#77): the stash drawer's three writes. No
            // `/api/stash/pop` — pop is apply-then-drop and one operation row
            // cannot tell the truth about the half-done state; see
            // handlers/stash.rs.
            .route("/api/stash/push", post(handlers::stash::push_stash))
            .route("/api/stash/apply", post(handlers::stash::apply_stash))
            .route("/api/stash/drop", post(handlers::stash::drop_stash))
            .route(
                "/api/stash/branch",
                post(handlers::stash::branch_from_stash),
            )
            .route("/api/tag", post(handlers::tags::create_tag))
            .route("/api/delete-tag", post(handlers::tags::delete_tag))
            // M2.21f (#240): the two **remote** tag writes — each opens a
            // socket with credentials on it, the same posture `/api/fetch`
            // and `/api/pull` document below, so full_routes-only like every
            // write here (a LAN visualize session must never publish or
            // delete a tag on a remote).
            .route("/api/push-tag", post(handlers::tags::push_tag))
            .route(
                "/api/delete-remote-tag",
                post(handlers::tags::delete_remote_tag),
            )
            // iPad-testing follow-up: switch HEAD to a branch (`git checkout`).
            .route("/api/checkout", post(checkout_branch))
            // M11.04 (#549), ADR 0118: open a second desk. `full_routes` only,
            // like every other write here. It creates a directory under a root
            // this application owns — the request names the desk, never its
            // location, so there is no path in the body for a traversal to
            // hide in.
            .route("/api/add-worktree", post(handlers::branch::add_worktree))
            // Issue #33 follow-up: force-delete an unmerged branch (`git branch -D`),
            // offered only after the safe `-d` above is refused; and rebase the
            // checked-out branch onto main (`git rebase`).
            .route("/api/force-delete-branch", post(force_delete_branch))
            .route("/api/rebase", post(rebase))
            // iPad-testing follow-up: restore a seeded *test repo* to its recorded
            // state (gated on the seed files `gv --seed` writes).
            .route("/api/reset-test-repo", post(reset_test_repo))
            // #219 (M2.18a): discard uncommitted changes to tracked paths
            // (`git checkout -- <paths>`), or delete untracked paths outright
            // (`git clean -f -- <paths>`) — two separate operations (#71),
            // the second with no journal-backed undo at all.
            .route("/api/discard-tracked-paths", post(discard_tracked_paths))
            .route("/api/delete-untracked-paths", post(delete_untracked_paths))
            // M2.23d (#248, ADR 0046): build one reviewable Plan and hand it
            // back — the only endpoint that mints a plan without running it,
            // and what the MCP `plan_*` tools call. Registered here with the
            // writes, not with the reads, for two independent reasons: it is
            // the front half of a mutation (a plan is an approval token, not
            // a report), and a LAN visualize session must never see one
            // (ADR 0005). Executing an approved plan is the route below.
            .route("/api/plan", post(plan_operation))
            // M2.23e (#249, ADR 0046 continued): submit a plan `/api/plan`
            // built for execution. Same registration reasoning as `/api/plan`
            // immediately above — a plan submission is itself a mutation, and
            // a LAN visualize session must never reach it either (ADR 0005).
            .route("/api/execute-plan", post(execute_plan))
            // M10.08 (#576, ADR 0099): the graph a Plan would produce, drawn
            // by real git in a throwaway object store and written nowhere.
            // Registered here with the writes rather than with the reads, for
            // the same two reasons `/api/plan` is: it takes a Plan body, which
            // only a POST can carry, and a LAN visualize session must never
            // reach the plan-review surface at all (ADR 0005). It refuses
            // nothing on mode, though — a read-only repository gets the named
            // `Unavailable { RepositoryReadOnly }` answer instead of a 403;
            // see the handler's own module doc.
            .route("/api/preview", post(preview_plan))
            // M2.20f (#232): what operation id was admitted for an
            // idempotency key, readable while the operation it names may
            // still be running — closes the race where `POST /api/fetch`
            // (or any tracked write) doesn't answer with `OPERATION_HEADER`
            // until the operation is already terminal
            // (`planner::plan_and_execute_tracked` ends with
            // `record.wait_terminal().await`). Registered here, with the
            // writes, for the same ADR 0005 reason as `/api/operations/{id}`
            // immediately below: this describes an in-flight write's
            // identity, so the LAN router must never see it either.
            .route(
                "/api/operations/by-key/{key}",
                get(handlers::operations::operation_by_key),
            )
            // M1.08 (#61): what happened to one write, and its live progress.
            // Registered with the writes, not the reads — these describe write
            // outcomes, so the LAN router must never see them either (ADR 0005).
            .route(
                "/api/operations/{id}",
                get(handlers::operations::operation_status),
            )
            .route(
                "/api/operations/{id}/events",
                get(handlers::operations::operation_events),
            )
            // M2.20c (#229): ask a running operation to stop — it kills the
            // running child process, so it is a write (it changes what the
            // server does), not a read of a write's outcome like the two
            // routes above it, and carries the full session + CSRF posture
            // like every other POST here.
            .route(
                "/api/operations/{id}/cancel",
                post(handlers::operations::cancel_operation),
            )
            // M3.25 (#78): the Recovery Center. The list is a read of write
            // outcomes — same posture and same ADR 0005 reasoning as
            // `/api/operations/{id}` above — and the recover endpoint is a
            // full write: it runs git through the ordinary planner.
            .route(
                "/api/operations/history",
                get(recovery_center::operation_history),
            )
            .route(
                "/api/operations/{id}/recover",
                post(recovery_center::recover_operation),
            );
    }

    api
        // Innermost: the M1.08 idempotency scope. Inside the auth gate, so an
        // unauthenticated request never mints an operation, and inside the panic
        // catch, so the id it stamps is on a real handler response.
        .layer(axum::middleware::from_fn(middleware::idempotency))
        // A panicking handler becomes a 500 with the panic text (not a reset
        // connection) *before* the layers above see it, so that 500 is
        // rewrapped into the structured error envelope like any other failure.
        .layer(CatchPanicLayer::custom(panic_to_response))
        // Middle: the M1.04 auth gate — Origin/Host/CSRF/content-type/method and a
        // valid session. Inside the contract layer, so its refusals become the same
        // structured envelope; outside the panic catch, so a panic is still caught.
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            security::require_auth,
        ))
        // Outermost: the M1.02 versioned-API contract — protocol negotiation,
        // request id, the consistent error envelope, and the response headers.
        .layer(axum::middleware::from_fn(middleware::api_contract))
        // M1.10 (#63): the one shared `Arc<CursorCodec>` `/api/frame` and the
        // paged `/api/commits` extract via `Extension`. Minted once in `main`
        // and passed into both listener builds — never a fresh codec per
        // router — so a cursor minted on one listener decodes on the other.
        .layer(Extension(codec))
        // The session store the session handlers (and the auth layer) resolve
        // against. Erases the router's state type back to `()`.
        .with_state(session_state)
        // The profile is a property of the listener, not of any one handler.
        // Stamp it at the router boundary so every registered API response
        // declares the capability table that served it.
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static(LISTENER_PROFILE_HEADER),
            HeaderValue::from_static(listener_profile.as_header_value()),
        ))
}

/// Assemble one full application — [`api_router`] plus the static SPA fallback
/// and the two outer layers — for one listener.
fn build_app(
    session_state: SessionState,
    hosts: HostPolicy,
    full_routes: bool,
    codec: Arc<CursorCodec>,
) -> Router {
    let listener_profile = ListenerProfile::from_write_routes(full_routes);
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

    Router::new()
        .merge(api_router(session_state, hosts, full_routes, codec))
        // Anything that isn't the API is served from the built SPA bundle.
        .fallback_service(spa)
        // Global backstop for the static SPA / fallback. The `/api` space has its
        // own inner catch above (so API panics are already enveloped); this keeps
        // any panic outside it from tearing down the connection — which iPad
        // Safari would report as the unexplained "Load failed".
        .layer(CatchPanicLayer::custom(panic_to_response))
        // M1.04: the browser hardening headers (CSP, COOP/CORP, nosniff, …) on
        // *every* response — the SPA shell as well as the API — so framing,
        // cross-origin embedding, and off-origin script/connect are denied
        // everywhere the app is served.
        .layer(axum::middleware::from_fn(security::security_headers))
        // Also cover the static fallback's response to an absent API route.
        // That is the live LAN failure shape: POST /api/select falls through
        // to the file service and receives an ordinary 405.  The response must
        // still say which listener profile produced it.
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static(LISTENER_PROFILE_HEADER),
            HeaderValue::from_static(listener_profile.as_header_value()),
        ))
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

/// Print the supported access paths: local loopback, an SSH tunnel whose remote
/// endpoint is that same loopback listener, and — only when `lan_addr` is
/// `Some` — the LAN view profile's plain-HTTP address and its documented risk.
fn print_startup_banner(token_path: &Path, lan_addr: Option<SocketAddr>) {
    println!("git-vista server — serving {}", current().0.display());
    println!("  • on this machine: http://localhost:{PORT}/");
    println!("  • from the iPad: use an SSH local port forward to 127.0.0.1:{PORT}");
    println!("    example: ssh -N -L {PORT}:127.0.0.1:{PORT} <linux-host>");
    match lan_addr {
        Some(addr) => {
            println!("  • LAN view (ADR 0005, read-only): http://{addr}/");
            println!("    WARNING: plain HTTP — repo contents and the session cookie are");
            println!("    readable by anyone on this network. Trusted home LAN only,");
            println!("    never a guest or shared network.");
        }
        None => println!("  • direct LAN access is disabled"),
    }
    // M1.04: the app needs a one-time session first. The bootstrap token is *not*
    // printed here (it must never land in a log) — `gv` reads the 0600 file and
    // builds the setup URL, or the operator can read it by hand.
    println!("  • sign in: open the setup link `gv` printed (or `gv --token`).");
    println!(
        "    the one-time token lives, 0600, at {}",
        token_path.display()
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use git_vista_protocol::{
        ListenerProfile, SessionInfo, LISTENER_PROFILE_HEADER, PROTOCOL_HEADER, PROTOCOL_VERSION,
    };
    use tower::ServiceExt;

    /// Establish a session against `router` (whichever host it expects) and
    /// return just the `Cookie` header value. Only exercises the session
    /// store (`SessionManager`, process-local to this test) — never touches
    /// the `state::CURRENT`/`CATALOG` globals other tests in this binary
    /// share, so it's safe to run alongside them in any order.
    async fn bootstrap_cookie(router: Router, host: &str, token: &str) -> String {
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header(header::HOST, host)
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 55000))))
                    .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bootstrap should succeed");
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let cookie = set_cookie.split(';').next().unwrap().to_string();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let info: SessionInfo = serde_json::from_slice(&bytes).unwrap();
        assert!(info.authenticated);
        cookie
    }

    /// Read the declaration from a real response. `/api/protocol` is present
    /// on both route tables and needs no session, so this measures only the
    /// listener profile rather than coupling the assertion to repository state.
    async fn declared_profile(router: Router, host: &str) -> ListenerProfile {
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/protocol")
                    .header(header::HOST, host)
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 55000))))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let wire = resp
            .headers()
            .get(LISTENER_PROFILE_HEADER)
            .expect("every API response declares its listener profile")
            .to_str()
            .unwrap();
        ListenerProfile::from_header_value(wire).expect("server emitted a known profile")
    }

    /// The exact live-server symptom behind #589, through `build_app` rather
    /// than the API-only test router: the LAN request falls through to the
    /// static service and receives an ordinary 405, while loopback reaches the
    /// registered route and is stopped by authentication. Both responses still
    /// declare the profile that explains the difference.
    #[tokio::test]
    async fn select_route_presence_and_listener_declaration_cannot_disagree() {
        for (full_routes, via_lan, host, status, profile) in [
            (
                false,
                true,
                "192.168.1.42:8080",
                StatusCode::METHOD_NOT_ALLOWED,
                ListenerProfile::ReadOnly,
            ),
            (
                true,
                false,
                "localhost:8080",
                StatusCode::UNAUTHORIZED,
                ListenerProfile::Full,
            ),
        ] {
            let sessions = Arc::new(SessionManager::new(None));
            let app = build_app(
                SessionState {
                    manager: sessions,
                    via_lan,
                    rate_limiter: None,
                },
                if via_lan {
                    HostPolicy::lan("192.168.1.42".parse().unwrap(), PORT)
                } else {
                    HostPolicy::loopback(PORT)
                },
                full_routes,
                Arc::new(CursorCodec::new()),
            );
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/select")
                        .header(header::HOST, host)
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::CONTENT_TYPE, "application/json")
                        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 55000))))
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), status, "profile {profile:?}");
            assert_eq!(
                resp.headers()
                    .get(LISTENER_PROFILE_HEADER)
                    .and_then(|value| value.to_str().ok()),
                Some(profile.as_header_value()),
                "response and route table disagree for {profile:?}"
            );
        }
    }

    /// Proves the spec's required case directly against the real route table:
    /// a GET to a write-only path passes the session gate (needs only a live
    /// session, not CSRF) and reaches axum's own routing — distinguishing
    /// "path never registered" (404, LAN) from "path registered, wrong
    /// method" (405, loopback) without ever invoking the write handler
    /// itself (which reads the process-global `CURRENT` selection other
    /// tests in this binary also touch).
    #[tokio::test]
    async fn the_lan_router_has_no_write_routes() {
        let sessions = Arc::new(SessionManager::new(None));
        let token = sessions.current_bootstrap();
        let session_state = SessionState {
            manager: sessions,
            via_lan: true,
            rate_limiter: None,
        };
        let router = api_router(
            session_state,
            HostPolicy::lan("192.168.1.42".parse().unwrap(), PORT),
            false,
            Arc::new(CursorCodec::new()),
        );
        assert_eq!(
            declared_profile(router.clone(), "192.168.1.42:8080").await,
            ListenerProfile::ReadOnly
        );
        let cookie = bootstrap_cookie(router.clone(), "192.168.1.42:8080", &token).await;
        // `/api/plan` (M2.23d, #248) is checked here beside `/api/commit`:
        // it mutates nothing, but it mints the approval token #249's submit
        // stage accepts, and it reveals the repository's live generation,
        // preconditions and expected ref changes. A LAN visualize session
        // must not be able to ask for one — ADR 0005 says the route is never
        // *built* on this router, and a 404 is what proves that (a 403 would
        // mean it exists and something gated it).
        for path in ["/api/commit", "/api/plan"] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .header(header::HOST, "192.168.1.42:8080")
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::COOKIE, cookie.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} is registered on the LAN router"
            );
        }
    }

    #[tokio::test]
    async fn the_loopback_router_still_has_write_routes_registered() {
        let sessions = Arc::new(SessionManager::new(None));
        let token = sessions.current_bootstrap();
        let session_state = SessionState {
            manager: sessions,
            via_lan: false,
            rate_limiter: None,
        };
        let router = api_router(
            session_state,
            HostPolicy::loopback(PORT),
            true,
            Arc::new(CursorCodec::new()),
        );
        assert_eq!(
            declared_profile(router.clone(), "localhost:8080").await,
            ListenerProfile::Full
        );
        let cookie = bootstrap_cookie(router.clone(), "localhost:8080", &token).await;
        // /api/commit is registered POST-only; a GET reaches real routing and
        // gets axum's own 405 -- proving the path exists, in contrast to the
        // LAN router's 404 above. This is the paired positive for the LAN
        // test: without it, a `/api/plan` route deleted outright would still
        // 404 there and the LAN assertion would pass vacuously.
        for path in ["/api/commit", "/api/plan"] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .header(header::HOST, "localhost:8080")
                        .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                        .header(header::COOKIE, cookie.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{path} is not registered on the loopback router"
            );
        }
    }

    /// M2.21b (#236) end to end: a real request through the real router — auth
    /// gate, contract layer, route table, handler, `git_vista_git::read_tags`,
    /// and the `TagDetail` mapping — against a repository on disk.
    ///
    /// The expectations are written as literal wire JSON compared against
    /// values read back out of git itself (`rev-parse`, `cat-file`), never
    /// against anything the mapping code produced, so the assertion cannot be
    /// satisfied by a mapping that is merely self-consistent.
    #[tokio::test]
    async fn api_tags_reports_both_tag_kinds_end_to_end() {
        state::with_isolated_test_current(api_tags_reports_both_tag_kinds_end_to_end_in_scope())
            .await;
    }

    async fn api_tags_reports_both_tag_kinds_end_to_end_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = handlers::tags::tests::build_tagged_fixture(dir.path());
        let repo_id = &fixture.repo_id;

        let sessions = Arc::new(SessionManager::new(None));
        let token = sessions.current_bootstrap();
        let session_state = SessionState {
            manager: sessions,
            via_lan: false,
            rate_limiter: None,
        };
        let router = api_router(
            session_state,
            HostPolicy::loopback(PORT),
            true,
            Arc::new(CursorCodec::new()),
        );
        let cookie = bootstrap_cookie(router.clone(), "localhost:8080", &token).await;
        let resp = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/tags?repo={repo_id}"))
                    .header(header::HOST, "localhost:8080")
                    .header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string())
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "as the client sees it, a tag listing is never cacheable. Note this \
             is `security::require_auth`'s doing — it overwrites Cache-Control \
             on every authenticated API response — so this cannot fail if the \
             handler alone stops setting it; that case is covered by \
             `handlers::tags::tests::the_handler_itself_marks_the_listing_no_store`"
        );
        let bytes = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            body,
            serde_json::json!([
                {
                    "name": "tip-marker",
                    "kind": "lightweight",
                    "target": fixture.tip,
                    // Absence as absence: a lightweight tag genuinely has no
                    // object, no tagger and no message.
                    "tag_object": null,
                    "tagger": null,
                    "message": null,
                    "signature": "unsigned",
                },
                {
                    "name": "v1.0",
                    "kind": "annotated",
                    // The peeled commit, not the tag object — the two are
                    // asserted to differ below.
                    "target": fixture.root,
                    "tag_object": fixture.tag_object,
                    "tagger": fixture.tagger,
                    "message": "one\n\nrelease notes",
                    "signature": "unsigned",
                }
            ])
        );
        assert_ne!(
            fixture.tag_object, fixture.root,
            "an annotated tag's object and its target must be different objects, \
             or the assertion above would pass for a handler that never peeled"
        );
    }
}
