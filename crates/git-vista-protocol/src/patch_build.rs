//! Building the exact patch a [`PatchPlan`] selects (M2.17b/#213, M2.17c/#214)
//! — pure text work, no repository access, so preview and apply construct the
//! very same bytes and `cargo test` can prove every property on the host.
//!
//! The inputs are the pinned diff (already parsed to [`ParsedPatch`]) and a
//! structurally-valid plan ([`PatchPlan::validate`] has passed). This module
//! owns the *cross-checks between the two* — the remaining 400 class: a
//! selection naming a path the diff doesn't have, a hunk ordinal out of
//! range, an anchor that contradicts the pinned hunk header, a line index out
//! of range or pointed at a context line. Staleness (409) is decided before
//! this runs, by the generation gate; by the time this executes, the bytes
//! are the bytes the user saw.
//!
//! ## Two execution routes, one selection
//!
//! Hunk and line selections both become **patch text** for `git apply
//! --cached` (`--reverse` for unstage). Entire-file selections become
//! **pathspecs** for `git add -- <path>` / `git reset -q HEAD -- <path>`
//! instead: a binary, mode-only, or no-content-rename change has no hunks to
//! put in a text patch (`git diff` without `--binary` prints an unappliable
//! stub for binary content), while the pathspec route stages any shape git
//! itself can stage. The split is invisible on the wire — it is how one plan
//! executes, reported together.
//!
//! ## Reconstruction fidelity
//!
//! A selected whole hunk is re-serialized from [`Hunk`] verbatim: header from
//! the recorded starts/lengths (git's omitted-`,1` shorthand is normalized to
//! explicit `,1`, which `git apply` accepts), each line's marker + text, and
//! `\ No newline at end of file` re-emitted after any line that carried the
//! flag.
//!
//! A line-level selection ([`SelectionShape::Lines`], #214) reconstructs a
//! **sub-hunk** — git's own `add -p` semantics, implemented by
//! [`append_sub_hunk`]:
//!
//!  - Every context line stays context, unconditionally.
//!  - A selected added line stays added (`+`); an unselected added line is
//!    **dropped entirely** — it is not being added yet.
//!  - A selected removed line stays removed (`-`); an unselected removed line
//!    is **reclassified to context** (` `) — it is not being staged for
//!    removal, so the sub-patch must show it as still present on both sides.
//!    Every removed line, selected or not, still occupies exactly one
//!    old-side slot; only whether it *also* occupies a new-side slot differs
//!    by selection.
//!
//! The sub-hunk's `old_len`/`new_len` are computed from the emitted lines
//! (context + all-removed for `old_len`; context + selected-added +
//! unselected-removed for `new_len`) — never assumed. `old_start`/`new_start`
//! are unchanged from the original hunk's header: omitting earlier hunks
//! entirely never shifts a later hunk's anchor (this module's callers only
//! ever address one hunk at a time by its own recorded header), so only the
//! counts change, never the starting positions.

use crate::diff::{FileDiff, Hunk, LineKind, ParsedPatch};
use crate::patch_plan::{FileSelection, HunkLines, HunkRef, PatchPlan, SelectionShape};

/// The executable form of one plan: patch text for the hunk-level part (empty
/// when only whole files were selected) and canonical paths for the
/// entire-file part (empty when only hunks were selected).
///
/// Known divergence (recorded, not hidden): a mode change accompanying
/// content hunks (`old mode`/`new mode` on a chmod'd, edited file) rides
/// only the **entire-file** route — the parser keeps no mode fields on
/// hunk-shaped diffs, so selecting every hunk stages content only and the
/// mode flip stays unstaged (a later diff still shows it; nothing is lost).
/// Carrying modes through the DTO so a client can steer is follow-up work
/// filed on #215.
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
    /// A [`HunkLines`] line index past the end of that hunk's own line list.
    LineOutOfRange {
        path: String,
        hunk_index: u32,
        line_index: u32,
    },
    /// A [`HunkLines`] line index pointed at a context line — [`HunkLines`]'s
    /// own doc promises this is refused, never silently ignored (#214).
    ContextLineSelected {
        path: String,
        hunk_index: u32,
        line_index: u32,
    },
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
            Self::LineOutOfRange {
                path,
                hunk_index,
                line_index,
            } => write!(
                f,
                "line {line_index} is past the end of hunk {hunk_index} of {path}'s line list"
            ),
            Self::ContextLineSelected {
                path,
                hunk_index,
                line_index,
            } => write!(
                f,
                "line {line_index} of hunk {hunk_index} of {path} is a context line — \
                 only added or removed lines may be selected"
            ),
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
/// pinned diff, emit patch text for hunk/line selections and pathspecs for
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
            SelectionShape::Lines { hunks: selected } => {
                append_file_patch_lines(&mut patch, selection, file, selected)?;
            }
        }
    }

    Ok(SelectedPatch { patch, whole_files })
}

