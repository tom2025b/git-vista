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
//! ## Which diff a selection addresses
//!
//! A generation pins a worktree *state*, and one state has several diffs.
//! The base diff is implied by [`StageDirection`], the `git add -p` /
//! `git reset -p` convention, and that mapping is contract: `stage`
//! selections address the **worktree-vs-index** diff, `unstage` selections
//! the **index-vs-HEAD** diff. Hunk ordinals and line indices are defined
//! against that diff and no other; if a viewed [`crate::DiffSpec`] ever
//! becomes a legal selection base, that is a wire change (a new field), not
//! a reinterpretation.
//!
//! ## Addressing files
//!
//! One canonical `path` string per selected file: the **new-side** path when
//! the file has one, else the old-side path (a deletion). That is the single
//! name a user sees in the diff view, and under a pinned generation it
//! resolves unambiguously — for a rename, the new path. The server matches
//! it against the same rule applied to [`crate::FileDiff`]; for the
//! single-path variants (`ModeChangeOnly`, `Combined`) the canonical path is
//! that path.

use serde::{Deserialize, Serialize};

use crate::plan::{GenerationToken, RepositoryToken, WorktreeToken};

/// Which way the selection moves between worktree and index — and thereby
/// **which diff the selection's coordinates address** (module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageDirection {
    /// Apply the selected changes worktree → index. Ordinals and line
    /// indices address the **worktree-vs-index** diff.
    Stage,
    /// Reverse the selected changes out of the index. Ordinals and line
    /// indices address the **index-vs-HEAD** diff.
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
///
/// `Deserialize` is **hand-written** (below) because serde's derived
/// internally-tagged deserialization silently ignores unknown keys inside
/// the variant object — `{"select": "entire_file", "hunks": [...]}` would
/// parse as `EntireFile` and stage the whole file when the client meant a
/// hunk selection. The manual impl routes through a strict wire struct so a
/// stray or mismatched key is a hard 400 here too, matching the
/// `deny_unknown_fields` posture of every other shape in this module.
/// `Serialize` stays derived; the wire form is unchanged and fixture-pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "select", rename_all = "snake_case")]
pub enum SelectionShape {
    /// The file's entire change, whatever its diff shape — the only
    /// granularity that exists for binary, mode-only, no-content-rename,
    /// and combined-merge files, and the "stage this file" shortcut for
    /// ordinary ones.
    EntireFile,
    /// Chosen hunks, applied whole. Strictly ascending by `index`.
    Hunks { hunks: Vec<HunkRef> },
    /// Chosen lines within chosen hunks. Strictly ascending by `hunk.index`.
    Lines { hunks: Vec<HunkLines> },
}

/// The strict deserialization route for [`SelectionShape`] (see its doc).
/// One field set covers all variants; `deny_unknown_fields` on this struct
/// is what the derived internally-tagged form cannot provide.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionShapeWire {
    select: SelectTag,
    #[serde(default)]
    hunks: Option<HunksWire>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SelectTag {
    EntireFile,
    Hunks,
    Lines,
}

/// The two payload shapes the shared `hunks` key can carry. Untagged is safe
/// here: [`HunkRef`] and [`HunkLines`] have disjoint required fields (both
/// `deny_unknown_fields`), so a non-empty list parses as exactly one of the
/// two; the empty list parses as `Refs` and is re-routed by tag below.
#[derive(Deserialize)]
#[serde(untagged)]
enum HunksWire {
    Refs(Vec<HunkRef>),
    Lines(Vec<HunkLines>),
}

