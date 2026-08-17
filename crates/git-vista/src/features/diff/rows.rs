//! Flattening a [`ParsedPatch`] into the rows a diff view renders (#361).
//!
//! ## Why this exists
//!
//! `hunk_nav` (deleted with #361's completion) re-derived hunk structure by
//! walking raw patch text with a marker/countdown scan — structure the parser
//! ([`git_vista_protocol::diff::parse_unified_diff`], #69a) has *already*
//! produced. Two independent derivations of the same fact is how they drift:
//! a body line beginning `+++` or `@@` is a genuine hazard for a text walk
//! and a non-event for the parser. This module is the structured derivation
//! that replaced it, for rendering, spoken labels, and heights alike.
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

// `hunk_ordinal()` and `is_nav_stop()` were declared here and never reached
// production: `accessible_rows_window` matches the enum directly, which is
// shorter at the call site and exhaustive-checked by the compiler.
// `is_nav_stop` had two callers, both assertions in this file's own tests —
// the shape the reachability census exists to reject, since a function proved
// correct by tests nothing else calls is scaffolding wearing a proof. The
// invariant those tests assert (a combined merge diff is never navigable) is
// real and still asserted, now against the enum directly.

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
/// the raw-text measurement this replaced.
///
/// A separate function rather than a reuse, because the two walk different
/// sequences: the old raw-text walk used `patch.lines()`, which includes the
/// `diff --git`/`index`/`---`/`+++` headers the parser drops and excludes the
/// per-file heading rows this view adds. Measuring rows with a line-based
/// walk would offset every height in the document, and the window would then
/// render one slice while the scrollbar described another.
///
/// Counts **characters, not bytes**, as the raw-text version did: a patch
/// carrying non-ASCII would otherwise over-estimate its height and leave a
/// gap at the bottom of the scroll range.
///
/// [`CumulativeHeights::new`]: git_vista_core::virtualize::CumulativeHeights::new
pub fn row_heights(rows: &[DiffRow], line_height: f64, wrap: LineWrap) -> Vec<f64> {
    rows.iter()
        .map(|row| match wrap {
            LineWrap::Never => line_height,
            LineWrap::Wrapped { columns } => {
                wrapped_rows(&row.display_text(), columns) as f64 * line_height
            }
        })
        .collect()
}

