//! The generation-tagged working-tree status DTO (M2.15, #68a).
//!
//! This is the **DTO only** — the wire shape `GET /api/status` (a future
//! endpoint, #68c) will eventually serve, and what a `git status --porcelain=v2
//! -z` parser (#68b) will eventually populate. Nothing here spawns git, reads a
//! repository, or parses porcelain text; every value in the golden fixture is
//! hand-built, the same way `plan_golden.rs` hand-builds [`crate::GitOperation`]
//! values with no repository involved.
//!
//! ## The generation mechanism is reused, not reinvented
//!
//! [`WorktreeStatus::generation`] is a plain [`crate::GenerationToken`] — the
//! same opaque, equality-only wire type [`crate::HistoryFrame`] already carries
//! (ADR 0001). The underlying algorithm is `git-vista-core::identity`'s
//! [`RepositoryGeneration`](https://docs.rs/git-vista-core "content digest, not
//! a counter — see ADR 0001"), whose own `GenerationInputs` builder already
//! defines a `worktree` slot as *"a digest of the unstaged working-tree status
//! (tracked modifications + untracked files)"* — i.e. exactly this DTO's
//! reason to exist. `git-vista-git::read_generation_inputs`'s doc comment
//! already shows the intended call shape: read HEAD/refs/index, then
//! `inputs.worktree(digest)` from the real status read, then
//! `inputs.generation()`.
//!
//! So this task is not choosing between "a monotonic counter" and "an
//! mtime+HEAD digest" — ADR 0001 already settled that question for the whole
//! codebase, with reasoning (a digest handles *revert-to-prior-state*
//! correctly for a stale-tab guard; a counter would treat a reverted edit as
//! "still moved forward," which is the wrong answer for "is this the state the
//! user reviewed"). Reusing the existing mechanism is what keeps this DTO's
//! generation and [`crate::HistoryFrame`]'s generation — and #70's future
//! write-precondition check — all comparable under the one contract ADR 0001
//! defines, instead of three subsystems each inventing their own notion of
//! "stale."
//!
//! Namespacing follows `history.rs`'s own precedent
//! (`GenerationToken::new(format!("history-v1:{}", ...))`): when #68c builds
//! the real token, it should prefix with `status-v1:` before wrapping the
//! digest, so a status generation can never be confused with (or accidentally
//! compared to) a history generation by a client that mixes the two up. That
//! prefixing is 68c's job — this DTO only carries the already-opaque
//! [`crate::GenerationToken`] and does not care what's inside it.
//!
//! **What this mechanism does *not* detect**, stated plainly per ADR 0001's own
//! framing: it is a content digest, so an edit that is later reverted to the
//! exact prior bytes produces the exact prior generation, not a new one — two
//! reads that differ only by a "make a change, then undo it" round trip inside
//! a client's window will show the *same* status generation on both sides of
//! that round trip, even though the working tree was briefly different in
//! between. ADR 0001 argues this is the right answer for a *write*
//! precondition (the reviewed state and the current state genuinely are
//! identical again, so admitting the write is correct) — for a *status
//! display* specifically, the same argument holds: there is nothing left to
//! show that differs, so "no visible change" is the honest read, not a false
//! negative.
//!
//! ## The eight states (#68's "staged, unstaged, untracked, ignored,
//! conflicted, renamed, submodule, and binary states")
//!
//! Modelled as an internally-tagged, closed [`StatusEntry`] enum — one variant
//! per condition git's own porcelain v2 format actually distinguishes, no
//! catch-all, the same shape [`crate::GitOperation`] uses and for the same
//! reason (see that type's doc comment): a nonsensical combination should be
//! unrepresentable, not merely "shouldn't happen in practice."
//!
//! - **staged / unstaged** are not two separate top-level lists here (unlike
//!   the current `git-vista-core::status::RepoStatus`, which this DTO
//!   deliberately does not extend or replace — that type still serves the
//!   existing `GET /api/status`, forbidden to this task). They are
//!   [`ChangeSides`], a 3-variant enum (`StagedOnly` / `UnstagedOnly` /
//!   `Both`) attached to one entry, because one path can legitimately be dirty
//!   on both sides at once (staged, then edited again) and "neither side
//!   changed" should not type-check as a value at all.
//! - **untracked** / **ignored** are their own variants — no staged/unstaged
//!   split exists for either (they aren't in the index).
//! - **conflicted** is [`ConflictKind`], the seven combinations
//!   `git-status(1)`'s short-format table names exactly (`DD`/`AU`/`UD`/`UA`/
//!   `DU`/`AA`/`UU`) — not a staged/unstaged pair, because a merge conflict's
//!   `<XY>` codes mean something structurally different (ours/theirs, not
//!   index/worktree) and forcing them through [`ChangeSides`] would silently
//!   misreport what they mean.
//! - **renamed** (folding in copies, matching the existing core parser's
//!   choice) is its own variant carrying a *required* `origin_path` — a
//!   renamed entry with no source path cannot be constructed.
//! - **submodule** is [`SubmoduleState`], attached as `Option<SubmoduleState>`
//!   to every variant porcelain v2's `<sub>` field can appear on (`Changed`,
//!   `Renamed`, `Conflicted`) — orthogonal to the entry's own change
//!   classification, exactly as git models it (a submodule can be dirty
//!   *without* its recorded commit having changed).
//! - **binary** is `bool` on the variants that carry real content
//!   (`Changed`/`Renamed`/`Untracked`) rather than a ninth top-level state —
//!   it's a property of the blob, not a status axis. Porcelain v2 does not
//!   report this directly (unlike the diff endpoint's `--numstat`-derived
//!   detection); populating it accurately is 68b's problem, not this DTO's —
//!   the field exists so 68b has somewhere to put the answer.

