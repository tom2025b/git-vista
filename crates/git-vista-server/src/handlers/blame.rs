//! `GET /api/file-history` and `GET /api/blame`: rename-aware file history and
//! line-range blame, both paged, cancellable, and explicit about rename
//! limits, binary files and absent paths (M5.33, #86).
//!
//! Both endpoints share one path-classification step ([`classify_path`]) and
//! one rename-limit-scanning step (`git_vista_protocol::blame::
//! scan_rename_limit_warnings`) before doing their own kind of read. Neither
//! endpoint retains any server-side state across requests — cursors and line
//! ranges are re-derived from scratch on every call, the same stateless
//! posture ADR 0022 established for paged commit history.
//!
//! # Cancellation, and exactly how far it goes
//!
//! The reads that carry the data — the blob, the history walk, the blame —
//! go through [`crate::git_cmd::git_stdout_capped`] and
//! [`crate::git_cmd::git_stdout_stderr_capped`], which set
//! `.kill_on_drop(true)`. Axum drops a handler's future when the client
//! disconnects, which drops those children with it (ADR 0022's mechanism,
//! reused rather than reinvented).
//!
//! **The short exit-code probes do not have that property, and this doc used
//! to claim they did.** `git cat-file -e` runs through
//! [`crate::git_cmd::git_output`], whose `.output()` never sets
//! `kill_on_drop` — tokio's default is `false`. A dropped future therefore
//! leaves those probes to finish on their own. They are bounded by what they
//! are (one object-existence check: no walk, no output to speak of) rather
//! than by a kill signal, which is why this is recorded honestly instead of
//! being fixed by widening a shared helper every other caller depends on.
//! Found in review (#86): the original sentence asserted `git_output` sets
//! the flag, and it does not.

use std::path::Path;

use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use git_vista_protocol::blame::{
    parse_follow_history, parse_line_porcelain_blame, scan_rename_limit_warnings, BlamePage,
    BlameRange, FileHistoryPage, PathState, RenameLimitNotice,
};

use crate::git_cmd::{git_output, git_stdout_capped, git_stdout_stderr_capped};
use crate::handlers::read::resolve_repo;

/// Upper bound on one file-history/blame metadata read — same cap family as
/// `handlers::read`'s diff/file caps (2 MiB). Crossing it is a `413`: a short
/// read here would parse into a plausible, wrong history or blame result,
/// exactly the failure `commit_diff_for_repo`'s own metadata cap refuses.
const METADATA_CAP: usize = 8 * 1024 * 1024;

/// Upper bound on how many rename hops [`classify_path`] will chase.
///
/// # What this bounds, precisely (corrected in #86 review)
///
/// A successful hop costs **three** spawns, not two as an earlier version of
/// this comment said: `git log --diff-filter=D -1`, then an unrestricted
/// `git show --name-status`, then a `git cat-file -e` liveness probe on the
/// destination. Plus one `cat-file -e` before the walk begins. So 20 hops is
/// bounded by ~61 spawns, not 40.
///
/// And it bounds the **spawn count only**. Each `git log --diff-filter=D -1`
/// may walk from `rev` back to the commit that removed the name, so the work
/// inside a spawn still depends on history depth — the earlier claim of
/// "never `O(history size)`" was too strong. What the cap genuinely prevents
/// is an unbounded *chain* of spawns from a pathological or adversarial
/// rename cascade. 20 is generous: a file renamed 20 times in one
/// repository's life is already extraordinary, and exceeding it now produces
/// [`PathState::RenameChainTooLong`] rather than a confident wrong answer.
const MAX_RENAME_HOPS: u32 = 20;

fn process_error(endpoint: &str, e: std::io::Error) -> (StatusCode, String) {
    eprintln!("git-vista: {endpoint} couldn't run git: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Couldn't run git: {e}"),
    )
}

// ---------------------------------------------------------------------------
// Path classification, shared by both endpoints
// ---------------------------------------------------------------------------

