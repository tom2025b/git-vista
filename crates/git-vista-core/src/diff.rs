//! Commit diffs: the shared types and the parsers for git's machine-readable
//! per-file diff listings (`--name-status -z` and `--numstat -z`).
//!
//! Like [`crate::status`], the parsing lives in core — pure bytes → structs,
//! unit-testable with no repo — while the server owns running git. The server
//! asks git for the same diff three ways and this module folds two of them:
//!
//!  * `--name-status -z` — one status letter per file (A/M/D/T/R/C). The
//!    authoritative *file list* and its order.
//!  * `--numstat -z`     — per-file added/deleted line counts (`-` = binary).
//!  * `--patch`          — the unified diff text, passed through verbatim
//!    (size-capped server-side) for the panel to colour line by line.
//!
//! `-z` (NUL-separated) is used for both listings on purpose: it switches off
//! git's C-style path quoting entirely, so paths with spaces, quotes or
//! non-ASCII arrive verbatim and the records are unambiguous to split. The
//! only format subtlety is renames/copies, where the path field becomes *two*
//! NUL-separated fields (old, then new) — both parsers handle that.

use serde::{Deserialize, Serialize};

use crate::status::ChangeKind;

/// One file touched by a commit, for the detail panel's Changes list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    /// The path shown to the user — for a rename, the *new* path.
    pub path: String,
    /// The old path, for renames/copies only — shown as "old → new".
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    /// Added/deleted line counts from `--numstat`; `None` for binary files
    /// (where git prints `-` instead of a number).
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

/// The full diff of one commit — the payload of `GET /api/diff/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDiff {
    /// Full hex id of the commit this diff belongs to, echoed back so the
    /// panel can ignore a stale response after the user switches commits.
    pub id: String,
    pub files: Vec<DiffFile>,
    /// The unified diff text (`--patch --no-color`), possibly truncated.
    pub patch: String,
    /// True when the patch was cut at the server's size cap — the panel says
    /// so instead of silently showing a partial diff.
    pub truncated: bool,
    /// True for a merge commit, whose diff is taken against its *first parent*
    /// (the branch it merged into). A merge's combined diff is usually empty —
    /// the interesting answer to "what did this merge bring in?" is exactly
    /// the first-parent diff — but the panel labels it so nobody is misled.
    pub against_first_parent: bool,
}

impl CommitDiff {
    /// Totals across all files, for the Changes header ("+120 −34").
    pub fn totals(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(a, d), f| {
            (a + f.additions.unwrap_or(0), d + f.deletions.unwrap_or(0))
        })
    }
}

