//! The four panes of a conflict view, and the state each one is in (M4.31a,
//! #428).
//!
//! Framework-free and host-tested — see this module's parent for why that
//! placement is load-bearing rather than tidy.
//!
//! # Six pane states, and why none of them may collapse
//!
//! ADR 0063 established three states per *stage* and spent most of its
//! reasoning on keeping [`Stage::Absent`] apart from an empty blob. This
//! module is where that distinction either survives contact with a renderer
//! or quietly dies, so it carries the same discipline one layer out.
//!
//! | state | what the user is being told |
//! |---|---|
//! | [`PaneState::Absent`] | there is nothing on this side |
//! | [`PaneState::Unreadable`] | this side exists but could not be read |
//! | [`PaneState::Binary`] | this side is not text; there is nothing to show |
//! | [`PaneState::AwaitingContent`] | this side is text and is being fetched |
//! | [`PaneState::Text`] | here is the content |
//! | [`PaneState::ContentUnavailable`] | the content fetch itself failed |
//!
//! **`Absent`, `Unreadable` and `ContentUnavailable` are three different
//! facts and a renderer must not show any of them as an empty text pane.**
//! An empty pane reads as "this version was blank", which is a claim about
//! the repository. Only `Text { content: "" }` may make that claim.
//!
//! `Binary` is separate from `AwaitingContent` for a reason the wire alone
//! does not force: a binary stage has an oid and could be fetched, but
//! fetching it would produce bytes no text pane can render, so the pane
//! stops at the metadata it already has. `Stage::Present { binary, .. }`
//! carries that flag per side precisely because the sides can differ.

use git_vista_core::diff::{BlobContent, WorktreeFileContent};
use git_vista_protocol::conflict::{ConflictedFile, NotTextResolvable, Stage};
use git_vista_protocol::status::ConflictKind;

/// Which of the four views a pane is.
///
/// An enum rather than four loose fields so a caller cannot render three
/// panes and silently omit the fourth — #428's first acceptance criterion is
/// that all four are *reachable*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Stage 1 — the common ancestor.
    Base,
    /// Stage 2 — our side.
    Ours,
    /// Stage 3 — their side.
    Theirs,
    /// The working tree's own copy: what git actually wrote, markers and all.
    Result,
}

impl Pane {
    /// Every pane, in the order a viewer lays them out.
    pub const ALL: [Pane; 4] = [Pane::Base, Pane::Ours, Pane::Theirs, Pane::Result];

    /// The pane's heading.
    ///
    /// `Result` says "read-only" in its own label rather than relying on a
    /// caller to remember: #428's decision comment settled that the result
    /// pane ships read-only and **labelled as such**, and a label a renderer
    /// has to add separately is a label a renderer can forget. Editing
    /// arrives in #429, which is where this string changes.
    pub fn label(&self) -> &'static str {
        match self {
            Pane::Base => "Base",
            Pane::Ours => "Ours",
            Pane::Theirs => "Theirs",
            Pane::Result => "Result (read-only)",
        }
    }
}

/// What one pane must render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneState {
    /// Git holds no version on this side. **Never an empty text pane** — an
    /// add/add conflict genuinely has no base, and drawing a blank one
    /// invents an ancestor that never existed (ADR 0063).
    Absent,
    /// The stage could not be read. `reason` is for a human.
    Unreadable { reason: String },
    /// This side is not text. Carries the size so a viewer can offer a
    /// download decision without fetching the bytes first.
    Binary { size_bytes: u64 },
    /// Text, with content still to fetch from `GET /api/blob/{oid}`.
    AwaitingContent { oid: String },
    /// The content, and whether the server cut it at its cap.
    Text { content: String, truncated: bool },
    /// The content fetch failed after the stage said it was readable text.
    /// Distinct from [`PaneState::Unreadable`]: the *stage* was fine, the
    /// follow-up read was not, and a viewer can offer a retry for one and
    /// not the other.
    ContentUnavailable { reason: String },
}

impl PaneState {
    /// The pane state a stage implies before any content has been fetched.
    pub fn for_stage(stage: &Stage) -> Self {
        match stage {
            Stage::Absent {} => PaneState::Absent,
            Stage::Unreadable { reason } => PaneState::Unreadable {
                reason: reason.clone(),
            },
            Stage::Present {
                binary: true,
                size_bytes,
                ..
            } => PaneState::Binary {
                size_bytes: *size_bytes,
            },
            Stage::Present { oid, .. } => PaneState::AwaitingContent {
                oid: oid.as_str().to_string(),
            },
        }
    }

    /// Fold a completed `GET /api/blob/{oid}` into this pane.
    ///
    /// **A pane that is not [`AwaitingContent`] is returned unchanged**, and
    /// that is the invariant this function exists for rather than an edge
    /// case it tolerates. A response can land late — after the user moved to
    /// another file, or for a side nothing asked about — and letting it
    /// overwrite an `Absent` or `Unreadable` pane with `Text { content: "" }`
    /// would turn "there is nothing here" and "nobody could look" into "this
    /// version was blank", which is precisely the collapse ADR 0063 exists to
    /// prevent, arriving through the back door of a stale fetch.
    ///
    /// The fetched `binary` flag wins over the stage's when they disagree.
    /// Both are git's own first-8000-bytes NUL sniff so they should agree,
    /// but if they ever do not, the safe answer is the one that refuses to
    /// decode: rendering real binary bytes as lossy text is worse than
    /// withholding a pane.
    ///
    /// [`AwaitingContent`]: PaneState::AwaitingContent
    pub fn with_content(self, fetched: Result<BlobContent, String>) -> Self {
        let PaneState::AwaitingContent { ref oid } = self else {
            return self;
        };
        match fetched {
            Err(reason) => PaneState::ContentUnavailable { reason },
            // A response for a different object is not this pane's content.
            // Same reasoning as the state guard above, one field deeper.
            Ok(blob) if blob.oid != *oid => self,
            Ok(blob) if blob.binary => PaneState::Binary {
                size_bytes: blob.content.len() as u64,
            },
            Ok(blob) => PaneState::Text {
                content: blob.content,
                truncated: blob.truncated,
            },
        }
    }

