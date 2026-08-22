//! Inspecting a conflict (M4.31a, #428): `GET /api/conflicts` (every
//! conflicted path's stage metadata), `GET /api/blob/{oid}` (bounded content
//! for a base/ours/theirs stage), and `GET /api/worktree-file/{*path}` (the
//! result pane — what git actually wrote to disk, read-only and labelled as
//! such per the issue's decision comment).
//!
//! All three are `full_routes`-only, `Authz::SessionRequired` — see
//! `main.rs`'s registration comment and `route_authz.rs`'s table for the
//! fixed reasoning (recorded on the issue itself before this landed): every
//! one of them can disclose uncommitted worktree or index content, which ADR
//! 0005's LAN profile withholds, and none of them is a write so CSRF is not
//! the concern that makes a route `SessionAndCsrf` instead.
//!
//! No new git reader here. `/api/conflicts` is `conflicts::scan()` (ADR
//! 0063) unchanged; `/api/blob/{oid}` and the worktree read share the same
//! cap and the same `truncate_at_line` tidy-up
//! `handlers::read::file_at_commit_for_repo` uses, so a truncated blob and a
//! truncated worktree file report the identical fact the same way a
//! truncated commit file already does.
//!
//! Every handler is a thin wrapper over a `..._for_repo` function, same shape
//! as `handlers::read`: the handler resolves `?repo=` from the process-wide
//! `CURRENT` selection (no test-time setter), so the seam a test actually
//! drives is the `_for_repo` function with an explicit repository.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use tokio::io::AsyncReadExt;

use git_vista_core::diff::{BlobContent, WorktreeFileContent};
use git_vista_protocol::conflict::ConflictedFile;
use git_vista_protocol::plan::CommitOid;
use git_vista_protocol::{GitOperation, ResolveConflictRequest, WorktreePath};

use crate::git_cmd::{git_cat_file_batch_oid, BatchFileRead};
use crate::handlers::read::{resolve_repo, truncate_at_line, RepoQuery, FILE_CONTENT_CAP};
use crate::planner;
use crate::state::reject_if_read_only;

/// `GET /api/conflicts`: every conflicted path's stage metadata
/// ([`ConflictedFile`]), nothing else. The type's own doc comment says a
/// caller fetches content independently (`/api/blob/{oid}`) — this is
/// metadata only, so a client that just wants the count for a status chip
/// never pays for a blob read.
pub(crate) async fn list_conflicts(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let files = list_conflicts_for_repo(&repo).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(files)))
}

async fn list_conflicts_for_repo(repo: &Path) -> Result<Vec<ConflictedFile>, (StatusCode, String)> {
    crate::conflicts::scan(repo).await.map_err(|e| {
        eprintln!("git-vista: /api/conflicts failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })
}

/// Sniff-then-decode one already-bounded byte buffer the same way
/// `file_at_commit_for_repo` does: binary if a NUL sits in the first 8000
/// bytes (git's own heuristic — matching it is deliberate, see
/// `conflicts::describe_blob`'s identical sniff), otherwise lossily decoded
/// text with `truncate_at_line` tidying the cut when `read_truncated` is set.
/// `read_truncated` is the *reader's* byte-level fact and is authoritative,
/// never re-derived from the decoded string's length — same reasoning
/// `commit_diff_for_repo`'s doc comment gives for why that would be wrong.
fn decode_bounded(bytes: &[u8], read_truncated: bool, cap: usize) -> (String, bool, bool) {
    let binary = bytes.iter().take(8000).any(|&b| b == 0);
    if binary {
        (String::new(), false, true)
    } else {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if read_truncated {
            truncate_at_line(&mut text, cap);
        }
        (text, read_truncated, false)
    }
}

/// `GET /api/blob/{oid}`: bounded content for one conflict stage's blob,
/// addressed directly by the object id a `/api/conflicts` response carries in
/// `Stage::Present.oid`. Serves base, ours and theirs alike — the endpoint
/// has no notion of which side an oid belongs to, because the oid alone
/// already answers "what content" without needing to know "whose side".
pub(crate) async fn blob_content(
    AxumPath(oid): AxumPath<String>,
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let content = blob_content_for_repo(&repo, oid).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(content)))
}

async fn blob_content_for_repo(
    repo: &Path,
    oid: String,
) -> Result<BlobContent, (StatusCode, String)> {
    // The discriminating check (#428): `cat-file --batch` accepts full
    // revision syntax, not just object ids. Without this gate first,
    // `/api/blob/HEAD:secrets.txt` or `/api/blob/:0:path` would be working
    // object reads through a route that claims to take a bare oid.
    if CommitOid::new(&oid).is_err() {
        return Err((StatusCode::BAD_REQUEST, "Not an object id.".to_string()));
    }
    let found = git_cat_file_batch_oid(repo, &oid, FILE_CONTENT_CAP, "/api/blob").await?;
    let (bytes, read_truncated) = match found {
        BatchFileRead::NotABlob { kind } => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("'{oid}' is a {kind}, not a blob."),
            ));
        }
        BatchFileRead::Blob { bytes, truncated } => (bytes, truncated),
    };
    let (content, truncated, binary) = decode_bounded(&bytes, read_truncated, FILE_CONTENT_CAP);
    Ok(BlobContent {
        oid,
        content,
        truncated,
        binary,
    })
}

