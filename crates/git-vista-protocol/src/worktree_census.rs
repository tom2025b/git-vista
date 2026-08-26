//! The worktree census (M11, #546): enumerating the linked worktree siblings
//! of a repository, from `git worktree list --porcelain -z`.
//!
//! # Why this exists
//!
//! `git-vista-core`'s identity model already gives every worktree its own
//! stable [`WorktreeId`](crate::plan::WorktreeToken) while every worktree of
//! one clone shares a `RepositoryId` — but nothing in this codebase ever
//! calls `git worktree list`, so the app cannot tell a user their repository
//! has five other desks open elsewhere, nor refuse to offer a checkout that
//! git itself would refuse. This module is the read that fixes the first
//! half of that; the collision-aware checkout is a later issue.
//!
//! # `Serviceable` is not `locked`/`prunable`, on purpose
//!
//! `locked` and `prunable` are **git's own flags**, read verbatim from the
//! porcelain stream. [`Serviceable`] is a *separate*, app-owned fence: can
//! this server's catalog actually open the sibling? A worktree can be
//! discovered and refused (outside the allowed roots) while being neither
//! locked nor prunable, and a locked worktree is still fully serviceable.
//! Folding these into one boolean is exactly the failure mode this type
//! exists to prevent — see [`Serviceable::OutsideAllowedRoots`] and
//! [`Serviceable::Missing`].
//!
//! # The census itself can fail, and that is not an empty list
//!
//! [`WorktreeCensus::CensusFailed`] is a distinct state from
//! `Observed(vec![])`: a repository with no linked worktrees is a real,
//! empty observation, but a `git worktree list` that could not be run or
//! parsed has observed *nothing at all*. Nothing downstream may treat a
//! failed census as evidence that any particular branch is free — collapsing
//! the two would let a checkout that should be refused (because some other
//! worktree already holds the branch) proceed on the strength of a read that
//! never happened.
//!
//! # Parsing is strict
//!
//! A porcelain record this parser does not understand is
//! [`WorktreeListParseError`], never a silently skipped line: a skipped
//! sibling is a worktree that quietly disappears from a census that claims
//! to be complete, which is the same failure class as a status read that
//! drops a file.

use serde::{Deserialize, Serialize};

use crate::plan::{BranchName, CommitOid, WorktreeToken};

/// One `git worktree list --porcelain -z` record, before enrichment
/// (identity, `is_current`, [`Serviceable`]) — the pure, wire-independent
/// intermediate [`parse_worktree_list_porcelain_z`] produces. The native
/// server is the only thing that can turn this into a [`WorktreeSibling`]:
/// enrichment needs the filesystem and the catalog's allowed roots, neither
/// of which this crate may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListRecord {
    /// The worktree's own path, exactly as git reports it — absolute,
    /// unresolved (no symlinks followed, not yet checked for existence).
    pub path: String,
    /// `bare` in the porcelain stream: a bare repository has no working tree
    /// and reports no `HEAD`/`branch`/`detached` line at all.
    pub bare: bool,
    /// The commit HEAD points at, as 40/64 lowercase hex. `None` only for a
    /// [`bare`](Self::bare) record — every other shape reports a `HEAD` line,
    /// including a detached one and the all-zero id of an unborn branch.
    pub head: Option<String>,
    /// The checked-out branch's short name (`refs/heads/<name>` with the
    /// prefix stripped). `None` for [`detached`](Self::detached) HEAD and for
    /// a [`bare`](Self::bare) record.
    pub branch: Option<String>,
    /// `detached` in the porcelain stream: HEAD is not on a branch.
    pub detached: bool,
    /// `locked` in the porcelain stream, with or without a reason — git's own
    /// flag, read verbatim.
    pub locked: bool,
    /// `prunable` in the porcelain stream, with or without a reason — git's
    /// own flag, read verbatim. Not the same question as whether the
    /// worktree's directory still exists; see [`Serviceable::Missing`] for
    /// the fact that actually answers that.
    pub prunable: bool,
}

/// Why [`parse_worktree_list_porcelain_z`] refused a stream. Carries enough
/// text to debug a real git version's output; never inferred from prose
/// beyond the fixed set of keys `git-worktree`(1) documents for `--porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListParseError(pub String);

impl std::fmt::Display for WorktreeListParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unparseable `git worktree list --porcelain` record: {}",
            self.0
        )
    }
}

