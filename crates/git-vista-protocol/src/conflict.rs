//! Conflicts, modelled independently of whatever caused them (M4.31, #84).
//!
//! # Why this is its own module and not part of `status`
//!
//! [`crate::status::ConflictKind`] already classifies *that* a path is
//! conflicted, from porcelain-v2's `<XY>` codes. That is the right amount for a
//! status listing: seven variants, one line each, no file contents.
//!
//! Resolving a conflict needs something else entirely — the three versions git
//! is holding, whether each of them exists, and whether any of them is a thing
//! a text editor can even open. Putting that on `StatusEntry::Conflicted` would
//! make every status response carry blob reads nobody asked for.
//!
//! # One model, six operations
//!
//! A merge, a rebase, a cherry-pick, a revert, a stash pop and a pull all
//! produce *the same thing*: index entries at stages 1, 2 and 3, and a working
//! tree with markers in it. Git does not record which operation put them there,
//! and the resolution is identical in every case. So this vocabulary names none
//! of them. An operation-specific conflict type would be six near-copies that
//! drift, and it would push the "which resolver do I use" decision onto every
//! caller for a question that has one answer.
//!
//! # Stages, and why absence is a variant rather than an empty blob
//!
//! Git's index holds up to three versions of a conflicted path:
//!
//! | stage | meaning        | absent when |
//! |-------|----------------|-------------|
//! | 1     | base (common ancestor) | the file was added on both sides — there is no ancestor |
//! | 2     | ours           | we deleted it |
//! | 3     | theirs         | they deleted it |
//!
//! **A missing stage is not an empty file, and the difference decides what the
//! user is looking at.** "The base is empty" means both sides added content to
//! a file that existed and was blank. "There is no base" means the file did not
//! exist before — an add/add conflict, where showing a blank base pane would
//! invent a common ancestor that never existed. [`Stage`] keeps those apart, and
//! keeps a third case apart from both: a stage that could not be read.

use serde::{Deserialize, Serialize};

/// One of the three versions git holds for a conflicted path.
///
/// The three states are deliberate and none of them may collapse into another:
///
/// - [`Present`] — git returned this version's content.
/// - [`Absent`] — git says this stage does not exist, and that is *information*
///   about the conflict's shape, not a failure. An add/add conflict genuinely
///   has no base; a delete/modify genuinely has no ours or theirs.
/// - [`Unreadable`] — the read itself failed. **Never** rendered as an empty
///   pane, because a pane that looks empty tells the user the version was blank
///   when in fact nobody looked.
///
/// The last distinction is the one this codebase keeps paying for elsewhere —
/// see `Advisory::DefaultBranchUnknown` and `drift`'s `NotCheckable`. A
/// resolution UI that cannot tell "there is nothing here" from "I could not
/// look" will invite someone to resolve a conflict against a version they never
/// actually saw.
///
/// [`Present`]: Stage::Present
/// [`Absent`]: Stage::Absent
/// [`Unreadable`]: Stage::Unreadable
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Stage {
    /// This version exists and was read.
    Present {
        /// The blob's object id, so a caller can fetch or cache it
        /// independently of this response.
        oid: crate::plan::CommitOid,
        /// Whether the content is binary. Carried per stage rather than per
        /// file because the sides can genuinely differ — replacing a text file
        /// with a binary one, or the reverse, is a real conflict shape, and a
        /// single per-file flag would have to pick a side and be wrong about
        /// the other.
        binary: bool,
        /// Size in bytes as git reports it. Present even for binary content, so
        /// a caller can decide whether to offer a download without fetching it
        /// first.
        size_bytes: u64,
    },
    /// Git reports no entry at this stage. A fact about the conflict, not an
    /// error — see the module docs' stage table for when each stage is
    /// legitimately absent.
    ///
    /// # Why this carries an empty brace rather than being a unit variant
    ///
    /// `#[serde(deny_unknown_fields)]` is **not enforced for unit variants of
    /// an internally-tagged enum** — serde only applies it to struct variants.
    /// As a bare `Absent`, `{"state":"absent","content":"..."}` would
    /// deserialize happily and silently discard the stray key. `ForcePublish`
    /// documents the same serde behaviour next door.
    ///
    /// That matters more here than almost anywhere: this is the variant that
    /// says *there is nothing on this side*. A body that also carried content
    /// would be self-contradictory, and accepting it quietly is how a resolver
    /// ends up showing a side that the type says does not exist. An empty
    /// struct variant costs nothing on the wire — it still serialises as
    /// `{"state":"absent"}` — and makes the stray key a hard error.
    Absent {},
    /// The read failed. `reason` is for a human; callers must not match on it.
    ///
    /// A caller must render this as an explicit "could not read" state and must
    /// **not** offer it as a resolution choice — choosing a side you were never
    /// shown is the failure this variant exists to prevent.
    Unreadable { reason: String },
}