/// Whether `path` is a real blob at `rev`, and if not, what happened to it.
///
/// The existence check is `git cat-file -e <rev>:<path>` — an exit-code-only
/// probe, never text-matched against git's stderr — because
/// [`crate::git_cmd::git_cat_file_batch`]'s own `<id>^:<path>` parent
/// fallback (built for the file-viewer's "show what a deleting commit
/// deleted" behaviour) is the wrong tool here: this function needs to know
/// whether the path is alive at *exactly* the requested revision, not at its
/// parent.
async fn classify_path(
    repo: &Path,
    rev: &str,
    path: &str,
    endpoint: &str,
) -> Result<(PathState, Option<Vec<u8>>), (StatusCode, String)> {
    let spec = format!("{rev}:{path}");
    let exists = git_output(repo, &["cat-file", "-e", &spec])
        .await
        .map_err(|e| process_error(endpoint, e))?
        .status
        .success();

    if exists {
        let (bytes, truncated) = git_stdout_capped(
            repo,
            &["show".to_string(), spec, "--no-textconv".to_string()],
            endpoint,
            METADATA_CAP,
        )
        .await?;
        if truncated {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("{path} is larger than this server will read for blame/history."),
            ));
        }
        // Same sniff `handlers::read::file_at_commit_for_repo` uses: a NUL in
        // the first 8000 bytes. Bounding the read cannot change this verdict
        // because the cap is far larger than 8000 bytes.
        let binary = bytes.iter().take(8000).any(|&b| b == 0);
        if binary {
            return Ok((PathState::Binary, Some(bytes)));
        }
        return Ok((PathState::Readable, Some(bytes)));
    }

    Ok((chase_rename_chain(repo, rev, path, endpoint).await?, None))
}

/// Walk forward through however many renames separate a now-dead `path` from
/// wherever its identity ended up, bounded by [`MAX_RENAME_HOPS`].
///
/// Why this can't be answered with one `git log --follow`: verified directly
/// (see `docs/adr/0124-a-rename-is-followed-forward-by-walking-not-by-asking-follow.md`) that `--follow` only resolves a full rename
/// record when the queried name is the *immediate* predecessor of the file's
/// identity at the log's starting point. Query it with a name that is itself
/// two or more renames stale and `--follow` degrades the very next rename
/// into a bare delete — the ADD side of a rename record disappears from a
/// pathspec-restricted diff the moment it stops being the literal string
/// being searched for. So each hop here:
///
/// 1. Finds the most recent commit that removed exactly `current`
///    (`git log --diff-filter=D -1 --format=%H -- current`) — a plain,
///    pathspec-restricted search, which is exactly the operation that
///    degrades renames to deletes, used here deliberately for that property:
///    it always finds *a* commit that ended `current`'s life under that name,
///    rename or not.
/// 2. Re-examines that one commit **unrestricted** (`git show --name-status
///    -M<sim>%`, no pathspec) to recover the rename record a pathspec would
///    have hidden — the ADD side is only visible without the pathspec filter.
/// 3. If that commit's diff pairs `current` with a new path, and the new path
///    is alive at `rev`, the chain resolves: [`PathState::RenamedAway`]. If
///    the new path is *also* dead, `current` becomes the new path and the
///    loop repeats. If no pairing is found, the trail ends in a genuine
///    deletion: [`PathState::Deleted`].
async fn chase_rename_chain(
    repo: &Path,
    rev: &str,
    path: &str,
    endpoint: &str,
) -> Result<PathState, (StatusCode, String)> {
    let mut current = path.to_string();
    let mut last_commit: Option<String> = None;

    for _ in 0..MAX_RENAME_HOPS {
        let Some(commit) = last_commit_removing(repo, rev, &current, endpoint).await? else {
            return Ok(match last_commit {
                None => PathState::NeverExisted,
                // A hop resolved a new name that this search then could not
                // find any removal for at all — it should be alive, but
                // `classify_path`'s own `cat-file -e` already said it is not.
                // Treated conservatively as a deletion at the last commit
                // this walk could actually name, rather than asserting a
                // state the evidence does not support.
                Some(prev) => PathState::Deleted { last_commit: prev },
            });
        };

        match rename_target_in_commit(repo, &commit, &current, endpoint).await? {
            Some(new_path) => {
                let spec = format!("{rev}:{new_path}");
                let alive = git_output(repo, &["cat-file", "-e", &spec])
                    .await
                    .map_err(|e| process_error(endpoint, e))?
                    .status
                    .success();
                if alive {
                    return Ok(PathState::RenamedAway {
                        last_commit: commit,
                        current_path: new_path,
                    });
                }
                current = new_path;
                last_commit = Some(commit);
            }
            None => {
                return Ok(PathState::Deleted {
                    last_commit: commit,
                })
            }
        }
    }

    // Hop budget exhausted. NOT `RenamedAway`: the loop only reaches here
    // because every destination it found was proven dead at this revision, so
    // calling the last one `current_path` would assert the opposite of what
    // was just measured (#86 review). The incomplete answer gets its own
    // state and says how far it got.
    Ok(PathState::RenameChainTooLong {
        last_commit: last_commit.unwrap_or_else(|| path.to_string()),
        last_known_path: current,
        hops: MAX_RENAME_HOPS,
    })
}

