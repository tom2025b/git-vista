//! Pure grouping/sort/count logic and accessible-label data for the
//! working-tree status list (M2.15, #68d — the pure-logic slice).
//!
//! Framework-free, matching this project's `core.rs` convention
//! (`features/activity/core.rs` is the shortest example): no Leptos, no
//! signals, no `#[cfg(target_arch = "wasm32")]` gate, `cargo test`-able on
//! the host. **Deliberately not the whole of 68d** — rendering (touch
//! cards, the `/api/status/v2` resource fetch, real DOM `aria-*`
//! attributes) needs a place to render (`#65`'s shell) and is out of scope
//! here, the same way #69a deferred endpoint wiring and #69c deferred
//! `layout/**` wiring. [`StatusSections::from_worktree_status`] is the one
//! entry point a future view calls; everything else is its supporting
//! shape.
//!
//! ## Grouping decision
//!
//! Five sections, ordered by how urgently they need attention:
//! [`StatusSection::Conflicted`], [`StatusSection::Staged`],
//! [`StatusSection::Unstaged`], [`StatusSection::Untracked`],
//! [`StatusSection::Ignored`]. `renamed`, `submodule`, and `binary` are
//! **not** their own sections — [`StatusEntry`] already models them as
//! per-entry properties (a rename is a `StatusEntry::Renamed`, not a
//! separate axis from staged/unstaged; a submodule or binary flag rides
//! alongside whichever section the entry's change-state already puts it
//! in), so giving them sections of their own would model the same fact
//! twice.
//!
//! **A path dirty on both sides (`ChangeSides::Both`) appears in *both*
//! `Staged` and `Unstaged`, deliberately, not once.** That matches how git
//! itself reports it (two independent `<XY>` letters on one record) and how
//! every mainstream git UI (the two-column `git status` porcelain itself,
//! GitHub Desktop, VS Code's Source Control view) shows the same path in
//! both lists — the two states really are independent facts a user acts on
//! separately (stage the rest, or unstage what's already staged). A rename
//! with a further edit (`StatusEntry::Renamed { unstaged: Some(_), .. }`)
//! follows the same rule: it is always in `Staged` (a rename is staged by
//! construction — see [`StatusEntry::Renamed`]'s own doc comment) and *also*
//! in `Unstaged` when `unstaged` is `Some`.
//!
//! **Consequence for counts**: [`StatusSections`] counts are *section
//! memberships*, not unique paths — a doubly-dirty path counts once toward
//! `Staged`'s count and once toward `Unstaged`'s, which is what a
//! per-section header badge ("3 staged", "1 unstaged") actually wants to
//! show, even though it means the two counts can sum to more than the total
//! number of distinct paths.
//!
//! ## Sort order
//!
//! `git-status(1)` documents tracked entries as printed in an **undefined
//! order** — #68b's own module doc already established this for the parser
//! side. An accessible list that silently reorders itself between two reads
//! of an unchanged worktree is hostile to a screen-reader user tracking
//! position by index, so every section here is sorted by path, giving a
//! stable, deterministic order regardless of what order the entries arrived
//! in.
//!
//! ## Accessible labels
//!
//! Each row carries a human-readable `accessible_label` describing its
//! state **in words**, not just a glyph — #68's own acceptance criterion.
//! Each label states the change kind by name (`"Added"`, `"Modified"`, …)
//! even though the section it lives in already implies part of that, on the
//! reasoning that a screen-reader user navigating by list item (rather than
//! by heading) should get a self-sufficient description without having to
//! recall which section they scrolled into.

use git_vista_protocol::{
    ChangeKind, ChangeSides, ConflictKind, StatusEntry, SubmoduleState, WorktreeStatus,
};

/// Which section of the status list an entry contributes a row to. Ordered
/// (via `ALL`) by urgency: a conflict needs resolving before anything else
/// is actionable, staged/unstaged changes are the everyday case, untracked
/// and ignored are lowest-priority housekeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusSection {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
    Ignored,
}

impl StatusSection {
    /// Every section, in display order.
    pub const ALL: [StatusSection; 5] = [
        StatusSection::Conflicted,
        StatusSection::Staged,
        StatusSection::Unstaged,
        StatusSection::Untracked,
        StatusSection::Ignored,
    ];