    /// A short human description, and the one place the
    /// absent/unreadable/failed distinction becomes words a user reads.
    pub fn describe(&self) -> String {
        match self {
            PaneState::Absent => "Not present on this side".to_string(),
            PaneState::Unreadable { reason } => format!("Could not be read — {reason}"),
            PaneState::Binary { size_bytes } => format!("Binary file ({size_bytes} bytes)"),
            PaneState::AwaitingContent { .. } => "Loading…".to_string(),
            PaneState::Text {
                truncated: true, ..
            } => "Shown up to the size limit".to_string(),
            PaneState::Text { .. } => "Loaded".to_string(),
            PaneState::ContentUnavailable { reason } => {
                format!("Content could not be loaded — {reason}")
            }
        }
    }
}

/// The outcome of reading the working tree's copy for the result pane.
///
/// [`Self::NoFile`] is its own variant rather than an error string because a
/// missing worktree file is **information**: in a delete/modify conflict git
/// legitimately leaves nothing on disk, and reporting that as a failed read
/// would tell the user something broke when nothing did. Same shape, and
/// same reason, as `Stage::Absent` versus `Stage::Unreadable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultRead {
    /// Git wrote a file, and here it is.
    Wrote(WorktreeFileContent),
    /// There is no file at this path in the working tree.
    NoFile,
    /// The read itself failed.
    Failed(String),
}

/// The result pane's state, from a completed worktree read.
pub fn result_pane_state(read: ResultRead) -> PaneState {
    match read {
        ResultRead::NoFile => PaneState::Absent,
        ResultRead::Failed(reason) => PaneState::ContentUnavailable { reason },
        ResultRead::Wrote(file) if file.binary => PaneState::Binary {
            size_bytes: file.content.len() as u64,
        },
        ResultRead::Wrote(file) => PaneState::Text {
            content: file.content,
            truncated: file.truncated,
        },
    }
}

/// Why a resolution control is not offered, in words a user reads (M4.31d,
/// #430).
///
/// A *reason*, not a bool, for the same argument
/// [`NotTextResolvable`](git_vista_protocol::conflict::NotTextResolvable)
/// makes one layer down: "disabled" with no sentence is a dead control the
/// user cannot act on, and the three causes here call for three different
/// next moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Withheld {
    /// The side this control takes holds nothing. Refused by
    /// `ConflictedFile::refuses` as `SideAbsent`, so offering it would produce
    /// a server error the user could have been spared.
    SideAbsent,
    /// The side this control takes could not be read, so choosing it would be
    /// choosing a version nobody has seen.
    SideUnreadable { reason: String },
    /// Some *other* stage of this file is unreadable. Nothing may be resolved
    /// until it can be read — `ConflictedFile::all_sides_readable`'s own doc
    /// says a caller "must not present a resolution UI for such a file".
    FileNotFullyReadable,
}

impl Withheld {
    /// The sentence shown in place of the control.
    pub fn describe(&self) -> String {
        match self {
            Withheld::SideAbsent => {
                "Not offered — that side holds no version of this file".to_string()
            }
            Withheld::SideUnreadable { reason } => {
                format!("Not offered — that side could not be read: {reason}")
            }
            Withheld::FileNotFullyReadable => {
                "Not offered — one side of this file could not be read".to_string()
            }
        }
    }
}

/// What kind of conflict this is, as a sentence and a set of offered controls
/// (M4.31d, #430).
///
/// # Why this is a type and not three `if`s in the viewer
///
/// Issue #430's whole premise is that binary, delete/modify and rename
/// conflicts "are the cases a text-first resolver gets wrong by default".
/// Getting them right is a *classification*, and classification tested in a
/// `#[cfg(target_arch = "wasm32")]` viewer is classification `cargo test`
/// never compiles — the failure mode this module's parent documents at
/// length. So the decision lives here, host-tested, and the viewer only draws
/// what it is handed.
///
/// # Rename is deliberately absent
///
/// [`NotTextResolvable::Rename`](git_vista_protocol::conflict::NotTextResolvable::Rename)
/// exists on the wire and **nothing ever constructs it**. Git does not record
/// rename information for conflicted paths: `git status --porcelain=v2` gives
/// a rename's original path only on a `2` record, and a conflicted path is a
/// `u` record, whose grammar has exactly one path field. Producing two paths
/// would mean re-running git's own similarity heuristic and inventing a
/// confidence this type could not honestly state — precisely what
/// [`Stage::Unreadable`] exists to refuse. #430's third acceptance criterion
/// is therefore **not implemented, and is recorded as unbuildable** rather
/// than satisfied by a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionSurface {
    /// The plain-language fact about this conflict's shape, or `None` for an
    /// ordinary text conflict that needs no explanation.
    pub note: Option<String>,
    /// Whether a line-level text resolver may open on this path at all.
    ///
    /// #430's fourth acceptance criterion. **There is no line-level resolver
    /// yet** — that is #432 — so today this flag is a promise nothing consumes.
    /// It is computed and tested here so that when the resolver arrives it has
    /// a tested answer to ask, rather than re-deriving the rule at the call
    /// site.
    pub text_resolution_allowed: bool,
    /// `Take ours`, or why it is withheld.
    pub take_ours: Result<(), Withheld>,
    /// `Take theirs`, or why it is withheld.
    pub take_theirs: Result<(), Withheld>,
    /// `Delete file`, or why it is withheld.
    ///
    /// Deleting depends on neither side's content — `ConflictedFile::refuses`
    /// returns `None` for it unconditionally — so this is withheld only when
    /// the file as a whole cannot be resolved.
    pub take_deletion: Result<(), Withheld>,
}

