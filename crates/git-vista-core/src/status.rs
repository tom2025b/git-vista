//! Working-tree status: the shared types and the parser for
//! `git status --porcelain=v2 --branch` output.
//!
//! The parser lives *here* — not in the server that runs the command — because
//! it's pure string → struct logic: keeping it in core makes it unit-testable
//! with no repo and shares the resulting [`RepoStatus`] type with the wasm
//! frontend across the JSON boundary, exactly like the graph models.
//!
//! Porcelain **v2** (not v1) is the input on purpose: it's git's
//! machine-readable, stability-guaranteed format, and `--branch` adds header
//! lines carrying the checked-out branch, its upstream, and the ahead/behind
//! counts — everything the topbar chip and the Activity panel's status section
//! show. The format (from git-status(1)):
//!
//! ```text
//! # branch.oid <commit> | (initial)
//! # branch.head <name>  | (detached)
//! # branch.upstream <upstream>          only when an upstream is set
//! # branch.ab +<ahead> -<behind>        only when it can be computed
//! 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>                 changed
//! 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>
//! u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>       unmerged
//! ? <path>                                                     untracked
//! ! <path>                                                     ignored
//! ```
//!
//! In `<XY>`, `X` is the staged (index) state and `Y` the worktree state; `.`
//! means unchanged on that side. One file can be dirty on *both* sides (e.g.
//! staged then edited again), so it legitimately appears in both `staged` and
//! `unstaged` here — the UI shows exactly that.

use serde::{Deserialize, Serialize};

/// What kind of change a file underwent, folded to the four states the UI has
/// glyphs for (icons.rs: added/modified/deleted/renamed). git's `T` (type
/// change) reads as a modification and `C` (copy) as a rename — the nuance
/// isn't worth a fifth badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    /// Map one side of the porcelain `<XY>` pair to a kind. `None` for `.`
    /// (unchanged on that side) — the caller skips those entirely.
    fn from_letter(letter: u8) -> Option<ChangeKind> {
        match letter {
            b'A' => Some(ChangeKind::Added),
            b'M' | b'T' => Some(ChangeKind::Modified),
            b'D' => Some(ChangeKind::Deleted),
            b'R' | b'C' => Some(ChangeKind::Renamed),
            _ => None, // '.' and anything unexpected: not a change on this side
        }
    }
}

/// One changed file on one side (staged or unstaged) of the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The path as shown to the user. For a rename this is the *new* path.
    pub path: String,
    pub kind: ChangeKind,
}

/// The parsed working-tree status — the payload of `GET /api/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatus {
    /// The checked-out branch; `None` => detached HEAD.
    pub branch: Option<String>,
    /// The branch's upstream (e.g. `origin/main`), when one is set.
    pub upstream: Option<String>,
    /// Commits ahead of / behind the upstream. Both 0 when there's no
    /// upstream (or git couldn't compute them).
    pub ahead: u32,
    pub behind: u32,
    /// Files with staged (index) changes.
    pub staged: Vec<FileChange>,
    /// Files with unstaged (worktree) changes.
    pub unstaged: Vec<FileChange>,
    /// Untracked files (`?` records).
    pub untracked: Vec<String>,
    /// Unmerged paths (`u` records) — conflict markers in the tree.
    pub conflicted: Vec<String>,
}

impl RepoStatus {
    /// True when nothing is staged, modified, untracked or conflicted — the
    /// state the topbar chip shows as a green check.
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty()
            && self.unstaged.is_empty()
            && self.untracked.is_empty()
            && self.conflicted.is_empty()
    }

    /// Total number of dirty paths, for the chip's "N changes" label. A file
    /// dirty on both sides counts twice — the chip counts *changes*, not files,
    /// which keeps the number honest against the panel's two lists.
    pub fn change_count(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len() + self.conflicted.len()
    }
}