impl Stage {
    /// Whether this stage can be offered to a user as a resolution choice.
    ///
    /// `Absent` is deliberately *choosable*: "take theirs" when theirs is a
    /// deletion is a legitimate, common resolution, and refusing it would make
    /// delete/modify conflicts unresolvable through the normal path. Only
    /// `Unreadable` is barred, because nobody has seen it.
    pub fn is_choosable(&self) -> bool {
        !matches!(self, Stage::Unreadable { .. })
    }

    /// Whether this stage holds content a text resolver can work with.
    /// `Absent` is not text — it is nothing — and binary content is not text
    /// either.
    pub fn is_text(&self) -> bool {
        matches!(self, Stage::Present { binary: false, .. })
    }
}

/// Why a conflicted path cannot be resolved by picking lines out of a text
/// buffer (M4.31, #84).
///
/// Existing as a *typed reason* rather than a bare "not text" boolean matters:
/// the three cases need visibly different treatment, and a UI that lumps them
/// together will offer a diff view for a rename and a "keep both" button for a
/// binary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum NotTextResolvable {
    /// At least one side is binary. The only honest resolution is choosing a
    /// whole side; there is no meaningful line-level merge.
    Binary {
        /// Which sides were binary, so the UI can say "theirs is an image"
        /// rather than "this file is binary".
        ours: bool,
        theirs: bool,
    },
    /// One or both sides deleted the file. The choice is keep-or-delete, not a
    /// text merge — and the surviving side's content still needs showing so the
    /// decision is informed.
    Deletion {
        /// True when *we* deleted it (`DeletedByUs`/`BothDeleted`).
        ours_deleted: bool,
        /// True when *they* deleted it (`DeletedByThem`/`BothDeleted`).
        theirs_deleted: bool,
    },
    /// The file was renamed on one side and modified on the other, so the two
    /// sides do not even agree on the path.
    ///
    /// **Detection is a caller's job and is not attempted here.** Git's index
    /// does not record rename information for conflicts — rename detection is a
    /// diff-time heuristic, not stored state. This variant exists so a caller
    /// that *has* done that work has somewhere honest to put it, rather than
    /// this type quietly implying a capability it does not have.
    Rename {
        /// The path on our side.
        ours_path: String,
        /// The path on their side.
        theirs_path: String,
    },
}

/// One conflicted path, with everything a resolver needs and nothing about the
/// operation that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictedFile {
    /// Repository-relative path, as git reports it.
    pub path: String,
    /// The porcelain-v2 classification, reused rather than re-derived so a
    /// status listing and a resolver can never disagree about what kind of
    /// conflict a path has.
    pub kind: crate::status::ConflictKind,
    /// Stage 1 — the common ancestor.
    pub base: Stage,
    /// Stage 2 — our side.
    pub ours: Stage,
    /// Stage 3 — their side.
    pub theirs: Stage,
    /// Set when this path cannot be resolved by picking text. `None` means a
    /// normal text conflict; it does **not** mean "resolvable", since an
    /// `Unreadable` stage can still block resolution.
    pub not_text_resolvable: Option<NotTextResolvable>,
}

