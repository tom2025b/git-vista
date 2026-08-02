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
}
