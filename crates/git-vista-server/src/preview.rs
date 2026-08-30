//! M10.08 (#576): draw the repository as it *would* be after a [`Plan`],
//! writing nothing to it.
//!
//! The whole feature is one idea, and it is a measurement rather than a claim:
//! **a throwaway bare object store whose `objects/info/alternates` names the
//! served repository's own object directory can read every object that
//! repository has, and writes only into itself.** So `merge-tree --write-tree`
//! computes the real three-way merge against the real objects, `commit-tree`
//! writes the hypothetical commit *into the scratch store*, and the served
//! repository never learns either happened.
//!
//! Re-measured on this host on 2026-08-30, in a throwaway repository, with the
//! exact argv composition this file uses (`git -C <real repo>
//! --git-dir=<scratch> …`, which is what [`crate::git_cmd::git_output`] execs
//! once it prepends its own `-C`):
//!
//! ```text
//! objects under <commondir>/objects before : 9
//! merge-tree -z --write-tree --merge-base=…: rc=1 (a real conflict)
//! objects under <commondir>/objects after  : 9      <- unchanged
//! objects under <scratch>/objects          : 3
//! commit-tree <tree> -p <head>             : rc=0, commit 6b205017…
//! objects under <commondir>/objects after  : 9      <- still unchanged
//! scratch store: cat-file -t 6b205017…     -> commit
//! real repository: cat-file -t 6b205017…   -> fatal: could not get object info
//! real repository: show-ref                -> byte-identical before and after
//! ```
//!
//! # What this module refuses to do
//!
//! It never models git. The failure mode of a modelled git is not being wrong,
//! it is being *confidently* wrong — a plausible graph that quietly differs
//! from what the command will actually do, on exactly the operations a user
//! cannot check by eye. Every place this file cannot establish an answer it
//! says so in the type ([`PreviewUnavailable`]) rather than guessing, and the
//! `_` arm of [`previewable`] means a `GitOperation` variant added later is
//! *invisible* here rather than wrong.
//!
//! That posture, and the exit-code classification below, are lifted verbatim
//! from [`crate::activity::revert_would_conflict`], which has run
//! `merge-tree --write-tree` on a live served repository since #327. Its doc
//! comment is the house standard: "The answer is git's own exit code, not a
//! text heuristic … `Err` means the check itself did not produce an answer …
//! 'couldn't tell' must never read as 'yes'."
//!
//! # Where the work is split
//!
//! This file does the *impure* half only: run git, read what git said. Laying
//! the two graphs out and deriving what changed is
//! [`git_vista_core::preview::lay_out_preview`], which takes commit lists and
//! refs and never sees a repository. Nothing here re-implements a lane.
//!
//! # No `Command` is constructed here
//!
//! Every spawn goes through [`crate::git_cmd::git_output`], the sealed sandbox
//! launcher, so `crate::argv_boundary`'s source scan needs no new entry and its
//! allowlist is untouched. That is deliberate and load-bearing: reaching for
//! `ALLOWED_SPAWN_SITES` is how a production spawn gets pre-authorised by a
//! comment written about a test.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use git_vista_core::model::{BranchStub, CommitSummary, Edge, GitRef, Graph, GraphRow, Oid};
use git_vista_core::preview::{lay_out_preview, PreviewChange, PreviewInput, PreviewLayout};
use git_vista_protocol::preview::{PreviewGraph, PreviewOutcome, PreviewUnavailable};
use git_vista_protocol::{GitOperation, Plan};

use crate::git_cmd;

/// This crate's concrete alias, exactly as `handlers::read` aliases
/// `HistoryPage<GraphRow, Edge, FrameStub>` into [`crate::handlers::read::Page`]
/// — a server-private alias, never imported across a crate boundary.
pub(crate) type PreviewResponse = PreviewOutcome<GraphRow, Edge, BranchStub, PreviewChange>;

/// `merge-tree --write-tree`'s own floor.
///
/// Deliberately **not** the product floor in `docs/SUPPORTED_VERSIONS.md`
/// (2.32, which CI builds and exercises for real — #365, ADR 0082). This is
/// one feature's requirement, which is exactly why it degrades to
/// [`PreviewUnavailable::GitTooOld`] here instead of refusing at boot: a host
/// on 2.32–2.37 is a fully supported host on which everything else works.
pub(crate) const MIN_GIT_FOR_PREVIEW: (u32, u32) = (2, 38);

/// How much history the preview lays out. A preview is a picture of the tip,
/// not a paged view: both halves come from one capped walk so the before/after
/// lane comparison is between two layouts of the same window.
pub(crate) const PREVIEW_HISTORY_LIMIT: usize = 500;

/// The scratch store's directory-name prefix.
///
/// It is a **named** prefix rather than `tempfile`'s default `.tmpXXXXXX` for
/// one reason: [`ScratchStore::sweep_stale`] only ever deletes directories it
/// can prove this module created, and it cannot prove that about a name it did
/// not choose. A sweep matching a prefix nothing produces is inert — it would
/// never delete anything and a test that hand-created a stale directory would
/// pass anyway, which is the shape of green-but-proves-nothing this repository
/// has paid for repeatedly.
const SCRATCH_PREFIX: &str = "gv-preview-";

/// How old a `gv-preview-*` sibling must be before the sweep removes it.
///
/// [`tempfile::TempDir`] removes on drop — the return, the `?` and the panic —
/// but not on `SIGKILL` or a power loss, so a stale store can survive inside
/// the user's `.git`. An hour is comfortably longer than any preview and short
/// enough that a crashed server's leftovers do not accumulate. The bound
/// matters: without it, a *concurrent* preview's store would be a candidate.
const STALE_SCRATCH_AGE: Duration = Duration::from_secs(60 * 60);

