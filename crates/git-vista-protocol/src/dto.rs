//! Shared request/response transport DTOs — the exact structs the frontend
//! serialises and the server deserialises across the HTTP/JSON boundary.
//!
//! These are *transport*, not *domain*: wire messages for operations, kept out of
//! `git-vista-core` (which owns the repository/graph domain model) so the API
//! contract versions independently of the internal model. Each request body
//! carries `#[serde(deny_unknown_fields)]` so an unexpected key is a hard
//! deserialization error (a `400`), not a silently-ignored value — part of the
//! guarantee that a request cannot smuggle in a field the server might act on,
//! such as a repository path (the repo is server-selected, never chosen by a
//! request; see `docs/adr/0002-versioned-api-contract.md`).

use serde::{Deserialize, Serialize};

/// Body of a `POST /api/branch` request (Issue #18): create a branch named
/// `name` pointing at the commit `commit` (full hex id). Shared so the frontend
/// serialises exactly what the backend deserialises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBranchRequest {
    pub name: String,
    pub commit: String,
}

/// Body of a `POST /api/commit` request (Issue #33): create a commit with the
/// message `message`. When `allow_empty` is true the commit is made even with
/// nothing staged (`git commit --allow-empty`); otherwise git commits the
/// staged changes and fails if there are none.
///
/// `branch` names the branch the commit should land on. `None` — and any name
/// that turns out to be the checked-out branch — means a plain `git commit` on
/// HEAD, exactly as before this field existed. A *different* branch is allowed
/// only for empty commits (there's no meaning to committing HEAD's staged tree
/// onto another branch): the backend writes the commit with `git commit-tree`
/// and advances the ref with a compare-and-swap `git update-ref`, never
/// touching HEAD or the working tree. This is how a branch stub — a new branch
/// with no commits of its own — takes its first (empty) commit from the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCommitRequest {
    pub message: String,
    pub allow_empty: bool,
    #[serde(default)]
    pub branch: Option<String>,
}

/// Body of the branch-operation requests (Issue #33 follow-up): merge
/// (`POST /api/merge`), push (`POST /api/push`), delete (`POST /api/delete-branch`),
/// force-delete (`POST /api/force-delete-branch`), and checkout (`POST /api/checkout`).
/// All act on a single named branch, so they share one shape. `branch` is a
/// local branch name; the backend validates it and forwards git's own error text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRequest {
    pub branch: String,
}

/// Body of a `POST /api/clone` request (Phase 12): clone the public repository at
/// `url` into the persistent clones store (ADR 0008) and open it look-only
/// pending the operator's mode choice. `url` is a git-cloneable URL (typically
/// `https://…`); the backend
/// validates its scheme with [`validate_clone_url`] and forwards git's own error
/// text on failure. There is deliberately no destination field — the server picks
/// the clone directory, so a request can never point the server at a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneRequest {
    pub url: String,
}

/// Which experience a repository is opened in (ADR 0006/0007). `Visualize` is
/// look-only: the server refuses every mutation while it is the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoMode {
    Visualize,
    Active,
}

/// Body of `POST /api/select` (ADR 0007): make the repository addressed by the
/// opaque `worktree` id the current selection, opened in `mode`. The id resolves
/// only through the server-owned catalog — an unknown/forged id is a 404, and
/// like every request body this cannot carry a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectRequest {
    pub worktree: String,
    pub mode: RepoMode,
}

/// Body of `POST /api/delete-clone` (ADR 0008): delete the *clone* addressed by
/// the opaque `worktree` id — its catalog entry and its directory. The server
/// refuses any id whose path does not canonicalize inside the clones root, so
/// this can only ever remove server-made clones, never a user repository; it
/// also refuses the currently open repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCloneRequest {
    pub worktree: String,
}

/// Body of a `POST /api/session` request (M1.04, #57): the one-time bootstrap
/// `token` the SPA read from the `#s=<token>` URL fragment, exchanged for an
/// HttpOnly session cookie. The only `/api` write body that legitimately carries
/// a secret — which is why it travels in the JSON body (never a query string, so
/// it can't land in a server log) and the endpoint is served over loopback only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    pub token: String,
}

