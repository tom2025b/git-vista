//! The activity/undo endpoints: `GET /api/activity` (the assembled feed),
//! `GET /api/undoables/{id}` (undo actions for one tapped commit, computed
//! live), and `POST /api/undo` (execute one [`UndoAction`]).
//!
//! The feed handler is deliberately thin: it *collects* (current branches, the
//! journal, every reflog, the remote-commit set), lets the pure, unit-tested
//! [`assemble_feed`] do the folding, and maintains the ref snapshot on the
//! way through. The interesting logic lives in `git_vista_core::activity`
//! and `crate::journal`.
//!
//! Snapshot upkeep happens in `activity_feed` — and only there — because
//! detection and bookkeeping must be one atomic step: whoever rewrites the
//! snapshot must first synthesize deletion events for branches that vanished
//! since the last one, or those deletions are silently forgotten. Keeping a
//! single writer makes that invariant easy to hold (which is why `undoables`
//! reads the same sources but leaves the snapshot alone).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Extension, Path as AxumPath, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use git_vista_core::activity::{
    assemble_feed, ActivityEvent, ActivityKind, ActivitySource, ReflogEntry, RefsAtEvent,
    UndoAction, Undoable,
};
use git_vista_core::model::RefKind;
use git_vista_git::{read_commit, read_reflogs, read_refs, read_remote_commits, RepoError};
use git_vista_protocol::{ActivityPage, BranchName, CommitOid, GenerationToken, GitOperation};

use crate::git_cmd;
use crate::history::{CursorCodec, CursorScope};
use crate::journal;

/// How many events one page returns by default, and at most. `MAX_PAGE_LIMIT`
/// bounds one response, never the folded feed: older events remain reachable
/// through the response cursor.
const DEFAULT_LIMIT: usize = 100;
const MAX_PAGE_LIMIT: usize = 500;

/// Reflog entries read per ref. A rebase writes one line per replayed commit,
/// so this is deliberately far above the feed limit.
const REFLOG_PER_REF: usize = 200;

#[derive(Deserialize)]
pub struct ActivityParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// The position inside one exact folded-feed snapshot. It is never exposed
/// directly: [`CursorCodec`] signs it together with repository scope and the
/// feed generation before it reaches the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ActivityCursor {
    next_event: usize,
}

/// The server's concrete activity-page response.
type Page = ActivityPage<ActivityEvent>;

/// Unix seconds now; the timestamp journaled onto synthesized events.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The feed: journal + reflogs + snapshot diff, folded newest-first.
pub async fn activity_feed(
    Extension(codec): Extension<Arc<CursorCodec>>,
    Query(params): Query<ActivityParams>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (repo, read_only) = crate::state::current();
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
    let handle = crate::state::current_handle();
    let scope = codec.scope_for_target(handle.as_ref(), &canonical);
    let page = activity_page_for_target(&repo, read_only, scope, &params, codec.as_ref())?;
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(page)))
}

