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
use git_vista_protocol::conflict::{ConflictedFile, Stage};

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

    /// Whether this pane may be rendered as a text buffer at all.
    ///
    /// Exists so no caller writes `matches!(.., Text { .. })` in four places
    /// and gets one of them wrong — the same reason
    /// [`Continuation::may_continue`](git_vista_protocol::conflict::Continuation::may_continue)
    /// exists next door.
    pub fn is_text(&self) -> bool {
        matches!(self, PaneState::Text { .. })
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

/// All four panes for one conflicted path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictPanes {
    pub path: String,
    pub base: PaneState,
    pub ours: PaneState,
    pub theirs: PaneState,
    pub result: PaneState,
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
            !state.is_text(),
            "absent must never render as a text buffer"
        );
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
        assert!(!state.is_text());
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
        assert!(!state.is_text());
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
        assert!(filled.is_text());
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
        assert!(!failed.is_text());
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
        assert!(!state.is_text());
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
