//! Flattening a [`ParsedPatch`] into the rows a diff view renders (#361).
//!
//! ## Why this exists
//!
//! [`super::core::hunk_nav`] re-derives hunk structure by walking raw patch
//! text with a marker/countdown scan — structure the parser
//! ([`git_vista_protocol::diff::parse_unified_diff`], #69a) has *already*
//! produced. Two independent derivations of the same fact is how they drift:
//! a body line beginning `+++` or `@@` is a genuine hazard for the text walk
//! and a non-event for the parser.
//!
//! ## The two coordinates, kept deliberately separate
//!
//! The old renderer keyed everything on one number — the index into
//! `patch.lines()` — because raw text was the only thing it had. Structured
//! rendering needs **two**, and conflating them is the bug this module exists
//! to prevent:
//!
//! * **Row index** — position in the flattened `Vec<DiffRow>`. This is what
//!   virtualization windows over (#350) and what `CumulativeHeights` measures.
//!   It counts *everything*: file headers, hunk headers, body lines, notes.
//! * **Hunk ordinal** — position among navigable hunks, counted **globally
//!   across files**. This is what [`GraphFocus`] roves over (#210), and it is
//!   what "hunk 3 of 40" means to a screen reader.
//!
//! A window shows some rows; it never changes how many hunks exist. #350's
//! warning is exactly this: a windowed hunk count silently renumbers hunks as
//! the user scrolls.
//!
//! ## What is deliberately NOT navigable
//!
//! A combined (merge) diff is opaque to the parser
//! ([`FileDiff::Combined`]), so its `@@@` headers are rendered as inert text.
//! That matches the shipped behaviour: header-coloured, but never wearing the
//! interactive styling that would make it look tappable.

use super::core::LineWrap;
use git_vista_protocol::diff::{FileDiff, Hunk, LineKind, ParsedPatch};

/// One rendered row. Every variant occupies exactly one row index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    /// The file's own heading — one per `FileDiff`, whatever its shape.
    FileHeader { file_index: usize, title: String },
    /// A navigable `@@` header. `hunk_ordinal` is global across files.
    HunkHeader {
        file_index: usize,
        hunk_ordinal: usize,
        label: String,
        text: String,
    },
    /// One body line of a hunk.
    Line {
        file_index: usize,
        hunk_ordinal: usize,
        kind: LineKind,
        text: String,
        no_newline_at_eof: bool,
    },
    /// A file with no hunks to navigate: binary, mode-change-only, pure
    /// rename — or a combined diff's raw text, which is shown but never
    /// navigable.
    Note { file_index: usize, text: String },
}

impl DiffRow {
    /// The hunk this row belongs to, if any. `None` for file headers and
    /// notes — the rows a hunk-wise reader skips over.
    pub fn hunk_ordinal(&self) -> Option<usize> {
        match self {
            DiffRow::HunkHeader { hunk_ordinal, .. } | DiffRow::Line { hunk_ordinal, .. } => {
                Some(*hunk_ordinal)
            }
            DiffRow::FileHeader { .. } | DiffRow::Note { .. } => None,
        }
    }

    /// True for rows [`GraphFocus`] can land on — the navigable hunk headers,
    /// and nothing else.
    pub fn is_nav_stop(&self) -> bool {
        matches!(self, DiffRow::HunkHeader { .. })
    }
}

impl DiffRow {
    /// The text this row actually renders — what a width measurement must
    /// count. A body line renders with its marker restored, because that
    /// character occupies a column on screen even though the parser strips
    /// it from `text`; forgetting it under-measures every added and removed
    /// line by one column, and a wrapped patch then reports a height slightly
    /// short of what it draws.
    pub fn display_text(&self) -> String {
        match self {
            DiffRow::FileHeader { title, .. } => title.clone(),
            DiffRow::HunkHeader { text, .. } => text.clone(),
            DiffRow::Note { text, .. } => text.clone(),
            DiffRow::Line { kind, text, .. } => {
                let marker = match kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                format!("{marker}{text}")
            }
        }
    }
}