    /// The section header text a future view would show.
    pub fn heading(self) -> &'static str {
        match self {
            StatusSection::Conflicted => "Conflicted",
            StatusSection::Staged => "Staged changes",
            StatusSection::Unstaged => "Unstaged changes",
            StatusSection::Untracked => "Untracked files",
            StatusSection::Ignored => "Ignored files",
        }
    }
}

/// One renderable row: the data a future touch-card view attaches to one
/// list item, key included so a `<For each=... key=...>` (or #69c's
/// virtualization primitive) has something stable to key on. `key` is
/// unique **within one section's row list**, not globally — the same path
/// can legitimately produce a row in two different sections (see the module
/// doc's `ChangeSides::Both` decision), and those two rows are never
/// compared against each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub key: String,
    /// The path shown as the row's primary text — the *new* path for a
    /// rename.
    pub path: String,
    /// Secondary detail text, when there is any worth showing beyond the
    /// path itself (a rename's source path and similarity, a submodule's
    /// flags) — `None` for an ordinary entry with nothing extra to say.
    pub secondary: Option<String>,
    /// The full state description in words — see the module doc's
    /// "Accessible labels" section for the reasoning.
    pub accessible_label: String,
}

/// The status list, grouped into sections, sorted, with a row and a count
/// per section — everything a future view needs to render the list except
/// the rendering itself.
#[derive(Debug, Clone, Default)]
pub struct StatusSections {
    rows: std::collections::BTreeMap<StatusSection, Vec<StatusRow>>,
}

impl StatusSections {
    /// Build the grouped, sorted sections from a real `WorktreeStatus` read
    /// (#68c). Pure — no I/O, no signal reads.
    pub fn from_worktree_status(status: &WorktreeStatus) -> Self {
        let mut rows: std::collections::BTreeMap<StatusSection, Vec<StatusRow>> =
            std::collections::BTreeMap::new();
        for entry in &status.entries {
            for (section, row) in rows_for_entry(entry) {
                rows.entry(section).or_default().push(row);
            }
        }
        for section_rows in rows.values_mut() {
            section_rows.sort_by(|a, b| a.path.cmp(&b.path));
        }
        StatusSections { rows }
    }

    /// The rows for one section, in sorted order — empty slice for a
    /// section with nothing in it (never absent from iteration, so a view
    /// can always ask every `StatusSection::ALL` member the same way).
    pub fn rows(&self, section: StatusSection) -> &[StatusRow] {
        self.rows.get(&section).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The number of rows in one section — a *section membership* count,
    /// not a unique-path count; see the module doc for why a doubly-dirty
    /// path is meant to count toward both `Staged` and `Unstaged`.
    pub fn count(&self, section: StatusSection) -> usize {
        self.rows(section).len()
    }

    /// True when every section is empty — the working tree is clean.
    pub fn is_clean(&self) -> bool {
        StatusSection::ALL.iter().all(|&s| self.count(s) == 0)
    }

    /// The three-way decision a status headline (topbar chip, Activity
    /// panel's summary line) makes over these sections, pulled out here so
    /// it is host-testable rather than left inline in `activity.rs`, which
    /// is wasm-only. Conflicted takes priority over an ordinary dirty count
    /// even when both are non-zero — a conflict needs resolving first,
    /// matching [`StatusSection::ALL`]'s own urgency ordering.
    pub fn headline(&self) -> StatusHeadline {
        let conflicted = self.count(StatusSection::Conflicted);
        if conflicted > 0 {
            return StatusHeadline::Conflicted(conflicted);
        }
        // Section-membership count, not distinct paths: a path dirty on
        // both sides counts toward both Staged and Unstaged — see the
        // module doc's "Consequence for counts" — matching v1
        // `RepoStatus::change_count()`'s identical semantics, which this
        // replaces. Ignored is deliberately excluded, same as v1: an
        // ignored file is not a "change" in the sense this headline reports.
        let dirty = self.count(StatusSection::Staged)
            + self.count(StatusSection::Unstaged)
            + self.count(StatusSection::Untracked);
        if dirty > 0 {
            StatusHeadline::Dirty(dirty)
        } else {
            StatusHeadline::Clean
        }
    }
}

/// The three states a status headline can show, in priority order —
/// [`StatusSections::headline`]'s pure result, before a view attaches an
/// icon or CSS class to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusHeadline {
    /// At least one conflicted path — takes priority over `Dirty` even when
    /// both counts are non-zero.
    Conflicted(usize),
    /// No conflicts, but at least one staged, unstaged, or untracked path.
    /// The count is section-membership, not distinct paths.
    Dirty(usize),
    /// Every section (including Ignored) is empty.
    Clean,
}

