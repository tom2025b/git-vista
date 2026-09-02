//! Pure working-tree status projection for the terminal (#459).
//!
//! The wire vocabulary is shared with the browser through
//! [`git_vista_protocol::WorktreeStatus`]. This module projects it into the
//! same five sections, in the same priority order, without introducing a
//! second status parser. One path can appear twice: `ChangeSides::Both` is
//! independently actionable in Staged and Unstaged, as is a rename edited
//! again after it was staged.
//!
//! Loading, ready, and failed are deliberately three states. A failed read is
//! never rendered as a clean tree; if a prior successful snapshot exists its
//! rows remain visible while the refusal is reported separately.

use git_vista_protocol::{
    ChangeKind, ChangeSides, ConflictKind, StageDirection, StatusEntry, SubmoduleState,
    WorktreeStatus,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
    Ignored,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::Conflicted,
        Section::Staged,
        Section::Unstaged,
        Section::Untracked,
        Section::Ignored,
    ];

    pub fn heading(self) -> &'static str {
        match self {
            Section::Conflicted => "Conflicted",
            Section::Staged => "Staged changes",
            Section::Unstaged => "Unstaged changes",
            Section::Untracked => "Untracked files",
            Section::Ignored => "Ignored files",
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            Section::Conflicted => "!!",
            Section::Staged => "S ",
            Section::Unstaged => " U",
            Section::Untracked => "??",
            Section::Ignored => " I",
        }
    }

    /// Direction for the section-level whole-tree shortcut.
    pub fn whole_direction(self) -> Option<StageDirection> {
        match self {
            Section::Staged => Some(StageDirection::Unstage),
            Section::Unstaged | Section::Untracked => Some(StageDirection::Stage),
            Section::Conflicted | Section::Ignored => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub section: Section,
    pub path: String,
    pub detail: Option<String>,
    /// File-level partial staging is available only when the path is in the
    /// corresponding staging diff. Untracked paths are absent from `git diff`
    /// and therefore have no honest file-level preview in the shared API.
    pub file_direction: Option<StageDirection>,
    pub discardable: bool,
}

impl Row {
    pub fn render(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} {} — {detail}", self.section.marker(), self.path),
            None => format!("{} {}", self.section.marker(), self.path),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct WorktreePane {
    snapshot: Option<WorktreeStatus>,
    rows: Vec<Row>,
    state: LoadState,
}

impl Default for WorktreePane {
    fn default() -> Self {
        Self {
            snapshot: None,
            rows: Vec::new(),
            state: LoadState::Loading,
        }
    }
}

impl WorktreePane {
    pub fn begin_load(&mut self) {
        self.state = LoadState::Loading;
    }

    pub fn receive(&mut self, result: Result<WorktreeStatus, String>) {
        match result {
            Ok(status) => {
                self.rows = rows_for_status(&status);
                self.snapshot = Some(status);
                self.state = LoadState::Ready;
            }
            Err(message) => self.state = LoadState::Failed(message),
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index)
    }

    pub fn state(&self) -> &LoadState {
        &self.state
    }

    pub fn branch_line(&self) -> Option<String> {
        let status = self.snapshot.as_ref()?;
        let branch = status.branch.as_deref().unwrap_or("detached HEAD");
        let mut out = branch.to_string();
        if let Some(upstream) = &status.upstream {
            out.push_str(&format!(" → {upstream}"));
        }
        if status.ahead > 0 || status.behind > 0 {
            out.push_str(&format!("  ↑{} ↓{}", status.ahead, status.behind));
        }
        Some(out)
    }
}

fn rows_for_status(status: &WorktreeStatus) -> Vec<Row> {
    let mut rows = Vec::new();
    for entry in &status.entries {
        rows.extend(rows_for_entry(entry));
    }
    rows.sort_by(|a, b| (a.section, &a.path).cmp(&(b.section, &b.path)));
    rows
}

fn rows_for_entry(entry: &StatusEntry) -> Vec<Row> {
    match entry {
        StatusEntry::Changed {
            path,
            sides,
            submodule,
            binary,
        } => {
            let detail = extras(submodule.as_ref(), *binary);
            let mut rows = Vec::new();
            if let Some(kind) = staged_kind(*sides) {
                rows.push(Row {
                    section: Section::Staged,
                    path: path.clone(),
                    detail: join_detail(Some(kind_word(kind).to_string()), detail.clone()),
                    file_direction: Some(StageDirection::Unstage),
                    discardable: false,
                });
            }
            if let Some(kind) = unstaged_kind(*sides) {
                rows.push(Row {
                    section: Section::Unstaged,
                    path: path.clone(),
                    detail: join_detail(Some(kind_word(kind).to_string()), detail),
                    file_direction: Some(StageDirection::Stage),
                    discardable: true,
                });
            }
            rows
        }
        StatusEntry::Renamed {
            path,
            origin_path,
            score,
            unstaged,
            submodule,
            binary,
        } => {
            let extra = extras(submodule.as_ref(), *binary);
            let mut rows = vec![Row {
                section: Section::Staged,
                path: path.clone(),
                detail: join_detail(
                    Some(format!("renamed from {origin_path}, {score}% similar")),
                    extra.clone(),
                ),
                file_direction: Some(StageDirection::Unstage),
                discardable: false,
            }];
            if let Some(kind) = unstaged {
                rows.push(Row {
                    section: Section::Unstaged,
                    path: path.clone(),
                    detail: join_detail(Some(format!("{} since rename", kind_word(*kind))), extra),
                    file_direction: Some(StageDirection::Stage),
                    discardable: true,
                });
            }
            rows
        }
        StatusEntry::Untracked { path, binary } => vec![Row {
            section: Section::Untracked,
            path: path.clone(),
            detail: binary.then(|| "binary".to_string()),
            file_direction: None,
            discardable: false,
        }],
        StatusEntry::Ignored { path } => vec![Row {
            section: Section::Ignored,
            path: path.clone(),
            detail: None,
            file_direction: None,
            discardable: false,
        }],
        StatusEntry::Conflicted {
            path,
            kind,
            submodule,
        } => vec![Row {
            section: Section::Conflicted,
            path: path.clone(),
            detail: join_detail(
                Some(conflict_word(*kind).to_string()),
                submodule_detail(submodule.as_ref()),
            ),
            file_direction: None,
            discardable: false,
        }],
    }
}

fn staged_kind(sides: ChangeSides) -> Option<ChangeKind> {
    match sides {
        ChangeSides::StagedOnly { staged } | ChangeSides::Both { staged, .. } => Some(staged),
        ChangeSides::UnstagedOnly { .. } => None,
    }
}

fn unstaged_kind(sides: ChangeSides) -> Option<ChangeKind> {
    match sides {
        ChangeSides::UnstagedOnly { unstaged } | ChangeSides::Both { unstaged, .. } => {
            Some(unstaged)
        }
        ChangeSides::StagedOnly { .. } => None,
    }
}

fn kind_word(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

fn conflict_word(kind: ConflictKind) -> &'static str {
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

fn submodule_detail(submodule: Option<&SubmoduleState>) -> Option<String> {
    let submodule = submodule?;
    let mut parts = Vec::new();
    if submodule.commit_changed {
        parts.push("commit changed");
    }
    if submodule.has_tracked_changes {
        parts.push("modified content");
    }
    if submodule.has_untracked_changes {
        parts.push("untracked content");
    }
    (!parts.is_empty()).then(|| format!("submodule: {}", parts.join(", ")))
}

fn extras(submodule: Option<&SubmoduleState>, binary: bool) -> Option<String> {
    join_detail(
        submodule_detail(submodule),
        binary.then(|| "binary".to_string()),
    )
}

fn join_detail(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use git_vista_protocol::GenerationToken;

    use super::*;

    fn status(entries: Vec<StatusEntry>) -> WorktreeStatus {
        WorktreeStatus {
            generation: GenerationToken::new("status-v1:test").unwrap(),
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            ahead: 2,
            behind: 1,
            entries,
        }
    }

    #[test]
    fn every_browser_status_state_is_visible_and_actionable_only_when_supported() {
        let submodule = Some(SubmoduleState {
            commit_changed: true,
            has_tracked_changes: true,
            has_untracked_changes: false,
        });
        let mut pane = WorktreePane::default();
        pane.receive(Ok(status(vec![
            StatusEntry::Changed {
                path: "both.rs".into(),
                sides: ChangeSides::Both {
                    staged: ChangeKind::Added,
                    unstaged: ChangeKind::Modified,
                },
                submodule,
                binary: false,
            },
            StatusEntry::Renamed {
                path: "new.rs".into(),
                origin_path: "old.rs".into(),
                score: 91,
                unstaged: Some(ChangeKind::Modified),
                submodule: None,
                binary: true,
            },
            StatusEntry::Untracked {
                path: "photo.bin".into(),
                binary: true,
            },
            StatusEntry::Ignored {
                path: "target/".into(),
            },
            StatusEntry::Conflicted {
                path: "clash.rs".into(),
                kind: ConflictKind::BothModified,
                submodule: None,
            },
        ])));

        let sections: Vec<Section> = pane.rows().iter().map(|row| row.section).collect();
        assert_eq!(
            sections,
            [
                Section::Conflicted,
                Section::Staged,
                Section::Staged,
                Section::Unstaged,
                Section::Unstaged,
                Section::Untracked,
                Section::Ignored,
            ]
        );
        assert_eq!(
            pane.rows()
                .iter()
                .filter(|row| row.path == "both.rs")
                .count(),
            2,
            "a both-sides change must remain two independently actionable rows"
        );
        assert_eq!(
            pane.rows()
                .iter()
                .filter(|row| row.path == "new.rs")
                .count(),
            2,
            "a renamed-then-edited path must remain staged and unstaged"
        );
        assert!(pane.rows().iter().any(|row| row
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("submodule"))));
        assert!(pane.rows().iter().any(|row| row
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("binary"))));
        let untracked = pane
            .rows()
            .iter()
            .find(|row| row.section == Section::Untracked)
            .unwrap();
        assert_eq!(untracked.file_direction, None);
        assert_eq!(
            untracked.section.whole_direction(),
            Some(StageDirection::Stage)
        );
        assert_eq!(
            pane.branch_line().as_deref(),
            Some("main → origin/main  ↑2 ↓1")
        );
    }

    #[test]
    fn a_failed_refresh_is_not_clean_and_does_not_erase_the_last_good_rows() {
        let mut pane = WorktreePane::default();
        assert_eq!(pane.state(), &LoadState::Loading);
        pane.receive(Err("status unavailable".into()));
        assert_eq!(
            pane.state(),
            &LoadState::Failed("status unavailable".into())
        );
        assert!(pane.rows().is_empty());

        pane.receive(Ok(status(vec![StatusEntry::Untracked {
            path: "kept.txt".into(),
            binary: false,
        }])));
        pane.begin_load();
        pane.receive(Err("refresh refused".into()));
        assert_eq!(pane.rows().len(), 1, "the last good snapshot stays visible");
        assert_eq!(pane.rows()[0].path, "kept.txt");
        assert_eq!(pane.state(), &LoadState::Failed("refresh refused".into()));
    }

    #[test]
    fn rows_are_section_then_path_sorted_independent_of_porcelain_order() {
        let entries = vec![
            StatusEntry::Untracked {
                path: "z.txt".into(),
                binary: false,
            },
            StatusEntry::Untracked {
                path: "a.txt".into(),
                binary: false,
            },
            StatusEntry::Changed {
                path: "m.txt".into(),
                sides: ChangeSides::StagedOnly {
                    staged: ChangeKind::Modified,
                },
                submodule: None,
                binary: false,
            },
        ];
        let mut pane = WorktreePane::default();
        pane.receive(Ok(status(entries)));
        let got: Vec<(Section, &str)> = pane
            .rows()
            .iter()
            .map(|row| (row.section, row.path.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Section::Staged, "m.txt"),
                (Section::Untracked, "a.txt"),
                (Section::Untracked, "z.txt"),
            ]
        );
    }
}