/// Map a `--name-status` letter to the UI's [`ChangeKind`]. Same folding as
/// the status parser: `T` (type change) reads as modified, `C` (copy) as a
/// rename. Unknown letters read as modified rather than dropping the file.
fn kind_from_letter(letter: u8) -> ChangeKind {
    match letter {
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' | b'C' => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

/// Parse `--name-status -z` output: repeating `status NUL path NUL`, where a
/// rename/copy status (`R<score>`/`C<score>`) is followed by *two* paths —
/// old NUL new NUL — in that order.
pub fn parse_name_status_z(bytes: &[u8]) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut tokens = bytes
        .split(|&b| b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| String::from_utf8_lossy(t).into_owned());
    while let Some(status) = tokens.next() {
        let Some(&letter) = status.as_bytes().first() else {
            continue;
        };
        let kind = kind_from_letter(letter);
        let renameish = matches!(letter, b'R' | b'C');
        let (old_path, path) = if renameish {
            // Two path fields: old, then new.
            match (tokens.next(), tokens.next()) {
                (Some(old), Some(new)) => (Some(old), new),
                _ => break, // truncated record: stop rather than misalign
            }
        } else {
            match tokens.next() {
                Some(p) => (None, p),
                None => break,
            }
        };
        files.push(DiffFile { path, old_path, kind, additions: None, deletions: None });
    }
    files
}

/// Fold `--numstat -z` counts into an existing file list (from
/// [`parse_name_status_z`]), matching by (new) path. Numstat records are
/// `added TAB deleted TAB path NUL` — except renames/copies, where the path
/// after the second TAB is empty and the paths follow as two NUL fields
/// (old, then new). `-` for a count marks a binary file: left as `None`.
pub fn fold_numstat_z(bytes: &[u8], files: &mut [DiffFile]) {
    let mut tokens = bytes.split(|&b| b == 0);
    while let Some(head) = tokens.next() {
        if head.is_empty() {
            continue;
        }
        let head = String::from_utf8_lossy(head);
        let mut cols = head.splitn(3, '\t');
        let (Some(added), Some(deleted)) = (cols.next(), cols.next()) else {
            continue;
        };
        // Path in the same token => ordinary entry; empty => rename/copy, with
        // old NUL new following as their own tokens.
        let path = match cols.next() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => {
                let _old = tokens.next();
                match tokens.next() {
                    Some(new) => String::from_utf8_lossy(new).into_owned(),
                    None => break,
                }
            }
        };
        if let Some(file) = files.iter_mut().find(|f| f.path == path) {
            file.additions = added.parse().ok(); // "-" (binary) parses to None
            file.deletions = deleted.parse().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_plain_entries() {
        let bytes = b"A\0new.rs\0M\0src/lib.rs\0D\0gone.txt\0";
        let files = parse_name_status_z(bytes);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "new.rs");
        assert_eq!(files[0].kind, ChangeKind::Added);
        assert_eq!(files[1].kind, ChangeKind::Modified);
        assert_eq!(files[2].kind, ChangeKind::Deleted);
        assert_eq!(files[0].old_path, None);
    }

    #[test]
    fn name_status_rename_consumes_two_paths() {
        let bytes = b"R100\0old/name.rs\0new/name.rs\0M\0other.rs\0";
        let files = parse_name_status_z(bytes);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "new/name.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(files[0].kind, ChangeKind::Renamed);
        assert_eq!(files[1].path, "other.rs");
    }

    #[test]
    fn numstat_counts_fold_in_by_path() {
        let mut files = parse_name_status_z(b"M\0a.rs\0M\0b.rs\0");
        fold_numstat_z(b"10\t2\ta.rs\05\t0\tb.rs\0", &mut files);
        assert_eq!(files[0].additions, Some(10));
        assert_eq!(files[0].deletions, Some(2));
        assert_eq!(files[1].additions, Some(5));
    }

    #[test]
    fn numstat_binary_stays_none() {
        let mut files = parse_name_status_z(b"M\0logo.png\0");
        fold_numstat_z(b"-\t-\tlogo.png\0", &mut files);
        assert_eq!(files[0].additions, None);
        assert_eq!(files[0].deletions, None);
    }

    #[test]
    fn numstat_rename_matches_new_path() {
        let mut files = parse_name_status_z(b"R090\0old.rs\0new.rs\0");
        fold_numstat_z(b"3\t1\t\0old.rs\0new.rs\0", &mut files);
        assert_eq!(files[0].additions, Some(3));
        assert_eq!(files[0].deletions, Some(1));
    }

    #[test]
    fn totals_sum_across_files_skipping_binary() {
        let mut files = parse_name_status_z(b"M\0a.rs\0M\0logo.png\0");
        fold_numstat_z(b"10\t2\ta.rs\0-\t-\tlogo.png\0", &mut files);
        let diff = CommitDiff { files, ..Default::default() };
        assert_eq!(diff.totals(), (10, 2));
    }

    #[test]
    fn paths_with_spaces_and_specials_arrive_verbatim() {
        // -z means no C-quoting: the bytes are the literal path.
        let files = parse_name_status_z(b"M\0dir name/file \"x\".txt\0");
        assert_eq!(files[0].path, "dir name/file \"x\".txt");
    }
}
