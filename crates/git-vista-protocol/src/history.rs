//! Generic paged-history wire envelopes (M1.10, #63).
//!
//! History moved from one whole-graph payload to two shapes: a cheap
//! [`HistoryFrame`] (refs, branch colours, resolved-target metadata — no
//! commits) fetched once per view, and repeated [`HistoryPage`]s (the actual
//! rows/edges/stubs, cursor-paginated) fetched as the user scrolls. Both are
//! generic over the row/edge/stub types so this crate stays pure and
//! wasm-safe: it declares only the transport shape, never the domain types
//! that fill it in. `git-vista-server` and `git-vista` (the frontend) each
//! declare their own concrete aliases — see `docs/superpowers/plans/
//! 2026-07-25-m1.10-paged-history-bounded-diff.md` lines 869-906.
//!
//! `HistoryFrame` carries no stubs — a page is a window into history and only
//! a page's rows can anchor a stub; `HistoryPage` carries `stubs` for exactly
//! that reason. `Page.lane_count` is the commit-lane high-water only (see
//! `FrameStub::lane_offset` in `git-vista-core`), not a count inclusive of
//! stub columns.

use serde::{Deserialize, Serialize};

use crate::plan::GenerationToken;

/// What state HEAD is in, stated explicitly rather than inferred from an
/// absent branch name (#473).
///
/// `head_branch: None` cannot answer this on its own: it is the same `None`
/// for a **healthy detached HEAD** and for a **HEAD that resolves to nothing**,
/// and those are opposite situations for whoever is looking at the screen. One
/// is normal; the other means the repository is broken, which is precisely when
/// someone opens this app to find out what state they are in.
///
/// Inferring it client-side from "no branch and no HEAD ref" would work — after
/// #465 a dangling HEAD is no longer badged — but that is the shape ADR 0068
/// refuses: a state read from the absence of two other things is a state nobody
/// can find when it is wrong. It is said here instead.
///
/// **`Unreadable` is deliberately absent.** When `.git/HEAD` itself will not
/// read, `read_history_materials` fails the whole request and the user gets an
/// error, not a frame — so there is no frame in which that state could be
/// reported. A variant that cannot occur is vocabulary nobody can trust.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadState {
    /// HEAD names a branch, and that branch has a commit.
    OnBranch,
    /// HEAD points straight at a commit; no branch is checked out. Normal.
    Detached,
    /// HEAD names a branch that has no commit yet — a fresh repository.
    Unborn,
    /// HEAD holds an object id that nothing resolves. **The repository is
    /// broken**, and this is the state the user most needs to be told about.
    Unresolvable,
    /// The server did not send this field. Only reachable across a version
    /// skew — a browser holding a bundle older than the running server — and
    /// rendered as nothing, which is what that browser would have shown anyway.
    #[default]
    Unknown,
}

/// The cheap, once-per-view half of paged history: refs, branch colour slots,
/// and resolved-target/session metadata — never commit rows, edges, or stubs.
/// Generic over the ref type `R` so this crate carries no dependency on
/// `git-vista-core`; the server aliases `R` to `git_vista_core::GitRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFrame<R> {
    pub generation: GenerationToken,
    pub refs: Vec<R>,
    pub head_branch: Option<String>,
    /// HEAD's state, which `head_branch` alone cannot express (#473).
    /// `#[serde(default)]` so a frame from a server that predates the field
    /// deserializes as [`HeadState::Unknown`] rather than failing outright.
    #[serde(default)]
    pub head_state: HeadState,
    pub branch_colors: Vec<(String, usize)>,
    pub repo_label: Option<String>,
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub read_only: bool,
    pub resettable: bool,
    pub repo_url: Option<String>,
    pub remote_web_url: Option<String>,
}

/// One cursor-paginated window of history rows/edges/stubs. Generic over the
/// row type `R`, the edge type `E`, and the stub type `S` so this crate stays
/// pure and wasm-safe; the server aliases these to `git_vista_core::{GraphRow,
/// Edge, FrameStub}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage<R, E, S> {
    pub rows: Vec<R>,
    pub edges: Vec<E>,
    pub stubs: Vec<S>,
    pub lane_count: usize,
    pub cursor: Option<String>,
    pub generation: GenerationToken,
}

#[cfg(test)]
mod head_state_tests {
    use super::*;

    /// #473: the three situations that all arrive as `head_branch: None` must
    /// be distinguishable on the wire. A test that only checked "unresolvable
    /// round-trips" would pass against a payload that reported the same thing
    /// for a healthy detached HEAD, which is the actual defect.
    #[test]
    fn the_states_that_share_an_absent_branch_name_are_still_told_apart() {
        let detached = serde_json::to_string(&HeadState::Detached).unwrap();
        let unresolvable = serde_json::to_string(&HeadState::Unresolvable).unwrap();
        let unborn = serde_json::to_string(&HeadState::Unborn).unwrap();

        assert_eq!(detached, "\"detached\"");
        assert_eq!(unresolvable, "\"unresolvable\"");
        assert_ne!(
            detached, unresolvable,
            "a healthy detached HEAD and a HEAD that resolves to nothing must \
             not serialize the same — telling them apart is the whole point"
        );
        assert_ne!(unborn, unresolvable);

        for s in [
            HeadState::OnBranch,
            HeadState::Detached,
            HeadState::Unborn,
            HeadState::Unresolvable,
            HeadState::Unknown,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<HeadState>(&json).unwrap(), s);
        }
    }

    /// A frame written before the field existed must still load, as the state
    /// nobody can act on — not as an error, and not as a healthy default that
    /// would quietly claim HEAD is fine.
    #[test]
    fn a_frame_without_the_field_loads_as_unknown_not_as_healthy() {
        let json = r#"{
            "generation": "history-v1:1",
            "refs": [],
            "head_branch": null,
            "branch_colors": [],
            "repo_label": null,
            "repo_id": null,
            "worktree_id": null,
            "read_only": false,
            "resettable": false,
            "repo_url": null,
            "remote_web_url": null
        }"#;
        let frame: HistoryFrame<String> = serde_json::from_str(json).expect("an older frame loads");
        assert_eq!(frame.head_state, HeadState::Unknown);
        assert_ne!(
            frame.head_state,
            HeadState::OnBranch,
            "a missing field must never read as a healthy HEAD"
        );
    }
}
