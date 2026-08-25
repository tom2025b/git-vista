//! Reading conflicted paths out of git's index (M4.31, #84).
//!
//! The vocabulary lives in `git_vista_protocol::conflict`; this is the half
//! that talks to git. One scan answers "what is conflicted, and what are the
//! three versions" for every unmerged path, whatever operation left them there
//! — a merge, rebase, cherry-pick, revert, stash pop or pull all produce the
//! same index state and git does not record which.
//!
//! # Read-only, always
//!
//! Nothing here writes. Resolution is a later slice and a different code path;
//! this module exists so a resolver — and the continuation gate — can see what
//! it is dealing with before anything is decided.
//!
//! # Failure is per-stage, never per-scan
//!
//! A blob that will not read becomes [`Stage::Unreadable`] for that one side.
//! It does not fail the whole scan and it does not silently vanish, because a
//! path missing from a conflict listing reads as "resolved" — the single worst
//! way this could be wrong, since it would let an operation continue over a
//! file nobody looked at.

use git_vista_protocol::conflict::{ConflictedFile, Continuation, NotTextResolvable, Stage};
use git_vista_protocol::status::ConflictKind;
use std::collections::BTreeMap;
use std::path::Path;

/// How much of a blob to sniff when deciding whether it is binary.
///
/// Git's own heuristic reads the first 8000 bytes and calls the content binary
/// if it contains a NUL. Matching that number is deliberate: a file git treats
/// as binary and a file this scanner treats as text would disagree about
/// whether a text resolver can be offered, and git's judgement is the one the
/// rest of the toolchain acts on.
const BINARY_SNIFF_BYTES: usize = 8000;

/// One `git ls-files -u` row, before the three stages of a path are folded
/// together.
struct UnmergedEntry {
    stage: u8,
    oid: String,
    path: String,
}

/// Parse the NUL-separated output of `git ls-files -u -z`.
///
/// Each record is `<mode> SP <oid> SP <stage> TAB <path>`. Rows that do not fit
/// that shape are **skipped rather than erroring** — the same
/// undercount-not-failure posture `status::parse_porcelain_v2_z` takes, for the
/// same reason: the format is git's own and versioned, so something
/// unrecognised is likelier to be a future addition than corruption.
///
/// The one asymmetry worth knowing: an undercount here is not free. A skipped
/// row means a stage silently missing, which the caller will render as
/// [`Stage::Absent`] — "there is nothing on this side" — when in truth it was
/// not understood. That is why the shape check below is loose about *content*
/// (any oid text, any path) and strict about *structure*.
fn parse_unmerged(stdout: &[u8]) -> Vec<UnmergedEntry> {
    let mut out = Vec::new();
    for record in stdout.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let Some((meta, path)) = text.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let (_mode, oid, stage) = (parts.next(), parts.next(), parts.next());
        let (Some(oid), Some(stage)) = (oid, stage) else {
            continue;
        };
        let Ok(stage) = stage.parse::<u8>() else {
            continue;
        };
        if !(1..=3).contains(&stage) || oid.is_empty() || path.is_empty() {
            continue;
        }
        out.push(UnmergedEntry {
            stage,
            oid: oid.to_string(),
            path: path.to_string(),
        });
    }
    out
}