/// The `---`/`+++` header pair shared by both hunk-addressable routes (whole
/// hunk and line-level) — the only difference between them is what follows.
fn emit_file_headers(patch: &mut String, old_path: &Option<String>, new_path: &Option<String>) {
    match old_path {
        Some(p) => {
            patch.push_str("--- ");
            push_quoted_path(patch, "a/", p);
        }
        None => patch.push_str("--- /dev/null"),
    }
    patch.push('\n');
    match new_path {
        Some(p) => {
            patch.push_str("+++ ");
            push_quoted_path(patch, "b/", p);
        }
        None => patch.push_str("+++ /dev/null"),
    }
    patch.push('\n');
}

/// One file's contribution to the patch text for a whole-hunk selection:
/// `---`/`+++` headers plus each selected hunk, re-serialized verbatim
/// (module doc).
fn append_file_patch(
    patch: &mut String,
    selection: &FileSelection,
    file: &FileDiff,
    selected: &[HunkRef],
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
    emit_file_headers(patch, old_path, new_path);

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

/// One file's contribution to the patch text for a line-level selection
/// (#214): `---`/`+++` headers plus each selected hunk, reconstructed as a
/// **sub-hunk** by [`append_sub_hunk`] — only the selected lines' worth of
/// change, everything else folded back to context. Cross-checks are the same
/// as [`append_file_patch`] (a `Lines` selection references a [`HunkRef`]
/// exactly like `Hunks` does) plus the two line-specific checks
/// [`HunkLines`]'s own doc promises: every referenced index must exist in the
/// hunk's line list, and must not point at a context line.
fn append_file_patch_lines(
    patch: &mut String,
    selection: &FileSelection,
    file: &FileDiff,
    selected: &[HunkLines],
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
    emit_file_headers(patch, old_path, new_path);

    for sel in selected {
        let hunk_ref = sel.hunk;
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
        for &line_index in &sel.lines {
            match hunk.lines.get(line_index as usize) {
                None => {
                    return Err(SelectionMismatch::LineOutOfRange {
                        path: selection.path.clone(),
                        hunk_index: hunk_ref.index,
                        line_index,
                    });
                }
                Some(line) if line.kind == LineKind::Context => {
                    return Err(SelectionMismatch::ContextLineSelected {
                        path: selection.path.clone(),
                        hunk_index: hunk_ref.index,
                        line_index,
                    });
                }
                Some(_) => {}
            }
        }
        append_sub_hunk(patch, hunk, &sel.lines);
    }
    Ok(())
}

/// Emit `prefix/path`, C-quoting the whole thing the way git does whenever
/// the path carries a byte outside git's safe set — the parser stores real
/// names (`diff::path_or_dev_null` unquotes), so the reconstruction must
/// re-quote or `git apply` mis-reads the header. Quoting when git wouldn't
/// have (e.g. non-ASCII with `core.quotePath=false`) is harmless: `git
/// apply` always accepts the quoted form.
fn push_quoted_path(patch: &mut String, prefix: &str, path: &str) {
    let needs_quoting = path
        .bytes()
        .any(|b| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f || b >= 0x80);
    if !needs_quoting {
        patch.push_str(prefix);
        patch.push_str(path);
        return;
    }
    patch.push('"');
    for b in prefix.bytes().chain(path.bytes()) {
        match b {
            b'"' => patch.push_str("\\\""),
            b'\\' => patch.push_str("\\\\"),
            b'\n' => patch.push_str("\\n"),
            b'\t' => patch.push_str("\\t"),
            b'\r' => patch.push_str("\\r"),
            0x20..=0x7e => patch.push(b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(patch, "\\{b:03o}");
            }
        }
    }
    patch.push('"');
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

/// Reconstruct one hunk restricted to a line-level selection — git's own
/// `add -p` sub-hunk semantics (#214, module doc). `selected_lines` are
/// 0-based indices into `hunk.lines`; the caller has already verified every
/// index exists and names an added or removed line.
///
/// The algorithm, one pass over `hunk.lines`:
///
///  - **Context** lines are always emitted as context, and always count
///    toward both `old_len` and `new_len`.
///  - **Added** lines are emitted (as `+`, counting toward `new_len`) only
///    when selected; an unselected added line is skipped entirely — it
///    contributes to neither side's count, because it is not part of this
///    sub-patch at all.
///  - **Removed** lines always count toward `old_len` (every removed line
///    occupies an old-side slot regardless of selection) but are emitted
///    differently: selected ones stay `-` (no new-side slot); unselected
///    ones are re-emitted as context ` ` (a new-side slot, since the
///    sub-patch is not staging their removal — the line must still show as
///    present after this sub-patch applies).
///
/// `old_len`/`new_len` fall out of the same pass that builds the body, rather
/// than being computed separately and hoped to agree — see the emitted-line
/// counts below. `old_start`/`new_start` are the original hunk's own header
/// values, unchanged (module doc).
fn append_sub_hunk(patch: &mut String, hunk: &Hunk, selected_lines: &[u32]) {
    use std::fmt::Write;

    let selected: std::collections::HashSet<u32> = selected_lines.iter().copied().collect();

    struct Emitted<'a> {
        marker: char,
        text: &'a str,
        no_newline_at_eof: bool,
    }

    let mut body: Vec<Emitted> = Vec::with_capacity(hunk.lines.len());
    let mut old_len: u32 = 0;
    let mut new_len: u32 = 0;

    for (i, line) in hunk.lines.iter().enumerate() {
        let is_selected = selected.contains(&(i as u32));
        match line.kind {
            LineKind::Context => {
                old_len += 1;
                new_len += 1;
                body.push(Emitted {
                    marker: ' ',
                    text: &line.text,
                    no_newline_at_eof: line.no_newline_at_eof,
                });
            }
            LineKind::Added => {
                if is_selected {
                    new_len += 1;
                    body.push(Emitted {
                        marker: '+',
                        text: &line.text,
                        no_newline_at_eof: line.no_newline_at_eof,
                    });
                }
                // Unselected added line: dropped entirely, no count either side.
            }
            LineKind::Removed => {
                old_len += 1;
                if is_selected {
                    body.push(Emitted {
                        marker: '-',
                        text: &line.text,
                        no_newline_at_eof: line.no_newline_at_eof,
                    });
                } else {
                    // Reclassified to context: still present on the new side
                    // too, since this sub-patch does not remove it.
                    new_len += 1;
                    body.push(Emitted {
                        marker: ' ',
                        text: &line.text,
                        no_newline_at_eof: line.no_newline_at_eof,
                    });
                }
            }
        }
    }

    // A `\ No newline at end of file` marker is a positional claim: the
    // side(s) it names end exactly here, nothing of that side following. The
    // OLD side's ordering can never change — every Removed line, selected or
    // not, still occupies its original old-side slot, so a flag on a `-`
    // entry (still removed) always lands correctly as copied. The NEW side
    // is different: reclassification (above) can insert a formerly-old-only
    // line into the new side, so a flag copied verbatim from the source line
    // can end up on an entry that is no longer new-side's terminal one —
    // `git apply` accepts the self-contradictory result and silently
    // concatenates two lines with no separating newline (confirmed against
    // real git; this fixup exists because of that finding). Re-verify against
    // the *reconstructed* body, not the source line:
    //
    //  - a `+` entry whose flag no longer matches "nothing new-side follows"
    //    just has the flag dropped. This is not a conservative fallback —
    //    it's the correct answer: if new-side content now follows, this line
    //    genuinely needs an ordinary trailing newline, exactly what omitting
    //    the marker asserts.
    //  - a reclassified `-`-turned-context entry in the same situation can't
    //    be fixed by dropping alone: it legitimately still IS old-side's
    //    terminal content (that never moves) while no longer being new-side's
    //    — one context line cannot assert two different endings. Split back
    //    into the removed+added pair a real diff would use for this exact
    //    transition: the removed half keeps the flag, a same-text added half
    //    carries none. old_len/new_len (already computed above) are
    //    unaffected — a context slot's dual accounting and an equivalent
    //    removed+added pair contribute the same one old-side and one
    //    new-side unit either way.
    let last_new_idx = body.iter().rposition(|e| e.marker != '-');
    let mut fixed: Vec<Emitted> = Vec::with_capacity(body.len() + 1);
    for (i, e) in body.into_iter().enumerate() {
        let new_side_ends_here = last_new_idx == Some(i);
        match e.marker {
            '+' if e.no_newline_at_eof && !new_side_ends_here => {
                fixed.push(Emitted {
                    marker: '+',
                    text: e.text,
                    no_newline_at_eof: false,
                });
            }
            ' ' if e.no_newline_at_eof && !new_side_ends_here => {
                fixed.push(Emitted {
                    marker: '-',
                    text: e.text,
                    no_newline_at_eof: true,
                });
                fixed.push(Emitted {
                    marker: '+',
                    text: e.text,
                    no_newline_at_eof: false,
                });
            }
            _ => fixed.push(e),
        }
    }

    let _ = write!(
        patch,
        "@@ -{},{} +{},{} @@",
        hunk.old_start, old_len, hunk.new_start, new_len
    );
    if !hunk.section_heading.is_empty() {
        patch.push(' ');
        patch.push_str(&hunk.section_heading);
    }
    patch.push('\n');
    for line in fixed {
        patch.push(line.marker);
        patch.push_str(line.text);
        patch.push('\n');
        if line.no_newline_at_eof {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffLine;
    use crate::parse_unified_diff;
    use crate::patch_plan::StageDirection;
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

    fn select_lines(path: &str, hunks: Vec<HunkLines>) -> FileSelection {
        FileSelection {
            path: path.into(),
            selection: SelectionShape::Lines { hunks },
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
    }

    // #214: the two line-specific mismatches, same "its own refusal" posture
    // as the hunk-level ones above (a Lines selection references a HunkRef
    // exactly like Hunks does, so the anchor/out-of-range hunk checks are
    // shared code — these two are what's new).
    #[test]
    fn line_level_mismatches_are_their_own_refusal() {
        let pinned = parse_unified_diff(DIFF);
        // Hunk 0 of src/foo.rs has 5 lines (indices 0..=4): context, added,
        // added, context, removed.
        let href = HunkRef {
            index: 0,
            old_start: 10,
            new_start: 10,
        };
        let out_of_range = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "src/foo.rs",
                vec![HunkLines {
                    hunk: href,
                    lines: vec![99],
                }],
            )]),
        );
        assert_eq!(
            out_of_range,
            Err(SelectionMismatch::LineOutOfRange {
                path: "src/foo.rs".into(),
                hunk_index: 0,
                line_index: 99,
            })
        );
        // Index 0 is the leading context line.
        let context_selected = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "src/foo.rs",
                vec![HunkLines {
                    hunk: href,
                    lines: vec![0],
                }],
            )]),
        );
        assert_eq!(
            context_selected,
            Err(SelectionMismatch::ContextLineSelected {
                path: "src/foo.rs".into(),
                hunk_index: 0,
                line_index: 0,
            })
        );
        // A Lines selection whose HunkRef fails the same anchor cross-check
        // Hunks selections use.
        let bad_anchor = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "src/foo.rs",
                vec![HunkLines {
                    hunk: HunkRef {
                        index: 0,
                        old_start: 999,
                        new_start: 10,
                    },
                    lines: vec![1],
                }],
            )]),
        );
        assert_eq!(
            bad_anchor,
            Err(SelectionMismatch::AnchorMismatch {
                path: "src/foo.rs".into(),
                index: 0,
            })
        );
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

    // --- Task 1: the sub-hunk reconstruction algorithm --------------------

    /// A hunk with a mix of added and removed lines, wide enough to exercise
    /// every combination the algorithm distinguishes: selected/unselected
    /// added, selected/unselected removed, plus plain context.
    const MIXED_DIFF: &str = "\
diff --git a/m.rs b/m.rs
index 111..222 100644
--- a/m.rs
+++ b/m.rs
@@ -1,4 +1,4 @@
 context one
