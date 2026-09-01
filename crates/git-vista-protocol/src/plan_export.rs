//! Plan export (M10, #590) — turning an approved [`Plan`] into the commands a
//! human can read, check off, and type themselves.
//!
//! The point is control: after the app has planned a complex git operation,
//! the user gets the *list of actions* rather than a button that runs them.
//!
//! ## The finding this module is shaped around
//!
//! #590 was filed on the premise that the export is nearly free, because "the
//! planner already computes the exact argv for every operation — the printed
//! command IS the command the executor would run". Reading the executors
//! first, as the issue asked, that premise turns out to be **half true, and
//! the false half is the interesting one**.
//!
//! Before this module there were exactly four places where a git argv existed
//! as a value a caller could hold without spawning it: `push::push_argv`,
//! `tag_exec::create_tag_argv`, `remote_tags::push_tag_argv` and
//! `remote_tags::delete_remote_tag_argv`. Everywhere else the argv was a local
//! `Vec<&str>` built *inside* an `async fn exec_*` and consumed by `run_git`
//! in the same expression — reachable only by running it. The planner's own
//! comment in `execute` says so plainly: *"By the time the argv exists, the
//! operation is gone."*
//!
//! So the export could not simply read something that was already there. The
//! honest fix is the one `push_argv` already models — lift the argv into a
//! pure function, and have the executor call it — which is what this module
//! does. It is the **single** source: the export prints the same `Vec<String>`
//! the executor hands to git. Rebuilding the command strings a second time
//! here would have reintroduced exactly the drift the design exists to avoid.
//!
//! ## And three kinds of operation where no honest command can be printed
//!
//! Lifting the argv is not enough on its own, because for some operations the
//! argv is **not a function of the plan at all**:
//!
//! 1. **Chosen at run time.** [`GitOperation::ResetBranch`] runs
//!    `git reset --hard <to>` when its branch is the checked-out one and
//!    `git branch -f <branch> <to>` when it is not — a decision made inside
//!    the executor from live repository state the plan does not carry. The
//!    three `Sequence*` verbs are the same shape: whether they spell
//!    `git cherry-pick --continue` or `git revert --continue` depends on which
//!    of `CHERRY_PICK_HEAD` / `REVERT_HEAD` exists at the moment they run.
//!    Printing one of the two would be worse than printing nothing: for
//!    `ResetBranch` the wrong guess hands the user a working-tree-destroying
//!    `reset --hard` where the app would have moved a ref and touched no file.
//!
//! 2. **Chained on an earlier step's output.**
//!    [`GitOperation::EmptyCommitOnBranch`] writes a commit object with
//!    `git commit-tree` and then advances the ref with `git update-ref` **to
//!    the object id the first command printed**. There is no literal to put in
//!    step 2 of a checklist.
//!
//! 3. **Not a command line at all.** [`GitOperation::StageSelection`] feeds a
//!    patch to `git apply` on **stdin**, and
//!    [`GitOperation::ResolveConflictContent`] writes bytes the user composed
//!    into a file. Neither is expressible as arguments, at any amount of
//!    quoting.
//!
//! [`Export`] therefore has four arms, not one, and the classifier
//! ([`export_operation`]) is an exhaustive wildcard-free match — the same
//! discipline [`crate::effects::network_need_for_operation`] uses, and for the
//! same reason: a new [`GitOperation`] variant must stop the build until
//! someone decides what it prints, rather than silently acquiring a wrong
//! command.
//!
//! This mirrors a decision the tree has already made once. When M6.39 (#92)
//! needed to say what `ResetBranch` does to the working tree, it did not pick
//! the worst case — it added [`crate::effects::WorktreeEffect`]'s named
//! conditional variant, because "answering `FilesRewritten` would tell a user
//! their files are about to be rewritten on a run where nothing is touched".
//! [`Export::ChosenAtRunTime`] is that same answer in the argv dimension, and
//! it names *both* candidates and what decides between them, so the honest
//! answer is more useful than the guess would have been rather than less.
//!
//! ## Quoting is a fact about the arguments, not a formatting choice
//!
//! Tom's login shell is fish; most shell advice on the internet is bash. A
//! single-quoted argument means the identical thing in POSIX `sh` and in fish
//! — *until it contains a single quote itself*, at which point the two need
//! genuinely different escapes (`'\''` vs `\'`). That is the entire divergence
//! for the arguments git-vista can produce, so [`render`] reports it as a
//! two-armed fact ([`Rendered`]) rather than picking a shell and hoping. A
//! command whose arguments are all shell-safe is [`Rendered::Portable`] and
//! can be pasted anywhere; one that is not says so and gives both spellings.
//!
//! ## Nothing here executes, reads a repository, or crosses the wire
//!
//! [`export_operation`] is a pure function of typed plan data, like
//! [`crate::explain::explain`] next door, and for the same stated reason:
//! pulling it out "makes the property testable over the entire input space
//! without spawning anything". [`Export`] deliberately does not derive
//! `Serialize` — the holder of a `Plan` computes it locally, and a serialized
//! copy would be the first thing to drift from the plan it describes.

use crate::conflict::Resolution;
use crate::plan::{
    BranchName, CommitMessage, CommitOid, ForcePublish, GitOperation, MergeStrategy, Plan, RefName,
    RemoteName, StashMessage, StashSelector, TagAnnotation, TagName, WorktreePath,
};

// ---------------------------------------------------------------------------
// The argv builders — one place per operation, shared with the executors
// ---------------------------------------------------------------------------
//
// Every function below returns the argv **without** a leading "git": that is
// the shape `run_git` takes, so the executor passes the result straight
// through and the export renders the same vector with `git ` in front. Owned
// `String`s throughout, even where `&str` would do, so that one uniform type
// crosses the crate boundary and no caller has to care which operations
// happen to need a computed argument.

/// `git branch <name> <at>` — create a branch at a commit.
///
/// Shared with [`GitOperation::RestoreBranch`], which is the same command line
/// with a different intent (re-creating a branch that was deleted, at its
/// journaled tip). Two operations, one argv, deliberately: they differ in what
/// the plan *means*, not in what git is asked to do.
pub fn create_branch_argv(name: &BranchName, at: &CommitOid) -> Vec<String> {
    vec![
        "branch".to_string(),
        name.as_str().to_string(),
        at.as_str().to_string(),
    ]
}

/// `git commit [--allow-empty] -m <message>`.
pub fn commit_on_head_argv(message: &CommitMessage, allow_empty: bool) -> Vec<String> {
    let mut argv = vec!["commit".to_string()];
    if allow_empty {
        argv.push("--allow-empty".to_string());
    }
    argv.push("-m".to_string());
    argv.push(message.as_str().to_string());
    argv
}

/// `git commit --amend [--allow-empty] -m <message>`.
///
/// The plan's `expected_tip` is a compare-and-swap the *executor* enforces
/// before spawning; it is not an argument, so it does not appear here. A
/// printed amend therefore carries a precondition the reader must check —
/// which is why [`checklist`] prints the plan's preconditions above the
/// commands rather than leaving them on the screen the user just left.
pub fn amend_commit_argv(message: &CommitMessage, allow_empty: bool) -> Vec<String> {
    let mut argv = vec!["commit".to_string(), "--amend".to_string()];
    if allow_empty {
        argv.push("--allow-empty".to_string());
    }
    argv.push("-m".to_string());
    argv.push(message.as_str().to_string());
    argv
}

/// `git add -A` — stage every working-tree change.
pub fn stage_all_argv() -> Vec<String> {
    vec!["add".to_string(), "-A".to_string()]
}

