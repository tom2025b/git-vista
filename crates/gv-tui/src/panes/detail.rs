//! Commit detail and diff projection (M10.03, #458).
//!
//! The finished pane consumes the existing commit-detail and bounded-diff
//! payloads, plus the protocol crate's unified-diff parser. It will expose
//! only a viewport-sized row window to Ratatui, keep author and committer
//! identities distinct, make parents keyboard-selectable, and represent a
//! binary as a label rather than terminal content.

use git_vista_core::diff::{CommitDiff, DiffFile};
use git_vista_core::model::CommitDetail;
use git_vista_core::status::ChangeKind;
use git_vista_protocol::diff::{parse_unified_diff, FileDiff, LineKind, ParsedPatch};

/// Semantic styling attached to one terminal row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    Plain,
    Heading,
    Muted,
    Added,
    Removed,
    Hunk,
    Error,
    Parent,
    SelectedParent,
}

/// One logical row. A draw asks for only the visible window of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailRow {
    pub text: String,
    pub tone: RowTone,
}

#[derive(Debug)]
enum Load<T> {
    Waiting,
    Ready(T),
    Failed(String),
}

#[derive(Debug)]
struct LoadedDiff {
    payload: CommitDiff,
    parsed: ParsedPatch,
}

/// Detail-pane state. Its request key is kept beside both independent reads,
/// so an answer for a commit the user has already left cannot repaint it.
#[derive(Debug)]
pub struct DetailPane {
    repo: Option<String>,
    id: Option<String>,
    detail: Load<CommitDetail>,
    diff: Load<LoadedDiff>,
    parent: usize,
    horizontal: usize,
}

impl Default for DetailPane {
    fn default() -> Self {
        Self {
            repo: None,
            id: None,
            detail: Load::Waiting,
            diff: Load::Waiting,
            parent: 0,
            horizontal: 0,
        }
    }
}

impl DetailPane {
    pub fn open(&mut self, repo: String, id: String) {
        self.repo = Some(repo);
        self.id = Some(id);
        self.detail = Load::Waiting;
        self.diff = Load::Waiting;
        self.parent = 0;
        self.horizontal = 0;
    }

    pub fn receive_detail(
        &mut self,
        repo: &str,
        id: &str,
        result: Result<CommitDetail, String>,
    ) -> bool {
        if !self.is_current(repo, id) {
            return false;
        }
        self.detail = match result {
            Ok(detail) if detail.id.0 == id => Load::Ready(detail),
            Ok(detail) => Load::Failed(format!(
                "commit response for {id} identified {} instead",
                detail.id.0
            )),
            Err(message) => Load::Failed(message),
        };
        self.clamp_parent();
        true
    }

    pub fn receive_diff(
        &mut self,
        repo: &str,
        id: &str,
        result: Result<CommitDiff, String>,
    ) -> bool {
        if !self.is_current(repo, id) {
            return false;
        }
        self.diff = match result {
            Ok(payload) if payload.id == id => {
                let parsed = parse_unified_diff(&payload.patch);
                Load::Ready(LoadedDiff { payload, parsed })
            }
            Ok(payload) => Load::Failed(format!(
                "diff response for {id} identified {} instead",
                payload.id
            )),
            Err(message) => Load::Failed(message),
        };
        true
    }

    pub fn row_count(&self) -> usize {
        if self.id.is_none() {
            return 1;
        }
        detail_row_count(&self.detail) + diff_row_count(&self.diff)
    }

    pub fn window(&self, offset: usize, limit: usize) -> Vec<DetailRow> {
        self.project_window(offset, limit).rows
    }

    fn project_window(&self, offset: usize, limit: usize) -> RowWindow {
        let mut window = RowWindow::new(offset, limit);
        self.visit_rows(|row| window.push(row));
        window
    }

    #[cfg(test)]
    fn window_with_visit_count(&self, offset: usize, limit: usize) -> (Vec<DetailRow>, usize) {
        let window = self.project_window(offset, limit);
        (window.rows, window.seen)
    }

