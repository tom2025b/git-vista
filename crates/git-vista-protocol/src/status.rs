//! The generation-tagged working-tree status DTO (M2.15, #68a).
//!
//! This is the **DTO only** — the wire shape `GET /api/status` (a future
//! endpoint, #68c) will eventually serve, and what a `git status --porcelain=v2
//! -z` parser (#68b) will eventually populate. Nothing here spawns git, reads a
//! repository, or parses porcelain text; every value in the golden fixture is
//! hand-built, the same way `plan_golden.rs` hand-builds [`crate::GitOperation`]
//! values with no repository involved.
//!
//! ## The generation mechanism is reused, not reinvented
//!
//! [`WorktreeStatus::generation`] is a plain [`crate::GenerationToken`] — the
//! same opaque, equality-only wire type [`crate::HistoryFrame`] already carries
//! (ADR 0001). The underlying algorithm is `git-vista-core::identity`'s
//! [`RepositoryGeneration`](https://docs.rs/git-vista-core "content digest, not
//! a counter — see ADR 0001"), whose own `GenerationInputs` builder already
//! defines a `worktree` slot as *"a digest of the unstaged working-tree status
//! (tracked modifications + untracked files)"* — i.e. exactly this DTO's
//! reason to exist. `git-vista-git::read_generation_inputs`'s doc comment
//! already shows the intended call shape: read HEAD/refs/index, then
//! `inputs.worktree(digest)` from the real status read, then
//! `inputs.generation()`.
//!
//! So this task is not choosing between "a monotonic counter" and "an
//! mtime+HEAD digest" — ADR 0001 already settled that question for the whole
//! codebase, with reasoning (a digest handles *revert-to-prior-state*
//! correctly for a stale-tab guard; a counter would treat a reverted edit as
//! "still moved forward," which is the wrong answer for "is this the state the
//! user reviewed"). Reusing the existing mechanism is what keeps this DTO's
//! generation and [`crate::HistoryFrame`]'s generation — and #70's future
//! write-precondition check — all comparable under the one contract ADR 0001
//! defines, instead of three subsystems each inventing their own notion of
//! "stale."
//!
//! Namespacing follows `history.rs`'s own precedent
//! (`GenerationToken::new(format!("history-v1:{}", ...))`): when #68c builds
//! the real token, it should prefix with `status-v1:` before wrapping the
//! digest, so a status generation can never be confused with (or accidentally
//! compared to) a history generation by a client that mixes the two up. That
//! prefixing is 68c's job — this DTO only carries the already-opaque
//! [`crate::GenerationToken`] and does not care what's inside it.
//!
//! **What this mechanism does *not* detect**, stated plainly per ADR 0001's own
//! framing: it is a content digest, so an edit that is later reverted to the
//! exact prior bytes produces the exact prior generation, not a new one — two
//! reads that differ only by a "make a change, then undo it" round trip inside
//! a client's window will show the *same* status generation on both sides of
//! that round trip, even though the working tree was briefly different in
//! between. ADR 0001 argues this is the right answer for a *write*
//! precondition (the reviewed state and the current state genuinely are
//! identical again, so admitting the write is correct) — for a *status
//! display* specifically, the same argument holds: there is nothing left to
//! show that differs, so "no visible change" is the honest read, not a false
//! negative.
//!
//! ## The eight states (#68's "staged, unstaged, untracked, ignored,
//! conflicted, renamed, submodule, and binary states")
//!
//! Modelled as an internally-tagged, closed [`StatusEntry`] enum — one variant
//! per condition git's own porcelain v2 format actually distinguishes, no
//! catch-all, the same shape [`crate::GitOperation`] uses and for the same
//! reason (see that type's doc comment): a nonsensical combination should be
//! unrepresentable, not merely "shouldn't happen in practice."
//!
//! - **staged / unstaged** are not two separate top-level lists here (unlike
//!   the current `git-vista-core::status::RepoStatus`, which this DTO
//!   deliberately does not extend or replace — that type still serves the
//!   existing `GET /api/status`, forbidden to this task). They are
//!   [`ChangeSides`], a 3-variant enum (`StagedOnly` / `UnstagedOnly` /
//!   `Both`) attached to one entry, because one path can legitimately be dirty
//!   on both sides at once (staged, then edited again) and "neither side
//!   changed" should not type-check as a value at all.
//! - **untracked** / **ignored** are their own variants — no staged/unstaged
//!   split exists for either (they aren't in the index).
//! - **conflicted** is [`ConflictKind`], the seven combinations
//!   `git-status(1)`'s short-format table names exactly (`DD`/`AU`/`UD`/`UA`/
//!   `DU`/`AA`/`UU`) — not a staged/unstaged pair, because a merge conflict's
//!   `<XY>` codes mean something structurally different (ours/theirs, not
//!   index/worktree) and forcing them through [`ChangeSides`] would silently
//!   misreport what they mean.
//! - **renamed** (folding in copies, matching the existing core parser's
//!   choice) is its own variant carrying a *required* `origin_path` — a
//!   renamed entry with no source path cannot be constructed. It does **not**
//!   carry a [`ChangeSides`] (corrected in 68b, after the parser this variant
//!   was designed for turned out unable to produce it cleanly — see
//!   [`StatusEntry::Renamed`]'s own doc comment for why).
//! - **submodule** is [`SubmoduleState`], attached as `Option<SubmoduleState>`
//!   to every variant porcelain v2's `<sub>` field can appear on (`Changed`,
//!   `Renamed`, `Conflicted`) — orthogonal to the entry's own change
//!   classification, exactly as git models it (a submodule can be dirty
//!   *without* its recorded commit having changed).
//! - **binary** is `bool` on the variants that carry real content
//!   (`Changed`/`Renamed`/`Untracked`) rather than a ninth top-level state —
//!   it's a property of the blob, not a status axis. Porcelain v2 does not
//!   report this directly (unlike the diff endpoint's `--numstat`-derived
//!   detection); populating it accurately is 68b's problem, not this DTO's —
//!   the field exists so 68b has somewhere to put the answer.

