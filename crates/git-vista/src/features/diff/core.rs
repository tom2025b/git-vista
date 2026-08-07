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

// ---------------------------------------------------------------------------
// Virtualized rendering (M2.16g, #350) — the consumer #69c's primitive lacked
// ---------------------------------------------------------------------------

/// How a diff line's on-screen height is decided, which differs between the
/// app's two diff surfaces and is **not** a detail a caller may guess at.
///
/// Measured from the stylesheet, not assumed:
///
/// * `.detail-diff` (the commit detail panel) is `white-space: pre` with
///   `overflow-x: auto` — a long line scrolls sideways and every line is
///   exactly one row tall.
/// * `.viewer-pre` (the full-screen viewer) adds `white-space: pre-wrap` and
///   `word-break: break-word` — a long line *wraps*, so its height depends on
///   its own length and the container's width.
///
/// Feeding uniform heights for the wrapping surface would put every row after
/// the first wrapped line at the wrong offset — the scroll position and the
/// rendered window would disagree, which is the whole failure mode
/// virtualization is supposed to avoid. [`CumulativeHeights`] takes an
/// arbitrary heights array precisely so both cases can be expressed.
///
/// [`CumulativeHeights`]: git_vista_core::virtualize::CumulativeHeights
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineWrap {
    /// One row per line regardless of length (`white-space: pre`).
    Never,
    /// Lines wrap at `columns` characters (`white-space: pre-wrap`), so a
    /// line occupies `ceil(len / columns)` rows, minimum one.
    ///
    /// `columns` is an **estimate** derived from container width divided by
    /// monospace character width — this module cannot measure the DOM. It is
    /// good enough for windowing (an off-by-a-little estimate shifts the
    /// window slightly, it does not corrupt it) but it is an estimate, and
    /// the device pass is what confirms it looks right.
    Wrapped { columns: usize },
}

/// Per-line heights for a patch, ready to hand to [`CumulativeHeights::new`].
///
/// Counts **characters, not bytes**, for the wrapped case: a patch carrying
/// non-ASCII text would otherwise over-estimate its own height and leave a
/// visible gap at the bottom of the scroll range.
///
/// [`CumulativeHeights::new`]: git_vista_core::virtualize::CumulativeHeights::new
pub fn line_heights(patch: &str, line_height: f64, wrap: LineWrap) -> Vec<f64> {
    patch
        .lines()
        .map(|line| match wrap {
            LineWrap::Never => line_height,
            LineWrap::Wrapped { columns } => {
                if columns == 0 {
                    // A zero-width container is not a real layout; treat it
                    // as one row rather than dividing by zero.
                    return line_height;
                }
                let chars = line.chars().count().max(1);
                let rows = chars.div_ceil(columns);
                rows as f64 * line_height
            }
        })
        .collect()
}

/// The slice of a patch to actually render, plus the spacer heights that keep
/// the scrollbar honest about the un-rendered remainder.
///
/// `pad_top`/`pad_bottom` exist because a virtualized list must still occupy
/// its full scroll height — otherwise the scrollbar reports the height of the
/// rendered window rather than the document, and scrolling jumps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderWindow {
    /// First line index to render (inclusive).
    pub start: usize,
    /// One past the last line index to render (exclusive).
    pub end: usize,
    /// Height of the un-rendered block above `start`.
    pub pad_top: f64,
    /// Height of the un-rendered block below `end`.
    pub pad_bottom: f64,
}

impl RenderWindow {
    /// Number of lines this window renders.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Whether line `index` is inside the rendered window — the question a
    /// focus handler asks before assuming the element it wants to focus
    /// exists in the DOM at all.
    pub fn contains(&self, index: usize) -> bool {
        index >= self.start && index < self.end
    }
}