/// Parse the complete stdout of `git status --porcelain=v2 --branch`.
///
/// Unknown record types and malformed lines are *skipped*, not errors: the
/// format is versioned and stable, so anything unrecognized is either a future
/// git addition (ignore it and keep working) or line noise. The worst outcome
/// of skipping is an undercount — never a failed status call.
pub fn parse_porcelain_v2(text: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    for line in text.lines() {
        match line.as_bytes().first() {
            Some(b'#') => parse_header(line, &mut status),
            Some(b'1') => parse_changed(line, &mut status),
            Some(b'2') => parse_renamed(line, &mut status),
            Some(b'u') => {
                // Unmerged: 10 space-separated fields before the path.
                if let Some(path) = nth_field_rest(line, 10) {
                    status.conflicted.push(unquote_path(path));
                }
            }
            Some(b'?') => {
                if let Some(path) = line.strip_prefix("? ") {
                    status.untracked.push(unquote_path(path));
                }
            }
            // '!' (ignored) and anything unknown: deliberately skipped.
            _ => {}
        }
    }
    status
}

/// One `# branch.*` header line.
fn parse_header(line: &str, status: &mut RepoStatus) {
    if let Some(head) = line.strip_prefix("# branch.head ") {
        // "(detached)" is git's literal marker for a detached HEAD.
        if head != "(detached)" {
            status.branch = Some(head.to_string());
        }
    } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
        status.upstream = Some(upstream.to_string());
    } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
        // "+<ahead> -<behind>", e.g. "+2 -0".
        for part in ab.split_whitespace() {
            if let Some(n) = part.strip_prefix('+') {
                status.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix('-') {
                status.behind = n.parse().unwrap_or(0);
            }
        }
    }
    // branch.oid is not surfaced: the graph already knows every tip.
}

/// A `1` record: ordinary changed entry — 8 space-separated fields, then the
/// path (which may itself contain spaces, so it's "the rest", not a field).
fn parse_changed(line: &str, status: &mut RepoStatus) {
    let (Some(xy), Some(path)) = (nth_field(line, 1), nth_field_rest(line, 8)) else {
        return;
    };
    push_sides(xy, unquote_path(path), status);
}

/// A `2` record: rename/copy — 9 fields, then `<newPath>\t<origPath>`. The UI
/// shows the new path; the original is dropped (the rename glyph carries the
/// meaning, and the detail panel's diff names both anyway).
fn parse_renamed(line: &str, status: &mut RepoStatus) {
    let (Some(xy), Some(paths)) = (nth_field(line, 1), nth_field_rest(line, 9)) else {
        return;
    };
    let new_path = paths.split('\t').next().unwrap_or(paths);
    push_sides(xy, unquote_path(new_path), status);
}

/// File a path under `staged` and/or `unstaged` according to its `<XY>` pair.
fn push_sides(xy: &str, path: String, status: &mut RepoStatus) {
    let bytes = xy.as_bytes();
    if bytes.len() != 2 {
        return;
    }
    if let Some(kind) = ChangeKind::from_letter(bytes[0]) {
        status.staged.push(FileChange { path: path.clone(), kind });
    }
    if let Some(kind) = ChangeKind::from_letter(bytes[1]) {
        status.unstaged.push(FileChange { path, kind });
    }
}

/// The `n`-th (0-based) space-separated field of `line`.
fn nth_field(line: &str, n: usize) -> Option<&str> {
    line.split_ascii_whitespace().nth(n)
}