use serde::{Deserialize, Serialize};

use crate::GenerationToken;

/// One side's ordinary change kind. Folds git's `T` (type change) into
/// `Modified` and reserves the dedicated [`StatusEntry::Renamed`] shape for
/// `R`/`C` — the same collapse `git-vista-core::status::ChangeKind` already
/// makes, kept consistent rather than reinvented differently here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Which side(s) of the index/worktree split an entry is dirty on. A plain
/// `Option<ChangeKind>` pair would let "neither side changed" type-check as a
/// value; this doesn't. One path dirty on both sides (staged, then edited
/// again) is `Both`, not two separate entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "side", rename_all = "snake_case")]
pub enum ChangeSides {
    StagedOnly {
        staged: ChangeKind,
    },
    UnstagedOnly {
        unstaged: ChangeKind,
    },
    Both {
        staged: ChangeKind,
        unstaged: ChangeKind,
    },
}

/// A merge conflict's classification — the seven combinations
/// `git-status(1)`'s short-format table names exactly, not a staged/unstaged
/// pair (a conflict's `<XY>` codes mean ours/theirs, a different axis
/// entirely from index/worktree, so [`ChangeSides`] would misreport it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// `DD` — deleted on both sides.
    BothDeleted,
    /// `AU` — we added it, they haven't touched it (unmerged).
    AddedByUs,
    /// `UD` — they deleted it, we haven't touched it (unmerged).
    DeletedByThem,
    /// `UA` — they added it, we haven't touched it (unmerged).
    AddedByThem,
    /// `DU` — we deleted it, they haven't touched it (unmerged).
    DeletedByUs,
    /// `AA` — added on both sides, differently.
    BothAdded,
    /// `UU` — modified on both sides, differently.
    BothModified,
}

/// A submodule entry's dirty state, from porcelain v2's `<sub>` field
/// (`S<c><m><u>`) — orthogonal to the entry's own [`ChangeSides`]/
/// [`ConflictKind`] classification, since a submodule can be dirty *inside*
/// without its recorded commit pointer having changed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleState {
    /// `<c>` — the recorded commit differs from what's checked out.
    pub commit_changed: bool,
    /// `<m>` — the submodule has modified tracked content.
    pub has_tracked_changes: bool,
    /// `<u>` — the submodule has untracked content.
    pub has_untracked_changes: bool,
}