/// The sandbox policy a repository's hooks actually run under
/// (`SECURITY_MODEL.md:236`, #66 — declared by ADR 0025, made real by M1.13b's
/// tier dispatch). Not a `bool`, for ADR 0015's reason: a closed, *named*
/// vocabulary can grow without a breaking wire change, which is exactly what
/// happened here.
///
/// # These four names are the server's own `sandbox::Tier`, plus one
///
/// [`Strict`](Self::Strict), [`Network`](Self::Network) and
/// [`Unsandboxed`](Self::Unsandboxed) are the three tiers
/// `git-vista-server`'s `sandbox::tier_for` dispatches to, reported on the
/// wire under the same names so the disclosed value and the enforced value
/// cannot drift into two vocabularies. [`Blocked`](Self::Blocked) is the
/// fourth: hooks suppressed outright.
///
/// # What changed from ADR 0025, and why it is not additive
///
/// ADR 0025 shipped two variants, `allow` / `restricted`, *session*-scoped and
/// **declared, not enforced** — no code read them. INV-15 needs a per-repository
/// value that names the tier the git-spawn chokepoint really uses, and the
/// banner polarity inverts with it (ADR 0025 flew the banner on `allow`; INV-15
/// flies it on anything that is **not** `strict` — see [`requires_banner`]).
/// The old wire strings still *deserialize* (`restricted` → `Strict`, `allow` →
/// `Unsandboxed`) so a stored older value is not a hard error, but they are
/// never emitted again. This is a value-domain change, not an additive field:
/// see the note in `dto_golden.rs` when its fixture is regenerated.
///
/// [`requires_banner`]: Self::requires_banner
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPolicy {
    /// Landlock + seccomp inside a bwrap pid/net/ipc/uts/cgroup namespace
    /// (`sandbox::Tier::Strict`). Hooks run, and they reach no network and no
    /// unix socket.
    ///
    /// **This is not "confined to the repository."** The same policy also
    /// grants read-only system trees, `$HOME` read-only minus the secret
    /// exclusion set, and a private `/dev` read-write. What it guarantees is
    /// narrower and stated in the server's own `sandbox` module docs: nothing
    /// is *written* outside the declared read-write trees, and nothing is
    /// *read* outside the declared read-only grants minus `secret_excludes`.
    /// The banner is silent for this one variant only, so overstating it here
    /// would be the one place a reader could be misled.
    #[serde(alias = "restricted")]
    Strict,
    /// Landlock + seccomp with no network namespace (`sandbox::Tier::Network`)
    /// — the only tier under which `git push`/`fetch`/`clone` can work, because
    /// a network namespace breaks DNS resolution. Hooks run with `AF_INET`
    /// reachable, which is why this flies the banner even though it is
    /// sandboxed.
    #[serde(alias = "allow")]
    Network,
    /// No sandbox at all (`sandbox::Tier::Unsandboxed`). Reachable only through
    /// explicit, persisted, per-repository operator trust
    /// (`sandbox::trust::grant`), and it flies a permanent banner.
    Unsandboxed,
    /// Hooks do not run at all — `core.hooksPath` pointed at a server-owned
    /// empty directory.
    ///
    /// **No production policy constructor yields this today**, and that is
    /// checked, not merely asserted: `sandbox::escape_contract`'s R8 scan fails
    /// if any production `Policy` literal in the server sets a blocked hook
    /// mode. The variant exists because [`default`](Self::default) needs a
    /// value meaning "hooks are not known to be running," and because ADR 0029
    /// rules out the one mapping that would otherwise produce it: a host that
    /// cannot supply the strict tier must **refuse the operation**, not run it
    /// with hooks suppressed.
    Blocked,
}