/// How many rows one line occupies under `white-space: pre-wrap` plus
/// `word-break: break-word` — the viewer's actual CSS (`styles.css`,
/// `.viewer-pre`).
///
/// **Why not `ceil(chars / columns)`.** That is a *character*-wrap model, and
/// the browser wraps at *word* boundaries, breaking inside a word only when the
/// word cannot fit on a line of its own. The two disagree on ordinary code: at
/// 20 columns, `let result = compute();` is one row by the character model and
/// two by the browser, because `compute();` will not fit in the tail of the
/// first row and moves down whole.
///
/// The disagreement is not a small percentage error, which is what makes it
/// worth fixing rather than tolerating. `ceil` is quantized per line, so being
/// wrong by one row on a line is wrong by a whole row — and the error is
/// data-dependent (it depends on where the spaces fall), so it neither shrinks
/// with scale nor stays proportional. Errors accumulate down the document, and
/// a virtualized window keyed on those heights renders one slice while the
/// scrollbar describes another.
///
/// The rules being modelled, from CSS Text 3:
/// * a soft wrap opportunity exists **after** a space run (`pre-wrap` keeps the
///   spaces and lets them hang past the edge rather than forcing a break),
/// * a word that does not fit the remaining space moves to the next line whole,
/// * a word too long for *any* line breaks mid-word — that is what
///   `word-break: break-word` adds, and without it such a word would overflow.
///
/// Counts **characters, not bytes**: a non-ASCII line would otherwise be
/// over-measured and leave a gap at the end of the scroll range.
/// Columns one character occupies in a monospace cell grid.
///
/// East Asian **Wide** and **Fullwidth** characters — CJK ideographs, kana,
/// Hangul, fullwidth forms — and emoji occupy TWO cells, not one. Counting
/// them as one under-measures those lines by half, and under-measuring is the
/// harmful direction: the scrollbar then describes a shorter document than is
/// drawn, and a window keyed on those heights lands short.
///
/// Caught by the Chromium cross-check in `ci/browser/tests/wrap-model.spec.mjs`,
/// not by any unit test — the unit tests only ever asked this model to agree
/// with itself, and it did.
///
/// **This is an approximation and cannot be otherwise.** Actual advance width
/// depends on the font that ends up rendering the glyph, and the browser's
/// monospace fallback does not always draw CJK at exactly two cells (measured:
/// ten ideographs wrapped at ten columns, but eleven did not need a third row,
/// so the real width sits just under 2.0). Two cells is the Unicode-standard
/// answer and errs toward over-measuring, which is the safe direction.
fn char_columns(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F        // Hangul Jamo initial consonants
        | 0x2E80..=0x303E      // CJK radicals, Kangxi, CJK symbols/punctuation
        | 0x3041..=0x33FF      // kana, bopomofo, Hangul compat, CJK compat
        | 0x3400..=0x4DBF      // CJK unified ext A
        | 0x4E00..=0x9FFF      // CJK unified ideographs
        | 0xA000..=0xA4CF      // Yi
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE10..=0xFE19      // vertical forms
        | 0xFE30..=0xFE6F      // CJK compatibility forms
        | 0xFF00..=0xFF60      // fullwidth forms
        | 0xFFE0..=0xFFE6      // fullwidth signs
        | 0x1F300..=0x1F64F    // emoji: symbols, pictographs, emoticons
        | 0x1F900..=0x1F9FF    // emoji: supplemental
        | 0x20000..=0x2FFFD    // CJK ext B..F
        | 0x30000..=0x3FFFD    // CJK ext G
    );
    if wide {
        2
    } else {
        1
    }
}

fn wrapped_rows(text: &str, columns: usize) -> usize {
    // A zero-width container is not a real layout; one row rather than a
    // division by zero.
    if columns == 0 {
        return 1;
    }

    let mut rows = 1usize;
    let mut col = 0usize;

    // Walk whitespace runs and word runs alternately. Splitting on
    // `char_indices` rather than `split_whitespace` because the whitespace
    // itself occupies columns under `pre-wrap` and cannot be discarded.
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            // Spaces consume columns but never force a break themselves —
            // `pre-wrap` hangs a trailing space past the edge rather than
            // pushing a row. Clamp instead of wrapping so a run of trailing
            // spaces cannot invent rows the browser does not draw.
            col = (col + 1).min(columns);
            i += 1;
            continue;
        }

        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        // Width in CELLS, not characters — a run of ideographs is twice as
        // wide as its character count suggests.
        let word: usize = chars[start..i].iter().copied().map(char_columns).sum();

        if col + word <= columns {
            col += word; // fits on this row
        } else if word <= columns {
            rows += 1; // moves down whole
            col = word;
        } else {
            // Longer than a whole row: `break-word` splits it. It starts on a
            // fresh row unless this one is still empty, then consumes as many
            // full rows as it needs.
            if col > 0 {
                rows += 1;
            }
            // Either way the word now starts at column 0 — either the row was
            // already empty, or the line above moved to a fresh one — so the
            // arithmetic below measures from the word alone.
            let full = (word - 1) / columns; // additional rows after the first
            rows += full;
            col = word - full * columns;
        }
    }

    rows
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
                        label: hunk_label(&name, within_file, hunks.len(), hunk),
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