/// `git reset -q HEAD` — unstage everything, keeping every edit on disk.
pub fn unstage_all_argv() -> Vec<String> {
    vec!["reset".to_string(), "-q".to_string(), "HEAD".to_string()]
}

/// `git checkout <branch>`.
pub fn checkout_argv(branch: &BranchName) -> Vec<String> {
    vec!["checkout".to_string(), branch.as_str().to_string()]
}

/// `git merge --no-edit <ref>`.
///
/// Takes a [`RefName`] rather than a [`BranchName`] because the merge half of
/// a pull integrates the remote-tracking name (`origin/main`), which is not a
/// local branch — the same widening `run_branch_cmd` took in M2.20d (#230).
pub fn merge_argv(target: &RefName) -> Vec<String> {
    vec![
        "merge".to_string(),
        "--no-edit".to_string(),
        target.as_str().to_string(),
    ]
}

/// `git branch -d <branch>`, or `git branch -D <branch>` when `force`.
///
/// One builder with a flag rather than two functions, matching `exec_delete`,
/// which is also one executor with a flag: the safe and the force delete are
/// the same command line differing by one character, and splitting them would
/// invite the two spellings to drift apart.
pub fn delete_branch_argv(branch: &BranchName, force: bool) -> Vec<String> {
    vec![
        "branch".to_string(),
        if force { "-D" } else { "-d" }.to_string(),
        branch.as_str().to_string(),
    ]
}

/// `git rebase <base>`.
pub fn rebase_argv(base: &RefName) -> Vec<String> {
    vec!["rebase".to_string(), base.as_str().to_string()]
}

/// `git rebase --abort` — the executor's cleanup when a rebase fails, and the
/// line a reader needs if they run the rebase by hand and it stops on a
/// conflict.
///
/// Not a step of any exported plan: the app runs it only on a failure path, so
/// printing it as step 2 would describe a rebase that went wrong as though it
/// were the plan. It is shared for the same reason [`revert_abort_argv`] is —
/// the checklist names it in prose, and a named command should be the real one.
pub fn rebase_abort_argv() -> Vec<String> {
    vec!["rebase".to_string(), "--abort".to_string()]
}

/// `git cherry-pick [-m <mainline>] <commit>`.
pub fn cherry_pick_argv(commit: &CommitOid, mainline: Option<std::num::NonZeroU8>) -> Vec<String> {
    let mut argv = vec!["cherry-pick".to_string()];
    if let Some(m) = mainline {
        argv.push("-m".to_string());
        argv.push(m.get().to_string());
    }
    argv.push(commit.as_str().to_string());
    argv
}

/// `git revert --no-commit [-m <mainline>] <commit>` — step 1 of the two-step
/// revert.
///
/// The executor computes the revert into the index and commits it separately
/// (see [`revert_commit_argv`]) rather than letting `git revert` do both,
/// because only the split form can pass `--allow-empty`. A printed revert is
/// therefore two lines, and it has to be: a reader who runs only the first
/// line is left mid-sequence with `REVERT_HEAD` set, which is a real state to
/// be in and not a failure — but it is not a finished revert.
pub fn revert_compute_argv(
    commit: &CommitOid,
    mainline: Option<std::num::NonZeroU8>,
) -> Vec<String> {
    let mut argv = vec!["revert".to_string(), "--no-commit".to_string()];
    if let Some(m) = mainline {
        argv.push("-m".to_string());
        argv.push(m.get().to_string());
    }
    argv.push(commit.as_str().to_string());
    argv
}

/// `git commit --allow-empty --no-edit` — step 2 of the two-step revert.
pub fn revert_commit_argv() -> Vec<String> {
    vec![
        "commit".to_string(),
        "--allow-empty".to_string(),
        "--no-edit".to_string(),
    ]
}

/// `git revert --abort` — the executor's own cleanup when either revert step
/// fails, and the line a reader needs if they run the two steps by hand and
/// the first one conflicts.
pub fn revert_abort_argv() -> Vec<String> {
    vec!["revert".to_string(), "--abort".to_string()]
}

/// `<remote>/<branch>` — the remote-tracking name a pull's integration half
/// runs against, e.g. `origin/main`.
///
/// Moved here from `planner::pull::tracking_ref` so the export and the
/// executor cannot disagree about which ref the second half of a pull names.
/// That mattered more than the other moves: the export has to reproduce this
/// string to print the merge/rebase line, and reproducing it *locally* is
/// precisely the drift #590 exists to avoid.
///
/// A [`RefName`] and not a [`BranchName`], because it is not a local branch.
/// The conversion cannot fail and the `expect` explains why rather than
/// hoping — both halves already passed [`RefName`]'s identical
/// `require_git_safe` gate (non-empty, not option-shaped), so the join is
/// non-empty and begins with `remote`'s first byte, which is not `-`. Should
/// that gate ever widen asymmetrically this fails loudly at the one place
/// instead of silently at every argv.
pub fn tracking_ref(remote: &RemoteName, branch: &BranchName) -> RefName {
    RefName::new(format!("{}/{}", remote.as_str(), branch.as_str())).expect(
        "RemoteName and BranchName already satisfy RefName's require_git_safe \
         gate, so their `/`-join does too",
    )
}

/// `git fetch --progress <remote>`.
pub fn fetch_argv(remote: &RemoteName) -> Vec<String> {
    vec![
        "fetch".to_string(),
        "--progress".to_string(),
        remote.as_str().to_string(),
    ]
}

/// `git push [--set-upstream] [--force-with-lease=<branch>:<oid>] <remote> <branch>`.
///
/// Moved here verbatim from `planner::push::push_argv`, keeping its two
/// original properties: flags first so the tests describe one shape rather
/// than a family of them, and **no wildcard arm** in the [`ForcePublish`]
/// match, so a future force variant is a compile error rather than a silent
/// downgrade of a force the user approved into a fast-forward push.
///
/// The lease's ref half is the remote-side short name (`main`, not
/// `refs/remotes/origin/main`) — `--force-with-lease=<refname>:<expect>` names
/// a ref on the remote and git expands the short form against the remote's
/// advertised `refs/heads/main`.
pub fn push_argv(
    branch: &BranchName,
    remote: &RemoteName,
    set_upstream: bool,
    force: &ForcePublish,
) -> Vec<String> {
    let mut argv = vec!["push".to_string(), "--progress".to_string()];
    if set_upstream {
        argv.push("--set-upstream".to_string());
    }
    match force {
        ForcePublish::None => {}
        ForcePublish::WithLease {
            expected_remote_tip,
        } => argv.push(format!(
            "--force-with-lease={}:{}",
            branch.as_str(),
            expected_remote_tip.as_str()
        )),
    }
    argv.push(remote.as_str().to_string());
    argv.push(branch.as_str().to_string());
    argv
}

/// `git tag <name> <target>`, or `git tag -a|-s -m <message> <name> <target>`.
///
/// Moved here from `planner::tag_exec::create_tag_argv` unchanged.
pub fn create_tag_argv(
    name: &TagName,
    target: &CommitOid,
    annotation: Option<&TagAnnotation>,
) -> Vec<String> {
    let mut argv = vec!["tag".to_string()];
    match annotation {
        None => {}
        Some(a) => {
            argv.push(if a.sign { "-s" } else { "-a" }.to_string());
            argv.push("-m".to_string());
            argv.push(a.message.as_str().to_string());
        }
    }
    argv.push(name.as_str().to_string());
    argv.push(target.as_str().to_string());
    argv
}