-removed a
-removed b
+added a
+added b
 context two
";

    // Line indices in MIXED_DIFF's single hunk:
    // 0: context "context one"
    // 1: removed "removed a"
    // 2: removed "removed b"
    // 3: added "added a"
    // 4: added "added b"
    // 5: context "context two"

    fn mixed_href() -> HunkRef {
        HunkRef {
            index: 0,
            old_start: 1,
            new_start: 1,
        }
    }

    #[test]
    fn selecting_one_added_and_one_removed_line_builds_the_exact_sub_hunk() {
        // Select "removed a" (1, stays removed) and "added a" (3, stays
        // added); "removed b" is unselected (reclassified to context) and
        // "added b" is unselected (dropped entirely).
        let pinned = parse_unified_diff(MIXED_DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "m.rs",
                vec![HunkLines {
                    hunk: mixed_href(),
                    lines: vec![1, 3],
                }],
            )]),
        )
        .unwrap();
        // old side: context one, removed a (-), removed b (now context),
        // context two = 4 lines. new side: context one, added a (+), removed
        // b (context), context two = 4 lines.
        assert_eq!(
            built.patch,
            "--- a/m.rs\n\
             +++ b/m.rs\n\
             @@ -1,4 +1,4 @@\n\
             \x20context one\n\
             -removed a\n\
             \x20removed b\n\
             +added a\n\
             \x20context two\n"
        );
    }

    #[test]
    fn selecting_only_an_unselected_removed_line_stays_context_on_both_sides() {
        // Select nothing but "removed b" (2): "removed a" unselected
        // (context), both added lines unselected (dropped).
        let pinned = parse_unified_diff(MIXED_DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "m.rs",
                vec![HunkLines {
                    hunk: mixed_href(),
                    lines: vec![2],
                }],
            )]),
        )
        .unwrap();
        assert_eq!(
            built.patch,
            "--- a/m.rs\n\
             +++ b/m.rs\n\
             @@ -1,4 +1,3 @@\n\
             \x20context one\n\
             \x20removed a\n\
             -removed b\n\
             \x20context two\n"
        );
    }

    #[test]
    fn selecting_both_added_lines_and_no_removed_lines_only_adds() {
        // Select "added a" (3) and "added b" (4); both removed lines
        // unselected, so both become context.
        let pinned = parse_unified_diff(MIXED_DIFF);
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "m.rs",
                vec![HunkLines {
                    hunk: mixed_href(),
                    lines: vec![3, 4],
                }],
            )]),
        )
        .unwrap();
        assert_eq!(
            built.patch,
            "--- a/m.rs\n\
             +++ b/m.rs\n\
             @@ -1,4 +1,6 @@\n\
             \x20context one\n\
             \x20removed a\n\
             \x20removed b\n\
             +added a\n\
             +added b\n\
             \x20context two\n"
        );
    }

    #[test]
    fn selecting_every_line_reproduces_the_whole_hunk_verbatim() {
        // The degenerate case: selecting every added/removed line must
        // byte-match what append_file_patch (whole-hunk) would have built.
        let pinned = parse_unified_diff(MIXED_DIFF);
        let via_lines = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "m.rs",
                vec![HunkLines {
                    hunk: mixed_href(),
                    lines: vec![1, 2, 3, 4],
                }],
            )]),
        )
        .unwrap();
        let via_hunk = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks("m.rs", vec![mixed_href()])]),
        )
        .unwrap();
        assert_eq!(via_lines.patch, via_hunk.patch);
    }

    // --- Task 5: whitespace-only content is not special-cased -------------

    #[test]
    fn whitespace_only_changes_round_trip_byte_exact() {
        // A hunk whose only changed lines differ purely in whitespace: one
        // pair swaps a leading tab for spaces (selected), the other adds a
        // trailing space (left unselected, so it exercises both halves of
        // the algorithm — reclassify-to-context and drop-entirely — on
        // whitespace content specifically). No trimming, no collapsing, no
        // special-casing: byte-exact throughout.
        let diff = "diff --git a/w.rs b/w.rs\nindex 111..222 100644\n--- a/w.rs\n+++ b/w.rs\n@@ -1,4 +1,4 @@\n context\n-\tindented with tab\n+    indented with spaces\n-trailing\n+trailing \n context two\n";
        let pinned = parse_unified_diff(diff);
        let href = HunkRef {
            index: 0,
            old_start: 1,
            new_start: 1,
        };
        // Line indices: 0 context, 1 removed (tab), 2 added (spaces),
        // 3 removed ("trailing"), 4 added ("trailing " + trailing space),
        // 5 context. Select only the tab->spaces pair.
        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_lines(
                "w.rs",
                vec![HunkLines {
                    hunk: href,
                    lines: vec![1, 2],
                }],
            )]),
        )
        .unwrap();
        assert_eq!(
            built.patch,
            "--- a/w.rs\n\
             +++ b/w.rs\n\
             @@ -1,4 +1,4 @@\n\
             \x20context\n\
             -\tindented with tab\n\
             +    indented with spaces\n\
             \x20trailing\n\
             \x20context two\n",
            "the unselected removed line reverts to its OWN text \
             (\"trailing\", no trailing space) — never the added line's text"
        );
    }

    // --- Task 3: CRLF byte-exact round trip --------------------------------

    #[test]
    fn crlf_content_lines_round_trip_with_the_carriage_return_intact() {
        // str::lines() strips a trailing \r that precedes \n, which is
        // exactly the byte a CRLF file's content lines carry (verified
        // against real `git diff` output — see planner.rs's host test and
        // diff.rs's split_diff_lines doc). Assert the parsed DiffLine text
        // still has it, and that reconstruction re-emits it.
        let diff = "diff --git a/c.txt b/c.txt\nindex 111..222 100644\n--- a/c.txt\n+++ b/c.txt\n@@ -1,3 +1,4 @@\n one\r\n-two\r\n+TWO\r\n three\r\n+four\r\n";
        let pinned = parse_unified_diff(diff);
        let crate::diff::FileDiff::Hunks { hunks, .. } = &pinned.files[0] else {
            panic!("expected Hunks");
        };
        assert_eq!(
            hunks[0].lines[0],
            DiffLine {
                kind: LineKind::Context,
                text: "one\r".into(),
                no_newline_at_eof: false,
            },
            "the trailing \\r must survive parsing, not just the \\n"
        );

        let built = build_selected_patch(
            &pinned,
            &plan(vec![select_hunks(
                "c.txt",
                vec![HunkRef {
                    index: 0,
                    old_start: 1,
                    new_start: 1,
                }],
            )]),
        )
        .unwrap();
        assert_eq!(
            built.patch,
            "--- a/c.txt\n\
             +++ b/c.txt\n\
             @@ -1,3 +1,4 @@\n\
             \x20one\r\n\
             -two\r\n\
             +TWO\r\n\
             \x20three\r\n\
             +four\r\n",
            "reconstruction must preserve every \\r byte-exact"
        );
    }
}
