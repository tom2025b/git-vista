//! Hunk-level diff parsing into a structured DTO (M2.16, #69a).
//!
//! [`parse_unified_diff`] is a **pure function**: unified-diff text (what
//! `git show --patch --no-color` already produces, and what
//! `git_vista_core::diff::CommitDiff.patch` already carries as a raw string
//! today) in, [`ParsedPatch`] out. No git process spawn, no I/O — the same
//! posture `status::parse_porcelain_v2_z` (#68b) already established for the
//! porcelain-v2 format.
//!
//! **Additive, not a replacement.** `CommitDiff.patch` is untouched; this is a
//! new, separate structured representation nothing consumes yet (server
//! wiring is a later #69 sub-task). Same shape 68a took relative to the old
//! `git_vista_core::status::RepoStatus`.
//!
//! ## The six file-shapes a real unified diff produces
//!
//! Verified against real `git show --patch --no-color` output on a scratch
//! repository before designing anything — not assumed from memory of the
//! format:
//!
//! 1. **Ordinary edit** ([`FileDiff::Hunks`]) — `--- a/path` / `+++ b/path` /
//!    one or more `@@ -a,b +c,d @@` hunks with context/added/removed lines.
//!    Also covers a brand-new **empty** file (`new file mode` + `index
//!    0000000..<hash>`, no `--- `/`+++ `/`@@ ` lines at all — verified
//!    directly) as `hunks: vec![]`, since it's structurally an ordinary add
//!    with nothing to diff, not a distinct condition.
//! 2. **`\ No newline at end of file`** — git's literal marker line,
//!    immediately following the line it describes. Modelled as
//!    [`DiffLine::no_newline_at_eof`], a property of that line, not a
//!    separate line-kind — a sentinel-line approach would read wrong to any
//!    consumer that doesn't already know to filter it out.
//! 3. **Mode-change-only** ([`FileDiff::ModeChangeOnly`]) — `old mode
//!    NNNNNN` / `new mode NNNNNN`, **no** `--- `/`+++ `/`@@ ` lines at all
//!    (verified: a content-identical `chmod +x` produces exactly this).
//! 4. **Binary** ([`FileDiff::Binary`]) — `Binary files a/path and b/path
//!    differ` instead of any hunks.
//! 5. **Pure rename/copy, no content change** ([`FileDiff::Renamed`]) —
//!    `similarity index NN%` / `rename from X` / `rename to Y` (or `copy
//!    from`/`copy to`), again with **no** `--- `/`+++ `/`@@ ` lines at all
//!    (verified: a content-identical `git mv` produces exactly this — easy to
//!    conflate with shape 3 since both have zero hunk lines, but they mean
//!    different things and carry different fields). A rename **with**
//!    further content edits carries `--- `/`+++ `/`@@ ` lines on top of the
//!    rename headers — that's shape 1 with `old_path != Some(new path)`, not
//!    this shape.
//! 6. **Combined (merge) diff** ([`FileDiff::Combined`]) — `diff --combined
//!    <path>` (not `diff --git`), `@@@ -a,b -c,d +e,f @@@`-style headers,
//!    content lines carrying one marker character per parent instead of one
//!    (verified against a real 2-parent merge via `git show -c`).
//!    **Deliberately not structurally parsed line-by-line** — correctly
//!    modelling N-parent line markers is a materially bigger, separate
//!    problem than this task's scope, and the risk named in the task brief
//!    (a combined diff "is easy to forget and yields nonsense rather than an
//!    error") sets the bar at *detect and preserve*, not *fully structurally
//!    parse*. The file's raw section text is kept verbatim so nothing is
//!    lost, and a future task can build real per-parent parsing on top
//!    without this one having silently mangled the input in the meantime.

use serde::{Deserialize, Serialize};

/// One side of an ordinary (non-combined) hunk line's change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line inside a [`Hunk`]. `no_newline_at_eof` is true when git's literal
/// `\ No newline at end of file` marker immediately followed this line in the
/// source text — see the module doc for why that's a flag here, not a
/// separate line variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line's text, without its leading `' '`/`'+'`/`'-'` marker
    /// character.
    pub text: String,
    pub no_newline_at_eof: bool,
}

/// One `@@ -old_start,old_len +new_start,new_len @@ section_heading` hunk.
/// `old_len`/`new_len` are always concrete here even though git omits the
/// `,1` suffix when a range's length is exactly 1 — [`parse_hunk_header`]
/// defaults an omitted length to `1` on the way in, so a consumer never has
/// to re-derive that git-specific shorthand itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Text after the second `@@` on the header line — git's context
    /// heuristic sometimes finds an enclosing function/class name here.
    /// Empty when git printed none.
    pub section_heading: String,
    pub lines: Vec<DiffLine>,
}

