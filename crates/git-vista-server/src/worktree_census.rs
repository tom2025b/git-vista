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
//! is called from a handler yet; [`worktree_census`] has `#[allow(dead_code)]`
//! outside tests for exactly that reason, the same attribute `conflicts`
//! carries for the same reason.
//!
//! # No new sandbox tier, no new grant
//!
//! `git worktree list --porcelain` reaches git through
//! [`crate::git_cmd::git_output`], declaring `NetworkNeed::Local` — the same
//! arity and the same tier `handlers::read::worktree_status` already uses for
//! `git status --porcelain=v2 --branch`. The argv's first non-flag token is
//! `worktree`, which is absent from `sandbox::REMOTE_SUBCOMMANDS`, so
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
//! # Why the 2.32 floor rules out `-z`
//!
//! `git worktree list --porcelain` has a `-z` form that NUL-terminates
//! records instead of newline-terminates them, which is the safer contract
//! for a path that could contain a literal newline. It is not used here: the
//! git-scm manual for 2.31 (the closest version with its own page; 2.32's
//! page redirects to it, meaning nothing about `worktree list` changed
//! between them) documents `list`, `--porcelain`, and `-v`/`--verbose` and
//! says nothing about `-z` at all, while the current manual documents it —
//! so `-z` post-dates this project's documented git floor
//! (`docs/SUPPORTED_VERSIONS.md`, "Git: 2.32 or later"). Parsing the
//! newline-terminated form inherits git's own limitation at that floor: a
//! worktree path containing a literal newline cannot be parsed unambiguously.
//! That is a fact about the porcelain contract at the supported floor, not a
//! defect introduced by [`parse_worktree_porcelain`].
//!
//! The one place that limitation could bite silently — quoting — does not
//! apply to anything this module keeps. The manual documents that *only* the
//! lock reason is quoted/escaped (`core.quotePath`-style) when it contains
//! unusual characters and `-z` is not used; [`WorktreeSibling`] has no reason
//! field (the spec's struct doesn't carry one, and nothing here needs it), so
//! the parser only ever needs to recognise the `locked`/`prunable` label
//! itself, never interpret the escaping of the text after it.

use std::path::{Path, PathBuf};

use git_vista_core::identity::WorktreeId;
use git_vista_git::{read_repo_facts, RepoFacts};
use git_vista_protocol::{BranchName, CommitOid, Serviceable, WorktreeCensus, WorktreeSibling};