/// The git version, probed once per process.
///
/// Per process, not per call and not at boot. Not per call because the git
/// binary a process execs is a property of that process's `PATH`, not of the
/// repository or the request. Not at boot because `sandbox::probe`'s gate has
/// exactly one non-fatal outcome by design ("There is no degrade: a verdict
/// other than `Contained` means no server, full stop (ADR 0029)") and putting
/// a *capability* question into a fatal gate is how a degrade gets bolted onto
/// a gate whose whole argument is that it has none.
///
/// The honest limit, stated rather than hidden: an operator who upgrades git
/// under a running server does not get this feature until restart. That is the
/// same posture `sandbox::capabilities::current()` already takes toward host
/// capability.
///
/// Only a *success* is cached ([`tokio::sync::OnceCell::get_or_try_init`]), so
/// a transient failure to run git does not permanently disable the feature.
static GIT_VERSION: tokio::sync::OnceCell<(u32, u32, u32)> = tokio::sync::OnceCell::const_new();

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Draw the repository as it *would* be after `plan`, writing nothing to it.
///
/// Returns an answer in every case; there is no `Result`, because "I could not
/// tell" is one of the four things this function is allowed to say and it says
/// it in the type ([`PreviewUnavailable`]) rather than through an error a
/// caller might flatten into one of the other three.
///
/// Checks run in a **pinned order**, so a repository that trips two conditions
/// reports deterministically:
///
/// 1. [`PreviewOutcome::Unsupported`] — pure, from `plan.operation` alone, no
///    IO. First because it is the permanent answer: telling someone to reopen
///    in Active mode does not help an operation that can never be previewed.
/// 2. [`PreviewUnavailable::RepositoryReadOnly`] — one catalog lookup, still no
///    spawn.
/// 3. [`PreviewUnavailable::GitTooOld`] — the cached version probe.
/// 4. The git work.
///
/// # Spawn count, so nobody has to discover it by profiling
///
/// Seven `git` spawns for a revert or a cherry-pick, eight for a merge, plus
/// one more on the first call of the process (`--version`). Each goes through
/// bwrap and the shim. That is fine for a user-initiated preview and is *not*
/// fine per keystroke or per row; a surface that wants it live needs its own
/// caching decision. See ADR 0099.
pub(crate) async fn preview(repo: &Path, plan: &Plan) -> PreviewResponse {
    match compute(repo, plan).await {
        Ok(outcome) => outcome,
        Err(reason) => PreviewOutcome::Unavailable { reason },
    }
}

/// [`preview`]'s body, with the `Unavailable` arm expressed as `Err` so `?`
/// can carry it. Every `Err` here becomes `Unavailable`; the other three arms
/// are `Ok`.
async fn compute(repo: &Path, plan: &Plan) -> Result<PreviewResponse, PreviewUnavailable> {
    // 1. Unsupported — pure, no IO.
    let Some(op) = previewable(&plan.operation) else {
        return Ok(PreviewOutcome::Unsupported {
            operation: operation_name(&plan.operation),
        });
    };

    // 2. Read-only. Read from `state::read_only_for_path`, the *same* function
    //    `git_cmd::sandboxed` derives `read_only` from before calling
    //    `sandbox::policy_for`. One source of truth means this refusal and the
    //    Landlock grant can never disagree — a second source could refuse a
    //    repository the policy would have granted, or the reverse.
    if crate::state::read_only_for_path(repo) {
        return Err(PreviewUnavailable::RepositoryReadOnly);
    }

    // 3. The version gate.
    if let Some(too_old) = version_gate(git_version(repo).await?) {
        return Err(too_old);
    }

    // 4. The git work.
    let head = git_cmd::rev_parse(repo, "HEAD")
        .await
        .map_err(|e| check_failed(format!("resolving HEAD: {e}")))?
        .ok_or_else(|| check_failed("HEAD does not resolve to a commit"))?;

    let recipe = match resolve_plumbing(repo, &op, &head).await? {
        Plumbing::Unsupported(what) => return Ok(PreviewOutcome::Unsupported { operation: what }),
        // A fast-forward creates no commit: only refs move. `added: None` is
        // exactly what `PreviewInput` documents for this case.
        Plumbing::FastForward { to } => return lay_out(repo, None, ref_moves_to(repo, &to)),
        // Already up to date: nothing is added and no ref moves. An empty
        // `changes` here is the claim, not an absence.
        Plumbing::AlreadyUpToDate => return lay_out(repo, None, Vec::new()),
        Plumbing::Synthesize(recipe) => recipe,
    };

    let tree = match merge_tree(repo, &recipe).await? {
        MergeTreeAnswer::Conflict { paths } => return Ok(PreviewOutcome::Conflict { paths }),
        MergeTreeAnswer::Clean { tree } => tree,
    };

    let parents: Vec<&str> = recipe.parents.iter().map(String::as_str).collect();
    let oid = commit_tree(repo, &recipe.store, &tree, &parents, &recipe.message).await?;
    let added = read_back(repo, &recipe.store, &oid).await?;
    lay_out(repo, Some(added), ref_moves_to(repo, &oid))
}

/// Shorthand for the `CheckFailed` arm — "a git step ran and did not produce
/// an answer". Never "no".
fn check_failed(detail: impl Into<String>) -> PreviewUnavailable {
    PreviewUnavailable::CheckFailed {
        detail: detail.into(),
    }
}