/// Per-ROW heights, ready for [`CumulativeHeights::new`] — the rows twin of
/// [`super::core::line_heights`].
///
/// A separate function rather than a reuse, because the two walk different
/// sequences: `line_heights` walks `patch.lines()`, which includes the
/// `diff --git`/`index`/`---`/`+++` headers the parser drops and excludes the
/// per-file heading rows this view adds. Measuring rows with a line-based
/// walk would offset every height in the document, and the window would then
/// render one slice while the scrollbar described another.
///
/// Counts **characters, not bytes**, matching `line_heights`: a patch
/// carrying non-ASCII would otherwise over-estimate its height and leave a
/// gap at the bottom of the scroll range.
///
/// [`CumulativeHeights::new`]: git_vista_core::virtualize::CumulativeHeights::new
pub fn row_heights(rows: &[DiffRow], line_height: f64, wrap: LineWrap) -> Vec<f64> {
    rows.iter()
        .map(|row| match wrap {
            LineWrap::Never => line_height,
            LineWrap::Wrapped { columns } => {
                if columns == 0 {
                    // A zero-width container is not a real layout; treat it
                    // as one row rather than dividing by zero.
                    return line_height;
                }
                let chars = row.display_text().chars().count().max(1);
                chars.div_ceil(columns) as f64 * line_height
            }
        })
        .collect()
}

/// The flattened patch plus the hunk count focus needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffRows {
    pub rows: Vec<DiffRow>,
    /// Navigable hunks across the whole patch. **Not** the row count, and
    /// never reduced by windowing.
    pub hunk_count: usize,
}

impl DiffRows {
    /// Row index of the header for `ordinal`, for scroll-into-view before
    /// focusing (#350's reveal-then-refocus dance).
    pub fn row_of_hunk(&self, ordinal: usize) -> Option<usize> {
        self.rows.iter().position(
            |row| matches!(row, DiffRow::HunkHeader { hunk_ordinal, .. } if *hunk_ordinal == ordinal),
        )
    }
}

/// Flatten a parsed patch into rows.
pub fn flatten(patch: &ParsedPatch) -> DiffRows {
    let mut rows = Vec::new();
    let mut hunk_ordinal = 0usize;

    for (file_index, file) in patch.files.iter().enumerate() {
        rows.push(DiffRow::FileHeader {
            file_index,
            title: file_title(file),
        });

        match file {
            FileDiff::Hunks { hunks, .. } => {
                let name = display_path(file);
                for (within_file, hunk) in hunks.iter().enumerate() {
                    rows.push(DiffRow::HunkHeader {
                        file_index,
                        hunk_ordinal,
                        label: hunk_label(&name, within_file, hunk),
                        text: hunk_header_text(hunk),
                    });
                    for line in &hunk.lines {
                        rows.push(DiffRow::Line {
                            file_index,
                            hunk_ordinal,
                            kind: line.kind,
                            text: line.text.clone(),
                            no_newline_at_eof: line.no_newline_at_eof,
                        });
                    }
                    hunk_ordinal += 1;
                }
            }
            // Everything below has no navigable hunk: the file header plus
            // one note row describing what changed.
            FileDiff::ModeChangeOnly {
                old_mode, new_mode, ..
            } => rows.push(DiffRow::Note {
                file_index,
                text: format!("mode changed from {old_mode} to {new_mode}"),
            }),
            FileDiff::Binary { .. } => rows.push(DiffRow::Note {
                file_index,
                text: "binary file — contents not shown".to_string(),
            }),
            FileDiff::Renamed {
                similarity,
                is_copy,
                ..
            } => rows.push(DiffRow::Note {
                file_index,
                text: format!(
                    "{} with no content change ({similarity}% similar)",
                    if *is_copy { "copied" } else { "renamed" }
                ),
            }),
            // A combined diff is opaque by design. Its raw text is shown one
            // line per row so windowing still bounds it, but no line is a
            // navigation stop — including its `@@@` headers.
            FileDiff::Combined { raw, .. } => {
                for line in raw.lines() {
                    rows.push(DiffRow::Note {
                        file_index,
                        text: line.to_string(),
                    });
                }
            }
        }
    }

    DiffRows {
        rows,
        hunk_count: hunk_ordinal,
    }
}

/// The `@@ … @@` text as git printed it. Reconstructed rather than carried
/// through, because the parser keeps the numbers and drops the formatting —
/// and the numbers are what a reader checks against their own file.
fn hunk_header_text(hunk: &Hunk) -> String {
    let head = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
    );
    if hunk.section_heading.is_empty() {
        head
    } else {
        format!("{head} {}", hunk.section_heading)
    }
}

/// The spoken label, matching [`super::core::hunk_nav`]'s shape: file, the
/// per-file ordinal, the new-side range, then the counts. Leads with file and
/// position because that is what orients a listener — the raw `-12,5 +12,8`
/// shorthand does not.
fn hunk_label(file: &str, within_file: usize, hunk: &Hunk) -> String {
    let (added, removed) = hunk
        .lines
        .iter()
        .fold((0u32, 0u32), |(a, r), line| match line.kind {
            LineKind::Added => (a + 1, r),
            LineKind::Removed => (a, r + 1),
            LineKind::Context => (a, r),
        });
    format!(
        "{file} hunk {} at line {}, {} added, {} removed",
        within_file + 1,
        hunk.new_start,
        added,
        removed
    )
}

