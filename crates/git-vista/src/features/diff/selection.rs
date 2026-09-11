//! Pure hunk/line selection state for staging (M2.17d, #215).
//!
//! Mirrors [`crate::features::a11y::focus::GraphFocus`] on purpose: a plain,
//! DOM-free state machine that a caller drives with small, named transitions
//! (`toggle_hunk`, `toggle_line`, `clear`, `select_all_in_hunk`) and that
//! `cargo test` can reach directly, with the DOM wiring (`staging_view.rs`,
//! wasm-only) staying thin — checkbox click → one method call, and a
//! reactive read of `is_hunk_selected`/`is_line_selected` for whether a
//! checkbox is drawn checked.
//!
//! ## Design decisions carried over from Task 1 of #215
//!
//! **Granularity split (finger vs Pencil).** A per-line tap target cannot
//! realistically meet the #65 44px minimum without visually disrupting the
//! diff — `.detail-diff` runs `font-size: 0.78rem; line-height: 1.45`,
//! roughly 17 CSS px per rendered line, and turning every line into a 44px
//! band would triple the patch's vertical footprint and make a hunk read as
//! a wall of padding rather than code. So finger selection is naturally
//! **hunk-granularity**, reusing the existing 44px `.diff-hunk` header band
//! (M2.16e) as the selection tap target — no new geometry to invent, and it
//! stays consistent with what a finger already taps today (roving focus).
//! Line-level selection is addressable by this module (`toggle_line`,
//! `HunkSelection::Lines`) because ADR 0011 already treats `"pen"` as a
//! *precise* pointer type, grouped with mouse (4px slop) rather than touch
//! (12px). Whether 44px SHOULD still gate pen input: **no** — ADR 0011's own
//! precision distinction is precisely the argument that a stylus does not
//! need a touch-sized target to be reliably hit, and holding pen input to the
//! same 44px floor as an undifferentiated finger tap would waste the exact
//! precision ADR 0011 exists to recognise.
//!
//! **Mode: always-on, not a separate selection mode.** Selection is layered
//! orthogonally on top of #210's existing roving-tabindex hunk navigation,
//! not a distinct "enter selection mode" screen. A tap on a hunk header
//! continues to mean exactly what #210 already documented it means (move the
//! roving position there) — selection gets its **own** tap target (a
//! checkbox drawn beside the header, `staging_view.rs`), so one tap never
//! has to mean two things. This was the deciding factor over overloading the
//! header's own click handler: a control that both moves focus and toggles
//! selection on the same gesture is a well-known source of "did that select
//! it or did it just move my cursor" confusion, and #210's tap-to-focus
//! contract predates this issue and should not change shape to accommodate
//! it.
//!
//! **Keyboard/VoiceOver equivalence.** `Space`/`Enter` on the currently
//! roving-focused hunk toggles its selection (wired in `staging_view.rs`'s
//! own `on_keydown`, alongside the arrow/Home/End/Escape handling #210
//! already has) — the same "whatever a tap can reach, a keyboard press
//! reaches too" contract #210 set for navigation, extended to selection.
//!
//! **Pencil-specific surface deliberately left for a follow-up issue** (this
//! paragraph described Task 1 of #215; #357 below is what changed since).
//! This module's line-level API (`toggle_line`, `HunkSelection::Lines`,
//! `to_patch_plan`'s `Lines` branch) was implemented and host-tested here
//! because it is cheap, pure, and the wire format ([`HunkLines`]) already
//! existed from #214 — but #215 wired no DOM consumer to it. A real per-line
//! UI (visible per-line tap targets in the rendered patch, hover/press
//! affordances distinguishing "this needs a stylus" from "this is a dead
//! zone for a finger", and pen-vs-touch-gated visibility) is a second,
//! substantial design surface of its own — building it without a device in
//! the loop to check that a 17px-tall inline target is even reachable with a
//! Pencil would be guessing, not implementing. Per #215's own explicit
//! permission, that surface was split into a follow-up issue rather than
//! guessed at then — #357 is that follow-up.
//!
//! **What #357 wired, and what it didn't.** `staging_view.rs` now calls
//! `toggle_line` from a per-line mouse/pointer checkbox (mirroring the
//! existing hunk checkbox and `blame_row`'s own per-row select target — a
//! precise pointer, mouse or pen alike per ADR 0011, needs no larger target
//! than that) and calls `select_all_in_hunk` from Shift+Enter/Space on the
//! roving-focused hunk header (mirroring `blame_row`'s own "Shift changes
//! what the roving key means" idiom, not a new one). What #357 did **not**
//! wire, because it remained genuinely open rather than becoming answerable
//! by inspection: touch/Pencil-specific affordances (still needs a device in
//! the loop), and keyboard access to one arbitrary line by itself (as
//! opposed to "every changed line in this hunk") — that would need its own
//! roving focus nested inside the hunk-level one #210 already owns, which is
//! a design surface in its own right, not a wiring gap. See #357's own
//! follow-up issue for both.