/// Read one blob's size and whether it is binary.
///
/// Both come from a single `cat-file` read of the first
/// [`BINARY_SNIFF_BYTES`], plus `cat-file -s` for the true size — the sniff
/// alone cannot give the size of anything larger than it reads.
async fn describe_blob(repo: &Path, oid: &str) -> Stage {
    let Ok(parsed_oid) = git_vista_protocol::plan::CommitOid::new(oid) else {
        return Stage::Unreadable {
            reason: format!("git reported an object id this server cannot parse: {oid}"),
        };
    };

    let size = match crate::git_cmd::git_output(repo, &["cat-file", "-s", oid]).await {
        Err(e) => {
            return Stage::Unreadable {
                reason: format!("couldn't run git cat-file: {e}"),
            }
        }
        Ok(out) if !out.status.success() => {
            return Stage::Unreadable {
                reason: format!(
                    "git could not size the object — {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            }
        }
        Ok(out) => match String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                return Stage::Unreadable {
                    reason: "git reported a size this server could not read as a number".into(),
                }
            }
        },
    };

    // The content sniff. A failure here is `Unreadable` rather than a guess at
    // `binary: false` — defaulting to text would offer a line-level resolver
    // for content nobody managed to read.
    let content = match crate::git_cmd::git_output(repo, &["cat-file", "blob", oid]).await {
        Err(e) => {
            return Stage::Unreadable {
                reason: format!("couldn't run git cat-file blob: {e}"),
            }
        }
        Ok(out) if !out.status.success() => {
            return Stage::Unreadable {
                reason: format!(
                    "git could not read the object — {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            }
        }
        Ok(out) => out.stdout,
    };

    let binary = content.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0);

    Stage::Present {
        oid: parsed_oid,
        binary,
        size_bytes: size,
    }
}

/// Classify why a path cannot be resolved as text, if it cannot.
///
/// Returns `None` for an ordinary text conflict. Deliberately does **not**
/// attempt rename detection: git's index records no rename information for
/// conflicts, so claiming one would be inventing a fact. `NotTextResolvable::Rename`
/// exists for a caller that has done that work by other means.
fn not_text_resolvable(
    kind: ConflictKind,
    ours: &Stage,
    theirs: &Stage,
) -> Option<NotTextResolvable> {
    let ours_deleted = matches!(
        kind,
        ConflictKind::DeletedByUs | ConflictKind::BothDeleted | ConflictKind::AddedByThem
    );
    let theirs_deleted = matches!(
        kind,
        ConflictKind::DeletedByThem | ConflictKind::BothDeleted | ConflictKind::AddedByUs
    );
    if ours_deleted || theirs_deleted {
        return Some(NotTextResolvable::Deletion {
            ours_deleted,
            theirs_deleted,
        });
    }

    let ours_binary = matches!(ours, Stage::Present { binary: true, .. });
    let theirs_binary = matches!(theirs, Stage::Present { binary: true, .. });
    if ours_binary || theirs_binary {
        return Some(NotTextResolvable::Binary {
            ours: ours_binary,
            theirs: theirs_binary,
        });
    }
    None
}

/// The `conflict-v1:` token for one served [`ConflictSource`](git_vista_protocol::ConflictSource)
/// document (M4.31c, #432, ADR 0069).
///
/// Repository generation (HEAD, every ref, the index checksum — via
/// [`git_vista_git::read_generation_inputs`], the same reader `/api/status`
/// and `/api/history` already use) plus a digest of the marker-file bytes
/// served, folded together exactly the way `handlers/read.rs`'s `diff-v1:`
/// recipe folds direction and patch bytes.
///
/// Still read-only, despite this module's header claiming nothing here
/// writes: minting a token reads the repository and hashes bytes already in
/// hand. Two-phase, like `diff-v1:` — the handler mints this when serving the
/// document; [`crate::planner`]'s executor re-mints it from the LIVE file,
/// inside the coordinator lock, immediately before writing anything, and
/// refuses on any mismatch. That is the mechanism ADR 0069 exists to
/// establish: this is the one input no repository-level generation can see on
/// its own, because porcelain v2 carries stage OIDs but no worktree hash.
pub(crate) async fn conflict_source_token(
    repo: &Path,
    path: &str,
    marker_bytes: &[u8],
) -> Result<git_vista_protocol::GenerationToken, String> {
    let mut inputs = git_vista_git::read_generation_inputs(repo)
        .map_err(|e| format!("couldn't read generation inputs: {e}"))?;
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        // Namespaced and path-folded the same way `diff-v1:` folds its
        // direction: two different paths' marker files must never hash to the
        // same digest merely because their bytes happened to collide.
        hasher.update(b"conflict-source:marker-file\0");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(marker_bytes);
        format!("{:x}", hasher.finalize())
    };
    inputs.worktree(&digest);
    let generation = inputs.generation();
    git_vista_protocol::GenerationToken::new(format!("conflict-v1:{generation}"))
        .map_err(|e| format!("couldn't build the conflict-v1 token: {e}"))
}

