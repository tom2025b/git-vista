//! The worktree census (M11.01, #546): read `git worktree list --porcelain`
//! for the served repository and resolve it into
//! [`git_vista_protocol::WorktreeCensus`].
//!
//! `docs/superpowers/specs/m3.23-worktrees.md` §1 designs this. It is
//! deliberately staged the way `conflicts.rs` (M4.31, #84) and the tag
//! contract (M2.21a, #235) were staged before it: the read primitive and its
//! wire type land and are reviewed first, with no route yet exposing them —
//! this issue is scoped to the query, not the UI (M11.03) or the
//! checkout-collision precondition the spec's §2 designs next. Nothing here
//! is called from a handler yet; [`worktree_census`] itself carries
//! `#[allow(dead_code)]` outside tests for exactly that reason — on the
//! function only, not the `mod worktree_census;` declaration in `main.rs`
//! (see that declaration's own comment for why: unlike `conflicts`, which
//! has no caller anywhere in it, everything below `worktree_census` is
//! reached from within this module and should stay eligible for the
//! dead-code lint).
//!
//! # No new sandbox tier, no new grant
//!
//! `git worktree list --porcelain` reaches git through
//! [`crate::git_cmd::git_stdout_capped`], which declares `NetworkNeed::Local`
//! — the same helper and the same tier
//! `handlers::read::worktree_status_v2_for_repo` already uses for
//! `git status --porcelain=v2 --branch -z`, including its posture on a cap
//! hit (refuse, never parse a truncated stream). The argv's first non-flag
//! token is `worktree`, which is absent from `sandbox::REMOTE_SUBCOMMANDS`, so
//! `sandbox::reconcile_need` agrees with `Local` and the existing repo-scoped
//! grant `sandbox::policy_for` already computed for every other read on this
//! repository covers it. No new argv shape, no new grant path.
//!
//! Resolving a **sibling's** identity is a second, unrelated question, and it
//! runs entirely outside the sandbox: [`git_vista_git::read_repo_facts`] opens
//! a candidate path directly via `gix`, on the host process, with no
//! subprocess and no bwrap. `Catalog::register` already does exactly this —
//! `read_repo_facts(path)` runs **before** the allowed-roots check
//! (`catalog.rs`'s `register`) — which is the precedent this module leans on:
//! computing a path's identity has never been the security-sensitive step,
//! only *serving* it (executing git inside it, or admitting it to the
//! catalog) is. So resolving a sibling `Serviceable::OutsideAllowedRoots`
//! costs nothing new either.
//!
//! # Why the allowed-roots check and the path-exposure flag are parameters
//!
//! [`worktree_census`] takes `expose_paths`/`path_is_allowed` rather than
//! calling `crate::state::expose_paths()`/`crate::state::path_is_allowed`
//! itself — the same hoist `Catalog::descriptor`/`descriptor_with_policy`
//! already make for exactly this reason (see that function's own doc
//! comment): the process-global catalog is a `OnceLock`, shared by every test
//! in this binary, so a function that reaches into it directly cannot be unit
//! tested without either polluting or depending on what other tests already
//! registered. A production call site supplies
//! `crate::state::expose_paths()`/`&crate::state::path_is_allowed` once this
//! is wired to a handler; every test here supplies its own, so an
//! "outside the allowed roots" test needs no dependency on what any other
//! test happened to register first.
//!
//! # The pure parse lives in `git-vista-protocol`, not here
//!
//! [`git_vista_protocol::parse_worktree_porcelain`] turns git's stdout into
//! [`git_vista_protocol::WorktreeListRecord`]s; this module keeps only the
//! two halves that need the machine — the **spawn** above and the
//! **enrichment** below (identity resolution via `gix`, the allowed-roots
//! fence, and the `HEAD 000…0` sentinel that must not become a `CommitOid`).
//! That is the split `handlers::read` already uses for status: it calls
//! `git_vista_protocol::parse_porcelain_v2_z` rather than owning a porcelain
//! parser of its own. The protocol module's own doc carries the porcelain
//! contract itself — in particular why the 2.32 git floor rules `-z` out, and
//! why the newline-terminated form's one ambiguity (a path containing a
//! literal newline) is git's limitation at that floor rather than a defect in
//! the parser.

use std::path::{Path, PathBuf};

use git_vista_core::identity::WorktreeId;
use git_vista_git::{read_repo_facts, RepoFacts};
use git_vista_protocol::{
    parse_worktree_porcelain, BranchName, CommitOid, Serviceable, WorktreeCensus,
    WorktreeListRecord, WorktreeSibling,
};