impl ResolutionSurface {
    /// Classify one conflicted file.
    pub fn of(file: &ConflictedFile) -> Self {
        let fully_readable = file.all_sides_readable();

        // The gate that outranks everything below it. `all_sides_readable`
        // means "no stage is Unreadable"; when one is, the user would be
        // choosing between versions they have not all seen, so nothing is
        // offered — including deletion, because the decision to delete is
        // still made against a file the user cannot fully inspect.
        if !fully_readable {
            return ResolutionSurface {
                note: Some(Self::note_for(file)),
                text_resolution_allowed: false,
                take_ours: Err(Withheld::FileNotFullyReadable),
                take_theirs: Err(Withheld::FileNotFullyReadable),
                take_deletion: Err(Withheld::FileNotFullyReadable),
            };
        }

        // Mirrors `ConflictedFile::refuses` rather than re-deciding: a control
        // offered here and refused by the server is a 409 the user was walked
        // into. Absent is the delete/modify case — the side that deleted the
        // file holds nothing to take, and `TakeDeletion` is the control that
        // expresses that intent.
        let side = |stage: &Stage| match stage {
            Stage::Present { .. } => Ok(()),
            Stage::Absent {} => Err(Withheld::SideAbsent),
            Stage::Unreadable { reason } => Err(Withheld::SideUnreadable {
                reason: reason.clone(),
            }),
        };

        ResolutionSurface {
            note: file
                .not_text_resolvable
                .as_ref()
                .map(|_| Self::note_for(file)),
            // Delegated to `ConflictedFile::text_resolvable` (protocol
            // conflict.rs, added for #432/ADR 0069) rather than computed here
            // a second time: the server asks the identical question before
            // executing a content resolution, and two independent copies of
            // this exact rule is how #430 shipped a wrong sentence.
            text_resolution_allowed: file.text_resolvable(),
            take_ours: side(&file.ours),
            take_theirs: side(&file.theirs),
            take_deletion: Ok(()),
        }
    }

    /// The plain-language sentence for this conflict's shape.
    ///
    /// Named sides ("theirs deleted it, ours changed it") rather than a
    /// generic "this file cannot be text-merged": #430's second acceptance
    /// criterion is specifically that a delete/modify conflict **names which
    /// side deleted and which modified**.
    fn note_for(file: &ConflictedFile) -> String {
        match &file.not_text_resolvable {
            Some(NotTextResolvable::Binary { ours, theirs }) => {
                let which = match (ours, theirs) {
                    (true, true) => "Both sides are binary",
                    (true, false) => "Our side is binary",
                    (false, true) => "Their side is binary",
                    // The server sets at least one flag when it reports
                    // Binary; this arm keeps the sentence honest rather than
                    // asserting a side we were not told about.
                    (false, false) => "This file is binary",
                };
                format!("{which}. There is no line-by-line merge for binary content — choose a whole side, or delete the file.")
            }
            // BRANCHES ON `kind`, NOT ON THE TWO BOOLEANS — and that is the
            // whole point of this arm.
            //
            // The server sets `ours_deleted` for `DeletedByUs`, `BothDeleted`
            // AND `AddedByThem` (server conflicts.rs:164-167); `theirs_deleted`
            // likewise covers `AddedByUs`. So the booleans conflate two
            // genuinely different facts: "this side deleted an existing file"
            // and "this side never had the file at all". An add/add-shaped
            // conflict (`UA`/`AU`) would otherwise be described as a deletion
            // that never happened, and the other side as a change nobody made
            // — a UI asserting two facts it was never told, which is exactly
            // what ADR 0063 exists to prevent.
            //
            // `kind` is git's own porcelain classification and cannot be
            // conflated, so it is what gets read here. Note also that no arm
            // claims the surviving side "changed" anything: the wire carries
            // deletion flags, not modification flags, and "still has it" is
            // supported by that side's stage being Present.
            Some(NotTextResolvable::Deletion { .. }) => match file.kind {
                ConflictKind::BothDeleted => {
                    "Both sides deleted this file. Deleting it is the only resolution.".to_string()
                }
                ConflictKind::DeletedByUs => {
                    "We deleted this file; their side still has it. Keep their version, or delete it."
                        .to_string()
                }
                ConflictKind::DeletedByThem => {
                    "They deleted this file; our side still has it. Keep our version, or delete it."
                        .to_string()
                }
                ConflictKind::AddedByUs => {
                    "We added this file; their side does not have it. Keep our version, or delete it."
                        .to_string()
                }
                ConflictKind::AddedByThem => {
                    "They added this file; our side does not have it. Keep their version, or delete it."
                        .to_string()
                }
                // The server only reports Deletion for the five kinds above, so
                // these are unreachable today. They describe the shape without
                // naming a side, rather than asserting a deletion this type was
                // never actually told about.
                ConflictKind::BothAdded | ConflictKind::BothModified => {
                    "Only one side has a version of this file. Keep a version, or delete it."
                        .to_string()
                }
            },
            // Unreachable from the server today (nothing constructs Rename),
            // but the wire type permits it, so it gets an honest sentence
            // rather than a panic or a silent blank.
            Some(NotTextResolvable::Rename {
                ours_path,
                theirs_path,
            }) => format!(
                "This file was renamed: ours is {ours_path}, theirs is {theirs_path}. \
                 The two sides do not agree on the path, so there is no line-by-line merge."
            ),
            None if !file.all_sides_readable() => {
                "One side of this file could not be read, so it cannot be resolved yet.".to_string()
            }
            None => String::new(),
        }
    }
}