/// Every conflicted path in the repository, with all three stages described.
///
/// An `Err` means the *scan* failed and the caller must surface that — it must
/// never be turned into an empty list, which would read as "no conflicts" and
/// let an operation continue over files nobody looked at.
pub(crate) async fn scan(repo: &Path) -> Result<Vec<ConflictedFile>, String> {
    // Local (D3): reading the index touches no remote.
    let out = crate::git_cmd::git_output(repo, &["ls-files", "-u", "-z"])
        .await
        .map_err(|e| format!("couldn't run git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files failed — {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    // BTreeMap so the result is ordered by path and a scan is reproducible;
    // an unordered listing would make two identical repository states produce
    // different responses and defeat any caching a client does.
    let mut by_path: BTreeMap<String, [Option<String>; 3]> = BTreeMap::new();
    for e in parse_unmerged(&out.stdout) {
        by_path.entry(e.path).or_default()[usize::from(e.stage) - 1] = Some(e.oid);
    }

    let kinds = conflict_kinds(repo).await?;

    let mut files = Vec::with_capacity(by_path.len());
    for (path, oids) in by_path {
        let mut stages = Vec::with_capacity(3);
        for oid in &oids {
            stages.push(match oid {
                Some(oid) => describe_blob(repo, oid).await,
                // No index entry at this stage. See the protocol module's
                // stage table: this is the conflict's shape, not a failure.
                None => Stage::Absent {},
            });
        }
        let (theirs, ours, base) = (
            stages.pop().expect("three stages pushed"),
            stages.pop().expect("three stages pushed"),
            stages.pop().expect("three stages pushed"),
        );

        // A path that `ls-files -u` reports but `status` did not classify is a
        // disagreement between two git reads. Reported as BothModified — the
        // ordinary text-conflict shape — rather than skipped, because dropping
        // it would hide a conflicted file entirely.
        let kind = kinds
            .get(&path)
            .copied()
            .unwrap_or(ConflictKind::BothModified);

        files.push(ConflictedFile {
            not_text_resolvable: not_text_resolvable(kind, &ours, &theirs),
            path,
            kind,
            base,
            ours,
            theirs,
        });
    }
    Ok(files)
}

/// The porcelain-v2 conflict classification for every conflicted path, reused
/// from the existing status parser rather than re-derived from `ls-files`'s
/// stage pattern.
///
/// Two readers of the same repository must not disagree about what kind of
/// conflict a path has, and the stage pattern alone cannot always tell: stages
/// 2+3 present looks identical for `BothModified` and `BothAdded`, which differ
/// only in whether stage 1 existed — and a resolver shows a different UI for
/// each.
async fn conflict_kinds(repo: &Path) -> Result<BTreeMap<String, ConflictKind>, String> {
    let out = crate::git_cmd::git_output(
        repo,
        &["status", "--porcelain=v2", "-z", "--untracked-files=no"],
    )
    .await
    .map_err(|e| format!("couldn't run git status: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status failed — {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let parsed = git_vista_protocol::status::parse_porcelain_v2_z(&out.stdout);
    Ok(parsed
        .entries
        .into_iter()
        .filter_map(|e| match e {
            git_vista_protocol::status::StatusEntry::Conflicted { path, kind, .. } => {
                Some((path, kind))
            }
            _ => None,
        })
        .collect())
}

/// Whether an in-progress operation may continue.
///
/// Propagates a scan failure rather than answering; a gate that says "clear"
/// because it could not look is the failure this whole crate is organised
/// against.
pub(crate) async fn continuation(repo: &Path) -> Result<Continuation, String> {
    Ok(Continuation::from_files(&scan(repo).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_fixtures::{conflict_add_add, conflict_modify_modify as conflicted_repo, seeded};

    #[tokio::test]
    async fn a_real_merge_conflict_yields_all_three_stages() {
        let (_d, repo) = conflicted_repo();
        let files = scan(&repo).await.expect("scan must succeed");

        assert_eq!(files.len(), 1, "one conflicted path, got {files:?}");
        let f = &files[0];
        assert_eq!(f.path, "a.txt");
        assert_eq!(f.kind, ConflictKind::BothModified);
        // All three sides exist for a modify/modify conflict — this is the
        // case where a base genuinely is available.
        assert!(
            f.base.is_text(),
            "base should be present text: {:?}",
            f.base
        );
        assert!(f.ours.is_text());
        assert!(f.theirs.is_text());
        assert!(f.all_sides_readable());
        assert!(
            f.not_text_resolvable.is_none(),
            "a plain modify/modify conflict is text-resolvable"
        );
    }

    #[tokio::test]
    async fn an_add_add_conflict_has_no_base_and_that_is_not_a_failure() {
        // MUTATION: report a missing stage as Unreadable. Every add/add
        // conflict would then look broken, and `all_sides_readable` would
        // refuse to offer a resolver for a perfectly resolvable file.
        let (_d, repo) = conflict_add_add();

        let files = scan(&repo).await.expect("scan must succeed");
        let f = files.iter().find(|f| f.path == "c.txt").expect("c.txt");
        assert_eq!(f.base, Stage::Absent {}, "add/add has no common ancestor");
        assert!(f.ours.is_text());
        assert!(f.theirs.is_text());
        assert!(
            f.all_sides_readable(),
            "an absent base must not make a file unreadable"
        );
    }

    #[tokio::test]
    async fn a_clean_repository_is_clear_and_a_conflicted_one_is_blocked() {
        let (_d, repo) = seeded();

        assert!(continuation(&repo).await.unwrap().may_continue());

        let (_d2, conflicted) = conflicted_repo();
        let blocked = continuation(&conflicted).await.unwrap();
        assert!(!blocked.may_continue());
        match blocked {
            Continuation::Blocked {
                unresolved,
                unreadable,
            } => {
                assert_eq!(unresolved, vec!["a.txt".to_string()]);
                assert!(unreadable.is_empty());
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_scan_of_a_path_that_is_not_a_repository_is_an_error_not_an_empty_list() {
        // THE test in this file. MUTATION: map the ls-files failure to
        // `Ok(vec![])`. `continuation` would then report Clear for a
        // repository it could not read, and an operation would proceed over
        // conflicts nobody looked at — a green light meaning "I did not check".
        let dir = tempfile::tempdir().unwrap();
        let err = scan(dir.path()).await.expect_err("must not report success");
        assert!(
            err.contains("ls-files"),
            "the error must name what failed: {err}"
        );
    }

    #[test]
    fn a_malformed_ls_files_row_is_skipped_not_guessed_at() {
        let good = b"100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1\ta.txt\0";
        assert_eq!(parse_unmerged(good).len(), 1);

        // No tab, no stage number, stage out of range: each skipped.
        assert!(parse_unmerged(b"garbage-with-no-tab\0").is_empty());
        assert!(parse_unmerged(b"100644 aaaa notanumber\ta.txt\0").is_empty());
        assert!(parse_unmerged(b"100644 aaaa 9\ta.txt\0").is_empty());
        assert!(parse_unmerged(b"100644 aaaa 1\t\0").is_empty());
    }

    #[test]
    fn a_deletion_conflict_is_not_text_resolvable_and_names_the_side() {
        // MUTATION: return None for deletions. A resolver would open a text
        // merge view for a file one side deleted, with a phantom empty pane
        // standing in for the deletion.
        let present = Stage::Present {
            oid: git_vista_protocol::plan::CommitOid::new("a".repeat(40)).unwrap(),
            binary: false,
            size_bytes: 3,
        };
        match not_text_resolvable(ConflictKind::DeletedByThem, &present, &Stage::Absent {}) {
            Some(NotTextResolvable::Deletion {
                ours_deleted,
                theirs_deleted,
            }) => {
                assert!(!ours_deleted);
                assert!(theirs_deleted);
            }
            other => panic!("expected a Deletion reason, got {other:?}"),
        }
        assert!(
            not_text_resolvable(ConflictKind::BothModified, &present, &present).is_none(),
            "an ordinary modify/modify conflict IS text-resolvable"
        );
    }
}