/// `git tag -d <name>` — delete a local tag.
pub fn delete_local_tag_argv(name: &TagName) -> Vec<String> {
    vec![
        "tag".to_string(),
        "-d".to_string(),
        name.as_str().to_string(),
    ]
}

/// `git push --progress <remote> refs/tags/<name>`.
///
/// Moved here from `planner::remote_tags::push_tag_argv`. The full
/// `refs/tags/` path rather than the bare short name, so git's refspec
/// matching cannot decide that a same-named *branch* on the remote was meant.
pub fn push_tag_argv(name: &TagName, remote: &RemoteName) -> Vec<String> {
    vec![
        "push".to_string(),
        "--progress".to_string(),
        remote.as_str().to_string(),
        format!("refs/tags/{}", name.as_str()),
    ]
}

/// `git push --progress <remote> --delete refs/tags/<name>`.
///
/// Moved here from `planner::remote_tags::delete_remote_tag_argv`.
pub fn delete_remote_tag_argv(name: &TagName, remote: &RemoteName) -> Vec<String> {
    vec![
        "push".to_string(),
        "--progress".to_string(),
        remote.as_str().to_string(),
        "--delete".to_string(),
        format!("refs/tags/{}", name.as_str()),
    ]
}

/// `git stash push [--keep-index] [--include-untracked] [-m <message>]`.
pub fn push_stash_argv(
    message: Option<&StashMessage>,
    keep_index: bool,
    include_untracked: bool,
) -> Vec<String> {
    let mut argv = vec!["stash".to_string(), "push".to_string()];
    if keep_index {
        argv.push("--keep-index".to_string());
    }
    if include_untracked {
        argv.push("--include-untracked".to_string());
    }
    if let Some(m) = message {
        argv.push("-m".to_string());
        argv.push(m.as_str().to_string());
    }
    argv
}

/// `git stash apply <entry>`.
pub fn apply_stash_argv(entry: &StashSelector) -> Vec<String> {
    vec![
        "stash".to_string(),
        "apply".to_string(),
        entry.as_str().to_string(),
    ]
}

/// `git stash branch <name> <entry>`.
pub fn branch_from_stash_argv(name: &BranchName, entry: &StashSelector) -> Vec<String> {
    vec![
        "stash".to_string(),
        "branch".to_string(),
        name.as_str().to_string(),
        entry.as_str().to_string(),
    ]
}

/// `git stash drop <entry>`.
pub fn drop_stash_argv(entry: &StashSelector) -> Vec<String> {
    vec![
        "stash".to_string(),
        "drop".to_string(),
        entry.as_str().to_string(),
    ]
}

/// `git reset --hard <to>` — the checked-out arm of
/// [`GitOperation::ResetBranch`].
///
/// This one and [`move_branch_argv`] are the two candidates of an
/// [`Export::ChosenAtRunTime`], and they are shared builders for the same
/// reason every other argv here is: an export that spelled its *candidates*
/// locally would have the drift problem the module exists to solve, merely one
/// level further down, and in the arm where being wrong is worst.
pub fn reset_hard_argv(to: &CommitOid) -> Vec<String> {
    vec![
        "reset".to_string(),
        "--hard".to_string(),
        to.as_str().to_string(),
    ]
}

/// `git branch -f <branch> <to>` — the not-checked-out arm of
/// [`GitOperation::ResetBranch`]. Moves the label; touches no file.
pub fn move_branch_argv(branch: &BranchName, to: &CommitOid) -> Vec<String> {
    vec![
        "branch".to_string(),
        "-f".to_string(),
        branch.as_str().to_string(),
        to.as_str().to_string(),
    ]
}

/// Which sequence git is in the middle of — the fact the three `Sequence*`
/// operations' argv depends on, and which the plan does not carry.
///
/// Git keeps one sequencer per repository and records which in
/// `.git/CHERRY_PICK_HEAD` or `.git/REVERT_HEAD`. The executor reads that and
/// refuses when neither exists; the export names both possibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceKind {
    /// `.git/CHERRY_PICK_HEAD` exists.
    CherryPick,
    /// `.git/REVERT_HEAD` exists.
    Revert,
}

impl SequenceKind {
    /// The git subcommand that drives this sequence.
    pub fn subcommand(self) -> &'static str {
        match self {
            SequenceKind::CherryPick => "cherry-pick",
            SequenceKind::Revert => "revert",
        }
    }
}

/// Which way an in-progress sequence is being driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceVerb {
    /// Carry on with the rest of the sequence.
    Continue,
    /// Abandon the commit being applied; keep going.
    Skip,
    /// Unwind the whole sequence.
    Abort,
}

impl SequenceVerb {
    /// The flag this verb passes to the sequence's subcommand.
    pub fn flag(self) -> &'static str {
        match self {
            SequenceVerb::Continue => "--continue",
            SequenceVerb::Skip => "--skip",
            SequenceVerb::Abort => "--abort",
        }
    }
}

/// `git cherry-pick|revert --continue|--skip|--abort`.
pub fn sequence_argv(kind: SequenceKind, verb: SequenceVerb) -> Vec<String> {
    vec![kind.subcommand().to_string(), verb.flag().to_string()]
}

/// `git checkout --ours|--theirs -- <path>`, or `git rm -f -- <path>`.
///
/// `--` before the path always: it stops a path beginning with a dash being
/// read as an option. The newtype already rejects the worst shapes; the
/// separator is what makes that irrelevant rather than load-bearing.
pub fn resolve_conflict_argv(path: &WorktreePath, resolution: Resolution) -> Vec<String> {
    match resolution {
        Resolution::TakeOurs => vec![
            "checkout".to_string(),
            "--ours".to_string(),
            "--".to_string(),
            path.as_str().to_string(),
        ],
        Resolution::TakeTheirs => vec![
            "checkout".to_string(),
            "--theirs".to_string(),
            "--".to_string(),
            path.as_str().to_string(),
        ],
        // `rm` clears the index entries and removes the file in one step; `-f`
        // because a conflicted path is by definition not clean and git refuses
        // without it. It needs no `add` afterwards — and running one on a path
        // it just deleted would fail.
        Resolution::TakeDeletion => vec![
            "rm".to_string(),
            "-f".to_string(),
            "--".to_string(),
            path.as_str().to_string(),
        ],
    }
}

/// `git add -- <path>` — the second half of a take-a-side resolution.
///
/// A checkout writes the working tree but leaves the stage entries in place,
/// so the path stays conflicted until it is staged.
pub fn stage_resolved_path_argv(path: &WorktreePath) -> Vec<String> {
    vec![
        "add".to_string(),
        "--".to_string(),
        path.as_str().to_string(),
    ]
}

/// `git checkout HEAD -- <paths…>` — discard uncommitted changes to tracked
/// paths.
///
/// `checkout HEAD --`, never the bare `checkout --` that resets the worktree
/// to the *index*: a path whose only difference is staged would be a silent
/// no-op under the bare form.
pub fn discard_tracked_argv(paths: &[WorktreePath]) -> Vec<String> {
    let mut argv = vec!["checkout".to_string(), "HEAD".to_string(), "--".to_string()];
    argv.extend(paths.iter().map(|p| p.as_str().to_string()));
    argv
}

/// `git clean -f -- <paths…>` — delete untracked paths.
pub fn delete_untracked_argv(paths: &[WorktreePath]) -> Vec<String> {
    let mut argv = vec!["clean".to_string(), "-f".to_string(), "--".to_string()];
    argv.extend(paths.iter().map(|p| p.as_str().to_string()));
    argv
}

