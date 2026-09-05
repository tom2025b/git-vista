//! The worktree census wire contract (M11.01, #546).
//!
//! `docs/superpowers/specs/m3.23-worktrees.md` §1 designs the shape this
//! module implements: a read-only enumeration of a repository's linked
//! worktrees, built from `git worktree list --porcelain` and carrying two
//! kinds of fact that must never be folded into one:
//!
//! - [`WorktreeSibling::locked`] / [`WorktreeSibling::prunable`] are **git's
//!   own flags**, read verbatim from the porcelain stream.
//! - [`WorktreeSibling::serviceable`] is **this application's separate
//!   fence** — whether the sibling's path lies inside an allowed root, is
//!   real, or is a phantom the working directory has already vanished from.
//!
//! "git says this worktree is locked" and "this application refuses to open
//! it" are different sentences and different offers to the user; a single
//! `usable: bool` would make both impossible to say. See [`Serviceable`]'s
//! own doc for why it has three states rather than two, and
//! `git-vista-server`'s `worktree_census` module for the query that builds
//! this type from a live repository.
//!
//! # The parser lives here too, beside the types it produces
//!
//! [`parse_worktree_porcelain`] turns `git worktree list --porcelain`'s
//! stdout into [`WorktreeListRecord`]s — a pure `&str -> Result<Vec<_>>`
//! function with no filesystem and no process, exactly like
//! [`crate::status::parse_porcelain_v2_z`] and
//! [`crate::diff::parse_unified_diff`] next door. That is this codebase's
//! established split for a read-side feature: the pure parse is a protocol
//! concern (it *is* the wire contract with git), and the server keeps only
//! what needs the machine — the spawn, the identity lookup, and the
//! allowed-roots fence. `git-vista-server::handlers::read` calls
//! `parse_porcelain_v2_z` rather than owning a status parser of its own, and
//! `worktree_census` calls this one on the same terms.
//!
//! # Why the 2.32 floor rules out `-z`
//!
//! `git worktree list --porcelain` has a `-z` form that NUL-terminates
//! records instead of newline-terminating them, which is the safer contract
//! for a path that could contain a literal newline. It is not used here:
//! git-scm's manual for 2.31 documents `list`, `--porcelain`, and
//! `-v`/`--verbose` and says nothing about `-z` at all; 2.32 has no distinct
//! page of its own on git-scm.com (its URL redirects to 2.31's); the
//! *current* manual documents `-z`. Taken together, `-z` was added to
//! `worktree list` at some later version, after this project's documented
//! git floor (`docs/SUPPORTED_VERSIONS.md`, "Git: 2.32 or later") — so it
//! isn't used here. Parsing the newline-terminated form inherits git's own
//! limitation at that floor: a worktree path containing a literal newline
//! cannot be parsed unambiguously. That is a fact about the porcelain
//! contract at the supported floor, not a defect introduced by
//! [`parse_worktree_porcelain`].
//!
//! The one place that limitation could bite silently — quoting — does not
//! apply to anything this module keeps. The manual documents that *only* the
//! lock reason is quoted/escaped (`core.quotePath`-style) when it contains
//! unusual characters and `-z` is not used; [`WorktreeSibling`] has no reason
//! field (the spec's struct doesn't carry one, and nothing here needs it), so
//! the parser only ever needs to recognise the `locked`/`prunable` label
//! itself, never interpret the escaping of the text after it.
//!
//! # Why this crate, not `git-vista-core`
//!
//! Every id here is the **opaque string form** of a `git-vista-core` id
//! (`RepositoryId`/`WorktreeId`), exactly like
//! [`RepositoryDescriptor`](crate::RepositoryDescriptor): this crate does not
//! depend on `git-vista-core` (see the crate doc's dependency diagram), so the
//! wire shape cannot smuggle a domain type across the transport boundary. Only
//! the native backend ever holds the path an id was derived from.

use serde::{Deserialize, Serialize};

use crate::plan::{BranchName, CommitOid};