impl HookPolicy {
    /// INV-15: a persistent, non-dismissible banner is required for anything
    /// other than [`Strict`](Self::Strict).
    ///
    /// The polarity is inverted from ADR 0025 on purpose. ADR 0025's `Allow`
    /// meant "hooks run unrestricted," so the banner marked the *permissive*
    /// case; these four name tiers, so the banner marks everything that is not
    /// the fullest isolation — including [`Blocked`](Self::Blocked), because
    /// "your hooks silently did not run" is a surprise a user must be told
    /// about just as much as "your hooks ran unsandboxed."
    ///
    /// Written as `!matches!(self, Strict)` rather than as an exhaustive match
    /// so a variant added later flies the banner by default. That is the
    /// fail-safe direction: a new tier no one has written banner text for
    /// over-warns instead of going silent.
    pub fn requires_banner(self) -> bool {
        !matches!(self, HookPolicy::Strict)
    }
}

impl Default for HookPolicy {
    /// Fail-closed, and one notch stricter than ADR 0025's `Restricted`: if the
    /// field is missing on the wire (an older server's response read by a newer
    /// client, via `SessionInfo`'s / `RepositoryDescriptor`'s
    /// `#[serde(default)]`), assume hooks are not running rather than assume
    /// they are running under a guarantee nothing measured.
    ///
    /// [`Strict`](Self::Strict) would be the wrong default precisely because it
    /// is the one variant that claims a guarantee *and* silences the banner —
    /// defaulting to it would turn an absent field into an unearned green
    /// light.
    fn default() -> Self {
        HookPolicy::Blocked
    }
}

/// Transitional aliases for ADR 0025's two Rust variant names, so the four-way
/// widening above does not break call sites in crates this change does not own.
///
/// **These are a migration shim with a deletion date, not API.** They exist
/// because `HookPolicy::{Allow, Restricted}` is still spelled in
/// `git-vista-server/src/security.rs`, `git-vista/src/features/session/core.rs`
/// and `git-vista-protocol/tests/dto_golden.rs`, none of which were in scope
/// for the change that widened this enum. The mapping is the same one the
/// `#[serde(alias)]`es above use, so a call site reading `HookPolicy::Allow`
/// keeps meaning exactly what it deserialized to.
///
/// Delete both, and the `#[serde(alias)]`es, once those three files spell the
/// tier names directly. Nothing new should reference them.
#[allow(non_upper_case_globals)]
impl HookPolicy {
    /// ADR 0025's `Allow` — "hooks run unrestricted." Now [`Network`](Self::Network):
    /// sandboxed but network-reachable is the closest live tier to what the old
    /// value actually described for a session that could push.
    #[doc(hidden)]
    pub const Allow: HookPolicy = HookPolicy::Network;
    /// ADR 0025's `Restricted`. Now [`Strict`](Self::Strict).
    #[doc(hidden)]
    pub const Restricted: HookPolicy = HookPolicy::Strict;
}

/// Response of `GET`/`POST /api/session` (M1.04): whether the caller now has a
/// live session and, when it does, the CSRF token to echo in the
/// [`CSRF_HEADER`](crate::CSRF_HEADER) on every state-changing request. `csrf` is
/// `None` exactly when `authenticated` is false, so the SPA can branch on either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub authenticated: bool,
    #[serde(default)]
    pub csrf: Option<String>,
    /// Whether this session was established through the LAN listener (ADR
    /// 0005). Additive field (M1.02 rule: new fields are `#[serde(default)]`,
    /// no protocol bump) — an older client ignores it. Purely a UI signal: the
    /// LAN listener's write routes are structurally absent regardless of what
    /// a client does with this flag.
    #[serde(default)]
    pub via_lan: bool,
    /// The current hook policy (M1.13a, #66, ADR 0025) — see [`HookPolicy`]'s
    /// own doc comment for what this does and does not mean today. Additive
    /// field, same `#[serde(default)]` convention as `via_lan` above.
    #[serde(default)]
    pub hook_policy: HookPolicy,
}

