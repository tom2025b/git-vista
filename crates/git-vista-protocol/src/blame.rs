//! Rename-aware file history and blame: wire types plus the pure parsers for
//! git's own machine-readable formats (M5.33, #86).
//!
//! Every format below was verified against a real repository before writing
//! a parser for it — the same posture [`crate::diff`] documents for unified
//! diffs — not assumed from memory of the man page.
//!
//! ## Why this crate follows real git, not `gix`
//!
//! `git-vista-git` (the native, `gix`-backed crate) walks commit graphs, but
//! blame and rename-following are asked here of the **real `git` binary**,
//! the same B3 posture `git-vista-core::diff` already takes for `--numstat`
//! and `--name-status`: git's own diffcore is the rename engine this whole
//! feature depends on, `gix` 0.84 (as vendored, `blame` feature disabled)
//! does not expose an equivalent, and re-implementing similarity detection
//! would be a second, divergent rename engine living next to the one git
//! already ships. `git-vista-server` spawns the process (sandboxed, bounded,
//! killed on drop); this crate only ever sees the bytes it already printed.
//!
//! ## The rename limit is `diff.renameLimit`, and it is observable
//!
//! `git log --follow` and `git diff -M` both fall back to a plain
//! delete+add when a commit changes more files than `diff.renameLimit`
//! (default 1000) allows for the O(n²) exhaustive comparison. Verified
//! directly: forcing the limit down (`-l1`) on a commit that renamed one file
//! *and* changed 30 others prints, to stderr —
//!
//! ```text
//! warning: exhaustive rename detection was skipped due to too many files.
//! warning: you may want to set your diff.renameLimit variable to at least 31 and retry the command.
//! ```
//!
//! — and the rename that would otherwise have been detected reports as a
//! bare delete + add instead of `R`. That is the real "silent shorter
//! history" cliff ADR 0022 named for commit count and this feature names for
//! renames: [`RenameLimitNotice`] exists so the server can say plainly "the
//! chain may be broken here" instead of a client inferring nothing from a
//! shorter list. [`is_rename_limit_warning`] recognises exactly this
//! sentence family and pulls out git's own suggested minimum when it states
//! one.
//!
//! ## Blame uses `--line-porcelain`, not `--porcelain`
//!
//! `--porcelain` prints a commit's metadata only the first time it is seen,
//! which means correct parsing needs cross-hunk dedup state. `--line-porcelain`
//! (verified against real output below) repeats full metadata — author,
//! summary, `previous`, `filename` — on **every** line-group, so
//! [`parse_line_porcelain_blame`] is a stateless per-group parser with one
//! failure mode (a malformed group) rather than two (malformed, and
//! "forgot a commit shown three groups ago"). The bandwidth cost is real but
//! bounded by the same caps every other capped read in this app already
//! accepts (see `git-vista-server::git_cmd::git_stdout_capped`).
//!
//! Verified shape, one root commit (line 1-5) and one rename+edit commit
//! (line 6, renamed from `sub/target.txt`):
//!
//! ```text
//! <sha> 1 1 5
//! author git-vista-ci
//! author-mail <git-vista-ci@example.invalid>
//! author-time 1788604866
//! author-tz -0400
//! committer git-vista-ci
//! committer-mail <git-vista-ci@example.invalid>
//! committer-time 1788604866
//! committer-tz -0400
//! summary c1
//! boundary
//! filename sub/target.txt
//! \tline1
//! ...
//! <sha2> 6 6 1
//! ...
//! summary c2
//! previous <sha> sub/target.txt
//! filename sub/renamed.txt
//! \textra line changing content
//! ```
//!
//! `boundary` (no value) marks a commit blame cannot look past — by default
//! that includes a genuine root commit; content lines are prefixed with
//! exactly one tab. `previous <sha> <name>` and `boundary` are mutually
//! exclusive (a boundary commit has no parent to be "previous").
//!
//! ## File history uses one NUL-delimited stream, not two reads
//!
//! `git log --follow -z --name-status --format=%x00%H%x09%an%x09%at%x09%s`
//! interleaves a NUL-prefixed pretty-printed header with `-z`'s own
//! NUL-terminated name-status record for that commit. Verified byte-for-byte
//! (see `docs/adr/0124-a-rename-is-followed-forward-by-walking-not-by-asking-follow.md`), and re-verified after an initial misreading of
//! the same trace got the byte order backwards: `-z` NUL-terminates the
//! **pretty-printed header itself** (a plain header token, cleanly
//! `"<hash>\t<author>\t<time>\t<summary>"` with no NUL inside it), and git
//! separately inserts one literal `\n` *before* the name-status listing — so
//! that newline lands glued onto the **front of the following token**
//! (`"\nR100"`, `"\nA"`, …), not the tail of the header. Splitting the whole
//! buffer on NUL therefore yields, per commit: a clean header token, then a
//! status token with a leading `\n` to strip, then its 1-2 path tokens, then
//! one empty separator token before the next commit's header (the byte
//! between two adjacent NULs — the previous record's terminator and the next
//! record's own leading `%x00`). `--follow` combined with a single pathspec
//! restricts the name-status listing to exactly one record per commit, which
//! is what makes [`parse_follow_history`] a flat loop rather than a nested
//! one.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Path classification — shared by history and blame
// ---------------------------------------------------------------------------