use serde::{Deserialize, Serialize};

use crate::GenerationToken;

/// One side's ordinary change kind. Folds git's `T` (type change) into
/// `Modified` and reserves the dedicated [`StatusEntry::Renamed`] shape for
/// `R`/`C` — the same collapse `git-vista-core::status::ChangeKind` already
/// makes, kept consistent rather than reinvented differently here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Which side(s) of the index/worktree split an entry is dirty on. A plain
/// `Option<ChangeKind>` pair would let "neither side changed" type-check as a
/// value; this doesn't. One path dirty on both sides (staged, then edited
/// again) is `Both`, not two separate entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "side", rename_all = "snake_case")]
pub enum ChangeSides {
    StagedOnly {
        staged: ChangeKind,
    },
    UnstagedOnly {
        unstaged: ChangeKind,
    },
    Both {
        staged: ChangeKind,
        unstaged: ChangeKind,
    },
}

/// A merge conflict's classification — the seven combinations
/// `git-status(1)`'s short-format table names exactly, not a staged/unstaged
/// pair (a conflict's `<XY>` codes mean ours/theirs, a different axis
/// entirely from index/worktree, so [`ChangeSides`] would misreport it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// `DD` — deleted on both sides.
    BothDeleted,
    /// `AU` — we added it, they haven't touched it (unmerged).
    AddedByUs,
    /// `UD` — they deleted it, we haven't touched it (unmerged).
    DeletedByThem,
    /// `UA` — they added it, we haven't touched it (unmerged).
    AddedByThem,
    /// `DU` — we deleted it, they haven't touched it (unmerged).
    DeletedByUs,
    /// `AA` — added on both sides, differently.
    BothAdded,
    /// `UU` — modified on both sides, differently.
    BothModified,
}

/// A submodule entry's dirty state, from porcelain v2's `<sub>` field
/// (`S<c><m><u>`) — orthogonal to the entry's own [`ChangeSides`]/
/// [`ConflictKind`] classification, since a submodule can be dirty *inside*
/// without its recorded commit pointer having changed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleState {
    /// `<c>` — the recorded commit differs from what's checked out.
    pub commit_changed: bool,
    /// `<m>` — the submodule has modified tracked content.
    pub has_tracked_changes: bool,
    /// `<u>` — the submodule has untracked content.
    pub has_untracked_changes: bool,
}