impl ConflictedFile {
    /// Whether every side a user might choose was actually readable.
    ///
    /// False when any stage is [`Stage::Unreadable`]. A caller must not present
    /// a resolution UI for such a file — one of its panes would be a hole, and
    /// the user would be choosing between versions they have not all seen.
    pub fn all_sides_readable(&self) -> bool {
        self.base.is_choosable() && self.ours.is_choosable() && self.theirs.is_choosable()
    }

    /// Whether a line-level text resolver may open on this path at all
    /// (M4.31c/d, #430/#432, ADR 0069).
    ///
    /// Lives here rather than being computed separately by the client
    /// (rendering) and the server (executing the resolution) so both ask the
    /// SAME question. Two independent copies of this exact three-clause rule
    /// is how #430 shipped a wrong sentence for an hour: the client and the
    /// scanner each held their own idea of "resolvable" and one of them was
    /// subtly wrong. There is exactly one definition now.
    ///
    /// A typed reason (any `NotTextResolvable`) rules it out outright; failing
    /// that, both live sides must actually be text — `not_text_resolvable`
    /// is the server's own classification and should already agree, but a
    /// stage's `is_text()` is the per-side ground truth and wins if they ever
    /// disagree, the same direction `PaneState::with_content`'s binary-wins
    /// rule already takes.
    pub fn text_resolvable(&self) -> bool {
        self.not_text_resolvable.is_none() && self.ours.is_text() && self.theirs.is_text()
    }
}

/// The answer to "may this operation continue?" (M4.31, #84).
///
/// A dedicated type rather than a `bool` because the two blocking cases are
/// **different** and a caller must treat them differently: paths a human still
/// has to decide, versus paths this application could not even read. Collapsing
/// them into "not clear yet" would let a UI tell someone to resolve a file it
/// cannot show them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Continuation {
    /// No conflicted paths remain. The operation may continue.
    Clear,
    /// Conflicts remain and must be resolved first.
    Blocked {
        /// Paths still awaiting a human decision.
        unresolved: Vec<String>,
        /// Paths where at least one side could not be read. Separate from
        /// `unresolved` because the user cannot fix these by choosing — they
        /// are a fault to report, not work to do.
        unreadable: Vec<String>,
    },
}

impl Continuation {
    /// Build the verdict from the conflicted files a scan found.
    ///
    /// **An empty input means `Clear`, and that is only safe because the caller
    /// is required to have actually looked.** A scan that failed must never
    /// reach here with an empty vector — it must surface its own failure — or
    /// this returns a green light meaning "I did not check". The one caller in
    /// `git-vista-server` propagates read failures rather than defaulting to an
    /// empty list, and any future caller must do the same.
    pub fn from_files(files: &[ConflictedFile]) -> Self {
        if files.is_empty() {
            return Continuation::Clear;
        }
        let mut unresolved = Vec::new();
        let mut unreadable = Vec::new();
        for f in files {
            if f.all_sides_readable() {
                unresolved.push(f.path.clone());
            } else {
                unreadable.push(f.path.clone());
            }
        }
        Continuation::Blocked {
            unresolved,
            unreadable,
        }
    }

    /// Whether the operation may proceed. Exactly one state permits it, and
    /// this exists so no caller writes `!matches!(.., Blocked { .. })` and gets
    /// the polarity wrong.
    pub fn may_continue(&self) -> bool {
        matches!(self, Continuation::Clear)
    }
}

