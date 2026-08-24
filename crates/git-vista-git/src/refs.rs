//! Reading a repository's refs — HEAD, branches and tags — and the short name of
//! the currently checked-out branch, for badging and per-branch colouring.

use std::path::Path;

use gix::refs::Category;

use git_vista_core::activity::HeadAtEvent;
use git_vista_core::model::{GitRef, Oid, RefKind};

use crate::RepoError;

/// Read the repository's refs — HEAD, local & remote branches, and tags — each
/// peeled to the commit it ultimately points at, for badging and per-branch
/// colouring in the UI.
///
/// HEAD is emitted (as [`RefKind::Head`], named `"HEAD"`) exactly when it
/// **resolves to a commit**, whether it's on a branch or detached; when it's on
/// a branch the branch is emitted too, so a tip shows both. Refs that don't
/// resolve to a commit — an unborn HEAD, a HEAD holding an oid nothing resolves,
/// a broken ref — are skipped: every entry here is a claim about a commit, and
/// there is no commit to claim. Notes and worktree-private refs are ignored.
pub fn read_refs(path: &Path) -> Result<Vec<GitRef>, RepoError> {
    read_refs_at(path).map(|read| read.refs)
}

/// [`read_refs`]'s output plus the *state* of HEAD — which ref it named, and
/// whether that resolved — from the **same** open, so the two describe one
/// instant (#449).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefsAt {
    /// Display refs exactly as [`read_refs`] produces them.
    pub refs: Vec<GitRef>,
    /// Where HEAD pointed. This is strictly more than the `RefKind::Head`
    /// entry in `refs`: that entry exists only when HEAD resolves and carries
    /// the commit alone, so it cannot say *which branch* HEAD was on — the
    /// one fact a "watch the HEAD move" replay is made of.
    pub head: HeadAtEvent,
}

/// Read [`RefsAt`]: the badge refs and HEAD's state together, from one open.
///
/// This is the whole of [`read_refs`]'s work — that function is this one with
/// the HEAD state dropped — so the two can never drift apart, and no third
/// copy of the ref-classification loop enters the crate.
///
/// Deliberately **not** built on [`read_history_materials`], which would
/// otherwise supply the same HEAD facts:
///
/// 1. It also reads `$GIT_DIR/shallow` and treats malformed shallow metadata
///    as a hard error. A corrupt `shallow` file would then turn every
///    journaled event's capture into a failure, for a reason that has nothing
///    to do with refs.
/// 2. It reads HEAD through `head_name()`, whose failure is a hard error
///    there. Here a HEAD that will not read is [`HeadAtEvent::Unreadable`]
///    and the branches that *did* read are still returned.
///
/// The `RefKind::Head` badge comes from `head_id()`, the same resolved id
/// [`read_history_materials`] badges from — **not** from `repo.head()`, which
/// hands back a HEAD's raw object id without checking anything resolves to it.
/// Those two disagree for exactly one state, a HEAD holding an oid with no
/// object behind it, and this reader used to be the one that badged it. That
/// made two readers of one repository give opposite answers about whether HEAD
/// existed (#465, ADR 0071).
///
/// The fact is not discarded with the badge: a HEAD that points at nothing is
/// [`HeadAtEvent::Unresolvable`] in [`RefsAt::head`], which is the field that
/// can say so. `refs` is display refs peeled to commits; `head` is HEAD's
/// state. Only the second can describe a HEAD with no commit.
pub fn read_refs_at(path: &Path) -> Result<RefsAt, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let mut refs = Vec::new();

    // The two questions gix answers separately: the symbolic name HEAD holds,
    // and the commit it resolves to.
    let head_name = repo.head_name();
    let resolved = repo.head_id().ok().map(|id| id.detach().to_string());

    // HEAD first, so it's the leading badge on its commit — and badged from the
    // id that RESOLVED, never the raw one `repo.head()` hands back unvalidated
    // (#465). `read_history_materials` has always badged it this way; this is
    // the reader that disagreed.
    if let Some(oid) = &resolved {
        refs.push(GitRef {
            name: "HEAD".to_string(),
            kind: RefKind::Head,
            target: Oid(oid.clone()),
        });
    }

    // Classify HEAD from those same two answers. All five states below were
    // reproduced against gix 0.84 on real repositories; the `Unreadable` arm is
    // the one a repo with a corrupt `.git/HEAD` and an intact `.git/refs` lands
    // in, where the ref store still lists normally.
    let head = match head_name {
        Err(e) => HeadAtEvent::Unreadable {
            reason: e.to_string(),
        },
        Ok(name) => match (name.map(|n| n.as_bstr().to_string()), resolved) {
            (Some(symbolic), Some(oid)) => HeadAtEvent::OnBranch { symbolic, oid },
            (Some(symbolic), None) => HeadAtEvent::Unborn { symbolic },
            (None, Some(oid)) => HeadAtEvent::Detached { oid },
            (None, None) => HeadAtEvent::Unresolvable,
        },
    };

    // As in `walk_history`, treat a ref-store open/list failure as a real error
    // rather than silently returning only the HEAD badge (issue #16).
    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    for reference in all {
        let mut reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while reading refs: {e}");
                continue;
            }
        };
        // Classify by ref category, keeping only branches and tags. The short
        // name (owned now, before we consume the reference) is the badge text:
        // "main", "origin/main", "v1.0.0".
        let (kind, name) = match reference.name().category_and_short_name() {
            Some((Category::LocalBranch, short)) => (RefKind::Branch, short.to_string()),
            Some((Category::RemoteBranch, short)) => {
                let name = short.to_string();
                // Skip the remote's symbolic default-branch pointer
                // (`refs/remotes/<remote>/HEAD`): it just mirrors another branch
                // and isn't a branch tip worth badging.
                if name.ends_with("/HEAD") {
                    continue;
                }
                (RefKind::RemoteBranch, name)
            }
            Some((Category::Tag, short)) => (RefKind::Tag, short.to_string()),
            _ => continue, // HEAD pseudo-ref, notes, worktree-private, …
        };
        // Peel through tag objects to the commit the ref resolves to.
        match reference.peel_to_id() {
            Ok(id) => refs.push(GitRef {
                name,
                kind,
                target: Oid(id.detach().to_string()),
            }),
            Err(e) => {
                eprintln!("git-vista: ref {name:?} won't resolve to a commit ({e}); not badged")
            }
        }
    }

    Ok(RefsAt { refs, head })
}