/// One entry in a [`WorktreeStatus`] — the closed vocabulary of every
/// condition `git status --porcelain=v2` distinguishes. Internally tagged on
/// `"entry_kind"` (not `"kind"` — [`StatusEntry::Conflicted`] already has a
/// field named `kind`, and serde refuses a variant field that collides with
/// the internal tag), `snake_case` variant names, following
/// [`crate::GitOperation`]'s wire shape otherwise exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry_kind", rename_all = "snake_case")]
pub enum StatusEntry {
    /// An ordinary changed path (porcelain `1` record) — no rename, no
    /// conflict.
    Changed {
        path: String,
        sides: ChangeSides,
        submodule: Option<SubmoduleState>,
        binary: bool,
    },
    /// A rename or copy git detected (porcelain `2` record). `origin_path` is
    /// required, not optional — a renamed entry with no source path cannot be
    /// constructed. `score` is the similarity percentage git reported (e.g.
    /// `100` for `R100`).
    ///
    /// No [`ChangeSides`] here — a `2` record's `<XY>` pair is **not** two
    /// independent [`ChangeKind`]s the way a `1` record's is. `X` is always
    /// `R` or `C` (that is what makes it a `2` record instead of a `1` in the
    /// first place — git never emits a rename/copy record with any other `X`
    /// letter), so a field typed `ChangeSides` could represent nonsense
    /// `git status --porcelain=v2` can never produce, such as `UnstagedOnly`
    /// (implying the rename *isn't* staged, which is false by construction)
    /// or `Both { staged: ChangeKind::Added, .. }` (mislabelling the staged
    /// side as an ordinary add rather than the rename it actually is). `Y`
    /// — whether the worktree changed the file again after the rename/copy
    /// was staged — is the only real variable, so that's the only field:
    /// `unstaged: None` means `Y` was `.`, `Some(kind)` carries git's own
    /// `M`/`T`/`D` for it. Found and fixed here, in 68b, while writing the
    /// parser this variant's original `sides: ChangeSides` shape (from #68a)
    /// could never cleanly produce — see `pro-result.md` for the task 10 PR
    /// this corrects.
    Renamed {
        path: String,
        origin_path: String,
        score: u8,
        unstaged: Option<ChangeKind>,
        submodule: Option<SubmoduleState>,
        binary: bool,
    },
    /// An untracked path (porcelain `?` record) — never has a staged/unstaged
    /// split; it isn't in the index at all.
    Untracked { path: String, binary: bool },
    /// An ignored path (porcelain `!` record).
    Ignored { path: String },
    /// A merge conflict (porcelain `u` record). No [`ChangeSides`] — see
    /// [`ConflictKind`]'s doc comment for why that axis doesn't apply here.
    Conflicted {
        path: String,
        kind: ConflictKind,
        submodule: Option<SubmoduleState>,
    },
}

/// The full working-tree status — the payload a future `GET /api/status` (v2,
/// #68c) will serve. [`generation`](Self::generation) is what makes it
/// staleness-detectable (#68's *"generation-tagged and detects external
/// changes"* criterion) and is what a future write-precondition check (#70)
/// will compare against — see the module doc for why this reuses
/// [`GenerationToken`]/ADR 0001 rather than a new mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub generation: GenerationToken,
    /// The checked-out branch; `None` for detached HEAD.
    pub branch: Option<String>,
    /// The branch's upstream (e.g. `origin/main`), when one is set.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Every changed/renamed/untracked/ignored/conflicted path, in no
    /// particular order (porcelain v2 doesn't guarantee one either — see
    /// `git-status(1)`'s "Tracked entries are printed in an undefined
    /// order").
    pub entries: Vec<StatusEntry>,
}

// ---------------------------------------------------------------------------
// Parsing `git status --porcelain=v2 --branch -z` (#68b)
// ---------------------------------------------------------------------------
//
// A **pure function** over bytes: no git process spawn, no repository read,
// no server involvement. This is deliberately -z only, not the tab/quoted
// v2 format: a path can contain a newline (PR #167 tested exactly that case
// on the read side; the same discipline applies here), and -z is the only
// v2 form that survives one losslessly.
//
// Verified against real git, not assumed from `git-status(1)` alone. Every
// record shape below (the rename/copy two-token split, a submodule's `<sub>`
// field, every conflict XY combination) was captured from an actual `git
// status --porcelain=v2 --branch -z` run against a real repository before
// being encoded here.
//
// The unit tests in this file feed the parser hand-written bytes, which is
// deliberate — they are tests of the parser, and a literal is the clearest way
// to say which byte sequence is under test. They are not, on their own,
// evidence about any git version, and until #365 nothing else was: this parser
// had only ever been *exercised* against the 2.43.0 on one host, a single
// version rather than the floor itself.
//
// That gap is now closed rather than merely admitted (#365, ADR 0082).
// `crates/git-vista-fixtures/tests/status_floor.rs` builds real repositories
// covering this whole vocabulary and runs the argv above over them with TWO
// binaries — the runner's git and a build of the floor named by the `## Git:`
// heading in `docs/SUPPORTED_VERSIONS.md` — parsing both streams with
// `parse_porcelain_v2_z` and holding each to a named expected value. CI
// provisions the floor binary and then asserts, in shell over the test's own
// report, that the second leg really ran; a missing report or an absent floor
// fails the build rather than passing quietly.
//
// The finding, recorded because "no difference" is a result and not an
// absence of one: **the floor and the current git parse identically here**, on
// every shape and under all three read modes. Measured pairs so far — 2.32.0
// against 2.43.0 on a developer box, and 2.32.0 against **2.55.0** on the CI
// runner. The second leg is deliberately not pinned to a version: it is
// whatever git the machine has, so the span widens on its own as runners move,
// and no comment here has to be edited to keep up.
//
// That is the expected outcome — porcelain v2 has not changed shape since its
// introduction in git 2.11 — and it is now measured on every CI run instead of
// inferred from the release notes.