use std::collections::{BTreeMap, BTreeSet};

use git_vista_protocol::plan::{GenerationToken, RepositoryToken, WorktreeToken};
use git_vista_protocol::{
    FileSelection, HunkLines, HunkRef, PatchPlan, SelectionShape, StageDirection,
};

use crate::features::a11y::focus::FocusMove;

use super::core::CompleteHunkLines;

/// One file's selection state: each selected hunk, either whole or narrowed
/// to specific lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FileSel {
    /// Ordinal → this hunk's selection. A hunk with no entry is unselected.
    hunks: BTreeMap<u32, HunkEntry>,
}

/// A single hunk's selection entry: its wire anchor (needed to build a
/// [`HunkRef`] without re-deriving it from the patch text at serialize time)
/// plus whether the whole hunk is selected or only specific lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkEntry {
    anchor: HunkRef,
    /// `None` = the whole hunk is selected. `Some(lines)` = only these
    /// 0-based indices into the hunk's parsed `Hunk::lines` (see
    /// [`HunkLines`]'s doc — this is *not* the raw-text line-index space
    /// [`super::core::selectable_hunks`] uses).
    lines: Option<BTreeSet<u32>>,
}

/// The staging selection: which files, hunks, and individual lines are selected.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffSelection {
    files: BTreeMap<String, FileSel>,
}

/// A whole hunk cannot be expanded without its complete changed-line set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteHunk {
    pub path: String,
    pub hunk: HunkRef,
}

impl std::fmt::Display for IncompleteHunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cannot include the whole hunk {} in {}: the displayed patch does not \
             contain its complete changed lines. Deselect that hunk or select \
             individual visible lines.",
            u64::from(self.hunk.index) + 1,
            self.path,
        )
    }
}

impl std::error::Error for IncompleteHunk {}

impl DiffSelection {
    /// A fresh, empty selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing is selected — the "Stage selected" action should be disabled.
    pub fn is_empty(&self) -> bool {
        self.files.values().all(|f| f.hunks.is_empty())
    }

    /// Drop every selection, in every file — the "Clear" affordance, and
    /// what a fresh `StagingDiff` fetch should call first (a selection made
    /// against a since-replaced diff addresses hunks that may no longer
    /// exist at those ordinals).
    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// Toggle whole-hunk selection for `hunk` in `path`. Selecting a hunk
    /// that was previously selected only by specific lines replaces that
    /// partial selection with the whole hunk (the natural "widen" reading of
    /// tapping the same finger-level control again); toggling an already
    /// wholly-selected hunk deselects it entirely.
    pub fn toggle_hunk(&mut self, path: &str, hunk: HunkRef) {
        let file = self.files.entry(path.to_string()).or_default();
        match file.hunks.get(&hunk.index) {
            Some(HunkEntry { lines: None, .. }) => {
                file.hunks.remove(&hunk.index);
            }
            _ => {
                file.hunks.insert(
                    hunk.index,
                    HunkEntry {
                        anchor: hunk,
                        lines: None,
                    },
                );
            }
        }
        Self::prune_empty_file(&mut self.files, path);
    }

    /// Deterministically select or deselect the *whole* hunk `hunk` in
    /// `path` — unlike [`Self::toggle_hunk`], idempotent: setting `true`
    /// twice in a row leaves it selected, it does not flip back off. This is
    /// what a drag-select gesture wants (Task 3, #215): as the pointer
    /// sweeps back across a hunk it already passed, the hunk must *stay*
    /// selected, not flicker — a `toggle` per pointer-move would do exactly
    /// that on any re-entry, which `toggle_hunk`'s tap-driven contract
    /// neither needs nor should have.
    pub fn set_hunk_selected(&mut self, path: &str, hunk: HunkRef, selected: bool) {
        if selected {
            let file = self.files.entry(path.to_string()).or_default();
            file.hunks.insert(
                hunk.index,
                HunkEntry {
                    anchor: hunk,
                    lines: None,
                },
            );
        } else if let Some(file) = self.files.get_mut(path) {
            file.hunks.remove(&hunk.index);
            Self::prune_empty_file(&mut self.files, path);
        }
    }