/// Response of `GET /api/rebase-status`: whether "Rebase onto main" would do
/// anything right now, resolved live server-side — the same freshness posture
/// as `/api/head-branch`, so the menu can disable the item instead of offering
/// a rebase that no-ops (or the nonsense "rebase ‘main’ onto main").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseStatus {
    /// The checked-out branch; `None` => detached HEAD (nothing to rebase).
    pub branch: Option<String>,
    /// What the server would rebase onto: `origin/main` when that
    /// remote-tracking ref exists, else the local `main`.
    pub base: String,
    /// False when `base` doesn't resolve at all (a repo with no `main`).
    pub base_exists: bool,
    /// True when HEAD already contains the base tip — the branch is already
    /// based on the latest `base`, so a rebase would change nothing.
    pub up_to_date: bool,
}

/// How a servable repository entry relates to git's on-disk layout (M1.03). The
/// catalog classifies every registered repository so the API — and eventually the
/// UI — can treat a bare repository or a linked worktree explicitly instead of
/// assuming one working tree per clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryKind {
    /// A bare repository: a git directory with no working tree. Reads work; the
    /// working-tree/status reads and every mutation are meaningless and refused.
    Bare,
    /// The main working tree of a repository (its git dir *is* the common dir).
    MainWorktree,
    /// A linked worktree (`git worktree add`): its own working tree and git dir
    /// under `…/worktrees/<name>`, sharing the repository's common dir — so it
    /// shares the [`repository`](RepositoryDescriptor::repository) id but carries
    /// a distinct [`worktree`](RepositoryDescriptor::worktree) id.
    LinkedWorktree,
}

/// One entry in the server-owned repository catalog (M1.03), as reported to the
/// client. This is the *capability* view: it addresses a repository by opaque
/// ids, never by a filesystem path, so the browser selects what to act on with a
/// [`worktree`](Self::worktree) id it cannot forge into a path.
///
/// The id fields are the opaque string forms of `git-vista-core`'s
/// `RepositoryId`/`WorktreeId`. They are kept as plain strings here to hold the
/// transport/domain boundary this crate exists to enforce: the client treats them
/// as meaningless handles and echoes them back, and only the native backend maps
/// an id to a path — through the catalog, which fails closed on anything it did
/// not itself register.
///
/// `path` is `None` by default. Absolute paths are server-filesystem detail and
/// are omitted unless the operator opts in (the `GIT_VISTA_EXPOSE_PATHS`
/// diagnostic), so the capability report never leaks the layout of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDescriptor {
    /// Opaque id of the shared repository (its common git directory). Every
    /// worktree of one clone reports the same value here.
    pub repository: String,
    /// Opaque id of this specific worktree — the handle the client sends back to
    /// address this entry. Distinct per worktree even within one repository.
    pub worktree: String,
    /// A short, non-path display label (the directory's base name), safe to show
    /// in the UI without revealing where on disk the repository lives.
    pub name: String,
    /// Whether this entry is a bare repo, the main worktree, or a linked worktree.
    pub kind: RepositoryKind,
    /// True when the entry is view-only (e.g. a clone opened from a URL): every
    /// mutation is refused, mirroring the graph's `read_only`.
    pub read_only: bool,
    /// The absolute filesystem path — omitted (`None`) unless the operator opted
    /// into path exposure. Never sent by default; the client must not depend on it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// The repo's `origin` remote normalized to a browsable https base URL
    /// (ADR 0010), e.g. `"https://github.com/owner/repo"`. `None` when there is
    /// no usable remote. Optional on the wire (M1.02 contract rule).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_web_url: Option<String>,
}