impl<'de> Deserialize<'de> for SelectionShape {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let wire = SelectionShapeWire::deserialize(deserializer)?;
        match (wire.select, wire.hunks) {
            (SelectTag::EntireFile, None) => Ok(SelectionShape::EntireFile),
            (SelectTag::EntireFile, Some(_)) => Err(D::Error::custom(
                "an entire_file selection carries no hunks key",
            )),
            (SelectTag::Hunks, Some(HunksWire::Refs(hunks))) => Ok(SelectionShape::Hunks { hunks }),
            (SelectTag::Hunks, Some(HunksWire::Lines(_))) => Err(D::Error::custom(
                "a hunks selection carries hunk references, not line selections",
            )),
            (SelectTag::Lines, Some(HunksWire::Lines(hunks))) => {
                Ok(SelectionShape::Lines { hunks })
            }
            // `[]` matches the Refs arm of the untagged enum; under the
            // lines tag it is still an empty lines list (validate() then
            // rejects it as NoHunks — empty is malformed, not ambiguous).
            (SelectTag::Lines, Some(HunksWire::Refs(refs))) if refs.is_empty() => {
                Ok(SelectionShape::Lines { hunks: vec![] })
            }
            (SelectTag::Lines, Some(HunksWire::Refs(_))) => Err(D::Error::custom(
                "a lines selection carries line selections, not bare hunk references",
            )),
            (SelectTag::Hunks | SelectTag::Lines, None) => Err(D::Error::missing_field("hunks")),
        }
    }
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
    ///
    /// **Provenance is part of the contract**: tokens are opaque and
    /// equality-only (ADR 0001), so this must be the *verbatim* token served
    /// with the diff the user selected from, and the gate must compare it
    /// against a live token minted by that same recipe/namespace. The
    /// codebase already has three incompatible producers (`plan`'s bare
    /// digest, `history-v1:`, `status-v1:`) — #213's diff read serves its
    /// own namespaced token, and comparing across recipes would refuse
    /// forever (see `staging.rs` server-side).
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
    /// A selection with an empty path names no file.
    EmptyPath,
    /// Two selections name the same path — the canonical form is one
    /// selection per file.
    DuplicatePath(String),
    /// A `Hunks`/`Lines` selection with an empty hunk list selects nothing.
    NoHunks(String),
    /// Hunk ordinals must be strictly ascending (unique and ordered) so a
    /// plan has exactly one canonical byte form.
    UnorderedHunks(String),
    /// Header anchors must ascend with their ordinals — real hunk starts
    /// strictly ascend within a file, so anchors that don't are not a stale
    /// selection but a malformed one.
    MisorderedAnchors(String),
    /// A `Lines` entry with no line indices selects nothing from its hunk.
    NoLines(String),
    /// Line indices must be strictly ascending, same canonical-form rule.
    UnorderedLines(String),
}

impl std::fmt::Display for PatchPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFiles => write!(f, "the plan selects no files"),
            Self::EmptyPath => write!(f, "a selection has an empty path"),
            Self::DuplicatePath(p) => write!(f, "path selected twice: {p}"),
            Self::NoHunks(p) => write!(f, "empty hunk selection for {p}"),
            Self::UnorderedHunks(p) => {
                write!(f, "hunk indices not strictly ascending for {p}")
            }
            Self::MisorderedAnchors(p) => {
                write!(f, "hunk anchors do not ascend with their indices for {p}")
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

/// Ordinals and both anchors must each strictly ascend — hunk starts
/// strictly ascend within a real file's diff, so anchors that don't cannot
/// match any diff and fail structurally (400), before the server ever
/// cross-checks them against the pinned diff.
fn well_ordered<'a>(hunks: impl Iterator<Item = &'a HunkRef> + Clone) -> Result<(), Disorder> {
    if !strictly_ascending(hunks.clone().map(|h| h.index)) {
        return Err(Disorder::Index);
    }
    if !strictly_ascending(hunks.clone().map(|h| h.old_start))
        || !strictly_ascending(hunks.map(|h| h.new_start))
    {
        return Err(Disorder::Anchor);
    }
    Ok(())
}

enum Disorder {
    Index,
    Anchor,
}

