//! The typed patch-plan DTO (M2.17a, #212) — the wire shape of a *partial
//! staging selection*: which files, hunks, or individual lines of a viewed
//! diff the user wants staged or unstaged.
//!
//! **DTO only.** No endpoint accepts a [`PatchPlan`] yet — #213 (M2.17b)
//! builds the preview/apply endpoints on this shape, and #214 (M2.17c)
//! implements the sub-hunk apply semantics (context drift, unusual paths).
//! Defining the full closed vocabulary here — including the line-level
//! variant #214 executes — is deliberate: the golden fixture pins the wire
//! contract once, so 70b/70c are server-semantics work with no wire change.
//! Same posture as [`crate::WorktreeStatus`]: shape first, transport later.
//!
//! ## How a selection stays honest against a moving repository
//!
//! A selection is only meaningful against the exact diff the user was
//! looking at, so a plan carries the [`GenerationToken`] of the worktree
//! state that diff was computed from (ADR 0001: opaque, equality-only).
//! The server refuses a plan whose generation no longer equals the live one
//! (409, same gate as `Plan` execution and history paging) — that is #212's
//! staleness rejection, seeded server-side next to the future endpoint.
//!
//! Within a pinned generation, hunks are addressed by **ordinal index** into
//! the file's hunk list — the coordinate both sides derive from the same
//! bytes. Each [`HunkRef`] also repeats the hunk header's `old_start`/
//! `new_start` as a cross-check: the generation already guarantees the bytes
//! match, so the anchors defend against *indexing* bugs (a client and server
//! disagreeing on hunk order or splitting), not against drift — a mismatch
//! is a 400 (malformed selection), never a 409.
//!
//! ## Addressing files
//!
//! One canonical `path` string per selected file: the **new-side** path when
//! the file has one, else the old-side path (a deletion). That is the single
//! name a user sees in the diff view, and under a pinned generation it
//! resolves unambiguously — for a rename, the new path. The server matches
//! it against the same rule applied to [`crate::FileDiff`].

use serde::{Deserialize, Serialize};

use crate::plan::{GenerationToken, RepositoryToken, WorktreeToken};

/// Which way the selection moves between worktree and index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageDirection {
    /// Apply the selected changes worktree → index.
    Stage,
    /// Reverse the selected changes out of the index.
    Unstage,
}

/// One hunk of one file's diff, addressed by ordinal with a header anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HunkRef {
    /// 0-based index into the file's hunk list in the diff the plan's
    /// generation pins.
    pub index: u32,
    /// The referenced hunk header's old-side start — must match the pinned
    /// diff exactly (see the module doc: an indexing cross-check, not a
    /// staleness mechanism).
    pub old_start: u32,
    /// The referenced hunk header's new-side start, same contract.
    pub new_start: u32,
}

/// Line-level selection within one hunk (#214's execution scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HunkLines {
    /// The hunk the lines live in.
    pub hunk: HunkRef,
    /// 0-based indices into the hunk's `lines` (the same enumeration
    /// [`crate::Hunk::lines`] carries, context lines included). Each must
    /// reference an added or removed line — selecting a context line is
    /// meaningless and the server rejects it as malformed. Strictly
    /// ascending; see [`PatchPlan::validate`].
    pub lines: Vec<u32>,
}

/// What part of one file's change the plan selects — the closed vocabulary
/// of selection granularities. Internally tagged on `"select"`, `snake_case`,
/// matching the crate's other closed enums ([`crate::FileDiff`]'s `"shape"`,
/// [`crate::GitOperation`]'s `"op"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "select", rename_all = "snake_case")]
pub enum SelectionShape {
    /// The file's entire change, whatever its diff shape — the only
    /// granularity that exists for binary, mode-only, and no-content-rename
    /// files, and the "stage this file" shortcut for ordinary ones.
    EntireFile,
    /// Chosen hunks, applied whole. Strictly ascending by `index`.
    Hunks { hunks: Vec<HunkRef> },
    /// Chosen lines within chosen hunks. Strictly ascending by `hunk.index`.
    Lines { hunks: Vec<HunkLines> },
}

/// One selected file: the canonical path (module doc) plus the granularity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSelection {
    /// New-side path when the file has one, else the old-side path.
    pub path: String,
    /// What part of this file's change is selected.
    pub selection: SelectionShape,
}

/// The patch plan: everything #213's preview/apply endpoints need to build
/// the exact patch the user selected, and everything the staleness gate
/// needs to refuse it once the worktree has moved.
///
/// `#[serde(deny_unknown_fields)]` like every request body: a stray key is a
/// hard 400, never a silently-ignored value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchPlan {
    /// Opaque id of the shared repository (never a path; ADR 0003).
    pub repository: RepositoryToken,
    /// Opaque id of the worktree the selection targets — the scope
    /// [`PatchPlan::generation`] is meaningful in (ADR 0001).
    pub worktree: WorktreeToken,
    /// The generation of the diff the selection was made against. The server
    /// admits the plan only while the worktree's live generation still
    /// *equals* this token — the same reuse of [`GenerationToken`] that
    /// `Plan` and `WorktreeStatus` already share.
    pub generation: GenerationToken,
    /// Stage or unstage.
    pub direction: StageDirection,
    /// The selected files, in diff order. Non-empty, paths unique — see
    /// [`PatchPlan::validate`].
    pub files: Vec<FileSelection>,
}