/// Everything a paged-history snapshot needs, read from **one** opened
/// repository so refs, HEAD, and shallow state are mutually consistent
/// (M1.10, #63). The server derives traversal tips and the `history-v1`
/// generation from these raw materials; nothing here digests or sorts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMaterials {
    /// Display refs exactly as [`read_refs`] produces them: HEAD first (when it
    /// resolves), then branches/remote branches/tags under their short badge
    /// names, each peeled to a commit.
    pub refs: Vec<GitRef>,
    /// The same branch/remote-branch/tag tips under their **full** ref names
    /// (`refs/heads/main`, `refs/remotes/origin/main`, `refs/tags/v1.0`),
    /// captured before display-shortening. The remote symbolic default-branch
    /// pointer (`refs/remotes/<remote>/HEAD`) is skipped, as in [`read_refs`].
    /// Enumeration order; the caller canonicalises.
    pub full_ref_targets: Vec<(String, Oid)>,
    /// HEAD's full symbolic target (`refs/heads/main`); `None` when detached.
    pub head_symbolic_full: Option<String>,
    /// HEAD's display-short branch name (`main`); `None` when detached.
    pub head_branch: Option<String>,
    /// The commit HEAD resolves to; `None` for an unborn HEAD.
    pub resolved_head: Option<Oid>,
    /// The raw `$GIT_DIR/shallow` boundary set: empty when the repository is
    /// not shallow (or the file is empty). Malformed or unreadable shallow
    /// metadata is a hard [`RepoError`] — never silently "not shallow".
    pub shallow: Vec<Oid>,
}