/// Whether a discovered [`WorktreeSibling`] can actually be opened by this
/// application — a question **independent of** whether git itself considers
/// the worktree healthy ([`WorktreeSibling::locked`]/`prunable` answer that
/// one).
///
/// # Three states, not two
///
/// `docs/superpowers/specs/m3.23-worktrees.md` §1 ("The security interaction")
/// rejects the two-state version by name: hiding a sibling outside the
/// allowed roots leaves the branch-collision check with a blind spot (it
/// would say a branch is free when a worktree it refused to look at holds
/// it), and silently widening the allowed roots to cover it defeats the fence
/// entirely. The only honest answer is a third state: **discovered,
/// real, and refused, with the reason** — the same shape this codebase has
/// already reached for four times this month for an unrelated fact
/// (`Advisory::DefaultBranchUnknown` next door in [`crate::plan`],
/// `HeadState::Unborn` in [`crate::history`], and the `Obs`/`RecoveryClass`
/// families in `git-vista-server`): a state nobody chose not to check, that
/// is nonetheless not a green light.
///
/// # `Missing` is not `OutsideAllowedRoots`
///
/// A `prunable` sibling whose working directory is gone cannot be
/// meaningfully tested against the allowed roots at all — canonicalising a
/// path that no longer exists does not produce evidence either way. Folding
/// "gone" into "refused for policy reasons" would tell the user the wrong
/// story (there is nothing here to open a fence around) and would tell a
/// collision check the wrong thing too (a `Missing` sibling cannot hold a
/// branch checkout the way a live one can — see the spec's collision-check
/// section for how a future consumer is expected to read this).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Serviceable {
    /// Inside an allowed root. Selecting this sibling would work.
    Yes,
    /// Discovered and real, but its canonical path lies outside every
    /// allowed root. Still counted for anything that only needs to know
    /// *what branch is checked out where* (a collision check); refused for
    /// anything that would open or mutate it.
    OutsideAllowedRoots,
    /// git reports this sibling `prunable` and its working directory could
    /// not be opened — the desk itself is gone, distinct from a real desk
    /// this application merely declines to open.
    Missing,
}

impl Serviceable {
    /// Whether this application may open this sibling — the one place the
    /// three-state answer becomes the yes/no a button needs (M11.03, #548).
    ///
    /// Spelled as a method rather than left to each call site's `matches!`,
    /// so a caller cannot write `!matches!(s, Serviceable::Missing)` and
    /// thereby treat a fenced-off worktree as openable.
    pub fn is_openable(&self) -> bool {
        matches!(self, Self::Yes)
    }

    /// Why this sibling cannot be opened, in the words a person reads —
    /// `None` exactly when [`Self::is_openable`] is `true`.
    ///
    /// # One sentence, two consumers, on purpose
    ///
    /// The server refuses `POST /api/select-worktree` with this text, and the
    /// drawer renders the same text beside the row *before* anyone taps it.
    /// Those must not be two sentences maintained in two crates: M11.02's
    /// `collision_refusal` earned that rule the hard way, and the failure mode
    /// here is worse, because the drawer's copy is the one a user reads while
    /// deciding whether to tap at all.
    ///
    /// It is deliberately a **stated fence**, never a silent omission and
    /// never a bare greyed-out control: `docs/superpowers/specs/m3.23-worktrees.md`
    /// §1 weighs hiding a refused sibling and rejects it — "a wrong answer
    /// produced by a deliberate omission is the worst of the three".
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Yes => None,
            Self::OutsideAllowedRoots => {
                Some("This worktree is outside the folders you allowed, so it cannot be opened.")
            }
            Self::Missing => Some(
                "This worktree's folder is gone from disk, though git still holds its entry. \
                 Run git worktree prune to release the branch it is holding.",
            ),
        }
    }
}