    pub fn select_parent(&mut self, delta: isize) {
        let len = self.parents().map_or(0, <[_]>::len);
        if len == 0 {
            self.parent = 0;
            return;
        }
        self.parent = self
            .parent
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
    }

    pub fn selected_parent(&self) -> Option<&str> {
        self.parents()?
            .get(self.parent)
            .map(|parent| parent.0.as_str())
    }

    pub fn scroll_horizontal(&mut self, delta: isize) {
        self.horizontal = self.horizontal.saturating_add_signed(delta);
    }

    pub fn horizontal(&self) -> usize {
        self.horizontal
    }

    pub fn current(&self) -> Option<(&str, &str)> {
        Some((self.repo.as_deref()?, self.id.as_deref()?))
    }

    fn is_current(&self, repo: &str, id: &str) -> bool {
        self.current() == Some((repo, id))
    }

    fn parents(&self) -> Option<&[git_vista_core::model::Oid]> {
        match &self.detail {
            Load::Ready(detail) => Some(&detail.parents),
            Load::Waiting | Load::Failed(_) => None,
        }
    }

    fn clamp_parent(&mut self) {
        let len = self.parents().map_or(0, <[_]>::len);
        self.parent = self.parent.min(len.saturating_sub(1));
    }

    fn visit_rows(&self, mut emit: impl FnMut(DetailRow) -> bool) {
        if self.id.is_none() {
            let _ = emit(row("Select a commit and press Enter.", RowTone::Muted));
            return;
        }

        match &self.detail {
            Load::Waiting => {
                if !emit(row("Loading commit…", RowTone::Muted)) {
                    return;
                }
            }
            Load::Failed(message) => {
                if !emit(row(
                    format!("Could not load commit: {message}"),
                    RowTone::Error,
                )) {
                    return;
                }
            }
            Load::Ready(detail) => {
                for item in [
                    row(format!("Commit {}", detail.id.0), RowTone::Heading),
                    row(
                        format!(
                            "Author {} <{}> · {}",
                            detail.author_name, detail.author_email, detail.author_time
                        ),
                        RowTone::Plain,
                    ),
                    row(
                        format!(
                            "Committer {} <{}> · {}",
                            detail.committer_name, detail.committer_email, detail.commit_time
                        ),
                        RowTone::Plain,
                    ),
                    row("Parents", RowTone::Heading),
                ] {
                    if !emit(item) {
                        return;
                    }
                }
                if detail.parents.is_empty() {
                    if !emit(row("none (root commit)", RowTone::Muted)) {
                        return;
                    }
                } else {
                    for (index, parent) in detail.parents.iter().enumerate() {
                        let tone = if index == self.parent {
                            RowTone::SelectedParent
                        } else {
                            RowTone::Parent
                        };
                        if !emit(row(format!("Parent {}  {}", index + 1, parent.0), tone)) {
                            return;
                        }
                    }
                }
                if !emit(row("Message", RowTone::Heading)) {
                    return;
                }
                let mut lines = detail.message.lines().peekable();
                if lines.peek().is_none() {
                    if !emit(row("", RowTone::Plain)) {
                        return;
                    }
                } else {
                    for line in lines {
                        if !emit(row(line, RowTone::Plain)) {
                            return;
                        }
                    }
                }
            }
        }

        match &self.diff {
            Load::Waiting => {
                let _ = emit(row("Loading changes…", RowTone::Muted));
            }
            Load::Failed(message) => {
                let _ = emit(row(
                    format!("Could not load diff: {message}"),
                    RowTone::Error,
                ));
            }
            Load::Ready(diff) => {
                let (adds, deletes) = diff.payload.totals();
                if !emit(row(
                    format!(
                        "Changes — {} file{}  +{adds} -{deletes}",
                        diff.payload.files.len(),
                        if diff.payload.files.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    RowTone::Heading,
                )) {
                    return;
                }
                if diff.payload.against_first_parent
                    && !emit(row("merge diff against first parent", RowTone::Muted))
                {
                    return;
                }
                for file in &diff.payload.files {
                    if !emit(file_summary(file)) {
                        return;
                    }
                }
                if diff.payload.truncated
                    && !emit(row(
                        "diff truncated at the server display cap",
                        RowTone::Muted,
                    ))
                {
                    return;
                }
                if !emit(row("Unified diff", RowTone::Heading)) {
                    return;
                }
                if diff.parsed.files.is_empty() {
                    let _ = emit(row("(no textual patch)", RowTone::Muted));
                    return;
                }
                visit_patch(&diff.parsed, emit);
            }
        }
    }
}

fn row(text: impl Into<String>, tone: RowTone) -> DetailRow {
    DetailRow {
        text: text.into(),
        tone,
    }
}

fn detail_row_count(detail: &Load<CommitDetail>) -> usize {
    match detail {
        Load::Waiting | Load::Failed(_) => 1,
        Load::Ready(detail) => {
            let parents = detail.parents.len().max(1);
            let message = detail.message.lines().count().max(1);
            4 + parents + 1 + message
        }
    }
}

fn diff_row_count(diff: &Load<LoadedDiff>) -> usize {
    match diff {
        Load::Waiting | Load::Failed(_) => 1,
        Load::Ready(diff) => {
            1 + usize::from(diff.payload.against_first_parent)
                + diff.payload.files.len()
                + usize::from(diff.payload.truncated)
                + 1
                + parsed_row_count(&diff.parsed).max(1)
        }
    }
}

fn parsed_row_count(patch: &ParsedPatch) -> usize {
    patch
        .files
        .iter()
        .map(|file| {
            1 + match file {
                FileDiff::Hunks { hunks, .. } => hunks
                    .iter()
                    .map(|hunk| {
                        1 + hunk.lines.len()
                            + hunk
                                .lines
                                .iter()
                                .filter(|line| line.no_newline_at_eof)
                                .count()
                    })
                    .sum::<usize>(),
                FileDiff::ModeChangeOnly { .. }
                | FileDiff::Binary { .. }
                | FileDiff::Renamed { .. } => 1,
                FileDiff::Combined { raw, .. } => raw.lines().count().max(1),
            }
        })
        .sum()
}

fn file_summary(file: &DiffFile) -> DetailRow {
    let kind = match file.kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    };
    let path = file
        .old_path
        .as_ref()
        .map_or_else(|| file.path.clone(), |old| format!("{old} → {}", file.path));
    let counts = match (file.additions, file.deletions) {
        (Some(adds), Some(deletes)) => format!("+{adds} -{deletes}"),
        _ => "binary — content not shown".to_string(),
    };
    row(format!("{kind:>8}  {path}  {counts}"), RowTone::Plain)
}