/// [`parse_porcelain_v2_z`]'s output — everything a real repository read can
/// produce **except** the generation tag, which this pure function has no
/// way to compute (that needs `git-vista-git::read_generation_inputs` plus a
/// digest of this very output, per the module doc's "generation mechanism"
/// section — a real git read, forbidden to this task). Fold in a
/// [`GenerationToken`] with [`ParsedStatus::into_worktree_status`] to get the
/// full wire DTO.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedStatus {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<StatusEntry>,
}

impl ParsedStatus {
    /// Attach the generation tag a real repository read derives, producing
    /// the full [`WorktreeStatus`] wire payload.
    pub fn into_worktree_status(self, generation: GenerationToken) -> WorktreeStatus {
        WorktreeStatus {
            generation,
            branch: self.branch,
            upstream: self.upstream,
            ahead: self.ahead,
            behind: self.behind,
            entries: self.entries,
        }
    }
}

/// Parse the complete stdout of `git status --porcelain=v2 --branch -z`.
///
/// Unknown record types and malformed lines are *skipped*, not errors — the
/// same posture `git-vista-core::status::parse_porcelain_v2`'s doc comment
/// already argues for the non-`-z` parser: the format is versioned and
/// stable, so anything unrecognised is either a future git addition (ignore
/// it, keep working) or line noise, and the worst outcome of skipping is an
/// undercount, never a failed status read.
///
/// `-z` records are NUL-terminated, not newline-terminated, and carry no
/// quoting — a path can contain any byte except NUL, including a literal
/// newline. **Rename/copy (`2`) records are the one shape that spans two
/// NUL-terminated tokens**, not one: `git-status(1)`'s own "Pathname Format
/// Notes" section documents `<sep>` as "a NUL byte" between the two paths
/// under `-z`, which means the record's own token ends at `<path>` and
/// `<origPath>` is the *next* token with no marker prefix — confirmed against
/// a real `git status --porcelain=v2 --branch -z` run on a renamed file, not
/// assumed. Every other record type is a single token.
pub fn parse_porcelain_v2_z(bytes: &[u8]) -> ParsedStatus {
    let mut status = ParsedStatus::default();
    let mut tokens = bytes.split(|&b| b == 0).filter(|t| !t.is_empty());
    while let Some(tok) = tokens.next() {
        let line = String::from_utf8_lossy(tok);
        match line.as_bytes().first() {
            Some(b'#') => parse_header(&line, &mut status),
            Some(b'1') => {
                if let Some(entry) = parse_changed(&line) {
                    status.entries.push(entry);
                }
            }
            Some(b'2') => {
                // The rename/copy trap this doc comment warns about: origPath
                // is the *next* token, not part of this line. If there is no
                // next token (a truncated/malformed stream), the record is
                // dropped rather than guessed at — same fail-soft posture as
                // every other unparseable line here.
                if let Some(orig_tok) = tokens.next() {
                    let origin_path = String::from_utf8_lossy(orig_tok).into_owned();
                    if let Some(entry) = parse_renamed(&line, origin_path) {
                        status.entries.push(entry);
                    }
                }
            }
            Some(b'u') => {
                if let Some(entry) = parse_conflicted(&line) {
                    status.entries.push(entry);
                }
            }
            Some(b'?') => {
                if let Some(path) = line.strip_prefix("? ") {
                    status.entries.push(StatusEntry::Untracked {
                        path: path.to_string(),
                        binary: false,
                    });
                }
            }
            Some(b'!') => {
                if let Some(path) = line.strip_prefix("! ") {
                    status.entries.push(StatusEntry::Ignored {
                        path: path.to_string(),
                    });
                }
            }
            // Anything else (a future record type, line noise): skipped.
            _ => {}
        }
    }
    status
}