/// The most recent commit reachable from `rev` whose diff removed the exact
/// literal path `path` — `None` if no commit ever did (the path never
/// existed under this exact name in this history).
async fn last_commit_removing(
    repo: &Path,
    rev: &str,
    path: &str,
    endpoint: &str,
) -> Result<Option<String>, (StatusCode, String)> {
    let args = [
        "log".to_string(),
        rev.to_string(),
        "-1".to_string(),
        "--diff-filter=D".to_string(),
        "--format=%H".to_string(),
        "--".to_string(),
        path.to_string(),
    ];
    let (bytes, truncated) = git_stdout_capped(repo, &args, endpoint, METADATA_CAP).await?;
    if truncated {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Too much history to search for this path's deletion.".to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    let hash = text.trim();
    if hash.is_empty() {
        Ok(None)
    } else {
        Ok(Some(hash.to_string()))
    }
}

/// Whether `commit`'s own diff (against its first parent; a root commit's
/// diff is against the empty tree) pairs `old_path` with a new path via
/// rename or copy detection — read from git's **unrestricted** name-status
/// listing (no pathspec), because a pathspec-restricted listing hides the ADD
/// side of a rename whose OLD name is the only side matching the pathspec
/// (see [`chase_rename_chain`]'s doc for the verified reasoning).
async fn rename_target_in_commit(
    repo: &Path,
    commit: &str,
    old_path: &str,
    endpoint: &str,
) -> Result<Option<String>, (StatusCode, String)> {
    // `--no-patch`/`-s` is deliberately absent: it is git's *other* diff-output
    // selector and is refused outright when combined with `--name-status`
    // ("options '--name-only', '--name-status', '--check', and '-s' cannot be
    // used together" — hit and fixed while writing this function's tests).
    // `--format=` alone already suppresses the pretty-printed commit header.
    let args = [
        "show".to_string(),
        "--format=".to_string(),
        "-M50%".to_string(),
        "--name-status".to_string(),
        "-z".to_string(),
        commit.to_string(),
    ];
    let (bytes, truncated) = git_stdout_capped(repo, &args, endpoint, METADATA_CAP).await?;
    if truncated {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Commit {commit} changed too many files to search for a rename."),
        ));
    }
    Ok(git_vista_core::diff::parse_name_status_z(&bytes)
        .into_iter()
        .find(|f| f.old_path.as_deref() == Some(old_path))
        .map(|f| f.path))
}

// ---------------------------------------------------------------------------
// GET /api/file-history
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct FileHistoryQuery {
    path: String,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    skip: Option<usize>,
    #[serde(default)]
    repo: Option<String>,
}

/// Default page size for `/api/file-history` — generous for a single file's
/// commit list (nowhere near the byte volume a full-graph page carries), and
/// small enough that `skip`'s inherent re-walk-from-zero cost (the same
/// accepted quadratic-over-a-full-scroll tradeoff ADR 0022 took for commit
/// history) stays cheap for the common case of a file with tens of commits.
const HISTORY_PAGE_SIZE: usize = 100;

pub(crate) async fn file_history(
    Query(q): Query<FileHistoryQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let page = file_history_for_repo(
        &repo,
        q.rev.as_deref().unwrap_or("HEAD"),
        &q.path,
        q.skip.unwrap_or(0),
        HISTORY_PAGE_SIZE,
        "/api/file-history",
    )
    .await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(page)))
}