/// One worktree of the repository being served, as reported by
/// `git worktree list --porcelain` (M11.01, #546) — the app's own working
/// tree ([`is_current`](Self::is_current)) or one of its linked siblings.
///
/// # Why `head`/`branch` are `Option`, not always-present
///
/// A freshly `git worktree add`ed sibling can carry an **unborn** branch — no
/// commit yet, so no real object for [`head`](Self::head) to name. Porcelain
/// spells this as `HEAD 000…0`, git's null-oid sentinel, but that value names
/// no object; passing it through as a [`CommitOid`] would claim a commit
/// exists where none does. [`crate::history::HeadState::Unborn`] is the exact
/// same fact about the *current* worktree's HEAD, and the same reasoning
/// applies here: `None`, not a fabricated oid. `branch` is `None` for a
/// detached HEAD (a normal, healthy state) and, for the same reason, for a
/// `bare` record (see [`bare`](Self::bare)) — neither has anything to name.
///
/// # `bare` — a third git-native flag the design spec didn't anticipate
///
/// `git worktree list --porcelain`, run from a linked worktree of a
/// **bare-hub** layout (a bare repository plus one or more linked worktrees —
/// verified by hand, not assumed: `git init --bare hub.git`, then
/// `git worktree add` a sibling, then list from inside the sibling), reports
/// the bare directory itself as its own record: `worktree <path>` followed by
/// a lone `bare` line, no `HEAD`, no `branch`. That is a **third boolean git
/// hands over directly**, on the same footing as `locked` and `prunable` —
/// folding it away (dropping the row, or reporting it as an ordinary detached
/// worktree with no HEAD) is exactly the "never fold a real git flag into
/// something it isn't" mistake this module's own doc opens with. So it gets
/// its own field rather than being inferred from `branch`/`head` both being
/// absent, which is also what a corrupt read would look like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSibling {
    /// Opaque id of the shared repository (its common git directory). Every
    /// sibling in one census carries the same value — this is `git worktree
    /// list`'s whole premise — included per-row anyway so a client can
    /// confirm it without a second field to keep in sync, the same posture
    /// [`crate::RepositoryDescriptor`] takes.
    pub repository: String,
    /// Opaque id of this specific worktree — stable across restarts,
    /// path-independent, and (for a live sibling) exactly what
    /// [`crate::RepositoryDescriptor::worktree`] would report if this sibling
    /// were itself the served repository.
    pub id: String,
    /// A short, non-path display label (the directory's base name), safe to
    /// show without revealing where on disk the sibling lives.
    pub name: String,
    /// The absolute filesystem path — omitted (`None`) unless the operator
    /// opted into path exposure (`GIT_VISTA_EXPOSE_PATHS`), identically to
    /// [`crate::RepositoryDescriptor::path`]. Never sent by default.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// The branch checked out here, or `None` for a detached HEAD or a
    /// `bare` record (see the struct doc).
    pub branch: Option<BranchName>,
    /// The commit HEAD resolves to, or `None` for an unborn branch or a
    /// `bare` record (see the struct doc).
    pub head: Option<CommitOid>,
    /// Whether this row is the worktree currently being served. Exactly one
    /// row in an [`WorktreeCensus::Observed`] list carries `true` — the
    /// query that builds this refuses to answer at all rather than emit a
    /// census with zero or with more than one.
    pub is_current: bool,
    /// git's own lock flag, read verbatim — independent of
    /// [`serviceable`](Self::serviceable). A locked sibling inside the
    /// allowed roots is still `Serviceable::Yes`; locking is git's business
    /// (it refuses `worktree remove`/`prune`), not this application's.
    pub locked: bool,
    /// git's own prunable flag, read verbatim. See [`Serviceable::Missing`]
    /// for what this implies about `serviceable` — and what it does not
    /// (a `prunable` sibling whose directory can still be opened, e.g. an
    /// `--expire`-style staleness reason, is reported with its real
    /// `serviceable` value, not forced to `Missing`).
    pub prunable: bool,
    /// Whether this record is the repository's own bare administrative
    /// directory rather than a working tree — see the struct doc.
    pub bare: bool,
    /// Whether this application can open this sibling — the fence, kept
    /// deliberately separate from `locked`/`prunable` above.
    pub serviceable: Serviceable,
}