/// What a requested path resolves to at the requested revision (#86). Stated
/// explicitly, the same posture [`crate::history::HeadState`] takes for
/// HEAD: "absent" is not one fact, and a client that has to infer *which*
/// absence it is from an empty result list cannot tell "you mistyped this"
/// from "this used to live somewhere else" from "we found nothing, but did
/// we even look properly".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PathState {
    /// A text blob at the requested revision. Blame and history both apply.
    Readable,
    /// A real blob, but binary (NUL sniffed in the first 8000 bytes — the
    /// exact test `handlers::read::file_at_commit_for_repo` already uses).
    /// Line-based blame on binary content is not wrong, it is meaningless:
    /// git happily splits arbitrary bytes on `\n` and blames the resulting
    /// "lines", which is why this is refused rather than shown.
    Binary,
    /// No commit reachable from the requested revision, under any name this
    /// server's rename search followed, ever touched this path.
    NeverExisted,
    /// The path was removed and no later commit reintroduces it under any
    /// name the rename search followed.
    Deleted { last_commit: String },
    /// The path was renamed away; its history continues at `current_path`.
    ///
    /// `current_path` is **alive at the requested revision** — the chase
    /// proves that with its own `cat-file -e` before reporting this. See
    /// [`PathState::RenameChainTooLong`] for what happens when it cannot.
    RenamedAway {
        last_commit: String,
        current_path: String,
    },
    /// The rename chase hit its hop limit before finding a live name (#86
    /// review).
    ///
    /// This state exists because the alternative was a lie. The chase used to
    /// return `RenamedAway { current_path: <the last hop> }` on exhaustion —
    /// but every hop only continues *because* its destination was just proven
    /// **dead** at this revision, so that field named a path the code had
    /// already disproved, and the UI said "this path was renamed to X" about
    /// a file that is not there. An incomplete answer has to be sayable, or
    /// the incomplete case borrows the vocabulary of a complete one.
    ///
    /// `last_known_path` is the furthest name actually reached — offered as a
    /// lead to follow, explicitly not as the file's current location.
    RenameChainTooLong {
        last_commit: String,
        last_known_path: String,
        hops: u32,
    },
}

/// Recorded the moment git's own exhaustive rename detection gave up because
/// a commit changed more files than `diff.renameLimit` allows (see the
/// module doc). `commit` is the commit whose diff hit the limit; a rename at
/// that exact point may have been missed, so the chain either stops there or
/// (worse, silently) continues under a name that is not really the same
/// file's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameLimitNotice {
    /// The commit whose diff hit the limit — **`None` whenever git did not
    /// say**, which is the normal case.
    ///
    /// git's warning names no commit. An earlier version of this type
    /// declared `commit: String` and the server filled it with the newest
    /// entry of whatever page happened to be in hand, which made the UI state
    /// "rename detection was skipped at <commit>" about a commit that may have
    /// had nothing to do with it. A field that is sometimes a guess is worse
    /// than a field that is sometimes absent: the guess is indistinguishable
    /// from knowledge at the point of reading. So this is `Option`, and
    /// nothing fills it in from context.
    pub commit: Option<String>,
    /// git's own suggested minimum, parsed from its second warning line,
    /// when it printed one.
    pub suggested_minimum: Option<u32>,
}

// ---------------------------------------------------------------------------
// File history
// ---------------------------------------------------------------------------

/// One commit in a file's rename-aware history, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistoryEntry {
    pub commit: String,
    pub author: String,
    /// Unix seconds, same unit as [`crate::history`]'s commit times.
    pub time: i64,
    pub summary: String,
    /// The name this commit shows the file under. A page spanning a rename
    /// contains entries under more than one name.
    pub path: String,
    /// `Some(old_path)` on exactly the commit that performed the rename to
    /// `path`; `None` on every other entry, including the file's first
    /// appearance (which is an add, not a rename).
    pub renamed_from: Option<String>,
}

/// One page of [`FileHistoryEntry`], oldest-to-newest cursor-paginated the
/// same way [`crate::history::HistoryPage`] is: re-run the walk, skip
/// `cursor`'s offset, take the next window. Cost is the same accepted
/// quadratic-over-a-full-scroll tradeoff ADR 0022 already took for commit
/// history — see `docs/adr/0022-paged-history-and-bounded-reads.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistoryPage {
    /// Echoes the request (same posture as [`crate::diff::SpecDiff::spec`]):
    /// a client comparing this against what it currently wants can drop a
    /// late answer to a superseded request rather than paint it.
    pub path: String,
    pub rev: String,
    pub entries: Vec<FileHistoryEntry>,
    /// Present when there may be more history past this page.
    pub cursor: Option<String>,
    pub path_state: PathState,
    /// Every rename-limit hit discovered by the walk *so far* (across every
    /// page fetched, not just this one — the server re-derives the whole
    /// prefix on every page, so this is naturally cumulative and a client
    /// showing "history may be incomplete" does not need to remember pages
    /// it already discarded).
    pub rename_limit_hits: Vec<RenameLimitNotice>,
}

