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
//! cap and the same `truncate_at_line` tidy-up `handlers::read::file_at_commit_for_repo`
//! uses, so a truncated blob and a truncated worktree file report the
//! identical fact the same way a truncated commit file already does.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use tokio::io::AsyncReadExt;

use git_vista_core::diff::{BlobContent, WorktreeFileContent};
use git_vista_protocol::plan::CommitOid;
use git_vista_protocol::WorktreePath;

use crate::git_cmd::{git_cat_file_batch_oid, BatchFileRead};
use crate::handlers::read::{resolve_repo, truncate_at_line, RepoQuery, FILE_CONTENT_CAP};

/// `GET /api/conflicts`: every conflicted path's stage metadata
/// ([`git_vista_protocol::conflict::ConflictedFile`]), nothing else. The
/// type's own doc comment says a caller fetches content independently
/// (`/api/blob/{oid}`) — this is metadata only, so a client that just wants
/// the count for a status chip never pays for a blob read.
pub(crate) async fn list_conflicts(
    Query(q): Query<RepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let files = crate::conflicts::scan(&repo).await.map_err(|e| {
        eprintln!("git-vista: /api/conflicts failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(files)))
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
    // The discriminating check (#428): `cat-file --batch` accepts full
    // revision syntax, not just object ids. Without this gate first,
    // `/api/blob/HEAD:secrets.txt` or `/api/blob/:0:path` would be working
    // object reads through a route that claims to take a bare oid.
    if CommitOid::new(&oid).is_err() {
        return Err((StatusCode::BAD_REQUEST, "Not an object id.".to_string()));
    }
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let found = git_cat_file_batch_oid(&repo, &oid, FILE_CONTENT_CAP, "/api/blob").await?;
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
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((
        no_store,
        Json(BlobContent {
            oid,
            content,
            truncated,
            binary,
        }),
    ))
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
    let path =
        WorktreePath::new(raw_path).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let repo = resolve_repo(q.repo.as_deref())?.0;
    let resolved = resolve_worktree_read_path(&repo, &path).await?;
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
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((
        no_store,
        Json(WorktreeFileContent {
            path: path.as_str().to_string(),
            content,
            truncated,
            binary,
        }),
    ))
}

#[cfg(test)]
mod tests;