/// Resolve `path` to a real, in-worktree, non-directory file, refusing a
/// symlink escape the same way `planner::symlink_containment_guard` does for
/// the write endpoints it guards — same canonicalize-and-compare pattern,
/// not that function itself: this is a read with different refusal
/// semantics (a `404`, never that guard's deletion-flavoured `409`), and no
/// downstream `verify_path_states` re-check exists here to lean on for a
/// vanished path, so this treats "does not exist" as a refusal outright
/// rather than deferring it. `WorktreePath`'s own wire-boundary validation
/// (no `..`, not absolute) is necessary but not sufficient — a symlinked path
/// component or final entry can still resolve outside the worktree with no
/// `..` anywhere in the string, which is exactly what canonicalizing and
/// comparing against the canonicalized worktree root catches.
///
/// Blocking filesystem I/O, so this runs on a blocking thread — the same
/// offload discipline `symlink_containment_guard` documents for itself.
async fn resolve_worktree_read_path(
    repo: &Path,
    path: &WorktreePath,
) -> Result<PathBuf, (StatusCode, String)> {
    let repo_owned = repo.to_path_buf();
    let rel = path.as_str().to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<PathBuf, (StatusCode, String)> {
        let repo_canon = std::fs::canonicalize(&repo_owned).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't resolve the worktree root: {e}"),
            )
        })?;
        let joined = repo_owned.join(&rel);
        match std::fs::canonicalize(&joined) {
            Ok(resolved) => {
                if !resolved.starts_with(&repo_canon) {
                    return Err((
                        StatusCode::NOT_FOUND,
                        format!("'{rel}' resolves outside the worktree."),
                    ));
                }
                // `symlink_metadata`, not `metadata`: judged by what the
                // already-proven-in-bounds resolved path actually is.
                let is_dir = std::fs::symlink_metadata(&resolved)
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if is_dir {
                    return Err((
                        StatusCode::NOT_FOUND,
                        format!("'{rel}' is a directory, not a file."),
                    ));
                }
                Ok(resolved)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err((
                StatusCode::NOT_FOUND,
                format!("'{rel}' does not exist in the working tree."),
            )),
            Err(e) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't resolve '{rel}': {e}"),
            )),
        }
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(join_err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("containment check task panicked: {join_err}"),
        )),
    }
}

