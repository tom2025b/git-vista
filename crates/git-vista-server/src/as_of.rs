//! One signed position in an exact activity fold, backed only by captured refs.
//! No token is persisted, no timestamp is an identity, and no live ref can fill
//! a missing historical field. See ADR 0149 and issue #136.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use axum::http::StatusCode;
use git_vista_core::activity::{refs_at, ActivityEvent, HeadAtEvent, RefsAtEvent};
use git_vista_core::identity::ObjectId;
use git_vista_core::model::{GitRef, Oid, RefKind};
use git_vista_protocol::{AsOfAvailability, AsOfUnavailable, BranchName, GenerationToken};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::activity::{activity_generation, collect_activity_feed};
use crate::history::{
    snapshot_from_materials, CursorCodec, CursorScope, HistorySnapshot, SnapshotOrigin,
};

/// A different state shape from both ActivityCursor and HistoryCursor. Serde
/// rejects either of those at this boundary even under the same signing key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AsOfPosition {
    event_index: usize,
}

fn invalid_token() -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, "invalid as-of token".into())
}

fn moved() -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        "activity moved; reopen Activity and choose the observation again".into(),
    )
}

fn unavailable(reason: AsOfUnavailable) -> (StatusCode, String) {
    (StatusCode::UNPROCESSABLE_ENTITY, reason.to_string())
}

/// One request's object-check memo. A successful walk certifies all visited
/// commits' ancestry, so older captures usually cost only tip lookups. Failed
/// walks certify nothing. This is neither persistent state nor a commit table.
#[derive(Default)]
pub(crate) struct ObjectAvailability {
    complete: HashSet<Oid>,
    failed: HashMap<Oid, AsOfUnavailable>,
}

impl ObjectAvailability {
    fn check(&mut self, repo: &Path, snapshot: &HistorySnapshot) -> Result<(), AsOfUnavailable> {
        for tip in &snapshot.tips {
            if self.complete.contains(&tip.object_id) {
                continue;
            }
            if let Some(reason) = self.failed.get(&tip.object_id) {
                return Err(*reason);
            }
            let mut visited = Vec::new();
            let result = git_vista_git::walk_history_topo(
                repo,
                &[(tip.full_ref_name.clone(), tip.object_id.clone())],
                &[],
                |commit| {
                    visited.push(commit.id);
                    ControlFlow::Continue(())
                },
            );
            if result.is_err() {
                let reason = AsOfUnavailable::MissingCommitObjects;
                self.failed.insert(tip.object_id.clone(), reason);
                return Err(reason);
            }
            self.complete.extend(visited);
        }
        Ok(())
    }
}

fn oid(value: &str) -> Result<Oid, AsOfUnavailable> {
    ObjectId::parse(value).map_err(|_| AsOfUnavailable::InvalidCapture)?;
    Ok(Oid(value.to_string()))
}

fn branch(symbolic: &str) -> Result<String, AsOfUnavailable> {
    let short = symbolic
        .strip_prefix("refs/heads/")
        .ok_or(AsOfUnavailable::InvalidCapture)?;
    BranchName::new(short).map_err(|_| AsOfUnavailable::InvalidCapture)?;
    Ok(short.to_string())
}

/// Convert only facts actually present in the resolved journal observation.
/// `assemble_feed` already resolves batches; refs_at keeps unknown/orphaned
/// values fail-closed if this helper is ever given an unresolved row.
fn captured_snapshot(event: &ActivityEvent) -> Result<HistorySnapshot, AsOfUnavailable> {
    let capture =
        refs_at(event, std::slice::from_ref(event)).ok_or(AsOfUnavailable::CaptureNotRecorded)?;
    let RefsAtEvent::Captured {
        branches,
        truncated_at,
        head,
        tags,
        remotes,
        shallow,
        ..
    } = capture
    else {
        return Err(AsOfUnavailable::CaptureFailed);
    };
    let (Some(head), Some(tags), Some(remotes)) = (head, tags, remotes) else {
        return Err(AsOfUnavailable::IncompleteCapture);
    };
    if truncated_at.is_some() || tags.truncated_at.is_some() || remotes.truncated_at.is_some() {
        return Err(AsOfUnavailable::TruncatedCapture);
    }
    match shallow {
        Some(false) => {}
        Some(true) => return Err(AsOfUnavailable::ShallowRepository),
        None => return Err(AsOfUnavailable::ShallowStateNotRecorded),
    }
    let (head_symbolic_full, head_branch, resolved_head) = match head {
        HeadAtEvent::OnBranch {
            symbolic,
            oid: target,
        } => {
            let short = branch(symbolic)?;
            if branches.get(&short) != Some(target) {
                return Err(AsOfUnavailable::InvalidCapture);
            }
            (Some(symbolic.clone()), Some(short), Some(oid(target)?))
        }
        HeadAtEvent::Detached { oid: target } => (None, None, Some(oid(target)?)),
        HeadAtEvent::Unborn { symbolic } => {
            let short = branch(symbolic)?;
            if branches.contains_key(&short) {
                return Err(AsOfUnavailable::InvalidCapture);
            }
            (Some(symbolic.clone()), Some(short), None)
        }
        HeadAtEvent::Unreadable { .. } => return Err(AsOfUnavailable::UnreadableHead),
        HeadAtEvent::Unresolvable => return Err(AsOfUnavailable::UnresolvableHead),
    };
    let mut refs = Vec::new();
    if let Some(target) = &resolved_head {
        refs.push(GitRef {
            name: "HEAD".into(),
            kind: RefKind::Head,
            target: target.clone(),
        });
    }
    let mut full_ref_targets = Vec::new();
    for (prefix, kind, entries) in [
        ("refs/heads/", RefKind::Branch, branches),
        ("refs/remotes/", RefKind::RemoteBranch, &remotes.entries),
        ("refs/tags/", RefKind::Tag, &tags.entries),
    ] {
        for (name, target) in entries {
            // Apply the existing non-empty, non-option-shaped name boundary.
            // Traversal uses only the captured OID, never a lookup by this name.
            BranchName::new(name.as_str()).map_err(|_| AsOfUnavailable::InvalidCapture)?;
            let target = oid(target)?;
            refs.push(GitRef {
                name: name.clone(),
                kind: kind.clone(),
                target: target.clone(),
            });
            full_ref_targets.push((format!("{prefix}{name}"), target));
        }
    }
    // Live enumeration is full-ref-name order. Preserve that order for badges
    // too, including collisions such as a branch and tag both called main.
    refs.sort_by_key(|r| match r.kind {
        RefKind::Head => String::new(),
        RefKind::Branch => format!("refs/heads/{}", r.name),
        RefKind::RemoteBranch => format!("refs/remotes/{}", r.name),
        RefKind::Tag => format!("refs/tags/{}", r.name),
    });
    snapshot_from_materials(git_vista_git::HistoryMaterials {
        refs,
        full_ref_targets,
        head_symbolic_full,
        head_branch,
        resolved_head,
        shallow: Vec::new(), // observed unshallow, never today's boundary set
    })
    .map_err(|_| AsOfUnavailable::InvalidCapture)
}