impl Disorder {
    fn for_path(self, path: &str) -> PatchPlanError {
        match self {
            Self::Index => PatchPlanError::UnorderedHunks(path.to_string()),
            Self::Anchor => PatchPlanError::MisorderedAnchors(path.to_string()),
        }
    }
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
            if file.path.is_empty() {
                return Err(PatchPlanError::EmptyPath);
            }
            if !seen.insert(file.path.as_str()) {
                return Err(PatchPlanError::DuplicatePath(file.path.clone()));
            }
            match &file.selection {
                SelectionShape::EntireFile => {}
                SelectionShape::Hunks { hunks } => {
                    if hunks.is_empty() {
                        return Err(PatchPlanError::NoHunks(file.path.clone()));
                    }
                    well_ordered(hunks.iter()).map_err(|d| d.for_path(&file.path))?;
                }
                SelectionShape::Lines { hunks } => {
                    if hunks.is_empty() {
                        return Err(PatchPlanError::NoHunks(file.path.clone()));
                    }
                    well_ordered(hunks.iter().map(|s| &s.hunk))
                        .map_err(|d| d.for_path(&file.path))?;
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

    // The manual Deserialize's whole reason to exist (see SelectionShape's
    // doc): the derived internally-tagged form silently accepted these.
    #[test]
    fn selection_wire_rejects_stray_and_mismatched_keys() {
        // A stray hunks key next to entire_file — the dangerous one: derived
        // serde parsed this as EntireFile and would stage the whole file.
        assert!(serde_json::from_str::<SelectionShape>(
            r#"{"select":"entire_file","hunks":[{"index":0,"old_start":1,"new_start":1}]}"#
        )
        .is_err());
        // An unknown key on a payload variant.
        assert!(serde_json::from_str::<SelectionShape>(
            r#"{"select":"hunks","hunks":[{"index":0,"old_start":1,"new_start":1}],"extra":true}"#
        )
        .is_err());
        // A payload of the wrong granularity for the tag.
        assert!(serde_json::from_str::<SelectionShape>(
            r#"{"select":"lines","hunks":[{"index":0,"old_start":1,"new_start":1}]}"#
        )
        .is_err());
        assert!(serde_json::from_str::<SelectionShape>(
            r#"{"select":"hunks","hunks":[{"hunk":{"index":0,"old_start":1,"new_start":1},"lines":[0]}]}"#
        )
        .is_err());
        // A missing payload.
        assert!(serde_json::from_str::<SelectionShape>(r#"{"select":"hunks"}"#).is_err());
    }

    // The closed-vocabulary pin plan.rs's there_is_no_catch_all_operation
    // sets for GitOperation, applied here.
    #[test]
    fn there_is_no_catch_all_selection() {
        assert!(serde_json::from_str::<SelectionShape>(r#"{"select":"everything"}"#).is_err());
    }

    #[test]
    fn well_formed_selections_still_round_trip() {
        // The strict route must not over-reject: every legal form parses.
        for json in [
            r#"{"select":"entire_file"}"#,
            r#"{"select":"hunks","hunks":[{"index":0,"old_start":1,"new_start":1}]}"#,
            r#"{"select":"lines","hunks":[{"hunk":{"index":0,"old_start":1,"new_start":1},"lines":[0,2]}]}"#,
        ] {
            let parsed: SelectionShape = serde_json::from_str(json).unwrap();
            let back = serde_json::to_string(&parsed).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&back).unwrap(),
                serde_json::from_str::<serde_json::Value>(json).unwrap(),
                "round trip changed the wire form"
            );
        }
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

    #[test]
    fn impossible_anchors_and_empty_paths_are_rejected() {
        // Ordinals ascend but the anchors run backwards — no real diff has
        // hunk 1 starting before hunk 0, so this is malformed, not stale.
        let backwards = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Hunks {
                hunks: vec![
                    HunkRef {
                        index: 0,
                        old_start: 100,
                        new_start: 100,
                    },
                    HunkRef {
                        index: 1,
                        old_start: 5,
                        new_start: 5,
                    },
                ],
            },
        }]);
        assert_eq!(
            backwards.validate(),
            Err(PatchPlanError::MisorderedAnchors("a.rs".into()))
        );
        // Identical anchors on distinct ordinals are equally impossible.
        let identical = plan(vec![FileSelection {
            path: "a.rs".into(),
            selection: SelectionShape::Hunks {
                hunks: vec![
                    HunkRef {
                        index: 0,
                        old_start: 5,
                        new_start: 5,
                    },
                    HunkRef {
                        index: 1,
                        old_start: 5,
                        new_start: 9,
                    },
                ],
            },
        }]);
        assert_eq!(
            identical.validate(),
            Err(PatchPlanError::MisorderedAnchors("a.rs".into()))
        );
        let unnamed = plan(vec![FileSelection {
            path: String::new(),
            selection: SelectionShape::EntireFile,
        }]);
        assert_eq!(unnamed.validate(), Err(PatchPlanError::EmptyPath));
    }
}