    /// Whether `hunk`'s whole selection band should render "checked" —
    /// `true` only for a whole-hunk selection, never a partial-lines one (a
    /// hunk narrowed to some lines is not "the whole hunk is selected").
    pub fn is_hunk_selected(&self, path: &str, index: u32) -> bool {
        matches!(
            self.files.get(path).and_then(|f| f.hunks.get(&index)),
            Some(HunkEntry { lines: None, .. })
        )
    }

    /// Toggle one line (an index into the hunk's parsed `Hunk::lines`,
    /// [`HunkLines`]'s coordinate space) within `hunk` of `path`. The
    /// staging view's per-line checkbox drives this transition (#357).
    ///
    /// A hunk that was previously whole-selected is narrowed to exactly
    /// `line` — toggling a specific line is the user saying "not the whole
    /// hunk, just this", so a prior whole-hunk selection does not survive as
    /// an implicit "all lines" set (this module has no way to enumerate a
    /// hunk's lines on its own; only the caller, holding the parsed patch,
    /// knows them — see [`Self::select_all_in_hunk`] for the explicit
    /// version of "all").
    pub fn toggle_line(&mut self, path: &str, hunk: HunkRef, line: u32) {
        let file = self.files.entry(path.to_string()).or_default();
        let entry = file.hunks.entry(hunk.index).or_insert_with(|| HunkEntry {
            anchor: hunk,
            lines: Some(BTreeSet::new()),
        });
        entry.anchor = hunk;
        let lines = entry.lines.get_or_insert_with(BTreeSet::new);
        if !lines.remove(&line) {
            lines.insert(line);
        }
        if lines.is_empty() {
            file.hunks.remove(&hunk.index);
        }
        Self::prune_empty_file(&mut self.files, path);
    }

    /// Whether `line` (in `hunk`'s `Hunk::lines` space) is currently
    /// selected — `true` for an explicit line selection; a whole-hunk
    /// selection does not imply every individual line is "selected" in this
    /// query, since nothing here knows how many lines the hunk has.
    pub fn is_line_selected(&self, path: &str, hunk_index: u32, line: u32) -> bool {
        self.files
            .get(path)
            .and_then(|f| f.hunks.get(&hunk_index))
            .and_then(|e| e.lines.as_ref())
            .is_some_and(|lines| lines.contains(&line))
    }

    /// Select exactly `lines` within `hunk` of `path` — the explicit
    /// "select every addable/removable line" transition, for a caller that
    /// knows the hunk's full line set (the parsed `Hunk::lines`). A no-op
    /// (clears any existing selection for the hunk) if `lines` is empty.
    pub fn select_all_in_hunk(
        &mut self,
        path: &str,
        hunk: HunkRef,
        lines: impl IntoIterator<Item = u32>,
    ) {
        let set: BTreeSet<u32> = lines.into_iter().collect();
        let file = self.files.entry(path.to_string()).or_default();
        if set.is_empty() {
            file.hunks.remove(&hunk.index);
        } else {
            file.hunks.insert(
                hunk.index,
                HunkEntry {
                    anchor: hunk,
                    lines: Some(set),
                },
            );
        }
        Self::prune_empty_file(&mut self.files, path);
    }

    fn prune_empty_file(files: &mut BTreeMap<String, FileSel>, path: &str) {
        if files.get(path).is_some_and(|f| f.hunks.is_empty()) {
            files.remove(path);
        }
    }

    /// Build the wire [`PatchPlan`] for this selection. `Ok(None)` when nothing
    /// is selected (a plan with no files is malformed — [`PatchPlan::validate`]
    /// would reject it, so this returns `Ok(None)` up front rather than handing
    /// the caller something certain to fail).
    ///
    /// **Mixed hunk/line files.** A file whose selected hunks are entirely
    /// whole serializes as [`SelectionShape::Hunks`]; a file with at least
    /// one line-narrowed hunk serializes as [`SelectionShape::Lines`] — and
    /// in that second case, whole-selected hunks expand to their complete
    /// changed-line sets from the caller's patch. Missing or incomplete hunks
    /// refuse the entire plan with an error; a visible subset cannot stand in
    /// for a whole hunk. Serialization never narrows the UI selection state.
    pub fn to_patch_plan(
        &self,
        repository: RepositoryToken,
        worktree: WorktreeToken,
        generation: GenerationToken,
        direction: StageDirection,
        whole_hunk_lines: &CompleteHunkLines,
    ) -> Result<Option<PatchPlan>, IncompleteHunk> {
        let mut files = Vec::new();
        for (path, sel) in &self.files {
            if sel.hunks.is_empty() {
                continue;
            }
            let has_lines = sel.hunks.values().any(|e| e.lines.is_some());
            let selection = if has_lines {
                let mut hunks = Vec::new();
                for e in sel.hunks.values() {
                    let lines = match &e.lines {
                        Some(lines) => lines.iter().copied().collect(),
                        None => whole_hunk_lines
                            .get(path, e.anchor)
                            .filter(|lines| !lines.is_empty())
                            .ok_or_else(|| IncompleteHunk {
                                path: path.clone(),
                                hunk: e.anchor,
                            })?
                            .to_vec(),
                    };
                    hunks.push(HunkLines {
                        hunk: e.anchor,
                        lines,
                    });
                }
                SelectionShape::Lines { hunks }
            } else {
                SelectionShape::Hunks {
                    hunks: sel.hunks.values().map(|e| e.anchor).collect(),
                }
            };
            files.push(FileSelection {
                path: path.clone(),
                selection,
            });
        }
        if files.is_empty() {
            return Ok(None);
        }
        Ok(Some(PatchPlan {
            repository,
            worktree,
            generation,
            direction,
            files,
        }))
    }
}

