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
    /// row this application could not derive a stable identity for. `reason`
    /// is for a human reading a diagnostic; nothing downstream may treat this
    /// as evidence about any branch or any sibling.
    CensusFailed { reason: String },
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
        };
        let empty = WorktreeCensus::Observed { siblings: vec![] };
        assert_ne!(failed, empty);

        let failed_json = serde_json::to_value(failed).unwrap();
        let empty_json = serde_json::to_value(empty).unwrap();
        assert_ne!(failed_json, empty_json);
        assert_eq!(failed_json["kind"], "census_failed");
        assert_eq!(empty_json["kind"], "observed");
    }
}
