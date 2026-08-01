//! Pure decisions for the diff view's accessible hunk navigation (M2.16e, #210).
//!
//! Today's diff view renders `CommitDiff.patch` as one flat `<pre>` of
//! per-line spans (`detail.rs`, mirrored full-screen in `viewer.rs`). The
//! roving-focus wiring there needs to know, for the raw patch text, **which
//! rendered lines are hunk headers** and **what a screen reader should say
//! about each** — and that mapping is a pure function of the text, so it
//! lives here where `cargo test` can reach it.
//!
//! ## Deliberately a raw-text walk, and deliberately temporary
//!
//! `git_vista_protocol::diff::parse_unified_diff` (#69a) already parses this
//! text into structured [`Hunk`]s — but its output carries no mapping back to
//! *line indices in the raw text*, which is the coordinate the flat rendering
//! actually uses. Rather than bolt indices onto the protocol type, this walk
//! re-derives the little it needs (header positions, per-hunk add/remove
//! counts) directly. When #69e replaces the flat `<pre>` with rendering driven
//! by `ParsedPatch` itself, this function is the piece that dies with it; the
//! focus model it feeds (`features::a11y::focus::GraphFocus`) is index-based
//! and survives unchanged. Scope note argued on #210 (2026-08-01).
//!
//! Combined (merge) hunk headers (`@@@`) are *not* navigation stops: the
//! protocol parser leaves combined diffs deliberately opaque, and a label
//! this walk can't back up would be worse than the plain colored line the
//! view already shows. Only ordinary `@@ -a,b +c,d @@` headers qualify.
//!
//! ## Truncated patches
//!
//! The server caps patches at a **line boundary** (`read.rs`,
//! `truncate_at_line`), so a cap that lands mid-hunk simply runs the
//! `old_len`/`new_len` countdown out at end of input: no phantom stops, every
//! header before the cut keeps its stop, and headers after the cut don't
//! exist in the text at all. What the cut *can* do is leave the final hunk's
//! counted added/removed lines short of what its header declared — so when
//! the walk ends with the countdown unexhausted, the final label says
//! `, truncated` rather than stating an undercount as fact. The rendering's
//! separate truncation note discloses that the patch as a whole is cut.

use std::collections::HashMap;

/// One keyboard-navigable hunk header in the rendered patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkNavEntry {
    /// Index into `patch.lines()` of the `@@` header this entry describes —
    /// the same enumeration the rendering walks, so the wiring can attach
    /// focus attributes while it maps lines to spans.
    pub line_index: usize,
    /// The VoiceOver label: file, per-file hunk ordinal, the new-side line
    /// range, and add/remove counts. Spoken once on focus, so it leads with
    /// the file and position rather than the raw `-12,5 +12,8` shorthand.
    pub label: String,
}