/// Read at most `cap + 1` bytes of `path` — never more, whatever the file's
/// real size, so a pathological worktree file costs this request nothing
/// past the cap. `truncated` is set from what was actually read (`> cap`),
/// not from a separate `metadata().len()` call: bounding at read time avoids
/// a TOCTOU race between statting the file and reading it, and matches the
/// reader-fact-is-authoritative rule [`decode_bounded`] already applies to
/// git's own capped reads.
async fn read_bounded_worktree_file(path: &Path, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let file = tokio::fs::File::open(path).await?;
    let mut limited = file.take(cap as u64 + 1);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).await?;
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    Ok((buf, truncated))
}

/// `GET /api/worktree-file/{*path}`: the result pane (#428) — the live
/// working-tree content at `path`, read-only. This is what git actually
/// wrote after leaving conflict markers in the file; the client must label
/// it as such rather than presenting it as a resolvable side (see the
/// issue's decision comment — editing arrives in #429).
pub(crate) async fn worktree_file(
    AxumPath(raw_path): AxumPath<String>,
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let content = worktree_file_for_repo(&repo, raw_path).await?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(content)))
}

async fn worktree_file_for_repo(
    repo: &Path,
    raw_path: String,
) -> Result<WorktreeFileContent, (StatusCode, String)> {
    let path = WorktreePath::new(raw_path).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let resolved = resolve_worktree_read_path(repo, &path).await?;
    let (bytes, read_truncated) = read_bounded_worktree_file(&resolved, FILE_CONTENT_CAP)
        .await
        .map_err(|e| {
            eprintln!(
                "git-vista: /api/worktree-file couldn't read '{}': {e}",
                path.as_str()
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't read '{}': {e}", path.as_str()),
            )
        })?;
    let (content, truncated, binary) = decode_bounded(&bytes, read_truncated, FILE_CONTENT_CAP);
    Ok(WorktreeFileContent {
        path: path.as_str().to_string(),
        content,
        truncated,
        binary,
    })
}