fn rows_for_entry(entry: &StatusEntry) -> Vec<(StatusSection, StatusRow)> {
    match entry {
        StatusEntry::Changed {
            path,
            sides,
            submodule,
            binary,
        } => changed_rows(path, sides, submodule.as_ref(), *binary),
        StatusEntry::Renamed {
            path,
            origin_path,
            score,
            unstaged,
            submodule,
            binary,
        } => renamed_rows(
            path,
            origin_path,
            *score,
            unstaged.as_ref(),
            submodule.as_ref(),
            *binary,
        ),
        StatusEntry::Untracked { path, binary } => vec![(
            StatusSection::Untracked,
            StatusRow {
                key: path.clone(),
                path: path.clone(),
                secondary: binary.then(|| "binary".to_string()),
                accessible_label: label_with_binary(format!("Untracked: {path}"), *binary),
            },
        )],
        StatusEntry::Ignored { path } => vec![(
            StatusSection::Ignored,
            StatusRow {
                key: path.clone(),
                path: path.clone(),
                secondary: None,
                accessible_label: format!("Ignored: {path}"),
            },
        )],
        StatusEntry::Conflicted {
            path,
            kind,
            submodule,
        } => vec![(
            StatusSection::Conflicted,
            StatusRow {
                key: path.clone(),
                path: path.clone(),
                secondary: submodule_summary(submodule.as_ref()),
                accessible_label: label_with_submodule(
                    format!("Merge conflict, {}: {path}", conflict_kind_words(*kind)),
                    submodule.as_ref(),
                ),
            },
        )],
    }
}

fn changed_rows(
    path: &str,
    sides: &ChangeSides,
    submodule: Option<&SubmoduleState>,
    binary: bool,
) -> Vec<(StatusSection, StatusRow)> {
    let mut out = Vec::new();
    if let Some(kind) = staged_kind(sides) {
        out.push((
            StatusSection::Staged,
            StatusRow {
                key: path.to_string(),
                path: path.to_string(),
                secondary: submodule_summary(submodule),
                accessible_label: label_with_extras(
                    format!("{}, staged: {path}", change_kind_words(kind)),
                    submodule,
                    binary,
                ),
            },
        ));
    }
    if let Some(kind) = unstaged_kind(sides) {
        out.push((
            StatusSection::Unstaged,
            StatusRow {
                key: path.to_string(),
                path: path.to_string(),
                secondary: submodule_summary(submodule),
                accessible_label: label_with_extras(
                    format!("{}, unstaged: {path}", change_kind_words(kind)),
                    submodule,
                    binary,
                ),
            },
        ));
    }
    out
}

fn renamed_rows(
    path: &str,
    origin_path: &str,
    score: u8,
    unstaged: Option<&ChangeKind>,
    submodule: Option<&SubmoduleState>,
    binary: bool,
) -> Vec<(StatusSection, StatusRow)> {
    // A rename is always staged (StatusEntry::Renamed's own doc comment: X
    // is always R/C by construction), so the Staged row is unconditional.
    let mut out = vec![(
        StatusSection::Staged,
        StatusRow {
            key: path.to_string(),
            path: path.to_string(),
            secondary: Some(format!("renamed from {origin_path}, {score}% similar")),
            accessible_label: label_with_extras(
                format!("Renamed from {origin_path} to {path}, {score}% similar, staged"),
                submodule,
                binary,
            ),
        },
    )];
    if let Some(kind) = unstaged {
        out.push((
            StatusSection::Unstaged,
            StatusRow {
                key: path.to_string(),
                path: path.to_string(),
                secondary: Some("modified since rename".to_string()),
                accessible_label: label_with_extras(
                    format!(
                        "{} since rename, unstaged: {path}",
                        change_kind_words(*kind)
                    ),
                    submodule,
                    binary,
                ),
            },
        ));
    }
    out
}