// ---------------------------------------------------------------------------
// What an export can honestly say
// ---------------------------------------------------------------------------

/// One command in an exported plan: the argv, and one line saying why it is
/// there.
///
/// `argv` carries no leading `"git"` — it is exactly what the executor hands
/// to `run_git`, so the two cannot disagree about the program being run, and
/// a renderer targeting something other than a shell (the YAML manifest of
/// #590 slice 3) gets the arguments as data rather than as a parsed string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The git arguments, in order, without the leading `git`.
    pub argv: Vec<String>,
    /// One line, in plain language, saying what this command does and why the
    /// plan contains it. Written for someone reading the printout weeks later.
    pub why: String,
}

impl Step {
    /// Convenience constructor — the two fields are always built together.
    fn new(argv: Vec<String>, why: impl Into<String>) -> Self {
        Step {
            argv,
            why: why.into(),
        }
    }
}

/// What can honestly be printed for one [`GitOperation`].
///
/// Four arms rather than one, because three families of operation have no
/// single command line that is true in advance — see this module's own doc
/// for the finding, and for why guessing is worse than declining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Export {
    /// The commands, in order, exactly as the executor would spawn them.
    Commands(Vec<Step>),
    /// The argv is chosen at execution time from repository state the plan
    /// does not carry.
    ///
    /// Both candidates are named — this is a statement of what *will* be
    /// decided and by what, not a refusal to answer. A reader who knows which
    /// side of `decided_by` they are on can use the matching candidate; a
    /// reader who does not has been told the question exists, which is the
    /// part a guessed single command would have destroyed.
    ChosenAtRunTime {
        /// What the executor reads to decide. Plain language, for a human.
        decided_by: String,
        /// Each possible outcome: the condition under which it runs, and the
        /// commands it runs. Never empty.
        candidates: Vec<Candidate>,
    },
    /// A later command's arguments are an earlier command's *output*, so the
    /// sequence cannot be printed as literals.
    ///
    /// Distinct from [`Self::NotACommandLine`]: these operations are perfectly
    /// expressible in a *script*, with command substitution — which is exactly
    /// what #590 slice 2 is for. The checklist is where they cannot land,
    /// because a checklist is typed by hand.
    Chained {
        /// The shape of the chain, in plain language.
        why: String,
    },
    /// Not expressible as command arguments at any amount of quoting — the
    /// operation's input is bytes on stdin or a file's contents.
    NotACommandLine {
        /// What the operation needs that arguments cannot carry.
        why: String,
    },
}

/// One branch of an [`Export::ChosenAtRunTime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The condition under which this branch is the one that runs, phrased so
    /// a reader can check it themselves ("the branch is the checked-out one").
    pub when: String,
    /// The commands this branch runs.
    pub steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// The classifier
// ---------------------------------------------------------------------------

