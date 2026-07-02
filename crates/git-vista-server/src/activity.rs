//! `GET /api/activity` — the assembled activity feed.
//!
//! This handler is deliberately thin: it *collects* (current branches, the
//! journal, every reflog, the remote-commit set), lets the pure, unit-tested
//! [`assemble_feed`] do the folding, and maintains the ref snapshot on the
//! way through. The interesting logic lives in `git_vista_core::activity`
//! and `crate::journal`.
//!
//! Snapshot upkeep happens *here* — and only here — because detection and
//! bookkeeping must be one atomic step: whoever rewrites the snapshot must
//! first synthesize deletion events for branches that vanished since the last
//! one, or those deletions are silently forgotten. Keeping a single writer
//! makes that invariant easy to hold.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use git_vista_core::activity::{
    assemble_feed, ActivityEvent, ActivityKind, ActivitySource,
};
use git_vista_core::model::RefKind;
use git_vista_git::{read_reflogs, read_refs, read_remote_commits};

use crate::journal;

/// How many events the feed returns by default, and at most. The panel shows
/// a scrollable list, not an archive; anyone needing more can raise `limit`
/// up to the cap.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

/// Reflog entries read per ref. A rebase writes one line per replayed commit,
/// so this is deliberately far above the feed limit.
const REFLOG_PER_REF: usize = 200;

#[derive(Deserialize)]
pub struct ActivityParams {
    pub limit: Option<usize>,
}

/// Unix seconds now; the timestamp journaled onto synthesized events.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The feed: journal + reflogs + snapshot diff, folded newest-first.
pub async fn activity_feed(
    Query(params): Query<ActivityParams>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let repo = crate::current().0;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    // The current local branch → tip map: the baseline for undo hints and for
    // the next snapshot.
    let refs = read_refs(&repo).map_err(|e| {
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
    if let Some(snapshot) = journal::read_snapshot(&repo) {
        for (name, tip) in &snapshot {
            if !branches.contains_key(name) {
                journal::append(
                    &repo,
                    &ActivityEvent {
                        time: now_secs(),
                        kind: ActivityKind::BranchDeleted,
                        ref_name: Some(name.clone()),
                        summary: format!("deleted branch ‘{name}’ (outside git-vista)"),
                        old_oid: Some(tip.clone()),
                        new_oid: None,
                        source: ActivitySource::External,
                        undo: None,
                    },
                );
                println!("[/api/activity] noticed external deletion of branch '{name}' (was {tip})");
            }
        }
    }
    journal::write_snapshot(&repo, &branches);

    let journal_events = journal::read_all(&repo);
    let reflog = read_reflogs(&repo, REFLOG_PER_REF).map_err(|e| {
        eprintln!("git-vista: /api/activity failed reading reflogs: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // Which commits are on the remote — feeds the "already pushed" warning on
    // reset-style undo hints. Best-effort: no remote (or a failed walk) just
    // means no warnings.
    let remote: HashSet<String> =
        read_remote_commits(&repo, crate::HISTORY_LIMIT).unwrap_or_default();

    let feed = assemble_feed(journal_events, reflog, &branches, &remote, limit);
    let app_count = feed.iter().filter(|e| e.source == ActivitySource::App).count();
    println!(
        "[/api/activity] {} — {} event(s) ({app_count} via app), {} undoable",
        repo.display(),
        feed.len(),
        feed.iter().filter(|e| e.undo.is_some()).count(),
    );

    let no_store = [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))];
    Ok((no_store, Json(feed)))
}