/// The outcome of one worktree-enumeration read (M11.01, #546):
/// `git worktree list --porcelain` was read and understood, or it wasn't.
///
/// # Why this is its own type, and not a bare `Vec` that happens to be empty
/// on failure
///
/// `docs/superpowers/specs/m3.23-worktrees.md` §1 ("the enumeration ITSELF is
/// fallible") states the hazard directly: a failed `git worktree list`
/// (spawn error, non-zero exit, a porcelain line the parser does not
/// understand) that silently became `vec![]` would read downstream as "no
/// conflicting checkout anywhere" — fail-**open**, from the one event that
/// established nothing. [`CensusFailed`](Self::CensusFailed) exists so
/// nothing built on top of this can make that mistake: it is a distinct
/// variant, not a value that compares equal to an empty, healthy
/// [`Observed`](Self::Observed).
///
/// This is the same `Known`/`Absent`/`Unknown` split
/// `git-vista-server::planner`'s private `Obs<T>` type makes for a single
/// git read, generalised to a read that produces a list: `Observed` (even an
/// empty one) is a fact about the repository, `CensusFailed` is the absence
/// of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeCensus {
    /// The list was read, parsed, and resolved. Possibly a single entry (a
    /// repository with no linked worktrees) — that is a real, reportable
    /// observation, not a failure.
    Observed { siblings: Vec<WorktreeSibling> },
    /// `git worktree list --porcelain` could not be run, exited non-zero,
    /// printed something the parser does not understand, or resolved to a
    /// row this application could not derive a stable identity for. The two
    /// fields are for a human reading a diagnostic; nothing downstream may
    /// treat this as evidence about any branch or any sibling.
    ///
    /// # Why the diagnostic is split in two (#657)
    ///
    /// [`WorktreeSibling::path`] honours `GIT_VISTA_EXPOSE_PATHS`; a single
    /// `reason` string did not, and every failure this variant carries is
    /// serialized to the client on `GET /api/worktrees`, on
    /// `POST /api/select-worktree`, and (through
    /// [`BranchHolder::Unknown`]) in the planner's collision refusal. So the
    /// flag was right on the arm everyone tests and absent from the arm
    /// nobody does. Redacting the whole string instead would have cost the
    /// diagnosability this variant exists to provide, which is the real
    /// trade-off ADR 0119 weighs; splitting keeps both.
    ///
    /// The invariant a client may rely on: **`reason` is identical whether or
    /// not the operator opted in.** The flag adds `detail`; it never rewrites
    /// `reason`.
    CensusFailed {
        /// The always-client-safe half. Composed only of literals the census
        /// writes itself plus values from a closed set proven path-free
        /// (counts, base names, ref names, byte ceilings) — never a string
        /// that arrived from git, from `gix`, or from a parser, since any of
        /// those can name an absolute path.
        reason: String,
        /// The path-bearing half — omitted (`None`) unless the operator
        /// opted into path exposure (`GIT_VISTA_EXPOSE_PATHS`), identically
        /// to [`WorktreeSibling::path`]. Always written to the server's own
        /// log regardless of the flag, so nothing is ever lost, only
        /// withheld.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        detail: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Parsing `git worktree list --porcelain` (no `-z` — see the module doc)
// ---------------------------------------------------------------------------

/// One fully-parsed `git worktree list --porcelain` record, before identity
/// resolution. Every field here is **exactly what git printed** — no
/// filesystem access, no fence check, no interpretation. Turning one of these
/// into a [`WorktreeSibling`] is the native server's job: it needs the
/// filesystem (to derive a [`WorktreeSibling::id`] and to decide
/// [`Serviceable`]) and this crate has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListRecord {
    /// The `worktree <path>` line's value, verbatim. A `String`, not a
    /// `PathBuf`: this crate is wasm-safe and touches no filesystem, so the
    /// bytes git printed stay bytes until the native server turns them into
    /// a path.
    pub path: String,
    /// The `HEAD <oid>` line's value, verbatim — **including** git's
    /// all-zero null-oid sentinel for an unborn branch, which this parser
    /// deliberately does not normalise away (that is the caller's decision;
    /// see [`WorktreeSibling::head`]). `None` only for a `bare` record.
    pub head_hex: Option<String>,
    /// The `branch <ref>` line's value, verbatim and **unstripped** —
    /// `refs/heads/main`, not `main`. Whether a non-`refs/heads/` ref is
    /// acceptable is a contract question for the caller, not a parse
    /// question. `None` for `detached` and for `bare`.
    pub branch_ref: Option<String>,
    /// The `detached` line was present.
    pub detached: bool,
    /// The `bare` line was present — see [`WorktreeSibling::bare`].
    pub bare: bool,
    /// The `locked` line was present (its optional reason text is discarded;
    /// see [`WorktreeSibling::locked`]).
    pub locked: bool,
    /// The `prunable` line was present (its optional reason text is
    /// discarded; see [`WorktreeSibling::prunable`]).
    pub prunable: bool,
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
pub fn parse_worktree_porcelain(text: &str) -> Result<Vec<WorktreeListRecord>, String> {
    let mut records = Vec::new();
    let mut current: Option<RecordBuilder> = None;

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
            current = Some(RecordBuilder::new(rest.to_string()));
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
struct RecordBuilder {
    path: String,
    head_hex: Option<String>,
    branch_ref: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
}

impl RecordBuilder {
    fn new(path: String) -> Self {
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
                    return Err(format!("`{}` has more than one `HEAD` line", self.path));
                }
                self.head_hex = Some(value.to_string());
            }
            "branch" => {
                let value = rest.ok_or_else(|| "`branch` line has no value".to_string())?;
                if self.head_shape_taken() {
                    return Err(format!(
                        "`{}`'s `branch` line conflicts with an earlier \
                         `branch`/`detached`/`bare` line",
                        self.path
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
                        self.path
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
                        self.path
                    ));
                }
                self.bare = true;
            }
            "locked" => {
                if self.locked {
                    return Err(format!("`{}` has more than one `locked` line", self.path));
                }
                self.locked = true;
                // The reason (`rest`) is discarded on purpose — `WorktreeSibling`
                // carries no reason field (see the protocol module's doc), so
                // there is nothing here that needs the `-z`/quoting distinction
                // the manual documents for that text.
            }
            "prunable" => {
                if self.prunable {
                    return Err(format!("`{}` has more than one `prunable` line", self.path));
                }
                self.prunable = true;
            }
            other => return Err(format!("unrecognised worktree-list attribute `{other}`")),
        }
        Ok(())
    }

    fn finish(self) -> Result<WorktreeListRecord, String> {
        if self.bare {
            if self.head_hex.is_some() {
                return Err(format!(
                    "`{}` is `bare` but also carries a `HEAD` line",
                    self.path
                ));
            }
        } else if self.head_hex.is_none() {
            return Err(format!(
                "`{}` has no `HEAD` line and is not `bare`",
                self.path
            ));
        } else if !self.detached && self.branch_ref.is_none() {
            return Err(format!(
                "`{}` names neither a `branch` nor `detached`",
                self.path
            ));
        }
        Ok(WorktreeListRecord {
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

// ---------------------------------------------------------------------------
// The collision question (M11.02, #547)
// ---------------------------------------------------------------------------

/// What a [`WorktreeCensus`] says about one branch being free to check out
/// **here** — the answer behind
/// [`crate::plan::Precondition::BranchFreeInEveryOtherWorktree`].
///
/// Three values, not two, because the census has three states and the middle
/// one must survive the trip. See [`branch_holder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchHolder<'a> {
    /// The census was read and no other worktree has the branch checked out.
    /// The checkout may be offered.
    Free,
    /// Another worktree has it. Git will refuse the checkout; this is the
    /// worktree to name, and (when it is [`Serviceable::Yes`]) the one to
    /// offer to open instead.
    HeldBy(&'a WorktreeSibling),
    /// The census could not be read, so nothing is known about this branch or
    /// any other. Carries [`WorktreeCensus::CensusFailed`]'s reason.
    ///
    /// **This is not `Free`.** A caller that folds it into either of the
    /// other two is re-introducing the exact fail-open the census type exists
    /// to prevent: an unread enumeration contains no conflicting checkout, so
    /// "no conflict found" and "nobody looked" are the same bytes unless a
    /// type keeps them apart.
    Unknown(&'a str),
}

/// Resolve [`BranchHolder`] for `branch` from `census` — the single place a
/// census becomes an answer about a branch, shared by the server's
/// precondition verification and by the UI's decision whether to offer the
/// checkout button at all.
///
/// # What counts as a holder, and what does not
///
/// * **The current worktree does not count.** The precondition is about every
///   *other* worktree; a branch already checked out here is a no-op checkout,
///   which is a different message and a different (already existing) check.
/// * **A sibling outside the allowed roots counts.** git's refusal does not
///   care about this application's fence, so a worktree it may not *open*
///   still holds the branch and still makes the checkout fail. Hiding it here
///   would produce a wrong answer by deliberate omission — the worst of the
///   three options `docs/superpowers/specs/m3.23-worktrees.md` §1 weighs.
/// * **A [`Serviceable::Missing`] sibling counts too.** Its directory is gone
///   but its administrative entry is not, and git keeps refusing the branch
///   until someone prunes it. The refusal a user meets is real, so the
///   precondition must see it.
/// * A `bare` record holds no branch (`branch: None`) and so can never match.
///
/// Git itself makes more than one holder impossible, so the first match is
/// the answer; if a future git ever listed two, naming one of them is still
/// strictly better than naming none.
pub fn branch_holder<'a>(census: &'a WorktreeCensus, branch: &BranchName) -> BranchHolder<'a> {
    match census {
        // `reason`, never `detail`: this string is relayed into a refusal
        // message the client reads, and `reason` is the half that carries no
        // path whether or not the operator opted in (#657).
        WorktreeCensus::CensusFailed { reason, .. } => BranchHolder::Unknown(reason.as_str()),
        WorktreeCensus::Observed { siblings } => siblings
            .iter()
            .find(|s| !s.is_current && s.branch.as_ref() == Some(branch))
            .map_or(BranchHolder::Free, BranchHolder::HeldBy),
    }
}

impl BranchHolder<'_> {
    /// Whether the checkout may be offered — true for [`Self::Free`] alone.
    ///
    /// Spelled as a method rather than left to each call site's `matches!`,
    /// because the one mistake this whole type exists to prevent is a call
    /// site that writes `!matches!(h, BranchHolder::HeldBy(_))` and thereby
    /// treats [`Self::Unknown`] as permission.
    pub fn permits_checkout(&self) -> bool {
        matches!(self, Self::Free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(s: &str) -> BranchName {
        BranchName::new(s).unwrap()
    }

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn sibling() -> WorktreeSibling {
        WorktreeSibling {
            repository: "repo-1".to_string(),
            id: "worktree-1".to_string(),
            name: "Git-Vista".to_string(),
            path: None,
            branch: Some(branch("main")),
            head: Some(oid('a')),
            is_current: true,
            locked: false,
            prunable: false,
            bare: false,
            serviceable: Serviceable::Yes,
        }
    }

    /// Round-trip every [`Serviceable`] variant — the crate's stated
    /// convention (see the module doc of e.g. `plan.rs`) for every wire type.
    #[test]
    fn serviceable_round_trips_every_variant() {
        for value in [
            Serviceable::Yes,
            Serviceable::OutsideAllowedRoots,
            Serviceable::Missing,
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let back: Serviceable = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    /// `Serviceable`'s wire tag, pinned literally: a client matches on this
    /// string, so a refactor that silently renames a variant must fail here
    /// rather than only in an integration the client-side isn't part of.
    #[test]
    fn serviceable_wire_tag_is_stable() {
        assert_eq!(
            serde_json::to_string(&Serviceable::Yes).unwrap(),
            r#"{"kind":"yes"}"#
        );
        assert_eq!(
            serde_json::to_string(&Serviceable::OutsideAllowedRoots).unwrap(),
            r#"{"kind":"outside_allowed_roots"}"#
        );
        assert_eq!(
            serde_json::to_string(&Serviceable::Missing).unwrap(),
            r#"{"kind":"missing"}"#
        );
    }

    #[test]
    fn worktree_sibling_round_trips() {
        let s = sibling();
        let json = serde_json::to_string(&s).unwrap();
        let back: WorktreeSibling = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    /// `path` is omitted from the wire entirely when `None` — the same
    /// leak-nothing-by-default posture as `RepositoryDescriptor::path`, and
    /// the reason this is asserted on the raw JSON rather than only through
    /// a round trip (a round trip alone cannot tell "omitted" from "sent as
    /// null").
    #[test]
    fn absent_path_is_omitted_not_sent_as_null() {
        let json = serde_json::to_value(sibling()).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("path"),
            "path must be absent from the wire when None, not present as null: {json}"
        );
    }

    #[test]
    fn unborn_and_detached_and_bare_round_trip_as_none() {
        let mut s = sibling();
        s.branch = None;
        s.head = None;
        s.bare = true;
        let json = serde_json::to_string(&s).unwrap();
        let back: WorktreeSibling = serde_json::from_str(&json).unwrap();
        assert_eq!(back.branch, None);
        assert_eq!(back.head, None);
        assert!(back.bare);
    }

    #[test]
    fn census_observed_round_trips_including_empty() {
        let census = WorktreeCensus::Observed { siblings: vec![] };
        let json = serde_json::to_string(&census).unwrap();
        let back: WorktreeCensus = serde_json::from_str(&json).unwrap();
        assert_eq!(census, back);

        let census = WorktreeCensus::Observed {
            siblings: vec![sibling()],
        };
        let json = serde_json::to_string(&census).unwrap();
        let back: WorktreeCensus = serde_json::from_str(&json).unwrap();
        assert_eq!(census, back);
    }

    /// An empty `Observed` and a `CensusFailed` must never compare equal or
    /// share a wire shape — this is the entire reason the type exists (see
    /// the module doc). Pinned as an explicit test, not left to be implied by
    /// the enum derive.
    #[test]
    fn census_failed_is_not_an_empty_observed() {
        let failed = WorktreeCensus::CensusFailed {
            reason: "spawn failed".to_string(),
            detail: None,
        };
        let empty = WorktreeCensus::Observed { siblings: vec![] };
        assert_ne!(failed, empty);

        let failed_json = serde_json::to_value(failed).unwrap();
        let empty_json = serde_json::to_value(empty).unwrap();
        assert_ne!(failed_json, empty_json);
        assert_eq!(failed_json["kind"], "census_failed");
        assert_eq!(empty_json["kind"], "observed");
    }

    /// `detail` is omitted from the wire entirely when `None`, exactly as
    /// [`WorktreeSibling::path`] is — the leak-nothing-by-default posture
    /// #657 exists to extend to this arm. Asserted on the raw JSON because a
    /// round trip alone cannot tell "omitted" from "sent as null".
    #[test]
    fn absent_census_detail_is_omitted_not_sent_as_null() {
        let json = serde_json::to_value(WorktreeCensus::CensusFailed {
            reason: "spawn failed".to_string(),
            detail: None,
        })
        .unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("detail"),
            "detail must be absent from the wire when None, not present as null: {json}"
        );

        let json = serde_json::to_value(WorktreeCensus::CensusFailed {
            reason: "spawn failed".to_string(),
            detail: Some("/home/someone/secret".to_string()),
        })
        .unwrap();
        assert_eq!(json["detail"], "/home/someone/secret");
    }

    /// The refusal a client reads is built from `reason`, never from
    /// `detail`. `BranchHolder::Unknown` is relayed verbatim into the
    /// planner's collision refusal (`git-vista-server::planner`), which is
    /// the third route #657's finding did not name — and the reason the fix
    /// lives in the value rather than in each route.
    #[test]
    fn branch_holder_unknown_relays_the_safe_reason_not_the_detail() {
        let census = WorktreeCensus::CensusFailed {
            reason: "the worktree list could not be read".to_string(),
            detail: Some("/home/someone/private/repo".to_string()),
        };
        match branch_holder(&census, &branch("feature/x")) {
            BranchHolder::Unknown(r) => {
                assert_eq!(r, "the worktree list could not be read");
                assert!(
                    !r.contains("/home/someone"),
                    "the path-bearing half must never reach a refusal message: {r}"
                );
            }
            other => panic!("a failed census is Unknown, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // `parse_worktree_porcelain` — moved here with the parser it exercises
    // (it was `git-vista-server::worktree_census`'s until M11.01 landed the
    // pure half in this crate, beside `status.rs`/`diff.rs`'s parsers).
    // -----------------------------------------------------------------------

    #[test]
    fn parses_a_well_formed_multi_record_stream() {
        let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\n\nworktree /tmp/side\nHEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nbranch refs/heads/feature\nlocked reason with spaces\nprunable\n\n";
        let records = parse_worktree_porcelain(text).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "/tmp/main");
        assert_eq!(records[0].branch_ref.as_deref(), Some("refs/heads/main"));
        assert!(!records[0].locked);
        assert!(records[1].locked);
        assert!(records[1].prunable);
    }

    #[test]
    fn tolerates_a_missing_trailing_blank_line() {
        let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ndetached";
        let records = parse_worktree_porcelain(text).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].detached);
    }

    #[test]
    fn a_bare_record_has_no_head_or_branch() {
        let text = "worktree /tmp/hub.git\nbare\n";
        let records = parse_worktree_porcelain(text).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].bare);
        assert_eq!(records[0].head_hex, None);
    }

    #[test]
    fn an_attribute_before_any_worktree_line_is_an_error() {
        let text = "HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn a_second_worktree_line_without_a_blank_terminator_is_an_error() {
        let text = "worktree /tmp/main\nworktree /tmp/side\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn an_unrecognized_attribute_is_an_error_not_a_skip() {
        let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\nsomething-new value\n";
        let err = parse_worktree_porcelain(text).unwrap_err();
        assert!(err.contains("something-new"));
    }

    #[test]
    fn branch_and_detached_together_is_an_error() {
        let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\ndetached\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn a_non_bare_record_missing_head_is_an_error() {
        let text = "worktree /tmp/main\nbranch refs/heads/main\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn a_record_naming_neither_branch_nor_detached_is_an_error() {
        let text = "worktree /tmp/main\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn a_bare_record_carrying_head_is_an_error() {
        let text = "worktree /tmp/hub.git\nbare\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        assert!(parse_worktree_porcelain(text).is_err());
    }

    #[test]
    fn empty_input_parses_to_no_records() {
        // The caller (`git-vista-server`'s `worktree_census`) is what turns
        // zero records into a `CensusFailed` — the parser itself just reports
        // what it saw.
        assert_eq!(parse_worktree_porcelain("").unwrap().len(), 0);
    }

    // -----------------------------------------------------------------
    // `branch_holder` — the collision question (M11.02, #547)
    // -----------------------------------------------------------------

    /// A sibling that is *not* the current worktree, on `on`.
    fn other(name: &str, on: Option<&str>, serviceable: Serviceable) -> WorktreeSibling {
        WorktreeSibling {
            name: name.to_string(),
            id: format!("worktree-{name}"),
            branch: on.map(branch),
            is_current: false,
            serviceable,
            ..sibling()
        }
    }

    fn observed(siblings: Vec<WorktreeSibling>) -> WorktreeCensus {
        WorktreeCensus::Observed { siblings }
    }

    #[test]
    fn a_branch_no_other_worktree_holds_is_free() {
        let census = observed(vec![
            sibling(),
            other("desk-two", Some("feature/x"), Serviceable::Yes),
        ]);
        let holder = branch_holder(&census, &branch("feature/y"));
        assert_eq!(holder, BranchHolder::Free);
        assert!(holder.permits_checkout());
    }

    #[test]
    fn a_branch_held_by_a_sibling_names_that_sibling() {
        let census = observed(vec![
            sibling(),
            other("desk-two", Some("feature/x"), Serviceable::Yes),
        ]);
        match branch_holder(&census, &branch("feature/x")) {
            BranchHolder::HeldBy(s) => assert_eq!(s.name, "desk-two"),
            other => panic!("expected the sibling to be named, got {other:?}"),
        }
        assert!(!branch_holder(&census, &branch("feature/x")).permits_checkout());
    }

    /// The precondition is about every **other** worktree. A branch checked
    /// out here is a no-op checkout — a different message, decided elsewhere —
    /// and reporting it as a collision would make every branch you are
    /// standing on look occupied by a stranger.
    #[test]
    fn the_current_worktrees_own_branch_is_not_a_collision() {
        // `sibling()` is `is_current: true` on `main`.
        let census = observed(vec![sibling()]);
        assert_eq!(branch_holder(&census, &branch("main")), BranchHolder::Free);
    }

    /// git's refusal does not consult this application's allowed-roots fence.
    /// A worktree it may not open still holds the branch.
    #[test]
    fn a_sibling_outside_the_allowed_roots_still_holds_the_branch() {
        let census = observed(vec![
            sibling(),
            other(
                "outside",
                Some("feature/x"),
                Serviceable::OutsideAllowedRoots,
            ),
        ]);
        match branch_holder(&census, &branch("feature/x")) {
            BranchHolder::HeldBy(s) => assert_eq!(s.name, "outside"),
            other => panic!("a fenced-off worktree still blocks the checkout, got {other:?}"),
        }
    }

    /// A prunable worktree whose directory is gone keeps its administrative
    /// entry, and git keeps refusing the branch until somebody prunes it.
    #[test]
    fn a_missing_sibling_still_holds_the_branch() {
        let census = observed(vec![
            sibling(),
            other("ghost", Some("feature/x"), Serviceable::Missing),
        ]);
        match branch_holder(&census, &branch("feature/x")) {
            BranchHolder::HeldBy(s) => assert_eq!(s.name, "ghost"),
            other => panic!("a missing worktree still blocks the checkout, got {other:?}"),
        }
    }

    /// The fail-open this whole type exists to prevent: an unread census
    /// contains no conflicting checkout, so "nobody looked" must not arrive
    /// at a call site wearing `Free`'s clothes.
    #[test]
    fn a_failed_census_is_never_read_as_a_free_branch() {
        let census = WorktreeCensus::CensusFailed {
            reason: "`git worktree list --porcelain` failed: no such command".to_string(),
            detail: None,
        };
        let holder = branch_holder(&census, &branch("feature/x"));
        assert!(
            matches!(holder, BranchHolder::Unknown(r) if r.contains("no such command")),
            "expected the failure reason to survive, got {holder:?}"
        );
        assert!(
            !holder.permits_checkout(),
            "an unread census must never permit the checkout"
        );
    }

    /// A bare record names no branch, so it can never be mistaken for a
    /// holder — including of a branch whose name is the empty-ish default a
    /// careless `unwrap_or_default` would produce.
    #[test]
    fn a_bare_record_holds_no_branch() {
        let mut bare = other("hub.git", None, Serviceable::Yes);
        bare.bare = true;
        bare.head = None;
        let census = observed(vec![sibling(), bare]);
        assert_eq!(
            branch_holder(&census, &branch("main-2")),
            BranchHolder::Free
        );
    }

    // -----------------------------------------------------------------
    // `Serviceable`'s user-facing half (M11.03, #548)
    // -----------------------------------------------------------------

    /// Exactly the openable variant has no refusal, and every refused one has
    /// a real sentence. A `Some("")` would compile and read as done.
    #[test]
    fn every_refused_variant_says_why_and_the_openable_one_does_not() {
        assert!(Serviceable::Yes.is_openable());
        assert_eq!(Serviceable::Yes.refusal(), None);
        for refused in [Serviceable::OutsideAllowedRoots, Serviceable::Missing] {
            assert!(!refused.is_openable(), "{refused:?}");
            let why = refused
                .refusal()
                .unwrap_or_else(|| panic!("{refused:?} refuses without saying why"));
            assert!(why.len() > 20, "{refused:?} says only {why:?}");
            assert!(
                why.ends_with('.'),
                "{refused:?} says {why:?} — not a sentence"
            );
        }
    }

    /// The fence sentence the issue names, pinned literally. It is read by a
    /// person deciding whether to tap, and it is asserted by the browser
    /// suite, so a reword is a deliberate edit in both places rather than a
    /// silent drift that leaves the spec failing for a reason nobody expects.
    #[test]
    fn the_fence_sentence_is_the_one_the_issue_names() {
        assert_eq!(
            Serviceable::OutsideAllowedRoots.refusal(),
            Some("This worktree is outside the folders you allowed, so it cannot be opened.")
        );
    }

    /// The two refusals must not read alike: they are different states with
    /// different remedies, and a user who cannot tell them apart has been
    /// given one "unusable" badge wearing two hats — the exact failure this
    /// issue's second acceptance criterion forbids.
    #[test]
    fn the_two_refusals_are_not_the_same_sentence() {
        assert_ne!(
            Serviceable::OutsideAllowedRoots.refusal(),
            Serviceable::Missing.refusal()
        );
        assert!(
            Serviceable::Missing
                .refusal()
                .expect("missing refuses")
                .contains("prune"),
            "the missing case must name the remedy that actually releases the branch"
        );
    }
}