/// What can be printed for `operation` — the exhaustive, wildcard-free match.
///
/// **No `_ =>` arm, ever.** A new [`GitOperation`] variant must fail to
/// compile here until someone decides whether it has a printable command,
/// exactly as [`crate::effects::network_need_for_operation`] forces a decision
/// about its network need. A wildcard is how a new operation silently acquires
/// a wrong command, and a wrong command in this module is one the user pastes
/// into a terminal.
pub fn export_operation(operation: &GitOperation) -> Export {
    match operation {
        GitOperation::CreateBranch { name, at } => Export::Commands(vec![Step::new(
            create_branch_argv(name, at),
            format!("Create branch ‘{}’ pointing at commit {}.", name, short(at)),
        )]),

        GitOperation::CommitOnHead {
            message,
            allow_empty,
        } => Export::Commands(vec![Step::new(
            commit_on_head_argv(message, *allow_empty),
            if *allow_empty {
                "Commit the staged changes, allowing a commit that changes nothing."
            } else {
                "Commit the staged changes on the checked-out branch."
            },
        )]),

        GitOperation::AmendCommit {
            message,
            expected_tip,
            allow_empty,
        } => Export::Commands(vec![Step::new(
            amend_commit_argv(message, *allow_empty),
            format!(
                "Replace the tip commit {} with a new one carrying this message. \
                 The old commit stays in the reflog.",
                short(expected_tip)
            ),
        )]),

        GitOperation::StageAll => Export::Commands(vec![Step::new(
            stage_all_argv(),
            "Stage every change in the working tree, including new files.",
        )]),

        GitOperation::UnstageAll => Export::Commands(vec![Step::new(
            unstage_all_argv(),
            "Unstage everything. Every edit stays in the working tree.",
        )]),

        GitOperation::CheckoutBranch { branch } => Export::Commands(vec![Step::new(
            checkout_argv(branch),
            format!(
                "Switch to branch ‘{branch}’. Git refuses this itself if it would \
                 overwrite uncommitted work."
            ),
        )]),

        GitOperation::MergeBranch { branch } => Export::Commands(vec![Step::new(
            merge_argv(&RefName::from(branch)),
            format!(
                "Merge ‘{branch}’ into the checked-out branch, taking git's own \
                 merge message rather than opening an editor."
            ),
        )]),

        GitOperation::DeleteBranch { branch } => Export::Commands(vec![Step::new(
            delete_branch_argv(branch, false),
            format!(
                "Delete branch ‘{branch}’. The lower-case -d is the safe one: git \
                 refuses if the branch holds commits that are nowhere else."
            ),
        )]),

        GitOperation::ForceDeleteBranch { branch } => Export::Commands(vec![Step::new(
            delete_branch_argv(branch, true),
            format!(
                "Delete branch ‘{branch}’ even though it holds commits that are \
                 nowhere else. Those commits become unreachable — recoverable from \
                 the reflog until git garbage-collects them, and not after."
            ),
        )]),

        GitOperation::RebaseOntoBase { base } => Export::Commands(vec![Step::new(
            rebase_argv(base),
            format!(
                "Replay the checked-out branch's commits on top of ‘{base}’. Every \
                 replayed commit gets a new id."
            ),
        )]),

        GitOperation::RestoreBranch { name, tip } => Export::Commands(vec![Step::new(
            create_branch_argv(name, tip),
            format!(
                "Re-create the deleted branch ‘{}’ at {}, the tip it had when it was \
                 deleted.",
                name,
                short(tip)
            ),
        )]),

        GitOperation::CherryPick { commit } => Export::Commands(vec![Step::new(
            cherry_pick_argv(commit, None),
            format!(
                "Apply {}'s changes to the checked-out branch as a new commit.",
                short(commit)
            ),
        )]),

        GitOperation::CherryPickMerge { commit, mainline } => Export::Commands(vec![Step::new(
            cherry_pick_argv(commit, Some(*mainline)),
            format!(
                "Apply merge commit {}'s changes as a new commit, measuring them \
                 against parent {} — that is what -m picks.",
                short(commit),
                mainline.get()
            ),
        )]),

        GitOperation::RevertCommit { commit } => Export::Commands(revert_steps(commit, None)),

        GitOperation::RevertMerge { commit, mainline } => {
            Export::Commands(revert_steps(commit, Some(*mainline)))
        }

        GitOperation::FetchRemote { remote } => Export::Commands(vec![Step::new(
            fetch_argv(remote),
            format!(
                "Download new commits from ‘{remote}’. Nothing in the working tree \
                 or on any local branch moves."
            ),
        )]),

        GitOperation::PullBranch {
            remote,
            branch,
            strategy,
        } => {
            let integration = tracking_ref(remote, branch);
            let second = match strategy {
                MergeStrategy::Merge => Step::new(
                    merge_argv(&integration),
                    format!(
                        "Merge the downloaded ‘{integration}’ into the checked-out \
                         branch, keeping both histories and adding a merge commit."
                    ),
                ),
                MergeStrategy::Rebase => Step::new(
                    rebase_argv(&integration),
                    format!(
                        "Replay the checked-out branch's own commits on top of the \
                         downloaded ‘{integration}’. Every replayed commit gets a new id."
                    ),
                ),
            };
            Export::Commands(vec![
                Step::new(
                    fetch_argv(remote),
                    format!("Download new commits from ‘{remote}’ — nothing moves yet."),
                ),
                second,
            ])
        }

        GitOperation::PushBranch {
            branch,
            remote,
            set_upstream,
            force,
        } => Export::Commands(vec![Step::new(
            push_argv(branch, remote, *set_upstream, force),
            match force {
                ForcePublish::None => format!(
                    "Send ‘{branch}’ to ‘{remote}’. This leaves the machine — nothing \
                     local can recall it once other people have fetched it."
                ),
                ForcePublish::WithLease { .. } => format!(
                    "Overwrite ‘{branch}’ on ‘{remote}’, but only if the remote is \
                     still where this plan saw it — that is what the lease checks. \
                     Commits the remote branch held stop being referenced there."
                ),
            },
        )]),

        GitOperation::CreateTag {
            name,
            target,
            annotation,
        } => Export::Commands(vec![Step::new(
            create_tag_argv(name, target, annotation.as_ref()),
            match annotation {
                None => format!(
                    "Tag {} as ‘{}’ — a lightweight tag, which is just a name for a \
                     commit.",
                    short(target),
                    name
                ),
                Some(a) if a.sign => format!(
                    "Tag {} as ‘{}’ with a signed tag object carrying the message. \
                     Signing needs a gpg key this shell can reach.",
                    short(target),
                    name
                ),
                Some(_) => format!(
                    "Tag {} as ‘{}’ with an annotated tag object carrying the message.",
                    short(target),
                    name
                ),
            },
        )]),

        GitOperation::DeleteLocalTag { name } => Export::Commands(vec![Step::new(
            delete_local_tag_argv(name),
            format!("Delete the local tag ‘{name}’. Any remote copy is untouched."),
        )]),

        GitOperation::PushTag { name, remote } => Export::Commands(vec![Step::new(
            push_tag_argv(name, remote),
            format!(
                "Send tag ‘{name}’ to ‘{remote}’. The full refs/tags/ path is spelled \
                 out so a same-named branch on the remote cannot be matched instead."
            ),
        )]),

        GitOperation::DeleteRemoteTag { name, remote } => Export::Commands(vec![Step::new(
            delete_remote_tag_argv(name, remote),
            format!(
                "Delete tag ‘{name}’ on ‘{remote}’. The local tag is untouched, and \
                 anyone who already fetched the tag still has it."
            ),
        )]),

        GitOperation::PushStash {
            message,
            keep_index,
            include_untracked,
        } => Export::Commands(vec![Step::new(
            push_stash_argv(message.as_ref(), *keep_index, *include_untracked),
            {
                let mut why = String::from("Put the working-tree changes aside on the stash");
                if *include_untracked {
                    why.push_str(", including files git is not yet tracking");
                }
                if *keep_index {
                    why.push_str(", leaving the staged version in place");
                }
                why.push('.');
                why
            },
        )]),

        GitOperation::ApplyStash { entry, .. } => Export::Commands(vec![Step::new(
            apply_stash_argv(entry),
            format!(
                "Re-apply the changes held in {entry} to the working tree. The stash \
                 entry stays on the stack."
            ),
        )]),

        GitOperation::BranchFromStash { name, entry, .. } => Export::Commands(vec![Step::new(
            branch_from_stash_argv(name, entry),
            format!(
                "Create branch ‘{name}’ from the commit {entry} was made on, apply the \
                 stashed changes there, and drop the entry if that succeeds."
            ),
        )]),

        GitOperation::DropStash { entry, .. } => Export::Commands(vec![Step::new(
            drop_stash_argv(entry),
            format!(
                "Discard stash entry {entry}. Its commit survives in the reflog until \
                 git garbage-collects it, and not after."
            ),
        )]),

        GitOperation::ResolveConflict { path, resolution } => {
            let mut steps = vec![Step::new(
                resolve_conflict_argv(path, *resolution),
                match resolution {
                    Resolution::TakeOurs => {
                        format!("Resolve {path} by keeping this branch's version whole.")
                    }
                    Resolution::TakeTheirs => {
                        format!("Resolve {path} by keeping the incoming version whole.")
                    }
                    Resolution::TakeDeletion => format!(
                        "Resolve {path} by removing the file — this stages the removal too."
                    ),
                },
            )];
            // `rm` has already cleared the stage entries; the two checkout
            // forms leave the path conflicted until it is staged.
            if !matches!(resolution, Resolution::TakeDeletion) {
                steps.push(Step::new(
                    stage_resolved_path_argv(path),
                    format!(
                        "Mark {path} resolved. Until this runs the file still counts as \
                         conflicted, even though its contents are now correct."
                    ),
                ));
            }
            Export::Commands(steps)
        }

        GitOperation::DiscardTrackedPaths { paths } => Export::Commands(vec![Step::new(
            discard_tracked_argv(paths),
            format!(
                "Throw away the uncommitted edits to {} and restore the committed \
                 version. Staged and unstaged changes alike — this is not undoable.",
                count_paths(paths.len())
            ),
        )]),

        GitOperation::DeleteUntrackedPaths { paths } => Export::Commands(vec![Step::new(
            delete_untracked_argv(paths),
            format!(
                "Delete {} that git has never tracked. Git holds no copy, so this is \
                 not undoable by anything.",
                count_paths(paths.len())
            ),
        )]),

        // --- the three families with no honest single command line ----------
        GitOperation::ResetBranch { branch, to, .. } => Export::ChosenAtRunTime {
            decided_by: format!("whether ‘{branch}’ is the branch you currently have checked out"),
            candidates: vec![
                Candidate {
                    when: format!("‘{branch}’ IS the checked-out branch"),
                    steps: vec![Step::new(
                        move_branch_argv(branch, to),
                        format!(
                            "Move ‘{}’ back to {} and rewrite the working tree to match. \
                             The app refuses this outright if the working tree is dirty, \
                             and so should you — it eats uncommitted work.",
                            branch,
                            short(to)
                        ),
                    )],
                },
                Candidate {
                    when: format!("‘{branch}’ is NOT the checked-out branch"),
                    steps: vec![Step::new(
                        reset_hard_argv(to),
                        format!(
                            "Move the branch label ‘{}’ back to {}. No file is touched.",
                            branch,
                            short(to)
                        ),
                    )],
                },
            ],
        },

        GitOperation::SequenceContinue => sequence_export(
            SequenceVerb::Continue,
            "carry on with the rest of the sequence now that the conflicts are resolved",
        ),
        GitOperation::SequenceSkip => sequence_export(
            SequenceVerb::Skip,
            "abandon the commit currently being applied and move to the next one. The \
             original commit stays in history",
        ),
        GitOperation::SequenceAbort => sequence_export(
            SequenceVerb::Abort,
            "unwind the whole sequence back to where it started. Every conflict \
             resolution made so far in it is discarded",
        ),

        GitOperation::EmptyCommitOnBranch { branch, .. } => Export::Chained {
            why: format!(
                "This writes a commit object with `git commit-tree` and then points \
                 ‘{branch}’ at it with `git update-ref` — and the id to point at is \
                 whatever the first command prints. There is no literal to write down \
                 for the second line, so it cannot be a hand-typed checklist. A \
                 generated script can express it with command substitution."
            ),
        },

        GitOperation::StageSelection { .. } => Export::NotACommandLine {
            why: "This stages a hand-picked selection by feeding a patch to `git apply` \
                  on standard input. The patch is the operation's real content and it \
                  is not an argument, so there is no command line to copy — it would \
                  need the patch written to a file first."
                .to_string(),
        },

        GitOperation::ResolveConflictContent { path, .. } => Export::NotACommandLine {
            why: format!(
                "This resolves {path} by writing the exact file contents you composed \
                 in the editor. Those bytes are the operation; git is only asked to \
                 stage the result afterwards. No argument list can carry a file's \
                 contents."
            ),
        },

        GitOperation::ResetTestRepo => Export::NotACommandLine {
            why: "Resetting the built-in demo repository is a program, not a command: \
                  it unbundles a seed, rewrites every ref to match it, forces the \
                  working tree back, and deletes whatever branches the seed does not \
                  name. It exists to give the app a known fixture and has no meaning \
                  typed by hand."
                .to_string(),
        },
    }
}

