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

/// A commit placed in the vertical graph. `row` is the vertical position
/// (0 = newest, at the top); `lane` is the horizontal column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRow {
    pub commit: CommitSummary,
    pub row: usize,
    pub lane: usize,
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