/// Pure range computation for a hunk-granularity drag-select gesture (Task
/// 3, #215): given the flat position (matching
/// [`crate::features::a11y::focus::GraphFocus`]'s `active` index into the
/// patch's navigable-hunk ordering — the same index space
/// [`super::core::selectable_hunks`] enumerates)
/// where the drag started and where the pointer is now, the inclusive range
/// of flat indices the drag currently covers. Order-independent — dragging
/// upward or downward both produce the same range, ascending.
///
/// Kept separate from the pointer-event glue (`staging_view.rs`) per the
/// issue's own instruction: this is the part `cargo test` can reach.
pub fn drag_range(start: usize, current: usize) -> std::ops::RangeInclusive<usize> {
    if start <= current {
        start..=current
    } else {
        current..=start
    }
}

/// Keyboard access to one changed line inside a hunk, without taking arrow
/// keys away from #210's header-level [`GraphFocus`] (#770).
///
/// ## The design question #770 named, and the answer this type is
///
/// #357 gave every hunk header a Shift+Enter/Space shortcut
/// (`select_all_in_hunk`) but left plain keyboard access to *one specific
/// line* unwired, precisely because it "would need its own roving focus
/// nested inside the hunk-level one #210 already owns" (see this module's
/// top-level doc and `staging_view.rs`'s). Both the hunk-header list and a
/// hunk's own changed-line list are naturally arrow-key-navigated collections,
/// and only one input event stream exists to drive both.
///
/// `gv-tui`'s `panes/staging.rs` (the issue's own named prior art) sidesteps
/// this by *flattening* file/hunk/line into a single list with one cursor —
/// exactly right for a terminal UI where the whole pane is one column and
/// every row performs the same "act on this row now" gesture immediately.
/// The web staging view does not share that shape: hunk headers page through
/// this whole diff at a stable elevation while lines are nested markup
/// *inside* each header's own patch block, `select_all_in_hunk` already
/// treats "the hunk" and "the changed lines in the hunk" as two distinct
/// granularities worth their own affordances (not one flattened list a user
/// arrows straight through), and flattening would also turn *every* context
/// line into a stop a keyboard user has to tab past to reach the next hunk
/// header — a regression #210 never had.
///
/// So this type takes the **nested focus scope** resolution instead, the one
/// `acceptance.arrow_key_sharing_resolved` names: ArrowUp/ArrowDown always
/// belong to exactly one active scope at a time, never both, so the two
/// schemes never fight over the same keypress. `ArrowRight` on a hunk header
/// drills into that hunk's line scope (mirrors the WAI-ARIA treegrid pattern:
/// Right expands/enters a nested level, Left collapses/exits it); while
/// engaged, ArrowUp/ArrowDown/Home/End move `LineFocus`'s own cursor among
/// that hunk's changed lines only (`GraphFocus` does not move, is not even
/// consulted); `ArrowLeft` or `Escape` exits back to the header, which
/// resumes exactly where #210 left it, never reset. Enter/Space acts on
/// whichever scope currently holds the arrows: a bare header press still
/// means `toggle_hunk` (unchanged from #357), a press while a line is
/// drilled into means `toggle_line` on that one line — genuinely distinct
/// from both `toggle_hunk` and Shift+Activate's whole-hunk
/// `select_all_in_hunk`.
///
/// ## Why a separate type instead of extending [`GraphFocus`]
///
/// `GraphFocus` is deliberately a flat, single-scope model (see its own doc)
/// reused identically by the canvas, blame rows, and the hunk-header list
/// itself — bolting a second, hunk-scoped cursor onto it would make every
/// other caller carry state that means nothing to them. `LineFocus` is
/// standalone, holds only the hunk index it is drilled into (`None` when not
/// engaged) plus a line cursor, and a caller drives it with the same
/// [`FocusMove`] vocabulary `GraphFocus::mv` already uses — so `Home`/`End`
/// land where a caller of that type already expects, without this type
/// needing to know anything about rows, DOM, or wasm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineFocus {
    /// The flat hunk index (`GraphFocus`'s own index space, matching
    /// `hunks_by_flat_idx` in `staging_view.rs`) currently drilled into.
    /// `None` means arrow keys belong entirely to the header-level roving
    /// focus — the default, and where `exit` returns to.
    hunk: Option<usize>,
    /// 0-based index into that hunk's *changed-line* list — the same
    /// coordinate space `select_all_in_hunk`'s caller already builds
    /// (`changed_by_hunk` in `staging_view.rs`), not a raw patch-line offset
    /// and not `HunkLines`' own line-index space either.
    line: usize,
}