/// All four panes for one conflicted path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictPanes {
    pub path: String,
    pub base: PaneState,
    pub ours: PaneState,
    pub theirs: PaneState,
    pub result: PaneState,
    /// What kind of conflict this is and what may be done about it (M4.31d,
    /// #430).
    ///
    /// Carried here because `ConflictPanes` is the whole of what the viewer
    /// receives: before this field, `open()` took a `ConflictedFile` holding
    /// `not_text_resolvable` and returned a struct without it, so the typed
    /// reason died at the display boundary and no renderer could distinguish
    /// a binary conflict from a text one.
    pub surface: ResolutionSurface,
}

impl ConflictPanes {
    /// Open all four panes for `file`.
    ///
    /// The three stage panes come straight from [`PaneState::for_stage`]; the
    /// result pane starts as [`PaneState::AwaitingContent`] carrying the
    /// **path**, not an oid, because the working tree's copy is not a git
    /// object — it is whatever bytes are on disk at that path right now.
    pub fn open(file: &ConflictedFile) -> Self {
        ConflictPanes {
            path: file.path.clone(),
            base: PaneState::for_stage(&file.base),
            ours: PaneState::for_stage(&file.ours),
            theirs: PaneState::for_stage(&file.theirs),
            result: PaneState::AwaitingContent {
                oid: file.path.clone(),
            },
            surface: ResolutionSurface::of(file),
        }
    }

