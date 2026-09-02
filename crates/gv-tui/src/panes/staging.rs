//! Pure projection of a server-pinned staging diff into terminal rows (#459).
//!
//! A selectable row owns the exact shared wire coordinates needed to build a
//! [`PatchPlan`]. File rows use `EntireFile`, hunk rows copy the hunk ordinal
//! and anchors, and changed-line rows additionally copy their 0-based line
//! index. Context lines remain visible but cannot accidentally become a plan.

use git_vista_protocol::{
    canonical_path, parse_unified_diff, FileDiff, FileSelection, HunkLines, HunkRef, LineKind,
    PatchPlan, RepositoryToken, SelectionShape, StageDirection, StagingDiff, WorktreeToken,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    File,
    Hunk,
    Added,
    Removed,
    Muted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    File {
        path: String,
    },
    Hunk {
        path: String,
        hunk: HunkRef,
    },
    Line {
        path: String,
        hunk: HunkRef,
        line: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub tone: Tone,
    target: Option<Target>,
}

impl Row {
    #[cfg(test)]
    pub fn granularity(&self) -> Option<&'static str> {
        match self.target {
            Some(Target::File { .. }) => Some("file"),
            Some(Target::Hunk { .. }) => Some("hunk"),
            Some(Target::Line { .. }) => Some("line"),
            None => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StagingPane {
    pub direction: StageDirection,
    pub diff: StagingDiff,
    rows: Vec<Row>,
}

impl StagingPane {
    pub fn new(direction: StageDirection, diff: StagingDiff) -> Self {
        let rows = flatten(&diff.patch);
        Self {
            direction,
            diff,
            rows,
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn plan_for_row(
        &self,
        index: usize,
        repository: &str,
        worktree: &str,
    ) -> Result<PatchPlan, String> {
        let target = self
            .rows
            .get(index)
            .and_then(|row| row.target.clone())
            .ok_or_else(|| "Select a file, hunk header, or changed line.".to_string())?;
        let (path, selection) = match target {
            Target::File { path } => (path, SelectionShape::EntireFile),
            Target::Hunk { path, hunk } => (path, SelectionShape::Hunks { hunks: vec![hunk] }),
            Target::Line { path, hunk, line } => (
                path,
                SelectionShape::Lines {
                    hunks: vec![HunkLines {
                        hunk,
                        lines: vec![line],
                    }],
                },
            ),
        };
        let plan = PatchPlan {
            repository: RepositoryToken::new(repository).map_err(|error| error.to_string())?,
            worktree: WorktreeToken::new(worktree).map_err(|error| error.to_string())?,
            generation: self.diff.generation.clone(),
            direction: self.direction,
            files: vec![FileSelection { path, selection }],
        };
        plan.validate().map_err(|error| error.to_string())?;
        Ok(plan)
    }

    pub fn plan_for_file(
        &self,
        path: &str,
        repository: &str,
        worktree: &str,
    ) -> Result<PatchPlan, String> {
        let index = self
            .rows
            .iter()
            .position(|row| matches!(&row.target, Some(Target::File { path: p }) if p == path))
            .ok_or_else(|| format!("The pinned diff has no file at {path}."))?;
        self.plan_for_row(index, repository, worktree)
    }
}

fn flatten(patch: &str) -> Vec<Row> {
    let parsed = parse_unified_diff(patch);
    let mut rows = Vec::new();
    for file in parsed.files {
        let Some(path) = canonical_path(&file).map(str::to_string) else {
            continue;
        };
        rows.push(Row {
            text: format!("file {path}"),
            tone: Tone::File,
            target: Some(Target::File { path: path.clone() }),
        });
        match file {
            FileDiff::Hunks { hunks, .. } => {
                for (index, hunk) in hunks.into_iter().enumerate() {
                    let hunk_ref = HunkRef {
                        index: index as u32,
                        old_start: hunk.old_start,
                        new_start: hunk.new_start,
                    };
                    rows.push(Row {
                        text: format!(
                            "@@ -{},{} +{},{} @@ {}",
                            hunk.old_start,
                            hunk.old_len,
                            hunk.new_start,
                            hunk.new_len,
                            hunk.section_heading
                        ),
                        tone: Tone::Hunk,
                        target: Some(Target::Hunk {
                            path: path.clone(),
                            hunk: hunk_ref,
                        }),
                    });
                    for (line_index, line) in hunk.lines.into_iter().enumerate() {
                        let (marker, tone, selectable) = match line.kind {
                            LineKind::Context => (' ', Tone::Plain, false),
                            LineKind::Added => ('+', Tone::Added, true),
                            LineKind::Removed => ('-', Tone::Removed, true),
                        };
                        rows.push(Row {
                            text: format!("{marker}{}", line.text),
                            tone,
                            target: selectable.then(|| Target::Line {
                                path: path.clone(),
                                hunk: hunk_ref,
                                line: line_index as u32,
                            }),
                        });
                        if line.no_newline_at_eof {
                            rows.push(Row {
                                text: "\\ No newline at end of file".to_string(),
                                tone: Tone::Muted,
                                target: None,
                            });
                        }
                    }
                }
            }
            FileDiff::ModeChangeOnly {
                old_mode, new_mode, ..
            } => rows.push(Row {
                text: format!("mode {old_mode} → {new_mode}"),
                tone: Tone::Muted,
                target: None,
            }),
            FileDiff::Binary { .. } => rows.push(Row {
                text: "binary content".to_string(),
                tone: Tone::Muted,
                target: None,
            }),
            FileDiff::Renamed {
                old_path,
                similarity,
                is_copy,
                ..
            } => rows.push(Row {
                text: format!(
                    "{} from {old_path}, {similarity}% similar",
                    if is_copy { "copied" } else { "renamed" }
                ),
                tone: Tone::Muted,
                target: None,
            }),
            FileDiff::Combined { raw, .. } => rows.extend(raw.lines().map(|line| Row {
                text: line.to_string(),
                tone: Tone::Muted,
                target: None,
            })),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff() -> StagingDiff {
        StagingDiff {
            generation: git_vista_protocol::GenerationToken::new("diff-v1:test").unwrap(),
            patch: "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@ heading\n-old\n+new\n context\n"
                .to_string(),
            truncated: false,
        }
    }

    fn pane() -> StagingPane {
        StagingPane::new(StageDirection::Stage, diff())
    }

    #[test]
    fn file_hunk_and_changed_lines_build_the_three_shared_plan_shapes() {
        let pane = pane();
        let selectable: Vec<(usize, &str)> = pane
            .rows()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.granularity().map(|kind| (index, kind)))
            .collect();
        assert_eq!(
            selectable,
            [(0, "file"), (1, "hunk"), (2, "line"), (3, "line")]
        );

        let file = pane.plan_for_row(0, "repo-1", "wt-1").unwrap();
        assert_eq!(file.generation.as_str(), "diff-v1:test");
        assert_eq!(file.direction, StageDirection::Stage);
        assert!(matches!(
            file.files[0].selection,
            SelectionShape::EntireFile
        ));

        let hunk = pane.plan_for_row(1, "repo-1", "wt-1").unwrap();
        let SelectionShape::Hunks { hunks } = &hunk.files[0].selection else {
            panic!("hunk row built a different granularity");
        };
        assert_eq!(hunks[0].index, 0);
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));

        let line = pane.plan_for_row(3, "repo-1", "wt-1").unwrap();
        let SelectionShape::Lines { hunks } = &line.files[0].selection else {
            panic!("changed line built a different granularity");
        };
        assert_eq!(hunks[0].lines, [1]);
    }

    #[test]
    fn context_and_note_rows_can_never_become_line_plans() {
        let pane = pane();
        let context = pane
            .rows()
            .iter()
            .position(|row| row.text == " context")
            .unwrap();
        let refused = pane.plan_for_row(context, "repo-1", "wt-1").unwrap_err();
        assert!(refused.contains("changed line"), "{refused}");
    }

    #[test]
    fn a_file_shortcut_must_resolve_inside_the_pinned_diff() {
        let pane = pane();
        assert!(pane.plan_for_file("a.txt", "repo-1", "wt-1").is_ok());
        let refused = pane
            .plan_for_file("untracked.txt", "repo-1", "wt-1")
            .unwrap_err();
        assert!(refused.contains("no file"), "{refused}");
    }
}
