//! Data types shared between the core, the Tauri shell, and the Leptos UI.
//!
//! Everything here derives `Serialize`/`Deserialize` so the exact same structs
//! cross the Tauri IPC boundary (Rust → JSON → wasm) without a second set of
//! frontend types.

use serde::{Deserialize, Serialize};

/// A git object id (commit hash), kept as a hex string so it crosses the IPC
/// boundary with no custom (de)serialization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Oid(pub String);

impl Oid {
    /// The conventional 7-character short hash (or the whole id if shorter).
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(7)]
    }
}

/// One commit, flattened to exactly what the UI needs to render a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub id: Oid,
    /// Parent ids. 0 = root, 1 = normal, 2+ = a merge commit.
    pub parents: Vec<Oid>,
    pub summary: String,
    pub author: String,
    /// Commit time as a Unix timestamp (seconds). The UI formats it.
    pub time: i64,
}

impl CommitSummary {
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// Full detail for one commit, read on demand when the user opens the detail
/// panel (Phase 10). The graph's [`CommitSummary`] carries only what a row needs
/// (first line, author name, commit time); this carries everything the panel
/// shows — the whole message body, both the author and committer signatures with
/// their emails and their own times — so it's fetched per-commit rather than
/// bloating every row of the graph payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetail {
    pub id: Oid,
    /// Parent ids, in order. 0 = root, 1 = normal, 2+ = a merge commit.
    pub parents: Vec<Oid>,
    pub author_name: String,
    pub author_email: String,
    /// Author time (when the work was written) as a Unix timestamp (seconds).
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    /// Commit time (when it was recorded) as a Unix timestamp (seconds). Differs
    /// from `author_time` for rebased/cherry-picked/amended commits.
    pub commit_time: i64,
    /// The full commit message, verbatim — summary line and body together.
    pub message: String,
}

/// What a [`GitRef`] is, so the UI can badge and prioritise it. `Head` is the
/// special `HEAD` pointer; `Branch`/`RemoteBranch` are local/remote branches;
/// `Tag` is a (peeled) tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    Head,
    Branch,
    RemoteBranch,
    Tag,
}

/// A ref pointing at a commit — drawn as a badge, and (for branches) used to give
/// each branch a stable colour. `target` is always peeled to a commit id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRef {
    /// Badge text: `"HEAD"`, `"main"`, `"origin/main"`, `"v1.0.0"`.
    pub name: String,
    pub kind: RefKind,
    pub target: Oid,
}

impl GitRef {
    /// Branches (local or remote) seed branch colouring; HEAD and tags are
    /// badges only.
    pub fn is_branch(&self) -> bool {
        matches!(self.kind, RefKind::Branch | RefKind::RemoteBranch)
    }
}

/// A commit placed in the vertical graph. `row` is the vertical position
/// (0 = newest, at the top); `lane` is the horizontal column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRow {
    pub commit: CommitSummary,
    pub row: usize,
    pub lane: usize,
    /// Refs (branches/tags/HEAD) that point exactly at this commit — the badges
    /// drawn beside it. Usually empty.
    pub refs: Vec<GitRef>,
    /// Palette slot for the branch this commit belongs to. Stable per branch:
    /// every commit on the same branch carries the same value across the whole
    /// graph, so the UI can colour a branch consistently regardless of which
    /// lane it happens to occupy. The UI maps the index onto its palette.
    pub color: usize,
}

/// A line drawn between a commit and one of its parents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from_row: usize,
    pub from_lane: usize,
    pub to_row: usize,
    pub to_lane: usize,
}

