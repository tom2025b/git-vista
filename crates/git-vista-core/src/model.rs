//! Data types shared between the core, the server, and the Leptos UI.
//!
//! Everything here derives `Serialize`/`Deserialize` so the exact same structs
//! cross the HTTP/JSON boundary (server → JSON → wasm frontend) without a second
//! set of frontend types.

use serde::{Deserialize, Serialize};

/// A git object id (commit hash), kept as a hex string so it crosses the JSON
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
    /// The origin remote normalized to a browsable https base for ANY forge host
    /// (ADR 0010) — GitHub, GitLab, Codeberg, or a best-effort unknown host.
    /// Unlike [`repo_url`](Self::repo_url) (GitHub-only, drives the pushed-commit
    /// dot links) this powers the general "view on <host>" links. `None` => no
    /// usable remote; those links are simply absent.
    #[serde(default)]
    pub remote_web_url: Option<String>,
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
    /// A short, non-path label for the repository this graph was read from (its
    /// directory base name, e.g. `git-vista-test`). Surfaced in the UI header so
    /// it's always unambiguous *which* repo a given page is showing — the fastest
    /// way to catch a browser pointed at a stale server/tab. Deliberately *not*
    /// the absolute path by default (M1.03): the server's filesystem layout is not
    /// exposed to the browser unless the operator opts in (`GIT_VISTA_EXPOSE_PATHS`),
    /// in which case the full path is shown here instead. `None` => nothing extra.
    #[serde(default)]
    pub repo_label: Option<String>,
    /// Opaque id of the shared repository this graph came from (M1.03), as an
    /// otherwise-meaningless string handle. `None` on a repo the server is serving
    /// in degraded mode (couldn't classify it). The UI treats it as opaque and may
    /// echo it back to address later requests at the same repository.
    #[serde(default)]
    pub repo_id: Option<String>,
    /// Opaque id of the specific worktree this graph came from (M1.03) — the
    /// handle a request uses to select this exact worktree. Distinct per worktree
    /// even within one repository; `None` in degraded mode.
    #[serde(default)]
    pub worktree_id: Option<String>,
    /// True when this graph came from a throwaway clone the server made from a
    /// pasted URL (Phase 12). Such repos are for *viewing only*: the UI hides all
    /// write actions (branch/commit/merge/push/delete) since any change would be
    /// discarded when the clone is deleted. `false` for the user's own local repo.
    #[serde(default)]
    pub read_only: bool,
    /// True when this repo carries a recorded test-repo seed (`gv --seed`), so
    /// the UI may offer "Reset Test Repo" — restore the seeded branches/HEAD/
    /// worktree, discarding everything since. Never true on a read-only clone.
    #[serde(default)]
    pub resettable: bool,
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

// The request/response transport DTOs that used to live here — `CreateBranchRequest`,
// `CreateCommitRequest`, `BranchRequest`, `CloneRequest`, `RebaseStatus`, and the
// `validate_clone_url` gate — moved to the `git-vista-protocol` crate (M1.02, #102):
// they are the wire contract, versioned independently of this domain model. Core no
// longer knows about transport. The graph/commit types above stay here — they are the
// repository domain, produced by this crate's own layout engine.

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
    fn graph_remote_web_url_defaults_when_absent_from_wire() {
        // M1.02 contract rule: a new optional field must not break an older
        // server's payload — absent on the wire deserializes to None.
        let g: Graph = serde_json::from_str(r#"{"rows":[],"edges":[],"lane_count":0}"#).unwrap();
        assert_eq!(g.remote_web_url, None);
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