/// Which lines to render for a viewport, and the spacer heights around them.
///
/// A thin wrapper over [`visible_range`] that adds the padding arithmetic —
/// kept here rather than in `git-vista-core` because the spacer concept is a
/// rendering decision, while the primitive stays purely "heights and a scroll
/// offset in, indices out."
///
/// [`visible_range`]: git_vista_core::virtualize::CumulativeHeights::visible_range
pub fn render_window(
    heights: &git_vista_core::virtualize::CumulativeHeights,
    viewport_height: f64,
    scroll_offset: f64,
    overscan: usize,
) -> RenderWindow {
    let range = heights.visible_range(viewport_height, scroll_offset, overscan);
    let total = heights.total_height();
    let pad_top = range.start_offset;
    let pad_bottom = (total - heights.offset_of(range.end)).max(0.0);
    RenderWindow {
        start: range.start,
        end: range.end,
        pad_top,
        pad_bottom,
    }
}

/// The scroll offset that brings line `index` into a `viewport_height`
/// viewport, given where the view is scrolled now — **the piece that keeps
/// #210's keyboard hunk navigation working once the list is windowed.**
///
/// Without this, tabbing to a hunk header outside the rendered window focuses
/// an element that does not exist in the DOM: focus is silently lost and the
/// view does not move. That is the specific regression #350 names as most
/// likely to make a naive windowing implementation wrong.
///
/// Returns `None` when the line is already fully visible, so a caller can skip
/// a redundant scroll write (which would fight a user's in-progress scroll).
/// Otherwise returns the minimal scroll that reveals it: aligned to the top
/// when scrolling up to it, to the bottom when scrolling down — the same
/// "scroll the least that works" behaviour `Element::scrollIntoView` has with
/// `block: "nearest"`, chosen so keyboard navigation does not yank the
/// viewport further than the user's own step.
pub fn scroll_to_reveal(
    heights: &git_vista_core::virtualize::CumulativeHeights,
    index: usize,
    viewport_height: f64,
    current_scroll: f64,
) -> Option<f64> {
    if index >= heights.item_count() {
        return None;
    }
    let top = heights.offset_of(index);
    let bottom = heights.offset_of(index + 1);
    if top < current_scroll {
        Some(top)
    } else if bottom > current_scroll + viewport_height {
        // Align the item's bottom edge to the viewport's, never scrolling
        // past the top of the document.
        Some((bottom - viewport_height).max(0.0))
    } else {
        None
    }
}

/// One selectable hunk header, addressed the way `git_vista_protocol`'s
/// `HunkRef`/`FileSelection` need it (M2.17d, #215): canonical file path,
/// 0-based per-file ordinal, and the header's own declared anchors.
///
/// A second raw-text walk alongside [`hunk_nav`], deliberately — the staging
/// selection UI renders the exact same flat `patch.lines()` text `hunk_nav`
/// already maps for keyboard navigation, so it needs the same line-index
/// coordinate space `hunk_nav` uses, not the structured parser's. `hunk_nav`
/// itself is #210's tested contract and is not touched here; this is an
/// independent function that happens to share its two small parsing helpers
/// ([`parse_hunk_header`], [`parse_file_side`]). See `hunk_nav`'s module doc
/// for why a raw-text walk is the right (and deliberately temporary) choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectableHunk {
    /// Index into `patch.lines()` of this hunk's `@@` header — the same
    /// enumeration [`hunk_nav`]'s entries use, so a caller can pair the two
    /// walks up by `line_index` (both visit ordinary headers in the same
    /// order, skipping combined `@@@` headers identically).
    pub line_index: usize,
    /// The file this hunk belongs to, addressed the canonical way
    /// (`git_vista_protocol::patch_build::canonical_path`'s rule: new-side
    /// name when the header pair has one).
    pub file: String,
    /// 0-based index into *this file's own* hunk list — the ordinal
    /// `HunkRef::index` needs. Distinct from `hunk_nav`'s 1-based
    /// "hunk N of M" spoken ordinal.
    pub ordinal: u32,
    /// The hunk header's declared old-side start — `HunkRef::old_start`.
    pub old_start: u32,
    /// The hunk header's declared new-side start — `HunkRef::new_start`.
    pub new_start: u32,
}