/// The fully laid-out graph handed to the frontend for rendering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub rows: Vec<GraphRow>,
    pub edges: Vec<Edge>,
    /// Number of lanes (columns) used — the UI sizes the gutter from this.
    pub lane_count: usize,
    /// Web base URL of the repo's GitHub `origin`, e.g.
    /// `"https://github.com/owner/repo"`, when it has one. The UI links commits
    /// and refs under it; `None` => labels stay plain text. Set by the backend
    /// after layout (the pure layout doesn't know about remotes).
    #[serde(default)]
    pub repo_url: Option<String>,
    /// Commit ids (hex) reachable from a remote-tracking ref — i.e. the commits
    /// actually on the remote (GitHub). The UI links a commit/ref only when its
    /// commit is in this set, so links never point at unpushed objects that would
    /// 404; unpushed ones are shown dimmed and non-clickable. Empty when there's
    /// no remote. Set by the backend after layout, alongside `repo_url`.
    #[serde(default)]
    pub remote_commits: Vec<String>,
    /// Local branches that have no commits of their own — their tip is a commit
    /// another branch already owns (e.g. a branch just created from an existing
    /// commit). Rather than crowd that commit with another badge, the UI draws
    /// each as its own short, distinctly-coloured line forking off the commit.
    /// Set by the layout pass.
    #[serde(default)]
    pub stubs: Vec<BranchStub>,
    /// Filesystem path of the repository this graph was read from, as the server
    /// resolved it (e.g. `/home/tom/projects/git-vista-test`). Surfaced in the UI
    /// header so it's always unambiguous *which* repo a given page is showing —
    /// the fastest way to catch a browser that's pointed at a stale server/tab.
    /// Set by the backend; `None` => the UI shows nothing extra.
    #[serde(default)]
    pub repo_label: Option<String>,
    /// True when this graph came from a throwaway clone the server made from a
    /// pasted URL (Phase 12). Such repos are for *viewing only*: the UI hides all
    /// write actions (branch/commit/merge/push/delete) since any change would be
    /// discarded when the clone is deleted. `false` for the user's own local repo.
    #[serde(default)]
    pub read_only: bool,
}

/// A local branch with no commits of its own, drawn as a short fork off the
/// commit it points at (its `anchor`). Carries its own `lane` and `color` so the
/// UI renders it as a distinct line+badge rather than a second badge on the
/// shared commit. See [`Graph::stubs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStub {
    /// Branch name — the badge text, e.g. `"feature/ui-dark-mode"`.
    pub name: String,
    /// Row of the commit this branch forks from (its tip is that commit).
    pub anchor_row: usize,
    /// Lane of the commit it forks from, so the connector can curve out of it.
    pub anchor_lane: usize,
    /// The stub's own lane (column), to the right of the commit lanes.
    pub lane: usize,
    /// The stub's own colour slot — distinct from the branch it forked off.
    pub color: usize,
    /// Position in the cascade of stubs that share this anchor commit: 0 forks
    /// straight off the commit; 1 forks off stub 0's tip; 2 off stub 1's tip; …
    /// So creating another branch at a commit that already has one draws a *new*
    /// hollow dot forking off the previous stub's dot, rather than every stub
    /// fanning back to the shared commit. (Git records no "created from which
    /// stub" link, so the cascade is ordered deterministically by branch name.)
    #[serde(default)]
    pub depth: usize,
}

/// Body of a `POST /api/branch` request (Issue #18): create a branch named
/// `name` pointing at the commit `commit` (full hex id). Shared so the frontend
/// serialises exactly what the backend deserialises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBranchRequest {
    pub name: String,
    pub commit: String,
}

/// Body of a `POST /api/commit` request (Issue #33): create a commit on top of
/// the current HEAD with the message `message`. When `allow_empty` is true the
/// commit is made even with nothing staged (`git commit --allow-empty`);
/// otherwise git commits the staged changes and fails if there are none. The UI
/// only offers this on the HEAD tip, so the backend never needs to move HEAD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateCommitRequest {
    pub message: String,
    pub allow_empty: bool,
}

/// Body of the three branch-operation requests (Issue #33 follow-up): merge
/// (`POST /api/merge`), push (`POST /api/push`), and delete (`POST /api/delete-branch`).
/// All three act on a single named branch, so they share one shape. `branch` is a
/// local branch name; the backend validates it and forwards git's own error text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRequest {
    pub branch: String,
}

/// Body of a `POST /api/clone` request (Phase 12): clone the public repository at
/// `url` into a throwaway temp directory and switch the server to viewing it,
/// read-only. `url` is a git-cloneable URL (typically `https://…`); the backend
/// validates its scheme and forwards git's own error text on failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneRequest {
    pub url: String,
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
    fn short_hash_truncates_to_seven() {
        let oid = Oid("0123456789abcdef".into());
        assert_eq!(oid.short(), "0123456");
    }

    #[test]
    fn short_hash_handles_tiny_ids() {
        assert_eq!(Oid("abc".into()).short(), "abc");
    }

    #[test]
    fn merge_detection() {
        let two_parents = CommitSummary {
            id: Oid("a".into()),
            parents: vec![Oid("b".into()), Oid("c".into())],
            summary: "merge".into(),
            author: "t".into(),
            time: 0,
        };
        assert!(two_parents.is_merge());
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