/// Every navigable hunk header in `patch`, in rendering order.
///
/// The walk tracks hunk bodies by the exact `old_len`/`new_len` countdown the
/// unified format defines, so a body line that *begins* with `+++`/`---`/`@@`
/// (legal — markers are stripped per line, not per prefix-match) can never be
/// mistaken for a file header or a new hunk.
pub fn hunk_nav(patch: &str) -> Vec<HunkNavEntry> {
    struct PendingHunk {
        line_index: usize,
        file: String,
        new_start: u32,
        new_len: u32,
        heading: String,
        added: u32,
        removed: u32,
    }
    let mut pending: Vec<PendingHunk> = Vec::new();
    // Remaining old-side / new-side lines of the hunk body being consumed;
    // while either is nonzero, the current line belongs to a body.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    // The `+++` side names the file a hunk edits; a deleted file has
    // `+++ /dev/null`, so the `---` side is kept as the fallback name.
    let (mut minus_file, mut plus_file) = (None::<String>, None::<String>);

    for (i, line) in patch.lines().enumerate() {
        if old_left > 0 || new_left > 0 {
            // Inside a hunk body: classify by marker and count down.
            match line.as_bytes().first() {
                Some(b'+') => {
                    new_left = new_left.saturating_sub(1);
                    if let Some(h) = pending.last_mut() {
                        h.added += 1;
                    }
                }
                Some(b'-') => {
                    old_left = old_left.saturating_sub(1);
                    if let Some(h) = pending.last_mut() {
                        h.removed += 1;
                    }
                }
                // `\ No newline at end of file` counts on neither side.
                Some(b'\\') => {}
                // Context (or a blank context line git printed as "").
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            minus_file = parse_file_side(rest);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            plus_file = parse_file_side(rest);
        } else if let Some((old_len, new_start, new_len, heading)) = parse_hunk_header(line) {
            let file = plus_file
                .clone()
                .or_else(|| minus_file.clone())
                .unwrap_or_else(|| "unknown file".to_string());
            pending.push(PendingHunk {
                line_index: i,
                file,
                new_start,
                new_len,
                heading,
                added: 0,
                removed: 0,
            });
            old_left = old_len;
            new_left = new_len;
        }
    }

    // A countdown still owed lines at end of input means the cap cut the
    // final hunk's body short — its counted added/removed understate what
    // the header declared, so its label must say so (module doc).
    let cut_mid_hunk = old_left > 0 || new_left > 0;

    // Per-file ordinals need the per-file totals first; both passes are O(n)
    // — a 5 MB refactor diff can carry tens of thousands of hunks, and a
    // rescan-per-hunk would stall the iPad's main thread before first paint.
    let mut totals: HashMap<&str, u32> = HashMap::new();
    for h in &pending {
        *totals.entry(h.file.as_str()).or_insert(0) += 1;
    }
    let mut seen: HashMap<&str, u32> = HashMap::new();
    let last_idx = pending.len().checked_sub(1);
    let mut entries = Vec::with_capacity(pending.len());
    for (idx, h) in pending.iter().enumerate() {
        let ordinal = {
            let c = seen.entry(h.file.as_str()).or_insert(0);
            *c += 1;
            *c
        };
        let total = totals[h.file.as_str()];
        let range = match h.new_len {
            // A pure deletion has no new-side lines; "at line N" places it
            // without claiming a range that doesn't exist.
            0 => format!("at line {}", h.new_start),
            1 => format!("line {}", h.new_start),
            // Saturating: a crafted `+4294967295,2` header must not panic a
            // debug build; real git output never gets near the edge.
            n => format!(
                "lines {}\u{2013}{}",
                h.new_start,
                h.new_start.saturating_add(n - 1)
            ),
        };
        let heading = if h.heading.is_empty() {
            String::new()
        } else {
            format!(", in {}", h.heading)
        };
        let truncated = if cut_mid_hunk && Some(idx) == last_idx {
            ", truncated"
        } else {
            ""
        };
        entries.push(HunkNavEntry {
            line_index: h.line_index,
            label: format!(
                "{}, hunk {} of {}: {}, {} added, {} removed{}{}",
                h.file, ordinal, total, range, h.added, h.removed, heading, truncated
            ),
        });
    }
    entries
}

/// The path from one side of a `---`/`+++` header, `None` for `/dev/null`.
/// Strips git's `a/`/`b/` prefixes; quoted/escaped paths are passed through
/// as printed rather than unescaped — a spoken label with a literal escape in
/// it is still identifiable, and unescaping here would duplicate protocol
/// parser territory this walk deliberately stays out of.
fn parse_file_side(rest: &str) -> Option<String> {
    if rest == "/dev/null" {
        return None;
    }
    let rest = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    Some(rest.to_string())
}

/// `@@ -old_start[,old_len] +new_start[,new_len] @@[ heading]` →
/// `(old_len, new_start, new_len, heading)`. `None` for anything else,
/// including combined `@@@` headers (see the module doc). An omitted `,len`
/// is 1, git's shorthand, resolved here so callers never re-derive it.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, String)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let (_, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    let heading = rest.strip_prefix(' ').unwrap_or(rest).to_string();
    Some((old_len, new_start, new_len, heading))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic two-file patch: foo.rs has two hunks (the second with a
    // section heading), bar.txt one. Bodies are exact per the header counts.
    const PATCH: &str = "\
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
diff --git a/bar.txt b/bar.txt
--- a/bar.txt
+++ b/bar.txt
@@ -1 +1 @@
-old
+new
";

    #[test]
    fn headers_are_found_at_their_raw_line_indices() {
        let nav = hunk_nav(PATCH);
        // Literal indices counted by hand in PATCH above — not re-derived.
        assert_eq!(
            nav.iter().map(|e| e.line_index).collect::<Vec<_>>(),
            vec![4, 10, 17]
        );
    }

    #[test]
    fn labels_carry_file_ordinal_range_and_counts() {
        let nav = hunk_nav(PATCH);
        assert_eq!(
            nav[0].label,
            "src/foo.rs, hunk 1 of 2: lines 10\u{2013}13, 2 added, 1 removed"
        );
        assert_eq!(
            nav[1].label,
            "src/foo.rs, hunk 2 of 2: lines 31\u{2013}32, 1 added, 1 removed, \
             in fn frobnicate()"
        );
        assert_eq!(
            nav[2].label,
            "bar.txt, hunk 1 of 1: line 1, 1 added, 1 removed"
        );
    }

    #[test]
    fn a_body_line_starting_with_at_signs_is_not_a_navigation_stop() {
        // Legal: a context line whose *text* is "@@ -1 +1 @@" — the countdown
        // must swallow it. A naive prefix scan would mint a phantom hunk.
        let patch = "\
--- a/x
+++ b/x
@@ -1,3 +1,3 @@
 before
 @@ -9,9 +9,9 @@
 after
";
        let nav = hunk_nav(patch);
        assert_eq!(nav.len(), 1);
        assert_eq!(nav[0].line_index, 2);
    }

    #[test]
    fn combined_merge_headers_are_skipped() {
        let patch = "\
--- a/x
+++ b/x
@@@ -1,2 -1,2 +1,2 @@@
 whatever
";
        assert_eq!(hunk_nav(patch), Vec::new());
    }

    #[test]
    fn a_deleted_file_is_named_by_its_old_side() {
        let patch = "\
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-line one
-line two
";
        let nav = hunk_nav(patch);
        assert_eq!(nav.len(), 1);
        assert_eq!(
            nav[0].label,
            "gone.rs, hunk 1 of 1: at line 0, 0 added, 2 removed"
        );
    }

    #[test]
    fn a_patch_cut_mid_hunk_keeps_prior_stops_and_flags_the_last_label() {
        // The second header declares a 20/22-line body but the cap cut it
        // after two body lines — the countdown runs out at EOF (no phantom
        // stops), and the final label admits the undercount instead of
        // stating "1 added, 0 removed" as fact.
        let patch = "\
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
        let nav = hunk_nav(patch);
        assert_eq!(nav.len(), 2);
        assert_eq!(
            nav[0].label,
            "x, hunk 1 of 2: lines 1\u{2013}2, 1 added, 1 removed"
        );
        assert_eq!(
            nav[1].label,
            "x, hunk 2 of 2: lines 10\u{2013}31, 1 added, 0 removed, truncated"
        );
    }

    #[test]
    fn an_empty_or_headerless_patch_yields_no_stops() {
        assert_eq!(hunk_nav(""), Vec::new());
        assert_eq!(hunk_nav("Binary files a/x and b/x differ\n"), Vec::new());
    }
}