/// Collect and fold every event available in the bounded journal/reflog source
/// windows. Paging is applied only after this returns: passing `usize::MAX`
/// here is the mechanism that makes the old 500-event total ceiling disappear
/// from both the query path and the cursor path.
fn collect_activity_feed(
    repo: &Path,
    read_only: bool,
) -> Result<Vec<ActivityEvent>, (StatusCode, String)> {
    // The current local branch → tip map: the baseline for undo hints and for
    // the next snapshot.
    let refs = read_refs(repo).map_err(|e| {
        eprintln!("git-vista: /api/activity failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let branches: HashMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();

    // Snapshot diff: a branch known to the last snapshot but absent now was
    // deleted outside the app (app deletions remove their branch from the
    // snapshot the moment they happen — see the delete handlers). Journal the
    // synthesized event *before* rewriting the snapshot, so it's remembered
    // exactly once, with the last tip we saw — which is what makes even a
    // terminal deletion restorable.
    if let Some(snapshot) = journal::read_snapshot(repo) {
        for (name, tip) in &snapshot {
            if !branches.contains_key(name) {
                journal::append(
                    repo,
                    &ActivityEvent {
                        time: now_secs(),
                        kind: ActivityKind::BranchDeleted,
                        ref_name: Some(name.clone()),
                        summary: format!("deleted branch ‘{name}’ (outside git-vista)"),
                        old_oid: Some(tip.clone()),
                        new_oid: None,
                        source: ActivitySource::External,
                        undo: None,
                        // This event is synthesized on NOTICING a deletion
                        // that already happened, so the live repo no longer
                        // holds the branch. Attach the map that still does —
                        // the snapshot we are diffing against — rather than
                        // letting append() capture a present that has already
                        // lost the very branch this event is about (#131).
                        //
                        // The snapshot records branches and nothing else, so
                        // HEAD, tags and remotes stay `None`
                        // — *not recorded* (#449). Filling them from the live
                        // repo would date them to the moment of noticing, not
                        // the moment of the deletion, and pass that off as one
                        // observation; a replayer would then draw a HEAD that
                        // was never where this event claims.
                        refs: Some(RefsAtEvent::Captured {
                            branches: snapshot
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                            truncated_at: None,
                            head: None,
                            tags: None,
                            remotes: None,
                            // One event, its own capture: this anchors no
                            // batch (#485).
                            batch: None,
                        }),
                    },
                );
                println!(
                    "[/api/activity] noticed external deletion of branch '{name}' (was {tip})"
                );
            }
        }
    }
    journal::write_snapshot(repo, &branches);

    let journal_events = journal::read_all(repo);
    let reflog = read_reflogs(repo, REFLOG_PER_REF).map_err(|e| {
        eprintln!("git-vista: /api/activity failed reading reflogs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // Which commits are on the remote — feeds the "already pushed" warning on
    // reset-style undo hints. Best-effort: no remote (or a failed walk) just
    // means no warnings.
    let remote: HashSet<String> =
        read_remote_commits(repo, crate::state::HISTORY_LIMIT).unwrap_or_default();

    let mut feed = assemble_feed(journal_events, reflog, &branches, &remote, usize::MAX);
    // A read-only clone can't undo anything (`/api/undo` would 403), so its
    // feed carries no hints — the UI then simply never shows an Undo control.
    if read_only {
        for event in &mut feed {
            event.undo = None;
        }
    }
    let app_count = feed
        .iter()
        .filter(|e| e.source == ActivitySource::App)
        .count();
    println!(
        "[/api/activity] {} — {} event(s) ({app_count} via app), {} undoable",
        repo.display(),
        feed.len(),
        feed.iter().filter(|e| e.undo.is_some()).count(),
    );

    Ok(feed)
}

/// Digest the exact fully folded response source. A cursor's row number is
/// usable only while this token still matches; any head insertion, deletion,
/// re-fold, changed undo hint, or changed ref capture refuses the cursor with
/// 409 instead of applying an offset to a different sequence.
fn activity_generation(feed: &[ActivityEvent]) -> Result<GenerationToken, (StatusCode, String)> {
    let bytes = serde_json::to_vec(feed).map_err(|e| {
        eprintln!("git-vista: /api/activity could not serialize its snapshot: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not serialize the activity feed".to_string(),
        )
    })?;
    GenerationToken::new(format!("activity-v1:{:x}", Sha256::digest(bytes))).map_err(|e| {
        eprintln!("git-vista: /api/activity could not build its generation: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not identify the activity feed".to_string(),
        )
    })
}

/// Build one page against an exact folded snapshot.
fn activity_page_for_target(
    repo: &Path,
    read_only: bool,
    scope: CursorScope,
    params: &ActivityParams,
    codec: &CursorCodec,
) -> Result<Page, (StatusCode, String)> {
    // Authenticate and scope-check before touching repository sources. The
    // generation comparison necessarily waits for the fresh fold below, but a
    // forged or foreign cursor should not buy an attacker a journal/reflog
    // scan first.
    let decoded = match params.cursor.as_deref() {
        None => None,
        Some(encoded) => {
            let decoded = codec
                .decode::<ActivityCursor>(encoded)
                .map_err(|_| invalid_activity_cursor())?;
            if decoded.scope != scope {
                return Err(invalid_activity_cursor());
            }
            Some(decoded)
        }
    };
    let feed = collect_activity_feed(repo, read_only)?;
    let generation = activity_generation(&feed)?;
    let start = match decoded {
        None => 0,
        Some(decoded) => {
            if decoded.generation != generation {
                return Err((
                    StatusCode::CONFLICT,
                    "activity moved; restart from the first page".to_string(),
                ));
            }
            if decoded.state.next_event > feed.len() {
                return Err(invalid_activity_cursor());
            }
            decoded.state.next_event
        }
    };
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);
    let end = start.saturating_add(limit).min(feed.len());
    let cursor = if end < feed.len() {
        Some(
            codec
                .encode(scope, &generation, &ActivityCursor { next_event: end })
                .map_err(|_| {
                    eprintln!("git-vista: /api/activity could not sign a cursor");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not sign an activity cursor".to_string(),
                    )
                })?,
        )
    } else {
        None
    };

    Ok(Page {
        events: feed[start..end].to_vec(),
        cursor,
    })
}

fn invalid_activity_cursor() -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        "invalid activity cursor".to_string(),
    )
}

/// The conventional 7-char short id, for labels and log lines.
fn short(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Belt-and-braces before an id goes anywhere near argv: real ids are hex.
/// Same check `/api/diff/{id}` does.
fn is_hex_id(id: &str) -> bool {
    id.len() >= 4 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A branch name safe to hand to git as its own argv entry: non-empty and not
/// option-shaped. git itself validates the rest (bad characters, collisions).
fn is_safe_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-')
}

/// `GET /api/undoables/{id}` — every undo action that applies to one commit,
/// computed live (step 5). The graph menu fetches this when it opens, so the
/// undo section always reflects the repo *now*, not the possibly-stale graph.
///
/// Two sources:
///  * the assembled feed's own hints (same fold as `/api/activity`, minus the
///    snapshot upkeep — that single-writer invariant belongs to the feed
///    handler): a reset-style undo whose result is this commit, or a deleted
///    branch whose lost tip was this commit;
///  * a revert offer for any non-merge commit **whose revert is established,
///    live, to actually apply** — the history-preserving undo, valid even for
///    commits buried deep or already pushed. (Reverting a merge needs a `-m`
///    parent choice, so merges don't get the offer.)
///
/// # #327: availability is established, not asserted
///
/// This used to offer a revert for every non-merge commit unconditionally —
/// "available" asserted, never checked. Reverting a commit that later work
/// depends on conflicts, and git refuses; the owner's own session log is
/// exactly this bug: `undoables` said one action existed, the tap failed with
/// `error: could not revert f993ba6… \n hint: after resolving the conflicts,
/// mark the corrected paths`. To the user that reads as a button that lights
/// up and then greys out.
///
/// The fix is a **lazy, real precheck**: [`revert_would_conflict`] runs the
/// exact three-way merge `git revert` itself would perform —
/// `git merge-tree --write-tree --merge-base=<commit> HEAD <parent>` — read
/// straight from the object database (no worktree, no index, no ref ever
/// moves) and classified from git's own exit code (`0`/`1`), not a text
/// heuristic. It is lazy in the sense that matters: this handler is already
/// called once per menu-open, for the one commit tapped — never for the
/// whole graph (see the module doc's cost note) — so one extra sandboxed git
/// call per click is the entire cost, not one per node on screen. An eager
/// version that prechecked every candidate commit on every graph render was
/// considered and rejected for exactly that reason.
///
/// A precheck that can't be run at all (HEAD won't resolve, the commit is a
/// root with no parent to diff against, the sandboxed spawn itself fails) is
/// **not** offered — "couldn't tell" must not read as "yes", same posture
/// [`crate::planner`]'s pull executor documents for `restored`.
///
/// A read-only clone gets an empty list: `/api/undo` would refuse anyway, so
/// the UI never shows a section it can't act on.
pub async fn undoables(
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    let (repo, read_only) = crate::state::current();
    if !is_hex_id(&id) {
        return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
    }
    if read_only {
        return Ok((no_store, Json(Vec::<Undoable>::new())));
    }
    let detail = read_commit(&repo, &id).map_err(|e| match e {
        RepoError::CommitNotFound(_) => (StatusCode::NOT_FOUND, "No such commit.".to_string()),
        other => {
            eprintln!("git-vista: /api/undoables/{id} failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
        }
    })?;
    let full = detail.id.0.clone();

    let refs = read_refs(&repo).map_err(|e| {
        eprintln!("git-vista: /api/undoables failed reading refs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let branches: HashMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();
    let journal_events = journal::read_all(&repo);
    let reflog = read_reflogs(&repo, REFLOG_PER_REF).map_err(|e| {
        eprintln!("git-vista: /api/undoables failed reading reflogs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let remote: HashSet<String> =
        read_remote_commits(&repo, crate::state::HISTORY_LIMIT).unwrap_or_default();

    let mut out = matching_feed_undoables(journal_events, reflog, &branches, &remote, &full);
    // #327 defect A: offer the revert only once it is established live that
    // it will actually apply — see this function's doc comment for why the
    // check runs here, lazily, for this one commit.
    //
    // A merge commit (`parents.len() > 1`) never reaches the precheck at
    // all, same as before this fix — reverting one needs a `-m` parent
    // choice this UI doesn't collect.
    if detail.parents.len() == 1 {
        let parent = detail.parents[0].0.as_str();
        if revert_offer_established(&repo, &full, parent).await {
            out.push(Undoable {
                action: UndoAction::RevertCommit {
                    commit: full.clone(),
                },
                label: format!("Revert {} (adds an inverse commit)", short(&full)),
                warn_pushed: false,
            });
        }
    }
    // A root commit (`parents.len() == 0`) has nothing to diff the revert
    // against — `merge-tree` needs a parent commit as `theirs`, and a
    // synthetic empty-tree stand-in isn't a commit `--merge-base` will
    // accept (verified: git refuses it, "expected commit type"). Reverting
    // the very first commit is a rare, almost always self-conflicting
    // operation in any repository with real history on top of it, so this
    // falls out of the same fail-closed rule rather than needing its own
    // arm: no established answer, no offer.
    println!(
        "[/api/undoables] {} → {} action(s)",
        short(&full),
        out.len()
    );
    Ok((no_store, Json(out)))
}

/// Find a commit's hints across the entire available folded feed.
///
/// This path deliberately does not take a page limit. An event made reachable
/// past row 500 by `/api/activity` must also keep the undo hint that
/// `/api/undoables/{id}` derives from the same fold.
fn matching_feed_undoables(
    journal_events: Vec<ActivityEvent>,
    reflog: Vec<ReflogEntry>,
    branches: &HashMap<String, String>,
    remote: &HashSet<String>,
    full: &str,
) -> Vec<Undoable> {
    let feed = assemble_feed(journal_events, reflog, branches, remote, usize::MAX);
    let mut out = Vec::new();
    for event in feed {
        let Some(undoable) = event.undo else { continue };
        // The commit each hint is "about": the state a reset would discard,
        // or the tip a restore would bring back — i.e. the dot being tapped.
        let about = match &undoable.action {
            UndoAction::ResetBranch { expected_tip, .. } => expected_tip.as_str(),
            UndoAction::RestoreBranch { tip, .. } => tip.as_str(),
            UndoAction::RevertCommit { commit } => commit.as_str(),
        };
        if about == full && !out.contains(&undoable) {
            out.push(undoable);
        }
    }
    out
}

/// The git this check needs, which is **above the documented product floor**
/// of 2.32 (`docs/SUPPORTED_VERSIONS.md`).
///
/// `git merge-tree --write-tree` arrived in **2.38.0** (October 2022). Below
/// it, `merge-tree` is the older positional-argument command that does not
/// understand the flag at all.
///
/// Deliberately its own constant rather than a shared one with
/// [`crate::preview::MIN_GIT_FOR_PREVIEW`], even though both are 2.38 for the
/// same reason: they are two features' floors that happen to coincide, and a
/// single shared number would quietly become a second product floor. What the
/// two share is the *measurement* ([`crate::git_version`]), not the policy.
pub(crate) const MIN_GIT_FOR_MERGE_TREE: (u32, u32) = (2, 38);

/// Why [`revert_would_conflict`] could not answer.
///
/// #581: before this existed, both arms were one `Err(String)` and the caller
/// could only say "the check failed". They are different facts and the
/// distinction is the same one this module already keeps elsewhere
/// (`RecoveryClass::CheckFailed` vs `Expired { WouldConflict }`): *this host
/// cannot answer the question* is a property of the host that will be true
/// again next time, while *the check ran and errored* may not be.
///
/// Neither is ever grounds to offer a revert. The distinction buys the user an
/// explanation, not a different safety posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevertCheckError {
    /// This git predates `merge-tree --write-tree`, so the question cannot be
    /// asked of it at all.
    ///
    /// **Measured 2026-09-02** on git 2.34.1 (Ubuntu 22.04, inside the
    /// documented 2.32 product floor): the exact argv below exits **129** with
    /// `usage: git merge-tree <base-tree> <branch1> <branch2>`. 129 is neither
    /// 0 nor 1, so before #581 it landed in the arm below and the revert offer
    /// silently never appeared, with nothing said about why.
    GitTooOld {
        /// The host's version, `major.minor.patch`.
        found: String,
        /// The floor, `major.minor`.
        minimum: String,
    },
    /// The check ran and did not produce an answer — the sandboxed spawn
    /// failed, or `merge-tree` exited some other way (e.g. a genuinely bad
    /// revision).
    CheckFailed(String),
}

/// Whether `found` is too old to be asked the revert-conflict question, as the
/// error to report.
///
/// Pure and separate from the probe so the *decision* can be tested with
/// literal versions on both sides of the floor, rather than only on whatever
/// git the host running the tests happens to have — which on every machine
/// this project has ever run on is far above it. The same shape, and the same
/// reason, as [`crate::preview`]'s `version_gate` (#576, ADR 0099).
fn merge_tree_version_gate(found: (u32, u32, u32)) -> Option<RevertCheckError> {
    if crate::git_version::meets(found, MIN_GIT_FOR_MERGE_TREE) {
        return None;
    }
    Some(RevertCheckError::GitTooOld {
        found: crate::git_version::render(found),
        minimum: crate::git_version::render_floor(MIN_GIT_FOR_MERGE_TREE),
    })
}

/// Whether reverting `commit` (whose sole parent is `parent`) onto `head`
/// would conflict — established, not guessed (#327 defect A).
///
/// `git merge-tree --write-tree --merge-base=<commit> <head> <parent>`
/// computes exactly the three-way merge `git revert` performs internally:
/// base = the commit being reverted, ours = `head`, theirs = `parent` (the
/// tree as it looked immediately before `commit`). It reads straight from the
/// object database — no worktree is created, no index is touched, no ref
/// moves — so it is safe to run on a live, served repository on every
/// menu-open. (It does write a merged tree/blob objects to the object
/// database on a clean result, same as any other content-addressed git
/// plumbing — no different from the loose objects `git diff`, `git stash`,
/// or a dry-run `git merge-tree` a user runs by hand already leave behind;
/// nothing references them, and normal `git gc --auto` reclaims them.)
///
/// The answer is git's own exit code, not a text heuristic: `--write-tree`
/// documents `0` for a clean merge and `1` for a real conflict, and that
/// contract — unlike git's prose — doesn't shift with locale or version.
/// Verified against this server's git (2.43.0; see the tests).
///
/// `Err` means the check did not produce an answer, and since #581 it says
/// **which** of two reasons: [`RevertCheckError::GitTooOld`] (this host's git
/// predates `--write-tree`, so the question cannot be asked of it) or
/// [`RevertCheckError::CheckFailed`] (the sandboxed spawn failed, or
/// `merge-tree` exited some other way, e.g. a genuinely bad revision).
///
/// The caller's job is unchanged and the safety posture is unchanged: both
/// arms are treated exactly like a conflict — "couldn't tell" must never read
/// as "safe to offer". What the split buys is that a user on an old git can be
/// told why the offer is missing, instead of watching it silently not appear.
///
/// `pub(crate)` since M3.25 (#78): [`crate::recovery_center::classify_recovery`]
/// reuses this directly rather than [`revert_offer_established`] below, which
/// collapses `Err` and "no conflict" into one caller-facing `bool` — the
/// Recovery Center has to keep those apart (`RecoveryClass::CheckFailed` vs.
/// `Expired { WouldConflict }` vs. `Offered`).
pub(crate) async fn revert_would_conflict(
    repo: &Path,
    commit: &str,
    parent: &str,
    head: &str,
) -> Result<bool, RevertCheckError> {
    // #581: ask what git this is BEFORE asking it a question it may not
    // understand. Cached per process by `crate::git_version`, so this costs one
    // `git --version` for the life of the server, not one per menu-open.
    let found = crate::git_version::current(repo)
        .await
        .map_err(RevertCheckError::CheckFailed)?;
    revert_would_conflict_at_version(repo, commit, parent, head, found).await
}

/// [`revert_would_conflict`] with the host's git version supplied rather than
/// probed.
///
/// Split out for one reason, and it is a testing reason worth stating plainly:
/// [`crate::git_version::current`] caches per process, so no test can make the
/// running host's git look old. Without this arity the gate's *wiring* — as
/// opposed to [`merge_tree_version_gate`]'s pure decision — could only be
/// pinned by a source scan, and deleting the gate call would leave every test
/// in this file green. With it, a test drives the real function with a literal
/// 2.34.1 against a real repository and sees the real refusal.
///
/// The version is the only injected value. The repository, the spawn and the
/// exit-code reading are all the production path.
async fn revert_would_conflict_at_version(
    repo: &Path,
    commit: &str,
    parent: &str,
    head: &str,
    found: (u32, u32, u32),
) -> Result<bool, RevertCheckError> {
    if let Some(too_old) = merge_tree_version_gate(found) {
        return Err(too_old);
    }
    let merge_base = format!("--merge-base={commit}");
    let output = git_cmd::git_output(
        repo,
        &["merge-tree", "--write-tree", &merge_base, head, parent],
    )
    .await
    .map_err(|e| RevertCheckError::CheckFailed(format!("couldn't run git merge-tree: {e}")))?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(RevertCheckError::CheckFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
    }
}

/// The wiring `undoables` depends on for its offer decision (#327 defect A):
/// resolve `HEAD`, then run [`revert_would_conflict`] against it — offering
/// only when both steps ran **and** answered "clean".
///
/// Pulled out of `undoables` on its own, rather than left inline, so this
/// decision is a fact a test can pin without a `crate::state::current()`
/// fixture: it takes `repo`/`commit`/`parent` directly and returns a plain
/// `bool`, same shape as [`revert_would_conflict`] beside it.
///
/// This is exactly the kind of single-character inversion defect A is
/// about — `.map(|conflicts| !conflicts)` flipped, or the `unwrap_or`
/// changed from `false` to `true`, silently reintroduces "available
/// asserted, never established". See the tests for the mutation this
/// proves.
///
/// Three ways to land on `false` (don't offer), all fail-closed for the same
/// reason: `HEAD` doesn't resolve (`Ok(None)`), resolving it couldn't even
/// run (`Err`), or it resolved but [`revert_would_conflict`] itself couldn't
/// produce an answer (its own `Err` arm). None of these is "no conflict" —
/// they're "no fact", and a fact we don't have is never grounds to offer.
async fn revert_offer_established(repo: &Path, commit: &str, parent: &str) -> bool {
    match git_cmd::rev_parse(repo, "HEAD").await {
        Ok(Some(head)) => revert_would_conflict(repo, commit, parent, &head)
            .await
            .map(|conflicts| !conflicts)
            .unwrap_or(false),
        Ok(None) | Err(_) => false,
    }
}

/// `POST /api/undo` — execute one [`UndoAction`] (step 5). The body is the
/// tagged action exactly as `/api/activity` / `/api/undoables` handed it out.
///
/// Since M1.06b (#143) the handler validates the action's fields (same
/// wording), builds the matching [`GitOperation`], and hands it to the shared
/// planner; the execution safety posture is unchanged, now inside the
/// planner's executor:
///  * read-only clones are refused outright (403);
///  * ids and branch names are validated before they go near argv;
///  * `ResetBranch` is compare-and-swap — the branch must still point at
///    `expected_tip`, so a hint from a stale menu can never reset away work
///    that happened after it was shown (409 when it has moved);
///  * resetting the *checked-out* branch requires a clean working tree —
///    `git reset --hard` must never eat uncommitted work. A branch that isn't
///    checked out moves with `git branch -f`, which touches no worktree;
///  * a conflicted `git revert` is auto-aborted (like `/api/rebase`), so a
///    browser-only user is never left mid-revert with no shell to fix it.
///
/// Every successful undo is itself journaled — it's a repo event like any
/// other, shows up in the feed attributed to the app, and (for a reset) gets
/// its own undo hint, which is what makes "undo the undo" fall out for free.
pub async fn undo(Json(action): Json<UndoAction>) -> (StatusCode, String) {
    if let Some(rejected) = crate::state::reject_if_read_only() {
        return rejected;
    }
    // D2 (#66, Task 7): the validated resolution, replacing a raw
    // `state::current()` call — see `state::resolve_target`'s doc comment.
    let repo = match crate::state::resolve_target() {
        Ok((repo, _entry)) => repo,
        Err(rejected) => return rejected,
    };
    let op = match undo_action_to_operation(&repo, action).await {
        Ok(op) => op,
        Err(refused) => return refused,
    };
    crate::planner::plan_and_execute(op).await
}

/// Validate one [`UndoAction`]'s fields — same checks, same wording, same
/// order — and build the matching [`GitOperation`]. The part of [`undo`] that
/// has nothing to do with *executing* it.
///
/// Factored out for M3.25 (#78) so the Recovery Center's
/// `POST /api/operations/{id}/recover` builds the identical operation from an
/// `UndoAction` it has already re-derived and verified live, without a second
/// copy of this validation to drift from `/api/undo`'s.
pub(crate) async fn undo_action_to_operation(
    repo: &Path,
    action: UndoAction,
) -> Result<GitOperation, (StatusCode, String)> {
    match action {
        UndoAction::RestoreBranch { name, tip } => {
            let name = name.trim();
            if !is_safe_branch_name(name) {
                return Err((StatusCode::BAD_REQUEST, "Bad branch name.".to_string()));
            }
            if !is_hex_id(&tip) {
                return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
            }
            let Ok(name) = BranchName::new(name) else {
                return Err((StatusCode::BAD_REQUEST, "Bad branch name.".to_string()));
            };
            let tip = undo_commit_oid(repo, &tip).await?;
            Ok(GitOperation::RestoreBranch { name, tip })
        }
        UndoAction::ResetBranch {
            branch,
            to,
            expected_tip,
        } => {
            let branch = branch.trim();
            if !is_safe_branch_name(branch) {
                return Err((StatusCode::BAD_REQUEST, "Bad branch name.".to_string()));
            }
            if !is_hex_id(&to) || !is_hex_id(&expected_tip) {
                return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
            }
            let Ok(branch) = BranchName::new(branch) else {
                return Err((StatusCode::BAD_REQUEST, "Bad branch name.".to_string()));
            };
            // The hint's compare-and-swap tip must be exact — the feed only
            // ever hands out full ids, so anything else is a hand-crafted
            // request whose CAS could never have matched the live tip anyway.
            let Ok(expected_tip) = CommitOid::new(expected_tip) else {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "‘{branch}’ has moved since this undo was offered — refresh and try again."
                    ),
                ));
            };
            let to = undo_commit_oid(repo, &to).await?;
            Ok(GitOperation::ResetBranch {
                branch,
                to,
                expected_tip,
            })
        }
        UndoAction::RevertCommit { commit } => {
            if !is_hex_id(&commit) {
                return Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string()));
            }
            let commit = undo_commit_oid(repo, &commit).await?;
            Ok(GitOperation::RevertCommit { commit })
        }
    }
}

/// An undo id as an exact [`CommitOid`]: the feed's full 40/64-hex ids are
/// taken as-is; an abbreviated (hand-crafted) id is resolved through
/// `git rev-parse` — the commands the old handler passed short ids straight
/// to accepted them the same way.
async fn undo_commit_oid(repo: &Path, given: &str) -> Result<CommitOid, (StatusCode, String)> {
    if let Ok(oid) = CommitOid::new(given) {
        return Ok(oid);
    }
    match crate::git_cmd::rev_parse(repo, given).await {
        Ok(Some(full)) => CommitOid::new(full).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "git rev-parse returned an unusable id.".to_string(),
            )
        }),
        // git ran and rejected the id: the request is wrong.
        Ok(None) => Err((StatusCode::BAD_REQUEST, "Not a commit id.".to_string())),
        // D5 (#66, Task 19): git never ran, so it rejected nothing. "Not a
        // commit id" here would be an assertion about the user's input made
        // on no evidence — and this is an *undo* path, where the id usually
        // came straight out of the app's own activity feed.
        Err(e) => Err(crate::planner::couldnt_run(
            "/api/undo",
            &format!("couldn't resolve ‘{given}’: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    /// The activity suite's seed: one commit named `base` holding `f.txt`.
    /// Named differently from the catalogue default because these tests assert
    /// on the subject line and add further commits to `f.txt`.
    fn seeded_repo() -> (tempfile::TempDir, PathBuf) {
        git_vista_fixtures::seeded_files(&[("f.txt", "line1\n")], "base")
    }

    fn run(repo: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap()
                .success(),
            "git {args:?} failed in {repo:?}"
        );
    }

    /// `git rev-parse <rev>` in `repo`, trimmed — plain and unsandboxed, since
    /// these tests are pinning `revert_would_conflict`'s own answer, not
    /// exercising the sandbox.
    fn rev_parse_plain(repo: &Path, rev: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-parse {rev} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn paging_event(index: usize) -> ActivityEvent {
        ActivityEvent {
            time: 10_000 + index as i64,
            kind: ActivityKind::Other,
            ref_name: None,
            summary: format!("paging-{index:03}"),
            old_oid: None,
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }
    }

    /// #559/#562 acceptance: the fixture really contains more than the former
    /// 500-event ceiling, and repeated page requests walk every folded event
    /// exactly once, newest-first, before an explicit end cursor terminates the
    /// loop. This is intentionally not a cursor round-trip test: the 620
    /// fixture summaries are enumerated independently, so truncating the fold
    /// would leave this assertion short even if every returned cursor decoded.
    ///
    /// Two failure-atlas mutations for completeness: change the full-fold
    /// `usize::MAX` to `MAX_PAGE_LIMIT` (the walk ends at 500), and advance
    /// `next_event` by one (one summary is missing and order equality fails).
    /// Two for termination disclosure: mint no cursor on a non-final page (the
    /// count is short), and mint one on the final page (the page budget trips).
    #[test]
    fn more_than_500_folded_events_are_walked_once_in_order_and_terminate() {
        const EVENT_COUNT: usize = 620;
        const PAGE_LIMIT: usize = 137;

        let (_dir, repo) = seeded_repo();
        let fixture: Vec<ActivityEvent> = (0..EVENT_COUNT).map(paging_event).collect();
        journal::append_all(&repo, &fixture);
        assert_eq!(journal::read_all(&repo).len(), EVENT_COUNT);

        let codec = CursorCodec::with_key([0x59; 32]);
        let canonical = repo.canonicalize().unwrap();
        let scope = codec.scope_for_target(None, &canonical);
        let mut cursor = None;
        let mut pages = 0;
        let mut walked = Vec::new();

        loop {
            pages += 1;
            assert!(pages <= 6, "the page walk did not terminate");
            let page = activity_page_for_target(
                &repo,
                false,
                scope,
                &ActivityParams {
                    limit: Some(PAGE_LIMIT),
                    cursor: cursor.clone(),
                },
                &codec,
            )
            .expect("an unchanged fixture accepts its own cursor");
            walked.extend(
                page.events
                    .iter()
                    .filter(|event| event.summary.starts_with("paging-"))
                    .map(|event| event.summary.clone()),
            );
            cursor = page.cursor;
            if cursor.is_none() {
                break;
            }
        }

        let expected: Vec<String> = (0..EVENT_COUNT)
            .rev()
            .map(|index| format!("paging-{index:03}"))
            .collect();
        assert_eq!(
            walked, expected,
            "the fold was truncated, skipped, or reordered"
        );
        let unique: HashSet<&String> = walked.iter().collect();
        assert_eq!(
            unique.len(),
            EVENT_COUNT,
            "an event appeared more than once"
        );
        assert_eq!(pages, 5, "620 events at 137 per page must take five pages");
        assert!(
            cursor.is_none(),
            "the final page must state that this is the end"
        );
    }

    /// A cursor names one exact fold, not an offset in whichever live fold is
    /// current later. Inserting a new head event must refuse with 409 rather
    /// than shift the old offset and silently skip/repeat a row.
    ///
    /// Mutations: remove the generation comparison, or digest only the page
    /// length instead of the full serialized feed; both turn this into `Ok`.
    #[test]
    fn a_cursor_refuses_a_feed_that_changed_at_the_head() {
        let (_dir, repo) = seeded_repo();
        journal::append_all(&repo, &(0..4).map(paging_event).collect::<Vec<_>>());
        let codec = CursorCodec::with_key([0x62; 32]);
        let canonical = repo.canonicalize().unwrap();
        let scope = codec.scope_for_target(None, &canonical);
        let first = activity_page_for_target(
            &repo,
            false,
            scope,
            &ActivityParams {
                limit: Some(2),
                cursor: None,
            },
            &codec,
        )
        .unwrap();
        let cursor = first.cursor.expect("four events require another page");

        journal::append(&repo, &paging_event(999));
        let err = activity_page_for_target(
            &repo,
            false,
            scope,
            &ActivityParams {
                limit: Some(2),
                cursor: Some(cursor),
            },
            &codec,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(err.1.contains("restart from the first page"), "{}", err.1);
    }

    /// The opaque cursor is both authenticated and repository-scoped before a
    /// source scan. It can resume neither a sibling target nor edited bytes.
    ///
    /// Mutations: remove the scope comparison (the foreign call succeeds), or
    /// skip the codec decode and trust payload bytes (the tampered call stops
    /// producing the generic bad-request refusal).
    #[test]
    fn an_activity_cursor_is_authenticated_and_bound_to_its_repository() {
        let (_dir, repo) = seeded_repo();
        journal::append_all(&repo, &(0..4).map(paging_event).collect::<Vec<_>>());
        let codec = CursorCodec::with_key([0x63; 32]);
        let canonical = repo.canonicalize().unwrap();
        let scope = codec.scope_for_target(None, &canonical);
        let first = activity_page_for_target(
            &repo,
            false,
            scope,
            &ActivityParams {
                limit: Some(2),
                cursor: None,
            },
            &codec,
        )
        .unwrap();
        let cursor = first.cursor.unwrap();

        let foreign_scope = codec.scope_for_target(None, &canonical.join("sibling"));
        let foreign = activity_page_for_target(
            &repo,
            false,
            foreign_scope,
            &ActivityParams {
                limit: Some(2),
                cursor: Some(cursor.clone()),
            },
            &codec,
        )
        .unwrap_err();
        assert_eq!(foreign, invalid_activity_cursor());

        let mut tampered = cursor.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        let refused = activity_page_for_target(
            &repo,
            false,
            scope,
            &ActivityParams {
                limit: Some(2),
                cursor: Some(tampered),
            },
            &codec,
        )
        .unwrap_err();
        assert_eq!(refused, invalid_activity_cursor());
    }

    /// The undo-hints path had its own 500-row cap. Put the only matching hint
    /// behind 600 newer folded rows so this test fails if that second cap ever
    /// returns, independently of query pagination.
    ///
    /// Mutations: cap assembly at `MAX_PAGE_LIMIT`, or cap it at one event;
    /// both erase the restore hint and fail this exact action assertion.
    #[test]
    fn undo_hints_search_past_the_former_500_event_ceiling() {
        let target = "a".repeat(40);
        let mut journal_events = vec![ActivityEvent {
            time: 1,
            kind: ActivityKind::BranchDeleted,
            ref_name: Some("old-topic".to_string()),
            summary: "deleted old-topic".to_string(),
            old_oid: Some(target.clone()),
            new_oid: None,
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }];
        journal_events.extend((0..600).map(|index| ActivityEvent {
            time: 2 + index as i64,
            ..paging_event(index)
        }));

        let hints = matching_feed_undoables(
            journal_events,
            Vec::new(),
            &HashMap::new(),
            &HashSet::new(),
            &target,
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0].action,
            UndoAction::RestoreBranch {
                name: "old-topic".to_string(),
                tip: target,
            }
        );
    }

    /// #327 defect A: the exact repro shape from the owner's session log —
    /// an earlier commit that later work still depends on must be reported
    /// as conflicting, not offered as a clean revert.
    ///
    /// Mutation this proves: flip `Some(0) => Ok(false), Some(1) => Ok(true)`
    /// to the opposite pair, or replace the whole match with `Ok(false)`, and
    /// this test goes red — the classification is load-bearing, not a
    /// round-trip.
    #[tokio::test]
    async fn a_commit_later_work_depends_on_is_reported_as_conflicting() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let to_revert = rev_parse_plain(&repo, "HEAD");
        let parent_of_to_revert = rev_parse_plain(&repo, "HEAD^");

        std::fs::write(repo.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line3, needs line2"]);
        let head_now = rev_parse_plain(&repo, "HEAD");

        let conflicts = revert_would_conflict(&repo, &to_revert, &parent_of_to_revert, &head_now)
            .await
            .expect("the check itself must run cleanly against a real repo");
        assert!(
            conflicts,
            "reverting a commit later work depends on must be flagged conflicting"
        );
    }

    /// The mirror case, in the same repo shape: reverting the current tip —
    /// nothing is built on top of it yet — must be reported clean. Proves
    /// the function distinguishes the two cases rather than always agreeing
    /// with one answer (the failure mode a test that only checks the
    /// conflicting case would miss entirely).
    #[tokio::test]
    async fn reverting_the_current_tip_with_no_dependents_is_clean() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let tip = rev_parse_plain(&repo, "HEAD");
        let parent = rev_parse_plain(&repo, "HEAD^");

        let conflicts = revert_would_conflict(&repo, &tip, &parent, &tip)
            .await
            .expect("the check itself must run cleanly against a real repo");
        assert!(
            !conflicts,
            "reverting the tip with nothing built on top must be clean"
        );
    }

    /// #581: the version gate, decided with literal versions on both sides.
    ///
    /// This is the whole point of the issue. `merge-tree --write-tree` arrived
    /// in git 2.38, the documented product floor is 2.32, and every host this
    /// project has run on is far above both — so a test that used the host's
    /// git could never reach the arm that matters. These literals can.
    ///
    /// 2.34.1 is not an arbitrary example: it is what Ubuntu 22.04 LTS ships,
    /// measured 2026-09-02, and running this function's argv against it exits
    /// **129** with `usage: git merge-tree <base-tree> <branch1> <branch2>`.
    #[test]
    fn the_merge_tree_gate_refuses_below_2_38_and_allows_2_38_itself() {
        // At the floor and above: no gate, the check proceeds.
        assert!(
            merge_tree_version_gate((2, 38, 0)).is_none(),
            "2.38.0 is the floor itself and must be allowed"
        );
        assert!(
            merge_tree_version_gate((2, 38, 7)).is_none(),
            "a patch level above the floor must be allowed"
        );
        assert!(
            merge_tree_version_gate((2, 53, 0)).is_none(),
            "this host's git must be allowed"
        );

        // Below: refused, and refused with the numbers, not a bare failure.
        assert_eq!(
            merge_tree_version_gate((2, 34, 1)),
            Some(RevertCheckError::GitTooOld {
                found: String::from("2.34.1"),
                minimum: String::from("2.38"),
            }),
            "Ubuntu 22.04's git must be named as too old, with both numbers"
        );
        assert_eq!(
            merge_tree_version_gate((2, 37, 9)),
            Some(RevertCheckError::GitTooOld {
                found: String::from("2.37.9"),
                minimum: String::from("2.38"),
            }),
            "the last version below the floor must still be refused"
        );
        // The documented product floor is inside the refused band. That is the
        // fact #581 exists to state: a fully supported host on which this one
        // check cannot run.
        assert!(
            merge_tree_version_gate((2, 32, 0)).is_some(),
            "the documented product floor 2.32 is below merge-tree's 2.38"
        );
    }

    /// #581: the gate is reached *through the real function*, not only in
    /// isolation.
    ///
    /// The pure test above proves the decision; this proves the decision is
    /// wired in. It cannot force an old git — [`crate::git_version`] caches the
    /// host's version per process — so it asserts the other half: on this
    /// host's new-enough git the gate does **not** fire, and the function still
    /// answers the real question rather than refusing.
    ///
    #[tokio::test]
    async fn a_new_enough_git_passes_the_gate_and_answers_the_real_question() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let tip = rev_parse_plain(&repo, "HEAD");
        let parent = rev_parse_plain(&repo, "HEAD^");

        match revert_would_conflict(&repo, &tip, &parent, &tip).await {
            Ok(_) => {}
            Err(RevertCheckError::GitTooOld { found, minimum }) => panic!(
                "the gate fired on this host's git ({found} < {minimum}); either \
                 this machine's git is genuinely below {minimum}, or the gate's \
                 comparison is inverted"
            ),
            Err(RevertCheckError::CheckFailed(detail)) => {
                panic!("the check itself failed: {detail}")
            }
        }
    }

    /// #581: the gate is actually *wired in*, proven against a real repository
    /// on the production path with only the version supplied.
    ///
    /// This is the test that makes deleting the gate a red build. The pure test
    /// above proves the decision is right; this proves the decision is
    /// consulted. Without it, removing the `if let Some(too_old)` block would
    /// leave every other test in this file green — the host's git is 2.5x, so
    /// nothing else can reach the refusal.
    ///
    /// The repository, the argv and the exit-code reading are all production;
    /// only the version is injected, because
    /// [`crate::git_version::current`] caches per process and cannot be made
    /// to lie.
    #[tokio::test]
    async fn an_old_git_is_refused_by_name_before_merge_tree_is_ever_run() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let tip = rev_parse_plain(&repo, "HEAD");
        let parent = rev_parse_plain(&repo, "HEAD^");

        // 2.34.1 is Ubuntu 22.04's git, measured 2026-09-02. On this exact
        // repository shape the same call with the host's real git answers
        // `Ok(false)` — the test above pins that — so an `Err` here can only
        // come from the gate.
        let refused =
            revert_would_conflict_at_version(&repo, &tip, &parent, &tip, (2, 34, 1)).await;

        assert_eq!(
            refused,
            Err(RevertCheckError::GitTooOld {
                found: String::from("2.34.1"),
                minimum: String::from("2.38"),
            }),
            "an old git must be refused by name, not fall through to merge-tree \
             and surface as an unexplained CheckFailed"
        );
    }

    /// The wiring itself, not just `revert_would_conflict`'s raw answer:
    /// `revert_offer_established` must say `false` for the exact conflicting
    /// shape above — the owner's repro. Where the two prior tests pin the
    /// classifier, this one pins the decision that actually reaches
    /// `undoables`'s `if` — the one place a flipped `!` or a wrong
    /// `unwrap_or` would silently reintroduce defect A with every other test
    /// in this file still green.
    ///
    /// Mutation this proves: change `.map(|conflicts| !conflicts)` to
    /// `.map(|conflicts| conflicts)`, or `unwrap_or(false)` to
    /// `unwrap_or(true)`, and this test goes red.
    #[tokio::test]
    async fn the_offer_decision_says_no_for_a_real_conflict() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let to_revert = rev_parse_plain(&repo, "HEAD");
        let parent_of_to_revert = rev_parse_plain(&repo, "HEAD^");

        std::fs::write(repo.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line3, needs line2"]);

        assert!(
            !revert_offer_established(&repo, &to_revert, &parent_of_to_revert).await,
            "the offer decision must say no for a commit later work depends on"
        );
    }

    /// The mirror case for the wiring: a clean revert (the tip, nothing
    /// built on it) must be offered. Without this leg,
    /// `revert_offer_established` could be hardcoded to `false` and the test
    /// above would still pass — proving `false` alone is not proof the
    /// wiring works.
    #[tokio::test]
    async fn the_offer_decision_says_yes_for_a_clean_revert() {
        let (_dir, repo) = seeded_repo();
        std::fs::write(repo.join("f.txt"), "line1\nline2\n").unwrap();
        run(&repo, &["add", "f.txt"]);
        run(&repo, &["commit", "-q", "-m", "add line2"]);
        let tip = rev_parse_plain(&repo, "HEAD");
        let parent = rev_parse_plain(&repo, "HEAD^");

        assert!(
            revert_offer_established(&repo, &tip, &parent).await,
            "the offer decision must say yes for a clean revert of the tip"
        );
    }
}