impl LineFocus {
    /// Not drilled into any hunk yet — the state every fresh staging view
    /// starts in, same as `GraphFocus::new`'s `engaged: false`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a line, rather than the header list, currently owns arrow
    /// keys.
    pub fn is_engaged(&self) -> bool {
        self.hunk.is_some()
    }

    /// The `(hunk_index, line_index)` currently focused, or `None` when not
    /// drilled into any hunk.
    pub fn active(&self) -> Option<(usize, usize)> {
        self.hunk.map(|h| (h, self.line))
    }

    /// `ArrowRight` on hunk `hunk`'s header: drill into its line scope,
    /// landing on its first changed line. Refuses to engage (returns
    /// `false`, leaves state untouched) when `changed_len == 0` — a hunk
    /// with nothing addable/removable (pure-context, or the parser found
    /// nothing) has no line for this scope to represent, and `active()`
    /// returning `Some` with no real line to point at would be a lie a
    /// caller could try to render.
    pub fn enter(&mut self, hunk: usize, changed_len: usize) -> bool {
        self.enter_at(hunk, 0, changed_len)
    }

    /// A pointer landing on a changed-line checkbox enters at that exact
    /// ordinal. Invalid ordinals (including an empty hunk) leave state alone.
    pub fn enter_at(&mut self, hunk: usize, ordinal: usize, changed_len: usize) -> bool {
        if ordinal >= changed_len {
            return false;
        }
        self.hunk = Some(hunk);
        self.line = ordinal;
        true
    }

    /// `ArrowLeft` or `Escape` while engaged: hand arrow keys back to the
    /// header-level roving focus. That focus is a separate `GraphFocus`
    /// this type never touches, so it resumes at whatever hunk it already
    /// held — nothing here moves it or needs to know where it is.
    pub fn exit(&mut self) {
        self.hunk = None;
        self.line = 0;
    }