/// Read [`HistoryMaterials`] from a single `gix` open, so the ref tips, both
/// HEAD halves, and the shallow boundary set all describe the same moment.
///
/// Classification matches [`read_refs`] exactly (issue #16's hard error on a
/// ref-store failure included); the only addition is that each kept ref's full
/// name is retained alongside its short badge name, and `$GIT_DIR/shallow` is
/// read through the same repository.
pub fn read_history_materials(path: &Path) -> Result<HistoryMaterials, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    // HEAD: the full symbolic target and short branch name (both `None` when
    // detached), and the commit it resolves to (`None` for an unborn HEAD).
    let head_name = repo
        .head_name()
        .map_err(|e| RepoError::Walk(format!("reading HEAD name: {e}")))?;
    let head_symbolic_full = head_name.as_ref().map(|name| name.as_bstr().to_string());
    let head_branch = head_name.as_ref().map(|name| name.shorten().to_string());
    let resolved_head = repo.head_id().ok().map(|id| Oid(id.detach().to_string()));

    // HEAD first among the display refs, exactly as `read_refs` badges it.
    let mut refs = Vec::new();
    if let Some(target) = &resolved_head {
        refs.push(GitRef {
            name: "HEAD".to_string(),
            kind: RefKind::Head,
            target: target.clone(),
        });
    }

    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    let all = platform
        .all()
        .map_err(|e| RepoError::Walk(format!("listing refs: {e}")))?;
    let mut full_ref_targets = Vec::new();
    for reference in all {
        let mut reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "git-vista: skipping an unreadable ref while reading history materials: {e}"
                );
                continue;
            }
        };
        // Same classification as `read_refs`: branches and tags only, with the
        // remote's symbolic default-branch pointer skipped.
        let (kind, short) = match reference.name().category_and_short_name() {
            Some((Category::LocalBranch, short)) => (RefKind::Branch, short.to_string()),
            Some((Category::RemoteBranch, short)) => {
                let name = short.to_string();
                if name.ends_with("/HEAD") {
                    continue;
                }
                (RefKind::RemoteBranch, name)
            }
            Some((Category::Tag, short)) => (RefKind::Tag, short.to_string()),
            _ => continue, // HEAD pseudo-ref, notes, worktree-private, …
        };
        // The full name is captured here, BEFORE peeling consumes the
        // reference — this is what makes traversal tips independent of the
        // display-shortened badge names.
        let full_name = reference.name().as_bstr().to_string();
        match reference.peel_to_id() {
            Ok(id) => {
                let target = Oid(id.detach().to_string());
                refs.push(GitRef {
                    name: short,
                    kind,
                    target: target.clone(),
                });
                full_ref_targets.push((full_name, target));
            }
            Err(e) => {
                eprintln!(
                    "git-vista: ref {full_name:?} won't resolve to a commit ({e}); \
                     not in history materials"
                )
            }
        }
    }

    // The shallow boundary set, from the same opened repository. `gix` returns
    // `Ok(None)` for a missing or empty `$GIT_DIR/shallow` (not shallow) and a
    // hard error for a malformed line — exactly the propagation the paged
    // history contract requires.
    let shallow = match repo
        .shallow_commits()
        .map_err(|e| RepoError::Walk(format!("reading shallow metadata: {e}")))?
    {
        Some(commits) => commits.iter().map(|id| Oid(id.to_string())).collect(),
        None => Vec::new(),
    };

    Ok(HistoryMaterials {
        refs,
        full_ref_targets,
        head_symbolic_full,
        head_branch,
        resolved_head,
        shallow,
    })
}