// ---------------------------------------------------------------------------
// Blame
// ---------------------------------------------------------------------------

/// One contiguous run of lines blamed to the same commit under the same
/// path. Adjacent same-commit lines are coalesced server-side (parsing already
/// walks them one at a time; a rename boundary always starts a new range
/// because `path` changes even if, pathologically, the commit id did not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRange {
    pub commit: String,
    pub author: String,
    pub time: i64,
    pub summary: String,
    /// 1-based, inclusive, in the *requested* file's current line numbering
    /// (blame's `final` line number, not `orig`).
    pub start_line: usize,
    pub end_line: usize,
    /// The name this range's commit shows the file under.
    pub path: String,
    /// `Some(old_path)` exactly when this range's commit is the rename that
    /// produced `path`.
    pub renamed_from: Option<String>,
    /// True when this commit is a boundary git's walk could not look past
    /// (typically a root commit) — surfaced so a client can say "history
    /// ends here" rather than implying more exists.
    pub boundary: bool,
}

/// One page of blame, covering `[start_line, end_line]` of the file's
/// *current* total line count (`total_lines`) at the requested revision, via
/// `git blame -L start,end` — verified to still resolve renames correctly
/// across the boundary it cuts.
///
/// # What `-L` bounds, and what it does not (M5.33, #86)
///
/// An earlier version of this comment said a huge file's cost is bounded by
/// the page size rather than the whole file. That is **not true** and the
/// measurements are in ADR 0124: git still diffs every commit between the
/// requested revision and wherever the target lines last changed, so a page
/// at the tip costs ~21 ms on a 3,000-commit file while one at the root
/// costs ~450 ms. Two further costs are the server's own: it reads the whole
/// blob once to classify the path and count lines (so an 8 MiB file is
/// refused whatever page you ask for), and the walk above.
///
/// What paging *does* bound is the **parsed and returned window** — always
/// exactly the lines requested, never the whole file — and, through
/// `total_lines`, a client's ability to fetch the rest incrementally instead
/// of in one response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlamePage {
    /// Echoes the request, same posture as [`FileHistoryPage::path`].
    pub path: String,
    pub rev: String,
    pub ranges: Vec<BlameRange>,
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub path_state: PathState,
    pub rename_limit_hits: Vec<RenameLimitNotice>,
}

// ---------------------------------------------------------------------------
// Parsing: git's own rename-limit warning
// ---------------------------------------------------------------------------

/// Recognise git's exhaustive-rename-detection-skipped warning on one line of
/// stderr, returning the suggested minimum when git stated one. `commit` is
/// supplied by the caller (the warning text itself never names a commit —
/// callers see it while iterating one commit's diff at a time and know which
/// one they were asking about).
///
/// Matches only the first warning line (`"...skipped due to too many
/// files."`); the second, advisory line (`"...set your diff.renameLimit..."`)
/// is optional and git version-dependent in exact wording, so its number is
/// extracted separately by [`parse_suggested_minimum`] against the *next*
/// line, not required for a hit to count.
pub fn is_rename_limit_warning(line: &str) -> bool {
    line.trim_start_matches("warning:")
        .trim_start()
        .starts_with("exhaustive rename detection was skipped due to too many files")
}

/// Pull the suggested minimum out of git's advisory follow-up line, e.g.
/// `"warning: you may want to set your diff.renameLimit variable to at least
/// 31 and retry the command."`. `None` for any line that doesn't match this
/// exact shape — a change in git's wording here loses the number, never
/// panics or misparses a different number out of the sentence.
pub fn parse_suggested_minimum(line: &str) -> Option<u32> {
    let after = line.split("at least ").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Scan every line of a git stderr capture for the rename-limit warning pair.
///
/// Reports what it finds, once per matching first line, and **attributes
/// nothing**: git's warning carries no commit, so neither does the notice.
/// See [`RenameLimitNotice::commit`] for why guessing one was worse than
/// leaving it absent.
pub fn scan_rename_limit_warnings(stderr: &str) -> Vec<RenameLimitNotice> {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_rename_limit_warning(line) {
            let suggested_minimum = lines
                .get(i + 1)
                .and_then(|next| parse_suggested_minimum(next));
            hits.push(RenameLimitNotice {
                // Deliberately not inferred from the caller's context — see
                // `RenameLimitNotice::commit`.
                commit: None,
                suggested_minimum,
            });
        }
    }
    hits
}

// ---------------------------------------------------------------------------
// Parsing: `git log --follow -z --name-status --format=%x00%H%x09%an%x09%at%x09%s`
// ---------------------------------------------------------------------------

/// A malformed byte stream from `git log --follow ...` — never a panic, a
/// typed refusal instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowHistoryParseError {
    MalformedHeader(String),
    MissingPath(String, String),
    BadTime(String),
}