/// One file's diff — the closed vocabulary of every shape a real unified
/// diff produces (see the module doc for how each was verified). Internally
/// tagged on `"shape"`, `snake_case` variant names, matching
/// [`crate::StatusEntry`]'s wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum FileDiff {
    /// An ordinary edit, a rename/copy *with* further content changes, a new
    /// file (including an empty one, `hunks: []`), or a deleted file.
    /// `old_path`/`new_path` are `None` exactly when git printed `/dev/null`
    /// on that side (a new or deleted file respectively).
    Hunks {
        old_path: Option<String>,
        new_path: Option<String>,
        hunks: Vec<Hunk>,
    },
    /// `old mode`/`new mode` only — a permission change with no content diff.
    ModeChangeOnly {
        path: String,
        old_mode: String,
        new_mode: String,
    },
    /// `Binary files a/path and b/path differ`. `None` on the side that was
    /// `/dev/null` (a new or deleted binary file).
    Binary {
        old_path: Option<String>,
        new_path: Option<String>,
    },
    /// A rename or copy with **no** content change — `similarity index`,
    /// `rename from`/`rename to` (or `copy from`/`copy to`), no hunks at
    /// all. `is_copy` distinguishes the two since git's own header text
    /// does (`rename from`/`to` vs. `copy from`/`to`); everything else
    /// about the shape is identical.
    Renamed {
        old_path: String,
        new_path: String,
        similarity: u8,
        is_copy: bool,
    },
    /// A combined (merge) diff — see the module doc for why this is
    /// deliberately opaque rather than structurally parsed.
    Combined { path: String, raw: String },
}

/// The parsed diff — the payload a future structured diff view (#69e) will
/// eventually render. `files` preserves the order they appeared in the
/// source text (git's own file order, generally alphabetical but not
/// guaranteed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedPatch {
    pub files: Vec<FileDiff>,
}

/// Parse the complete unified-diff text of `git show --patch --no-color` (or
/// equivalent) into a [`ParsedPatch`].
///
/// Unrecognised lines between file sections, and a file section this parser
/// can't make sense of, are skipped rather than erroring — the same
/// undercount-not-failure posture `status::parse_porcelain_v2_z` and
/// `git_vista_core::status::parse_porcelain_v2` both already take: the
/// format is git's own and versioned, so something unrecognised is more
/// likely a future git addition than malformed input, and the worst outcome
/// of skipping is a missing file in the result, never a failed parse.
pub fn parse_unified_diff(text: &str) -> ParsedPatch {
    let mut files = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("diff --combined ") {
            let path = lines[i]["diff --combined ".len()..].to_string();
            let start = i;
            i += 1;
            while i < lines.len()
                && !lines[i].starts_with("diff --git ")
                && !lines[i].starts_with("diff --combined ")
            {
                i += 1;
            }
            files.push(FileDiff::Combined {
                path,
                raw: lines[start..i].join("\n"),
            });
        } else if lines[i].starts_with("diff --git ") {
            let start = i;
            i += 1;
            while i < lines.len()
                && !lines[i].starts_with("diff --git ")
                && !lines[i].starts_with("diff --combined ")
            {
                i += 1;
            }
            if let Some(file) = parse_file_section(&lines[start..i]) {
                files.push(file);
            }
        } else {
            i += 1;
        }
    }
    ParsedPatch { files }
}

/// Strip a leading `a/` or `b/` prefix the way git's own `--- `/`+++ ` lines
/// always carry one (unless `core.diffOpts` disables prefixes, not a
/// posture this server ever configures — `git_cmd.rs` always spawns with an
/// isolated, ambient-config-free environment).
fn strip_ab_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// `None` for `/dev/null` (a new or deleted file's missing side), `Some` of
/// the prefix-stripped path otherwise.
fn path_or_dev_null(path: &str) -> Option<String> {
    if path == "/dev/null" {
        None
    } else {
        Some(strip_ab_prefix(path).to_string())
    }
}

/// Recover a file's path from its `diff --git a/X b/Y` header line — the
/// only source of a path for a section that never printed a `--- `/`+++ `
/// header of its own (a pure mode change, or a new/deleted empty file).
fn git_git_line_path(header: &str) -> String {
    header
        .strip_prefix("diff --git ")
        .and_then(|rest| rest.split(" b/").next())
        .map(|a_side| strip_ab_prefix(a_side).to_string())
        .unwrap_or_default()
}