async fn file_history_for_repo(
    repo: &Path,
    rev: &str,
    path: &str,
    skip: usize,
    page_size: usize,
    endpoint: &str,
) -> Result<FileHistoryPage, (StatusCode, String)> {
    let (path_state, _) = classify_path(repo, rev, path, endpoint).await?;
    // A path that is `Readable`/`Binary` still has a history — its content at
    // `rev` says nothing about how it got there. A genuinely absent path
    // (never existed) has none, and that is reported without spawning `log`
    // at all: a `NeverExisted` classification already answered the question.
    if matches!(path_state, PathState::NeverExisted) {
        return Ok(FileHistoryPage {
            path: path.to_string(),
            rev: rev.to_string(),
            entries: Vec::new(),
            cursor: None,
            path_state,
            rename_limit_hits: Vec::new(),
        });
    }

    // The path to walk history from: a live/binary path walks from itself; an
    // absent-but-once-renamed-away path still has a history *before* the
    // rename, reached by walking from its last known name.
    let walk_from = match &path_state {
        PathState::RenamedAway { current_path, .. } => current_path.clone(),
        _ => path.to_string(),
    };

    let args = [
        "log".to_string(),
        "--follow".to_string(),
        // No `-l` here, and that absence is the decision (#86 review).
        // `-l<n>` is a per-invocation override of `diff.renameLimit`, so the
        // first version of this line passed `-l<i32::MAX>` while its own
        // trailing comment claimed the limit was "detected, not overridden".
        // It did the exact opposite of what it said: it replaced the
        // repository's configured policy AND suppressed the very warning this
        // feature exists to surface. Omitting the flag is what makes the
        // repository's own limit apply and its warning appear.
        "-z".to_string(),
        "--name-status".to_string(),
        "--format=%x00%H%x09%an%x09%at%x09%s".to_string(),
        format!("--skip={skip}"),
        format!("--max-count={page_size}"),
        rev.to_string(),
        "--".to_string(),
        walk_from,
    ];
    // Both streams from ONE child: the rename-limit warning is on stderr, and
    // re-running the command to fetch it (as this used to) was an uncapped,
    // un-killable second history walk whose output could disagree with the
    // first — see `git_stdout_stderr_capped`.
    let (bytes, errs, truncated) =
        git_stdout_stderr_capped(repo, &args, endpoint, METADATA_CAP).await?;
    if truncated {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "This file's history is too large to read in one page.".to_string(),
        ));
    }
    let entries = parse_follow_history(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't parse file history: {e}"),
        )
    })?;

    let cursor = if entries.len() == page_size {
        Some((skip + page_size).to_string())
    } else {
        None
    };

    // Rename-limit detection, from the same child's stderr. A hit anywhere in
    // this page's walk means this page's chain may be missing a rename; the
    // notice does not claim WHICH commit, because git's warning does not say
    // and an earlier version's guess (the newest entry of whatever page was in
    // hand) could name a commit with nothing to do with it.
    let rename_limit_hits = scan_rename_limit_warnings(&String::from_utf8_lossy(&errs));

    Ok(FileHistoryPage {
        path: path.to_string(),
        rev: rev.to_string(),
        entries,
        cursor,
        path_state,
        rename_limit_hits,
    })
}

// ---------------------------------------------------------------------------
// GET /api/blame
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct BlameQuery {
    path: String,
    #[serde(default)]
    rev: Option<String>,
    /// 1-based, inclusive. Both default to the whole file's first page — see
    /// [`BLAME_PAGE_LINES`].
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    repo: Option<String>,
}

/// Default page height for `/api/blame` — how many lines one request blames
/// when the client does not ask for a specific window.
///
/// It bounds the returned window and the porcelain this server parses, and
/// nothing else: git's own walk still costs what the target lines' distance
/// from the requested revision costs, and `classify_path` reads the whole
/// blob once regardless (#86 review corrected an earlier claim here that the
/// cost was proportional to what is shown). 500 is large enough that most
/// real source files arrive in one page.
const BLAME_PAGE_LINES: usize = 500;

pub(crate) async fn blame(
    Query(q): Query<BlameQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let page = blame_for_repo(
        &repo,
        q.rev.as_deref().unwrap_or("HEAD"),
        &q.path,
        q.start,
        q.end,
        "/api/blame",
    )
    .await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(page)))
}