/// Everything after the `n`-th space-separated field — i.e. the path tail of a
/// record, which may contain spaces and so can't be read as a plain field.
/// Walks the raw bytes so runs of separators inside quoted paths don't confuse
/// the count: fields before the path never contain spaces in porcelain v2.
fn nth_field_rest(line: &str, n: usize) -> Option<&str> {
    let mut rest = line;
    for _ in 0..n {
        let idx = rest.find(' ')?;
        rest = rest[idx + 1..].trim_start_matches(' ');
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Undo git's C-style path quoting. Without `-z`, git wraps a path containing
/// specials in double quotes and escapes it (`"a\"b\nc"`, octal for
/// non-ASCII). Quoted paths are rare — this handles the common escapes and
/// falls back to the raw text rather than failing, since a slightly-odd label
/// beats a lost row.
fn unquote_path(path: &str) -> String {
    let inner = match path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
        Some(inner) => inner,
        None => return path.to_string(),
    };
    let mut out = String::with_capacity(inner.len());
    let mut bytes = inner.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b as char);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push('\n'),
            Some(b't') => out.push('\t'),
            Some(b'\\') => out.push('\\'),
            Some(b'"') => out.push('"'),
            // Octal escape (\ooo): up to three digits, one raw byte. Multi-byte
            // UTF-8 comes out as its bytes pushed in order, which round-trips
            // through the String as long as it was valid UTF-8 to begin with.
            Some(d @ b'0'..=b'7') => {
                let mut val = (d - b'0') as u32;
                for _ in 0..2 {
                    match bytes.peek() {
                        Some(d @ b'0'..=b'7') => {
                            val = val * 8 + (*d - b'0') as u32;
                            bytes.next();
                        }
                        _ => break,
                    }
                }
                out.push(val as u8 as char);
            }
            Some(other) => {
                out.push('\\');
                out.push(other as char);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_repo_with_branch_headers() {
        let text = "# branch.oid 1234567890abcdef1234567890abcdef12345678\n\
                    # branch.head main\n\
                    # branch.upstream origin/main\n\
                    # branch.ab +2 -1\n";
        let s = parse_porcelain_v2(text);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert!(s.is_clean());
        assert_eq!(s.change_count(), 0);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let s = parse_porcelain_v2("# branch.oid abc\n# branch.head (detached)\n");
        assert_eq!(s.branch, None);
    }

    #[test]
    fn changed_records_split_by_side() {
        // Staged-only add, unstaged-only modify, and one file dirty on both
        // sides (staged modify + further worktree modify).
        let text = "\
1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 new.rs
1 .M N... 100644 100644 100644 2222222222222222222222222222222222222222 2222222222222222222222222222222222222222 edited.rs
1 MM N... 100644 100644 100644 3333333333333333333333333333333333333333 4444444444444444444444444444444444444444 both.rs
";
        let s = parse_porcelain_v2(text);
        assert_eq!(
            s.staged,
            vec![
                FileChange { path: "new.rs".into(), kind: ChangeKind::Added },
                FileChange { path: "both.rs".into(), kind: ChangeKind::Modified },
            ]
        );
        assert_eq!(
            s.unstaged,
            vec![
                FileChange { path: "edited.rs".into(), kind: ChangeKind::Modified },
                FileChange { path: "both.rs".into(), kind: ChangeKind::Modified },
            ]
        );
        assert_eq!(s.change_count(), 4); // both.rs counts once per side
    }

    #[test]
    fn paths_with_spaces_survive() {
        let text = "1 .M N... 100644 100644 100644 5555555555555555555555555555555555555555 5555555555555555555555555555555555555555 dir name/file with spaces.txt\n";
        let s = parse_porcelain_v2(text);
        assert_eq!(s.unstaged[0].path, "dir name/file with spaces.txt");
    }

    #[test]
    fn rename_takes_the_new_path() {
        let text = "2 R. N... 100644 100644 100644 6666666666666666666666666666666666666666 6666666666666666666666666666666666666666 R100 new/name.rs\told/name.rs\n";
        let s = parse_porcelain_v2(text);
        assert_eq!(
            s.staged,
            vec![FileChange { path: "new/name.rs".into(), kind: ChangeKind::Renamed }]
        );
        assert!(s.unstaged.is_empty());
    }

    #[test]
    fn untracked_and_conflicted() {
        let text = "\
? scratch.txt
u UU N... 100644 100644 100644 100644 7777777777777777777777777777777777777777 8888888888888888888888888888888888888888 9999999999999999999999999999999999999999 clash.rs
";
        let s = parse_porcelain_v2(text);
        assert_eq!(s.untracked, vec!["scratch.txt".to_string()]);
        assert_eq!(s.conflicted, vec!["clash.rs".to_string()]);
        assert!(!s.is_clean());
    }

    #[test]
    fn ignored_and_unknown_records_are_skipped() {
        let s = parse_porcelain_v2("! target/\nz weird future record\n\n");
        assert!(s.is_clean());
    }

    #[test]
    fn quoted_paths_are_unescaped() {
        assert_eq!(unquote_path(r#""a\"b.txt""#), "a\"b.txt");
        assert_eq!(unquote_path(r#""tab\there""#), "tab\there");
        assert_eq!(unquote_path(r#""oct\101l""#), "octAl");
        // Not quoted: returned verbatim.
        assert_eq!(unquote_path("plain.txt"), "plain.txt");
    }
}