    /// One pane by name, so a view can iterate [`Pane::ALL`] rather than
    /// naming four fields and risking a missed one.
    pub fn pane(&self, pane: Pane) -> &PaneState {
        match pane {
            Pane::Base => &self.base,
            Pane::Ours => &self.ours,
            Pane::Theirs => &self.theirs,
            Pane::Result => &self.result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::plan::CommitOid;
    use git_vista_protocol::status::ConflictKind;

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }

    fn present(c: char) -> Stage {
        Stage::Present {
            oid: oid(c),
            binary: false,
            size_bytes: 12,
        }
    }

    fn file(base: Stage, ours: Stage, theirs: Stage) -> ConflictedFile {
        ConflictedFile {
            path: "a.txt".into(),
            kind: ConflictKind::BothModified,
            base,
            ours,
            theirs,
            not_text_resolvable: None,
        }
    }

    #[test]
    fn an_absent_stage_is_absent_and_never_an_empty_text_pane() {
        // THE test in this file, and #428's second acceptance criterion.
        // MUTATION: map Absent to `Text { content: String::new() }`. Every
        // add/add conflict would then show a blank base pane, asserting a
        // common ancestor that never existed (ADR 0063).
        let state = PaneState::for_stage(&Stage::Absent {});
        assert_eq!(state, PaneState::Absent);
        assert!(
            state.describe().contains("Not present"),
            "{}",
            state.describe()
        );
    }

    #[test]
    fn an_unreadable_stage_says_so_and_keeps_its_reason() {
        // #428's third acceptance criterion. MUTATION: drop the reason, or
        // map Unreadable to Absent. The user would be told a side does not
        // exist when the truth is that nobody managed to look at it.
        let state = PaneState::for_stage(&Stage::Unreadable {
            reason: "blob missing".into(),
        });
        assert_eq!(
            state,
            PaneState::Unreadable {
                reason: "blob missing".into()
            }
        );
        assert!(
            state.describe().contains("blob missing"),
            "the reason must reach the user: {}",
            state.describe()
        );
    }

    #[test]
    fn a_binary_stage_stops_at_metadata_and_never_awaits_text() {
        // MUTATION: treat binary like any other Present stage. The pane
        // would fetch the blob and decode arbitrary bytes as lossy text.
        let state = PaneState::for_stage(&Stage::Present {
            oid: oid('b'),
            binary: true,
            size_bytes: 900,
        });
        assert_eq!(state, PaneState::Binary { size_bytes: 900 });
    }

    #[test]
    fn a_text_stage_awaits_its_content_under_its_own_oid() {
        let state = PaneState::for_stage(&present('a'));
        assert_eq!(
            state,
            PaneState::AwaitingContent {
                oid: "a".repeat(40)
            }
        );
    }

    #[test]
    fn fetched_content_fills_an_awaiting_pane() {
        let state = PaneState::for_stage(&present('a'));
        let filled = state.with_content(Ok(BlobContent {
            oid: "a".repeat(40),
            content: "hello\n".into(),
            truncated: false,
            binary: false,
        }));
        assert_eq!(
            filled,
            PaneState::Text {
                content: "hello\n".into(),
                truncated: false
            }
        );
    }

    #[test]
    fn a_truncated_fetch_keeps_saying_it_was_truncated() {
        // MUTATION: hardcode `truncated: false`. The viewer would present a
        // cut-off file as the whole file — the same wrong-answer-wearing-a-
        // success-status failure the server's own cap comments argue against.
        let filled = PaneState::for_stage(&present('a')).with_content(Ok(BlobContent {
            oid: "a".repeat(40),
            content: "part".into(),
            truncated: true,
            binary: false,
        }));
        assert_eq!(
            filled,
            PaneState::Text {
                content: "part".into(),
                truncated: true
            }
        );
        assert!(
            filled.describe().contains("size limit"),
            "{}",
            filled.describe()
        );
    }

    #[test]
    fn a_late_fetch_never_overwrites_absent_or_unreadable() {
        // THE other test in this file. MUTATION: drop the
        // `AwaitingContent` guard in `with_content` and fold the response in
        // unconditionally. A response landing for the wrong pane would
        // rewrite "there is nothing on this side" and "nobody could read
        // this" into an empty — or worse, a *populated* — text pane. Both
        // acceptance criteria above would then hold at `for_stage` and be
        // undone one call later.
        let blob = BlobContent {
            oid: "a".repeat(40),
            content: String::new(),
            truncated: false,
            binary: false,
        };

        let absent = PaneState::Absent.with_content(Ok(blob.clone()));
        assert_eq!(absent, PaneState::Absent, "an absent pane stays absent");

        let unreadable = PaneState::Unreadable {
            reason: "gone".into(),
        }
        .with_content(Ok(blob.clone()));
        assert_eq!(
            unreadable,
            PaneState::Unreadable {
                reason: "gone".into()
            },
            "an unreadable pane stays unreadable"
        );

        let binary = PaneState::Binary { size_bytes: 4 }.with_content(Ok(blob));
        assert_eq!(binary, PaneState::Binary { size_bytes: 4 });

        // The **failed** fetch is the half this test originally missed, and
        // missing it made the whole test inert against the state guard: with
        // only the `Ok` cases above, removing the guard left every assertion
        // green, because the oid comparison one line further down happened to
        // reject the mismatched blob anyway. Verified by `mutation_check`
        // (#428) — the mutation SURVIVED until these three lines existed.
        //
        // The `Err` path has no such second line to hide behind. Without the
        // guard, a failed fetch rewrites `Absent` to `ContentUnavailable`,
        // turning "there is nothing on this side" into "the content could not
        // be loaded" — a fault reported where git is simply, correctly,
        // holding nothing.
        for pane in [
            PaneState::Absent,
            PaneState::Unreadable {
                reason: "gone".into(),
            },
            PaneState::Binary { size_bytes: 4 },
        ] {
            assert_eq!(
                pane.clone().with_content(Err("timed out".into())),
                pane,
                "a failed fetch must not rewrite {pane:?}"
            );
        }
    }

    #[test]
    fn a_response_for_another_object_is_not_this_panes_content() {
        // MUTATION: drop the oid comparison. Switching files fast enough
        // would paint one side's content into another side's pane — content
        // the user is about to make a resolution decision against.
        let state = PaneState::for_stage(&present('a'));
        let unchanged = state.clone().with_content(Ok(BlobContent {
            oid: "b".repeat(40),
            content: "someone else's bytes".into(),
            truncated: false,
            binary: false,
        }));
        assert_eq!(unchanged, state, "a mismatched oid must be ignored");
    }

    #[test]
    fn a_failed_fetch_is_distinct_from_an_unreadable_stage() {
        let failed =
            PaneState::for_stage(&present('a')).with_content(Err("connection lost".into()));
        assert_eq!(
            failed,
            PaneState::ContentUnavailable {
                reason: "connection lost".into()
            }
        );
        assert!(failed.describe().contains("connection lost"));
    }

    #[test]
    fn a_fetch_that_turns_out_binary_wins_over_the_stages_claim() {
        // The stage said text; the content says otherwise. Refusing to
        // decode is the safe direction — rendering real binary as lossy text
        // is worse than withholding the pane.
        let filled = PaneState::for_stage(&present('a')).with_content(Ok(BlobContent {
            oid: "a".repeat(40),
            content: String::new(),
            truncated: false,
            binary: true,
        }));
        assert!(matches!(filled, PaneState::Binary { .. }));
    }

    // ---- the result pane -------------------------------------------------

    #[test]
    fn a_missing_worktree_file_is_absent_not_a_failure() {
        // MUTATION: map NoFile to ContentUnavailable. A delete/modify
        // conflict resolved toward deletion would report a fault where git
        // simply, correctly, left no file.
        let state = result_pane_state(ResultRead::NoFile);
        assert_eq!(state, PaneState::Absent);
        assert!(state.describe().contains("Not present"));
    }

    #[test]
    fn the_result_pane_shows_the_marker_file_git_wrote() {
        let state = result_pane_state(ResultRead::Wrote(WorktreeFileContent {
            path: "a.txt".into(),
            content: "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> theirs\n".into(),
            truncated: false,
            binary: false,
        }));
        let PaneState::Text { content, .. } = &state else {
            panic!("expected text, got {state:?}");
        };
        assert!(content.contains("<<<<<<<"));
    }

    #[test]
    fn a_failed_worktree_read_is_never_shown_as_an_empty_result() {
        let state = result_pane_state(ResultRead::Failed("permission denied".into()));
        assert_eq!(
            state,
            PaneState::ContentUnavailable {
                reason: "permission denied".into()
            }
        );
    }

    // ---- all four panes --------------------------------------------------

    #[test]
    fn opening_a_conflict_makes_all_four_panes_reachable() {
        // #428's first acceptance criterion, and the reason `Pane::ALL`
        // exists: a view iterating it cannot omit one.
        let panes = ConflictPanes::open(&file(present('1'), present('2'), present('3')));
        assert_eq!(Pane::ALL.len(), 4);
        for p in Pane::ALL {
            assert!(
                matches!(panes.pane(p), PaneState::AwaitingContent { .. }),
                "{p:?} should be awaiting content"
            );
        }
        assert_eq!(panes.path, "a.txt");
    }

    #[test]
    fn an_add_add_conflict_opens_with_an_absent_base_and_two_live_sides() {
        // The shape ADR 0063 spends its longest section on, end to end.
        let panes = ConflictPanes::open(&file(Stage::Absent {}, present('2'), present('3')));
        assert_eq!(*panes.pane(Pane::Base), PaneState::Absent);
        assert!(matches!(
            panes.pane(Pane::Ours),
            PaneState::AwaitingContent { .. }
        ));
        assert!(matches!(
            panes.pane(Pane::Theirs),
            PaneState::AwaitingContent { .. }
        ));
    }

    // ---- M4.31d (#430): binary, delete/modify and unreadable surfaces ------

    fn binary_stage(c: char) -> Stage {
        Stage::Present {
            oid: oid(c),
            binary: true,
            size_bytes: 4096,
        }
    }

    fn with_reason(
        base: Stage,
        ours: Stage,
        theirs: Stage,
        reason: Option<NotTextResolvable>,
    ) -> ConflictedFile {
        ConflictedFile {
            path: "a.bin".into(),
            kind: ConflictKind::BothModified,
            base,
            ours,
            theirs,
            not_text_resolvable: reason,
        }
    }

    /// A conflict of an explicit `kind`, for the deletion sentences — which
    /// read `kind` rather than the two booleans, and so cannot be exercised
    /// through [`with_reason`]'s fixed `BothModified`.
    fn of_kind(kind: ConflictKind, ours: Stage, theirs: Stage) -> ConflictedFile {
        // Mirrors the server's own mapping (conflicts.rs:164-171) so these
        // fixtures carry the same flags a real response would.
        let ours_deleted = matches!(
            kind,
            ConflictKind::DeletedByUs | ConflictKind::BothDeleted | ConflictKind::AddedByThem
        );
        let theirs_deleted = matches!(
            kind,
            ConflictKind::DeletedByThem | ConflictKind::BothDeleted | ConflictKind::AddedByUs
        );
        ConflictedFile {
            path: "a.txt".into(),
            kind,
            base: Stage::Absent {},
            ours,
            theirs,
            not_text_resolvable: Some(NotTextResolvable::Deletion {
                ours_deleted,
                theirs_deleted,
            }),
        }
    }

    #[test]
    fn a_binary_conflict_says_no_line_merge_is_possible_and_still_offers_both_sides() {
        // #430's FIRST acceptance criterion.
        //
        // MUTATION A: return `note: None` for Binary. The pane would show only
        // "Binary file (4096 bytes)" — a size, not an explanation — which is
        // exactly today's behaviour and exactly what this issue exists to fix.
        // MUTATION B: set `text_resolution_allowed: true`. A line resolver
        // would be allowed to open on bytes it cannot merge.
        let file = with_reason(
            Stage::Absent {},
            binary_stage('2'),
            binary_stage('3'),
            Some(NotTextResolvable::Binary {
                ours: true,
                theirs: true,
            }),
        );
        let s = ResolutionSurface::of(&file);

        let note = s.note.expect("a binary conflict must explain itself");
        assert!(note.contains("binary"), "{note}");
        assert!(
            note.contains("no line-by-line merge"),
            "the user must be told WHY, not just that it is binary: {note}"
        );
        assert!(
            !s.text_resolution_allowed,
            "a line resolver must never open on binary content"
        );
        // Choosing a whole side IS the honest resolution for binary, and the
        // server accepts it: `refuses()` returns None for a Present stage
        // whatever its `binary` flag. Withholding these would leave a binary
        // conflict unresolvable.
        assert_eq!(s.take_ours, Ok(()));
        assert_eq!(s.take_theirs, Ok(()));
        assert_eq!(s.take_deletion, Ok(()));
    }

    #[test]
    fn a_binary_note_names_which_side_rather_than_generalising() {
        // "Their side is binary" is actionable; "this file is binary" is not.
        // The protocol carries the two flags separately for this reason.
        let theirs_only = with_reason(
            present('1'),
            present('2'),
            binary_stage('3'),
            Some(NotTextResolvable::Binary {
                ours: false,
                theirs: true,
            }),
        );
        let note = ResolutionSurface::of(&theirs_only).note.unwrap();
        assert!(note.starts_with("Their side is binary"), "{note}");

        let ours_only = with_reason(
            present('1'),
            binary_stage('2'),
            present('3'),
            Some(NotTextResolvable::Binary {
                ours: true,
                theirs: false,
            }),
        );
        let note = ResolutionSurface::of(&ours_only).note.unwrap();
        assert!(note.starts_with("Our side is binary"), "{note}");
    }

    #[test]
    fn a_delete_modify_conflict_names_which_side_deleted() {
        // #430's SECOND acceptance criterion.
        //
        // RENAMED, and the assertion narrowed, on purpose. The first version of
        // this test was called `..._and_which_changed` and required the
        // sentence to say "we changed it" / "they changed it". That shipped in
        // 7ca1ac8c and was wrong: the wire carries DELETION flags only, so the
        // surviving side's modification was never a fact this type held. The
        // test was pinning a claim the data did not support — a green test
        // guarding a false sentence, which is worse than no test.
        //
        // What is legitimately pinned is the half the issue actually asks for:
        // WHICH SIDE deleted. The surviving side is described by what is known
        // — its stage is Present, so it "still has it".
        //
        // MUTATION: collapse the `kind` match to one generic sentence. The user
        // is told a deletion happened but not by whom, and choosing which side
        // to keep is the entire decision.
        let they_deleted = of_kind(ConflictKind::DeletedByThem, present('2'), Stage::Absent {});
        let note = ResolutionSurface::of(&they_deleted).note.unwrap();
        assert!(note.contains("They deleted"), "must name the side: {note}");
        assert!(
            note.contains("our side still has it"),
            "the surviving side is described by its stage, not by a guess: {note}"
        );

        let we_deleted = of_kind(ConflictKind::DeletedByUs, Stage::Absent {}, present('3'));
        let note = ResolutionSurface::of(&we_deleted).note.unwrap();
        assert!(note.contains("We deleted"), "{note}");
        assert!(note.contains("their side still has it"), "{note}");
    }

    #[test]
    fn an_added_by_one_side_conflict_is_never_described_as_a_deletion() {
        // Found by the #430 honesty review, and it had already shipped in
        // 7ca1ac8c.
        //
        // The server sets `ours_deleted` for AddedByThem (UA) and
        // `theirs_deleted` for AddedByUs (AU) — see server conflicts.rs:164-171
        // — because from the index's point of view "we have no stage 2" looks
        // the same either way. But UA means "they added it, we haven't touched
        // it": NOBODY deleted anything, and the other side changed nothing,
        // because there was nothing there to change.
        //
        // Reading the two booleans produced "We deleted this file; they changed
        // it." Both halves false — two facts asserted that the wire never
        // carried, which is the ADR 0063 collapse one layer up.
        //
        // MUTATION A: branch on (ours_deleted, theirs_deleted) again instead of
        // on `kind`. UA immediately claims a deletion that never happened.
        // MUTATION B: keep `kind` but merge the AddedBy* arms into the
        // DeletedBy* ones. Same false sentence, arrived at differently.
        let they_added = of_kind(ConflictKind::AddedByThem, Stage::Absent {}, present('3'));
        let note = ResolutionSurface::of(&they_added).note.unwrap();
        assert!(
            !note.contains("deleted"),
            "UA means they ADDED it — nobody deleted anything: {note}"
        );
        assert!(
            note.contains("They added"),
            "the sentence must say what actually happened: {note}"
        );

        let we_added = of_kind(ConflictKind::AddedByUs, present('2'), Stage::Absent {});
        let note = ResolutionSurface::of(&we_added).note.unwrap();
        assert!(
            !note.contains("deleted"),
            "AU is an add, not a delete: {note}"
        );
        assert!(note.contains("We added"), "{note}");
    }

    #[test]
    fn no_deletion_sentence_claims_the_surviving_side_changed_anything() {
        // The wire carries DELETION flags. It carries nothing about whether the
        // surviving side was modified, so no sentence may say it was. "still
        // has it" is supported by that side's stage being Present; "changed it"
        // is an inference, and this codebase does not print inferences as
        // facts.
        //
        // MUTATION: restore "they changed it" / "we changed it". The claim is
        // unsupported for every kind, and flatly contradicts DeletedByUs's own
        // doc ("we deleted it, they haven't touched it").
        for kind in [
            ConflictKind::DeletedByUs,
            ConflictKind::DeletedByThem,
            ConflictKind::AddedByUs,
            ConflictKind::AddedByThem,
            ConflictKind::BothDeleted,
        ] {
            let note = ResolutionSurface::of(&of_kind(kind, present('2'), present('3')))
                .note
                .unwrap();
            assert!(
                !note.contains("changed it"),
                "{kind:?} must not assert the other side changed anything: {note}"
            );
        }
    }

    #[test]
    fn the_two_deletion_directions_do_not_produce_the_same_sentence() {
        // Cheap, and it is the assertion that would survive someone
        // "simplifying" the match into a single generic string later.
        let we = ResolutionSurface::of(&of_kind(
            ConflictKind::DeletedByUs,
            Stage::Absent {},
            present('3'),
        ))
        .note
        .unwrap();
        let they = ResolutionSurface::of(&of_kind(
            ConflictKind::DeletedByThem,
            present('2'),
            Stage::Absent {},
        ))
        .note
        .unwrap();
        assert_ne!(we, they);
        assert!(we.starts_with("We deleted"), "{we}");
        assert!(they.starts_with("They deleted"), "{they}");
    }

    #[test]
    fn taking_a_side_that_deleted_the_file_is_withheld_before_the_server_refuses_it() {
        // The defect this slice actually fixes, and it is not in the issue text.
        //
        // `ConflictedFile::refuses` (protocol conflict.rs:343) returns
        // `SideAbsent` for TakeOurs/TakeTheirs against an Absent stage. The
        // viewer offers all three buttons unconditionally, so today a user
        // clicking "Take theirs" on the side that deleted the file receives a
        // 409 from the server. Withholding it here is the difference between
        // an explained control and a walked-into error.
        //
        // MUTATION: return Ok(()) for Stage::Absent. The button comes back and
        // so does the 409.
        let they_deleted = with_reason(
            present('1'),
            present('2'),
            Stage::Absent {},
            Some(NotTextResolvable::Deletion {
                ours_deleted: false,
                theirs_deleted: true,
            }),
        );
        let s = ResolutionSurface::of(&they_deleted);

        assert_eq!(
            s.take_theirs,
            Err(Withheld::SideAbsent),
            "their side holds nothing to take"
        );
        assert_eq!(s.take_ours, Ok(()), "our side still has content to keep");
        assert_eq!(
            s.take_deletion,
            Ok(()),
            "deletion needs neither side readable — refuses() returns None for it"
        );
        assert!(
            s.take_theirs.unwrap_err().describe().contains("no version"),
            "the withheld control must say why"
        );
    }

    #[test]
    fn an_unreadable_stage_withholds_every_control_including_deletion() {
        // `ConflictedFile::all_sides_readable`'s own doc says a caller "must
        // not present a resolution UI for such a file". Nothing in the client
        // enforced that before this slice — confirmed by reading viewer.rs's
        // control list, which is built unconditionally.
        //
        // MUTATION A: drop the `!fully_readable` early return. Every control
        // returns, and the user chooses between versions one of which nobody
        // has seen.
        // MUTATION B: leave `take_deletion: Ok(())` in the early return. That
        // is the subtle one — deleting does not need the stages readable, so
        // it looks safe, but the user is deciding to destroy a file they were
        // never able to inspect.
        let unreadable = with_reason(
            present('1'),
            present('2'),
            Stage::Unreadable {
                reason: "blob missing".into(),
            },
            None,
        );
        let s = ResolutionSurface::of(&unreadable);

        assert_eq!(s.take_ours, Err(Withheld::FileNotFullyReadable));
        assert_eq!(s.take_theirs, Err(Withheld::FileNotFullyReadable));
        assert_eq!(
            s.take_deletion,
            Err(Withheld::FileNotFullyReadable),
            "deletion is withheld too: the file cannot be inspected before destroying it"
        );
        assert!(!s.text_resolution_allowed);
        assert!(
            s.note.unwrap().contains("could not be read"),
            "an unreadable file must say so even with no NotTextResolvable reason"
        );
    }

    #[test]
    fn an_ordinary_text_conflict_gets_no_note_and_allows_text_resolution() {
        // The negative control. Without it, every assertion above could pass
        // on an implementation that slapped a note on everything and withheld
        // every button — maximally "safe" and completely useless.
        let s = ResolutionSurface::of(&file(present('1'), present('2'), present('3')));
        assert_eq!(s.note, None, "a normal text conflict needs no explanation");
        assert!(s.text_resolution_allowed);
        assert_eq!(s.take_ours, Ok(()));
        assert_eq!(s.take_theirs, Ok(()));
        assert_eq!(s.take_deletion, Ok(()));
    }

    #[test]
    fn a_binary_side_blocks_text_resolution_even_with_no_typed_reason() {
        // Defence in depth, and a real case: the server sets
        // `not_text_resolvable` from its own scan, but `Stage::is_text` is the
        // per-side truth. If the two ever disagree, the side that refuses to
        // decode must win — the same direction `with_content` already takes
        // when a fetched blob turns out binary.
        let sneaky = with_reason(present('1'), present('2'), binary_stage('3'), None);
        let s = ResolutionSurface::of(&sneaky);
        assert!(
            !s.text_resolution_allowed,
            "a binary side must block a line resolver whatever the typed reason says"
        );
    }

    #[test]
    fn opening_a_conflict_carries_the_reason_through_to_the_viewer() {
        // THE regression test for this slice. Before #430, `ConflictPanes`
        // had no `surface` field at all: `open()` accepted a ConflictedFile
        // carrying `not_text_resolvable` and returned a struct without it, so
        // the typed reason died at the display boundary and no renderer could
        // tell a binary conflict from a text one.
        //
        // MUTATION: build `surface` from a default/empty ConflictedFile rather
        // than from `file`. The panes still render, every existing test still
        // passes, and the explanation silently disappears — which is precisely
        // the bug this issue was filed about.
        let panes = ConflictPanes::open(&with_reason(
            Stage::Absent {},
            binary_stage('2'),
            binary_stage('3'),
            Some(NotTextResolvable::Binary {
                ours: true,
                theirs: true,
            }),
        ));
        assert!(
            panes.surface.note.is_some(),
            "the reason must survive the trip into ConflictPanes"
        );
        assert!(!panes.surface.text_resolution_allowed);
    }

    #[test]
    fn the_result_pane_is_labelled_read_only() {
        // #428's decision comment: the result pane ships read-only and
        // labelled as such. MUTATION: drop the suffix. Nothing on screen
        // would tell the user why their edits do not stick, and the label is
        // the only place that promise is made.
        assert_eq!(Pane::Result.label(), "Result (read-only)");
        assert_eq!(Pane::Base.label(), "Base");
        assert_eq!(Pane::Ours.label(), "Ours");
        assert_eq!(Pane::Theirs.label(), "Theirs");
    }
}