fn staged_kind(sides: &ChangeSides) -> Option<ChangeKind> {
    match sides {
        ChangeSides::StagedOnly { staged } | ChangeSides::Both { staged, .. } => Some(*staged),
        ChangeSides::UnstagedOnly { .. } => None,
    }
}

fn unstaged_kind(sides: &ChangeSides) -> Option<ChangeKind> {
    match sides {
        ChangeSides::UnstagedOnly { unstaged } | ChangeSides::Both { unstaged, .. } => {
            Some(*unstaged)
        }
        ChangeSides::StagedOnly { .. } => None,
    }
}

fn change_kind_words(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "Added",
        ChangeKind::Modified => "Modified",
        ChangeKind::Deleted => "Deleted",
    }
}

fn conflict_kind_words(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothDeleted => "both deleted",
        ConflictKind::AddedByUs => "added by us",
        ConflictKind::DeletedByThem => "deleted by them",
        ConflictKind::AddedByThem => "added by them",
        ConflictKind::DeletedByUs => "deleted by us",
        ConflictKind::BothAdded => "both added",
        ConflictKind::BothModified => "both modified",
    }
}

fn submodule_summary(submodule: Option<&SubmoduleState>) -> Option<String> {
    let s = submodule?;
    let mut parts = Vec::new();
    if s.commit_changed {
        parts.push("commit changed");
    }
    if s.has_tracked_changes {
        parts.push("modified content");
    }
    if s.has_untracked_changes {
        parts.push("untracked content");
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("submodule: {}", parts.join(", ")))
    }
}

fn label_with_submodule(base: String, submodule: Option<&SubmoduleState>) -> String {
    match submodule_summary(submodule) {
        Some(summary) => format!("{base} ({summary})"),
        None => base,
    }
}

fn label_with_binary(base: String, binary: bool) -> String {
    if binary {
        format!("{base} (binary)")
    } else {
        base
    }
}

fn label_with_extras(base: String, submodule: Option<&SubmoduleState>, binary: bool) -> String {
    label_with_binary(label_with_submodule(base, submodule), binary)
}

// ---------------------------------------------------------------------------
// Which paths each discard/delete operation may name (M2.18b, #220)
// ---------------------------------------------------------------------------
//
// These two functions exist to make the frontend's selection agree, by
// construction, with what the M2.18a backend (#219) will actually accept —
// `planner.rs`'s `classify_path_states` / `verify_path_states` re-derive the
// same classification from a *fresh* `git status` immediately before running
// git, and refuse the whole batch if any path disagrees. Offering the user a
// path the backend classifies differently isn't a cosmetic mismatch: it is a
// confirmation dialog that lists files, takes two taps, and then 409s.
//
// The rules are copied from that server-side classification, not invented:
//
//   * `DiscardTrackedPaths` wants `PathKind::TrackedDirty` — a porcelain `1`
//     (`StatusEntry::Changed`) or `2` (`StatusEntry::Renamed`) record. A
//     rename contributes its **new** path only; `origin_path` no longer names
//     anything on disk. Conflicted and ignored entries are never inserted
//     server-side, so they classify as `Other` by absence and are refused.
//   * `DeleteUntrackedPaths` wants `PathKind::Untracked` — a porcelain `?`
//     record and nothing else.
//
// Both also refuse a path that names a **directory**. `/api/status/v2` runs
// `git status --porcelain=v2 --branch -z` with no `--untracked-files=all`, so
// git's default `normal` mode collapses an entirely-untracked directory into
// one `?? dir/` record — and `symlink_containment_guard` refuses a directory
// target outright, precisely so that one entry cannot stand in for everything
// nested under it. Filtering the collapsed entry here keeps it out of a
// confirmation body that could otherwise understate the blast radius of the
// one operation with no way back.

/// True for the trailing-slash spelling `git status` uses for a collapsed
/// untracked directory. Kept as a named predicate because it is the one place
/// the frontend depends on that spelling.
fn names_a_directory(path: &str) -> bool {
    path.ends_with('/')
}