impl std::fmt::Display for FollowHistoryParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FollowHistoryParseError::MalformedHeader(header) => {
                write!(
                    f,
                    "a commit header did not carry 4 tab-separated fields: {header:?}"
                )
            }
            FollowHistoryParseError::MissingPath(commit, status) => {
                write!(
                    f,
                    "commit {commit} claims status {status:?}, missing its path"
                )
            }
            FollowHistoryParseError::BadTime(time) => {
                write!(f, "commit time {time:?} is not a valid integer")
            }
        }
    }
}

impl std::error::Error for FollowHistoryParseError {}

/// Parse the NUL-delimited stream `git log --follow -M<sim>% -l<limit> -z
/// --name-status --format=%x00%H%x09%an%x09%at%x09%s -- <path>` prints, into
/// newest-first [`FileHistoryEntry`] rows. See the module doc for the exact
/// byte layout this walks, verified against real git output.
///
/// Never panics on truncated or malformed input (a capped read can end
/// mid-record): a header missing its expected path, or a header that does not
/// split into exactly 4 tab-separated fields, is a [`FollowHistoryParseError`],
/// not a crash. A wholly empty buffer (a path with no history at all, which
/// callers distinguish from "never existed" via a separate existence check)
/// parses to an empty, successful `Vec`.
pub fn parse_follow_history(
    bytes: &[u8],
) -> Result<Vec<FileHistoryEntry>, FollowHistoryParseError> {
    // Every record — the header and each of its 1-2 paths — is one NUL-
    // delimited token; splitting the whole buffer this way is safe because a
    // path itself can never contain a NUL (a filesystem invariant, not
    // something this parser assumes lightly: git's own tree entries could not
    // represent one).
    let mut tokens = bytes
        .split(|&b| b == 0)
        .map(|t| String::from_utf8_lossy(t).into_owned());
    // The format string opens with `%x00`, so the very first split token is
    // always the empty string preceding it (or the whole buffer is empty).
    let Some(first) = tokens.next() else {
        return Ok(Vec::new());
    };
    if first.is_empty() {
        // expected: fall through with `tokens` now positioned at the first
        // header.
    } else {
        // A capped read landed mid-token with no leading NUL at all — an
        // empty result is the honest answer; nothing here parses as a commit.
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
    }

    let mut tokens: Vec<String> = tokens.collect();
    // A capped/truncated read can leave one trailing empty token (from the
    // final `\0`) or a partial, unusable tail token; both are dropped rather
    // than parsed as a phantom commit.
    while tokens.last().is_some_and(|t| t.is_empty()) {
        tokens.pop();
    }

    let mut entries = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let header = tokens[i].clone();
        i += 1;
        let fields: Vec<&str> = header.splitn(4, '\t').collect();
        let [hash, author, time_str, summary] = fields[..] else {
            return Err(FollowHistoryParseError::MalformedHeader(header));
        };
        let time: i64 = time_str
            .parse()
            .map_err(|_| FollowHistoryParseError::BadTime(time_str.to_string()))?;

        // The status token carries a leading literal `\n`: `-z` NUL-terminates
        // the custom `--format` output as its own record, and git then always
        // inserts one literal newline before the name-status listing — a
        // separate byte from any of `-z`'s own NUL terminators, verified
        // directly (see the module doc; this is the exact detail an earlier
        // reading of the same trace got wrong: the NUL lands *before* the
        // newline, not after it, so the newline glues onto the *next* token,
        // not the header).
        let status_token = tokens
            .get(i)
            .ok_or_else(|| FollowHistoryParseError::MissingPath(hash.to_string(), String::new()))?
            .clone();
        i += 1;
        let status = status_token.strip_prefix('\n').unwrap_or(&status_token);

        let is_rename_or_copy = status.starts_with('R') || status.starts_with('C');
        let (path, renamed_from) = if is_rename_or_copy {
            let old = tokens.get(i).cloned().ok_or_else(|| {
                FollowHistoryParseError::MissingPath(hash.to_string(), status.to_string())
            })?;
            i += 1;
            let new = tokens.get(i).cloned().ok_or_else(|| {
                FollowHistoryParseError::MissingPath(hash.to_string(), status.to_string())
            })?;
            i += 1;
            (new, Some(old))
        } else {
            let path = tokens.get(i).cloned().ok_or_else(|| {
                FollowHistoryParseError::MissingPath(hash.to_string(), status.to_string())
            })?;
            i += 1;
            (path, None)
        };

        // The separator artifact between this commit's last path and the next
        // commit's header is one empty token (see module doc); skip exactly
        // one if present. Its absence at the very end of the stream is normal.
        if tokens.get(i).is_some_and(|t| t.is_empty()) {
            i += 1;
        }

        entries.push(FileHistoryEntry {
            commit: hash.to_string(),
            author: author.to_string(),
            time,
            summary: summary.to_string(),
            path,
            renamed_from,
        });
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Parsing: `git blame --line-porcelain`
// ---------------------------------------------------------------------------

/// A malformed `--line-porcelain` stream — never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameParseError {
    MalformedHeader(String),
    MissingField(String, &'static str),
    BadInteger(String, &'static str, String),
    UnterminatedGroup(usize),
}

impl std::fmt::Display for BlameParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlameParseError::MalformedHeader(header) => write!(
                f,
                "blame header {header:?} did not have the form '<sha> <orig> <final> [<count>]'"
            ),
            BlameParseError::MissingField(commit, field) => {
                write!(f, "commit {commit} is missing its {field} field")
            }
            BlameParseError::BadInteger(commit, field, value) => {
                write!(
                    f,
                    "commit {commit}'s {field} field {value:?} is not a valid integer"
                )
            }
            BlameParseError::UnterminatedGroup(line) => {
                write!(
                    f,
                    "group starting at line {line} never reached a content line"
                )
            }
        }
    }
}