/// The two-step revert, shared by [`GitOperation::RevertCommit`] and
/// [`GitOperation::RevertMerge`].
fn revert_steps(commit: &CommitOid, mainline: Option<std::num::NonZeroU8>) -> Vec<Step> {
    let measured = match mainline {
        None => String::new(),
        Some(m) => format!(
            " Because it is a merge, -m {} says which parent to measure the change \
             against.",
            m.get()
        ),
    };
    vec![
        Step::new(
            revert_compute_argv(commit, mainline),
            format!(
                "Work out the opposite of {} and put it in the index, without \
                 committing yet.{}",
                short(commit),
                measured
            ),
        ),
        Step::new(
            revert_commit_argv(),
            "Commit that as its own commit. --allow-empty because a revert whose \
             change is already gone is still a real answer, and the two steps exist \
             precisely so this one can say so. If the first command reported \
             conflicts, resolve them before running this, or run `git revert --abort` \
             to unwind."
                .to_string(),
        ),
    ]
}

/// The three sequencer verbs, which share a shape: the flag is fixed by the
/// operation, and the *subcommand* is whichever sequence the repository is
/// actually in.
fn sequence_export(verb: SequenceVerb, what_it_does: &str) -> Export {
    Export::ChosenAtRunTime {
        decided_by: "which sequence the repository is in the middle of — git keeps one \
                     at a time, and records it as .git/CHERRY_PICK_HEAD or \
                     .git/REVERT_HEAD"
            .to_string(),
        candidates: vec![
            Candidate {
                when: "a cherry-pick is in progress (.git/CHERRY_PICK_HEAD exists)".to_string(),
                steps: vec![Step::new(
                    sequence_argv(SequenceKind::CherryPick, verb),
                    format!("Tell the in-progress cherry-pick to {what_it_does}."),
                )],
            },
            Candidate {
                when: "a revert is in progress (.git/REVERT_HEAD exists)".to_string(),
                steps: vec![Step::new(
                    sequence_argv(SequenceKind::Revert, verb),
                    format!("Tell the in-progress revert to {what_it_does}."),
                )],
            },
        ],
    }
}

/// `<n> file` / `<n> files`, for the two path-list operations' `why` lines.
fn count_paths(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// The first 8 characters of an object id — git's own habit, and what the rest
/// of this crate's user-facing text uses.
fn short(oid: &CommitOid) -> &str {
    let s = oid.as_str();
    &s[..s.len().min(8)]
}

// ---------------------------------------------------------------------------
// Rendering one command for a terminal
// ---------------------------------------------------------------------------

/// A command rendered as text a person can type.
///
/// Two arms because shells disagree about exactly one thing in the range of
/// arguments git-vista can produce — see [`render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    /// One spelling, correct in POSIX `sh`, `bash`, `zsh` and `fish` alike.
    /// Everything git-vista produces lands here unless an argument contains a
    /// single quote.
    Portable(String),
    /// The shells need different escapes for this command, because one of its
    /// arguments contains a single quote.
    ShellSpecific {
        /// Correct in POSIX `sh`, and therefore in bash and zsh.
        posix: String,
        /// Correct in fish, which is Tom's login shell.
        fish: String,
    },
}

impl Rendered {
    /// The spelling to show when only one line fits — the portable one where
    /// there is one, and fish's otherwise, since that is the shell the reader
    /// is standing in.
    pub fn for_fish(&self) -> &str {
        match self {
            Rendered::Portable(line) => line,
            Rendered::ShellSpecific { fish, .. } => fish,
        }
    }
}

/// Characters that make an argument need quoting. Deliberately a small
/// allowlist of what is *safe* rather than a denylist of what is not: a
/// denylist that misses a character produces a command line that silently
/// means something else, and this text is going into a terminal.
fn is_shell_safe(arg: &str) -> bool {
    !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '_' | '-' | '.' | '/' | '=' | ':' | '@' | '^' | '{' | '}' | ','
                )
        })
}

/// Render one step's argv as a `git …` command line.
///
/// # Why quoting is reported rather than chosen
///
/// A single-quoted argument is byte-identical in POSIX `sh` and in fish, so
/// every argument git-vista can produce renders the same in both — commit
/// messages, tag messages and stash messages included — **unless the argument
/// contains a single quote**. There the two diverge and cannot be reconciled:
/// POSIX `sh` has no escape inside single quotes at all and the idiom is to
/// close, escape, and reopen (`'it'\''s'`), while fish does honour a
/// backslash-escaped quote (`'it\'s'`). A renderer that picked one would be
/// silently wrong in the other shell roughly whenever a commit message
/// contains an apostrophe, which is often.
///
/// So the divergence is the return type. `Portable` is a promise; the split
/// arm is the honest alternative to a wrong promise, and it is what #590 slice
/// 2 needs to emit a script with a truthful header.
pub fn render(step: &Step) -> Rendered {
    let mut portable = true;
    let mut posix = String::from("git");
    let mut fish = String::from("git");
    for arg in &step.argv {
        posix.push(' ');
        fish.push(' ');
        if is_shell_safe(arg) {
            posix.push_str(arg);
            fish.push_str(arg);
        } else if arg.contains('\'') {
            portable = false;
            posix.push_str(&format!("'{}'", arg.replace('\'', r"'\''")));
            fish.push_str(&format!("'{}'", arg.replace('\'', r"\'")));
        } else {
            let quoted = format!("'{arg}'");
            posix.push_str(&quoted);
            fish.push_str(&quoted);
        }
    }
    if portable {
        Rendered::Portable(posix)
    } else {
        Rendered::ShellSpecific { posix, fish }
    }
}

// ---------------------------------------------------------------------------
// The printable checklist (#590 slice 1)
// ---------------------------------------------------------------------------