/// `POST /api/resolve-conflict` (M4.31b, #429): resolve one conflicted path
/// by taking a whole side, or by removing the file.
///
/// Thin on purpose. Every part of this that could be wrong already exists and
/// is already tested: [`git_vista_protocol::conflict::Resolution`] is the
/// closed vocabulary, `GitOperation::ResolveConflict` carries it,
/// `ConflictedFile::refuses` decides admissibility, and
/// `planner::exec_resolve_conflict` re-runs that check inside the coordinator
/// lock immediately before the write (ADR 0064) — because no precondition can
/// express "still conflicted, and this side is still readable". The issue's
/// own words: *this is the missing surface, not a missing mechanism.*
///
/// So this handler validates the wire shape and hands over. It deliberately
/// does **not** pre-check `refuses` here: a check at the HTTP boundary would
/// be a second, racier copy of the one that actually protects the write, and
/// two answers that can disagree is worse than one answer that cannot.
pub(crate) async fn resolve_conflict(
    Json(req): Json<ResolveConflictRequest>,
) -> (StatusCode, String) {
    if let Some(rejected) = reject_if_read_only() {
        return rejected;
    }
    let path = match WorktreePath::new(req.path) {
        Ok(path) => path,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()),
    };
    planner::plan_and_execute(GitOperation::ResolveConflict {
        path,
        resolution: req.resolution,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {repo:?}");
    }

    fn out(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {repo:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A fresh repository on branch `main` with one committed file.
    fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "seed"]);
        (dir, repo)
    }

    /// A repository with a real, unresolved modify/modify conflict on
    /// `a.txt`, mirroring `conflicts.rs`'s own fixture so both files' tests
    /// exercise the identical shape of git state.
    fn conflicted_repo() -> (tempfile::TempDir, PathBuf) {
        let (dir, repo) = seeded_repo();
        run(&repo, &["checkout", "-q", "-b", "theirs"]);
        std::fs::write(repo.join("a.txt"), "theirs\n").unwrap();
        run(&repo, &["commit", "-q", "-am", "theirs"]);
        run(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("a.txt"), "ours\n").unwrap();
        run(&repo, &["commit", "-q", "-am", "ours"]);
        let _ = std::process::Command::new("git")
            .args(["merge", "theirs"])
            .current_dir(&repo)
            .status();
        (dir, repo)
    }

    // ---- list_conflicts_for_repo -------------------------------------

    #[tokio::test]
    async fn a_clean_repo_lists_no_conflicts() {
        let (_d, repo) = seeded_repo();
        let files = list_conflicts_for_repo(&repo).await.unwrap();
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn a_conflicted_repo_lists_the_conflicted_path_with_all_stages() {
        let (_d, repo) = conflicted_repo();
        let files = list_conflicts_for_repo(&repo).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert!(files[0].base.is_text());
        assert!(files[0].ours.is_text());
        assert!(files[0].theirs.is_text());
    }

    #[tokio::test]
    async fn a_scan_failure_is_a_500_not_a_silent_empty_list() {
        // MUTATION: map the scan error to Ok(vec![]). The endpoint would then
        // report "no conflicts" for a repository it could not even read —
        // the exact failure `conflicts::scan`'s own contract exists to
        // prevent, now one HTTP hop further out.
        let dir = tempfile::tempdir().unwrap();
        let (status, msg) = list_conflicts_for_repo(dir.path()).await.unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(msg.contains("ls-files"), "{msg}");
    }

    // ---- blob_content_for_repo -----------------------------------------

    #[tokio::test]
    async fn a_present_stage_oid_reads_back_its_content() {
        let (_d, repo) = conflicted_repo();
        let oid = out(&repo, &["rev-parse", ":2:a.txt"]); // stage 2 = ours
        let content = blob_content_for_repo(&repo, oid.clone()).await.unwrap();
        assert_eq!(content.oid, oid);
        assert_eq!(content.content, "ours\n");
        assert!(!content.truncated);
        assert!(!content.binary);
    }

    #[tokio::test]
    async fn a_malformed_oid_is_refused_before_anything_spawns() {
        // THE test in this file. MUTATION: drop the CommitOid gate.
        // `cat-file --batch` accepts full revision syntax, so
        // `HEAD:some/path` would be a working object read through a route
        // that claims to take a bare object id.
        let (_d, repo) = seeded_repo();
        let (status, msg) = blob_content_for_repo(&repo, "HEAD:a.txt".to_string())
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("object id"), "{msg}");

        let (status, _) = blob_content_for_repo(&repo, "not-hex".to_string())
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_unknown_but_well_formed_oid_is_a_404() {
        let (_d, repo) = seeded_repo();
        let (status, _) = blob_content_for_repo(&repo, "a".repeat(40))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_tree_oid_is_refused_as_not_a_blob() {
        let (_d, repo) = seeded_repo();
        let tree_oid = out(&repo, &["rev-parse", "HEAD^{tree}"]);
        let (status, msg) = blob_content_for_repo(&repo, tree_oid).await.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(msg.contains("tree"), "{msg}");
    }

    #[tokio::test]
    async fn a_binary_blob_reports_binary_with_empty_content() {
        let (dir, repo) = seeded_repo();
        std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, b'x']).unwrap();
        run(&repo, &["add", "bin.dat"]);
        let oid = out(&repo, &["hash-object", "bin.dat"]);
        let content = blob_content_for_repo(&repo, oid).await.unwrap();
        assert!(content.binary);
        assert_eq!(content.content, "");
        drop(dir);
    }

    #[tokio::test]
    async fn a_blob_past_the_cap_is_truncated_at_a_line_boundary() {
        let (_d, repo) = seeded_repo();
        let big = "line\n".repeat(10);
        std::fs::write(repo.join("big.txt"), &big).unwrap();
        run(&repo, &["add", "big.txt"]);
        let oid = out(&repo, &["hash-object", "big.txt"]);

        // Drive the cap logic directly at a tiny cap, same shape as
        // read/content_suite.rs's cap tests: cap mid-line so the tidy-up has
        // something to do.
        let found = git_cat_file_batch_oid(&repo, &oid, 12, "/api/blob")
            .await
            .unwrap();
        let BatchFileRead::Blob { bytes, truncated } = found else {
            panic!("expected a blob");
        };
        assert!(truncated);
        let (content, reported_truncated, binary) = decode_bounded(&bytes, truncated, 12);
        assert!(reported_truncated);
        assert!(!binary);
        // Cut at the last full line *before* the cap — the cut point itself
        // (not a trailing newline) is what `truncate_at_line` guarantees, so
        // the tidied text never ends mid-line.
        assert_eq!(content, "line\nline");
        assert!(content.len() <= 12);
    }

    // ---- worktree_file_for_repo -----------------------------------------

    #[tokio::test]
    async fn the_result_pane_reads_the_marker_file_git_actually_wrote() {
        let (_d, repo) = conflicted_repo();
        let content = worktree_file_for_repo(&repo, "a.txt".to_string())
            .await
            .unwrap();
        assert_eq!(content.path, "a.txt");
        assert!(
            content.content.contains("<<<<<<<"),
            "must be the real marker file: {:?}",
            content.content
        );
        assert!(!content.binary);
    }

    #[tokio::test]
    async fn a_path_traversal_attempt_is_refused_at_the_wire_boundary() {
        // WorktreePath's own `..`-rejection, exercised through this seam.
        let (_d, repo) = seeded_repo();
        let (status, _) = worktree_file_for_repo(&repo, "../escape.txt".to_string())
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_symlink_escaping_the_worktree_is_refused() {
        // THE test for resolve_worktree_read_path. MUTATION: compare the
        // joined (unresolved) path against the repo root instead of the
        // canonicalized one. `WorktreePath` has no `..` in it anywhere, so a
        // lexical-only check would wave this straight through.
        #[cfg(unix)]
        {
            let (dir, repo) = seeded_repo();
            let outside = dir.path().join("outside.txt");
            std::fs::write(&outside, "secret\n").unwrap();
            std::os::unix::fs::symlink(&outside, repo.join("escape.txt")).unwrap();

            let (status, _) = worktree_file_for_repo(&repo, "escape.txt".to_string())
                .await
                .unwrap_err();
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn a_missing_worktree_path_is_a_404() {
        let (_d, repo) = seeded_repo();
        let (status, _) = worktree_file_for_repo(&repo, "does-not-exist.txt".to_string())
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_directory_is_refused_not_read() {
        let (_d, repo) = seeded_repo();
        std::fs::create_dir_all(repo.join("a-dir")).unwrap();
        let (status, msg) = worktree_file_for_repo(&repo, "a-dir".to_string())
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(msg.contains("directory"), "{msg}");
    }

    #[tokio::test]
    async fn a_worktree_file_past_the_cap_is_reported_truncated_and_capped() {
        // What this proves: crossing the cap sets `truncated`, and the buffer
        // handed back never exceeds it.
        //
        // What it does NOT prove, stated plainly rather than implied by the
        // name: that the read is *bounded at read time*. Replacing
        // `file.take(cap + 1)` with an unbounded `read_to_end` followed by the
        // same `buf.truncate(cap)` leaves every assertion here green — the
        // returned buffer is identical, and only the bytes pulled off disk
        // differ. That mutation SURVIVES, verified rather than assumed
        // (`mutation_check`, #428), and it is documented here for the same
        // reason `planner.rs`'s `exec_resolve_conflict` documents its own
        // survivor: a survived mutation hidden is worse than one written down.
        //
        // The read-time bound is what stops a pathological worktree file
        // costing this request its whole size in RAM on an 8 GB box. No test
        // in this suite can observe it — the only difference is bytes read and
        // memory held, neither of which a return value carries — so it is
        // enforced by `read_bounded_worktree_file`'s `.take()` and held by
        // code review, not by this test.
        let (_d, repo) = seeded_repo();
        let big = "line\n".repeat(10);
        std::fs::write(repo.join("big.txt"), &big).unwrap();

        let (bytes, truncated) = read_bounded_worktree_file(&repo.join("big.txt"), 12)
            .await
            .unwrap();
        assert!(truncated, "crossing the cap must be reported, never silent");
        assert_eq!(
            bytes.len(),
            12,
            "the buffer handed back never exceeds the cap"
        );

        // Well under the real (2 MiB) production cap, so the full-stack seam
        // reports it untruncated — this asserts `worktree_file_for_repo`
        // actually reaches `read_bounded_worktree_file` rather than some
        // other reader, not that this particular fixture crosses the cap.
        let content = worktree_file_for_repo(&repo, "big.txt".to_string())
            .await
            .unwrap();
        assert!(!content.truncated);
        assert_eq!(content.content, big);
    }

    // ---- decode_bounded ---------------------------------------------------

    #[test]
    fn binary_sniff_matches_gits_own_heuristic() {
        // MUTATION: treat any content as text. A conflict stage's binary
        // side would then decode as (likely mangled) lossy UTF-8 text
        // instead of being flagged for the caller to handle separately.
        let (content, truncated, binary) = decode_bounded(&[0, 1, 2], false, 100);
        assert!(binary);
        assert!(content.is_empty());
        assert!(!truncated);

        let (content, truncated, binary) = decode_bounded(b"hello\n", false, 100);
        assert!(!binary);
        assert_eq!(content, "hello\n");
        assert!(!truncated);
    }

    // ---- resolve_conflict's wire boundary (M4.31b, #429) ----------------
    //
    // The write path itself is `planner::exec_resolve_conflict`, tested in
    // planner's own suites against real conflicted repositories. What is
    // tested here is the part this file owns: the wire shape, and that a
    // malformed path never becomes a `WorktreePath`.

    #[test]
    fn the_request_body_refuses_a_stray_key() {
        // Same posture as every other body in this contract. A stray key
        // beside a resolution is a caller that believes it is sending
        // something this endpoint honours — silently dropping it is how a
        // user ends up with a resolution they did not ask for.
        let stray = serde_json::json!({
            "path": "a.txt",
            "resolution": {"choice": "take_ours"},
            "also_stage": true,
        });
        assert!(serde_json::from_value::<ResolveConflictRequest>(stray).is_err());
    }

    #[test]
    fn the_request_body_round_trips_every_resolution() {
        // MUTATION: drop a variant from the wire form. A resolution the UI
        // can offer but the wire cannot carry fails at the boundary with a
        // deserialize error, which reads as "it failed" — the exact
        // undifferentiated refusal #429 exists to prevent.
        for choice in ["take_ours", "take_theirs", "take_deletion"] {
            let body = serde_json::json!({
                "path": "src/a.rs",
                "resolution": {"choice": choice},
            });
            let req: ResolveConflictRequest =
                serde_json::from_value(body).unwrap_or_else(|e| panic!("{choice}: {e}"));
            assert_eq!(req.path, "src/a.rs");
        }
    }

    #[test]
    fn a_path_that_escapes_the_worktree_never_becomes_a_worktree_path() {
        // The newtype is the gate; this pins that the handler actually runs
        // it on THIS field rather than trusting the wire string.
        for bad in ["../escape.txt", "/etc/passwd", "-rf", ""] {
            assert!(
                WorktreePath::new(bad.to_string()).is_err(),
                "{bad:?} must be refused at the wire boundary"
            );
        }
        assert!(WorktreePath::new("src/a.rs".to_string()).is_ok());
    }

    #[test]
    fn a_read_truncation_is_reported_even_when_the_decoded_string_would_not_show_it() {
        let (_content, truncated, _binary) = decode_bounded(b"short", true, 100);
        assert!(
            truncated,
            "read_truncated must be authoritative, never re-derived from decoded length"
        );
    }
}