impl std::error::Error for WorktreeListParseError {}

/// In-progress state for one record while scanning tokens, before the closing
/// blank line confirms it is complete.
#[derive(Default)]
struct Building {
    path: Option<String>,
    bare: bool,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
    locked: bool,
    prunable: bool,
}

impl Building {
    fn finish(self) -> Result<WorktreeListRecord, WorktreeListParseError> {
        let path = self
            .path
            .ok_or_else(|| WorktreeListParseError("record has no `worktree` line".into()))?;
        if self.bare {
            if self.head.is_some() || self.branch.is_some() || self.detached {
                return Err(WorktreeListParseError(format!(
                    "{path}: `bare` record also carries HEAD/branch/detached"
                )));
            }
        } else if self.head.is_none() {
            return Err(WorktreeListParseError(format!(
                "{path}: no `HEAD` line and not `bare`"
            )));
        } else if self.branch.is_some() == self.detached {
            // Exactly one of branch/detached must hold for a non-bare record:
            // real git always emits one or the other, even for an unborn
            // HEAD (which still reports `branch refs/heads/<name>`).
            return Err(WorktreeListParseError(format!(
                "{path}: expected exactly one of `branch`/`detached`, got branch={:?} detached={}",
                self.branch, self.detached
            )));
        }
        Ok(WorktreeListRecord {
            path,
            bare: self.bare,
            head: self.head,
            branch: self.branch,
            detached: self.detached,
            locked: self.locked,
            prunable: self.prunable,
        })
    }
}

/// Parse `git worktree list --porcelain -z`'s stdout into one
/// [`WorktreeListRecord`] per worktree.
///
/// `-z`, not the newline-terminated porcelain form: a worktree's path can
/// itself contain a newline on this platform, and `-z` is the shape
/// `git-worktree`(1) added precisely so a path can never be confused with a
/// record boundary. Records are NUL-terminated fields, terminated by an empty
/// field (git's line-based "blank line" becomes back-to-back NULs); this
/// mirrors [`crate::status::parse_porcelain_v2_z`]'s own NUL-token style, but
/// **strictly** where that parser is deliberately lenient — see this module's
/// doc comment for why the two need different failure postures.
///
/// Every key this function does not recognise, every record missing its
/// required fields, and every malformed hex/utf8 byte is
/// [`WorktreeListParseError`] — never a skipped line.
pub fn parse_worktree_list_porcelain_z(
    bytes: &[u8],
) -> Result<Vec<WorktreeListRecord>, WorktreeListParseError> {
    let mut records = Vec::new();
    let mut current = Building::default();
    let mut has_fields = false;

    for raw in bytes.split(|&b| b == 0) {
        if raw.is_empty() {
            if has_fields {
                records.push(std::mem::take(&mut current).finish()?);
                has_fields = false;
            }
            continue;
        }
        let line = String::from_utf8(raw.to_vec())
            .map_err(|e| WorktreeListParseError(format!("non-UTF-8 line: {e}")))?;
        has_fields = true;
        if let Some(path) = line.strip_prefix("worktree ") {
            if current.path.is_some() {
                return Err(WorktreeListParseError(format!(
                    "two `worktree` lines in one record ({:?} and {path:?})",
                    current.path
                )));
            }
            current.path = Some(path.to_string());
        } else if line == "bare" {
            current.bare = true;
        } else if let Some(oid) = line.strip_prefix("HEAD ") {
            current.head = Some(oid.to_string());
        } else if let Some(refname) = line.strip_prefix("branch ") {
            let short = refname.strip_prefix("refs/heads/").ok_or_else(|| {
                WorktreeListParseError(format!(
                    "`branch` line {refname:?} is not a `refs/heads/` ref"
                ))
            })?;
            current.branch = Some(short.to_string());
        } else if line == "detached" {
            current.detached = true;
        } else if line == "locked" || line.starts_with("locked ") {
            current.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            current.prunable = true;
        } else {
            return Err(WorktreeListParseError(format!(
                "unrecognised line {line:?}"
            )));
        }
    }
    if has_fields {
        records.push(current.finish()?);
    }
    Ok(records)
}