/// Render a whole plan as a numbered checklist a person can print and work
/// through.
///
/// # What the header carries, and why
///
/// #590 asked an open question: *does the export carry the plan's generation
/// token so a stale printed plan can warn?* It does, and it must — a printout
/// is the one form of a plan that outlives the session that made it. The
/// header states the generation the plan was built against and the moment it
/// stops being executable **by the app**, and says the part that is easy to get
/// wrong: those bounds do not restrain a command typed by hand. Git will run
/// what it is given. The generation is what lets a reader ask "is this still
/// the repository I printed this for?" instead of assuming.
///
/// The plan's own preconditions are printed above the commands for the same
/// reason. The executor re-checks them and refuses; a terminal does not. They
/// are the difference between a checklist and a loaded gun, so they go where
/// they cannot be scrolled past.
pub fn checklist(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str("GIT-VISTA — PLAN CHECKLIST\n");
    out.push_str("==========================\n\n");
    out.push_str(&format!(
        "Operation:   {}\n",
        operation_name(&plan.operation)
    ));
    out.push_str(&format!("Risk:        {}\n", risk_word(plan.risk)));
    out.push_str(&format!("Generation:  {}\n", plan.generation.as_str()));
    out.push_str(&format!(
        "Issued:      {} (unix seconds)\n",
        plan.issued_at.0
    ));
    out.push_str(&format!(
        "App expiry:  {} (unix seconds)\n\n",
        plan.expires_at.0
    ));
    out.push_str(
        "This is a printout. The expiry and the generation above are what the APP\n\
         checks before it will run this plan itself — they cannot stop a command you\n\
         type by hand. If the repository has moved on since this was printed, these\n\
         commands may do something other than what they say. Re-generate rather than\n\
         guessing.\n\n",
    );

    if !plan.preconditions.is_empty() {
        out.push_str("CHECK FIRST — the app refuses the plan unless all of these hold:\n");
        for precondition in &plan.preconditions {
            out.push_str(&format!("  [ ] {}\n", describe_precondition(precondition)));
        }
        out.push_str("or exit $status\n\n");
    }

    match export_operation(&plan.operation) {
        Export::Commands(steps) => {
            out.push_str("COMMANDS — in this order:\n\n");
            for (i, step) in steps.iter().enumerate() {
                out.push_str(&format_step(i + 1, step));
            }
        }
        Export::ChosenAtRunTime {
            decided_by,
            candidates,
        } => {
            out.push_str("THIS ONE DEPENDS ON THE REPOSITORY — read before typing.\n\n");
            out.push_str(&format!(
                "The app picks the command when it runs, from {decided_by}.\n\
                 No single command line is printed here because the wrong one would be\n\
                 a real command that does the wrong thing. Check the condition, then use\n\
                 the matching block.\n\n"
            ));
            for candidate in &candidates {
                out.push_str(&format!("IF {}:\n\n", candidate.when));
                for (i, step) in candidate.steps.iter().enumerate() {
                    out.push_str(&format_step(i + 1, step));
                }
            }
        }
        Export::Chained { why } => {
            out.push_str("NOT A HAND-TYPED CHECKLIST\n\n");
            out.push_str(&wrapped(&why));
            out.push('\n');
        }
        Export::NotACommandLine { why } => {
            out.push_str("NOT A COMMAND LINE\n\n");
            out.push_str(&wrapped(&why));
            out.push('\n');
        }
    }

    out.push_str("\nRECOVERY\n\n");
    out.push_str(&wrapped(&describe_recovery(&plan.recovery)));
    out
}

// ---------------------------------------------------------------------------
// The fish script (#590 slice 2)
// ---------------------------------------------------------------------------

/// Why a plan cannot be emitted as a literal fish script.
///
/// A refusal is data rather than an empty/partial script: a caller must show
/// the reason, and can never accidentally save a file that looks runnable but
/// silently omitted the operation's hard half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptUnavailable {
    pub why: String,
}

/// Render a plan as an explicitly fish-targeted, fail-fast script.
///
/// Every `git` line comes from [`render`] over the same [`Step::argv`] the
/// executor's shared builder produced. The script adds only control surface:
/// a fish shebang, one explanatory comment per step, and fish's fail-fast
/// idiom (`or exit $status`) immediately after every command. It never parses
/// or reconstructs a command string.
///
/// Plans without a literal [`Export::Commands`] list are refused. In
/// particular, this function does not guess a runtime-selected argv, invent
/// command substitution for a prior-output chain, or flatten stdin/file bytes
/// into shell syntax.
pub fn fish_script(plan: &Plan) -> Result<String, ScriptUnavailable> {
    let steps = match export_operation(&plan.operation) {
        Export::Commands(steps) => steps,
        Export::ChosenAtRunTime { decided_by, .. } => {
            return Err(ScriptUnavailable {
                why: format!(
                    "the executor chooses this operation's argv at run time from {decided_by}"
                ),
            })
        }
        Export::Chained { why } | Export::NotACommandLine { why } => {
            return Err(ScriptUnavailable { why })
        }
    };

    let mut out = String::new();
    out.push_str("#!/usr/bin/env fish\n");
    out.push_str("# git-vista plan export\n");
    out.push_str("# Shell: fish (this file is not POSIX sh or bash)\n");
    out.push_str("# Fail-fast: each command exits immediately with its own non-zero status.\n");
    out.push_str(&format!("# Generation: {}\n", plan.generation.as_str()));
    out.push_str(&format!(
        "# App expiry: {} (unix seconds)\n",
        plan.expires_at.0
    ));
    out.push_str("# Re-generate if the repository has moved since this plan was built.\n\n");

    for (index, step) in steps.iter().enumerate() {
        let why = step.why.split_whitespace().collect::<Vec<_>>().join(" ");
        let command = match render(step) {
            Rendered::Portable(line) => line,
            Rendered::ShellSpecific { fish, .. } => fish,
        };
        out.push_str(&format!("# Step {}: {why}\n", index + 1));
        out.push_str(&command);
        out.push('\n');
        out.push_str("or exit $status\n\n");
    }
    Ok(out)
}

/// One numbered entry: the command, then the reason, then a checkbox.
fn format_step(n: usize, step: &Step) -> String {
    let mut out = String::new();
    match render(step) {
        Rendered::Portable(line) => {
            out.push_str(&format!("  {n}. {line}\n"));
        }
        Rendered::ShellSpecific { posix, fish } => {
            out.push_str(&format!(
                "  {n}. (this one is spelled differently per shell —\n"
            ));
            out.push_str("      an argument contains a single quote)\n");
            out.push_str(&format!("      fish:      {fish}\n"));
            out.push_str(&format!("      sh / bash: {posix}\n"));
        }
    }
    for line in wrapped(&step.why).lines() {
        out.push_str(&format!("      {line}\n"));
    }
    out.push_str("      [ ] done\n\n");
    out
}

/// Wrap prose at 76 columns so a printed page does not need a wide terminal.
/// Existing newlines are honoured as paragraph breaks.
fn wrapped(text: &str) -> String {
    let mut out = String::new();
    for paragraph in text.split('\n') {
        let mut column = 0usize;
        for word in paragraph.split_whitespace() {
            let width = word.chars().count();
            if column > 0 && column + 1 + width > 76 {
                out.push('\n');
                column = 0;
            } else if column > 0 {
                out.push(' ');
                column += 1;
            }
            out.push_str(word);
            column += width;
        }
        out.push('\n');
    }
    out
}

