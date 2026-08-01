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
//! **Pencil-specific surface deliberately left for a follow-up issue.** This
//! module's line-level API (`toggle_line`, `HunkSelection::Lines`,
//! `to_patch_plan`'s `Lines` branch) is implemented and host-tested here
//! because it is cheap, pure, and the wire format ([`HunkLines`]) already
//! exists from #214 — but no DOM wiring calls `toggle_line` in this issue.
//! A real per-line UI (visible per-line tap targets in the rendered patch,
//! hover/press affordances distinguishing "this needs a stylus" from "this
//! is a dead zone for a finger", and pen-vs-touch-gated visibility) is a
//! second, substantial design surface of its own — building it without a
//! device in the loop to check that a 17px-tall inline target is even
//! reachable with a Pencil would be guessing, not implementing. Per the
//! issue's own explicit permission, that is split into a follow-up issue
//! rather than guessed at here. What ships in this issue is finger (and
//! keyboard) hunk-level selection, end to end, wired to preview/apply.

use std::collections::{BTreeMap, BTreeSet};

use git_vista_protocol::plan::{GenerationToken, RepositoryToken, WorktreeToken};
use git_vista_protocol::{
    FileSelection, HunkLines, HunkRef, PatchPlan, SelectionShape, StageDirection,
};

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

/// The staging selection: which files, which hunks, and (deliberately
/// unwired to any tap target in this issue — see the module doc) which
/// lines within a hunk are selected right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffSelection {
    files: BTreeMap<String, FileSel>,
}

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
    /// [`HunkLines`]'s coordinate space) within `hunk` of `path`. See the
    /// module doc: not wired to any tap target in this issue, kept pure and
    /// tested for the follow-up Pencil-surface issue to build on.
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

    /// Build the wire [`PatchPlan`] for this selection. `None` when nothing
    /// is selected (a plan with no files is malformed — [`PatchPlan::validate`]
    /// would reject it, so this returns `None` up front rather than handing
    /// the caller something certain to fail).
    ///
    /// **Mixed hunk/line files.** A file whose selected hunks are entirely
    /// whole serializes as [`SelectionShape::Hunks`]; a file with at least
    /// one line-narrowed hunk serializes as [`SelectionShape::Lines`] — and
    /// in that second case, any *whole*-selected hunks in the same file are
    /// dropped from the plan (documented here, and pinned by this module's
    /// tests): the wire format has one shape per file, not a mix, and since
    /// no caller in this issue can produce a line-narrowed hunk in the first
    /// place (see the module doc), this branch exists for the follow-up
    /// issue to resolve properly — most likely by expressing a whole hunk as
    /// `Lines` over its full line set once that caller has the parsed patch
    /// in hand to enumerate it.
    pub fn to_patch_plan(
        &self,
        repository: RepositoryToken,
        worktree: WorktreeToken,
        generation: GenerationToken,
        direction: StageDirection,
    ) -> Option<PatchPlan> {
        let mut files = Vec::new();
        for (path, sel) in &self.files {
            if sel.hunks.is_empty() {
                continue;
            }
            let has_lines = sel.hunks.values().any(|e| e.lines.is_some());
            let selection = if has_lines {
                let hunks: Vec<HunkLines> = sel
                    .hunks
                    .values()
                    .filter_map(|e| {
                        e.lines.as_ref().map(|lines| HunkLines {
                            hunk: e.anchor,
                            lines: lines.iter().copied().collect(),
                        })
                    })
                    .collect();
                if hunks.is_empty() {
                    continue;
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
            return None;
        }
        Some(PatchPlan {
            repository,
            worktree,
            generation,
            direction,
            files,
        })
    }
}

/// Pure range computation for a hunk-granularity drag-select gesture (Task
/// 3, #215): given the flat position (matching
/// [`crate::features::a11y::focus::GraphFocus`]'s `active` index into the
/// patch's navigable-hunk ordering — the same index space
/// [`super::core::hunk_nav`]/[`super::core::selectable_hunks`] enumerate)
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
        assert_eq!(sel.to_patch_plan(r, w, g, StageDirection::Stage), None);
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
            .to_patch_plan(r.clone(), w.clone(), g.clone(), StageDirection::Unstage)
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
        let plan = sel.to_patch_plan(r, w, g, StageDirection::Stage).unwrap();
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

    #[test]
    fn to_patch_plan_drops_whole_hunks_mixed_into_a_line_selected_file() {
        // Documented simplification (module doc): no caller in this issue can
        // produce this state (line selection is unwired), but the pure
        // module's behaviour is still pinned rather than left to guess.
        let mut sel = DiffSelection::new();
        sel.toggle_hunk("a.rs", href(0));
        sel.toggle_line("a.rs", href(2), 1);
        let (r, w, g) = tokens();
        let plan = sel.to_patch_plan(r, w, g, StageDirection::Stage).unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].selection,
            SelectionShape::Lines {
                hunks: vec![HunkLines {
                    hunk: href(2),
                    lines: vec![1],
                }]
            },
            "the whole-hunk selection for hunk 0 was dropped, not smuggled into Lines"
        );
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
}