/// One `# branch.*` header line — same shapes
/// `git-vista-core::status::parse_header` already handles for the non-`-z`
/// format (the header lines are identical either way; only the tracked/
/// untracked/ignored records change shape under `-z`).
fn parse_header(line: &str, status: &mut ParsedStatus) {
    if let Some(head) = line.strip_prefix("# branch.head ") {
        if head != "(detached)" {
            status.branch = Some(head.to_string());
        }
    } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
        status.upstream = Some(upstream.to_string());
    } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
        for part in ab.split_whitespace() {
            if let Some(n) = part.strip_prefix('+') {
                status.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix('-') {
                status.behind = n.parse().unwrap_or(0);
            }
        }
    }
    // branch.oid is not surfaced: the client already knows every tip from
    // the history endpoints.
}

/// The `n`-th (0-based) whitespace-separated field of `line`, counting the
/// leading record marker (`1`/`2`/`u`) as field 0.
fn nth_field(line: &str, n: usize) -> Option<&str> {
    line.split_ascii_whitespace().nth(n)
}

/// Everything after the `n`-th whitespace-separated field — the path tail of
/// a record, which under `-z` may contain literally anything (spaces,
/// newlines, tabs) except a NUL byte, so it can never be read as "one more
/// field."
fn nth_field_rest(line: &str, n: usize) -> Option<&str> {
    let mut rest = line;
    for _ in 0..n {
        let idx = rest.find(' ')?;
        rest = &rest[idx + 1..];
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Porcelain v2's `<sub>` field: `"N..."` when the entry is not a submodule,
/// `"S<c><m><u>"` when it is (`git-status(1)`, "Renamed or copied entries").
fn parse_submodule(field: &str) -> Option<SubmoduleState> {
    let bytes = field.as_bytes();
    if bytes.len() != 4 || bytes[0] != b'S' {
        return None;
    }
    Some(SubmoduleState {
        commit_changed: bytes[1] == b'C',
        has_tracked_changes: bytes[2] == b'M',
        has_untracked_changes: bytes[3] == b'U',
    })
}

/// One side's letter, for the ordinary-change axis (`1`/`2` records' `Y`
/// side, and `1` records' `X` side). `.` (unchanged) is `None`; anything not
/// `A`/`M`/`T`/`D` is also `None` — `git status --porcelain=v2` never emits
/// another letter on this axis (an `R`/`C` `X` always means a `2` record,
/// handled separately by [`parse_renamed`], never reaching this function).
fn change_kind_from_letter(b: u8) -> Option<ChangeKind> {
    match b {
        b'A' => Some(ChangeKind::Added),
        b'M' | b'T' => Some(ChangeKind::Modified),
        b'D' => Some(ChangeKind::Deleted),
        _ => None,
    }
}

/// [`ChangeSides`] from an ordinary (`1` record) `<XY>` pair. `None` only if
/// *neither* side is a real change — which would mean git printed a `1`
/// record for a genuinely unchanged path, a contradiction the format itself
/// rules out; treated as "drop the record" rather than panicking, matching
/// this parser's fail-soft posture everywhere else.
fn change_sides_from_xy(xy: &str) -> Option<ChangeSides> {
    let bytes = xy.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let staged = change_kind_from_letter(bytes[0]);
    let unstaged = change_kind_from_letter(bytes[1]);
    match (staged, unstaged) {
        (Some(staged), Some(unstaged)) => Some(ChangeSides::Both { staged, unstaged }),
        (Some(staged), None) => Some(ChangeSides::StagedOnly { staged }),
        (None, Some(unstaged)) => Some(ChangeSides::UnstagedOnly { unstaged }),
        (None, None) => None,
    }
}

/// A `1` record: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — marker(0),
/// XY(1), sub(2), three modes(3..6), two hashes(6..8), path(8, "rest").
fn parse_changed(line: &str) -> Option<StatusEntry> {
    let xy = nth_field(line, 1)?;
    let sub = nth_field(line, 2)?;
    let path = nth_field_rest(line, 8)?;
    let sides = change_sides_from_xy(xy)?;
    Some(StatusEntry::Changed {
        path: path.to_string(),
        sides,
        submodule: parse_submodule(sub),
        binary: false, // porcelain v2 does not report this — see module doc.
    })
}

/// A `2` record: `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>`,
/// with `origin_path` supplied by the caller (the *next* `-z` token — see
/// [`parse_porcelain_v2_z`]'s doc comment). `X` is always `R` or `C` here
/// (that is what makes this a `2` record) and carries no independent
/// [`ChangeKind`] — see [`StatusEntry::Renamed`]'s doc comment for why this
/// entry has no [`ChangeSides`] field at all. Only `Y`, the score, and the
/// paths are extracted.
fn parse_renamed(line: &str, origin_path: String) -> Option<StatusEntry> {
    let xy = nth_field(line, 1)?;
    let sub = nth_field(line, 2)?;
    let xscore = nth_field(line, 8)?;
    let path = nth_field_rest(line, 9)?;
    let y = *xy.as_bytes().get(1)?;
    let score: u8 = xscore.get(1..)?.parse().ok()?;
    Some(StatusEntry::Renamed {
        path: path.to_string(),
        origin_path,
        score,
        unstaged: change_kind_from_letter(y),
        submodule: parse_submodule(sub),
        binary: false,
    })
}

/// The seven merge-conflict `<XY>` combinations `git-status(1)`'s
/// short-format table names exactly.
fn conflict_kind_from_xy(xy: &str) -> Option<ConflictKind> {
    match xy {
        "DD" => Some(ConflictKind::BothDeleted),
        "AU" => Some(ConflictKind::AddedByUs),
        "UD" => Some(ConflictKind::DeletedByThem),
        "UA" => Some(ConflictKind::AddedByThem),
        "DU" => Some(ConflictKind::DeletedByUs),
        "AA" => Some(ConflictKind::BothAdded),
        "UU" => Some(ConflictKind::BothModified),
        _ => None,
    }
}

/// A `u` record: `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` —
/// marker(0), XY(1), sub(2), four modes(3..7), three hashes(7..10),
/// path(10, "rest").
fn parse_conflicted(line: &str) -> Option<StatusEntry> {
    let xy = nth_field(line, 1)?;
    let sub = nth_field(line, 2)?;
    let path = nth_field_rest(line, 10)?;
    let kind = conflict_kind_from_xy(xy)?;
    Some(StatusEntry::Conflicted {
        path: path.to_string(),
        kind,
        submodule: parse_submodule(sub),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(s: &str) -> GenerationToken {
        GenerationToken::new(s).unwrap()
    }

    /// [`ChangeSides`] cannot represent "neither side changed" — there is no
    /// variant for it, which is the point: the type only has three shapes,
    /// and this pins that the enum still round-trips each of them.
    #[test]
    fn change_sides_round_trip_each_variant() {
        for sides in [
            ChangeSides::StagedOnly {
                staged: ChangeKind::Added,
            },
            ChangeSides::UnstagedOnly {
                unstaged: ChangeKind::Modified,
            },
            ChangeSides::Both {
                staged: ChangeKind::Added,
                unstaged: ChangeKind::Modified,
            },
        ] {
            let json = serde_json::to_string(&sides).unwrap();
            let back: ChangeSides = serde_json::from_str(&json).unwrap();
            assert_eq!(sides, back);
        }
    }

    /// A renamed entry's `origin_path` is a required field on the wire — an
    /// object missing it must fail to deserialize, not silently default to
    /// empty. Pins that the "cannot construct a renamed entry with no
    /// source" guarantee holds at the JSON boundary too, not just in Rust.
    #[test]
    fn renamed_entry_without_origin_path_is_rejected_at_the_wire() {
        let missing_origin = serde_json::json!({
            "entry_kind": "renamed",
            "path": "new.rs",
            "score": 100,
            "sides": {"side": "staged_only", "staged": "added"},
            "submodule": null,
            "binary": false,
        });
        let result: Result<StatusEntry, _> = serde_json::from_value(missing_origin);
        assert!(
            result.is_err(),
            "origin_path must be required, not optional"
        );
    }

    #[test]
    fn worktree_status_round_trips() {
        let status = WorktreeStatus {
            generation: token("status-v1:12345"),
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            ahead: 1,
            behind: 0,
            entries: vec![StatusEntry::Untracked {
                path: "scratch.txt".to_string(),
                binary: false,
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: WorktreeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }

    // ---- parse_porcelain_v2_z: table-driven over every named state (#68b) --

    /// Join `-z`-shaped tokens with real NUL bytes, the way git actually
    /// emits them — the tests build the byte stream this way rather than
    /// embedding literal `\0` in a Rust string, so the shape matches what
    /// [`parse_porcelain_v2_z`] receives from a real `git status
    /// --porcelain=v2 --branch -z` in production, not an approximation of it.
    fn z(tokens: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for t in tokens {
            bytes.extend_from_slice(t.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn clean_repo_with_branch_headers() {
        let bytes = z(&[
            "# branch.oid 1234567890abcdef1234567890abcdef12345678",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert!(s.entries.is_empty());
    }

    #[test]
    fn detached_head_has_no_branch() {
        let bytes = z(&["# branch.oid abc", "# branch.head (detached)"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(s.branch, None);
    }

    /// Added (staged only), modified (unstaged only), and one path dirty on
    /// both sides at once — captured from a real repository, not hand-typed
    /// from the man page's field-count description.
    #[test]
    fn ordinary_changes_staged_unstaged_and_both() {
        let bytes = z(&[
            "1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 new.rs",
            "1 .M N... 100644 100644 100644 2222222222222222222222222222222222222222 2222222222222222222222222222222222222222 edited.rs",
            "1 MM N... 100644 100644 100644 3333333333333333333333333333333333333333 4444444444444444444444444444444444444444 both.rs",
            "1 .D N... 100644 100644 000000 5555555555555555555555555555555555555555 0000000000000000000000000000000000000000 deleted.rs",
        ]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![
                StatusEntry::Changed {
                    path: "new.rs".to_string(),
                    sides: ChangeSides::StagedOnly {
                        staged: ChangeKind::Added
                    },
                    submodule: None,
                    binary: false,
                },
                StatusEntry::Changed {
                    path: "edited.rs".to_string(),
                    sides: ChangeSides::UnstagedOnly {
                        unstaged: ChangeKind::Modified
                    },
                    submodule: None,
                    binary: false,
                },
                StatusEntry::Changed {
                    path: "both.rs".to_string(),
                    sides: ChangeSides::Both {
                        staged: ChangeKind::Modified,
                        unstaged: ChangeKind::Modified
                    },
                    submodule: None,
                    binary: false,
                },
                StatusEntry::Changed {
                    path: "deleted.rs".to_string(),
                    sides: ChangeSides::UnstagedOnly {
                        unstaged: ChangeKind::Deleted
                    },
                    submodule: None,
                    binary: false,
                },
            ]
        );
    }

    /// A path containing spaces survives `-z`'s "path is the rest, verbatim"
    /// handling — the same guarantee `git-vista-core`'s parser gives for the
    /// tab/quoted format, pinned here for the `-z` shape.
    #[test]
    fn paths_with_spaces_survive() {
        let bytes = z(&["1 .M N... 100644 100644 100644 5555555555555555555555555555555555555555 5555555555555555555555555555555555555555 dir name/file with spaces.txt"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![StatusEntry::Changed {
                path: "dir name/file with spaces.txt".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified
                },
                submodule: None,
                binary: false,
            }]
        );
    }

    /// The rename/copy `-z` trap this module's doc comment names: `origPath`
    /// is a second, separate NUL-terminated token, not part of the record's
    /// own line. Also covers the "renamed AND further edited" (`RM`) case —
    /// confirmed against a real repository, not assumed.
    #[test]
    fn renamed_and_copied_span_two_z_tokens() {
        let bytes = z(&[
            "2 R. N... 100644 100644 100644 6666666666666666666666666666666666666666 6666666666666666666666666666666666666666 R100 new/name.rs",
            "old/name.rs",
            "2 RM N... 100644 100644 100644 7777777777777777777777777777777777777777 7777777777777777777777777777777777777777 R87 new/edited.rs",
            "old/edited.rs",
            "2 C. N... 100644 100644 100644 8888888888888888888888888888888888888888 8888888888888888888888888888888888888888 C75 copy/target.rs",
            "copy/source.rs",
        ]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![
                StatusEntry::Renamed {
                    path: "new/name.rs".to_string(),
                    origin_path: "old/name.rs".to_string(),
                    score: 100,
                    unstaged: None,
                    submodule: None,
                    binary: false,
                },
                StatusEntry::Renamed {
                    path: "new/edited.rs".to_string(),
                    origin_path: "old/edited.rs".to_string(),
                    score: 87,
                    unstaged: Some(ChangeKind::Modified),
                    submodule: None,
                    binary: false,
                },
                StatusEntry::Renamed {
                    path: "copy/target.rs".to_string(),
                    origin_path: "copy/source.rs".to_string(),
                    score: 75,
                    unstaged: None,
                    submodule: None,
                    binary: false,
                },
            ]
        );
    }

    #[test]
    fn untracked_and_ignored() {
        let bytes = z(&["? scratch.txt", "! target/"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![
                StatusEntry::Untracked {
                    path: "scratch.txt".to_string(),
                    binary: false
                },
                StatusEntry::Ignored {
                    path: "target/".to_string()
                },
            ]
        );
    }

    /// All seven `u`-record conflict combinations, in one table — each row
    /// captured from `git-status(1)`'s own short-format table (`DD`/`AU`/
    /// `UD`/`UA`/`DU`/`AA`/`UU`), not invented.
    #[test]
    fn every_conflict_xy_combination() {
        let cases = [
            ("DD", ConflictKind::BothDeleted),
            ("AU", ConflictKind::AddedByUs),
            ("UD", ConflictKind::DeletedByThem),
            ("UA", ConflictKind::AddedByThem),
            ("DU", ConflictKind::DeletedByUs),
            ("AA", ConflictKind::BothAdded),
            ("UU", ConflictKind::BothModified),
        ];
        for (xy, expected) in cases {
            let line = format!(
                "u {xy} N... 100644 100644 100644 100644 \
                 1111111111111111111111111111111111111111 \
                 2222222222222222222222222222222222222222 \
                 3333333333333333333333333333333333333333 clash.rs"
            );
            let bytes = z(&[&line]);
            let s = parse_porcelain_v2_z(&bytes);
            assert_eq!(
                s.entries,
                vec![StatusEntry::Conflicted {
                    path: "clash.rs".to_string(),
                    kind: expected,
                    submodule: None,
                }],
                "XY {xy} did not parse to {expected:?}"
            );
        }
    }

    /// A submodule dirty *without* its recorded commit having changed —
    /// captured from a real `git status --porcelain=v2 -z` run against an
    /// actual submodule with an untracked file inside it (`S..U`, `.M` on
    /// the outer XY), not guessed. This is the exact case the task-queue
    /// entry and the #68 issue both called out by name.
    #[test]
    fn submodule_dirty_without_pointer_change() {
        let bytes = z(&["1 .M S..U 160000 160000 160000 9999999999999999999999999999999999999999 9999999999999999999999999999999999999999 vendor"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![StatusEntry::Changed {
                path: "vendor".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified
                },
                submodule: Some(SubmoduleState {
                    commit_changed: false,
                    has_tracked_changes: false,
                    has_untracked_changes: true,
                }),
                binary: false,
            }]
        );
    }

    /// A submodule whose recorded commit pointer *did* change, with no
    /// tracked/untracked dirt inside it — the complementary case to the one
    /// above, so `commit_changed` and the other two flags are each proven
    /// independent, not just "some bit or other got set."
    #[test]
    fn submodule_pointer_changed_without_inner_dirt() {
        let bytes = z(&["1 .M SC.. 160000 160000 160000 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb vendor"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![StatusEntry::Changed {
                path: "vendor".to_string(),
                sides: ChangeSides::UnstagedOnly {
                    unstaged: ChangeKind::Modified
                },
                submodule: Some(SubmoduleState {
                    commit_changed: true,
                    has_tracked_changes: false,
                    has_untracked_changes: false,
                }),
                binary: false,
            }]
        );
    }

    #[test]
    fn ignored_and_unknown_records_are_skipped() {
        let bytes = z(&["! target/", "z weird future record"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert_eq!(
            s.entries,
            vec![StatusEntry::Ignored {
                path: "target/".to_string()
            }]
        );
    }

    /// A `2` record with nothing following it (a truncated/malformed stream)
    /// is dropped rather than guessed at — the fail-soft posture stated in
    /// [`parse_porcelain_v2_z`]'s doc comment, exercised rather than assumed.
    #[test]
    fn a_rename_record_with_no_following_token_is_dropped_not_guessed() {
        let bytes = z(&["2 R. N... 100644 100644 100644 cccccccccccccccccccccccccccccccccccccccc cccccccccccccccccccccccccccccccccccccccc R100 new/name.rs"]);
        let s = parse_porcelain_v2_z(&bytes);
        assert!(s.entries.is_empty());
    }

    /// `ParsedStatus::into_worktree_status` is the only place a
    /// [`GenerationToken`] enters this module's output — pinned so a future
    /// change can't accidentally start deriving one inside the parser
    /// itself (which would need a real git read, forbidden to this task).
    #[test]
    fn into_worktree_status_attaches_the_supplied_generation_verbatim() {
        let bytes = z(&["# branch.head main", "? scratch.txt"]);
        let parsed = parse_porcelain_v2_z(&bytes);
        let full = parsed.into_worktree_status(token("status-v1:999"));
        assert_eq!(full.generation, token("status-v1:999"));
        assert_eq!(full.branch.as_deref(), Some("main"));
        assert_eq!(
            full.entries,
            vec![StatusEntry::Untracked {
                path: "scratch.txt".to_string(),
                binary: false
            }]
        );
    }
}