    /// Move the line cursor within the currently-drilled-into hunk's
    /// `changed_len` lines. Clamped at both ends, never wraps — the same
    /// end-of-list policy `GraphFocus::mv` uses, so a caller already
    /// familiar with that type's `Home`/`End` behaviour gets the same shape
    /// here. A no-op returning `None` when nothing is engaged (arrow keys
    /// at the header level are not this type's concern). Exits and returns
    /// `None` when the hunk has no changed lines (defensive: a caller should not
    /// reach this with `changed_len == 0` while engaged, since `enter`
    /// refuses to engage on an empty hunk, but a stale caller passing a
    /// smaller `changed_len` than it entered with — e.g. after a diff
    /// refresh — must not panic or leave `line` pointing past the end).
    pub fn mv(&mut self, dir: FocusMove, changed_len: usize) -> Option<usize> {
        self.hunk?;
        if changed_len == 0 {
            self.exit();
            return None;
        }
        let last = changed_len - 1;
        self.line = match dir {
            FocusMove::Prev => self.line.saturating_sub(1),
            FocusMove::Next => (self.line + 1).min(last),
            FocusMove::First => 0,
            FocusMove::Last => last,
        }
        .min(last);
        Some(self.line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn href(index: u32) -> HunkRef {
        HunkRef {
            index,
            old_start: 10 * (index + 1),
            new_start: 10 * (index + 1),
        }
    }

    fn tokens() -> (RepositoryToken, WorktreeToken, GenerationToken) {
        (
            RepositoryToken::new("repo-1").unwrap(),
            WorktreeToken::new("wt-1").unwrap(),
            GenerationToken::new("diff-v1:1").unwrap(),
        )
    }

    #[test]
    fn a_fresh_selection_is_empty_and_serializes_to_none() {
        let sel = DiffSelection::new();
        assert!(sel.is_empty());
        let (r, w, g) = tokens();
        assert_eq!(
            sel.to_patch_plan(
                r,
                w,
                g,
                StageDirection::Stage,
                &CompleteHunkLines::default()
            ),
            Ok(None)
        );
    }

    #[test]
    fn toggling_a_hunk_selects_then_deselects_it() {
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        assert!(sel.is_hunk_selected("a.rs", 0));
        assert!(!sel.is_empty());
        sel.toggle_hunk("a.rs", href(0));
        assert!(!sel.is_hunk_selected("a.rs", 0));
        assert!(
            sel.is_empty(),
            "toggling off the only hunk empties the file too"
        );
    }

    #[test]
    fn toggling_a_second_hunk_in_the_same_file_does_not_disturb_the_first() {
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        sel.toggle_hunk("a.rs", href(2));
        assert!(sel.is_hunk_selected("a.rs", 0));
        assert!(sel.is_hunk_selected("a.rs", 2));
        sel.toggle_hunk("a.rs", href(0));
        assert!(!sel.is_hunk_selected("a.rs", 0));
        assert!(sel.is_hunk_selected("a.rs", 2), "the other hunk survives");
    }

    #[test]
    fn clear_empties_every_file() {
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        sel.toggle_hunk("b.rs", href(1));
        sel.clear();
        assert!(sel.is_empty());
        assert!(!sel.is_hunk_selected("a.rs", 0));
        assert!(!sel.is_hunk_selected("b.rs", 1));
    }

    #[test]
    fn toggle_line_selects_and_deselects_a_specific_line() {
        let mut sel = DiffSelection::new();
        sel.toggle_line("a.rs", href(0), 3);
        assert!(sel.is_line_selected("a.rs", 0, 3));
        assert!(!sel.is_line_selected("a.rs", 0, 4));
        assert!(
            !sel.is_hunk_selected("a.rs", 0),
            "a partial selection is not a whole-hunk one"
        );
        sel.toggle_line("a.rs", href(0), 3);
        assert!(!sel.is_line_selected("a.rs", 0, 3));
        assert!(
            sel.is_empty(),
            "removing the only line empties the hunk and file"
        );
    }

    #[test]
    fn toggle_line_accumulates_multiple_lines_in_one_hunk() {
        let mut sel = DiffSelection::new();
        sel.toggle_line("a.rs", href(0), 1);
        sel.toggle_line("a.rs", href(0), 4);
        assert!(sel.is_line_selected("a.rs", 0, 1));
        assert!(sel.is_line_selected("a.rs", 0, 4));
    }

    #[test]
    fn toggling_a_hunk_then_narrowing_to_a_line_replaces_the_whole_selection() {
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        assert!(sel.is_hunk_selected("a.rs", 0));
        sel.toggle_line("a.rs", href(0), 2);
        assert!(
            !sel.is_hunk_selected("a.rs", 0),
            "no longer a whole-hunk selection"
        );
        assert!(sel.is_line_selected("a.rs", 0, 2));
    }

    #[test]
    fn toggling_the_hunk_of_a_line_selection_widens_it_to_whole() {
        let mut sel = DiffSelection::new();
        sel.toggle_line("a.rs", href(0), 2);
        sel.toggle_hunk("a.rs", href(0));
        assert!(sel.is_hunk_selected("a.rs", 0));
        assert!(
            !sel.is_line_selected("a.rs", 0, 2),
            "the line query no longer applies once the hunk is whole"
        );
    }

    #[test]
    fn select_all_in_hunk_sets_exactly_the_given_lines() {
        let mut sel = DiffSelection::new();
        sel.select_all_in_hunk("a.rs", href(0), [0, 2, 5]);
        assert!(sel.is_line_selected("a.rs", 0, 0));
        assert!(sel.is_line_selected("a.rs", 0, 2));
        assert!(sel.is_line_selected("a.rs", 0, 5));
        assert!(!sel.is_line_selected("a.rs", 0, 1));
        // An empty set clears the hunk rather than selecting nothing forever.
        sel.select_all_in_hunk("a.rs", href(0), []);
        assert!(sel.is_empty());
    }

    #[test]
    fn to_patch_plan_serializes_whole_hunk_selections() {
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        sel.toggle_hunk("a.rs", href(2));
        sel.toggle_hunk("b.rs", href(1));
        let (r, w, g) = tokens();
        let plan = sel
            .to_patch_plan(
                r.clone(),
                w.clone(),
                g.clone(),
                StageDirection::Unstage,
                &CompleteHunkLines::default(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(plan.repository, r);
        assert_eq!(plan.worktree, w);
        assert_eq!(plan.generation, g);
        assert_eq!(plan.direction, StageDirection::Unstage);
        assert_eq!(plan.files.len(), 2);
        let a = plan.files.iter().find(|f| f.path == "a.rs").unwrap();
        assert_eq!(
            a.selection,
            SelectionShape::Hunks {
                hunks: vec![href(0), href(2)]
            }
        );
        let b = plan.files.iter().find(|f| f.path == "b.rs").unwrap();
        assert_eq!(
            b.selection,
            SelectionShape::Hunks {
                hunks: vec![href(1)]
            }
        );
        // The plan this module builds must itself validate — a selection this
        // module can produce should never be structurally rejected by the
        // exact same rules the server enforces (`PatchPlan::validate`).
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn to_patch_plan_serializes_line_selections() {
        let mut sel = DiffSelection::new();
        sel.toggle_line("a.rs", href(0), 1);
        sel.toggle_line("a.rs", href(0), 3);
        let (r, w, g) = tokens();
        let plan = sel
            .to_patch_plan(
                r,
                w,
                g,
                StageDirection::Stage,
                &CompleteHunkLines::default(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].selection,
            SelectionShape::Lines {
                hunks: vec![HunkLines {
                    hunk: href(0),
                    lines: vec![1, 3],
                }]
            }
        );
        assert_eq!(plan.validate(), Ok(()));
    }

    const MIXED_PATCH: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -10,4 +10,4 @@
 lead
-old red
+new red
 middle
-old blue
+new blue
\\ No newline at end of file
@@ -20,3 +20,3 @@
 before second
-old second
+new second
 after second
@@ -30,2 +30,2 @@
 context
-cut here
";

    #[test]
    fn to_patch_plan_preserves_every_changed_line_of_a_mixed_whole_hunk() {
        // #808 retires the old drop expectation: #357/#771/#781 wired
        // toggle_line into production, so a whole-hunk click followed by a
        // line click in a different hunk now reaches this state by mouse.
        // The trailing incomplete hunk is unselected: completeness must be
        // per hunk, not a global patch-truncated flag.
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        sel.toggle_line("a.rs", href(1), 1);
        sel.toggle_hunk("b.rs", href(0));
        let original = sel.clone();
        let whole_lines = CompleteHunkLines::from_patch(MIXED_PATCH);
        for direction in [StageDirection::Stage, StageDirection::Unstage] {
            let (r, w, g) = tokens();
            let plan = sel
                .to_patch_plan(r, w, g, direction, &whole_lines)
                .unwrap()
                .unwrap();
            assert_eq!(plan.direction, direction);
            assert_eq!(
                plan.files,
                vec![
                    FileSelection {
                        path: "a.rs".into(),
                        selection: SelectionShape::Lines {
                            hunks: vec![
                                HunkLines {
                                    hunk: href(0),
                                    lines: vec![1, 2, 4, 5],
                                },
                                HunkLines {
                                    hunk: href(1),
                                    lines: vec![1],
                                },
                            ],
                        },
                    },
                    FileSelection {
                        path: "b.rs".into(),
                        selection: SelectionShape::Hunks {
                            hunks: vec![href(0)],
                        },
                    },
                ],
                "every changed line of the whole hunk and the explicit subset must survive"
            );
            assert_eq!(plan.validate(), Ok(()));
            assert_eq!(sel, original, "serialization must not narrow UI state");
        }
    }

    #[test]
    fn to_patch_plan_refuses_incomplete_missing_or_stale_whole_hunks() {
        let whole_lines = CompleteHunkLines::from_patch(MIXED_PATCH);
        let stale = HunkRef {
            old_start: 999,
            ..href(0)
        };
        for whole in [href(2), href(3), stale] {
            for direction in [StageDirection::Stage, StageDirection::Unstage] {
                let mut sel = DiffSelection::new();
                sel.toggle_line("a.rs", href(1), 1);
                sel.toggle_hunk("a.rs", whole);
                let (r, w, g) = tokens();
                let error = sel
                    .to_patch_plan(r, w, g, direction, &whole_lines)
                    .unwrap_err();
                assert_eq!(
                    error,
                    IncompleteHunk {
                        path: "a.rs".into(),
                        hunk: whole
                    }
                );
                assert!(error.to_string().contains("complete changed lines"));
                assert!(error.to_string().contains("a.rs"));
                assert!(sel.is_hunk_selected("a.rs", whole.index));
            }
        }
    }

    #[test]
    fn set_hunk_selected_is_idempotent_unlike_toggle() {
        let mut sel = DiffSelection::new();
        sel.set_hunk_selected("a.rs", href(0), true);
        sel.set_hunk_selected("a.rs", href(0), true);
        assert!(
            sel.is_hunk_selected("a.rs", 0),
            "still selected after a repeat `true`"
        );
        sel.set_hunk_selected("a.rs", href(0), false);
        assert!(!sel.is_hunk_selected("a.rs", 0));
        sel.set_hunk_selected("a.rs", href(0), false);
        assert!(
            sel.is_empty(),
            "a repeat `false` on nothing selected stays empty, not a panic"
        );
    }

    #[test]
    fn drag_range_is_order_independent_and_inclusive() {
        assert_eq!(drag_range(2, 5), 2..=5);
        assert_eq!(drag_range(5, 2), 2..=5);
        assert_eq!(drag_range(3, 3), 3..=3);
    }

    #[test]
    fn line_focus_starts_disengaged() {
        let f = LineFocus::new();
        assert!(!f.is_engaged());
        assert_eq!(f.active(), None);
    }

    #[test]
    fn line_focus_enter_lands_on_the_first_changed_line() {
        let mut f = LineFocus::new();
        assert!(f.enter(2, 4));
        assert!(f.is_engaged());
        assert_eq!(f.active(), Some((2, 0)));
    }

    #[test]
    fn line_focus_enter_refuses_a_hunk_with_no_changed_lines() {
        let mut f = LineFocus::new();
        assert!(!f.enter(2, 0));
        assert!(
            !f.is_engaged(),
            "a hunk with nothing addable/removable has no line for this \
             scope to point at"
        );
        assert_eq!(f.active(), None);
    }

    #[test]
    fn line_focus_pointer_entry_retargets_the_hunk_and_exact_ordinal() {
        let mut f = LineFocus::new();
        assert!(f.enter_at(2, 3, 5));
        assert_eq!(f.active(), Some((2, 3)));
        assert_eq!(f.mv(FocusMove::Prev, 5), Some(2));
        assert!(f.enter_at(7, 1, 3));
        assert_eq!(f.active(), Some((7, 1)));
        assert_eq!(f.mv(FocusMove::Next, 3), Some(2));
        assert!(!f.enter_at(8, 3, 3));
        assert!(!f.enter(8, 0));
        assert_eq!(f.active(), Some((7, 2)));
    }

    #[test]
    fn line_focus_move_exits_when_the_hunk_has_no_changed_lines() {
        let mut f = LineFocus::new();
        assert!(f.enter_at(2, 3, 5));
        assert_eq!(f.mv(FocusMove::Next, 0), None);
        assert!(!f.is_engaged());
        assert_eq!(f.active(), None);
    }

    #[test]
    fn line_focus_exit_returns_control_to_the_header_and_resets_the_cursor() {
        let mut f = LineFocus::new();
        f.enter(1, 3);
        f.mv(FocusMove::Next, 3);
        f.exit();
        assert!(!f.is_engaged());
        assert_eq!(f.active(), None);
        // Re-entering starts fresh at line 0, not wherever it was left.
        f.enter(1, 3);
        assert_eq!(f.active(), Some((1, 0)));
    }

    #[test]
    fn line_focus_mv_is_a_no_op_when_not_engaged() {
        let mut f = LineFocus::new();
        assert_eq!(f.mv(FocusMove::Next, 5), None);
        assert!(!f.is_engaged());
    }

    #[test]
    fn line_focus_mv_clamps_at_both_ends_without_wrapping() {
        let mut f = LineFocus::new();
        f.enter(0, 3);
        assert_eq!(f.active(), Some((0, 0)));
        // Prev at the start stays at the start — it must not wrap to the end.
        assert_eq!(f.mv(FocusMove::Prev, 3), Some(0));
        assert_eq!(f.mv(FocusMove::Next, 3), Some(1));
        assert_eq!(f.mv(FocusMove::Next, 3), Some(2));
        // Next at the end stays at the end — it must not wrap to the start.
        assert_eq!(f.mv(FocusMove::Next, 3), Some(2));
        assert_eq!(f.mv(FocusMove::First, 3), Some(0));
        assert_eq!(f.mv(FocusMove::Last, 3), Some(2));
    }

    #[test]
    fn line_focus_mv_does_not_move_the_header_scope() {
        // The whole point of the nested-scope design: entering a hunk's line
        // scope and moving within it must leave the *hunk index* untouched —
        // only the line cursor moves. If `mv` ever changed which hunk is
        // active, a caller re-reading `active().0` after a move would see
        // the wrong hunk, silently corrupting which patch the next
        // `toggle_line` call addresses.
        let mut f = LineFocus::new();
        f.enter(7, 4);
        f.mv(FocusMove::Next, 4);
        f.mv(FocusMove::Next, 4);
        assert_eq!(f.active(), Some((7, 2)), "hunk index must stay 7");
    }
}