/// Every selectable (ordinary, non-combined) hunk header in `patch`, in
/// rendering order — see [`SelectableHunk`].
pub fn selectable_hunks(patch: &str) -> Vec<SelectableHunk> {
    struct Pending {
        line_index: usize,
        file: String,
        old_start: u32,
        new_start: u32,
    }
    let mut pending: Vec<Pending> = Vec::new();
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut minus_file, mut plus_file) = (None::<String>, None::<String>);

    for (i, line) in patch.lines().enumerate() {
        if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'\\') => {}
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
        } else if let Some((old_len, new_start, new_len, _heading)) = parse_hunk_header(line) {
            // New-side name when present, else old-side — the same
            // canonical-path rule `patch_build::canonical_path` applies to
            // parsed `FileDiff`s, applied here to the raw header pair.
            let file = plus_file
                .clone()
                .or_else(|| minus_file.clone())
                .unwrap_or_else(|| "unknown file".to_string());
            let old_start = line
                .strip_prefix("@@ -")
                .and_then(|rest| rest.split_once(" +"))
                .and_then(|(old, _)| parse_range(old))
                .map(|(start, _)| start)
                .unwrap_or(0);
            pending.push(Pending {
                line_index: i,
                file,
                old_start,
                new_start,
            });
            old_left = old_len;
            new_left = new_len;
        }
    }

    let mut seen: HashMap<&str, u32> = HashMap::new();
    pending
        .iter()
        .map(|h| {
            let ordinal = {
                let c = seen.entry(h.file.as_str()).or_insert(0);
                let ord = *c;
                *c += 1;
                ord
            };
            SelectableHunk {
                line_index: h.line_index,
                file: h.file.clone(),
                ordinal,
                old_start: h.old_start,
                new_start: h.new_start,
            }
        })
        .collect()
}

