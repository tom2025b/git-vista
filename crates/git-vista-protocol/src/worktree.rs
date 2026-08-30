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
}
