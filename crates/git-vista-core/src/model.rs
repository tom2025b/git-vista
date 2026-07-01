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
}