/// The short name of the branch currently checked out (HEAD's symbolic referent),
/// e.g. `"main"` or `"feature/ui"`. `None` when HEAD is detached or unreadable.
///
/// Used to colour the graph: the checked-out branch owns its line (and so a branch
/// freshly created from its tip is the one drawn as a new stub, not the trunk).
/// Several branches can sit on the same commit, so the commit alone can't say
/// which is "the" branch — the symbolic HEAD can.
pub fn read_head_branch(path: &Path) -> Option<String> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).ok()?;
    // `head_name()` is `Some` only when HEAD is symbolic (on a branch); `None`
    // when detached. Shorten `refs/heads/feature/ui` to `feature/ui`.
    let name = repo.head_name().ok()??;
    Some(name.shorten().to_string())
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::history::tests::{fixture, git};

    #[test]
    fn read_refs_sees_head_branches_and_tags() {
        let dir = fixture();
        let p = dir.path();
        // Tag the root commit so there's a tag to find.
        git(p, &["tag", "v1.0", "HEAD~2"]);
        let refs = read_refs(p).unwrap();

        let names = |k: RefKind| {
            let mut v: Vec<String> = refs
                .iter()
                .filter(|r| r.kind == k)
                .map(|r| r.name.clone())
                .collect();
            v.sort();
            v
        };

        // HEAD is emitted exactly once, both branches and the tag are seen.
        assert_eq!(names(RefKind::Head), vec!["HEAD"]);
        assert_eq!(names(RefKind::Branch), vec!["feature", "main"]);
        assert_eq!(names(RefKind::Tag), vec!["v1.0"]);

        // On `main`, so HEAD resolves to the same commit as the `main` branch.
        let head = refs.iter().find(|r| r.kind == RefKind::Head).unwrap();
        let main = refs.iter().find(|r| r.name == "main").unwrap();
        assert_eq!(head.target, main.target);
    }

    /// #465: two readers, one repository, opposite answers about whether HEAD
    /// exists. `read_refs_at` badged HEAD from `repo.head()`, which hands back
    /// the raw oid unvalidated; `read_history_materials` badges it from
    /// `repo.head_id()`, which refuses one nothing resolves. So a dangling HEAD
    /// produced a `Head:HEAD` badge from one and nothing from the other — and
    /// a dangling HEAD is exactly the state someone opens this app to
    /// understand.
    ///
    /// Asserted across every state the two readers share, not the broken one
    /// alone: proving the dangling case says the fix works, and proving the
    /// other three says it did not cost anything. (`Unreadable` — a corrupt
    /// `.git/HEAD` — is deliberately not here: `read_history_materials` treats
    /// that as a hard error and `read_refs_at` does not, which is a documented
    /// difference of error policy, not of what HEAD is.)
    ///
    /// MUTATION 1: badge from `repo.head()`/`head.id()` again — red on the
    ///   dangling row, the readers disagree.
    /// MUTATION 2: classify `(None, None)` as `Detached` — red on the recorded
    ///   state, which is where the fact now lives.
    #[test]
    fn the_two_readers_badge_head_identically_in_every_state_they_share() {
        let badges = |refs: &[GitRef]| {
            let mut v: Vec<String> = refs
                .iter()
                .map(|r| format!("{:?}:{}", r.kind, r.name))
                .collect();
            v.sort();
            v
        };

        /// One row: a label, how to put HEAD into that state, and whether
        /// HEAD resolves to a commit there.
        type HeadCase = (&'static str, Box<dyn Fn(&Path)>, bool);

        let cases: Vec<HeadCase> = vec![
            ("on a branch", Box::new(|_: &Path| {}), true),
            (
                "detached at a real commit",
                Box::new(|p: &Path| {
                    git(p, &["checkout", "-q", "--detach", "HEAD"]);
                }),
                true,
            ),
            (
                "unborn",
                Box::new(|p: &Path| {
                    std::fs::write(p.join(".git/HEAD"), "ref: refs/heads/nothing-here\n").unwrap();
                }),
                false,
            ),
            (
                "dangling — a well-formed oid with no object behind it",
                Box::new(|p: &Path| {
                    std::fs::write(p.join(".git/HEAD"), "0".repeat(40) + "\n").unwrap();
                }),
                false,
            ),
        ];

        for (label, put_head_into_state, resolves) in cases {
            let dir = fixture();
            let p = dir.path();
            put_head_into_state(p);

            let at = read_refs_at(p).expect("read_refs_at");
            let materials = read_history_materials(p).expect("read_history_materials");

            assert_eq!(
                badges(&at.refs),
                badges(&materials.refs),
                "{label}: the two readers must not disagree about what refs exist"
            );
            assert_eq!(
                at.refs.iter().any(|r| r.kind == RefKind::Head),
                resolves,
                "{label}: HEAD is badged exactly when it resolves to a commit — \
                 a badge is a claim about a commit, and there is no commit here"
            );
        }
    }

    /// The other half of #465, and the reason dropping the badge is not
    /// laundering: a HEAD that points at nothing is still *recorded* as a fact,
    /// in the one place that can say it. The badge list is display refs
    /// "peeled to the commit it ultimately points at"; `HeadAtEvent` is where a
    /// HEAD with no commit gets to exist.
    #[test]
    fn a_dangling_head_is_still_recorded_even_though_it_is_not_badged() {
        let dir = fixture();
        let p = dir.path();
        let main_tip = read_refs(p)
            .unwrap()
            .into_iter()
            .find(|r| r.name == "main")
            .expect("main is badged")
            .target;
        std::fs::write(p.join(".git/HEAD"), "0".repeat(40) + "\n").unwrap();

        let at = read_refs_at(p).expect("read_refs_at");
        assert_eq!(
            at.head,
            HeadAtEvent::Unresolvable,
            "the state must survive the badge's removal — otherwise the fix \
             discards a real fact instead of relocating it"
        );
        assert_eq!(
            at.refs.iter().find(|r| r.name == "main").map(|r| &r.target),
            Some(&main_tip),
            "a broken HEAD must not cost the branches that read perfectly well"
        );
    }
}