/// The path from one side of a `---`/`+++` header, `None` for `/dev/null`.
///
/// Delegates to [`git_vista_protocol::path_or_dev_null`] — the same
/// unquoting the server-side parser uses (trailing-tab space termination,
/// C-style quote/octal-escape undoing) — rather than a second, approximate
/// re-derivation. This walk previously stripped only the `a/`/`b/` prefix
/// and left quoting untouched, reasoning that a spoken hunk label doesn't
/// need exactness; `selectable_hunks` (#215) then reused that same
/// approximate path as `FileSelection.path` on the wire, where it does —
/// and a file with a space or non-ASCII byte in its name would silently
/// fail to stage (`SelectionMismatch::UnknownPath` on the server, since its
/// canonical path never matched this walk's quoted-looking one). Sharing
/// the one correct implementation is what keeps that from recurring, and
/// costs `hunk_nav`'s labels nothing — a correctly unescaped name is a
/// strictly better spoken label, not a worse one.
fn parse_file_side(rest: &str) -> Option<String> {
    git_vista_protocol::path_or_dev_null(rest)
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

    // ---- selectable_hunks (M2.17d, #215) ------------------------------

    #[test]
    fn selectable_hunks_carry_per_file_ordinals_and_anchors() {
        let hunks = selectable_hunks(PATCH);
        assert_eq!(
            hunks,
            vec![
                SelectableHunk {
                    line_index: 4,
                    file: "src/foo.rs".into(),
                    ordinal: 0,
                    old_start: 10,
                    new_start: 10,
                },
                SelectableHunk {
                    line_index: 10,
                    file: "src/foo.rs".into(),
                    ordinal: 1,
                    old_start: 30,
                    new_start: 31,
                },
                SelectableHunk {
                    line_index: 17,
                    file: "bar.txt".into(),
                    ordinal: 0,
                    old_start: 1,
                    new_start: 1,
                },
            ]
        );
    }

    #[test]
    fn selectable_hunks_line_indices_match_hunk_nav_exactly() {
        // Both walks must agree on which lines are navigation stops, and in
        // what order — the staging UI pairs them up by `line_index`.
        let nav_lines: Vec<usize> = hunk_nav(PATCH).iter().map(|e| e.line_index).collect();
        let sel_lines: Vec<usize> = selectable_hunks(PATCH)
            .iter()
            .map(|h| h.line_index)
            .collect();
        assert_eq!(nav_lines, sel_lines);
    }

    #[test]
    fn selectable_hunks_skip_combined_merge_headers() {
        let patch = "\
--- a/x
+++ b/x
@@@ -1,2 -1,2 +1,2 @@@
 whatever
";
        assert_eq!(selectable_hunks(patch), Vec::new());
    }

    #[test]
    fn selectable_hunks_names_a_deleted_file_by_its_old_side() {
        let patch = "\
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-line one
-line two
";
        let hunks = selectable_hunks(patch);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file, "gone.rs");
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].new_start, 0);
    }

    // ---- Diff hunk-navigation performance budget (#211, M2.16f) ---------------
    //
    // #211's text calls its target "the virtualized diff view (69c)". As of
    // this writing that is a stale premise, not a live shape to benchmark:
    // the diff view renders `CommitDiff.patch` as one flat `<pre>` (this
    // module's own doc comment, `detail.rs`, `viewer.rs`) — no virtualization
    // is wired into it. #69c's `CumulativeHeights`/`visible_range`
    // (`git_vista_core::virtualize`) is a real, already-tested primitive, but
    // it has zero consumers in the tree (verified: no reference to either
    // name outside its own module and its own tests). There is nothing
    // "virtualized diff view" shaped to measure yet.
    //
    // What *is* real, host-tested, and runs on every diff render today
    // regardless of virtualization is `hunk_nav` itself: a full O(n) walk of
    // the raw patch text, called from `detail.rs::accessible_patch_view`
    // (panel and full-screen viewer) and from `staging_view.rs` for
    // hunk-selection labels. Its own doc comment above already names the
    // failure mode this budget pins down: "a 5 MB refactor diff can carry
    // tens of thousands of hunks, and a rescan-per-hunk would stall the
    // iPad's main thread before first paint." That per-hunk-rescan shape
    // does not exist today (both passes here are already O(n)), but nothing
    // stops a future edit from accidentally reintroducing it — that is
    // exactly the regression a budget test exists to catch.
    //
    // **What this budget does NOT cover** (stated plainly, per house rule —
    // a budget that quietly claims more than it measures is worse than none):
    // - The actual DOM/`<pre>` construction in `detail.rs`/`viewer.rs` is
    //   `#[cfg(target_arch = "wasm32")]`-gated; `cargo test --workspace`
    //   never compiles it and this repo has no wasm test harness. Unmeasured.
    // - Whether virtualization is "engaged" — it isn't wired into the diff
    //   view at all, so there is nothing to prove engaged or broken. A
    //   future task that wires `CumulativeHeights` into the render path
    //   should add its own budget alongside that wiring.
    // - Real-world patch text (renamed files, binary markers, combined merge
    //   headers, mixed hunk sizes). The generator below produces one
    //   synthetic file with uniformly-sized hunks — deliberately the
    //   cheapest-per-byte shape (like 68e's uniform untracked-file
    //   generator), so this is closer to a best case than a worst case.

    /// One synthetic hunk: 2 context lines, `pairs` removed lines, `pairs`
    /// added lines — `old_len == new_len == 2 + pairs`, so the header counts
    /// match the body exactly (a mismatched header would make `hunk_nav`'s
    /// countdown desync, corrupting every later hunk's line-index mapping —
    /// this generator must not itself be the thing that breaks the
    /// measurement). Hunks are numbered into `bench.rs` under one file
    /// header, since `hunk_nav`'s cost is dominated by total line count, not
    /// by how many file headers that count is spread across.
    fn generate_patch(num_hunks: usize, pairs: usize) -> String {
        let mut s = String::from("--- a/bench.rs\n+++ b/bench.rs\n");
        for i in 0..num_hunks {
            let start = 1 + i * 1000; // spaced out so ranges never overlap
            let len = 2 + pairs;
            s.push_str(&format!("@@ -{start},{len} +{start},{len} @@\n"));
            s.push_str(" context one\n context two\n");
            for j in 0..pairs {
                s.push_str(&format!("-removed line {j}\n"));
            }
            for j in 0..pairs {
                s.push_str(&format!("+added line {j}\n"));
            }
        }
        s
    }

    /// One measurement: wall-clock time for `hunk_nav` over a synthetic
    /// `num_hunks`-hunk patch, plus the patch's own byte length (so the
    /// ladder can be read against `DIFF_PATCH_CAP`/`DIFF_PATCH_CAP_FULL` in
    /// `handlers/read.rs`, 200,000 / 5,000,000 bytes) and a structural check
    /// that every hunk was actually found — a fast wrong answer (e.g. an
    /// early return, or a countdown desync eating the rest of the patch)
    /// must not be mistaken for a fast correct one.
    fn time_hunk_nav(num_hunks: usize) -> (std::time::Duration, usize, usize) {
        let patch = generate_patch(num_hunks, 3);
        let bytes = patch.len();
        let start = std::time::Instant::now();
        let nav = hunk_nav(&patch);
        let elapsed = start.elapsed();
        assert_eq!(
            nav.len(),
            num_hunks,
            "hunk_nav found {} of {num_hunks} synthetic hunks — a fast wrong \
             answer, not a fast correct one; the measurement below is not \
             trustworthy if this fails",
            nav.len()
        );
        (elapsed, bytes, nav.len())
    }

    /// The real measurement behind `docs/PERFORMANCE_BUDGETS.md`'s `hunk_nav`
    /// section — **not** part of the normal test run. `#[ignore]`d because a
    /// 50,000-hunk synthetic patch (~7 MB of generated text) has no place in
    /// every `cargo test`/CI run; `hunk_nav_budget_holds_at_2k_hunks` below
    /// is the fast, always-on regression check derived from what this finds.
    ///
    /// Run explicitly to reproduce or update the recorded numbers:
    /// `cargo test -p git-vista -- --ignored --nocapture hunk_nav_ladder`
    ///
    /// One host, one run each — not a statistically controlled benchmark
    /// suite, same caveat `docs/PERFORMANCE_BUDGETS.md` states up front.
    #[test]
    #[ignore = "generates up to a ~7 MB synthetic patch; run explicitly, see doc comment"]
    fn hunk_nav_ladder() {
        println!("\n#211 hunk_nav ladder (one host, one run each):");
        println!("{:>10}  {:>12}  {:>10}", "n_hunks", "elapsed", "bytes");
        for n in [100usize, 1_000, 2_000, 10_000, 20_000, 50_000] {
            let (elapsed, bytes, _found) = time_hunk_nav(n);
            println!("{n:>10}  {elapsed:>12?}  {bytes:>10}");
        }
    }

    /// A scaling check, not just a wall-clock one: 10x the hunk count must
    /// not cost anywhere near 10x-squared the time. `hunk_nav` is O(n) by
    /// construction (two linear passes — see the module doc and the doc
    /// comment on the per-file-ordinal pass above), so time should scale
    /// roughly linearly; this asserts a generous *upper* bound on the ratio
    /// (25x for a 10x size increase) loose enough to absorb real per-call
    /// overhead and a loaded CI runner, but tight enough that an accidental
    /// quadratic reintroduction — which would show roughly a 10x *further*
    /// slowdown on top of the expected 10x, i.e. close to 100x — still fails
    /// it. This is the check that would actually catch the "rescan-per-hunk"
    /// regression the module doc above warns about; the wall-clock budget
    /// test below would not reliably catch it until it got much worse.
    #[test]
    fn hunk_nav_scales_roughly_linearly_not_quadratically() {
        let (small_elapsed, _, _) = time_hunk_nav(2_000);
        let (large_elapsed, _, _) = time_hunk_nav(20_000);
        let small_nanos = small_elapsed.as_nanos().max(1);
        let large_nanos = large_elapsed.as_nanos();
        let ratio = large_nanos as f64 / small_nanos as f64;
        assert!(
            ratio < 25.0,
            "hunk_nav took {large_elapsed:?} at 20,000 hunks vs {small_elapsed:?} \
             at 2,000 hunks — a {ratio:.1}x slowdown for a 10x size increase. \
             hunk_nav is meant to be O(n); this ratio is only consistent with \
             an accidental quadratic (or worse) regression, not measurement \
             noise."
        );
    }

    /// The always-on regression check: 2,000 hunks (~280 KB of patch text,
    /// cheap enough for every `cargo test`/CI run, and past `DIFF_PATCH_CAP`
    /// — a realistic "hit the panel's cap" size) must complete well inside a
    /// generous multiple of the budget `docs/PERFORMANCE_BUDGETS.md` states.
    #[test]
    fn hunk_nav_budget_holds_at_2k_hunks() {
        let (elapsed, _bytes, _found) = time_hunk_nav(2_000);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "hunk_nav over a 2,000-hunk patch took {elapsed:?}, budget is \
             500ms (see docs/PERFORMANCE_BUDGETS.md) — this is a real \
             regression, not flakiness, unless the CI runner is unusually \
             loaded"
        );
    }

    // -------------------------------------------------------------------
    // Virtualized rendering (M2.16g, #350)
    // -------------------------------------------------------------------

    use git_vista_core::virtualize::CumulativeHeights;

    const LH: f64 = 20.0;

    #[test]
    fn a_non_wrapping_surface_gives_every_line_one_row() {
        let patch = "short\na much longer line than the others\nx";
        let h = line_heights(patch, LH, LineWrap::Never);
        assert_eq!(h, vec![LH, LH, LH]);
    }

    #[test]
    fn a_wrapping_surface_charges_long_lines_for_the_rows_they_take() {
        // 10 columns: 5 chars = 1 row, 25 chars = 3 rows, 20 chars = 2 rows.
        let patch = format!("{}\n{}\n{}", "x".repeat(5), "y".repeat(25), "z".repeat(20));
        let h = line_heights(&patch, LH, LineWrap::Wrapped { columns: 10 });
        assert_eq!(h, vec![LH, 3.0 * LH, 2.0 * LH]);
    }

    #[test]
    fn an_empty_line_still_occupies_one_row_when_wrapping() {
        // A blank line is a real row on screen; charging it zero height
        // would make every offset below it wrong.
        let h = line_heights("\n\n", LH, LineWrap::Wrapped { columns: 10 });
        assert_eq!(h, vec![LH, LH]);
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        // 10 non-ASCII characters (3 bytes each in UTF-8) must be one row at
        // 10 columns, not three — byte-counting would over-estimate the
        // height and leave a gap at the end of the scroll range.
        let line = "\u{4e2d}".repeat(10);
        let h = line_heights(&line, LH, LineWrap::Wrapped { columns: 10 });
        assert_eq!(h, vec![LH], "counted bytes instead of characters");
    }

    #[test]
    fn a_zero_width_container_does_not_divide_by_zero() {
        let h = line_heights("abc", LH, LineWrap::Wrapped { columns: 0 });
        assert_eq!(h, vec![LH]);
    }

    #[test]
    fn the_window_renders_a_slice_and_pads_the_rest_to_full_height() {
        // 100 lines x 20px = 2000px total; a 200px viewport at the top.
        let heights = CumulativeHeights::new(&vec![LH; 100]);
        let w = render_window(&heights, 200.0, 0.0, 0);
        assert_eq!(w.start, 0);
        assert!(
            w.len() < 100,
            "a window must render fewer lines than the whole patch"
        );
        assert_eq!(w.pad_top, 0.0);
        // The padding plus the rendered block must still add up to the full
        // document height, or the scrollbar lies about how much there is.
        let rendered = (w.end - w.start) as f64 * LH;
        assert!(
            (w.pad_top + rendered + w.pad_bottom - 2000.0).abs() < f64::EPSILON,
            "pad_top {} + rendered {} + pad_bottom {} != total 2000",
            w.pad_top,
            rendered,
            w.pad_bottom
        );
    }

    #[test]
    fn scrolling_down_moves_the_window_and_shifts_the_top_pad() {
        let heights = CumulativeHeights::new(&vec![LH; 100]);
        let top = render_window(&heights, 200.0, 0.0, 0);
        let mid = render_window(&heights, 200.0, 1000.0, 0);
        assert!(
            mid.start > top.start,
            "window did not advance with the scroll"
        );
        assert!(
            mid.pad_top > 0.0,
            "scrolled-past content must still occupy height"
        );
        let rendered = (mid.end - mid.start) as f64 * LH;
        assert!(
            (mid.pad_top + rendered + mid.pad_bottom - 2000.0).abs() < 0.001,
            "document height not conserved while scrolled"
        );
    }

    #[test]
    fn a_patch_shorter_than_the_viewport_renders_whole_with_no_padding() {
        let heights = CumulativeHeights::new(&vec![LH; 3]);
        let w = render_window(&heights, 500.0, 0.0, 0);
        assert_eq!((w.start, w.end), (0, 3));
        assert_eq!(w.pad_top, 0.0);
        assert_eq!(w.pad_bottom, 0.0);
    }

    #[test]
    fn an_empty_patch_produces_an_empty_window() {
        let heights = CumulativeHeights::new(&[]);
        let w = render_window(&heights, 500.0, 0.0, 2);
        assert!(w.is_empty());
        assert_eq!(w.pad_top, 0.0);
        assert_eq!(w.pad_bottom, 0.0);
    }

    // --- The focus/navigation interaction #350 flags as the risky part ---

    #[test]
    fn a_line_already_on_screen_needs_no_scroll() {
        let heights = CumulativeHeights::new(&vec![LH; 100]);
        // Viewport 0..200 shows lines 0..10.
        assert_eq!(scroll_to_reveal(&heights, 5, 200.0, 0.0), None);
    }

    #[test]
    fn a_hunk_below_the_window_scrolls_just_far_enough_to_show_it() {
        let heights = CumulativeHeights::new(&vec![LH; 100]);
        // Line 20 spans 400..420; a 200px viewport at 0 must scroll so the
        // line's bottom edge (420) meets the viewport bottom.
        assert_eq!(scroll_to_reveal(&heights, 20, 200.0, 0.0), Some(220.0));
    }

    #[test]
    fn a_hunk_above_the_window_scrolls_up_to_its_top_edge() {
        let heights = CumulativeHeights::new(&vec![LH; 100]);
        // Scrolled to 1000 (lines 50..60 visible); line 10 starts at 200.
        assert_eq!(scroll_to_reveal(&heights, 10, 200.0, 1000.0), Some(200.0));
    }

    #[test]
    fn revealing_a_hunk_actually_puts_it_in_the_rendered_window() {
        // The property that matters end to end: whatever scroll_to_reveal
        // returns must make render_window include that line. Without this,
        // keyboard navigation focuses an element that is not in the DOM —
        // the exact regression #350 names.
        let heights = CumulativeHeights::new(&vec![LH; 500]);
        let viewport = 200.0;
        for &target in &[0usize, 1, 37, 250, 498, 499] {
            let scroll = scroll_to_reveal(&heights, target, viewport, 0.0).unwrap_or(0.0);
            let w = render_window(&heights, viewport, scroll, 0);
            assert!(
                w.contains(target),
                "line {target} not in window {}..{} after scrolling to {scroll}",
                w.start,
                w.end
            );
        }
    }

    #[test]
    fn revealing_works_with_wrapped_lines_of_uneven_height() {
        // The uneven-height case is where a uniform-height shortcut would
        // silently break: line 3 is 4 rows tall, so every later offset is
        // shifted and a naive index*line_height calculation lands wrong.
        let mut hs = vec![LH; 20];
        hs[3] = 4.0 * LH;
        let heights = CumulativeHeights::new(&hs);
        let scroll = scroll_to_reveal(&heights, 10, 100.0, 0.0).unwrap();
        let w = render_window(&heights, 100.0, scroll, 0);
        assert!(w.contains(10), "wrapped-height offsets not respected");
    }

    #[test]
    fn a_line_index_past_the_end_is_not_scrolled_to() {
        let heights = CumulativeHeights::new(&vec![LH; 10]);
        assert_eq!(scroll_to_reveal(&heights, 10, 200.0, 0.0), None);
        assert_eq!(scroll_to_reveal(&heights, 999, 200.0, 0.0), None);
    }
}