use crate::git_cmd::{git_output, git_stdout_capped};

/// The label `git_stdout_capped` logs a failure under. Not a route — nothing
/// exposes this census yet (M11.03's job) — so it names the read itself, in
/// the same `/api/…`-shaped slot every other call site fills.
const WORKTREE_LIST_ENDPOINT: &str = "worktree census";

/// Upper bound on `git worktree list --porcelain`'s stdout. 8 MiB: the same
/// ceiling `handlers::read`'s `STATUS_V2_STDOUT_CAP` uses (itself
/// `git_cmd::DEFAULT_GIT_STDOUT_CAP`'s fail-safe value), and for the same
/// reason — it is far past any real repository's worktree list, so a hit means
/// something has gone wrong rather than that a legitimate read was clipped. A
/// hit is a `CensusFailed`, never a best-effort parse; see the call site.
const WORKTREE_LIST_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// Read and resolve the worktree census for the repository at `repo` (the
/// worktree currently being served).
///
/// `expose_paths`/`path_is_allowed` are the production call site's
/// `crate::state::expose_paths()`/`&crate::state::path_is_allowed` — see the
/// module doc for why they arrive as parameters rather than being read here.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn worktree_census(
    repo: &Path,
    expose_paths: bool,
    path_is_allowed: &dyn Fn(&Path) -> bool,
) -> WorktreeCensus {
    worktree_census_capped(
        repo,
        expose_paths,
        path_is_allowed,
        WORKTREE_LIST_STDOUT_CAP,
    )
    .await
}

/// [`worktree_census`] with the stdout ceiling passed in rather than baked in
/// — the same split, for the same reason, that
/// `handlers::read::worktree_status_v2_for_repo` makes for
/// `STATUS_V2_STDOUT_CAP`: the refusal-on-truncation branch is only reachable
/// from a test if the test can pick a cap small enough to hit, and
/// constructing 8 MiB of real `git worktree list` output (tens of thousands of
/// linked worktrees) to exercise it is not a test anyone would run.
async fn worktree_census_capped(
    repo: &Path,
    expose_paths: bool,
    path_is_allowed: &dyn Fn(&Path) -> bool,
    stdout_cap: usize,
) -> WorktreeCensus {
    let current = match read_repo_facts(repo) {
        Ok(facts) => facts,
        Err(e) => return fail(format!("couldn't read this repository's own identity: {e}")),
    };

    let args = [
        "worktree".to_string(),
        "list".to_string(),
        "--porcelain".to_string(),
    ];
    // `git_stdout_capped`, not `git_output`: this stdout grows with the number
    // of worktrees, which is client-influenced (anyone who can `git worktree
    // add` in the served repository adds a record) and unbounded by anything
    // this process controls. `git_output` reads to EOF with no ceiling at all.
    // A non-zero exit is already folded into the `Err` arm here, so the
    // separate `status.success()` check the uncapped form needed is gone.
    let (stdout, truncated) =
        match git_stdout_capped(repo, &args, WORKTREE_LIST_ENDPOINT, stdout_cap).await {
            Ok(pair) => pair,
            Err((_status, message)) => {
                return fail(format!("`git worktree list --porcelain` failed: {message}"))
            }
        };
    if truncated {
        // Refused, never parsed — the same call `worktree_status_v2_for_repo`
        // makes for `git status --porcelain=v2`, and for the same reason: a cut
        // stream can end mid-record, and the parser cannot tell that from a
        // record that genuinely ended there. Parsing the prefix would drop
        // whole worktrees from a census that claims to be complete, which is
        // precisely the silent omission `CensusFailed` exists to prevent (spec
        // §1, "the enumeration ITSELF is fallible"). A `CensusFailed` says
        // "nothing was established"; a short `Observed` would lie.
        return fail(format!(
            "`git worktree list --porcelain` printed more than {stdout_cap} bytes; \
             a truncated stream is refused, never parsed"
        ));
    }
    let text = match std::str::from_utf8(&stdout) {
        Ok(s) => s,
        Err(_) => {
            return fail("`git worktree list --porcelain` did not print valid UTF-8".to_string())
        }
    };

    let raw_records = match parse_worktree_porcelain(text) {
        Ok(r) => r,
        Err(reason) => return fail(reason),
    };
    if raw_records.is_empty() {
        return fail(
            "`git worktree list --porcelain` reported no worktrees at all — \
             every repository has at least its own"
                .to_string(),
        );
    }

    let mut common_dir_cache: Option<PathBuf> = None;
    let mut siblings = Vec::with_capacity(raw_records.len());
    let mut current_count = 0usize;

    for raw in &raw_records {
        let sibling = match resolve_sibling(
            repo,
            raw,
            &current,
            expose_paths,
            path_is_allowed,
            &mut common_dir_cache,
        )
        .await
        {
            Ok(s) => s,
            Err(reason) => return fail(reason),
        };
        if sibling.is_current {
            current_count += 1;
        }
        siblings.push(sibling);
    }

    if current_count != 1 {
        return fail(format!(
            "the census resolved {current_count} entries as the currently served \
             worktree; exactly one is required (repository root: {})",
            current.root.display()
        ));
    }

    WorktreeCensus::Observed { siblings }
}