/// Shorthand for the `ScratchStore` arm — the store could not be created,
/// seeded or read. Distinct from `CheckFailed` on purpose: one says the
/// computation failed, the other says it never had anywhere to happen.
fn scratch_failed(detail: impl Into<String>) -> PreviewUnavailable {
    PreviewUnavailable::ScratchStore {
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------
// Operation → plumbing
// ---------------------------------------------------------------------------

/// The three operations this slice can express, with their arguments already
/// pulled out of the newtypes.
///
/// A dedicated enum rather than passing `&GitOperation` around so that the
/// `_ => None` default arm of [`previewable`] is the **only** place in this
/// file that matches on `GitOperation`. A variant added to the protocol later
/// cannot therefore be half-handled: it is invisible everywhere or nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Previewable {
    Revert { commit: String },
    CherryPick { commit: String },
    Merge { branch: String },
}

/// Map one [`GitOperation`] onto the plumbing shape this file understands, or
/// `None` for [`PreviewOutcome::Unsupported`].
///
/// **`_ => None` is the design, not a gap** (spec §4.3). Rebase, reset and
/// force-push are `Unsupported` because expressing them needs far more
/// plumbing; `CherryPickMerge` and `RevertMerge` are `Unsupported` because
/// they take one more input (`mainline`, which picks the base) and A1 names
/// three operations — a fourth arriving untested is precisely what the default
/// arm exists to prevent.
fn previewable(op: &GitOperation) -> Option<Previewable> {
    match op {
        GitOperation::RevertCommit { commit } => Some(Previewable::Revert {
            commit: commit.as_str().to_string(),
        }),
        GitOperation::CherryPick { commit } => Some(Previewable::CherryPick {
            commit: commit.as_str().to_string(),
        }),
        GitOperation::MergeBranch { branch } => Some(Previewable::Merge {
            branch: branch.as_str().to_string(),
        }),
        _ => None,
    }
}

/// The variant's own wire name, for a human reading
/// [`PreviewOutcome::Unsupported`].
///
/// Read out of serde's own `"op"` tag rather than written as a second match.
/// `GitOperation` is `#[serde(tag = "op", rename_all = "snake_case")]`, so a
/// variant added later gets a correct name here for free — a hand-written
/// match would need editing and would report the wrong variant until someone
/// noticed.
fn operation_name(op: &GitOperation) -> String {
    serde_json::to_value(op)
        .ok()
        .and_then(|v| v.get("op").and_then(|t| t.as_str()).map(str::to_string))
        // Unreachable for any variant of a `#[serde(tag = "op")]` enum that
        // serializes. Named rather than `unwrap()`ed: a panic in a preview
        // would be a 500 for a read-only question.
        .unwrap_or_else(|| "unknown".to_string())
}

/// The three-way merge and the parent list one operation reduces to, together
/// with the scratch store the merge runs in.
struct Recipe {
    /// The scratch store, held for as long as the recipe is: dropping it
    /// deletes the directory, so it must outlive `merge-tree`/`commit-tree`.
    store: ScratchStore,
    /// `--merge-base=<oid>`, or `None` to let git compute it.
    ///
    /// # `None` is not "unset", it is the faithful answer for a merge
    ///
    /// A revert and a cherry-pick *are* synthetic three-way merges with a
    /// stated base — that is what the operation means — so they pass one.
    /// `MergeBranch` is a real merge, and `git merge` computes its own base
    /// with the recursive strategy, which builds a **virtual** merge base when
    /// two branches have more than one. Passing a single `git merge-base`
    /// answer there would produce a tree `git merge` would not produce on a
    /// criss-cross history: a confidently wrong picture, which is the one
    /// failure §4.3 exists to make impossible. So the merge arm hands
    /// `merge-tree` the two commits and lets git do what git would do.
    merge_base: Option<String>,
    ours: String,
    theirs: String,
    parents: Vec<String>,
    message: String,
}

/// What one operation reduces to once the repository has been asked.
enum Plumbing {
    Synthesize(Recipe),
    /// The merge is a fast-forward: no commit is created, `to` is where the
    /// refs land.
    FastForward {
        to: String,
    },
    /// The branch tip is already an ancestor of HEAD. Nothing happens at all.
    AlreadyUpToDate,
    /// The operation is one of the three names, but *this* instance of it
    /// cannot be expressed — reverting a merge or a root commit. The payload
    /// is what [`PreviewOutcome::Unsupported`] reports.
    Unsupported(String),
}

/// Ask the repository what `op` reduces to.
///
/// The scratch store is created here, once, and moved into the [`Recipe`] —
/// the arms that create no commit never create a store at all.
async fn resolve_plumbing(
    repo: &Path,
    op: &Previewable,
    head: &str,
) -> Result<Plumbing, PreviewUnavailable> {
    match op {
        Previewable::Revert { commit } => {
            let target = read_commit_record(repo, None, commit).await?;
            // A merge commit or a root commit has no *sole* parent, and
            // `merge-tree` needs one as `theirs`. Same fail-closed rule
            // `activity::undoables` already applies: no established answer, no
            // picture. (Verified there: a synthetic empty-tree stand-in is not
            // a commit `--merge-base` will accept.)
            let Some(parent) = sole_parent(&target) else {
                return Ok(Plumbing::Unsupported("revert_commit".to_string()));
            };
            let store = ScratchStore::new(repo).await?;
            Ok(Plumbing::Synthesize(Recipe {
                store,
                // base = the commit being reverted, ours = HEAD, theirs = its
                // parent. Byte-identical to the merge
                // `activity::revert_would_conflict` already runs, which is what
                // makes this preview and the app's own revert offer consistent
                // by construction rather than by review.
                merge_base: Some(target.id.clone()),
                ours: head.to_string(),
                theirs: parent.to_string(),
                parents: vec![head.to_string()],
                message: revert_message(&target),
            }))
        }
        Previewable::CherryPick { commit } => {
            let target = read_commit_record(repo, None, commit).await?;
            let Some(parent) = sole_parent(&target) else {
                return Ok(Plumbing::Unsupported("cherry_pick".to_string()));
            };
            let store = ScratchStore::new(repo).await?;
            Ok(Plumbing::Synthesize(Recipe {
                store,
                merge_base: Some(parent.to_string()),
                ours: head.to_string(),
                theirs: target.id.clone(),
                parents: vec![head.to_string()],
                // git reuses the picked commit's message verbatim.
                message: target.body.clone(),
            }))
        }
        Previewable::Merge { branch } => {
            let tip = git_cmd::rev_parse(repo, branch)
                .await
                .map_err(|e| check_failed(format!("resolving `{branch}`: {e}")))?
                .ok_or_else(|| check_failed(format!("`{branch}` does not resolve to a commit")))?;
            // ONE spawn answers all three questions. `merge-base(head, tip)`
            // equals `tip` exactly when `tip` is an ancestor of `head`
            // (already up to date), and equals `head` exactly when `head` is an
            // ancestor of `tip` (fast-forward). Two extra
            // `merge-base --is-ancestor` spawns would tell us nothing this one
            // does not.
            let base = merge_base(repo, head, &tip).await?;
            if base == tip {
                return Ok(Plumbing::AlreadyUpToDate);
            }
            if base == head {
                return Ok(Plumbing::FastForward { to: tip });
            }
            let store = ScratchStore::new(repo).await?;
            Ok(Plumbing::Synthesize(Recipe {
                store,
                // See `Recipe::merge_base` for why this is `None` and not
                // `Some(base)`.
                merge_base: None,
                ours: head.to_string(),
                theirs: tip.clone(),
                // Order is load-bearing: `git merge` records HEAD first and
                // the merged tip second, and a transposed parent list draws a
                // graph a person can tell apart from the real one at a glance.
                parents: vec![head.to_string(), tip],
                message: merge_message(branch),
            }))
        }
    }
}

/// The commit's one parent, or `None` for a root (0) or a merge (2+).
fn sole_parent(target: &CommitRecord) -> Option<&str> {
    match target.parents.as_slice() {
        [only] => Some(only.as_str()),
        _ => None,
    }
}

/// git's own default revert message: `Revert "<subject>"`, a blank line, then
/// `This reverts commit <oid>.` — measured against git 2.43.0 on this host.
///
/// The literal words matter. This string is the row's summary in the UI, so a
/// preview that invented its own wording would show the user a commit whose
/// message is not the one `git revert` will write.
fn revert_message(target: &CommitRecord) -> String {
    format!(
        "Revert \"{}\"\n\nThis reverts commit {}.\n",
        target.subject, target.id
    )
}

/// git's own default merge message.
///
/// Honest limit: `git merge` appends ` into <branch>` when the current branch
/// is not the default one. That suffix is not reproduced here, so a merge
/// preview's summary can differ from the eventual commit's by that clause. It
/// is cosmetic — A5 compares parent topology, lane and row order, never
/// message text — and naming it is cheaper than a wrong guess at git's rule.
fn merge_message(branch: &str) -> String {
    format!("Merge branch '{branch}'\n")
}

// ---------------------------------------------------------------------------
// The scratch store
// ---------------------------------------------------------------------------

/// A throwaway bare object store, created inside the served repository's own
/// `commondir`, that can read every object the repository has and can write
/// none of them back.
///
/// # Why it lives under `commondir` and nowhere else — measured
///
/// [`crate::git_cmd::sandboxed`] derives `read_only` from
/// `state::read_only_for_path(repo)` and calls
/// `sandbox::policy_for(repo, read_only, need)`, which pushes `repo` and its
/// resolved `commondir` into the **read-write** trees, pushes `$HOME`
/// read-only, and grants nothing else. Nothing else on the filesystem is
/// reachable by the child.
///
/// A store in `/tmp` would therefore be created fine and then fail on its own
/// `objects/info/alternates`, which names a path outside every grant: Landlock
/// denies the read and the preview fails for a reason that has nothing to do
/// with git. Inside `commondir` there is exactly one grant and no new policy —
/// no security-boundary change, and nothing under `sandbox/` is touched.
///
/// The spawn passes the **real repository** as `repo` (so the grant is built
/// from it) and selects this store with `--git-dir=<abs path>`, which git
/// resolves before the subcommand and which `sandbox::network_need` skips as a
/// bare flag, classifying `merge-tree`/`commit-tree`/`show` as
/// `NetworkNeed::Local`.
///
/// # What A2 does and does not guarantee
///
/// A2 is "**no new object under `<commondir>/objects`**", not "nothing written
/// under `.git`". This store *is* a directory created under `commondir`. A
/// test that counted files under `<commondir>` would count this store's own
/// objects and fail for exactly the reason the design works.
///
/// # `TempDir`, and a named prefix
///
/// `tempfile` is a production dependency of this crate (`Cargo.toml`, above
/// `[dev-dependencies]`, for `sandbox::probe`'s boot fixture). [`tempfile::TempDir`]
/// gives uniqueness under concurrent previews and RAII removal on every exit
/// path — the return, the `?`, the panic. It does **not** survive a `SIGKILL`,
/// which is why [`Self::sweep_stale`] runs first; and the prefix is named
/// rather than `tempfile`'s default so the sweep has something it can prove it
/// created. See [`SCRATCH_PREFIX`].
struct ScratchStore {
    dir: tempfile::TempDir,
}

impl ScratchStore {
    /// Create `<commondir>/gv-preview-<random>/` as a bare repository whose
    /// object format matches `repo`'s, with `objects/info/alternates` pointing
    /// at `<commondir>/objects` beside it.
    ///
    /// Two git steps and one file write:
    ///
    /// 1. `rev-parse --show-object-format`
    /// 2. `-c init.templateDir= init -q --bare --object-format=<fmt> <abs>`
    ///    into the directory `TempDir` already created — git owns the on-disk
    ///    layout rather than this file guessing at it.
    /// 3. `std::fs::write(objects/info/alternates, <commondir>/objects)` —
    ///    git has no plumbing that adds an alternate.
    ///
    /// # The object format is load-bearing, and it was nearly missed
    ///
    /// A SHA-1 scratch store cannot read a SHA-256 repository across an
    /// alternates boundary. Measured on this host, 2026-08-30: with a
    /// `--object-format=sha1` store pointed at a `--object-format=sha256`
    /// repository, `cat-file -t <head>` answered `fatal: Not a valid object
    /// name`; with `--object-format=sha256` for the same store it answered
    /// `commit`. `CommitOid` accepts a 64-character id, so this codebase
    /// already contemplates SHA-256 repositories. `refStorage` needs no
    /// matching — this store never holds a ref. Only the hash format crosses
    /// the alternates boundary, which is why it is the only thing inherited.
    ///
    /// # `-c init.templateDir=` is pinned now, not later
    ///
    /// `git init --bare` copies `init.templateDir` into the new store, so
    /// `hooks/` arrives populated with git's 14 `.sample` files (measured).
    /// That is inert today — `merge-tree` and `commit-tree` fire no hooks —
    /// but `policy_for` sets `HookMode::Run` regardless, so a future step here
    /// that *did* fire hooks would inherit whatever the host's template
    /// directory points at. Emptying it costs one argv pair; discovering the
    /// omission later costs a security review. Measured: with the flag, the
    /// store has no `hooks` directory at all.
    async fn new(repo: &Path) -> Result<Self, PreviewUnavailable> {
        let commondir = commondir_of(repo)?;
        Self::sweep_stale(&commondir);

        let format = object_format(repo).await?;

        let dir = tempfile::Builder::new()
            .prefix(SCRATCH_PREFIX)
            .tempdir_in(&commondir)
            .map_err(|e| {
                scratch_failed(format!(
                    "could not create a scratch store in {}: {e}",
                    commondir.display()
                ))
            })?;
        let scratch = dir
            .path()
            .to_str()
            .ok_or_else(|| scratch_failed("the scratch store path is not valid UTF-8"))?
            .to_string();

        let out = git_cmd::git_output(
            repo,
            &[
                "-c",
                "init.templateDir=",
                "init",
                "-q",
                "--bare",
                "--object-format",
                &format,
                &scratch,
            ],
        )
        .await
        .map_err(|e| scratch_failed(format!("could not run git init: {e}")))?;
        if !out.status.success() {
            return Err(scratch_failed(git_said(&out.stderr, "git init failed")));
        }

        let alternates = dir.path().join("objects").join("info");
        std::fs::create_dir_all(&alternates)
            .map_err(|e| scratch_failed(format!("creating {}: {e}", alternates.display())))?;
        let objects = commondir.join("objects");
        // A trailing newline: git reads this file line by line, and a file with
        // no terminator is still read, but writing one keeps it a normal text
        // file for anyone who looks.
        let mut line = objects.as_os_str().to_string_lossy().into_owned();
        line.push('\n');
        std::fs::write(alternates.join("alternates"), line).map_err(|e| {
            scratch_failed(format!(
                "seeding the scratch store's alternates from {}: {e}",
                objects.display()
            ))
        })?;

        Ok(Self { dir })
    }

    /// The `--git-dir=<abs>` token.
    fn git_dir_flag(&self) -> String {
        format!("--git-dir={}", self.dir.path().display())
    }

    /// Remove `gv-preview-*` siblings older than [`STALE_SCRATCH_AGE`].
    ///
    /// Best-effort and entirely silent on failure: a sweep that refused to run
    /// would turn a leftover directory into a broken feature, which is worse
    /// than the leftover. Two rules make it safe to run inside a user's `.git`:
    ///
    /// * it only considers **directories** whose file name starts with
    ///   [`SCRATCH_PREFIX`] — a name this module chose, never one it found;
    /// * it only removes one whose modification time is older than the bound,
    ///   so a *concurrent* preview's store is never a candidate.
    ///
    /// An entry whose age cannot be read is left alone. "We could not tell how
    /// old it is" is not grounds to delete something inside someone's
    /// repository.
    fn sweep_stale(commondir: &Path) {
        let Ok(entries) = std::fs::read_dir(commondir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(SCRATCH_PREFIX) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(modified) else {
                continue;
            };
            if age < STALE_SCRATCH_AGE {
                continue;
            }
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `<commondir>` for `repo`, from `sandbox::repo_paths::resolve` — **the same
/// function `sandbox::policy_for` resolves it with**.
///
/// Deliberately not a second `rev-parse --git-common-dir`: the scratch store
/// must sit inside the tree the policy actually granted, and two resolvers can
/// disagree about a linked worktree. Reading `sandbox::repo_paths` is not
/// editing it; nothing under `sandbox/` changes.
fn commondir_of(repo: &Path) -> Result<PathBuf, PreviewUnavailable> {
    crate::sandbox::repo_paths::resolve(repo)
        .map(|paths| paths.commondir)
        .map_err(|e| scratch_failed(format!("resolving the repository's git directory: {e}")))
}

/// `rev-parse --show-object-format` — `sha1` or `sha256`.
async fn object_format(repo: &Path) -> Result<String, PreviewUnavailable> {
    let out = git_cmd::git_output(repo, &["rev-parse", "--show-object-format"])
        .await
        .map_err(|e| scratch_failed(format!("could not run git rev-parse: {e}")))?;
    if !out.status.success() {
        return Err(scratch_failed(git_said(
            &out.stderr,
            "git rev-parse --show-object-format failed",
        )));
    }
    let format = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if format.is_empty() {
        return Err(scratch_failed(
            "git rev-parse --show-object-format printed nothing",
        ));
    }
    Ok(format)
}

// ---------------------------------------------------------------------------
// The git steps
// ---------------------------------------------------------------------------

/// What `merge-tree --write-tree` established.
///
/// Classified from git's own exit code, exactly as
/// [`crate::activity::revert_would_conflict`] does: `Some(0)` clean, `Some(1)`
/// conflict, anything else an error. That contract — unlike git's prose —
/// does not shift with locale or version.
#[derive(Debug, PartialEq, Eq)]
enum MergeTreeAnswer {
    Clean { tree: String },
    Conflict { paths: Vec<String> },
}

/// Run the three-way merge in the scratch store.
///
/// argv: `["--git-dir=<scratch>", "merge-tree", "-z", "--write-tree",
/// ("--merge-base=<base>",)? "<ours>", "<theirs>"]`, to which
/// [`git_cmd::git_output`] prepends `-C <real repo>`.
///
/// `-z` because without it git C-quotes unusual paths, and this would then be
/// parsing a quoted form to recover bytes it could have had directly.
async fn merge_tree(repo: &Path, recipe: &Recipe) -> Result<MergeTreeAnswer, PreviewUnavailable> {
    let git_dir = recipe.store.git_dir_flag();
    let base_flag = recipe
        .merge_base
        .as_ref()
        .map(|base| format!("--merge-base={base}"));
    let mut args: Vec<&str> = vec![&git_dir, "merge-tree", "-z", "--write-tree"];
    if let Some(flag) = base_flag.as_deref() {
        args.push(flag);
    }
    args.push(&recipe.ours);
    args.push(&recipe.theirs);

    let out = git_cmd::git_output(repo, &args)
        .await
        .map_err(|e| check_failed(format!("could not run git merge-tree: {e}")))?;
    match out.status.code() {
        Some(0) => {
            let tree = parse_merge_tree_tree(&out.stdout)
                .ok_or_else(|| check_failed("git merge-tree printed no tree oid"))?;
            Ok(MergeTreeAnswer::Clean { tree })
        }
        Some(1) => {
            let paths = parse_merge_tree_conflicts(&out.stdout);
            if paths.is_empty() {
                // `Conflict { paths: [] }` reads as "conflicted, nothing
                // conflicted". git said conflict and we could not name a
                // single file, so we have no fact to report — not a fact
                // that reports nothing.
                return Err(check_failed(
                    "git merge-tree reported a conflict but named no path",
                ));
            }
            Ok(MergeTreeAnswer::Conflict { paths })
        }
        _ => Err(check_failed(git_said(
            &out.stderr,
            "git merge-tree did not produce an answer",
        ))),
    }
}

/// Write the hypothetical commit into the scratch store.
///
/// argv: `["--git-dir=<scratch>", "-c", "user.name=git-vista", "-c",
/// "user.email=preview@git-vista.invalid", "commit-tree", "<tree>",
/// ("-p", "<parent>")…, "-m", "<message>"]`.
///
/// # Identity is pinned on argv; the dates cannot be, and that is structural
///
/// `-c user.name` / `-c user.email` are passed rather than inherited so the
/// call cannot fail on a host with no configured identity, and so the row is
/// honestly attributed to something that did not write a commit. The `.invalid`
/// TLD is reserved by RFC 2606 and can never be a real address.
///
/// `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` cannot be set: `git_cmd` exposes no
/// arity that adds an environment variable, and adding one would widen the
/// sealed launcher for a preview. So this commit's oid is **not reproducible**,
/// by construction, and nothing downstream may compare it by identity — which
/// is exactly why the parity test maps the hypothetical oid onto the real one
/// by position.
async fn commit_tree(
    repo: &Path,
    store: &ScratchStore,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Result<String, PreviewUnavailable> {
    let git_dir = store.git_dir_flag();
    let mut args: Vec<&str> = vec![
        &git_dir,
        "-c",
        "user.name=git-vista",
        "-c",
        "user.email=preview@git-vista.invalid",
        "commit-tree",
        tree,
    ];
    for parent in parents {
        args.push("-p");
        args.push(parent);
    }
    args.push("-m");
    args.push(message);

    let out = git_cmd::git_output(repo, &args)
        .await
        .map_err(|e| check_failed(format!("could not run git commit-tree: {e}")))?;
    if !out.status.success() {
        return Err(check_failed(git_said(
            &out.stderr,
            "git commit-tree did not produce an answer",
        )));
    }
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if oid.is_empty() {
        return Err(check_failed("git commit-tree printed no commit oid"));
    }
    Ok(oid)
}

/// Read the hypothetical commit back **out of the scratch store** and build its
/// [`CommitSummary`], rather than synthesising one.
///
/// Two reasons, and the second is the binding one. `time` is the value git
/// actually recorded — a `SystemTime::now()` stand-in is off by up to a second
/// and would be a small lie in a feature whose entire argument is that it does
/// not model git. And a successful read is itself the tell that the store can
/// see its own object.
async fn read_back(
    repo: &Path,
    store: &ScratchStore,
    oid: &str,
) -> Result<CommitSummary, PreviewUnavailable> {
    let record = read_commit_record(repo, Some(&store.git_dir_flag()), oid).await?;
    Ok(record.into_summary())
}

/// One commit as `git show -s` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitRecord {
    id: String,
    parents: Vec<String>,
    /// **Committer** time, not author time. `git_vista_git::walk_history` fills
    /// `CommitSummary.time` from `info.commit_time()`, and
    /// `stable_topo_order` sorts on that field — so an author-time row would
    /// be placed on a different clock from every other row in the same graph,
    /// silently reordering the picture. Measured against `history.rs`'s own
    /// `time: info.commit_time()`.
    time: i64,
    author: String,
    subject: String,
    /// The raw message, `%B`, trailing newline included — what a cherry-pick
    /// reuses verbatim.
    body: String,
}

impl CommitRecord {
    fn into_summary(self) -> CommitSummary {
        CommitSummary {
            id: Oid(self.id),
            parents: self.parents.into_iter().map(Oid).collect(),
            summary: self.subject,
            author: self.author,
            time: self.time,
        }
    }
}

/// `show -s --format=%H%x00%P%x00%ct%x00%an%x00%s%x00%B%x00 <rev>`, in `repo`
/// or (with `git_dir`) in a scratch store.
///
/// NUL-separated because `%B` is a whole message: a newline-separated format
/// could not carry it, and a commit message cannot contain a NUL.
async fn read_commit_record(
    repo: &Path,
    git_dir: Option<&str>,
    rev: &str,
) -> Result<CommitRecord, PreviewUnavailable> {
    let mut args: Vec<&str> = Vec::new();
    if let Some(flag) = git_dir {
        args.push(flag);
    }
    args.extend_from_slice(&[
        "show",
        "-s",
        "--format=%H%x00%P%x00%ct%x00%an%x00%s%x00%B%x00",
        rev,
    ]);
    let out = git_cmd::git_output(repo, &args)
        .await
        .map_err(|e| check_failed(format!("could not run git show: {e}")))?;
    if !out.status.success() {
        return Err(check_failed(git_said(
            &out.stderr,
            &format!("git show could not read `{rev}`"),
        )));
    }
    parse_commit_record(&out.stdout)
        .ok_or_else(|| check_failed(format!("git show printed no readable record for `{rev}`")))
}

/// Probe the host's git version. See [`GIT_VERSION`] for why this is cached
/// per process.
///
/// `git_cmd::git_output(repo, &["--version"])` — the sealed launcher, no new
/// spawn site. `sandbox::network_need` classifies an argv with no subcommand
/// token at all as `NetworkNeed::Local`, so this needs no special declaration.
async fn git_version(repo: &Path) -> Result<(u32, u32, u32), PreviewUnavailable> {
    GIT_VERSION
        .get_or_try_init(|| async {
            let out = git_cmd::git_output(repo, &["--version"])
                .await
                .map_err(|e| check_failed(format!("could not run git --version: {e}")))?;
            if !out.status.success() {
                return Err(check_failed(git_said(
                    &out.stderr,
                    "git --version did not produce an answer",
                )));
            }
            let line = String::from_utf8_lossy(&out.stdout);
            // Never "assume new enough" and never "assume too old": a line we
            // cannot read is no fact at all.
            parse_git_version(&line).ok_or_else(|| {
                check_failed(format!(
                    "could not read a version out of git's own output: {:?}",
                    line.trim()
                ))
            })
        })
        .await
        .copied()
}

/// git's own stderr where there is any, else `fallback` — the B3 posture the
/// rest of this crate uses.
fn git_said(stderr: &[u8], fallback: &str) -> String {
    let said = String::from_utf8_lossy(stderr).trim().to_string();
    if said.is_empty() {
        fallback.to_string()
    } else {
        said
    }
}

/// `merge-base <a> <b>` — the single spawn the merge arm's three questions
/// reduce to.
async fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String, PreviewUnavailable> {
    let out = git_cmd::git_output(repo, &["merge-base", a, b])
        .await
        .map_err(|e| check_failed(format!("could not run git merge-base: {e}")))?;
    if !out.status.success() {
        return Err(check_failed(git_said(
            &out.stderr,
            "git merge-base did not produce an answer",
        )));
    }
    let base = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if base.is_empty() {
        // Two histories with no common ancestor. `git merge` refuses this
        // without `--allow-unrelated-histories`, so there is no picture to
        // draw and no fact to state beyond git's own silence.
        return Err(check_failed(
            "the two commits have no common ancestor, so there is no merge to preview",
        ));
    }
    Ok(base)
}

// ---------------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------------

/// Parse the `major.minor.patch` at the front of git's own `--version` line.
///
/// git prints `git version 2.43.0`, and vendor builds append suffixes
/// (`2.39.5 (Apple Git-154)`, `2.43.0.windows.1`), so this takes the first
/// three dot-separated integer runs after the `git version ` prefix and stops
/// at the first component that is not all digits. `None` means the line did not
/// look like git's — which is a `CheckFailed`, never a silent "old enough" or
/// "new enough".
fn parse_git_version(line: &str) -> Option<(u32, u32, u32)> {
    let rest = line.trim().strip_prefix("git version ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // A two-component version (`git version 2.38`) is a real, readable answer;
    // the patch level is the only part allowed to be missing or non-numeric.
    let patch: u32 = parts
        .next()
        .and_then(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether `found` is below [`MIN_GIT_FOR_PREVIEW`], as the reason to report.
///
/// Pure and separate from the probe so the *decision* can be tested with
/// literal versions on both sides of the floor, rather than only on whatever
/// git this host happens to have. The comparison is on `(major, minor)` alone:
/// `merge-tree --write-tree` arrived in 2.38.0, so every 2.38.x is new enough
/// and no patch level below it is.
fn version_gate(found: (u32, u32, u32)) -> Option<PreviewUnavailable> {
    let (major, minor, patch) = found;
    if (major, minor) >= MIN_GIT_FOR_PREVIEW {
        return None;
    }
    Some(PreviewUnavailable::GitTooOld {
        // The parsed triple re-rendered, never git's raw line: a vendor suffix
        // (`2.39.5 (Apple Git-154)`) is not the caller's business.
        found: format!("{major}.{minor}.{patch}"),
        minimum: format!("{}.{}", MIN_GIT_FOR_PREVIEW.0, MIN_GIT_FOR_PREVIEW.1),
    })
}

/// The tree oid `merge-tree -z` prints first.
///
/// Measured shape on git 2.43.0, clean: the whole stdout is
/// `<tree oid>\0` — one record then a terminator.
fn parse_merge_tree_tree(stdout: &[u8]) -> Option<String> {
    let first = stdout.split(|b| *b == 0).next()?;
    let tree = String::from_utf8_lossy(first).trim().to_string();
    (!tree.is_empty()).then_some(tree)
}

/// The conflicted-path set out of `merge-tree -z`'s stdout: deduplicated, in
/// first-appearance order, lossily decoded.
///
/// # The record shape, measured on git 2.43.0
///
/// NUL-separated records:
///
/// ```text
/// <tree oid>                                    the merged tree
/// <mode> <oid> <stage>\t<path>                  one per conflicted stage
/// …
/// <empty>                                       end of the stage block
/// <count>, <path>…, <type>, <message>           informational messages
/// ```
///
/// So: skip record 0, take records up to the **first empty one**, and stop.
/// Records past it are prose (`Auto-merging`, `CONFLICT (content): …`) that
/// happens to contain path-shaped text — reading them as paths would report
/// `Auto-merging` as a conflicted file.
///
/// The path is everything after the **first** TAB, because the three fields
/// before it (mode, oid, stage) can never contain one, while a path can.
/// Lossily decoded because `-z` hands back bytes and a path that is not UTF-8
/// is still a path the user needs named.
fn parse_merge_tree_conflicts(stdout: &[u8]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for record in stdout.split(|b| *b == 0).skip(1) {
        if record.is_empty() {
            break;
        }
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Split one `show -s --format=…%x00…` record into its six fields.
fn parse_commit_record(stdout: &[u8]) -> Option<CommitRecord> {
    let fields: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    if fields.len() < 6 {
        return None;
    }
    let text = |i: usize| String::from_utf8_lossy(fields[i]).into_owned();
    let id = text(0).trim().to_string();
    if id.is_empty() {
        return None;
    }
    let parents = text(1)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let time: i64 = text(2).trim().parse().ok()?;
    Some(CommitRecord {
        id,
        parents,
        time,
        author: text(3),
        subject: text(4),
        body: text(5),
    })
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// The refs one operation moves, as `(display ref name, new target)`.
///
/// # These names must be `read_refs`' display names, or the picture is wrong
///
/// `lay_out_preview` rewrites the ref slice **before** laying the `after`
/// graph out, because `layout_with_refs` reserves lane 0 from the refs it is
/// handed and seeds colour slot 0 from them. Hand it `refs/heads/main` where
/// `read_refs` emitted `main` and nothing matches: the hypothetical commit
/// finds lane 0 taken, lands in lane 1, and falls into the synthetic colour
/// fallback — a confidently wrong picture, drawn from correct data.
///
/// `read_refs` emits HEAD as its own entry named `"HEAD"` whenever it resolves
/// (and the branch too when HEAD is on one), so an attached HEAD moves two
/// refs and a detached HEAD moves one.
fn ref_moves_to(repo: &Path, target: &str) -> Vec<(String, Oid)> {
    let mut moves = Vec::new();
    if let Some(branch) = git_vista_git::read_head_branch(repo) {
        moves.push((branch, Oid(target.to_string())));
    }
    moves.push(("HEAD".to_string(), Oid(target.to_string())));
    moves
}

/// Read the repository's real history and refs, hand them plus the
/// hypothetical commit to the pure layout half, and package the two graphs.
///
/// # A damaged layout is `CheckFailed`, never a returned `Graph`
///
/// `lay_out_preview` reports two distinct caller mistakes — a `ref_moves`
/// entry that matched no ref (`unmatched_ref_moves`) and an `added` commit
/// with no `ref_moves` at all (`added_without_ref_moves`) — and a correct
/// preview has both clear. Either one means the `after` graph's lane and
/// colour assignment disagree with what a real run would produce. Returning
/// that graph would be the exact failure §4.3 exists to prevent, so it is not
/// an option: this is "no fact", and it says so.
fn lay_out(
    repo: &Path,
    added: Option<CommitSummary>,
    ref_moves: Vec<(String, Oid)>,
) -> Result<PreviewResponse, PreviewUnavailable> {
    let before = git_vista_git::walk_history(repo, PREVIEW_HISTORY_LIMIT)
        .map_err(|e| check_failed(format!("reading history: {e}")))?;
    let refs: Vec<GitRef> =
        git_vista_git::read_refs(repo).map_err(|e| check_failed(format!("reading refs: {e}")))?;
    let head_branch = git_vista_git::read_head_branch(repo);

    // Captured before the move so `changes` can report both endpoints.
    let previous: Vec<(String, Oid)> = ref_moves
        .iter()
        .filter_map(|(name, _)| {
            refs.iter()
                .find(|r| &r.name == name)
                .map(|r| (name.clone(), r.target.clone()))
        })
        .collect();

    let added_id = added.as_ref().map(|c| c.id.clone());
    let layout: PreviewLayout = lay_out_preview(PreviewInput {
        before,
        refs,
        head_branch,
        added,
        ref_moves: ref_moves.clone(),
    });

    if !layout.unmatched_ref_moves.is_empty() {
        return Err(check_failed(format!(
            "the preview moved refs that this repository does not have: {:?} — \
             the after graph's lanes and colours would not be the ones a real \
             run produces, so there is no honest picture to return",
            layout.unmatched_ref_moves
        )));
    }
    if layout.added_without_ref_moves {
        return Err(check_failed(
            "the preview added a commit and moved no ref — the after graph's \
             lanes and colours would not be the ones a real run produces",
        ));
    }

    let mut changes: Vec<PreviewChange> = Vec::new();
    if let Some(commit) = added_id {
        changes.push(PreviewChange::Added { commit });
    }
    for (name, to) in &ref_moves {
        let from = previous
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, oid)| oid.clone())
            // A ref that matched (checked above) always has a previous target;
            // this arm is unreachable and is written rather than `unwrap`ed.
            .unwrap_or_else(|| to.clone());
        changes.push(PreviewChange::RefMoved {
            ref_name: name.clone(),
            from,
            to: to.clone(),
        });
    }
    changes.extend(layout.lane_shifts.into_iter().map(PreviewChange::from));

    Ok(PreviewOutcome::Graph {
        before: envelope(layout.before),
        after: envelope(layout.after),
        changes,
    })
}

/// `Graph` → the wire envelope.
///
/// `lane_count` is `Graph::lane_count` verbatim — the gutter width, stub
/// columns included — which is what [`PreviewGraph`] documents it as.
fn envelope(graph: Graph) -> PreviewGraph<GraphRow, Edge, BranchStub> {
    PreviewGraph {
        rows: graph.rows,
        edges: graph.edges,
        stubs: graph.stubs,
        lane_count: graph.lane_count,
    }
}

#[cfg(test)]
#[path = "preview_suite.rs"]
mod suite;