/// Why a [`PatchPlan`] is structurally malformed — the 400 class of failure,
/// checkable without a repository (staleness, the 409 class, is the server
/// gate's job). Typed so #213's endpoint reports which selection is broken
/// instead of a free-text shrug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchPlanError {
    /// A plan that selects nothing is meaningless.
    NoFiles,
    /// Two selections name the same path — the canonical form is one
    /// selection per file.
    DuplicatePath(String),
    /// A `Hunks`/`Lines` selection with an empty hunk list selects nothing.
    NoHunks(String),
    /// Hunk ordinals must be strictly ascending (unique and ordered) so a
    /// plan has exactly one canonical byte form.
    UnorderedHunks(String),
    /// A `Lines` entry with no line indices selects nothing from its hunk.
    NoLines(String),
    /// Line indices must be strictly ascending, same canonical-form rule.
    UnorderedLines(String),
}

impl std::fmt::Display for PatchPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFiles => write!(f, "the plan selects no files"),
            Self::DuplicatePath(p) => write!(f, "path selected twice: {p}"),
            Self::NoHunks(p) => write!(f, "empty hunk selection for {p}"),
            Self::UnorderedHunks(p) => {
                write!(f, "hunk indices not strictly ascending for {p}")
            }
            Self::NoLines(p) => write!(f, "a hunk of {p} selects no lines"),
            Self::UnorderedLines(p) => {
                write!(f, "line indices not strictly ascending in a hunk of {p}")
            }
        }
    }
}

fn strictly_ascending(values: impl Iterator<Item = u32>) -> bool {
    let mut prev: Option<u32> = None;
    for v in values {
        if prev.is_some_and(|p| p >= v) {
            return false;
        }
        prev = Some(v);
    }
    true
}

impl PatchPlan {
    /// Structural validation — the invariants serde cannot express: something
    /// is selected, each file once, indices strictly ascending. Pure, so both
    /// the wasm client (before sending) and the server (before building a
    /// patch, #213) run the identical check.
    pub fn validate(&self) -> Result<(), PatchPlanError> {
        if self.files.is_empty() {
            return Err(PatchPlanError::NoFiles);
        }
        let mut seen = std::collections::BTreeSet::new();
        for file in &self.files {
            if !seen.insert(file.path.as_str()) {
                return Err(PatchPlanError::DuplicatePath(file.path.clone()));
            }
            match &file.selection {
                SelectionShape::EntireFile => {}
                SelectionShape::Hunks { hunks } => {
                    if hunks.is_empty() {
                        return Err(PatchPlanError::NoHunks(file.path.clone()));
                    }
                    if !strictly_ascending(hunks.iter().map(|h| h.index)) {
                        return Err(PatchPlanError::UnorderedHunks(file.path.clone()));
                    }
                }
                SelectionShape::Lines { hunks } => {
                    if hunks.is_empty() {
                        return Err(PatchPlanError::NoHunks(file.path.clone()));
                    }
                    if !strictly_ascending(hunks.iter().map(|h| h.hunk.index)) {
                        return Err(PatchPlanError::UnorderedHunks(file.path.clone()));
                    }
                    for sel in hunks {
                        if sel.lines.is_empty() {
                            return Err(PatchPlanError::NoLines(file.path.clone()));
                        }
                        if !strictly_ascending(sel.lines.iter().copied()) {
                            return Err(PatchPlanError::UnorderedLines(file.path.clone()));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(files: Vec<FileSelection>) -> PatchPlan {
        PatchPlan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("42").unwrap(),
            direction: StageDirection::Stage,
            files,
        }
    }

    fn href(index: u32) -> HunkRef {
        HunkRef {
            index,
            old_start: 10 * (index + 1),
            new_start: 10 * (index + 1),
        }
    }

    #[test]
    fn a_well_formed_plan_validates() {
        let p = plan(vec![
            FileSelection {
                path: "a.rs".into(),
                selection: SelectionShape::EntireFile,
            },
            FileSelection {
                path: "b.rs".into(),
                selection: SelectionShape::Hunks {
                    hunks: vec![href(0), href(2)],
                },
            },
            FileSelection {
                path: "c.rs".into(),
                selection: SelectionShape::Lines {
                    hunks: vec![HunkLines {
                        hunk: href(1),
                        lines: vec![0, 3, 4],
                    }],
                },
            },
        ]);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn an_empty_plan_and_empty_selections_are_rejected() {
        assert_eq!(plan(vec![]).validate(), Err(PatchPlanError::NoFiles));
        let empty_hunks = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Hunks { hunks: vec![] },
        }]);
        assert_eq!(
            empty_hunks.validate(),
            Err(PatchPlanError::NoHunks("a.rs".into()))
        );
        let empty_lines = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Lines {
                hunks: vec![HunkLines {
                    hunk: href(0),
                    lines: vec![],
                }],
            },
        }]);
        assert_eq!(
            empty_lines.validate(),
            Err(PatchPlanError::NoLines("a.rs".into()))
        );
    }

    #[test]
    fn duplicates_and_disorder_are_rejected() {
        let dup_path = plan(vec![
            FileSelection {
                path: "a.rs".into(),
                selection: SelectionShape::EntireFile,
            },
            FileSelection {
                path: "a.rs".into(),
                selection: SelectionShape::EntireFile,
            },
        ]);
        assert_eq!(
            dup_path.validate(),
            Err(PatchPlanError::DuplicatePath("a.rs".into()))
        );
        // Equal indices are as malformed as descending ones — "strictly".
        let dup_hunk = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Hunks {
                hunks: vec![href(1), href(1)],
            },
        }]);
        assert_eq!(
            dup_hunk.validate(),
            Err(PatchPlanError::UnorderedHunks("a.rs".into()))
        );
        let unordered_lines = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Lines {
                hunks: vec![HunkLines {
                    hunk: href(0),
                    lines: vec![4, 2],
                }],
            },
        }]);
        assert_eq!(
            unordered_lines.validate(),
            Err(PatchPlanError::UnorderedLines("a.rs".into()))
        );
    }
}
