//! Building the exact patch a [`PatchPlan`] selects (M2.17b, #213) — pure
//! text work, no repository access, so preview and apply construct the very
//! same bytes and `cargo test` can prove every property on the host.
//!
//! The inputs are the pinned diff (already parsed to [`ParsedPatch`]) and a
//! structurally-valid plan ([`PatchPlan::validate`] has passed). This module
//! owns the *cross-checks between the two* — the remaining 400 class: a
//! selection naming a path the diff doesn't have, a hunk ordinal out of
//! range, an anchor that contradicts the pinned hunk header. Staleness (409)
//! is decided before this runs, by the generation gate; by the time this
//! executes, the bytes are the bytes the user saw.
//!
//! ## Two execution routes, one selection
//!
//! Hunk selections become **patch text** for `git apply --cached`
//! (`--reverse` for unstage). Entire-file selections become **pathspecs**
//! for `git add -- <path>` / `git reset -q HEAD -- <path>` instead: a
//! binary, mode-only, or no-content-rename change has no hunks to put in a
//! text patch (`git diff` without `--binary` prints an unappliable stub for
//! binary content), while the pathspec route stages any shape git itself
//! can stage. The split is invisible on the wire — it is how one plan
//! executes, reported together.
//!
//! ## Reconstruction fidelity
//!
//! A selected hunk is re-serialized from [`Hunk`] verbatim: header from the
//! recorded starts/lengths (git's omitted-`,1` shorthand is normalized to
//! explicit `,1`, which `git apply` accepts), each line's marker + text, and
//! `\ No newline at end of file` re-emitted after any line that carried the
//! flag. #213 never omits lines *within* a hunk, so headers stay correct
//! without `--recount`; that arithmetic arrives with #214's line-level
//! execution.

use crate::diff::{FileDiff, Hunk, LineKind, ParsedPatch};
use crate::patch_plan::{FileSelection, PatchPlan, SelectionShape};

/// The executable form of one plan: patch text for the hunk-level part (empty
/// when only whole files were selected) and canonical paths for the
/// entire-file part (empty when only hunks were selected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedPatch {
    /// Unified-diff text for `git apply --cached` — exactly what a preview
    /// shows.
    pub patch: String,
    /// Canonical paths staged/unstaged whole, via pathspec.
    pub whole_files: Vec<String>,
}

/// Why a structurally-valid plan cannot be built against the pinned diff —
/// still the 400 class (the plan is *wrong about the diff*, and retrying
/// unchanged is pointless), distinct from the gate's 409 (the diff moved,
/// refresh and retry is exactly right).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionMismatch {
    /// No file in the pinned diff has this canonical path.
    UnknownPath(String),
    /// A hunk/line selection addressed a file whose diff shape has no hunks
    /// (binary, mode-only, no-content rename, combined merge).
    NotHunkAddressable(String),
    /// A hunk ordinal past the end of the file's hunk list.
    HunkOutOfRange { path: String, index: u32 },
    /// The anchor cross-check failed: the ordinal exists but its pinned
    /// header starts elsewhere — an indexing bug, not staleness (module doc).
    AnchorMismatch { path: String, index: u32 },
    /// `SelectionShape::Lines` — the wire shape is pinned (#212) but its
    /// execution is #214's scope, and pretending otherwise here would stage
    /// the wrong thing.
    LineLevelNotImplemented,
}

impl std::fmt::Display for SelectionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPath(p) => {
                write!(f, "the pinned diff has no file at {p}")
            }
            Self::NotHunkAddressable(p) => write!(
                f,
                "{p} has no hunks to select — stage the entire file instead"
            ),
            Self::HunkOutOfRange { path, index } => {
                write!(f, "hunk {index} is past the end of {path}'s hunk list")
            }
            Self::AnchorMismatch { path, index } => write!(
                f,
                "hunk {index} of {path} does not start where the selection \
                 says it does — selection and diff disagree about indexing"
            ),
            Self::LineLevelNotImplemented => {
                write!(f, "line-level staging is not implemented yet (issue #214)")
            }
        }
    }
}