use crate::git_cmd::git_output;

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
    let current = match read_repo_facts(repo) {
        Ok(facts) => facts,
        Err(e) => return fail(format!("couldn't read this repository's own identity: {e}")),
    };

    let output = match git_output(repo, &["worktree", "list", "--porcelain"]).await {
        Ok(o) => o,
        Err(e) => return fail(format!("couldn't run `git worktree list`: {e}")),
    };
    if !output.status.success() {
        return fail(format!(
            "`git worktree list --porcelain` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = match std::str::from_utf8(&output.stdout) {
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

    let mut common_dir_cache: Option<Result<PathBuf, String>> = None;
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
    raw: &RawRecord,
    current: &RepoFacts,
    expose_paths: bool,
    path_is_allowed: &dyn Fn(&Path) -> bool,
    common_dir_cache: &mut Option<Result<PathBuf, String>>,
) -> Result<WorktreeSibling, String> {
    let (worktree_id, repository_id, root_for_fence) = if raw.prunable {
        // `prunable` is git's own flag; whether it means "the directory is
        // really gone" (Serviceable::Missing) or something milder (e.g. an
        // `--expire`-style staleness reason on a directory that still opens)
        // is decided by trying the live path first. Only a failure to open it
        // falls back to admin-directory correlation.
        match read_repo_facts(&raw.path) {
            Ok(facts) => (
                facts.handle.worktree,
                facts.handle.repository,
                Some(facts.root),
            ),
            Err(_) => {
                let common_dir = get_common_dir(repo, common_dir_cache).await?;
                let admin_dir =
                    correlate_missing_admin_dir(&common_dir, &raw.path).ok_or_else(|| {
                        format!(
                            "`{}` is reported prunable but no admin worktree entry under \
                             `{}` names it — can't derive a stable identity for it",
                            raw.path.display(),
                            common_dir.display()
                        )
                    })?;
                let id = WorktreeId::from_git_dir(&canonicalize_lossy(&admin_dir));
                (id, current.handle.repository, None)
            }
        }
    } else {
        match read_repo_facts(&raw.path) {
            Ok(facts) => (
                facts.handle.worktree,
                facts.handle.repository,
                Some(facts.root),
            ),
            Err(e) => {
                return Err(format!(
                    "`git worktree list` reports `{}` as live, but it couldn't be read: {e}",
                    raw.path.display()
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
        .unwrap_or_else(|| display_name(&raw.path));

    let branch = match (&raw.branch_ref, raw.detached, raw.bare) {
        (Some(r), false, false) => {
            let short = r.strip_prefix("refs/heads/").ok_or_else(|| {
                format!(
                    "`{}`'s `branch` line names `{r}`, not a `refs/heads/` ref",
                    raw.path.display()
                )
            })?;
            let name = BranchName::new(short).map_err(|e| {
                format!(
                    "`{}`'s checked-out branch `{short}` doesn't fit this app's \
                     branch-name contract: {e}",
                    raw.path.display()
                )
            })?;
            Some(name)
        }
        (None, _, _) => None,
        (Some(_), true, _) | (Some(_), _, true) => {
            // Ruled out by the parser's own mutual-exclusion check in
            // `RawRecordBuilder::apply_line` — kept as a named error rather
            // than `unreachable!()` so a future change to that check fails
            // loudly here instead of panicking.
            return Err(format!(
                "`{}` names both a branch and detached/bare",
                raw.path.display()
            ));
        }
    };

    let head = match &raw.head_hex {
        None => None,
        Some(hex) if is_null_oid(hex) => None,
        Some(hex) => Some(CommitOid::new(hex.clone()).map_err(|e| {
            format!(
                "`{}`'s `HEAD` line (`{hex}`) isn't a commit id this app accepts: {e}",
                raw.path.display()
            )
        })?),
    };

    Ok(WorktreeSibling {
        repository: repository_id.to_string(),
        id: worktree_id.to_string(),
        name,
        path: expose_paths.then(|| raw.path.display().to_string()),
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
async fn get_common_dir(
    repo: &Path,
    cache: &mut Option<Result<PathBuf, String>>,
) -> Result<PathBuf, String> {
    if cache.is_none() {
        *cache = Some(common_dir(repo).await);
    }
    cache.clone().expect("just set")
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

// ---------------------------------------------------------------------------
// Parsing `git worktree list --porcelain` (no `-z` — see module doc)
// ---------------------------------------------------------------------------

/// One fully-parsed `git worktree list --porcelain` record, before identity
/// resolution. Every field here is exactly what git printed — no filesystem
/// access, no fence check.
#[derive(Debug)]
struct RawRecord {
    path: PathBuf,
    head_hex: Option<String>,
    branch_ref: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
}

/// Parse the complete stdout of `git worktree list --porcelain`.
///
/// Strict by design (the brief's own rule, and this codebase's established
/// posture for a fact that must never be silently dropped —
/// `RecoveryClass::CheckFailed` on an unrecognised ref shape,
/// `HeadState::Unresolvable`): every line must be either the start of a
/// record (`worktree <path>`) or a recognised attribute of the
/// currently-open record. Anything else — an attribute before any `worktree`
/// line, a second `worktree` line before the first record's blank-line
/// terminator, an unrecognised label, a value-shape git could never actually
/// produce — is a hard error, not a skipped line. A dropped worktree is
/// indistinguishable from one that never existed; a census that claims
/// completeness may not do that silently.
fn parse_worktree_porcelain(text: &str) -> Result<Vec<RawRecord>, String> {
    let mut records = Vec::new();
    let mut current: Option<RawRecordBuilder> = None;

    for line in text.split('\n') {
        if line.is_empty() {
            if let Some(builder) = current.take() {
                records.push(builder.finish()?);
            }
            // A blank line with no record open carries no data to lose — the
            // leading/trailing artifact of `str::split('\n')` on git's own
            // (also blank-line-terminated) stream. Tolerated rather than
            // treated as "an unrecognised line", since there is nothing here
            // that could be silently dropped.
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if current.is_some() {
                return Err(format!(
                    "a new `worktree` line (`{line}`) appeared before the previous \
                     record's blank-line terminator"
                ));
            }
            if rest.is_empty() {
                return Err("a `worktree` line named an empty path".to_string());
            }
            current = Some(RawRecordBuilder::new(PathBuf::from(rest)));
        } else {
            let builder = current
                .as_mut()
                .ok_or_else(|| format!("line `{line}` appeared before any `worktree` line"))?;
            builder.apply_line(line)?;
        }
    }
    if let Some(builder) = current.take() {
        records.push(builder.finish()?);
    }
    Ok(records)
}

#[derive(Default)]
struct RawRecordBuilder {
    path: PathBuf,
    head_hex: Option<String>,
    branch_ref: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
}

impl RawRecordBuilder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    /// Whether a `branch`/`detached`/`bare` line has already been set — those
    /// three are mutually exclusive within one record.
    fn head_shape_taken(&self) -> bool {
        self.branch_ref.is_some() || self.detached || self.bare
    }

    fn apply_line(&mut self, line: &str) -> Result<(), String> {
        let (label, rest) = match line.split_once(' ') {
            Some((l, r)) => (l, Some(r)),
            None => (line, None),
        };
        match label {
            "HEAD" => {
                let value = rest.ok_or_else(|| "`HEAD` line has no value".to_string())?;
                if self.head_hex.is_some() {
                    return Err(format!(
                        "`{}` has more than one `HEAD` line",
                        self.path.display()
                    ));
                }
                self.head_hex = Some(value.to_string());
            }
            "branch" => {
                let value = rest.ok_or_else(|| "`branch` line has no value".to_string())?;
                if self.head_shape_taken() {
                    return Err(format!(
                        "`{}`'s `branch` line conflicts with an earlier \
                         `branch`/`detached`/`bare` line",
                        self.path.display()
                    ));
                }
                self.branch_ref = Some(value.to_string());
            }
            "detached" => {
                if rest.is_some() {
                    return Err("`detached` takes no value".to_string());
                }
                if self.head_shape_taken() {
                    return Err(format!(
                        "`{}`'s `detached` line conflicts with an earlier \
                         `branch`/`detached`/`bare` line",
                        self.path.display()
                    ));
                }
                self.detached = true;
            }
            "bare" => {
                if rest.is_some() {
                    return Err("`bare` takes no value".to_string());
                }
                if self.head_shape_taken() {
                    return Err(format!(
                        "`{}`'s `bare` line conflicts with an earlier \
                         `branch`/`detached`/`bare` line",
                        self.path.display()
                    ));
                }
                self.bare = true;
            }
            "locked" => {
                if self.locked {
                    return Err(format!(
                        "`{}` has more than one `locked` line",
                        self.path.display()
                    ));
                }
                self.locked = true;
                // The reason (`rest`) is discarded on purpose — `WorktreeSibling`
                // carries no reason field (see the protocol module's doc), so
                // there is nothing here that needs the `-z`/quoting distinction
                // the manual documents for that text.
            }
            "prunable" => {
                if self.prunable {
                    return Err(format!(
                        "`{}` has more than one `prunable` line",
                        self.path.display()
                    ));
                }
                self.prunable = true;
            }
            other => return Err(format!("unrecognised worktree-list attribute `{other}`")),
        }
        Ok(())
    }

    fn finish(self) -> Result<RawRecord, String> {
        if self.bare {
            if self.head_hex.is_some() {
                return Err(format!(
                    "`{}` is `bare` but also carries a `HEAD` line",
                    self.path.display()
                ));
            }
        } else if self.head_hex.is_none() {
            return Err(format!(
                "`{}` has no `HEAD` line and is not `bare`",
                self.path.display()
            ));
        } else if !self.detached && self.branch_ref.is_none() {
            return Err(format!(
                "`{}` names neither a `branch` nor `detached`",
                self.path.display()
            ));
        }
        Ok(RawRecord {
            path: self.path,
            head_hex: self.head_hex,
            branch_ref: self.branch_ref,
            detached: self.detached,
            bare: self.bare,
            locked: self.locked,
            prunable: self.prunable,
        })
    }
}

#[cfg(test)]
mod tests;