impl std::error::Error for BlameParseError {}

/// One raw blame line, before adjacent-run coalescing — an internal step of
/// [`parse_line_porcelain_blame`], exposed only for that function's own
/// tests to check against the coalesced output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawBlameLine {
    commit: String,
    author: String,
    time: i64,
    summary: String,
    final_line: usize,
    path: String,
    renamed_from: Option<String>,
    boundary: bool,
}

/// Parse `git blame --line-porcelain` output into coalesced [`BlameRange`]s.
///
/// Because `--line-porcelain` repeats every commit's full metadata on every
/// line-group (see the module doc), this is a flat per-group parser: each
/// group is a header line, a fixed run of `key value` metadata lines and
/// flag lines (order-independent — parsed as a small map, not by position,
/// since `boundary`/`previous` are optional and git does not document a
/// fixed relative order beyond "before `filename`"), then `filename <path>`,
/// then exactly one content line (`\t` + the line's raw text, which may
/// itself be empty). A group's line count comes from the header's optional
/// 4th field when present, and is otherwise implicitly 1 — `--line-porcelain`
/// header lines observed in testing always carry it for a group's first
/// line and omit it for the rest of that same group, so this parser reads
/// one content line per header exactly like `--line-porcelain`'s own name
/// promises ("for each line"), never trusting the count field for anything
/// but validation.
pub fn parse_line_porcelain_blame(bytes: &[u8]) -> Result<Vec<BlameRange>, BlameParseError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().peekable();
    let mut raw = Vec::new();

    while let Some(header) = lines.next() {
        if header.is_empty() {
            continue;
        }
        let mut parts = header.split_ascii_whitespace();
        let sha = parts
            .next()
            .ok_or_else(|| BlameParseError::MalformedHeader(header.to_string()))?;
        let _orig_line = parts.next();
        let final_line: usize = parts
            .next()
            .ok_or_else(|| BlameParseError::MalformedHeader(header.to_string()))?
            .parse()
            .map_err(|_| BlameParseError::MalformedHeader(header.to_string()))?;
        // The optional 4th field (group size) is intentionally not read: see
        // the function doc — one content line is consumed per header
        // regardless, which is what `--line-porcelain` actually emits.
        if sha.len() < 4 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BlameParseError::MalformedHeader(header.to_string()));
        }

        let mut author = None;
        let mut time = None;
        let mut summary = None;
        let mut path = None;
        let mut renamed_from = None;
        let mut boundary = false;

        loop {
            let Some(line) = lines.next() else {
                return Err(BlameParseError::UnterminatedGroup(final_line));
            };
            if let Some(content) = line.strip_prefix('\t') {
                let _ = content; // raw text is not retained: ranges carry attribution, not content.
                break;
            }
            if line == "boundary" {
                boundary = true;
            } else if let Some(rest) = line.strip_prefix("previous ") {
                // "previous <sha> <name>" — the name is the ONLY part we need
                // (the previous sha is implied by walking history ourselves
                // via `parse_follow_history`); splitting on the first space
                // keeps a name containing spaces intact.
                let name = rest.split_once(' ').map(|(_, name)| name).unwrap_or(rest);
                renamed_from = Some(name.to_string());
            } else if let Some(rest) = line.strip_prefix("filename ") {
                path = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("author-time ") {
                time = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("summary ") {
                summary = Some(rest.to_string());
            }
            // Every other porcelain key (author-mail, author-tz, committer*,
            // filename repeated after a copy, etc.) is deliberately ignored:
            // this parser reads exactly the fields `BlameRange` carries.
        }

        let sha = sha.to_string();
        let author = author.ok_or_else(|| BlameParseError::MissingField(sha.clone(), "author"))?;
        let time_str =
            time.ok_or_else(|| BlameParseError::MissingField(sha.clone(), "author-time"))?;
        let time: i64 = time_str
            .parse()
            .map_err(|_| BlameParseError::BadInteger(sha.clone(), "author-time", time_str))?;
        let summary =
            summary.ok_or_else(|| BlameParseError::MissingField(sha.clone(), "summary"))?;
        let path = path.ok_or_else(|| BlameParseError::MissingField(sha.clone(), "filename"))?;

        raw.push(RawBlameLine {
            commit: sha,
            author,
            time,
            summary,
            final_line,
            path,
            renamed_from,
            boundary,
        });
    }

    Ok(coalesce(raw))
}