/// The canonical path of one [`FileDiff`] — the module-doc rule from
/// `patch_plan`: new-side when present, else old-side; single-path variants
/// name themselves. `None` only for the degenerate both-sides-`/dev/null`
/// shape git never emits.
pub fn canonical_path(file: &FileDiff) -> Option<&str> {
    match file {
        FileDiff::Hunks {
            old_path, new_path, ..
        }
        | FileDiff::Binary {
            old_path, new_path, ..
        } => new_path.as_deref().or(old_path.as_deref()),
        FileDiff::ModeChangeOnly { path, .. } | FileDiff::Combined { path, .. } => Some(path),
        FileDiff::Renamed { new_path, .. } => Some(new_path),
    }
}

/// Build the executable selection: cross-check every reference against the
/// pinned diff, emit patch text for hunk selections and pathspecs for
/// entire-file selections. Pure; both preview and apply call exactly this.
pub fn build_selected_patch(
    pinned: &ParsedPatch,
    plan: &PatchPlan,
) -> Result<SelectedPatch, SelectionMismatch> {
    let by_path: std::collections::HashMap<&str, &FileDiff> = pinned
        .files
        .iter()
        .filter_map(|f| canonical_path(f).map(|p| (p, f)))
        .collect();

    let mut patch = String::new();
    let mut whole_files = Vec::new();

    for selection in &plan.files {
        let Some(file) = by_path.get(selection.path.as_str()) else {
            return Err(SelectionMismatch::UnknownPath(selection.path.clone()));
        };
        match &selection.selection {
            SelectionShape::EntireFile => whole_files.push(selection.path.clone()),
            SelectionShape::Hunks { hunks: selected } => {
                append_file_patch(&mut patch, selection, file, selected)?;
            }
            SelectionShape::Lines { .. } => {
                return Err(SelectionMismatch::LineLevelNotImplemented);
            }
        }
    }

    Ok(SelectedPatch { patch, whole_files })
}

/// One file's contribution to the patch text: `---`/`+++` headers plus each
/// selected hunk, re-serialized verbatim (module doc).
fn append_file_patch(
    patch: &mut String,
    selection: &FileSelection,
    file: &FileDiff,
    selected: &[crate::patch_plan::HunkRef],
) -> Result<(), SelectionMismatch> {
    let FileDiff::Hunks {
        old_path,
        new_path,
        hunks,
    } = file
    else {
        return Err(SelectionMismatch::NotHunkAddressable(
            selection.path.clone(),
        ));
    };
    match old_path {
        Some(p) => {
            patch.push_str("--- a/");
            patch.push_str(p);
        }
        None => patch.push_str("--- /dev/null"),
    }
    patch.push('\n');
    match new_path {
        Some(p) => {
            patch.push_str("+++ b/");
            patch.push_str(p);
        }
        None => patch.push_str("+++ /dev/null"),
    }
    patch.push('\n');

    for hunk_ref in selected {
        let Some(hunk) = hunks.get(hunk_ref.index as usize) else {
            return Err(SelectionMismatch::HunkOutOfRange {
                path: selection.path.clone(),
                index: hunk_ref.index,
            });
        };
        if hunk.old_start != hunk_ref.old_start || hunk.new_start != hunk_ref.new_start {
            return Err(SelectionMismatch::AnchorMismatch {
                path: selection.path.clone(),
                index: hunk_ref.index,
            });
        }
        append_hunk(patch, hunk);
    }
    Ok(())
}