fn replayable_snapshot(
    repo: &Path,
    event: &ActivityEvent,
    objects: &mut ObjectAvailability,
) -> Result<HistorySnapshot, AsOfUnavailable> {
    let snapshot = captured_snapshot(event)?;
    match crate::journal::shallow_state(repo) {
        Some(false) => {}
        Some(true) => return Err(AsOfUnavailable::ShallowRepository),
        None => return Err(AsOfUnavailable::ShallowStateNotRecorded),
    }
    objects.check(repo, &snapshot)?;
    Ok(snapshot)
}

/// Mint only after the complete replayability gate. The unsigned row remains
/// untouched so the response decoration cannot recursively change the fold.
pub(crate) fn availability(
    repo: &Path,
    event: &ActivityEvent,
    event_index: usize,
    scope: CursorScope,
    generation: &GenerationToken,
    codec: &CursorCodec,
    objects: &mut ObjectAvailability,
) -> Result<AsOfAvailability, (StatusCode, String)> {
    match replayable_snapshot(repo, event, objects) {
        Err(reason) => Ok(AsOfAvailability::Unavailable { reason }),
        Ok(_) => {
            let token = codec
                .encode(scope, generation, &AsOfPosition { event_index })
                .map_err(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not sign an as-of token".into(),
                    )
                })?;
            Ok(AsOfAvailability::Available { token })
        }
    }
}

pub(crate) fn redeem(
    repo: &Path,
    read_only: bool,
    scope: CursorScope,
    token: &str,
    codec: &CursorCodec,
) -> Result<HistorySnapshot, (StatusCode, String)> {
    // Authenticate and bind to the resolved worktree before any repository IO.
    let decoded = codec
        .decode::<AsOfPosition>(token)
        .map_err(|_| invalid_token())?;
    if decoded.scope != scope {
        return Err(invalid_token());
    }
    let feed = collect_activity_feed(repo, read_only)?;
    let generation = activity_generation(&feed)?;
    if decoded.generation != generation {
        return Err(moved());
    }
    let event = feed
        .get(decoded.state.event_index)
        .ok_or_else(invalid_token)?;
    let mut snapshot = replayable_snapshot(repo, event, &mut ObjectAvailability::default())
        .map_err(unavailable)?;
    // Separate live cursors and different observations, even when their refs
    // happen to match. Reuse canonical history generation as topology input.
    let bytes = serde_json::to_vec(&(&generation, decoded.state.event_index, &snapshot.generation))
        .map_err(|_| invalid_token())?;
    snapshot.generation = GenerationToken::new(format!("asof-v1:{:x}", Sha256::digest(bytes)))
        .map_err(|_| invalid_token())?;
    snapshot.origin = SnapshotOrigin::Captured {
        fold_generation: generation,
    };
    Ok(snapshot)
}

/// Recheck after construction as well: activity may move during a long walk.
/// This validates the selector's fold, never whether captured refs match live.
pub(crate) fn require_current_fold(
    repo: &Path,
    read_only: bool,
    expected: &GenerationToken,
) -> Result<(), (StatusCode, String)> {
    let feed = collect_activity_feed(repo, read_only)?;
    if activity_generation(&feed)? != *expected {
        return Err(moved());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