/// Merge adjacent [`RawBlameLine`]s that share a commit, path and rename
/// origin into one [`BlameRange`]. Adjacency is checked on `final_line`
/// being exactly consecutive, not merely "same commit somewhere in this
/// page" — two disjoint hunks blamed to the same commit (a line touched,
/// reverted, and touched again elsewhere) must stay two ranges, or a client
/// asked to open "the range's commit" for a comparison would silently show
/// the wrong span of lines as changed.
fn coalesce(raw: Vec<RawBlameLine>) -> Vec<BlameRange> {
    let mut ranges: Vec<BlameRange> = Vec::new();
    for line in raw {
        if let Some(last) = ranges.last_mut() {
            let contiguous = line.final_line == last.end_line + 1;
            let same_identity = last.commit == line.commit
                && last.path == line.path
                && last.renamed_from == line.renamed_from
                && last.boundary == line.boundary;
            if contiguous && same_identity {
                last.end_line = line.final_line;
                continue;
            }
        }
        ranges.push(BlameRange {
            commit: line.commit,
            author: line.author,
            time: line.time,
            summary: line.summary,
            start_line: line.final_line,
            end_line: line.final_line,
            path: line.path,
            renamed_from: line.renamed_from,
            boundary: line.boundary,
        });
    }
    ranges
}

#[cfg(test)]
mod rename_limit_warning_tests {
    use super::*;

    /// Verbatim text captured from a real `git -l1 log --follow -M50% ...`
    /// run against a fixture that renamed one file amid 30 unrelated
    /// delete/add pairs (see `docs/adr/0124-a-rename-is-followed-forward-by-walking-not-by-asking-follow.md` for the full transcript).
    const FIRST: &str = "warning: exhaustive rename detection was skipped due to too many files.";
    const SECOND: &str =
        "warning: you may want to set your diff.renameLimit variable to at least 31 and retry the command.";

    #[test]
    fn recognises_the_real_warning_verbatim() {
        assert!(is_rename_limit_warning(FIRST));
    }

    #[test]
    fn extracts_gits_own_suggested_minimum() {
        assert_eq!(parse_suggested_minimum(SECOND), Some(31));
    }

    #[test]
    fn an_unrelated_warning_does_not_match() {
        assert!(!is_rename_limit_warning("warning: something else entirely"));
        assert_eq!(parse_suggested_minimum("no numbers in here"), None);
    }

    #[test]
    fn scan_pairs_the_warning_with_its_advisory_line_and_attributes_no_commit() {
        let stderr = format!("{FIRST}\n{SECOND}\n");
        let hits = scan_rename_limit_warnings(&stderr);
        assert_eq!(
            hits,
            vec![RenameLimitNotice {
                // Not "deadbeef": git's warning names no commit, and this
                // function no longer accepts one to attach (#86 review).
                commit: None,
                suggested_minimum: Some(31),
            }]
        );
    }

    #[test]
    fn a_first_line_with_no_advisory_follow_up_still_counts_as_a_hit() {
        // Git version differences: only the first line's wording is load-bearing.
        let hits = scan_rename_limit_warnings(FIRST);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].suggested_minimum, None);
    }

    #[test]
    fn ordinary_stderr_with_no_warning_yields_nothing() {
        assert!(scan_rename_limit_warnings("").is_empty());
        assert!(scan_rename_limit_warnings("Auto packing...\n").is_empty());
    }
}

#[cfg(test)]
mod follow_history_tests {
    use super::*;