/// The name to show for a file, preferring the new side. A deleted file has
/// no new side, so the old one is the only name it has.
fn display_path(file: &FileDiff) -> String {
    match file {
        FileDiff::Hunks {
            old_path, new_path, ..
        }
        | FileDiff::Binary {
            old_path, new_path, ..
        } => new_path
            .clone()
            .or_else(|| old_path.clone())
            .unwrap_or_else(|| "(unknown)".to_string()),
        FileDiff::ModeChangeOnly { path, .. } | FileDiff::Combined { path, .. } => path.clone(),
        FileDiff::Renamed { new_path, .. } => new_path.clone(),
    }
}

fn file_title(file: &FileDiff) -> String {
    match file {
        FileDiff::Hunks {
            old_path, new_path, ..
        } => match (old_path, new_path) {
            (None, Some(new)) => format!("{new} (new file)"),
            (Some(old), None) => format!("{old} (deleted)"),
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            _ => display_path(file),
        },
        FileDiff::Renamed {
            old_path, new_path, ..
        } => format!("{old_path} → {new_path}"),
        _ => display_path(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::diff::parse_unified_diff;
    use git_vista_protocol::diff::DiffLine;

    fn line(kind: LineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.to_string(),
            no_newline_at_eof: false,
        }
    }

    fn hunk(new_start: u32, lines: Vec<DiffLine>) -> Hunk {
        Hunk {
            old_start: new_start,
            old_len: lines.iter().filter(|l| l.kind != LineKind::Added).count() as u32,
            new_start,
            new_len: lines.iter().filter(|l| l.kind != LineKind::Removed).count() as u32,
            section_heading: String::new(),
            lines,
        }
    }

    fn hunks_file(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff::Hunks {
            old_path: Some(path.to_string()),
            new_path: Some(path.to_string()),
            hunks,
        }
    }

    // ── the two coordinates ──

    #[test]
    fn an_empty_patch_flattens_to_nothing() {
        let flat = flatten(&ParsedPatch::default());
        assert!(flat.rows.is_empty());
        assert_eq!(flat.hunk_count, 0);
    }

    #[test]
    fn a_file_contributes_a_header_row_then_its_hunk_rows() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Added, "x")])],
            )],
        };
        let flat = flatten(&patch);
        // file header, hunk header, one body line
        assert_eq!(flat.rows.len(), 3, "{:?}", flat.rows);
        assert!(matches!(flat.rows[0], DiffRow::FileHeader { .. }));
        assert!(matches!(flat.rows[1], DiffRow::HunkHeader { .. }));
        assert!(matches!(flat.rows[2], DiffRow::Line { .. }));
    }

    #[test]
    fn hunk_ordinals_are_global_across_files_not_per_file() {
        // The bug this guards: numbering hunks within each file makes
        // "hunk 1 of 40" appear twice, and roving focus lands on the wrong
        // one. GraphFocus indexes one flat sequence.
        let patch = ParsedPatch {
            files: vec![
                hunks_file("a.rs", vec![hunk(1, vec![line(LineKind::Added, "x")])]),
                hunks_file("b.rs", vec![hunk(9, vec![line(LineKind::Added, "y")])]),
            ],
        };
        let flat = flatten(&patch);
        let ordinals: Vec<usize> = flat
            .rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::HunkHeader { hunk_ordinal, .. } => Some(*hunk_ordinal),
                _ => None,
            })
            .collect();
        assert_eq!(ordinals, vec![0, 1]);
        assert_eq!(flat.hunk_count, 2);
    }

    #[test]
    fn hunk_count_counts_hunks_not_rows() {
        // The #350 warning in one assertion: these two numbers are different
        // coordinates and must never be conflated.
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(
                    1,
                    vec![
                        line(LineKind::Context, "ctx"),
                        line(LineKind::Added, "add"),
                        line(LineKind::Removed, "del"),
                    ],
                )],
            )],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.hunk_count, 1);
        assert_eq!(flat.rows.len(), 5); // file + hunk header + 3 lines
    }

    #[test]
    fn row_of_hunk_finds_the_header_for_scroll_into_view() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![
                    hunk(1, vec![line(LineKind::Added, "x")]),
                    hunk(9, vec![line(LineKind::Added, "y")]),
                ],
            )],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.row_of_hunk(0), Some(1));
        assert_eq!(flat.row_of_hunk(1), Some(3));
        assert_eq!(flat.row_of_hunk(2), None);
    }

    // ── all five FileDiff shapes ──

    #[test]
    fn a_binary_file_gets_a_note_and_no_navigable_hunk() {
        let patch = ParsedPatch {
            files: vec![FileDiff::Binary {
                old_path: Some("logo.png".into()),
                new_path: Some("logo.png".into()),
            }],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.hunk_count, 0);
        assert!(flat
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::Note { text, .. } if text.contains("binary"))));
        assert!(!flat.rows.iter().any(DiffRow::is_nav_stop));
    }

    #[test]
    fn a_mode_change_names_both_modes() {
        let patch = ParsedPatch {
            files: vec![FileDiff::ModeChangeOnly {
                path: "run.sh".into(),
                old_mode: "100644".into(),
                new_mode: "100755".into(),
            }],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.hunk_count, 0);
        let note = flat.rows.iter().find_map(|r| match r {
            DiffRow::Note { text, .. } => Some(text.clone()),
            _ => None,
        });
        let note = note.expect("a mode change must say what changed");
        assert!(note.contains("100644") && note.contains("100755"), "{note}");
    }

    #[test]
    fn a_pure_rename_is_distinguished_from_a_copy() {
        let mk = |is_copy| ParsedPatch {
            files: vec![FileDiff::Renamed {
                old_path: "old.rs".into(),
                new_path: "new.rs".into(),
                similarity: 98,
                is_copy,
            }],
        };
        let note = |p: &ParsedPatch| {
            flatten(p)
                .rows
                .iter()
                .find_map(|r| match r {
                    DiffRow::Note { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(note(&mk(false)).contains("renamed"));
        assert!(note(&mk(true)).contains("copied"));
    }

    #[test]
    fn a_combined_merge_diff_is_shown_but_never_navigable() {
        // The shipped behaviour this preserves: `@@@` headers render
        // header-coloured but inert, so they never look tappable.
        let patch = ParsedPatch {
            files: vec![FileDiff::Combined {
                path: "merged.rs".into(),
                raw: "@@@ -1,2 -1,2 +1,2 @@@\n  ctx\n++both\n".into(),
            }],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.hunk_count, 0, "a combined diff has no navigable hunk");
        assert!(!flat.rows.iter().any(DiffRow::is_nav_stop));
        // …but its content is still rendered, one row per line, so
        // virtualization still bounds it.
        assert!(flat.rows.len() >= 4, "{:?}", flat.rows);
    }

    #[test]
    fn a_new_file_and_a_deleted_file_are_titled_differently() {
        let new_file = ParsedPatch {
            files: vec![FileDiff::Hunks {
                old_path: None,
                new_path: Some("fresh.rs".into()),
                hunks: vec![],
            }],
        };
        let deleted = ParsedPatch {
            files: vec![FileDiff::Hunks {
                old_path: Some("gone.rs".into()),
                new_path: None,
                hunks: vec![],
            }],
        };
        let title = |p: &ParsedPatch| match &flatten(p).rows[0] {
            DiffRow::FileHeader { title, .. } => title.clone(),
            other => panic!("expected a file header, got {other:?}"),
        };
        assert!(title(&new_file).contains("new file"));
        assert!(title(&deleted).contains("deleted"));
    }

    #[test]
    fn an_empty_new_file_still_gets_a_header_row() {
        // `hunks: []` is a real shape — an empty file that was added.
        let patch = ParsedPatch {
            files: vec![FileDiff::Hunks {
                old_path: None,
                new_path: Some("empty.rs".into()),
                hunks: vec![],
            }],
        };
        let flat = flatten(&patch);
        assert_eq!(flat.rows.len(), 1);
        assert_eq!(flat.hunk_count, 0);
    }

    // ── content preservation ──

    #[test]
    fn no_newline_at_eof_survives_flattening() {
        // Losing this byte silently changes what a staged patch would apply.
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(
                    1,
                    vec![DiffLine {
                        kind: LineKind::Added,
                        text: "last".into(),
                        no_newline_at_eof: true,
                    }],
                )],
            )],
        };
        let flat = flatten(&patch);
        assert!(flat
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::Line { no_newline_at_eof, .. } if *no_newline_at_eof)));
    }

    #[test]
    fn a_body_line_that_looks_like_a_header_is_body_not_a_hunk() {
        // The exact hazard the raw-text walk had to guard by countdown: a
        // line whose text begins `@@` or `+++`. The parser already knows it
        // is body, so flattening cannot get this wrong — asserted so a
        // future rewrite cannot reintroduce the walk.
        let patch = parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n\
             --- a/a.rs\n\
             +++ b/a.rs\n\
             @@ -1,2 +1,3 @@\n\
             \x20ctx\n\
             +@@ -9,9 +9,9 @@ not a header\n\
             +++ also not a header\n",
        );
        let flat = flatten(&patch);
        assert_eq!(
            flat.hunk_count, 1,
            "only the real @@ header is a hunk: {:?}",
            flat.rows
        );
    }

    #[test]
    fn the_label_leads_with_the_file_and_position() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "src/main.rs",
                vec![hunk(
                    42,
                    vec![line(LineKind::Added, "a"), line(LineKind::Removed, "b")],
                )],
            )],
        };
        let flat = flatten(&patch);
        let label = flat
            .rows
            .iter()
            .find_map(|r| match r {
                DiffRow::HunkHeader { label, .. } => Some(label.clone()),
                _ => None,
            })
            .unwrap();
        assert!(label.starts_with("src/main.rs"), "{label}");
        assert!(label.contains("42"), "{label}");
        assert!(label.contains("1 added"), "{label}");
        assert!(label.contains("1 removed"), "{label}");
    }

    #[test]
    fn the_header_text_reconstructs_the_line_numbers_a_reader_checks() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(12, vec![line(LineKind::Context, "c")])],
            )],
        };
        let flat = flatten(&patch);
        let text = flat
            .rows
            .iter()
            .find_map(|r| match r {
                DiffRow::HunkHeader { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(text.starts_with("@@ -12,"), "{text}");
        assert!(text.contains("+12,"), "{text}");
    }
    // ── heights: the rows twin of line_heights ──

    #[test]
    fn unwrapped_rows_are_all_one_line_tall() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Added, "x")])],
            )],
        };
        let flat = flatten(&patch);
        let heights = row_heights(&flat.rows, 20.0, LineWrap::Never);
        assert_eq!(heights, vec![20.0; flat.rows.len()]);
    }

    #[test]
    fn a_body_line_is_measured_including_its_marker_column() {
        // The parser strips the leading '+'/'-'/' ' from `text`, but that
        // character still occupies a column on screen. Measuring without it
        // under-counts every added and removed line by one, and a patch
        // sitting exactly on a wrap boundary then reports a height one row
        // short of what it draws.
        let nine = "123456789"; // 9 chars of text, 10 with the marker
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Added, nine)])],
            )],
        };
        let flat = flatten(&patch);
        let body = flat
            .rows
            .iter()
            .find(|r| matches!(r, DiffRow::Line { .. }))
            .unwrap();
        assert_eq!(body.display_text(), "+123456789");
        let heights = row_heights(
            std::slice::from_ref(body),
            10.0,
            LineWrap::Wrapped { columns: 10 },
        );
        assert_eq!(heights, vec![10.0], "10 chars in 10 columns is one row");

        let heights = row_heights(
            std::slice::from_ref(body),
            10.0,
            LineWrap::Wrapped { columns: 9 },
        );
        assert_eq!(
            heights,
            vec![20.0],
            "the marker pushes it past 9 columns onto a second row"
        );
    }

    #[test]
    fn a_zero_width_container_does_not_divide_by_zero() {
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Added, "x")])],
            )],
        };
        let flat = flatten(&patch);
        let heights = row_heights(&flat.rows, 20.0, LineWrap::Wrapped { columns: 0 });
        assert_eq!(heights, vec![20.0; flat.rows.len()]);
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        // A non-ASCII patch measured by bytes over-estimates its height and
        // leaves a gap at the bottom of the scroll range.
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Context, "ééééééééé")])],
            )],
        };
        let flat = flatten(&patch);
        let body = flat
            .rows
            .iter()
            .find(|r| matches!(r, DiffRow::Line { .. }))
            .unwrap();
        // 9 chars + 1 marker = 10 columns exactly; by bytes it would be 19.
        let heights = row_heights(
            std::slice::from_ref(body),
            10.0,
            LineWrap::Wrapped { columns: 10 },
        );
        assert_eq!(heights, vec![10.0]);
    }

    #[test]
    fn there_is_exactly_one_height_per_row() {
        // CumulativeHeights indexes rows by position; a length mismatch
        // silently offsets every window boundary after it.
        let patch = ParsedPatch {
            files: vec![
                hunks_file("a.rs", vec![hunk(1, vec![line(LineKind::Added, "x")])]),
                FileDiff::Binary {
                    old_path: Some("b.png".into()),
                    new_path: Some("b.png".into()),
                },
            ],
        };
        let flat = flatten(&patch);
        assert_eq!(
            row_heights(&flat.rows, 20.0, LineWrap::Never).len(),
            flat.rows.len()
        );
    }
}
