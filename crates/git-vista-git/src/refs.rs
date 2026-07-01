//! Reading a repository's refs — HEAD, branches and tags — and the short name of
//! the currently checked-out branch, for badging and per-branch colouring.

use std::path::Path;

use gix::refs::Category;

use git_vista_core::model::{GitRef, Oid, RefKind};

use crate::RepoError;

/// Read the repository's refs — HEAD, local & remote branches, and tags — each
/// peeled to the commit it ultimately points at, for badging and per-branch
/// colouring in the UI.
///
/// HEAD is always emitted (as [`RefKind::Head`], named `"HEAD"`) when it resolves
/// to a commit, whether it's on a branch or detached; when it's on a branch the
/// branch is emitted too, so a tip shows both. Refs that don't resolve to a
/// commit (an unborn HEAD, a broken ref) are skipped. Notes and worktree-private
/// refs are ignored.
pub fn read_refs(path: &Path) -> Result<Vec<GitRef>, RepoError> {
    let repo = gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut refs = Vec::new();

    // HEAD first, so it's the leading badge on its commit.
    if let Ok(head) = repo.head() {
        if let Some(id) = head.id() {
            refs.push(GitRef {
                name: "HEAD".to_string(),
                kind: RefKind::Head,
                target: Oid(id.detach().to_string()),
            });
        }
    }

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
            Err(e) => eprintln!("git-vista: ref {name:?} won't resolve to a commit ({e}); not badged"),
        }
    }

    Ok(refs)
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
}