fn fail(reason: String) -> WorktreeCensus {
    WorktreeCensus::CensusFailed { reason }
}

// ---------------------------------------------------------------------------
// Resolving one raw record into a WorktreeSibling
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn resolve_sibling(
    repo: &Path,
    raw: &WorktreeListRecord,
    current: &RepoFacts,
    expose_paths: bool,
    path_is_allowed: &dyn Fn(&Path) -> bool,
    common_dir_cache: &mut Option<PathBuf>,
) -> Result<WorktreeSibling, String> {
    // `raw.path` is the string git printed; the parser (now in
    // `git-vista-protocol`, which touches no filesystem) deliberately leaves it
    // a `String`. This is the one place it becomes a path.
    let raw_path = Path::new(raw.path.as_str());

    let (worktree_id, repository_id, root_for_fence) = if raw.prunable {
        // `prunable` is git's own flag; whether it means "the directory is
        // really gone" (Serviceable::Missing) or something milder (e.g. an
        // `--expire`-style staleness reason on a directory that still opens)
        // is decided by trying the live path first. Only a failure to open it
        // falls back to admin-directory correlation.
        match read_repo_facts(raw_path) {
            Ok(facts) => (
                facts.handle.worktree,
                facts.handle.repository,
                Some(facts.root),
            ),
            Err(_) => {
                let common_dir = get_common_dir(repo, common_dir_cache).await?;
                let admin_dir =
                    correlate_missing_admin_dir(&common_dir, raw_path).ok_or_else(|| {
                        format!(
                            "`{}` is reported prunable but no admin worktree entry under \
                             `{}` names it — can't derive a stable identity for it",
                            raw.path,
                            common_dir.display()
                        )
                    })?;
                let id = WorktreeId::from_git_dir(&canonicalize_lossy(&admin_dir));
                (id, current.handle.repository, None)
            }
        }
    } else {
        match read_repo_facts(raw_path) {
            Ok(facts) => (
                facts.handle.worktree,
                facts.handle.repository,
                Some(facts.root),
            ),
            Err(e) => {
                return Err(format!(
                    "`git worktree list` reports `{}` as live, but it couldn't be read: {e}",
                    raw.path
                ))
            }
        }
    };

    let serviceable = match &root_for_fence {
        None => Serviceable::Missing,
        Some(root) => {
            if path_is_allowed(root) {
                Serviceable::Yes
            } else {
                Serviceable::OutsideAllowedRoots
            }
        }
    };

    let is_current = worktree_id == current.handle.worktree;

    let name = root_for_fence
        .as_deref()
        .map(display_name)
        .unwrap_or_else(|| display_name(raw_path));

    let branch = match (&raw.branch_ref, raw.detached, raw.bare) {
        (Some(r), false, false) => {
            let short = r.strip_prefix("refs/heads/").ok_or_else(|| {
                format!(
                    "`{}`'s `branch` line names `{r}`, not a `refs/heads/` ref",
                    raw.path
                )
            })?;
            let name = BranchName::new(short).map_err(|e| {
                format!(
                    "`{}`'s checked-out branch `{short}` doesn't fit this app's \
                     branch-name contract: {e}",
                    raw.path
                )
            })?;
            Some(name)
        }
        (None, _, _) => None,
        (Some(_), true, _) | (Some(_), _, true) => {
            // Ruled out by the parser's own mutual-exclusion check in
            // `git_vista_protocol::parse_worktree_porcelain` — kept as a
            // named error rather
            // than `unreachable!()` so a future change to that check fails
            // loudly here instead of panicking.
            return Err(format!(
                "`{}` names both a branch and detached/bare",
                raw.path
            ));
        }
    };

    let head = match &raw.head_hex {
        None => None,
        Some(hex) if is_null_oid(hex) => None,
        Some(hex) => Some(CommitOid::new(hex.clone()).map_err(|e| {
            format!(
                "`{}`'s `HEAD` line (`{hex}`) isn't a commit id this app accepts: {e}",
                raw.path
            )
        })?),
    };

    Ok(WorktreeSibling {
        repository: repository_id.to_string(),
        id: worktree_id.to_string(),
        name,
        path: expose_paths.then(|| raw.path.clone()),
        branch,
        head,
        is_current,
        locked: raw.locked,
        prunable: raw.prunable,
        bare: raw.bare,
        serviceable,
    })
}