/// Whether this server can actually open a discovered worktree sibling — the
/// app's own fence, kept deliberately separate from git's `locked`/`prunable`
/// flags on [`WorktreeSibling`] (module doc explains why).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Serviceable {
    /// Inside an allowed root: selecting this sibling will work.
    Yes,
    /// Discovered, real, and refused — the sibling's directory exists but
    /// lies outside every root the operator allowed. It still counts for
    /// branch-collision purposes (git does not know or care about this
    /// app's fence); only *opening* it is refused.
    OutsideAllowedRoots,
    /// The directory is gone but git still lists the worktree (`prunable`
    /// tracks the same fact from git's side). Distinct from
    /// [`OutsideAllowedRoots`](Self::OutsideAllowedRoots): there is no
    /// directory left to admit or refuse.
    Missing,
}

/// One worktree sibling of a repository (M11, #546) — a row from
/// `git worktree list --porcelain -z`, enriched with this server's own
/// identity and fence.
///
/// Additive: no `deny_unknown_fields`, so a future field (e.g. surfacing
/// `locked`/`prunable`'s reason text) can be added without a wire break,
/// matching [`crate::dto::RepositoryDescriptor`]'s own contract rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSibling {
    /// The opaque id of this worktree — the string form of
    /// `git-vista-core`'s `WorktreeId`, the same identity
    /// [`crate::dto::RepositoryDescriptor::worktree`] already carries for a
    /// *registered* worktree. A sibling this server has never registered
    /// (e.g. [`Serviceable::OutsideAllowedRoots`]) still gets one: the id is
    /// derived from the worktree's on-disk git directory, not from catalog
    /// membership.
    pub id: WorktreeToken,
    /// The worktree's absolute path, omitted unless the operator opted into
    /// path exposure (`GIT_VISTA_EXPOSE_PATHS`) — the same default-hidden
    /// contract [`crate::dto::RepositoryDescriptor::path`] already has.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// The checked-out branch, or `None` for a detached HEAD or a bare
    /// repository record.
    pub branch: Option<BranchName>,
    /// The commit HEAD points at, or `None` only for a bare repository
    /// record (which has no working tree to have a HEAD "at").
    pub head: Option<CommitOid>,
    /// True for exactly one sibling in a given [`WorktreeCensus::Observed`]
    /// list: the worktree the census was requested against.
    pub is_current: bool,
    /// Git's own lock flag, read verbatim from the porcelain stream.
    pub locked: bool,
    /// Git's own prunable flag, read verbatim from the porcelain stream. Not
    /// the same fact as [`Serviceable::Missing`] — see that variant.
    pub prunable: bool,
    /// This server's fence, kept separate from `locked`/`prunable` above.
    pub serviceable: Serviceable,
}