/// One entry in a [`WorktreeStatus`] — the closed vocabulary of every
/// condition `git status --porcelain=v2` distinguishes. Internally tagged on
/// `"entry_kind"` (not `"kind"` — [`StatusEntry::Conflicted`] already has a
/// field named `kind`, and serde refuses a variant field that collides with
/// the internal tag), `snake_case` variant names, following
/// [`crate::GitOperation`]'s wire shape otherwise exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry_kind", rename_all = "snake_case")]
pub enum StatusEntry {
    /// An ordinary changed path (porcelain `1` record) — no rename, no
    /// conflict.
    Changed {
        path: String,
        sides: ChangeSides,
        submodule: Option<SubmoduleState>,
        binary: bool,
    },
    /// A rename or copy git detected (porcelain `2` record). `origin_path` is
    /// required, not optional — a renamed entry with no source path cannot be
    /// constructed. `score` is the similarity percentage git reported (e.g.
    /// `100` for `R100`).
    Renamed {
        path: String,
        origin_path: String,
        score: u8,
        sides: ChangeSides,
        submodule: Option<SubmoduleState>,
        binary: bool,
    },
    /// An untracked path (porcelain `?` record) — never has a staged/unstaged
    /// split; it isn't in the index at all.
    Untracked { path: String, binary: bool },
    /// An ignored path (porcelain `!` record).
    Ignored { path: String },
    /// A merge conflict (porcelain `u` record). No [`ChangeSides`] — see
    /// [`ConflictKind`]'s doc comment for why that axis doesn't apply here.
    Conflicted {
        path: String,
        kind: ConflictKind,
        submodule: Option<SubmoduleState>,
    },
}

/// The full working-tree status — the payload a future `GET /api/status` (v2,
/// #68c) will serve. [`generation`](Self::generation) is what makes it
/// staleness-detectable (#68's *"generation-tagged and detects external
/// changes"* criterion) and is what a future write-precondition check (#70)
/// will compare against — see the module doc for why this reuses
/// [`GenerationToken`]/ADR 0001 rather than a new mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub generation: GenerationToken,
    /// The checked-out branch; `None` for detached HEAD.
    pub branch: Option<String>,
    /// The branch's upstream (e.g. `origin/main`), when one is set.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Every changed/renamed/untracked/ignored/conflicted path, in no
    /// particular order (porcelain v2 doesn't guarantee one either — see
    /// `git-status(1)`'s "Tracked entries are printed in an undefined
    /// order").
    pub entries: Vec<StatusEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(s: &str) -> GenerationToken {
        GenerationToken::new(s).unwrap()
    }

    /// [`ChangeSides`] cannot represent "neither side changed" — there is no
    /// variant for it, which is the point: the type only has three shapes,
    /// and this pins that the enum still round-trips each of them.
    #[test]
    fn change_sides_round_trip_each_variant() {
        for sides in [
            ChangeSides::StagedOnly {
                staged: ChangeKind::Added,
            },
            ChangeSides::UnstagedOnly {
                unstaged: ChangeKind::Modified,
            },
            ChangeSides::Both {
                staged: ChangeKind::Added,
                unstaged: ChangeKind::Modified,
            },
        ] {
            let json = serde_json::to_string(&sides).unwrap();
            let back: ChangeSides = serde_json::from_str(&json).unwrap();
            assert_eq!(sides, back);
        }
    }

    /// A renamed entry's `origin_path` is a required field on the wire — an
    /// object missing it must fail to deserialize, not silently default to
    /// empty. Pins that the "cannot construct a renamed entry with no
    /// source" guarantee holds at the JSON boundary too, not just in Rust.
    #[test]
    fn renamed_entry_without_origin_path_is_rejected_at_the_wire() {
        let missing_origin = serde_json::json!({
            "entry_kind": "renamed",
            "path": "new.rs",
            "score": 100,
            "sides": {"side": "staged_only", "staged": "added"},
            "submodule": null,
            "binary": false,
        });
        let result: Result<StatusEntry, _> = serde_json::from_value(missing_origin);
        assert!(
            result.is_err(),
            "origin_path must be required, not optional"
        );
    }

    #[test]
    fn worktree_status_round_trips() {
        let status = WorktreeStatus {
            generation: token("status-v1:12345"),
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            ahead: 1,
            behind: 0,
            entries: vec![StatusEntry::Untracked {
                path: "scratch.txt".to_string(),
                binary: false,
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: WorktreeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}