/// `40`/`64` lowercase zeros — git's null-oid sentinel for "no commit here",
/// printed by `git worktree list --porcelain` for an unborn branch's `HEAD`
/// line. See the module doc's `head`/`branch` section.
fn is_null_oid(hex: &str) -> bool {
    (hex.len() == 40 || hex.len() == 64) && hex.bytes().all(|b| b == b'0')
}

/// The base-name display label `WorktreeSibling::name` carries — the same
/// derivation `read_repo_facts` uses for `RepoFacts::name`, reapplied here for
/// the [`Serviceable::Missing`] case where there is no live `RepoFacts` to
/// borrow it from.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Canonicalise, falling back to the original path on failure — the same
/// best-effort posture `git-vista-git`'s own (private, so not reusable here)
/// `canonical_dir` takes, reproduced because `WorktreeId::from_git_dir`'s
/// contract ("the caller is responsible for canonicalising first") is the
/// same regardless of which crate calls it.
fn canonicalize_lossy(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// The Missing-row identity mapping (spec §1, "Missing siblings need their id
// derived differently")
// ---------------------------------------------------------------------------

/// `git rev-parse --git-common-dir` for `repo`, resolved to an absolute,
/// best-effort-canonical path. Only called for a repository that lazily turns
/// out to hold a `prunable`-and-unreadable sibling — most censuses never
/// spawn this.
///
/// Only a **successful** read is cached. A failure here is propagated by the
/// caller's `?` straight to `WorktreeCensus::CensusFailed`, which ends the
/// whole census immediately — so a cached failure could never be read back
/// by a second `prunable` row in the same call. Caching it anyway would be
/// dead machinery for a path that cannot execute; simpler to just re-run
/// `common_dir` on the (census-ending) failure case; it happens at most once.
async fn get_common_dir(repo: &Path, cache: &mut Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = cache {
        return Ok(dir.clone());
    }
    let dir = common_dir(repo).await?;
    *cache = Some(dir.clone());
    Ok(dir)
}

async fn common_dir(repo: &Path) -> Result<PathBuf, String> {
    let output = git_output(repo, &["rev-parse", "--git-common-dir"])
        .await
        .map_err(|e| format!("couldn't run `git rev-parse --git-common-dir`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git rev-parse --git-common-dir` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Err("`git rev-parse --git-common-dir` printed nothing".to_string());
    }
    let joined = repo.join(raw);
    Ok(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// Resolve the admin `<common-dir>/worktrees/<name>` directory for a sibling
/// git reports `prunable` and whose own directory could not be opened. That
/// admin directory survives deletion of the working tree — it is how git can
/// still list the entry at all — so it, not the vanished working directory,
/// is what `WorktreeId::from_git_dir` hashes for a [`Serviceable::Missing`]
/// row (spec §1, option 1: "each porcelain record maps to the common
/// repository's `worktrees/<name>` administrative directory… hash *that* for
/// the existing `WorktreeId`").
///
/// The correlation is exact, not a naming guess: each admin directory's own
/// `gitdir` file records the *working tree's* `.git` pointer-file path
/// (verified by hand: `cat .git/worktrees/<name>/gitdir` prints
/// `<worktree-path>/.git`), so matching that file's content against the
/// porcelain-reported path is git's own bookkeeping, not an assumption about
/// directory-naming conventions (which the spec's own admission — porcelain
/// is the stable contract, human formats are not — argues against relying
/// on).
///
/// `None` when no admin directory names this path, or when more than one
/// does (ambiguous — refuses to guess rather than picking one).
fn correlate_missing_admin_dir(common_dir: &Path, sibling_path: &Path) -> Option<PathBuf> {
    let admin_root = common_dir.join("worktrees");
    let entries = std::fs::read_dir(&admin_root).ok()?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let admin_dir = entry.path();
        let Ok(recorded) = std::fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let recorded = recorded.trim();
        let Some(recorded_worktree) = recorded.strip_suffix("/.git") else {
            continue;
        };
        if Path::new(recorded_worktree) == sibling_path {
            if found.is_some() {
                return None; // ambiguous: two admin dirs claim the same path
            }
            found = Some(admin_dir);
        }
    }
    found
}

#[cfg(test)]
mod tests;