/// How one conflicted path is to be resolved (M4.31, #84).
///
/// # Whole sides only, in this slice
///
/// Every variant here takes one side *entirely*. Line- and hunk-level
/// resolution — #84's "block and line choices" — is deliberately absent,
/// because it means carrying file content through a [`crate::Plan`], and a
/// plan is hashed, reviewed and replayed. That is the `patch_plan` machinery's
/// problem (it already solved it for staging selections) and it deserves its
/// own decision rather than being smuggled in beside three simple choices.
///
/// # `TakeDeletion` is separate from "take the side that deleted it"
///
/// In a delete/modify conflict, taking the deleting side and deleting the file
/// are the same outcome — but they are *different requests*, and only one of
/// them stays correct if the caller is wrong about which side deleted what.
/// `TakeOurs` on a path where ours is absent means "I want what ours has,
/// which is nothing"; `TakeDeletion` means "I want this file gone" regardless.
/// Keeping them apart means a caller that has misread the conflict gets a
/// refusal rather than a silent deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Resolution {
    /// Keep our side's content exactly, discarding theirs.
    TakeOurs,
    /// Keep their side's content exactly, discarding ours.
    TakeTheirs,
    /// Resolve by removing the file, whatever either side held.
    TakeDeletion,
}

/// Why a [`Resolution`] cannot be applied to a given [`ConflictedFile`].
///
/// A value rather than an error string: refusing is a normal outcome here, and
/// a caller is expected to render each case differently.
/// The tag is `"refusal"`, not `"reason"`: a variant already carries a
/// `reason` field, and serde refuses an internal tag that collides with a
/// field name rather than silently shadowing it — caught at compile time,
/// which is the right place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ResolutionRefused {
    /// The chosen side could not be read, so choosing it would mean accepting
    /// content nobody has seen. The one refusal that is about *this
    /// application's* failure rather than the user's request.
    SideUnreadable {
        /// `"ours"` or `"theirs"`. An owned `String` rather than a
        /// `&'static str` because this crosses the wire and must deserialize
        /// as well as serialize.
        side: String,
        reason: String,
    },
    /// The chosen side does not exist. Taking a side that is not there is not
    /// the same as deleting the file — see [`Resolution::TakeDeletion`] — so
    /// this is refused rather than quietly reinterpreted.
    SideAbsent { side: String },
}

impl ConflictedFile {
    /// Whether `resolution` may be applied to this file, and why not if not.
    ///
    /// Pure, so the same answer is available before a plan is built and again
    /// before it executes, without a second look at the repository.
    pub fn refuses(&self, resolution: Resolution) -> Option<ResolutionRefused> {
        let (side, stage) = match resolution {
            // Deleting needs neither side to be readable: the request does not
            // depend on what either side holds.
            Resolution::TakeDeletion => return None,
            Resolution::TakeOurs => ("ours", &self.ours),
            Resolution::TakeTheirs => ("theirs", &self.theirs),
        };
        match stage {
            Stage::Present { .. } => None,
            Stage::Absent {} => Some(ResolutionRefused::SideAbsent { side: side.into() }),
            Stage::Unreadable { reason } => Some(ResolutionRefused::SideUnreadable {
                side: side.into(),
                reason: reason.clone(),
            }),
        }
    }
}

/// The document served to seed a content resolution's editor (M4.31c, #432,
/// ADR 0069): the working-tree marker file exactly as `GET
/// /api/worktree-file/{*path}` already serves it, plus the `conflict-v1:`
/// token that pins it.
///
/// ADR 0069's decision: the editor seeds from THIS file — the same bytes
/// every terminal merge tool works from — rather than composing text from the
/// three stage panes. The cost of that choice is that this is the one
/// document no existing staleness mechanism can see: porcelain v2's unmerged
/// lines carry the three stage OIDs but no worktree hash, and the index
/// checksum does not cover worktree bytes either. `source` is what closes
/// that gap — the executor re-mints it from the live file before writing
/// anything, and refuses on any mismatch, the same two-phase shape
/// `diff-v1:` already uses for staging selections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictSource {
    /// Repository-relative path, exactly as requested.
    pub path: String,
    /// The marker file's text. Empty when `binary` is set.
    pub content: String,
    /// True when the content was cut at the server's size cap.
    pub truncated: bool,
    /// True when the file isn't text (NUL bytes near the start).
    pub binary: bool,
    /// The `conflict-v1:` token this exact document was served under. Echo
    /// it back unchanged in a resolution submission; the executor refuses if
    /// it no longer matches the live file.
    pub source: crate::plan::GenerationToken,
}