/// The paths a `DiscardTrackedPaths` request may name, given one live status
/// read — sorted, so the confirmation body reads the same way twice over an
/// unchanged worktree (`git-status(1)` guarantees no order of its own; see
/// this module's "Sort order" section).
pub fn discardable_tracked_paths(status: &WorktreeStatus) -> Vec<String> {
    let mut paths: Vec<String> = status
        .entries
        .iter()
        .filter_map(|e| match e {
            StatusEntry::Changed { path, .. } | StatusEntry::Renamed { path, .. } => {
                Some(path.clone())
            }
            StatusEntry::Untracked { .. }
            | StatusEntry::Ignored { .. }
            | StatusEntry::Conflicted { .. } => None,
        })
        .filter(|p| !names_a_directory(p))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The paths a `DeleteUntrackedPaths` request may name, given one live status
/// read — sorted, same reasoning as [`discardable_tracked_paths`].
pub fn deletable_untracked_paths(status: &WorktreeStatus) -> Vec<String> {
    let mut paths: Vec<String> = status
        .entries
        .iter()
        .filter_map(|e| match e {
            StatusEntry::Untracked { path, .. } => Some(path.clone()),
            StatusEntry::Changed { .. }
            | StatusEntry::Renamed { .. }
            | StatusEntry::Ignored { .. }
            | StatusEntry::Conflicted { .. } => None,
        })
        .filter(|p| !names_a_directory(p))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::GenerationToken;

    fn token() -> GenerationToken {
        GenerationToken::new("status-v1:1").unwrap()
    }

    fn status(entries: Vec<StatusEntry>) -> WorktreeStatus {
        WorktreeStatus {
            generation: token(),
            branch: Some("main".to_string()),
            upstream: None,
            ahead: 0,
            behind: 0,
            entries,
        }
    }

    fn changed(path: &str, sides: ChangeSides) -> StatusEntry {
        StatusEntry::Changed {
            path: path.to_string(),
            sides,
            submodule: None,
            binary: false,
        }
    }

    /// Every one of the eight named states (#68) is reachable from a real
    /// `WorktreeStatus` through `StatusSections`.
    #[test]
    fn all_eight_named_states_are_grouped_and_reachable() {
        let s = status(vec![
            changed(
                "staged.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            changed(
                "unstaged.rs",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            ),
            StatusEntry::Untracked {
                path: "new.txt".to_string(),
                binary: false,
            },
            StatusEntry::Ignored {
                path: "target/".to_string(),
            },
            StatusEntry::Conflicted {
                path: "clash.rs".to_string(),
                kind: ConflictKind::BothModified,
                submodule: None,
            },
            StatusEntry::Renamed {
                path: "new_name.rs".to_string(),
                origin_path: "old_name.rs".to_string(),
                score: 100,
                unstaged: None,
                submodule: None,
                binary: false,
            },
            StatusEntry::Changed {
                path: "vendor/lib".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
                submodule: Some(SubmoduleState {
                    commit_changed: false,
                    has_tracked_changes: true,
                    has_untracked_changes: false,
                }),
                binary: false,
            },
            StatusEntry::Untracked {
                path: "photo.bin".to_string(),
                binary: true,
            },
        ]);
        let sections = StatusSections::from_worktree_status(&s);

        // staged
        assert_eq!(sections.count(StatusSection::Staged), 2); // staged.rs, new_name.rs (rename)
                                                              // unstaged
        assert_eq!(sections.count(StatusSection::Unstaged), 2); // unstaged.rs, vendor/lib (submodule)
                                                                // untracked
        assert_eq!(sections.count(StatusSection::Untracked), 2);
        // ignored
        assert_eq!(sections.count(StatusSection::Ignored), 1);
        // conflicted
        assert_eq!(sections.count(StatusSection::Conflicted), 1);

        // renamed: reachable, staged section, secondary mentions the source.
        let renamed_row = sections
            .rows(StatusSection::Staged)
            .iter()
            .find(|r| r.path == "new_name.rs")
            .unwrap();
        assert!(renamed_row
            .secondary
            .as_deref()
            .unwrap()
            .contains("old_name.rs"));

        // submodule: reachable, secondary mentions it.
        let submodule_row = sections
            .rows(StatusSection::Unstaged)
            .iter()
            .find(|r| r.path == "vendor/lib")
            .unwrap();
        assert!(submodule_row.accessible_label.contains("submodule"));

        // binary: reachable, secondary/label mentions it.
        let binary_row = sections
            .rows(StatusSection::Untracked)
            .iter()
            .find(|r| r.path == "photo.bin")
            .unwrap();
        assert!(binary_row.accessible_label.contains("binary"));

        assert!(!sections.is_clean());
    }

    #[test]
    fn empty_status_is_clean() {
        let s = status(vec![]);
        let sections = StatusSections::from_worktree_status(&s);
        assert!(sections.is_clean());
        for section in StatusSection::ALL {
            assert_eq!(sections.count(section), 0);
            assert!(sections.rows(section).is_empty());
        }
    }

    /// `ChangeSides::Both` deliberately produces a row in BOTH sections —
    /// the module doc's grouping decision, pinned rather than left implicit.
    #[test]
    fn both_sides_dirty_appears_in_both_staged_and_unstaged() {
        let s = status(vec![changed(
            "both.rs",
            ChangeSides::Both {
                staged: ChangeKind::Added,
                unstaged: ChangeKind::Modified,
            },
        )]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.count(StatusSection::Staged), 1);
        assert_eq!(sections.count(StatusSection::Unstaged), 1);
        assert_eq!(sections.rows(StatusSection::Staged)[0].path, "both.rs");
        assert_eq!(sections.rows(StatusSection::Unstaged)[0].path, "both.rs");
        // Different labels — one says "staged", the other "unstaged".
        assert!(sections.rows(StatusSection::Staged)[0]
            .accessible_label
            .contains("staged"));
        assert!(sections.rows(StatusSection::Unstaged)[0]
            .accessible_label
            .contains("unstaged"));
    }

    /// A rename with a further edit is in Staged (always) AND Unstaged (the
    /// further edit) — the same "appears twice" rule as `Both`.
    #[test]
    fn renamed_and_further_edited_appears_in_both_sections() {
        let s = status(vec![StatusEntry::Renamed {
            path: "b.rs".to_string(),
            origin_path: "a.rs".to_string(),
            score: 87,
            unstaged: Some(ChangeKind::Modified),
            submodule: None,
            binary: false,
        }]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.count(StatusSection::Staged), 1);
        assert_eq!(sections.count(StatusSection::Unstaged), 1);
    }

    /// A rename with no further edit is ONLY in Staged.
    #[test]
    fn pure_rename_is_staged_only() {
        let s = status(vec![StatusEntry::Renamed {
            path: "b.rs".to_string(),
            origin_path: "a.rs".to_string(),
            score: 100,
            unstaged: None,
            submodule: None,
            binary: false,
        }]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.count(StatusSection::Staged), 1);
        assert_eq!(sections.count(StatusSection::Unstaged), 0);
    }

    /// Two structurally-identical-but-differently-ordered inputs produce the
    /// same grouped, sorted output — porcelain v2's own "undefined order"
    /// must not leak into the accessible list's row order.
    #[test]
    fn sort_order_is_deterministic_regardless_of_input_order() {
        let entries_a = vec![
            changed(
                "b.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            changed(
                "a.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            changed(
                "c.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
        ];
        let entries_b = vec![
            entries_a[2].clone(),
            entries_a[0].clone(),
            entries_a[1].clone(),
        ];

        let sections_a = StatusSections::from_worktree_status(&status(entries_a));
        let sections_b = StatusSections::from_worktree_status(&status(entries_b));

        let paths_a: Vec<&str> = sections_a
            .rows(StatusSection::Staged)
            .iter()
            .map(|r| r.path.as_str())
            .collect();
        let paths_b: Vec<&str> = sections_b
            .rows(StatusSection::Staged)
            .iter()
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(paths_a, vec!["a.rs", "b.rs", "c.rs"]);
        assert_eq!(paths_a, paths_b);
    }

    #[test]
    fn conflict_label_names_the_specific_conflict_kind() {
        let s = status(vec![StatusEntry::Conflicted {
            path: "clash.rs".to_string(),
            kind: ConflictKind::AddedByThem,
            submodule: None,
        }]);
        let sections = StatusSections::from_worktree_status(&s);
        let row = &sections.rows(StatusSection::Conflicted)[0];
        assert!(row.accessible_label.contains("added by them"));
    }

    // -----------------------------------------------------------------
    // M2.18b (#220): which paths each operation may name
    // -----------------------------------------------------------------

    /// A worktree carrying one of every entry kind, so each test below can
    /// assert both what its function picks up *and* what it leaves behind.
    fn mixed_worktree() -> WorktreeStatus {
        status(vec![
            changed(
                "src/edited.rs",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            ),
            changed(
                "src/staged.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            StatusEntry::Renamed {
                path: "src/new_name.rs".to_string(),
                origin_path: "src/old_name.rs".to_string(),
                score: 100,
                unstaged: None,
                submodule: None,
                binary: false,
            },
            StatusEntry::Untracked {
                path: "scratch.txt".to_string(),
                binary: false,
            },
            StatusEntry::Ignored {
                path: "target/".to_string(),
            },
            StatusEntry::Conflicted {
                path: "clash.rs".to_string(),
                kind: ConflictKind::BothModified,
                submodule: None,
            },
        ])
    }

    /// The discard selection is exactly the server's `PathKind::TrackedDirty`
    /// set: changed + renamed-new-path, and *nothing* else. The exclusions
    /// carry the weight — an untracked or conflicted path reaching
    /// `/api/discard-tracked-paths` is a guaranteed 409 from
    /// `verify_path_states`, after the user has already confirmed.
    #[test]
    fn discard_selects_only_the_tracked_dirty_paths() {
        let picked = discardable_tracked_paths(&mixed_worktree());
        assert_eq!(
            picked,
            vec![
                "src/edited.rs".to_string(),
                "src/new_name.rs".to_string(),
                "src/staged.rs".to_string(),
            ]
        );
        // Named individually so a future regression says which rule broke.
        assert!(
            !picked.contains(&"src/old_name.rs".to_string()),
            "{picked:?}"
        );
        assert!(!picked.contains(&"scratch.txt".to_string()), "{picked:?}");
        assert!(!picked.contains(&"target/".to_string()), "{picked:?}");
        assert!(!picked.contains(&"clash.rs".to_string()), "{picked:?}");
    }

    /// The delete selection is exactly the server's `PathKind::Untracked`
    /// set. A tracked-but-dirty path here would mean `git clean -f` was asked
    /// to remove a file whose content *is* in the object database — the two
    /// operations are separate variants precisely so that cannot happen.
    #[test]
    fn delete_selects_only_the_untracked_paths() {
        let picked = deletable_untracked_paths(&mixed_worktree());
        assert_eq!(picked, vec!["scratch.txt".to_string()]);
        assert!(!picked.contains(&"src/edited.rs".to_string()), "{picked:?}");
        assert!(!picked.contains(&"target/".to_string()), "{picked:?}");
        assert!(!picked.contains(&"clash.rs".to_string()), "{picked:?}");
    }

    /// `/api/status/v2` runs `git status` in its default untracked mode, so
    /// an entirely-untracked directory arrives collapsed to one `dir/`
    /// record — and the backend refuses a directory target outright
    /// (`symlink_containment_guard`), because that single entry would stand
    /// in for every file nested under it.
    ///
    /// The second assertion is the paired negative: it pins that the entry
    /// really is present in the input and really is an `Untracked` record,
    /// so this test fails if the filter is removed rather than passing
    /// because the fixture never contained a directory in the first place.
    #[test]
    fn a_collapsed_untracked_directory_is_never_offered() {
        let s = status(vec![
            StatusEntry::Untracked {
                path: "scratch/".to_string(),
                binary: false,
            },
            StatusEntry::Untracked {
                path: "note.txt".to_string(),
                binary: false,
            },
        ]);
        assert_eq!(deletable_untracked_paths(&s), vec!["note.txt".to_string()]);
        // Paired negative: without the directory rule, the same input yields
        // both — so the assertion above is capable of failing.
        let unfiltered: Vec<String> = s
            .entries
            .iter()
            .filter_map(|e| match e {
                StatusEntry::Untracked { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(unfiltered.len(), 2, "{unfiltered:?}");
        assert!(
            unfiltered.contains(&"scratch/".to_string()),
            "{unfiltered:?}"
        );
    }

    /// Both selections are sorted, so two reads of an unchanged worktree
    /// produce the same confirmation body. The paired assertion pins that
    /// the *input* order really did differ — otherwise "equal outputs" would
    /// prove nothing about sorting.
    #[test]
    fn selection_order_does_not_follow_gits_undefined_entry_order() {
        let a = changed(
            "z.rs",
            ChangeSides::UnstagedOnly {
                unstaged: ChangeKind::Modified,
            },
        );
        let b = changed(
            "a.rs",
            ChangeSides::UnstagedOnly {
                unstaged: ChangeKind::Modified,
            },
        );
        let forwards = status(vec![a.clone(), b.clone()]);
        let backwards = status(vec![b, a]);
        assert_ne!(forwards.entries, backwards.entries);
        assert_eq!(
            discardable_tracked_paths(&forwards),
            discardable_tracked_paths(&backwards)
        );
        assert_eq!(
            discardable_tracked_paths(&forwards),
            vec!["a.rs".to_string(), "z.rs".to_string()]
        );
    }

    /// A clean worktree offers neither operation anything — the empty case
    /// the confirmation's own "nothing to act on" arm is built for.
    #[test]
    fn a_clean_worktree_offers_no_paths_to_either_operation() {
        let s = status(vec![]);
        assert!(discardable_tracked_paths(&s).is_empty());
        assert!(deletable_untracked_paths(&s).is_empty());
    }

    // -----------------------------------------------------------------
    // headline() — M2.15 (#68), the Activity panel's status summary line
    // -----------------------------------------------------------------

    #[test]
    fn a_clean_worktree_headlines_clean() {
        let sections = StatusSections::from_worktree_status(&status(vec![]));
        assert_eq!(sections.headline(), StatusHeadline::Clean);
    }

    #[test]
    fn ignored_files_alone_still_headline_clean() {
        // Ignored is a real section (housekeeping, per the module doc) but
        // must not count as "dirty" — an ignored-only tree reads the same
        // as an empty one at the headline level, matching v1's behaviour
        // (ignored paths were never part of `RepoStatus::change_count()`).
        let s = status(vec![StatusEntry::Ignored {
            path: "target/".to_string(),
        }]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.headline(), StatusHeadline::Clean);
        assert!(!sections.is_clean(), "is_clean() sees the ignored entry");
    }

    #[test]
    fn staged_unstaged_and_untracked_all_headline_dirty_and_sum() {
        let s = status(vec![
            changed(
                "a.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            changed(
                "b.rs",
                ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified,
                },
            ),
            StatusEntry::Untracked {
                path: "c.txt".to_string(),
                binary: false,
            },
        ]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.headline(), StatusHeadline::Dirty(3));
    }

    #[test]
    fn a_path_dirty_on_both_sides_counts_twice_in_the_headline() {
        // Same section-membership semantics as StatusSections::count itself
        // (see the module doc's "Consequence for counts") — the headline
        // must not silently switch to a distinct-path count.
        let s = status(vec![changed(
            "both.rs",
            ChangeSides::Both {
                staged: ChangeKind::Added,
                unstaged: ChangeKind::Modified,
            },
        )]);
        let sections = StatusSections::from_worktree_status(&s);
        assert_eq!(sections.headline(), StatusHeadline::Dirty(2));
    }

    #[test]
    fn conflicted_takes_priority_over_dirty_even_when_both_are_present() {
        let s = status(vec![
            changed(
                "staged.rs",
                ChangeSides::StagedOnly {
                    staged: ChangeKind::Added,
                },
            ),
            StatusEntry::Conflicted {
                path: "clash.rs".to_string(),
                kind: ConflictKind::BothModified,
                submodule: None,
            },
        ]);
        let sections = StatusSections::from_worktree_status(&s);
        // A conflict is not just "shown first" — the ordinary dirty count
        // must not leak into the headline at all while one exists, or a
        // caller matching on the variant loses the staged file's presence
        // silently rather than seeing it once the conflict is resolved.
        assert_eq!(sections.headline(), StatusHeadline::Conflicted(1));
    }
}