fn display_path(file: &FileDiff) -> String {
    match file {
        FileDiff::Hunks {
            old_path, new_path, ..
        }
        | FileDiff::Binary { old_path, new_path } => match (old_path, new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (Some(path), _) | (_, Some(path)) => path.clone(),
            (None, None) => "/dev/null".to_string(),
        },
        FileDiff::ModeChangeOnly { path, .. } | FileDiff::Combined { path, .. } => path.clone(),
        FileDiff::Renamed {
            old_path, new_path, ..
        } => format!("{old_path} → {new_path}"),
    }
}

fn visit_patch(patch: &ParsedPatch, mut emit: impl FnMut(DetailRow) -> bool) {
    for file in &patch.files {
        if !emit(row(display_path(file), RowTone::Heading)) {
            return;
        }
        match file {
            FileDiff::Hunks { hunks, .. } => {
                for hunk in hunks {
                    let heading = if hunk.section_heading.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", hunk.section_heading)
                    };
                    if !emit(row(
                        format!(
                            "@@ -{},{} +{},{} @@{}",
                            hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len, heading
                        ),
                        RowTone::Hunk,
                    )) {
                        return;
                    }
                    for line in &hunk.lines {
                        let (marker, tone) = match line.kind {
                            LineKind::Added => ('+', RowTone::Added),
                            LineKind::Removed => ('-', RowTone::Removed),
                            LineKind::Context => (' ', RowTone::Plain),
                        };
                        if !emit(row(format!("{marker}{}", line.text), tone)) {
                            return;
                        }
                        if line.no_newline_at_eof
                            && !emit(row("\\ No newline at end of file", RowTone::Muted))
                        {
                            return;
                        }
                    }
                }
            }
            FileDiff::ModeChangeOnly {
                old_mode, new_mode, ..
            } => {
                if !emit(row(
                    format!("mode changed from {old_mode} to {new_mode}"),
                    RowTone::Muted,
                )) {
                    return;
                }
            }
            FileDiff::Binary { .. } => {
                if !emit(row("binary file — contents not shown", RowTone::Muted)) {
                    return;
                }
            }
            FileDiff::Renamed {
                similarity,
                is_copy,
                ..
            } => {
                if !emit(row(
                    format!(
                        "{} with no content change ({similarity}% similar)",
                        if *is_copy { "copied" } else { "renamed" }
                    ),
                    RowTone::Muted,
                )) {
                    return;
                }
            }
            FileDiff::Combined { raw, .. } => {
                if raw.is_empty() {
                    if !emit(row("(empty combined diff)", RowTone::Muted)) {
                        return;
                    }
                } else {
                    for line in raw.lines() {
                        if !emit(row(line, RowTone::Muted)) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

struct RowWindow {
    start: usize,
    limit: usize,
    seen: usize,
    rows: Vec<DetailRow>,
}

impl RowWindow {
    fn new(start: usize, limit: usize) -> Self {
        Self {
            start,
            limit,
            seen: 0,
            rows: Vec::with_capacity(limit),
        }
    }

    /// `false` means the requested window is full and projection can stop;
    /// no off-screen row after that point is materialized.
    fn push(&mut self, row: DetailRow) -> bool {
        if self.limit == 0 {
            return false;
        }
        if self.seen >= self.start && self.rows.len() < self.limit {
            self.rows.push(row);
        }
        self.seen = self.seen.saturating_add(1);
        self.rows.len() < self.limit
    }
}

#[cfg(test)]
mod tests {
    use git_vista_core::diff::DiffFile;
    use git_vista_core::model::Oid;
    use git_vista_core::status::ChangeKind;

    use super::*;

    const ID: &str = "1111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222";
    const PARENT_1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn detail(id: &str, parents: &[&str], message: &str) -> CommitDetail {
        CommitDetail {
            id: Oid(id.to_string()),
            parents: parents.iter().map(|id| Oid((*id).to_string())).collect(),
            author_name: "Ada Author".to_string(),
            author_email: "ada@example.com".to_string(),
            author_time: 1_700_000_001,
            committer_name: "Casey Committer".to_string(),
            committer_email: "casey@example.com".to_string(),
            commit_time: 1_700_000_099,
            message: message.to_string(),
            on_remote: true,
        }
    }

    fn diff(id: &str, patch: &str) -> CommitDiff {
        CommitDiff {
            id: id.to_string(),
            files: Vec::new(),
            patch: patch.to_string(),
            truncated: false,
            against_first_parent: false,
        }
    }

    fn all_rows(pane: &DetailPane) -> Vec<DetailRow> {
        pane.window(0, pane.row_count())
    }

    #[test]
    fn metadata_full_message_and_each_selectable_parent_are_rows() {
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        assert!(pane.receive_detail(
            "worktree-1",
            ID,
            Ok(detail(ID, &[PARENT_1, PARENT_2], "subject\n\nwhole body")),
        ));

        let rows = all_rows(&pane);
        let text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        for expected in [
            "Ada Author <ada@example.com> · 1700000001",
            "Casey Committer <casey@example.com> · 1700000099",
            "subject",
            "whole body",
            PARENT_1,
            PARENT_2,
        ] {
            assert!(
                text.iter().any(|row| row.contains(expected)),
                "missing {expected:?}: {text:?}"
            );
        }
        assert_eq!(pane.selected_parent(), Some(PARENT_1));
        pane.select_parent(1);
        assert_eq!(pane.selected_parent(), Some(PARENT_2));
        assert!(all_rows(&pane)
            .iter()
            .any(|row| row.text.contains(PARENT_2) && row.tone == RowTone::SelectedParent));
    }

    #[test]
    fn binary_files_are_labelled_and_never_emitted_as_content() {
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        pane.receive_detail("worktree-1", ID, Ok(detail(ID, &[], "binary change")));
        let mut payload = diff(
            ID,
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n",
        );
        payload.files.push(DiffFile {
            path: "logo.png".to_string(),
            old_path: None,
            kind: ChangeKind::Modified,
            additions: None,
            deletions: None,
        });
        assert!(pane.receive_diff("worktree-1", ID, Ok(payload)));

        let rows = all_rows(&pane);
        assert!(rows
            .iter()
            .any(|row| row.text.contains("logo.png") && row.text.contains("binary")));
        assert!(rows
            .iter()
            .any(|row| row.text == "binary file — contents not shown"));
        assert!(
            !rows
                .iter()
                .any(|row| row.text.contains("Binary files a/logo.png")),
            "the parser's safe binary note must replace raw patch noise"
        );
    }

    #[test]
    fn a_large_patch_returns_only_the_requested_window() {
        let body = (0..200)
            .map(|n| format!("+line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let patch = format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,200 @@\n{body}\n"
        );
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        pane.receive_detail("worktree-1", ID, Ok(detail(ID, &[], "many lines")));
        pane.receive_diff("worktree-1", ID, Ok(diff(ID, &patch)));

        assert!(pane.row_count() > 200);
        let (window, visited) = pane.window_with_visit_count(80, 7);
        assert_eq!(
            window.len(),
            7,
            "the view must not materialize off-screen rows"
        );
        assert_eq!(
            visited, 87,
            "projection must stop after skipping 80 rows and building the seven-row viewport"
        );
        assert_eq!(window, pane.window(0, pane.row_count())[80..87]);
    }

    #[test]
    fn parsed_add_remove_and_context_lines_keep_distinct_tones() {
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n";
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        pane.receive_diff("worktree-1", ID, Ok(diff(ID, patch)));

        let rows = all_rows(&pane);
        assert!(rows
            .iter()
            .any(|row| row.text == "-old" && row.tone == RowTone::Removed));
        assert!(rows
            .iter()
            .any(|row| row.text == "+new" && row.tone == RowTone::Added));
        assert!(rows
            .iter()
            .any(|row| row.text == " same" && row.tone == RowTone::Plain));
    }

    #[test]
    fn stale_detail_and_diff_answers_cannot_replace_the_open_commit() {
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        pane.open("worktree-1".to_string(), OTHER.to_string());

        assert!(!pane.receive_detail("worktree-1", ID, Ok(detail(ID, &[], "stale detail"))));
        assert!(!pane.receive_diff("worktree-1", ID, Ok(diff(ID, "+stale diff"))));
        assert!(pane.receive_detail(
            "worktree-1",
            OTHER,
            Ok(detail(OTHER, &[], "current detail"))
        ));

        let text: Vec<String> = all_rows(&pane).into_iter().map(|row| row.text).collect();
        assert!(text.iter().any(|row| row == "current detail"));
        assert!(!text.iter().any(|row| row.contains("stale")));
    }

    #[test]
    fn opening_another_commit_resets_parent_and_horizontal_positions() {
        let mut pane = DetailPane::default();
        pane.open("worktree-1".to_string(), ID.to_string());
        pane.receive_detail(
            "worktree-1",
            ID,
            Ok(detail(ID, &[PARENT_1, PARENT_2], "first")),
        );
        pane.select_parent(1);
        pane.scroll_horizontal(12);
        assert_eq!(pane.selected_parent(), Some(PARENT_2));
        assert_eq!(pane.horizontal(), 12);

        pane.open("worktree-1".to_string(), OTHER.to_string());
        assert_eq!(pane.selected_parent(), None);
        assert_eq!(pane.horizontal(), 0);
    }
}