async fn blame_for_repo(
    repo: &Path,
    rev: &str,
    path: &str,
    start: Option<usize>,
    end: Option<usize>,
    endpoint: &str,
) -> Result<BlamePage, (StatusCode, String)> {
    let (path_state, content) = classify_path(repo, rev, path, endpoint).await?;

    if !matches!(path_state, PathState::Readable) {
        // Binary and every absent variant refuse blame outright rather than
        // returning an empty or nonsense range list — see `PathState`'s own
        // doc for why binary specifically is a refusal, not an empty result.
        return Ok(BlamePage {
            path: path.to_string(),
            rev: rev.to_string(),
            ranges: Vec::new(),
            start_line: start.unwrap_or(1),
            end_line: end.unwrap_or(1),
            total_lines: 0,
            path_state,
            rename_limit_hits: Vec::new(),
        });
    }

    let content = content.unwrap_or_default();
    let total_lines = count_lines(&content);
    let start_line = start.unwrap_or(1).max(1);
    let end_line = end
        .unwrap_or_else(|| (start_line + BLAME_PAGE_LINES - 1).min(total_lines.max(1)))
        .min(total_lines.max(1));

    if total_lines == 0 {
        return Ok(BlamePage {
            path: path.to_string(),
            rev: rev.to_string(),
            ranges: Vec::new(),
            start_line,
            end_line: start_line,
            total_lines: 0,
            path_state,
            rename_limit_hits: Vec::new(),
        });
    }
    if start_line > total_lines {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{path} has only {total_lines} lines; line {start_line} is past the end."),
        ));
    }

    let range_arg = format!("-L{start_line},{end_line}");
    let args = [
        "blame".to_string(),
        "--line-porcelain".to_string(),
        "-M50%".to_string(),
        range_arg,
        rev.to_string(),
        "--".to_string(),
        path.to_string(),
    ];
    let (bytes, errs, truncated) =
        git_stdout_stderr_capped(repo, &args, endpoint, METADATA_CAP).await?;
    if truncated {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "This blame page is too large to read in one request.".to_string(),
        ));
    }
    let ranges: Vec<BlameRange> = parse_line_porcelain_blame(&bytes).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't parse blame output: {e}"),
        )
    })?;

    let rename_limit_hits: Vec<RenameLimitNotice> =
        scan_rename_limit_warnings(&String::from_utf8_lossy(&errs));

    Ok(BlamePage {
        path: path.to_string(),
        rev: rev.to_string(),
        ranges,
        start_line,
        end_line,
        total_lines,
        path_state,
        rename_limit_hits,
    })
}