/// Why a submitted content resolution was refused, in words a user reads
/// (M4.31c, #432, ADR 0069).
///
/// Three distinct facts, not one generic "stale" message, for the same reason
/// [`ResolutionRefused`] is not a bool: each names a different thing that
/// moved, and only one of them is "someone else got there first" — the other
/// two are "the picture changed under you" in different ways.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ContentResolutionRefused {
    /// The path is no longer conflicted — resolved already, or the operation
    /// that produced the conflict ended.
    NoLongerConflicted,
    /// Not eligible for a line-level resolution — binary, deletion-shaped, or
    /// a side that could not be read. Mirrors
    /// [`ConflictedFile::text_resolvable`] and
    /// [`ConflictedFile::all_sides_readable`]; carried as a sentence rather
    /// than re-deriving which rule fired, since the caller only needs to know
    /// why, not which internal check tripped.
    NotTextResolvable { reason: String },
    /// One or more of base/ours/theirs no longer matches the stage the user
    /// resolved against — the picture the choice was made against has
    /// changed, whatever the surviving bytes now say.
    StagesMoved,
    /// The `conflict-v1:` token no longer matches the one the document was
    /// served under.
    ///
    /// **Deliberately does not name a cause, and the first version of this
    /// did.** It said the file "was edited elsewhere" — which the code cannot
    /// know. `conflict_source_token` folds the marker-file bytes *and* the
    /// whole repository generation (HEAD, every ref, the index checksum) into
    /// one opaque digest, and `GenerationInputs::generation()` hashes those
    /// fields together so no per-field attribution survives. A mismatch can
    /// therefore mean an unrelated branch moved, a fetch landed, or a
    /// different file was staged — none of which touched this path.
    ///
    /// Worse, by the time this can fire, [`StagesMoved`] has already proven
    /// this path's own stage OIDs unchanged, so the "someone edited your file"
    /// reading is the *least* likely of the remaining causes. Asserting it
    /// would be the exact ADR 0063 failure this vocabulary exists to prevent:
    /// stating as fact something never observed.
    ///
    /// [`StagesMoved`]: ContentResolutionRefused::StagesMoved
    SourceMoved,
}