fn append_hunk(patch: &mut String, hunk: &Hunk) {
    use std::fmt::Write;
    let _ = write!(
        patch,
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
    );
    if !hunk.section_heading.is_empty() {
        patch.push(' ');
        patch.push_str(&hunk.section_heading);
    }
    patch.push('\n');
    for line in &hunk.lines {
        patch.push(match line.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        });
        patch.push_str(&line.text);
        patch.push('\n');
        if line.no_newline_at_eof {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_unified_diff;
    use crate::patch_plan::{HunkRef, StageDirection};
    use crate::plan::{GenerationToken, RepositoryToken, WorktreeToken};

    // Two files: foo.rs with two hunks (second has a heading and a
    // no-trailing-newline final line), bar.txt binary.
    const DIFF: &str = "\
diff --git a/src/foo.rs b/src/foo.rs
index 111..222 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,3 +10,4 @@
 context
+added one
+added two
 context
-removed one
@@ -30,2 +31,2 @@ fn frobnicate()
 context
-old line
+new line
\\ No newline at end of file
diff --git a/bar.bin b/bar.bin
Binary files a/bar.bin and b/bar.bin differ
";

    fn plan(files: Vec<FileSelection>) -> PatchPlan {
        PatchPlan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("diff-v1:42").unwrap(),
            direction: StageDirection::Stage,
            files,
        }
    }

    fn select_hunks(path: &str, hunks: Vec<HunkRef>) -> FileSelection {
        FileSelection {
            path: path.into(),
            selection: SelectionShape::Hunks { hunks },
        }
    }

    #[test]
    fn a_selected_hunk_reserializes_verbatim_including_the_no_newline_marker() {
        let pinned = parse_unified_diff(DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "src/foo.rs",
                vec![HunkRef {
                    index: 1,
                    old_start: 30,
                    new_start: 31,
                }],
            )]),
        )
        .unwrap();
        assert_eq!(
            built.patch,
            "--- a/src/foo.rs\n\
             +++ b/src/foo.rs\n\
             @@ -30,2 +31,2 @@ fn frobnicate()\n\
             \x20context\n\
             -old line\n\
             +new line\n\
             \\ No newline at end of file\n"
        );
        assert!(built.whole_files.is_empty());
    }

    #[test]
    fn entire_file_selections_become_pathspecs_even_for_binaries() {
        let pinned = parse_unified_diff(DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![FileSelection {
                path: "bar.bin".into(),
                selection: SelectionShape::EntireFile,
            }]),
        )
        .unwrap();
        assert_eq!(built.patch, "");
        assert_eq!(built.whole_files, vec!["bar.bin".to_string()]);
    }

    #[test]
    fn every_mismatch_is_its_own_refusal() {
        let pinned = parse_unified_diff(DIFF);
        let unknown =
            build_selected_patch(&pinned, &plan(vec![select_hunks("no/such.rs", vec![])]));
        assert_eq!(
            unknown,
            Err(SelectionMismatch::UnknownPath("no/such.rs".into()))
        );
        let binary_hunks = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "bar.bin",
                vec![HunkRef {
                    index: 0,
                    old_start: 1,
                    new_start: 1,
                }],
            )]),
        );
        assert_eq!(
            binary_hunks,
            Err(SelectionMismatch::NotHunkAddressable("bar.bin".into()))
        );
        let out_of_range = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "src/foo.rs",
                vec![HunkRef {
                    index: 7,
                    old_start: 1,
                    new_start: 1,
                }],
            )]),
        );
        assert_eq!(
            out_of_range,
            Err(SelectionMismatch::HunkOutOfRange {
                path: "src/foo.rs".into(),
                index: 7
            })
        );
        let bad_anchor = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "src/foo.rs",
                vec![HunkRef {
                    index: 0,
                    old_start: 99,
                    new_start: 10,
                }],
            )]),
        );
        assert_eq!(
            bad_anchor,
            Err(SelectionMismatch::AnchorMismatch {
                path: "src/foo.rs".into(),
                index: 0
            })
        );
        let lines = build_selected_patch(
            &pinned,
            &plan(vec![FileSelection {
                path: "src/foo.rs".into(),
                selection: SelectionShape::Lines { hunks: vec![] },
            }]),
        );
        assert_eq!(lines, Err(SelectionMismatch::LineLevelNotImplemented));
    }

    #[test]
    fn selecting_both_hunks_of_a_file_reproduces_its_whole_patch_body() {
        // The built patch for "everything in foo.rs" must byte-match the
        // pinned diff's own hunk bodies — the reserialization is verbatim,
        // only the `diff --git`/`index` decoration is dropped.
        let pinned = parse_unified_diff(DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "src/foo.rs",
                vec![
                    HunkRef {
                        index: 0,
                        old_start: 10,
                        new_start: 10,
                    },
                    HunkRef {
                        index: 1,
                        old_start: 30,
                        new_start: 31,
                    },
                ],
            )]),
        )
        .unwrap();
        let expected: String = DIFF
            .lines()
            .skip(2) // drop `diff --git` + `index`
            .take_while(|l| !l.starts_with("diff --git"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(built.patch, expected);
    }

    #[test]
    fn a_deleted_file_names_dev_null_on_the_new_side() {
        let diff = "\
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-line one
-line two
";
        let pinned = parse_unified_diff(diff);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "gone.rs",
                vec![HunkRef {
                    index: 0,
                    old_start: 1,
                    new_start: 0,
                }],
            )]),
        )
        .unwrap();
        assert!(built.patch.starts_with("--- a/gone.rs\n+++ /dev/null\n"));
    }
}