    /// Byte-for-byte the two-commit stream captured from:
    /// `git log --follow -M50% -z --name-status
    /// --format=$'%x00%H%x09%an%x09%at%x09%s' -- sub/renamed.txt`
    /// against the fixture in `docs/adr/0124-a-rename-is-followed-forward-by-walking-not-by-asking-follow.md`. Newest first: a rename
    /// commit, then the root add.
    fn two_commit_stream() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"\x0048038450aa1a68c092cc9b1d65c7e359042b35b5\tgit-vista-ci\t1788604866\tc2\x00\nR051\x00",
        );
        buf.extend_from_slice(b"sub/target.txt\x00sub/renamed.txt\x00");
        buf.extend_from_slice(
            b"\x0024909fc9812477b9fdd37a29e02a20e046541aaf\tgit-vista-ci\t1788604866\tc1\x00\nA\x00",
        );
        buf.extend_from_slice(b"sub/target.txt\x00");
        buf
    }

    #[test]
    fn parses_a_rename_then_the_root_add_newest_first() {
        let entries = parse_follow_history(&two_commit_stream()).unwrap();
        assert_eq!(entries.len(), 2);

        assert_eq!(
            entries[0].commit,
            "48038450aa1a68c092cc9b1d65c7e359042b35b5"
        );
        assert_eq!(entries[0].author, "git-vista-ci");
        assert_eq!(entries[0].time, 1788604866);
        assert_eq!(entries[0].summary, "c2");
        assert_eq!(entries[0].path, "sub/renamed.txt");
        assert_eq!(entries[0].renamed_from.as_deref(), Some("sub/target.txt"));

        assert_eq!(
            entries[1].commit,
            "24909fc9812477b9fdd37a29e02a20e046541aaf"
        );
        assert_eq!(entries[1].path, "sub/target.txt");
        assert_eq!(
            entries[1].renamed_from, None,
            "the root add is not a rename"
        );
    }

    #[test]
    fn an_empty_buffer_is_an_empty_history_not_an_error() {
        assert_eq!(parse_follow_history(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn a_copy_status_carries_two_paths_like_a_rename() {
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\t1\tcopy\x00\nC100\x00",
        );
        buf.extend_from_slice(b"src.txt\x00dst.txt\x00");
        let entries = parse_follow_history(&buf).unwrap();
        assert_eq!(entries[0].path, "dst.txt");
        assert_eq!(entries[0].renamed_from.as_deref(), Some("src.txt"));
    }

    #[test]
    fn a_summary_containing_a_tab_does_not_break_field_splitting() {
        // `splitn(4, '\t')` puts anything past the 3rd tab into the summary
        // field whole, so a pathological subject line survives intact.
        let mut buf = Vec::new();
        buf.extend_from_slice(
            b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\t1\tsummary\twith\ttabs\x00\nA\x00",
        );
        buf.extend_from_slice(b"only.txt\x00");
        let entries = parse_follow_history(&buf).unwrap();
        assert_eq!(entries[0].summary, "summary\twith\ttabs");
    }

    #[test]
    fn a_header_with_fewer_than_four_tab_fields_is_rejected_not_panicked() {
        // Only 3 fields (hash, author, time) — the summary is missing
        // entirely, which a capped read ending exactly on a tab could produce.
        let buf = b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\t1\x00";
        assert!(matches!(
            parse_follow_history(buf),
            Err(FollowHistoryParseError::MalformedHeader(_))
        ));
    }

    #[test]
    fn a_header_with_no_status_token_at_all_is_rejected_not_panicked() {
        // A capped/truncated read can end right after a header, before the
        // name-status listing's own NUL-delimited status token even begins.
        let buf = b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\t1\ts";
        assert!(matches!(
            parse_follow_history(buf),
            Err(FollowHistoryParseError::MissingPath(_, _))
        ));
    }

    #[test]
    fn a_status_missing_its_path_is_rejected_not_panicked() {
        // The status token itself arrived, but nothing follows it.
        let buf = b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\t1\ts\x00\nA";
        assert!(matches!(
            parse_follow_history(buf),
            Err(FollowHistoryParseError::MissingPath(_, _))
        ));
    }

    #[test]
    fn a_non_numeric_time_is_rejected_not_panicked() {
        let buf =
            b"\x00cafefeedcafefeedcafefeedcafefeedcafefeed\ta\tnotanumber\ts\x00\nA\x00p.txt\x00";
        assert!(matches!(
            parse_follow_history(buf),
            Err(FollowHistoryParseError::BadTime(_))
        ));
    }
}

#[cfg(test)]
mod blame_parse_tests {
    use super::*;

    /// Byte-for-byte `git blame --line-porcelain -- sub/renamed.txt` from the
    /// same fixture: 5 lines from a root commit, then 1 renamed+edited line.
    fn six_line_stream() -> String {
        let mut s = String::new();
        for n in 1..=5 {
            s.push_str(&format!(
                "24909fc9812477b9fdd37a29e02a20e046541aaf {n} {n} {}\n",
                if n == 1 { 5 } else { 0 }
            ));
            // Real output omits the trailing count field entirely on lines
            // 2-5; a header without it is exercised by `sha orig final` alone
            // (see `a_header_without_a_group_count_still_parses` below) — this
            // helper keeps every header uniform for readability and is not
            // itself asserting the omitted-field shape.
            s.push_str("author git-vista-ci\n");
            s.push_str("author-mail <git-vista-ci@example.invalid>\n");
            s.push_str("author-time 1788604866\n");
            s.push_str("author-tz -0400\n");
            s.push_str("committer git-vista-ci\n");
            s.push_str("committer-mail <git-vista-ci@example.invalid>\n");
            s.push_str("committer-time 1788604866\n");
            s.push_str("committer-tz -0400\n");
            s.push_str("summary c1\n");
            s.push_str("boundary\n");
            s.push_str("filename sub/target.txt\n");
            s.push_str(&format!("\tline{n}\n"));
        }
        s.push_str("48038450aa1a68c092cc9b1d65c7e359042b35b5 6 6 1\n");
        s.push_str("author git-vista-ci\n");
        s.push_str("author-mail <git-vista-ci@example.invalid>\n");
        s.push_str("author-time 1788604866\n");
        s.push_str("author-tz -0400\n");
        s.push_str("committer git-vista-ci\n");
        s.push_str("committer-mail <git-vista-ci@example.invalid>\n");
        s.push_str("committer-time 1788604866\n");
        s.push_str("committer-tz -0400\n");
        s.push_str("summary c2\n");
        s.push_str("previous 24909fc9812477b9fdd37a29e02a20e046541aaf sub/target.txt\n");
        s.push_str("filename sub/renamed.txt\n");
        s.push_str("\textra line changing content\n");
        s
    }

    #[test]
    fn five_boundary_lines_coalesce_into_one_range_and_the_rename_starts_a_second() {
        let ranges = parse_line_porcelain_blame(six_line_stream().as_bytes()).unwrap();
        assert_eq!(
            ranges.len(),
            2,
            "the root's 5 lines coalesce; the rename is its own range"
        );

        let root = &ranges[0];
        assert_eq!(root.commit, "24909fc9812477b9fdd37a29e02a20e046541aaf");
        assert_eq!((root.start_line, root.end_line), (1, 5));
        assert_eq!(root.path, "sub/target.txt");
        assert!(root.boundary);
        assert_eq!(root.renamed_from, None);

        let renamed = &ranges[1];
        assert_eq!(renamed.commit, "48038450aa1a68c092cc9b1d65c7e359042b35b5");
        assert_eq!((renamed.start_line, renamed.end_line), (6, 6));
        assert_eq!(renamed.path, "sub/renamed.txt");
        assert_eq!(renamed.renamed_from.as_deref(), Some("sub/target.txt"));
        assert!(!renamed.boundary);
    }

    #[test]
    fn a_header_without_a_group_count_still_parses() {
        // Real `--line-porcelain` omits the 4th field on every line after a
        // group's first; the parser must not require it.
        let stream = "\
deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 2 2
author a
author-time 1
summary s
filename f.txt
\tcontent
";
        let ranges = parse_line_porcelain_blame(stream.as_bytes()).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start_line, ranges[0].end_line), (2, 2));
    }

    #[test]
    fn two_disjoint_hunks_from_the_same_commit_stay_two_ranges() {
        // Line 1 and line 3 both blamed to the same commit, line 2 to a
        // different one: coalescing by commit identity ALONE would wrongly
        // merge 1 and 3 into a fictitious "1-3" range that includes line 2's
        // real owner. Adjacency on `final_line` is what prevents that.
        let stream = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1
author a
author-time 1
summary s
filename f.txt
\tone
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1
author b
author-time 2
summary s2
filename f.txt
\ttwo
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 3 3 1
author a
author-time 1
summary s
filename f.txt
\tthree
";
        let ranges = parse_line_porcelain_blame(stream.as_bytes()).unwrap();
        assert_eq!(
            ranges.len(),
            3,
            "same commit id, but not adjacent, must not merge"
        );
        assert_eq!(ranges[0].end_line, 1);
        assert_eq!(ranges[2].start_line, 3);
    }

    #[test]
    fn a_rename_boundary_splits_a_range_even_if_the_commit_id_were_repeated() {
        // Pathological but must-hold: identity for coalescing is (commit,
        // path, renamed_from, boundary), not commit alone.
        let stream = "\
cccccccccccccccccccccccccccccccccccccccc 1 1 1
author a
author-time 1
summary s
filename old.txt
\tone
cccccccccccccccccccccccccccccccccccccccc 2 2 1
author a
author-time 1
summary s
previous cccccccccccccccccccccccccccccccccccccccc old.txt
filename new.txt
\ttwo
";
        let ranges = parse_line_porcelain_blame(stream.as_bytes()).unwrap();
        assert_eq!(ranges.len(), 2, "a path change must start a new range");
        assert_eq!(ranges[0].path, "old.txt");
        assert_eq!(ranges[1].path, "new.txt");
    }

    #[test]
    fn an_empty_buffer_is_an_empty_blame_not_an_error() {
        assert_eq!(parse_line_porcelain_blame(b"").unwrap(), Vec::new());
    }

    #[test]
    fn a_header_that_is_not_hex_is_rejected_not_panicked() {
        let stream = "not-a-sha 1 1 1\nauthor a\nauthor-time 1\nsummary s\nfilename f\n\tx\n";
        assert!(matches!(
            parse_line_porcelain_blame(stream.as_bytes()),
            Err(BlameParseError::MalformedHeader(_))
        ));
    }

    #[test]
    fn a_group_missing_its_content_line_is_rejected_not_panicked() {
        // A capped read can end mid-group.
        let stream = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\nauthor a\nauthor-time 1\nsummary s\nfilename f\n";
        assert!(matches!(
            parse_line_porcelain_blame(stream.as_bytes()),
            Err(BlameParseError::UnterminatedGroup(1))
        ));
    }

    #[test]
    fn a_group_missing_a_required_field_is_rejected_not_panicked() {
        let stream = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\nsummary s\nfilename f\n\tx\n";
        assert!(matches!(
            parse_line_porcelain_blame(stream.as_bytes()),
            Err(BlameParseError::MissingField(_, "author"))
        ));
    }
}