/// A short plain-language name for the operation, for the checklist header.
///
/// Written out rather than derived from the serde tag, because deriving it
/// would need `serde_json` and this crate keeps that a dev-dependency on
/// purpose — it must stay pure and wasm-safe. Exhaustive and wildcard-free
/// like every other match over this enum, so a new variant stops the build
/// here too rather than printing a tag name at a reader.
pub fn operation_name(operation: &GitOperation) -> &'static str {
    match operation {
        GitOperation::CreateBranch { .. } => "create a branch",
        GitOperation::CommitOnHead { .. } => "commit",
        GitOperation::EmptyCommitOnBranch { .. } => "empty commit on another branch",
        GitOperation::StageAll => "stage everything",
        GitOperation::UnstageAll => "unstage everything",
        GitOperation::CheckoutBranch { .. } => "switch branch",
        GitOperation::MergeBranch { .. } => "merge a branch",
        GitOperation::PushBranch { .. } => "push a branch",
        GitOperation::ResolveConflict { .. } => "resolve a conflict by taking a side",
        GitOperation::ResolveConflictContent { .. } => "resolve a conflict with edited content",
        GitOperation::DeleteBranch { .. } => "delete a branch (safe)",
        GitOperation::ForceDeleteBranch { .. } => "force-delete a branch",
        GitOperation::RebaseOntoBase { .. } => "rebase",
        GitOperation::RestoreBranch { .. } => "restore a deleted branch",
        GitOperation::ResetBranch { .. } => "move a branch back (undo)",
        GitOperation::SequenceContinue => "continue the sequence in progress",
        GitOperation::SequenceSkip => "skip this commit in the sequence",
        GitOperation::SequenceAbort => "abort the sequence in progress",
        GitOperation::CherryPick { .. } => "cherry-pick a commit",
        GitOperation::CherryPickMerge { .. } => "cherry-pick a merge commit",
        GitOperation::RevertCommit { .. } => "revert a commit",
        GitOperation::RevertMerge { .. } => "revert a merge commit",
        GitOperation::ResetTestRepo => "reset the demo repository",
        GitOperation::StageSelection { .. } => "stage a hand-picked selection",
        GitOperation::DiscardTrackedPaths { .. } => "discard changes to tracked files",
        GitOperation::DeleteUntrackedPaths { .. } => "delete untracked files",
        GitOperation::AmendCommit { .. } => "amend the last commit",
        GitOperation::FetchRemote { .. } => "fetch from a remote",
        GitOperation::PullBranch { .. } => "pull",
        GitOperation::CreateTag { .. } => "create a tag",
        GitOperation::DeleteLocalTag { .. } => "delete a local tag",
        GitOperation::DeleteRemoteTag { .. } => "delete a tag on the remote",
        GitOperation::PushTag { .. } => "push a tag",
        GitOperation::PushStash { .. } => "stash changes",
        GitOperation::ApplyStash { .. } => "apply a stash entry",
        GitOperation::BranchFromStash { .. } => "branch from a stash entry",
        GitOperation::DropStash { .. } => "drop a stash entry",
    }
}

/// Plain words for the risk level. The enum's own names are accurate but
/// terse; a printout is read without the UI's colour and ceremony around it.
fn risk_word(risk: crate::plan::RiskLevel) -> &'static str {
    use crate::plan::RiskLevel;
    match risk {
        RiskLevel::Safe => "safe — nothing can be lost",
        RiskLevel::Reversible => "reversible — the app records how to undo this",
        RiskLevel::Destructive => "DESTRUCTIVE — something can become unreachable",
        RiskLevel::Remote => "remote — this leaves your machine and cannot be recalled",
    }
}

/// One line describing a precondition, in the second person, so it reads as
/// something to check rather than something to parse.
fn describe_precondition(precondition: &crate::plan::Precondition) -> String {
    use crate::plan::Precondition;
    match precondition {
        Precondition::RefAt { ref_name, oid } => {
            format!(
                "‘{}’ is still at {} — check with `git rev-parse {}`",
                ref_name,
                short(oid),
                ref_name
            )
        }
        Precondition::RefExists { ref_name } => format!("‘{ref_name}’ exists"),
        Precondition::RefAbsent { ref_name } => format!("‘{ref_name}’ does NOT exist yet"),
        Precondition::BranchCheckedOut { branch } => {
            format!("‘{branch}’ is the checked-out branch")
        }
        Precondition::BranchNotCheckedOut { branch } => {
            format!("‘{branch}’ is NOT the checked-out branch")
        }
        Precondition::CleanWorktree => {
            "the working tree has no uncommitted changes — check with `git status`".to_string()
        }
        Precondition::RemoteConfigured { remote } => {
            format!("a remote named ‘{remote}’ is configured — check with `git remote`")
        }
        Precondition::SeedRecorded => {
            "the demo repository's seed state has been recorded".to_string()
        }
    }
}

/// Plain words for the recovery strategy.
/// Plain words for the recovery strategy.
///
/// # These describe the way back, they do not print it as a command
///
/// Every other command in this document is the executor's own argv, lifted.
/// A recovery is not: the app performs it through `/api/undo`, which builds
/// its *own* plan when the time comes, from the state as it is then. Printing
/// a recovery command line here would be the one place in this module that
/// invented a command rather than reporting one — the exact failure the
/// module exists to prevent, and the most dangerous place to do it, because a
/// recovery is typed by someone who has just watched something go wrong.
///
/// So these say what the way back *is*, and where the material for it lives.
fn describe_recovery(recovery: &crate::plan::RecoveryStrategy) -> String {
    use crate::plan::RecoveryStrategy;
    match recovery {
        RecoveryStrategy::NotNeeded => {
            "Nothing is lost by this, so there is nothing to recover.".to_string()
        }
        RecoveryStrategy::ResetRef { ref_name, to } => format!(
            "The way back is to put ‘{}’ back at {}, which is where it is now. Write \
             that id down before you start — after the fact, `git reflog` is where it \
             lives, and the reflog is local and expires.",
            ref_name,
            short(to)
        ),
        RecoveryStrategy::RecreateBranch { name, at } => format!(
            "The way back is to re-create branch ‘{}’ at {}. Write that id down before \
             you start: once the branch is gone, the reflog is the only thing still \
             naming it, and it expires.",
            name,
            short(at)
        ),
        RecoveryStrategy::DeleteCreatedBranch { name } => format!(
            "This creates branch ‘{name}’. The way back is to delete it again; nothing \
             else is touched."
        ),
        RecoveryStrategy::RecreateTag { name, at } => format!(
            "The way back is to re-create tag ‘{}’ at {}.",
            name,
            short(at)
        ),
        RecoveryStrategy::DeleteCreatedTag { name } => {
            format!("This creates tag ‘{name}’. The way back is to delete it again.")
        }
        RecoveryStrategy::RecreateStashEntry { at, message } => format!(
            "The way back is to re-create the stash entry from commit {}{}. A dropped \
             stash commit survives until git garbage-collects it, and not after.",
            short(at),
            match message {
                Some(m) => format!(" (it was labelled “{}”)", m.as_str()),
                None => String::new(),
            }
        ),
        RecoveryStrategy::CheckoutPrevious { branch } => {
            format!("The way back is to switch to ‘{branch}’ again — the branch you are on now.")
        }
        RecoveryStrategy::RevertCommit { commit } => format!(
            "The way back is to revert {}, which adds a commit undoing it rather than \
             removing it from history.",
            short(commit)
        ),
        RecoveryStrategy::RecoverableIfStaged => {
            "Changes that were STAGED survive as dangling objects until git \
             garbage-collects them, so they can be dug out with `git fsck --unreachable`. \
             Changes that were never staged are gone: git never had a copy."
                .to_string()
        }
        RecoveryStrategy::ConflictRecreatableWhileInProgress => {
            "While the operation that produced the conflict is still in progress, the \
             conflicted state can be re-created — the three sides are still in the \
             index. Once the sequence is finished or aborted, it cannot."
                .to_string()
        }
        RecoveryStrategy::Irrecoverable => {
            "Nothing can undo this. The effect leaves this machine, or it destroys the \
             only copy. Be sure before you run it."
                .to_string()
        }
    }
}