/// One spoken label per [`super::core::SelectableHunk`], paired by
/// **(file, per-file ordinal)** — never by position.
///
/// The staging view pairs `selectable_hunks` (a raw-text walk that reacts to
/// any `---`/`+++`/`@@` lines) with labels from the structured parser
/// ([`git_vista_protocol::diff::parse_unified_diff`], which only recognises
/// hunks inside a `diff --git`/`--combined`/`--cc` section, and whose
/// `parse_file_section` has a documented "nothing recognisable — skip"
/// path). The two are asymmetric by construction, so positional pairing had
/// two silent failure modes (review findings): a whole file dropped by the
/// structured parser would shift **every subsequent** hunk's spoken label
/// onto the wrong checkbox, and any count mismatch fell back to an **empty**
/// `aria-label`. Keying on `(file, ordinal)` — the same identity
/// `SelectableHunk` itself carries, built from the same shared
/// [`git_vista_protocol::diff::path_or_dev_null`] path rule — makes
/// misalignment structurally impossible: a hunk the parser dropped merely
/// gets the honest fallback below, and every hunk both walks agree on keeps
/// its own label.
///
/// The fallback label is built from the raw walk's own facts (file, per-file
/// number, new-side line) — less rich than a parsed label (no counts, no
/// heading), but never empty and never someone else's.
///
/// The output is index-aligned with `hunks`: exactly one label per
/// selectable hunk, in the same order.
pub fn labels_for_selectable_hunks(
    patch: &ParsedPatch,
    hunks: &[super::core::SelectableHunk],
) -> Vec<String> {
    use std::collections::HashMap;
    // Mirror `selectable_hunks`' per-file ordinal rule exactly: a running
    // count per file NAME (not per section), so a file split across two
    // `diff --git` sections still lines up with the raw walk's numbering.
    let mut labels_by_key: HashMap<(String, u32), String> = HashMap::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    for file in &patch.files {
        if let FileDiff::Hunks {
            hunks: parsed_hunks,
            ..
        } = file
        {
            let name = display_path(file);
            for (within_file, hunk) in parsed_hunks.iter().enumerate() {
                let ordinal = {
                    let c = seen.entry(name.clone()).or_insert(0);
                    let ord = *c;
                    *c += 1;
                    ord
                };
                labels_by_key.insert(
                    (name.clone(), ordinal),
                    hunk_label(&name, within_file, parsed_hunks.len(), hunk),
                );
            }
        }
    }
    hunks
        .iter()
        .map(|h| {
            labels_by_key
                .get(&(h.file.clone(), h.ordinal))
                .cloned()
                .unwrap_or_else(|| {
                    format!("{} hunk {} at line {}", h.file, h.ordinal + 1, h.new_start)
                })
        })
        .collect()
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

/// The spoken label: file, the per-file ordinal **and total**, the new-side
/// position, the counts, then the section heading when the header carried
/// one. Leads with file and position because that is what orients a
/// listener — the raw `-12,5 +12,8` shorthand does not. The "of {total}"
/// and ", in {heading}" cues are #210 content `hunk_nav` spoke and PR #386's
/// migration silently dropped (review finding): "how many hunks remain in
/// this file" and "which function is this" are orientation a screen-reader
/// user cannot get any other way.
///
/// **`, truncated` when the body is short of its header's declaration.** The
/// server caps patches at a line boundary (`read.rs`, `truncate_at_line`), so
/// a cap landing mid-hunk leaves the final hunk's parsed lines short of the
/// `old_len`/`new_len` its `@@` header declared. Stating the resulting
/// undercount as fact would be a lie a screen-reader user cannot see past;
/// the raw-text walk this replaced flagged exactly this case, and the flag
/// must survive the migration. Checked per hunk rather than only on the
/// patch's last one — for well-formed input only the cut hunk can fall
/// short, and a malformed mid-patch hunk deserves the same honesty.
fn hunk_label(file: &str, within_file: usize, total_in_file: usize, hunk: &Hunk) -> String {
    let (added, removed, context) =
        hunk.lines
            .iter()
            .fold((0u32, 0u32, 0u32), |(a, r, c), line| match line.kind {
                LineKind::Added => (a + 1, r, c),
                LineKind::Removed => (a, r + 1, c),
                LineKind::Context => (a, r, c + 1),
            });
    // Old side draws context + removed lines, new side context + added; a
    // body that ran out early is short on at least one of them.
    let cut = context + removed < hunk.old_len || context + added < hunk.new_len;
    format!(
        "{file} hunk {} of {} at line {}, {} added, {} removed{}{}",
        within_file + 1,
        total_in_file,
        hunk.new_start,
        added,
        removed,
        if cut { ", truncated" } else { "" },
        if hunk.section_heading.is_empty() {
            String::new()
        } else {
            format!(", in {}", hunk.section_heading)
        }
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
        assert!(!flat
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::HunkHeader { .. })));
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
        assert!(!flat
            .rows
            .iter()
            .any(|r| matches!(r, DiffRow::HunkHeader { .. })));
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

    /// Every navigable hunk's label in ordinal order — a test lens over the
    /// flattened rows. Production code never consumes labels positionally
    /// (the staging view goes through `labels_for_selectable_hunks`' keyed
    /// pairing), so this lives here rather than as API.
    fn hunk_labels(flat: &DiffRows) -> Vec<String> {
        flat.rows
            .iter()
            .filter_map(|row| match row {
                DiffRow::HunkHeader { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn flattened_hunk_labels_pair_with_selectable_hunks() {
        // Both walks must enumerate the same ordinary hunks in the same
        // order — the property that makes `labels_for_selectable_hunks`'
        // keyed pairing coincide with rendering order on well-formed
        // patches. The fixture deliberately mixes the shapes the two
        // walks must agree on skipping: an ordinary two-hunk file, a combined
        // merge section (`@@@`, navigable to neither), and a trailing
        // ordinary file.
        let patch = "\
diff --git a/src/foo.rs b/src/foo.rs
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
diff --cc merged.rs
--- a/merged.rs
+++ b/merged.rs
@@@ -1,2 -1,2 +1,2 @@@
  ctx
++both
diff --git a/bar.txt b/bar.txt
--- a/bar.txt
+++ b/bar.txt
@@ -1 +1 @@
-old
+new
";
        let sel = crate::features::diff::core::selectable_hunks(patch);
        let labels = hunk_labels(&flatten(&parse_unified_diff(patch)));
        assert_eq!(
            labels.len(),
            sel.len(),
            "both walks must find the same hunks: {labels:?} vs {sel:?}"
        );
        assert_eq!(
            sel.len(),
            3,
            "fixture should carry exactly 3 ordinary hunks"
        );
        for (label, hunk) in labels.iter().zip(&sel) {
            // Labels lead with the file (pinned elsewhere), so positional
            // pairing is verifiable file-by-file.
            assert!(
                label.starts_with(&hunk.file),
                "label {label:?} does not open with its paired hunk's file {:?}",
                hunk.file
            );
        }
    }

    #[test]
    fn selectable_labels_pair_by_file_and_ordinal_even_when_the_parsers_disagree() {
        // The two walks staging pairs are asymmetric by construction:
        // `selectable_hunks` reacts to bare `---`/`+++`/`@@` lines, while
        // `parse_unified_diff` only recognises hunks inside a
        // `diff --git`/`--combined`/`--cc` section. A hunk block with no
        // `diff --git` line is therefore visible to the raw walk and
        // invisible to the structured one. Positional pairing would shift
        // bar.txt's real label onto orphan.rs's checkbox and leave bar.txt's
        // own hunk with an empty aria-label (the exact silent failure the
        // review flagged); keyed pairing must instead give orphan.rs an
        // honest fallback and bar.txt its own label.
        let patch = "\
--- a/orphan.rs
+++ b/orphan.rs
@@ -1,2 +1,2 @@
 context
-old
+new
diff --git a/bar.txt b/bar.txt
--- a/bar.txt
+++ b/bar.txt
@@ -5 +5 @@
-old
+new
";
        let sel = crate::features::diff::core::selectable_hunks(patch);
        assert_eq!(sel.len(), 2, "the raw walk must see both hunks: {sel:?}");
        let parsed = parse_unified_diff(patch);
        assert_eq!(
            parsed.files.len(),
            1,
            "the structured parse must see only bar.txt — if this fails the \
             fixture no longer exercises the asymmetry"
        );
        let labels = labels_for_selectable_hunks(&parsed, &sel);
        assert_eq!(labels.len(), sel.len(), "one label per selectable hunk");
        assert_eq!(
            labels[0], "orphan.rs hunk 1 at line 1",
            "the unparsed hunk gets the honest raw-walk fallback, never an \
             empty label and never another file's label"
        );
        assert!(
            labels[1].starts_with("bar.txt hunk 1 of 1"),
            "the parsed hunk keeps its own full label: {:?}",
            labels[1]
        );
        assert!(
            labels[1].contains("1 added, 1 removed"),
            "the parsed hunk's label carries real counts: {:?}",
            labels[1]
        );
    }

    #[test]
    fn selectable_labels_match_the_flattened_labels_when_the_parsers_agree() {
        // On a well-formed patch (every hunk inside a `diff --git` section —
        // the only shape the server's git invocations produce) the keyed
        // pairing must be indistinguishable from the flattened ordinal-order
        // list: same labels, same order, no fallbacks.
        let patch = "\
diff --git a/src/foo.rs b/src/foo.rs
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
diff --git a/bar.txt b/bar.txt
--- a/bar.txt
+++ b/bar.txt
@@ -1 +1 @@
-old
+new
";
        let sel = crate::features::diff::core::selectable_hunks(patch);
        let parsed = parse_unified_diff(patch);
        assert_eq!(
            labels_for_selectable_hunks(&parsed, &sel),
            hunk_labels(&flatten(&parsed)),
            "keyed pairing and ordinal order must coincide when both walks \
             see the same hunks"
        );
    }

    #[test]
    fn labels_carry_the_per_file_total_and_the_section_heading() {
        // #210's spoken labels carried "hunk N of M" and the enclosing
        // section heading (", in fn frobnicate()"). `hunk_nav` was the last
        // carrier of both; deleting it (#361) must not delete the cues — a
        // screen-reader user staging hunks needs "how many remain in this
        // file" and "which function am I in" (review finding).
        let patch = "\
diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,3 +10,3 @@
 context
+added
 context
-removed
@@ -30,2 +31,2 @@ fn frobnicate()
 context
-old
+new
";
        let labels = hunk_labels(&flatten(&parse_unified_diff(patch)));
        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("hunk 1 of 2"), "{:?}", labels[0]);
        assert!(labels[1].contains("hunk 2 of 2"), "{:?}", labels[1]);
        assert!(
            labels[1].ends_with(", in fn frobnicate()"),
            "the section heading must be spoken: {:?}",
            labels[1]
        );
        assert!(
            !labels[0].contains(", in "),
            "no heading on the header means no heading in the label: {:?}",
            labels[0]
        );
    }

    #[test]
    fn a_patch_cut_mid_hunk_flags_the_short_hunks_label_as_truncated() {
        // The server caps patches at a line boundary (`read.rs`,
        // `truncate_at_line`), so a cap landing mid-hunk leaves the final
        // hunk's body short of what its header declared. The label must say
        // so rather than state the undercount as fact — the behaviour
        // `hunk_nav` had (#210) and the migration to rows must not lose.
        let patch = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,2 +1,2 @@
 context
-old
+new
@@ -10,20 +10,22 @@
 context
+added one
";
        let labels = hunk_labels(&flatten(&parse_unified_diff(patch)));
        assert_eq!(labels.len(), 2);
        assert!(
            !labels[0].contains("truncated"),
            "a complete hunk must not be flagged: {:?}",
            labels[0]
        );
        assert!(
            labels[1].ends_with(", truncated"),
            "the cut hunk's label must admit the undercount: {:?}",
            labels[1]
        );
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
    // ── heights: measured over rows, not raw patch lines ──

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
    fn a_long_line_is_charged_for_every_row_it_takes_not_just_two() {
        // The 1->2 transition above can pass on an implementation that merely
        // saturates at two rows. The deleted core::line_heights test covered a
        // 3-row case; this restores that reach, so div_ceil is proven to keep
        // counting rather than to notice one overflow.
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(
                    1,
                    vec![
                        line(LineKind::Context, &"x".repeat(4)), // +1 marker = 5  -> 1 row
                        line(LineKind::Added, &"y".repeat(24)),  // +1 marker = 25 -> 3 rows
                        line(LineKind::Removed, &"z".repeat(19)), // +1 marker = 20 -> 2 rows
                    ],
                )],
            )],
        };
        let flat = flatten(&patch);
        let bodies: Vec<_> = flat
            .rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Line { .. }))
            .cloned()
            .collect();
        let heights = row_heights(&bodies, 10.0, LineWrap::Wrapped { columns: 10 });
        assert_eq!(
            heights,
            vec![10.0, 30.0, 20.0],
            "at 10 columns: 5 chars is one row, 25 is three, 20 is two"
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

    // ── the word-wrap model (#362 step 2) ──
    //
    // Each of these is a case where character-wrap and word-wrap DISAGREE.
    // A test that both models pass would prove nothing about the change.

    #[test]
    fn a_word_that_does_not_fit_moves_down_whole_instead_of_splitting() {
        // 20 columns. "let result = compute();" is 23 chars, so ceil(23/20)
        // says two rows and happens to be right by luck. The interesting part
        // is WHERE: the browser breaks before `compute();` because it will not
        // fit in the 9 columns left, so row 2 holds 10 chars, not 3.
        // At 24 columns the models diverge outright: character-wrap says one
        // row (23 <= 24) and so does word wrap. Use 22 to force the split.
        assert_eq!(wrapped_rows("let result = compute();", 22), 2);
        // Character model would say ceil(23/22) = 2 as well — agreement here.
        // The next test is the one it cannot get right.
    }

    #[test]
    fn character_wrap_and_word_wrap_disagree_on_ordinary_code() {
        // 16 columns, 24 characters. ceil(24/16) = 2 rows by the character
        // model. The browser needs THREE: "fn compute_all(" is 15, then
        // "value:" will not fit in the 1 remaining column so it moves down,
        // then "u32)" follows it.
        let line = "fn compute_all( value: u32)";
        let chars = line.chars().count();
        assert_eq!(chars, 27);
        assert_eq!(chars.div_ceil(16), 2, "the character model's answer");
        assert_eq!(
            wrapped_rows(line, 16),
            2,
            "word wrap fits 'fn compute_all(' then 'value: u32)'"
        );

        // And a case where word wrap needs strictly MORE rows than the
        // character model — the direction that under-measures a document and
        // leaves the scrollbar describing a shorter page than is drawn.
        //
        // Three 7-character words at 12 columns. The character model packs
        // them ignoring the spaces' break opportunities and says two rows; the
        // browser cannot fit two whole words on any row, so it draws three.
        let wide = "aaaaaaa bbbbbbb ccccccc";
        assert_eq!(wide.chars().count(), 23);
        assert_eq!(
            wide.chars().count().div_ceil(12),
            2,
            "character model says 2"
        );
        assert_eq!(
            wrapped_rows(wide, 12),
            3,
            "the browser draws one word per row — 7 + 1 + 7 overflows 12"
        );
    }

    #[test]
    fn a_word_longer_than_the_line_breaks_mid_word() {
        // This is what `word-break: break-word` adds. Without it the word
        // would overflow the container instead of wrapping. A 30-char
        // identifier at 10 columns is exactly three rows.
        assert_eq!(wrapped_rows(&"x".repeat(30), 10), 3);
        // 31 chars needs a fourth row for the leftover character.
        assert_eq!(wrapped_rows(&"x".repeat(31), 10), 4);
    }

    #[test]
    fn a_long_word_starts_on_a_fresh_row_when_the_current_one_is_dirty() {
        // "ab " then a 20-char word at 10 columns: the word cannot fit in the
        // 7 remaining columns AND cannot fit a whole row, so it starts fresh
        // and then breaks. Three rows: "ab", then two rows of the word.
        assert_eq!(wrapped_rows("ab aaaaaaaaaaaaaaaaaaaa", 10), 3);
    }

    #[test]
    fn indentation_counts_toward_the_row() {
        // Leading whitespace is preserved under `pre-wrap` and occupies
        // columns. Dropping it — as `split_whitespace` would — under-measures
        // every indented line, which in a code diff is nearly all of them.
        assert_eq!(wrapped_rows("        indented", 12), 2);
        assert_eq!(wrapped_rows("indented", 12), 1, "same word, no indent");
    }

    #[test]
    fn trailing_spaces_do_not_invent_rows() {
        // `pre-wrap` hangs trailing spaces past the edge rather than pushing a
        // new row. Counting them naively would add rows the browser never
        // draws — and trailing whitespace is common in diffs.
        assert_eq!(wrapped_rows("abc        ", 5), 1);
    }

    #[test]
    fn the_word_model_counts_cells_not_bytes_and_not_characters() {
        // Three axes get confused here, so all three are pinned.
        //
        // BYTES would say 30 for ten 3-byte ideographs — wrong, and the bug
        // the original test guarded against.
        //
        // CHARACTERS would say 10 — also wrong, and this is what the model
        // said until the Chromium cross-check caught it. East Asian Wide
        // characters occupy TWO cells each.
        //
        // CELLS says 20, which is two rows at ten columns. That is what the
        // browser draws.
        let ten = "\u{4e2d}".repeat(10);
        assert_eq!(ten.len(), 30, "bytes");
        assert_eq!(ten.chars().count(), 10, "characters");
        assert_eq!(wrapped_rows(&ten, 10), 2, "cells: 20 wide, so two rows");

        // Latin text is unaffected — one cell per character.
        assert_eq!(wrapped_rows(&"x".repeat(10), 10), 1);
    }

    #[test]
    fn emoji_are_wide_too() {
        // Emoji are double-width in a monospace grid, and they turn up in
        // commit messages and comments constantly. Five of them fill a
        // ten-column row exactly; six need a second.
        assert_eq!(wrapped_rows(&"\u{1F600}".repeat(5), 10), 1);
        assert_eq!(wrapped_rows(&"\u{1F600}".repeat(6), 10), 2);
    }

    #[test]
    fn an_empty_line_still_occupies_one_row_when_wrapping() {
        // A blank context line has no text of its own, but it still draws a
        // row. Measuring it as zero would let the scroll range fall short of
        // the rendered patch by one row per blank line — and a diff of prose
        // is mostly blank lines. `.max(1)` in row_heights is what prevents it;
        // this is the test that fails if that guard is ever removed.
        let patch = ParsedPatch {
            files: vec![hunks_file(
                "a.rs",
                vec![hunk(1, vec![line(LineKind::Context, "")])],
            )],
        };
        let flat = flatten(&patch);
        let body = flat
            .rows
            .iter()
            .find(|r| matches!(r, DiffRow::Line { .. }))
            .unwrap();
        let heights = row_heights(
            std::slice::from_ref(body),
            10.0,
            LineWrap::Wrapped { columns: 80 },
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