/// Validate a URL a user pasted to clone, before the server hands it to
/// `git clone` (Phase 12). This is a *gate*, not a parser: it accepts only the
/// public, read-oriented transports (`https://`, `http://`, `git://`) and rejects
/// everything else, so the pasted string can't be an SSH URL that would prompt for
/// keys, a local filesystem path, or an option smuggled in with a leading `-`.
/// git itself does the real URL parsing and reports a clear error if the host or
/// repo is wrong. Returns the trimmed URL on success, or a user-facing reason.
pub fn validate_clone_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Enter a repository URL.".to_string());
    }
    // Belt-and-braces even though the URL is passed as its own argv entry: a value
    // starting with '-' could still be read by git as an option.
    if url.starts_with('-') {
        return Err("URL can't start with '-'.".to_string());
    }
    const ALLOWED: [&str; 3] = ["https://", "http://", "git://"];
    if !ALLOWED.iter().any(|scheme| url.starts_with(scheme)) {
        return Err("Only https://, http:// or git:// URLs are supported.".to_string());
    }
    // Reject whitespace inside the URL — a single field should hold one URL, and it
    // keeps a space-separated second token from ever reaching git as an extra arg.
    if url.split_whitespace().count() != 1 {
        return Err("URL can't contain spaces.".to_string());
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_branch_request_roundtrips() {
        let req = CreateBranchRequest {
            name: "feature".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateBranchRequest>(&json).unwrap(),
            req
        );
    }

    #[test]
    fn create_commit_request_roundtrips_with_and_without_branch() {
        let on_head = CreateCommitRequest {
            message: "msg".into(),
            allow_empty: false,
            branch: None,
        };
        let json = serde_json::to_string(&on_head).unwrap();
        assert_eq!(
            serde_json::from_str::<CreateCommitRequest>(&json).unwrap(),
            on_head
        );
        // `branch` defaults when absent from the wire.
        let back: CreateCommitRequest =
            serde_json::from_str(r#"{"message":"m","allow_empty":true}"#).unwrap();
        assert_eq!(back.branch, None);
    }

    #[test]
    fn branch_and_clone_requests_roundtrip() {
        let b = BranchRequest {
            branch: "main".into(),
        };
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<BranchRequest>(&json).unwrap(), b);

        let c = CloneRequest {
            url: "https://example.com/r.git".into(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<CloneRequest>(&json).unwrap(), c);
    }

    #[test]
    fn request_bodies_reject_unknown_fields() {
        // The core of the "no path-based repository selection" guarantee at the
        // wire level: a stray `repo`/`path`/anything key is a hard error, not a
        // silently-dropped value that a future handler might start honouring.
        assert!(
            serde_json::from_str::<BranchRequest>(r#"{"branch":"main","repo":"/etc"}"#).is_err()
        );
        assert!(serde_json::from_str::<CloneRequest>(
            r#"{"url":"https://x/r.git","path":"/srv/secret"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CreateBranchRequest>(
            r#"{"name":"b","commit":"c","dir":"/x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<CreateCommitRequest>(
            r#"{"message":"m","allow_empty":false,"cwd":"/x"}"#
        )
        .is_err());
    }

    #[test]
    fn select_request_roundtrips_and_rejects_unknown_fields() {
        let req = SelectRequest {
            worktree: "22222222-2222-5222-8222-222222222222".into(),
            mode: RepoMode::Visualize,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<SelectRequest>(&json).unwrap(), req);
        // No path smuggling on the select body either.
        assert!(serde_json::from_str::<SelectRequest>(
            r#"{"worktree":"w","mode":"active","path":"/etc"}"#
        )
        .is_err());
    }

    #[test]
    fn delete_clone_request_round_trips_and_rejects_unknown_fields() {
        let req = DeleteCloneRequest {
            worktree: "11111111-2222-5333-8444-555555555555".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<DeleteCloneRequest>(&json).unwrap(),
            req
        );

        assert!(
            serde_json::from_str::<DeleteCloneRequest>(r#"{"worktree":"x","path":"/etc"}"#)
                .is_err()
        );
    }

    #[test]
    fn repo_mode_uses_stable_snake_case_wire_names() {
        // Wire names are contract (like RepositoryKind's): pin them so a rename
        // is a deliberate protocol change, never an accident.
        assert_eq!(
            serde_json::to_string(&RepoMode::Visualize).unwrap(),
            "\"visualize\""
        );
        assert_eq!(
            serde_json::to_string(&RepoMode::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn session_dtos_roundtrip_and_reject_unknown_fields() {
        let req = SessionRequest {
            token: "deadbeef".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<SessionRequest>(&json).unwrap(), req);
        // A stray field on the bootstrap body is a hard error, like every other.
        assert!(serde_json::from_str::<SessionRequest>(r#"{"token":"x","extra":1}"#).is_err());

        let info = SessionInfo {
            authenticated: true,
            csrf: Some("csrf-token".into()),
            via_lan: false,
            hook_policy: HookPolicy::Allow,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<SessionInfo>(&json).unwrap(), info);
        // csrf defaults to None when absent (the unauthenticated response omits it);
        // hook_policy fails closed to Restricted per its own Default impl, the same
        // "an older/partial response still deserializes, safely" contract via_lan
        // already established.
        let back: SessionInfo = serde_json::from_str(r#"{"authenticated":false}"#).unwrap();
        assert_eq!(back.csrf, None);
        assert_eq!(back.hook_policy, HookPolicy::Restricted);
    }

    #[test]
    fn rebase_status_roundtrips_through_json() {
        let status = RebaseStatus {
            branch: Some("feature".into()),
            base: "origin/main".into(),
            base_exists: true,
            up_to_date: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<RebaseStatus>(&json).unwrap(), status);
    }

    #[test]
    fn repository_descriptor_roundtrips_and_omits_path_by_default() {
        let d = RepositoryDescriptor {
            repository: "11111111-1111-5111-8111-111111111111".into(),
            worktree: "22222222-2222-5222-8222-222222222222".into(),
            name: "my-repo".into(),
            kind: RepositoryKind::MainWorktree,
            read_only: false,
            path: None,
            remote_web_url: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        // A `None` path is skipped entirely — the wire form never carries the key,
        // so the capability report can't leak the server's filesystem by default.
        assert!(
            !json.contains("path"),
            "path must be omitted when None: {json}"
        );
        // Same for the optional forge base (ADR 0010 + M1.02 contract rule).
        assert!(
            !json.contains("remote_web_url"),
            "remote_web_url must be omitted when None: {json}"
        );
        assert_eq!(
            serde_json::from_str::<RepositoryDescriptor>(&json).unwrap(),
            d
        );
    }

    #[test]
    fn repository_kind_uses_stable_snake_case_wire_names() {
        // The wire names are part of the contract; pin them so a rename is a
        // deliberate, visible protocol change rather than an accident.
        assert_eq!(
            serde_json::to_string(&RepositoryKind::Bare).unwrap(),
            "\"bare\""
        );
        assert_eq!(
            serde_json::to_string(&RepositoryKind::MainWorktree).unwrap(),
            "\"main_worktree\""
        );
        assert_eq!(
            serde_json::to_string(&RepositoryKind::LinkedWorktree).unwrap(),
            "\"linked_worktree\""
        );
    }

    #[test]
    fn clone_url_accepts_public_transports_and_trims() {
        assert_eq!(
            validate_clone_url("  https://github.com/rust-lang/rust.git "),
            Ok("https://github.com/rust-lang/rust.git".to_string())
        );
        assert!(validate_clone_url("http://example.com/r.git").is_ok());
        assert!(validate_clone_url("git://example.com/r.git").is_ok());
    }

    #[test]
    fn clone_url_rejects_unsafe_or_unsupported() {
        // SSH URL (would prompt for keys), local path, empty, option-like, spaces.
        assert!(validate_clone_url("git@github.com:owner/repo.git").is_err());
        assert!(validate_clone_url("/home/tom/secret").is_err());
        assert!(validate_clone_url("file:///etc").is_err());
        assert!(validate_clone_url("").is_err());
        assert!(validate_clone_url("   ").is_err());
        assert!(validate_clone_url("--upload-pack=evil").is_err());
        assert!(validate_clone_url("https://a.com/r.git --extra").is_err());
    }
}