impl ContentResolutionRefused {
    /// The sentence a user reads.
    pub fn describe(&self, path: &str) -> String {
        match self {
            ContentResolutionRefused::NoLongerConflicted => format!(
                "{path} is not conflicted — it may have been resolved already, or the \
                 operation that produced the conflict may have ended"
            ),
            ContentResolutionRefused::NotTextResolvable { reason } => {
                format!("{path} cannot be resolved as text — {reason}")
            }
            ContentResolutionRefused::StagesMoved => format!(
                "{path} changed since you opened it — the version you resolved against is no \
                 longer current. Reopen it and try again."
            ),
            // Says WHAT was observed (the document is no longer the one served)
            // and what to do, never WHY — see the variant's own doc comment for
            // why naming a cause here would be a false statement.
            ContentResolutionRefused::SourceMoved => format!(
                "The repository changed while you were resolving {path}, so your changes were \
                 not applied — reopen it to see the current version."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::ConflictKind;

    fn oid(c: char) -> crate::plan::CommitOid {
        crate::plan::CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn present() -> Stage {
        Stage::Present {
            oid: oid('a'),
            binary: false,
            size_bytes: 12,
        }
    }

    fn file(path: &str, base: Stage, ours: Stage, theirs: Stage) -> ConflictedFile {
        ConflictedFile {
            path: path.into(),
            kind: ConflictKind::BothModified,
            base,
            ours,
            theirs,
            not_text_resolvable: None,
        }
    }

    #[test]
    fn an_absent_stage_is_still_a_choice_but_an_unreadable_one_is_not() {
        // MUTATION: make Absent unchoosable. Every delete/modify conflict
        // becomes unresolvable through the normal path, because "take theirs"
        // where theirs is a deletion is the correct and common resolution.
        assert!(Stage::Absent {}.is_choosable());
        assert!(present().is_choosable());
        assert!(!Stage::Unreadable {
            reason: "permission denied".into()
        }
        .is_choosable());
    }

    #[test]
    fn absent_is_not_text_and_neither_is_binary() {
        // MUTATION: treat Absent as text. A resolver would then open a text
        // editor on a side that does not exist and present it as empty.
        assert!(present().is_text());
        assert!(!Stage::Absent {}.is_text());
        assert!(!Stage::Unreadable { reason: "x".into() }.is_text());
        assert!(!Stage::Present {
            oid: oid('b'),
            binary: true,
            size_bytes: 900,
        }
        .is_text());
    }

    #[test]
    fn a_file_with_an_unreadable_side_is_not_fully_readable() {
        let f = file(
            "a.txt",
            present(),
            Stage::Unreadable {
                reason: "blob missing".into(),
            },
            present(),
        );
        assert!(!f.all_sides_readable());

        // Absent must NOT make a file unreadable — an add/add conflict has no
        // base and is perfectly resolvable.
        let add_add = file("b.txt", Stage::Absent {}, present(), present());
        assert!(add_add.all_sides_readable());
    }

    #[test]
    fn no_conflicts_means_clear() {
        let c = Continuation::from_files(&[]);
        assert_eq!(c, Continuation::Clear);
        assert!(c.may_continue());
    }

    #[test]
    fn unresolved_and_unreadable_paths_are_reported_separately() {
        // THE test in this file. MUTATION: put every conflicted path into
        // `unresolved`. A UI would then tell the user to go and resolve a file
        // one of whose sides it cannot show them — asking for a decision it
        // has made impossible.
        let files = vec![
            file("needs-a-human.txt", present(), present(), present()),
            file(
                "cannot-read.bin",
                present(),
                Stage::Unreadable {
                    reason: "blob missing".into(),
                },
                present(),
            ),
        ];
        match Continuation::from_files(&files) {
            Continuation::Blocked {
                unresolved,
                unreadable,
            } => {
                assert_eq!(unresolved, vec!["needs-a-human.txt".to_string()]);
                assert_eq!(unreadable, vec!["cannot-read.bin".to_string()]);
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn any_conflict_at_all_blocks_continuation() {
        // MUTATION: return Clear when only `unreadable` is non-empty. An
        // operation would continue over files nothing could even look at.
        let only_unreadable = vec![file(
            "x.bin",
            Stage::Unreadable { reason: "y".into() },
            present(),
            present(),
        )];
        assert!(!Continuation::from_files(&only_unreadable).may_continue());
    }

    #[test]
    fn every_shape_survives_a_json_round_trip() {
        // The whole vocabulary crosses the wire; a variant that serialises but
        // does not come back is a resolver that silently loses a side.
        let f = ConflictedFile {
            path: "src/a.rs".into(),
            kind: ConflictKind::BothAdded,
            base: Stage::Absent {},
            ours: present(),
            theirs: Stage::Unreadable {
                reason: "gone".into(),
            },
            not_text_resolvable: Some(NotTextResolvable::Binary {
                ours: false,
                theirs: true,
            }),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<ConflictedFile>(&json).unwrap(), f);

        let c = Continuation::Blocked {
            unresolved: vec!["a".into()],
            unreadable: vec!["b".into()],
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Continuation>(&json).unwrap(), c);
    }

    #[test]
    fn a_stray_key_in_a_stage_is_refused() {
        // Same posture as every other body in this contract: a stray key is a
        // hard error, never a silently-ignored value.
        let stray = serde_json::json!({
            "state": "absent",
            "why": "deleted",
        });
        assert!(
            serde_json::from_value::<Stage>(stray).is_err(),
            "a stray key beside `absent` must be refused — this is why Absent is \
             an empty STRUCT variant and not a unit variant; serde does not \
             enforce deny_unknown_fields on unit variants of an internally \
             tagged enum"
        );

        // The plain form still works, and still costs nothing extra on the wire.
        assert_eq!(
            serde_json::from_value::<Stage>(serde_json::json!({"state": "absent"})).unwrap(),
            Stage::Absent {}
        );
        assert_eq!(
            serde_json::to_value(Stage::Absent {}).unwrap(),
            serde_json::json!({"state": "absent"}),
            "the empty struct variant must not add a field to the wire form"
        );
    }

    #[test]
    fn taking_a_side_that_is_absent_is_refused_not_reinterpreted_as_a_deletion() {
        // THE test for this vocabulary. MUTATION: return None for Absent, so
        // "take ours" on a path we deleted silently becomes a deletion. A
        // caller that misread which side deleted what would then destroy the
        // surviving side while believing it kept one.
        let f = ConflictedFile {
            path: "a.txt".into(),
            kind: ConflictKind::DeletedByUs,
            base: present(),
            ours: Stage::Absent {},
            theirs: present(),
            not_text_resolvable: None,
        };
        assert_eq!(
            f.refuses(Resolution::TakeOurs),
            Some(ResolutionRefused::SideAbsent {
                side: "ours".into()
            })
        );
        // Theirs is present, so taking theirs is fine...
        assert_eq!(f.refuses(Resolution::TakeTheirs), None);
        // ...and deleting is always expressible, whatever the sides hold.
        assert_eq!(f.refuses(Resolution::TakeDeletion), None);
    }

    #[test]
    fn taking_an_unreadable_side_is_refused_and_names_which() {
        // MUTATION: allow Unreadable. The user would accept content this
        // application never managed to read, having been shown nothing.
        let f = ConflictedFile {
            path: "a.bin".into(),
            kind: ConflictKind::BothModified,
            base: present(),
            ours: present(),
            theirs: Stage::Unreadable {
                reason: "blob missing".into(),
            },
            not_text_resolvable: None,
        };
        match f.refuses(Resolution::TakeTheirs) {
            Some(ResolutionRefused::SideUnreadable { side, reason }) => {
                assert_eq!(side, "theirs");
                assert!(
                    reason.contains("blob missing"),
                    "reason must survive: {reason}"
                );
            }
            other => panic!("expected SideUnreadable, got {other:?}"),
        }
        assert_eq!(f.refuses(Resolution::TakeOurs), None);
    }

    #[test]
    fn a_deletion_is_expressible_even_when_neither_side_can_be_read() {
        // Deleting does not depend on what either side holds, so it must stay
        // available as the escape hatch when everything else is refused.
        let f = ConflictedFile {
            path: "x".into(),
            kind: ConflictKind::BothModified,
            base: Stage::Unreadable { reason: "a".into() },
            ours: Stage::Unreadable { reason: "b".into() },
            theirs: Stage::Unreadable { reason: "c".into() },
            not_text_resolvable: None,
        };
        assert_eq!(f.refuses(Resolution::TakeDeletion), None);
        assert!(f.refuses(Resolution::TakeOurs).is_some());
        assert!(f.refuses(Resolution::TakeTheirs).is_some());
    }

    #[test]
    fn the_resolution_vocabulary_round_trips() {
        for r in [
            Resolution::TakeOurs,
            Resolution::TakeTheirs,
            Resolution::TakeDeletion,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Resolution>(&json).unwrap(), r);
        }
        assert_eq!(
            serde_json::to_value(Resolution::TakeTheirs).unwrap(),
            serde_json::json!({"choice": "take_theirs"})
        );
        // The tag is `refusal`, not `reason` — a field already uses that name.
        assert_eq!(
            serde_json::to_value(ResolutionRefused::SideAbsent {
                side: "ours".into()
            })
            .unwrap(),
            serde_json::json!({"refusal": "side_absent", "side": "ours"})
        );
    }

    // ---- text_resolvable (M4.31c/d, #430/#432) ----------------------------

    #[test]
    fn an_ordinary_text_conflict_is_text_resolvable() {
        // The negative control. Without it, every assertion below could pass
        // on an implementation that returned false unconditionally.
        let f = file(
            "a.txt",
            present(),
            present(),
            Stage::Present {
                oid: oid('c'),
                binary: false,
                size_bytes: 12,
            },
        );
        assert!(f.text_resolvable());
    }

    #[test]
    fn a_typed_not_text_resolvable_reason_blocks_text_resolution() {
        // MUTATION: ignore `not_text_resolvable` and check only the stages.
        // A server that classified a conflict as Binary or Deletion, but whose
        // stages happen to both still read as text (a deletion's surviving
        // side, say), would then be offered a line-level resolver anyway.
        let mut f = file(
            "a.txt",
            present(),
            present(),
            Stage::Present {
                oid: oid('c'),
                binary: false,
                size_bytes: 12,
            },
        );
        f.not_text_resolvable = Some(NotTextResolvable::Deletion {
            ours_deleted: false,
            theirs_deleted: true,
        });
        assert!(!f.text_resolvable());
    }

    #[test]
    fn a_binary_side_blocks_text_resolution_even_with_no_typed_reason() {
        // Defence in depth: the per-side stage truth wins even if the typed
        // reason and the stages ever disagree — same direction
        // `PaneState::with_content`'s binary-wins rule already takes.
        let f = file(
            "a.txt",
            present(),
            present(),
            Stage::Present {
                oid: oid('c'),
                binary: true,
                size_bytes: 900,
            },
        );
        assert!(!f.text_resolvable());
    }

    #[test]
    fn an_absent_side_blocks_text_resolution() {
        // An add/add or delete/modify conflict's Absent side is not text —
        // there is nothing there to merge lines against.
        let f = file("a.txt", Stage::Absent {}, present(), Stage::Absent {});
        assert!(!f.text_resolvable());
    }

    // ---- ContentResolutionRefused::describe --------------------------------

    #[test]
    fn every_content_refusal_names_the_path_and_says_something_distinct() {
        // MUTATION: collapse all four arms to one generic "cannot be resolved"
        // string. Four different facts would then read identically, which is
        // exactly the undifferentiated-refusal failure #429's own handler doc
        // exists to prevent one layer up.
        let refusals = [
            ContentResolutionRefused::NoLongerConflicted,
            ContentResolutionRefused::NotTextResolvable {
                reason: "binary content".into(),
            },
            ContentResolutionRefused::StagesMoved,
            ContentResolutionRefused::SourceMoved,
        ];
        let mut seen = std::collections::HashSet::new();
        for r in &refusals {
            let text = r.describe("src/a.rs");
            assert!(text.contains("src/a.rs"), "{text}");
            assert!(seen.insert(text), "two refusals produced the same sentence");
        }
    }

    #[test]
    fn conflict_source_round_trips_and_refuses_a_stray_key() {
        let src = ConflictSource {
            path: "a.txt".into(),
            content: "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> theirs\n".into(),
            truncated: false,
            binary: false,
            source: crate::plan::GenerationToken::new("conflict-v1:deadbeef").unwrap(),
        };
        let json = serde_json::to_value(&src).unwrap();
        let back: ConflictSource = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(src, back);

        let mut stray = json;
        stray["extra"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<ConflictSource>(stray).is_err(),
            "a stray key must be refused, not silently dropped"
        );
    }
}