/// The result of enumerating a repository's worktree siblings (M11, #546):
/// either a real (possibly empty) observation, or a distinct failure that
/// must never be mistaken for one. See the module doc for why the two are
/// not interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum WorktreeCensus {
    /// `git worktree list --porcelain -z` was read and parsed. Possibly
    /// empty — a repository with no linked worktrees is a real observation,
    /// not a failure.
    Observed { siblings: Vec<WorktreeSibling> },
    /// The list could not be read or parsed (spawn failure, non-zero exit, a
    /// record this parser does not understand, or an identity that could not
    /// be resolved). **Not** an empty list: nothing downstream may treat this
    /// as evidence that any branch is free.
    CensusFailed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            out.extend_from_slice(line.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn one_main_worktree_parses() {
        let bytes = z(&[
            "worktree /home/user/git-vista",
            "HEAD b7a947f8011f10fa6362e0ec96d9d766ca1f92a6",
            "branch refs/heads/main",
            "",
            "",
        ]);
        let records = parse_worktree_list_porcelain_z(&bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "/home/user/git-vista");
        assert_eq!(
            records[0].head.as_deref(),
            Some("b7a947f8011f10fa6362e0ec96d9d766ca1f92a6")
        );
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert!(!records[0].bare);
        assert!(!records[0].detached);
        assert!(!records[0].locked);
        assert!(!records[0].prunable);
    }

    #[test]
    fn main_plus_linked_and_locked_and_prunable_and_detached_and_bare() {
        let bytes = z(&[
            "worktree /repo/main",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch refs/heads/main",
            "",
            "worktree /repo/locked-wt",
            "HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "branch refs/heads/feature",
            "locked a reason",
            "",
            "worktree /repo/gone",
            "HEAD cccccccccccccccccccccccccccccccccccccccc",
            "branch refs/heads/stale",
            "prunable gitdir file points to non-existent location",
            "",
            "worktree /repo/detached-wt",
            "HEAD dddddddddddddddddddddddddddddddddddddddd",
            "detached",
            "",
            "worktree /repo/bare.git",
            "bare",
            "",
            "",
        ]);
        let records = parse_worktree_list_porcelain_z(&bytes).unwrap();
        assert_eq!(records.len(), 5);
        assert!(records[1].locked);
        assert!(!records[1].prunable);
        assert!(records[2].prunable);
        assert!(!records[2].locked);
        assert!(records[3].detached);
        assert_eq!(records[3].branch, None);
        assert!(records[4].bare);
        assert_eq!(records[4].head, None);
        assert_eq!(records[4].branch, None);
    }

    #[test]
    fn locked_with_no_reason_still_sets_the_flag() {
        let bytes = z(&[
            "worktree /repo/wt",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch refs/heads/x",
            "locked",
            "",
            "",
        ]);
        let records = parse_worktree_list_porcelain_z(&bytes).unwrap();
        assert!(records[0].locked);
    }

    #[test]
    fn an_unrecognised_line_is_an_error_not_a_skip() {
        let bytes = z(&[
            "worktree /repo/wt",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch refs/heads/x",
            "some-future-flag never seen before",
            "",
            "",
        ]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("unrecognised line"), "got: {}", err.0);
    }

    #[test]
    fn a_record_missing_head_and_not_bare_is_an_error() {
        let bytes = z(&["worktree /repo/wt", "branch refs/heads/x", "", ""]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("no `HEAD` line"), "got: {}", err.0);
    }

    #[test]
    fn a_bare_record_carrying_head_is_an_error() {
        let bytes = z(&[
            "worktree /repo/bare.git",
            "bare",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "",
            "",
        ]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("also carries"), "got: {}", err.0);
    }

    #[test]
    fn neither_branch_nor_detached_on_a_non_bare_record_is_an_error() {
        let bytes = z(&[
            "worktree /repo/wt",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "",
            "",
        ]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("exactly one of"), "got: {}", err.0);
    }

    #[test]
    fn both_branch_and_detached_on_one_record_is_an_error() {
        let bytes = z(&[
            "worktree /repo/wt",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch refs/heads/x",
            "detached",
            "",
            "",
        ]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("exactly one of"), "got: {}", err.0);
    }

    #[test]
    fn a_branch_line_not_under_refs_heads_is_an_error() {
        let bytes = z(&[
            "worktree /repo/wt",
            "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch refs/remotes/origin/x",
            "",
            "",
        ]);
        let err = parse_worktree_list_porcelain_z(&bytes).unwrap_err();
        assert!(err.0.contains("not a `refs/heads/`"), "got: {}", err.0);
    }

    #[test]
    fn empty_input_is_an_empty_list_not_an_error() {
        assert_eq!(parse_worktree_list_porcelain_z(&[]).unwrap(), vec![]);
    }

    #[test]
    fn an_unborn_head_still_reports_a_branch_line() {
        // Real git behaviour on a fresh `git init`: HEAD is the all-zero oid
        // but a `branch` line is still emitted.
        let bytes = z(&[
            "worktree /repo/fresh",
            "HEAD 0000000000000000000000000000000000000000",
            "branch refs/heads/master",
            "",
            "",
        ]);
        let records = parse_worktree_list_porcelain_z(&bytes).unwrap();
        assert_eq!(
            records[0].head.as_deref(),
            Some("0000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn serviceable_round_trips_through_json_by_tag() {
        let yes = serde_json::to_value(Serviceable::Yes).unwrap();
        assert_eq!(yes, serde_json::json!({"kind": "yes"}));
        let outside = serde_json::to_value(Serviceable::OutsideAllowedRoots).unwrap();
        assert_eq!(
            outside,
            serde_json::json!({"kind": "outside_allowed_roots"})
        );
        let missing: Serviceable =
            serde_json::from_value(serde_json::json!({"kind": "missing"})).unwrap();
        assert_eq!(missing, Serviceable::Missing);
    }

    #[test]
    fn census_failed_is_not_an_empty_observed_list() {
        let failed = WorktreeCensus::CensusFailed {
            reason: "spawn failed".to_string(),
        };
        let observed = WorktreeCensus::Observed { siblings: vec![] };
        assert_ne!(failed, observed);
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["status"], "census_failed");
        assert!(json.get("siblings").is_none());
    }
}