fn parse_file_section(lines: &[&str]) -> Option<FileDiff> {
    let mut old_mode = None;
    let mut new_mode = None;
    let mut similarity = None;
    let mut rename_from = None;
    let mut rename_to = None;
    let mut is_copy = false;
    let mut old_side = None;
    let mut new_side = None;
    let mut binary_old = None;
    let mut binary_new = None;
    let mut new_file = false;
    let mut deleted_file = false;

    let mut i = 1; // line 0 is "diff --git a/X b/Y", not needed directly.
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("new file mode ") {
            new_file = true;
        } else if line.starts_with("deleted file mode ") {
            deleted_file = true;
        } else if let Some(v) = line.strip_prefix("old mode ") {
            old_mode = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("new mode ") {
            new_mode = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("similarity index ") {
            similarity = v.trim_end_matches('%').parse::<u8>().ok();
        } else if let Some(v) = line.strip_prefix("rename from ") {
            rename_from = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("rename to ") {
            rename_to = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("copy from ") {
            rename_from = Some(v.to_string());
            is_copy = true;
        } else if let Some(v) = line.strip_prefix("copy to ") {
            rename_to = Some(v.to_string());
            is_copy = true;
        } else if let Some(rest) = line.strip_prefix("Binary files ") {
            // "a/path and b/path differ"
            if let Some((a, b)) = rest
                .strip_suffix(" differ")
                .and_then(|r| r.split_once(" and "))
            {
                binary_old = Some(path_or_dev_null(a));
                binary_new = Some(path_or_dev_null(b));
            }
        } else if let Some(v) = line.strip_prefix("--- ") {
            old_side = Some(path_or_dev_null(v));
        } else if let Some(v) = line.strip_prefix("+++ ") {
            new_side = Some(path_or_dev_null(v));
            // Hunks start immediately after the +++ line.
            i += 1;
            break;
        }
        i += 1;
    }

    if let (Some(old), Some(new)) = (binary_old, binary_new) {
        return Some(FileDiff::Binary {
            old_path: old,
            new_path: new,
        });
    }

    if old_side.is_none() && new_side.is_none() {
        if let (Some(from), Some(to)) = (rename_from, rename_to) {
            return Some(FileDiff::Renamed {
                old_path: from,
                new_path: to,
                similarity: similarity.unwrap_or(0),
                is_copy,
            });
        }
        if let (Some(old_mode), Some(new_mode)) = (old_mode, new_mode) {
            // The path is the same on both sides for a pure mode change;
            // recover it from either --- style header this section never
            // had, so fall back to the "diff --git a/X b/Y" line instead.
            let path = git_git_line_path(lines[0]);
            return Some(FileDiff::ModeChangeOnly {
                path,
                old_mode,
                new_mode,
            });
        }
        if new_file || deleted_file {
            // A new/deleted file with nothing to diff (an empty file) —
            // structurally an ordinary add/delete with no hunks, not a
            // distinct condition (see the module doc). No --- /+++ header
            // ever appeared, so recover the one path that exists from the
            // "diff --git a/X b/Y" line the same way ModeChangeOnly does.
            let path = git_git_line_path(lines[0]);
            return Some(FileDiff::Hunks {
                old_path: if new_file { None } else { Some(path.clone()) },
                new_path: if deleted_file { None } else { Some(path) },
                hunks: Vec::new(),
            });
        }
        // A file section with nothing recognisable — skip.
        return None;
    }

    let old_path = old_side.flatten();
    let new_path = new_side.flatten();
    let hunks = parse_hunks(&lines[i..]);
    Some(FileDiff::Hunks {
        old_path,
        new_path,
        hunks,
    })
}

fn parse_hunks(lines: &[&str]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(mut hunk) = lines[i].strip_prefix("@@ ").and_then(parse_hunk_header) {
            i += 1;
            while i < lines.len() && !lines[i].starts_with("@@ ") {
                let line = lines[i];
                if line == "\\ No newline at end of file" {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_newline_at_eof = true;
                    }
                    i += 1;
                    continue;
                }
                let (kind, text) = match line.as_bytes().first() {
                    Some(b'+') => (LineKind::Added, &line[1..]),
                    Some(b'-') => (LineKind::Removed, &line[1..]),
                    Some(b' ') => (LineKind::Context, &line[1..]),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                hunk.lines.push(DiffLine {
                    kind,
                    text: text.to_string(),
                    no_newline_at_eof: false,
                });
                i += 1;
            }
            hunks.push(hunk);
        } else {
            i += 1;
        }
    }
    hunks
}

/// `-old_start,old_len +new_start,new_len @@ section_heading` (the caller has
/// already stripped the leading `@@ `). Length defaults to `1` when git
/// omitted the `,len` suffix (its own shorthand for "exactly one line").
fn parse_hunk_header(rest: &str) -> Option<Hunk> {
    let rest = rest.strip_prefix('-')?;
    let (old_range, rest) = rest.split_once(" +")?;
    let (new_range, rest) = rest.split_once(" @@")?;
    let (old_start, old_len) = parse_range(old_range)?;
    let (new_start, new_len) = parse_range(new_range)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        section_heading: rest.strip_prefix(' ').unwrap_or(rest).to_string(),
        lines: Vec::new(),
    })
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDINARY_EDIT: &str = "\
diff --git a/a.txt b/a.txt
index 4cb29ea..6addb9b 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,4 @@
 one
-two
+TWO
 three
+four
";

    const NO_NEWLINE_AT_EOF: &str = "\
diff --git a/a.txt b/a.txt
index 6addb9b..54d55bf 100644
--- a/a.txt
+++ b/a.txt
@@ -1,4 +1,3 @@
 one
-TWO
-three
-four
+two
+three
\\ No newline at end of file
";

    const MODE_CHANGE_ONLY: &str = "\
diff --git a/a.txt b/a.txt
old mode 100644
new mode 100755
";

    const BINARY: &str = "\
diff --git a/bin.dat b/bin.dat
index 87ae6b6..22f6b3b 100644
Binary files a/bin.dat and b/bin.dat differ
";

    const PURE_RENAME: &str = "\
diff --git a/a.txt b/b.txt
similarity index 100%
rename from a.txt
rename to b.txt
";

    const RENAME_WITH_EDIT: &str = "\
diff --git a/a.txt b/b.txt
similarity index 73%
rename from a.txt
rename to b.txt
index 4cb29ea..f384549 100644
--- a/a.txt
+++ b/b.txt
@@ -1,3 +1,4 @@
 one
 two
 three
+four
";

    const NEW_EMPTY_FILE: &str = "\
diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..e69de29
";

    const NEW_FILE_WITH_CONTENT: &str = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..7f9f639
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
";

    const DELETED_FILE: &str = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 7f9f639..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
";

    const COMBINED_MERGE: &str = "\
diff --combined f.txt
index c2f2e5e,5909b84..084d8dd
--- a/f.txt
+++ b/f.txt
@@@ -1,5 -1,5 +1,5 @@@
- a
+ A
  b
  c
  d
 -e
 +E
";

    #[test]
    fn ordinary_edit_has_line_level_old_new_numbers() {
        let parsed = parse_unified_diff(ORDINARY_EDIT);
        assert_eq!(parsed.files.len(), 1);
        let FileDiff::Hunks {
            old_path,
            new_path,
            hunks,
        } = &parsed.files[0]
        else {
            panic!("expected Hunks, got {:?}", parsed.files[0]);
        };
        assert_eq!(old_path.as_deref(), Some("a.txt"));
        assert_eq!(new_path.as_deref(), Some("a.txt"));
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!((hunk.old_start, hunk.old_len), (1, 3));
        assert_eq!((hunk.new_start, hunk.new_len), (1, 4));
        assert_eq!(
            hunk.lines,
            vec![
                DiffLine {
                    kind: LineKind::Context,
                    text: "one".into(),
                    no_newline_at_eof: false
                },
                DiffLine {
                    kind: LineKind::Removed,
                    text: "two".into(),
                    no_newline_at_eof: false
                },
                DiffLine {
                    kind: LineKind::Added,
                    text: "TWO".into(),
                    no_newline_at_eof: false
                },
                DiffLine {
                    kind: LineKind::Context,
                    text: "three".into(),
                    no_newline_at_eof: false
                },
                DiffLine {
                    kind: LineKind::Added,
                    text: "four".into(),
                    no_newline_at_eof: false
                },
            ]
        );
    }

    #[test]
    fn no_newline_at_eof_flags_the_preceding_line_not_a_separate_line() {
        let parsed = parse_unified_diff(NO_NEWLINE_AT_EOF);
        let FileDiff::Hunks { hunks, .. } = &parsed.files[0] else {
            panic!("expected Hunks");
        };
        let last = hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::Added);
        assert_eq!(last.text, "three");
        assert!(last.no_newline_at_eof);
        // No stray line for the marker itself.
        assert!(hunks[0]
            .lines
            .iter()
            .all(|l| l.text != "\\ No newline at end of file"));
    }

    #[test]
    fn mode_change_only_has_no_hunks() {
        let parsed = parse_unified_diff(MODE_CHANGE_ONLY);
        assert_eq!(
            parsed.files[0],
            FileDiff::ModeChangeOnly {
                path: "a.txt".into(),
                old_mode: "100644".into(),
                new_mode: "100755".into(),
            }
        );
    }

    #[test]
    fn binary_stanza_is_not_mistaken_for_hunks() {
        let parsed = parse_unified_diff(BINARY);
        assert_eq!(
            parsed.files[0],
            FileDiff::Binary {
                old_path: Some("bin.dat".into()),
                new_path: Some("bin.dat".into()),
            }
        );
    }

    #[test]
    fn pure_rename_has_no_content_change() {
        let parsed = parse_unified_diff(PURE_RENAME);
        assert_eq!(
            parsed.files[0],
            FileDiff::Renamed {
                old_path: "a.txt".into(),
                new_path: "b.txt".into(),
                similarity: 100,
                is_copy: false,
            }
        );
    }

    #[test]
    fn rename_with_edit_is_hunks_not_renamed() {
        let parsed = parse_unified_diff(RENAME_WITH_EDIT);
        let FileDiff::Hunks {
            old_path,
            new_path,
            hunks,
        } = &parsed.files[0]
        else {
            panic!(
                "expected Hunks (a rename with further edits), got {:?}",
                parsed.files[0]
            );
        };
        assert_eq!(old_path.as_deref(), Some("a.txt"));
        assert_eq!(new_path.as_deref(), Some("b.txt"));
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn new_empty_file_is_hunks_with_an_empty_hunk_list() {
        let parsed = parse_unified_diff(NEW_EMPTY_FILE);
        assert_eq!(
            parsed.files[0],
            FileDiff::Hunks {
                old_path: None,
                new_path: Some("empty.txt".into()),
                hunks: vec![],
            }
        );
    }

    #[test]
    fn new_file_with_content_has_no_old_path() {
        let parsed = parse_unified_diff(NEW_FILE_WITH_CONTENT);
        let FileDiff::Hunks {
            old_path,
            new_path,
            hunks,
        } = &parsed.files[0]
        else {
            panic!("expected Hunks");
        };
        assert_eq!(*old_path, None);
        assert_eq!(new_path.as_deref(), Some("new.txt"));
        assert_eq!(hunks[0].old_start, 0);
        assert_eq!(hunks[0].old_len, 0);
    }

    #[test]
    fn deleted_file_has_no_new_path() {
        let parsed = parse_unified_diff(DELETED_FILE);
        let FileDiff::Hunks {
            old_path, new_path, ..
        } = &parsed.files[0]
        else {
            panic!("expected Hunks");
        };
        assert_eq!(old_path.as_deref(), Some("gone.txt"));
        assert_eq!(*new_path, None);
    }

    #[test]
    fn combined_merge_diff_is_detected_and_preserved_not_mangled() {
        let parsed = parse_unified_diff(COMBINED_MERGE);
        let FileDiff::Combined { path, raw } = &parsed.files[0] else {
            panic!("expected Combined, got {:?}", parsed.files[0]);
        };
        assert_eq!(path, "f.txt");
        // Nothing lost: the raw text still contains the real @@@-style
        // header and every content line, byte for byte.
        assert!(raw.contains("@@@ -1,5 -1,5 +1,5 @@@"));
        assert!(raw.contains("+ A"));
        assert!(raw.contains(" -e"));
        assert!(raw.contains(" +E"));
    }

    #[test]
    fn multiple_files_in_one_patch_are_all_parsed() {
        let combined_text = format!("{ORDINARY_EDIT}{BINARY}{PURE_RENAME}");
        let parsed = parse_unified_diff(&combined_text);
        assert_eq!(parsed.files.len(), 3);
        assert!(matches!(parsed.files[0], FileDiff::Hunks { .. }));
        assert!(matches!(parsed.files[1], FileDiff::Binary { .. }));
        assert!(matches!(parsed.files[2], FileDiff::Renamed { .. }));
    }

    #[test]
    fn empty_patch_has_no_files() {
        assert_eq!(parse_unified_diff(""), ParsedPatch { files: vec![] });
    }
}