/// A file with `n` newlines and a trailing newline has `n` lines; one with
/// `n` newlines and no trailing newline (or any non-empty content — the
/// common case for a file not ending in `\n`) has `n + 1`. An empty file is
/// zero lines, not one — `git blame` itself refuses `-L1,1` on an empty file
/// ("has only 0 lines"), so reporting one line for it would offer a blame
/// range the underlying tool cannot honour.
fn count_lines(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.iter().filter(|&&b| b == b'\n').count();
    if content.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_fixtures::git as gf;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        gf::init(dir.path());
        dir
    }

    #[test]
    fn count_lines_matches_git_blames_own_notion_of_line_count() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
        assert_eq!(count_lines(b"a\nb\nc\n"), 3);
    }

    #[tokio::test]
    async fn a_never_existed_path_is_classified_without_a_history_walk() {
        let dir = repo();
        gf::write(dir.path(), "a.txt", b"hi\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "c1"]);

        let page = file_history_for_repo(dir.path(), "HEAD", "nope.txt", 0, 10, "test")
            .await
            .unwrap();
        assert_eq!(page.path_state, PathState::NeverExisted);
        assert!(page.entries.is_empty());
        assert!(page.cursor.is_none());
    }

    #[tokio::test]
    async fn a_readable_file_classifies_readable_and_blames_correctly() {
        let dir = repo();
        gf::write(dir.path(), "a.txt", b"one\ntwo\nthree\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add a"]);

        let (state, content) = classify_path(dir.path(), "HEAD", "a.txt", "test")
            .await
            .unwrap();
        assert_eq!(state, PathState::Readable);
        assert_eq!(content.unwrap(), b"one\ntwo\nthree\n");

        let page = blame_for_repo(dir.path(), "HEAD", "a.txt", None, None, "test")
            .await
            .unwrap();
        assert_eq!(page.total_lines, 3);
        assert_eq!(page.ranges.len(), 1, "all three lines came from one commit");
        assert_eq!(page.ranges[0].start_line, 1);
        assert_eq!(page.ranges[0].end_line, 3);
    }

    #[tokio::test]
    async fn a_binary_file_refuses_blame_rather_than_returning_nonsense_ranges() {
        let dir = repo();
        gf::write(dir.path(), "bin.dat", &[0u8, 1, 2, 3, 0, 4, 5]);
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add binary"]);

        let page = blame_for_repo(dir.path(), "HEAD", "bin.dat", None, None, "test")
            .await
            .unwrap();
        assert_eq!(page.path_state, PathState::Binary);
        assert!(page.ranges.is_empty());
    }

    #[tokio::test]
    async fn a_one_hop_rename_is_classified_and_history_spans_both_names() {
        let dir = repo();
        gf::write(dir.path(), "old.txt", b"hello\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add old"]);
        gf::run(dir.path(), &["mv", "old.txt", "new.txt"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "rename old to new"]);

        let (state, _) = classify_path(dir.path(), "HEAD", "old.txt", "test")
            .await
            .unwrap();
        assert_eq!(
            state,
            PathState::RenamedAway {
                last_commit: gf::out(dir.path(), &["rev-parse", "HEAD"]),
                current_path: "new.txt".to_string(),
            }
        );

        let page = file_history_for_repo(dir.path(), "HEAD", "old.txt", 0, 10, "test")
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 2, "history spans both names");
        assert_eq!(page.entries[0].path, "new.txt");
        assert_eq!(page.entries[0].renamed_from.as_deref(), Some("old.txt"));
        assert_eq!(page.entries[1].path, "old.txt");
    }

    /// The empirically-discovered gap `chase_rename_chain`'s doc explains:
    /// `--follow` alone misreports a two-hop-stale name as a plain delete.
    /// This proves the iterative chaser gets the *right* answer where a
    /// single `--follow` call would not.
    #[tokio::test]
    async fn a_two_hop_rename_chain_resolves_to_the_files_true_current_name() {
        let dir = repo();
        gf::write(dir.path(), "a.txt", b"hello\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add a"]);
        gf::run(dir.path(), &["mv", "a.txt", "b.txt"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "rename a to b"]);
        gf::run(dir.path(), &["mv", "b.txt", "c.txt"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "rename b to c"]);

        let (state, _) = classify_path(dir.path(), "HEAD", "a.txt", "test")
            .await
            .unwrap();
        match state {
            PathState::RenamedAway { current_path, .. } => {
                assert_eq!(
                    current_path, "c.txt",
                    "must chase through the intermediate b.txt hop"
                );
            }
            other => panic!("expected RenamedAway to c.txt, got {other:?}"),
        }

        // The middle name resolves in exactly one hop.
        let (state, _) = classify_path(dir.path(), "HEAD", "b.txt", "test")
            .await
            .unwrap();
        match state {
            PathState::RenamedAway { current_path, .. } => assert_eq!(current_path, "c.txt"),
            other => panic!("expected RenamedAway to c.txt, got {other:?}"),
        }
    }

    /// Exhausting the hop cap must not claim the file is at the last name it
    /// tried (#86 review). Every hop only continues because its destination
    /// was just proven absent, so `RenamedAway { current_path }` there would
    /// assert the opposite of what the code measured one line earlier.
    #[tokio::test]
    async fn exhausting_the_hop_cap_reports_incompleteness_not_a_false_location() {
        let dir = repo();
        gf::write(dir.path(), "n0.txt", b"hello\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add n0"]);
        // One more rename than the chase will follow, so the walk runs out
        // before it reaches the live name.
        let renames = MAX_RENAME_HOPS + 1;
        for i in 0..renames {
            gf::run(
                dir.path(),
                &["mv", &format!("n{i}.txt"), &format!("n{}.txt", i + 1)],
            );
            gf::run(dir.path(), &["commit", "-q", "-m", &format!("rename {i}")]);
        }

        let (state, _) = classify_path(dir.path(), "HEAD", "n0.txt", "test")
            .await
            .unwrap();
        match state {
            PathState::RenameChainTooLong {
                last_known_path,
                hops,
                ..
            } => {
                assert_eq!(hops, MAX_RENAME_HOPS);
                // The lead is real but is NOT where the file is: the live
                // name is the last one, which the walk never reached.
                assert_ne!(
                    last_known_path,
                    format!("n{renames}.txt"),
                    "the walk cannot have reached the live name — that is the premise"
                );
                // THE point of this state. The furthest name reached is dead
                // at HEAD — which is exactly why the old code calling it
                // `current_path` was a false statement, and why this variant
                // says "lead" rather than "location".
                assert!(
                    !gf::try_run(
                        dir.path(),
                        &["cat-file", "-e", &format!("HEAD:{last_known_path}")]
                    ),
                    "last_known_path ({last_known_path}) must NOT be alive — if it \
                     were, the chase should have returned RenamedAway"
                );
            }
            other => panic!("expected RenameChainTooLong, got {other:?}"),
        }
    }

    /// The control: one hop under the cap still resolves normally, so the
    /// test above is about the CAP and not about long chains generally.
    #[tokio::test]
    async fn a_chain_just_under_the_cap_still_resolves_to_the_live_name() {
        let dir = repo();
        gf::write(dir.path(), "m0.txt", b"hello\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add m0"]);
        let renames = MAX_RENAME_HOPS - 1;
        for i in 0..renames {
            gf::run(
                dir.path(),
                &["mv", &format!("m{i}.txt"), &format!("m{}.txt", i + 1)],
            );
            gf::run(dir.path(), &["commit", "-q", "-m", &format!("rename {i}")]);
        }
        let (state, _) = classify_path(dir.path(), "HEAD", "m0.txt", "test")
            .await
            .unwrap();
        match state {
            PathState::RenamedAway { current_path, .. } => {
                assert_eq!(current_path, format!("m{renames}.txt"));
            }
            other => panic!("expected RenamedAway, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_genuine_deletion_is_told_apart_from_a_rename() {
        let dir = repo();
        gf::write(dir.path(), "gone.txt", b"bye\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add gone"]);
        gf::run(dir.path(), &["rm", "-q", "gone.txt"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "remove gone"]);

        let (state, _) = classify_path(dir.path(), "HEAD", "gone.txt", "test")
            .await
            .unwrap();
        assert_eq!(
            state,
            PathState::Deleted {
                last_commit: gf::out(dir.path(), &["rev-parse", "HEAD"]),
            }
        );
    }

    #[tokio::test]
    async fn blame_across_a_rename_boundary_carries_the_previous_name() {
        let dir = repo();
        gf::write(dir.path(), "old.txt", b"one\ntwo\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add old"]);
        gf::run(dir.path(), &["mv", "old.txt", "new.txt"]);
        gf::write(dir.path(), "new.txt", b"one\ntwo\nthree\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "rename and extend"]);

        let page = blame_for_repo(dir.path(), "HEAD", "new.txt", None, None, "test")
            .await
            .unwrap();
        assert_eq!(page.total_lines, 3);
        assert_eq!(page.ranges.len(), 2, "the new line starts a second range");
        assert_eq!(page.ranges[0].path, "old.txt");
        assert_eq!(page.ranges[1].path, "new.txt");
        assert_eq!(page.ranges[1].renamed_from.as_deref(), Some("old.txt"));
    }

    #[tokio::test]
    async fn blame_pages_a_line_range_without_reading_the_whole_file() {
        let dir = repo();
        let lines: Vec<String> = (1..=50).map(|n| format!("line{n}")).collect();
        gf::write(dir.path(), "big.txt", (lines.join("\n") + "\n").as_bytes());
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add big"]);

        let page = blame_for_repo(dir.path(), "HEAD", "big.txt", Some(10), Some(15), "test")
            .await
            .unwrap();
        assert_eq!(page.total_lines, 50);
        assert_eq!(page.start_line, 10);
        assert_eq!(page.end_line, 15);
        assert_eq!(page.ranges.len(), 1);
        assert_eq!(page.ranges[0].start_line, 10);
        assert_eq!(page.ranges[0].end_line, 15);
    }

    #[tokio::test]
    async fn a_start_line_past_the_end_of_the_file_is_a_client_error_not_a_crash() {
        let dir = repo();
        gf::write(dir.path(), "a.txt", b"one\n");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add a"]);

        let err = blame_for_repo(dir.path(), "HEAD", "a.txt", Some(100), None, "test")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_empty_file_blames_to_zero_lines_not_one() {
        let dir = repo();
        gf::write(dir.path(), "empty.txt", b"");
        gf::run(dir.path(), &["add", "-A"]);
        gf::run(dir.path(), &["commit", "-q", "-m", "add empty"]);

        let page = blame_for_repo(dir.path(), "HEAD", "empty.txt", None, None, "test")
            .await
            .unwrap();
        assert_eq!(page.total_lines, 0);
        assert!(page.ranges.is_empty());
    }
}

#[cfg(test)]
mod perf_suite;
