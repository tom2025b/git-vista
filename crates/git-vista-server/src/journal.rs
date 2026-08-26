//! The app's operation journal and the branch-tip snapshots — the two pieces
//! of server-side state behind the activity feed, both living under
//! `.git/git-vista/` in the served repository.
//!
//! **Journal** (`journal.jsonl`): one JSON [`ActivityEvent`] per line, appended
//! by every write endpoint the moment its git command succeeds. It's what lets
//! the feed (a) attribute an event to the app rather than "the terminal", and
//! (b) undo a branch deletion — git deletes a branch's reflog *with* the
//! branch, so the journal is the only place its last tip survives.
//!
//! **Snapshot** (`refs.json`): the local branch → tip map as of the last feed
//! read. A branch present in the snapshot but missing from the repo — with no
//! journal record of the app deleting it — was deleted *outside* the app; the
//! feed synthesizes a deletion event (carrying the snapshot's tip, so even
//! terminal deletions get a Restore) and journals it so it's remembered once.
//!
//! Location rationale: inside `.git` so it's per-repository, survives server
//! restarts, travels with the repo, and can never be committed. Everything
//! here is best-effort by design — a journal that can't be written degrades
//! the feed's attribution, which must never break the git operation itself.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use git_vista_core::activity::{ActivityEvent, CapturedRefs, RefsAtEvent, REFS_PER_EVENT_CAP};
use git_vista_core::model::{GitRef, RefKind};
use git_vista_git::read_refs_at;

/// Only this many of the newest journal lines are read back. The journal is
/// append-only and unbounded; the feed shows nothing like this many events.
const JOURNAL_READ_CAP: usize = 1_000;

/// The journal format this binary **writes**, stamped on every line
/// [`append_all`] appends (#521, ADR 0085).
///
/// **Version 1** is: one JSON object per line, the object being an
/// [`ActivityEvent`]'s own fields plus this stamp, its `refs` carrying one of
/// the five [`RefsAtEvent`] answers.
///
/// A line with **no** stamp is not version 0 — it is a line written before the
/// stamp existed, which is every line on disk today. [`ReadLine::v`] keeps
/// those apart for the same reason [`RefsAtEvent`] keeps "not recorded" apart
/// from "recorded as empty".
///
/// **What this buys, stated honestly, because it is easy to overclaim.** It
/// does nothing for a binary that is already built: a pre-#485 git-vista still
/// drops every `in_batch` line whatever is stamped beside it, and no code here
/// can change that. What it buys is the *next* format change — from this
/// commit forward a reader can say "this line came from a writer newer than
/// me" at read time instead of guessing from a serde error, and
/// [`RefsAtEvent::Unknown`] means the line survives long enough for it to say
/// so. ADR 0085 has the full split between the past and the future.
const JOURNAL_FORMAT_VERSION: u32 = 1;

/// The state directory, `.git/git-vista/`, if this repo has a real `.git`
/// *directory*. (A linked worktree's `.git` is a file; journaling is quietly
/// skipped there rather than guessed at.) Public because the test-repo seed
/// files (`seed-refs` / `seed-head` / `seed.bundle`, written by `gv --seed`)
/// live in the same directory.
pub fn state_dir(repo: &Path) -> Option<PathBuf> {
    let git = repo.join(".git");
    git.is_dir().then(|| git.join("git-vista"))
}

/// One journal line on the way **out**: the format stamp, then the event's own
/// fields flattened beside it (#521, ADR 0085).
///
/// **The line is not the event; the line is a versioned envelope around one.**
/// That distinction is the whole structural change, and it is why the stamp
/// lives here rather than as a field on [`ActivityEvent`]: `ActivityEvent` is
/// *also* the wire DTO of `/api/activity`, and the version of a file on disk
/// means nothing to a browser — which has its own negotiation in
/// `git_vista_protocol`. Putting it on the event would mean stamping it in the
/// writer and stripping it in the handler, one fact in two places.
///
/// `v` is serialised first, so a line read by eye says what wrote it before
/// anything else.
#[derive(serde::Serialize)]
struct WrittenLine<'a> {
    v: u32,
    #[serde(flatten)]
    event: &'a ActivityEvent,
}

/// One journal line on the way **in**: whatever stamp it carries, if any, plus
/// the event.
///
/// `v: None` is an unstamped line — pre-#521, i.e. everything already on disk
/// — and is deliberately not `#[serde(default)]`-ed to `0`, which would be a
/// claim the line never made.
///
/// The stamp is invisible to readers that do not know about it, which is what
/// makes adding it safe: [`ActivityEvent`] does not set `deny_unknown_fields`,
/// so `serde_json::from_str::<ActivityEvent>` — the line of code this reader
/// used to be, and the line of code a *pre-#485* binary still is — parses a
/// stamped line exactly as it parsed an unstamped one and ignores the `v`.
/// [`tests::a_stamped_line_still_parses_through_the_bare_activity_event_path`]
/// pins that against the bare path rather than this one, because that is the
/// code whose behaviour is being claimed.
#[derive(serde::Deserialize)]
struct ReadLine {
    #[serde(default)]
    v: Option<u32>,
    #[serde(flatten)]
    event: ActivityEvent,
}

/// The part of a journal envelope that must remain readable even when the
/// flattened event is from a format this binary cannot deserialize.
///
/// Probing this separately is the stamp's diagnostic value: an unknown future
/// event kind or a missing required event field can still be attributed to the
/// newer writer before full event decoding fails.
#[derive(serde::Deserialize)]
struct VersionProbe {
    #[serde(default)]
    v: Option<u32>,
}

fn journal_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("journal.jsonl"))
}

fn snapshot_path(repo: &Path) -> Option<PathBuf> {
    state_dir(repo).map(|d| d.join("refs.json"))
}

/// Collect one ref kind into a [`CapturedRefs`], capped at
/// [`REFS_PER_EVENT_CAP`] entries by name order.
///
/// `truncated_at` carries the true count whenever the repo held more than the
/// cap — never a silently short map, which a replayer would read as "the rest
/// were deleted". The cap is applied here, per kind, so one kind overflowing
/// can never evict another's entries.
fn collect(refs: &[GitRef], kind: RefKind) -> CapturedRefs {
    let mut entries: BTreeMap<String, String> = refs
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| (r.name.clone(), r.target.0.clone()))
        .collect();
    let total = entries.len();
    let truncated_at = (total > REFS_PER_EVENT_CAP).then_some(total);
    if truncated_at.is_some() {
        let keep: Vec<String> = entries.keys().take(REFS_PER_EVENT_CAP).cloned().collect();
        entries.retain(|name, _| keep.binary_search(name).is_ok());
    }
    CapturedRefs {
        entries,
        truncated_at,
    }
}

/// Read the repo's refs for journaling with an event: HEAD, local branches,
/// tags and remote-tracking refs (#131, extended by #449).
///
/// The return type is the point. A failed read yields
/// [`RefsAtEvent::CaptureFailed`] carrying the reason — never an empty map,
/// which a replayer would read as "every branch was deleted at this instant".
/// An empty map is reserved for the genuine observation of a repo with no
/// branches, which is a real state a fresh repo is in.
///
/// Everything comes from **one** [`read_refs_at`] call, so HEAD and the three
/// maps describe the same instant rather than three successive ones.
///
/// Why HEAD and tags at all: #131's snapshot exists so "a future time
/// scrubber can replay history losslessly", and a snapshot of local branches
/// alone cannot show the HEAD moving — the one thing such a scrubber is for.
/// Why remote-tracking refs: the story a scrubber mostly tells is divergence,
/// "your branch moved, origin did not", and local branches alone cannot tell
/// it. See ADR 0070.
pub fn capture_refs(repo: &Path) -> RefsAtEvent {
    let read = match read_refs_at(repo) {
        Ok(read) => read,
        Err(e) => {
            return RefsAtEvent::CaptureFailed {
                reason: e.to_string(),
            }
        }
    };
    let branches = collect(&read.refs, RefKind::Branch);
    RefsAtEvent::Captured {
        branches: branches.entries,
        truncated_at: branches.truncated_at,
        head: Some(read.head),
        tags: Some(collect(&read.refs, RefKind::Tag)),
        remotes: Some(collect(&read.refs, RefKind::RemoteBranch)),
        // A bare capture anchors nothing; `append_all` stamps the batch id
        // when it is sharing this one snapshot across several lines.
        batch: None,
    }
}

/// Append one event to the journal, creating the directory on first use.
/// Best-effort: failure is logged to the terminal and swallowed — the git
/// operation this records already succeeded, and must stay succeeded.
///
/// The branch-tip capture (#131) happens *here* rather than at each call site,
/// so no caller can forget it and no future write endpoint can quietly ship
/// without history. An event that arrives already carrying `refs` keeps its
/// own — the feed's synthesized external-deletion event needs to attach the
/// map as it stood *before* the deletion it just noticed.
///
/// A batch of one, and deliberately spelled as one: the twenty-odd endpoints
/// that record one event apiece were untouched by #485 and are untouched by
/// #521, because both changes land in [`append_all`] and this is `append_all`
/// with one event.
///
/// The line itself is no longer byte-for-byte what #485 left — it gained the
/// `"v"` stamp (#521, ADR 0085), which ADR 0080 D1's "byte-for-byte unchanged"
/// sentence predates. What that sentence was promising — no call-site change,
/// and a single-event line that still carries its own capture rather than a
/// pointer — both still hold.
pub fn append(repo: &Path, event: &ActivityEvent) {
    append_all(repo, std::slice::from_ref(event));
}

/// A journal write batch id: unique among **concurrently-live processes** on
/// one box, and ordered enough to be legible in a file anyone may end up
/// reading by eye. Everything else treats it as an opaque string —
/// [`git_vista_core::activity::refs_at`] compares whole ids, nothing parses
/// the pieces.
///
/// Wall-clock nanoseconds, the process id, and a process-local counter. The
/// counter alone repeats after a restart; the clock alone repeats if two
/// batches land in the same nanosecond (and can go backwards, or collapse to
/// 0 on a pre-epoch clock — `unwrap_or(0)` below). The pid is what makes two
/// processes journalling their first batch in the same tick mint *different*
/// ids (#519): before it was added, both minted `<same nanos>-0` and a
/// referrer could resolve to the *other* process's ref map, because `refs_at`
/// takes the first matching anchor.
///
/// Still NOT guaranteed: uniqueness across pid reuse (two boots, or a pid
/// recycled after exit, plus an identical clock reading). Acceptable because
/// the id only has to be unambiguous within one [`JOURNAL_READ_CAP`] read
/// window, and a recycled pid colliding inside the same nanosecond within one
/// window would need the clock itself to have stood still or gone backwards.
fn mint_batch_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Append a whole operation's events under **one** ref capture (#485,
/// ADR 0080).
///
/// # Why this exists
///
/// [`capture_refs`] is a full ref read of the repository, and what it produces
/// is up to `REFS_PER_EVENT_CAP` branches, tags *and* remote-tracking refs —
/// tens of kilobytes. Calling [`append`] in a loop therefore pays both costs
/// once per event, and for the one caller whose event count tracks the
/// repository's ref count — a fetch — both are quadratic in the refs it moved.
/// Measured on 2026-08-25 before this function existed: a 500-ref fetch spent
/// **27.6 s** journalling, on the user's latency, and left a 14 MiB journal
/// whose lines averaged 28,872 bytes against a single event's 537.
///
/// # What one line means afterwards
///
/// The capture is read once and stored once. The batch's **last** event that
/// needs one carries it, stamped with a batch id; the earlier ones carry
/// [`RefsAtEvent::InBatch`] naming that id. So the maps are still recorded for
/// every event — a replayer asks [`git_vista_core::activity::refs_at`] instead
/// of reading the field — and one operation stores one snapshot.
///
/// **Last, not first, and the read cap is the reason.** [`read_all`] returns
/// the newest [`JOURNAL_READ_CAP`] lines, so a window can begin in the middle
/// of a batch. With the anchor written first, that window holds referrers
/// whose anchor was trimmed away; with it written last, every referrer the
/// window keeps is followed by its anchor in the same window. The failure is
/// survivable either way — [`git_vista_core::activity::refs_at`] answers
/// `None`, i.e. *no information* — but survivable is not the same as
/// unnecessary.
///
/// A **failed** capture is copied onto every line of the batch instead of
/// anchored. It is a reason string rather than three maps, so sharing it saves
/// nothing, and a batch whose anchor is a failure would have referrers
/// pointing at a line that carries no maps to resolve to.
///
/// Events arriving with their own `refs` keep them, exactly as [`append`]
/// always promised, and take no part in the batch.
///
/// Best-effort throughout, and now in one place: the file is opened once and
/// written once per batch rather than once per event.
pub fn append_all(repo: &Path, events: &[ActivityEvent]) {
    let Some(path) = journal_path(repo) else {
        return;
    };
    if events.is_empty() {
        return;
    }

    // The events that arrived without a capture of their own — the only ones
    // this function fills in, and the only ones the batch is sized by.
    let needing: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.refs.is_none())
        .map(|(i, _)| i)
        .collect();
    // ONE ref read for the whole batch, and none at all when nothing needs it.
    let capture = (!needing.is_empty()).then(|| capture_refs(repo));
    let shareable = matches!(capture, Some(RefsAtEvent::Captured { .. }));
    let anchor = shareable.then(|| needing.last().copied()).flatten();
    let batch = (shareable && needing.len() > 1).then(mint_batch_id);

    let mut lines = String::new();
    for (i, event) in events.iter().enumerate() {
        let filled;
        let event = match (&capture, event.refs.is_some()) {
            // Carrying its own capture: untouched, and outside the batch.
            (_, true) | (None, _) => event,
            (Some(capture), false) => {
                let refs = match (Some(i) == anchor, &batch) {
                    // The one line that stores the maps. `batch` is None for a
                    // batch of one, which is the pre-#485 single-event line.
                    (true, batch) => match capture.clone() {
                        RefsAtEvent::Captured {
                            branches,
                            truncated_at,
                            head,
                            tags,
                            remotes,
                            ..
                        } => RefsAtEvent::Captured {
                            branches,
                            truncated_at,
                            head,
                            tags,
                            remotes,
                            batch: batch.clone(),
                        },
                        failed => failed,
                    },
                    // A shared capture exists and is elsewhere in this batch.
                    (false, Some(batch)) => RefsAtEvent::InBatch {
                        batch: batch.clone(),
                    },
                    // Not the anchor and no batch id: the capture failed, so
                    // every line carries the reason itself.
                    (false, None) => capture.clone(),
                };
                filled = ActivityEvent {
                    refs: Some(refs),
                    ..event.clone()
                };
                &filled
            }
        };
        // Every line this binary writes says which format wrote it (#521,
        // ADR 0085). Costs `"v":1,` — six bytes against a batched line's
        // measured 225 — and is ignored outright by every reader that does
        // not know the field.
        let Ok(line) = serde_json::to_string(&WrittenLine {
            v: JOURNAL_FORMAT_VERSION,
            event,
        }) else {
            continue;
        };
        lines.push_str(&line);
        lines.push('\n');
    }
    if lines.is_empty() {
        return;
    }

    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
        })
        .and_then(|mut f| f.write_all(lines.as_bytes()));
    if let Err(e) = result {
        eprintln!(
            "git-vista: couldn't append to the journal at {}: {e}",
            path.display()
        );
    }
}

/// How much of the journal is pulled back per backward step. One step
/// already covers the whole window in any realistic journal; the loop exists
/// for the pathological case of very long lines.
const TAIL_CHUNK: usize = 64 * 1024;

/// The tail of `source` guaranteed to contain the last `cap` lines — read
/// backwards from the end and stopping the moment it has seen `cap + 1`
/// newlines (#464).
///
/// **The overshoot is the whole design.** Stopping one newline late means the
/// returned text holds at least `cap + 1` lines, so the caller's existing
/// `len().saturating_sub(cap)` window always discards the first of them. That
/// single invariant pays for three things at once: the leading line is
/// allowed to be a fragment (it is dropped), a multi-byte character split by
/// the chunk boundary can only land in that fragment, and a file with no
/// trailing newline needs no special case.
///
/// The `+ 1` is defensive rather than load-bearing at the current
/// [`TAIL_CHUNK`]: a 64 KiB step already spans far more than one line of any
/// realistic journal, so the scan overshoots the cap by many lines anyway.
/// It is what makes the invariant hold for a journal of very long lines, and
/// it costs one comparison.
///
/// Decoding is lossy on purpose: bytes older than the window are already
/// outside the answer, and refusing the whole file over one of them is what
/// `read_to_string` used to do. It is lossy by *fallback* rather than by
/// default (#468) — `String::from_utf8` takes the window's allocation over
/// unchanged when it is valid, which is every normal journal, and hands the
/// bytes back untouched when it is not. `from_utf8_lossy(&window)` instead
/// borrows and then copies the whole window a second time on the normal path,
/// which at ADR 0070's worst case is ~86 MB of copying to produce bytes that
/// were already owned, contiguous and valid.
///
/// The chunks are kept apart until the scan ends and joined once, into a
/// buffer sized from the total (#467). Growing the window in place instead —
/// prepending each new chunk to what is already held — recopies the whole
/// accumulated window on every backward step, which is quadratic in the size
/// of the window rather than linear. That is invisible in both the answer and
/// the bytes read, and it lands on the two hot paths #464 exists to protect:
/// at the ~90 KB-a-line worst case of ADR 0070 the window reaches ~86 MB, and
/// the difference measured 41 s against 0.5 s.
fn tail_window<R: Read + Seek>(source: &mut R, cap: usize) -> std::io::Result<String> {
    let mut pos = source.seek(SeekFrom::End(0))?;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newlines = 0usize;
    let mut total = 0usize;
    while pos > 0 && newlines <= cap {
        let step = TAIL_CHUNK.min(pos as usize);
        pos -= step as u64;
        source.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; step];
        source.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        total += step;
        chunks.push(chunk);
    }
    // The scan collected the chunks newest-first; the window is file order,
    // so they go back together in reverse (#467).
    let mut window = Vec::with_capacity(total);
    for chunk in chunks.iter().rev() {
        window.extend_from_slice(chunk);
    }
    Ok(match String::from_utf8(window) {
        Ok(text) => text,
        // The rare path, and the only one that pays a copy: `from_utf8` hands
        // the bytes back untouched so the lossy decode can still have them.
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Read the newest [`JOURNAL_READ_CAP`] journaled events (file order — oldest
/// first — is preserved within the returned slice). Unparsable lines are
/// skipped loudly: one corrupt line must not hide the rest of the history.
///
/// The cap bounds the *read*, not just the parse (#464): the journal is
/// append-only and unbounded, and both production callers are on hot paths —
/// the activity feed and `/api/undoables`, which the graph menu hits on every
/// open. [`tail_window`] seeks from the end rather than loading the file, so
/// the cost of a feed request stops growing with the age of the repository.
///
/// One deliberate behaviour change comes with it: bytes older than the window
/// can no longer blank the feed. `read_to_string` refused the entire file over
/// a single invalid byte anywhere in it, which contradicted this function's
/// own rule about corrupt lines.
///
/// **Mixed-format files are the normal case, not an edge** (#521, ADR 0085).
/// One journal can hold pre-#131 lines with no capture at all, #485 batch
/// anchors and referrers, and #521-stamped lines, because the file is
/// append-only and every binary that ever ran against this repository appended
/// to it — including, after a rollback, an older one. Every kind is read here;
/// [`parse_window`] is where that is decided, and where what could *not* be
/// read is counted so this function can say it rather than leave it to a guess.
pub fn read_all(repo: &Path) -> Vec<ActivityEvent> {
    let Some(path) = journal_path(repo) else {
        return Vec::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(text) = tail_window(&mut file, JOURNAL_READ_CAP) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(JOURNAL_READ_CAP);
    let window = parse_window(&lines[start..]);
    for notice in window.report.notices() {
        eprintln!("{notice}");
    }
    window.events
}

/// What one window parse produced: the events, and what the reader could not
/// read (#521, ADR 0085).
struct Window {
    events: Vec<ActivityEvent>,
    report: WindowReport,
}

/// The reader's account of a window it could not fully read.
///
/// Kept as a value, and produced by a pure function, so the notices below are
/// asserted directly by tests rather than through captured stderr — ADR 0082's
/// lesson, that a mechanism which "should have run" is worth nothing until
/// something exercises it.
#[derive(Debug, Default, PartialEq, Eq)]
struct WindowReport {
    /// One serde message per line that would not parse at all. Still reported
    /// per line, not just counted: that is the pre-existing loud skip, and one
    /// corrupt line must not hide the rest of the history.
    unreadable: Vec<String>,
    /// Lines stamped with a format version newer than [`JOURNAL_FORMAT_VERSION`].
    from_newer: usize,
    /// The highest such version seen — what to name in the notice.
    newest_version: Option<u32>,
    /// Events whose capture came back [`RefsAtEvent::Unknown`]: a capture is
    /// recorded and this binary has no reading for its `status`.
    unreadable_captures: usize,
}

impl WindowReport {
    /// What this read has to say out loud, in the order it should be said.
    fn notices(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .unreadable
            .iter()
            .map(|e| format!("git-vista: skipping an unreadable journal line: {e}"))
            .collect();
        if let (true, Some(newest)) = (self.from_newer > 0, self.newest_version) {
            out.push(format!(
                "git-vista: {} journal line(s) were written by newer journal \
                 formats; the newest was journal format v{newest}; this binary \
                 writes v{JOURNAL_FORMAT_VERSION}. Compatible newer events were \
                 retained; incompatible newer events were skipped by the \
                 unreadable-line diagnostics above. On retained events, a field \
                 or ref capture this binary has no reading for is treated as \
                 \"not recorded\", never as \"nothing was there\".",
                self.from_newer
            ));
        }
        if self.unreadable_captures > 0 {
            out.push(format!(
                "git-vista: {} journal line(s) carry a ref capture in a shape this \
                 binary cannot read; those events are shown with no capture at all, \
                 which is not a claim that the repository had no refs.",
                self.unreadable_captures
            ));
        }
        out
    }
}

/// Parse an already-capped window of journal lines, keeping account of what
/// could not be read.
///
/// **A line stamped newer than this binary writes is attempted, not refused by
/// version alone.** The format is additive by construction — every field added
/// since #131 is optional, and [`RefsAtEvent::Unknown`] makes the one enum
/// tolerant — so a compatible newer line remains readable. An incompatible
/// event shape is still skipped by the ordinary full-decode error path, while
/// the independent envelope probe preserves its writer-version explanation.
fn parse_window(lines: &[&str]) -> Window {
    let mut events = Vec::new();
    let mut report = WindowReport::default();
    for line in lines.iter().filter(|l| !l.trim().is_empty()) {
        if let Ok(VersionProbe { v: Some(v) }) = serde_json::from_str(line) {
            if v > JOURNAL_FORMAT_VERSION {
                report.from_newer += 1;
                report.newest_version = report.newest_version.max(Some(v));
            }
        }
        match serde_json::from_str::<ReadLine>(line) {
            Ok(ReadLine { v, event }) => {
                // `v` remains part of the full envelope shape so stamped and
                // unstamped success paths are both exercised here. Reporting
                // uses the independent probe above and must not move back
                // behind this successful decode.
                let _ = v;
                if matches!(event.refs, Some(RefsAtEvent::Unknown)) {
                    report.unreadable_captures += 1;
                }
                events.push(event);
            }
            Err(e) => report.unreadable.push(e.to_string()),
        }
    }
    Window { events, report }
}

/// The branch → tip map as of the last snapshot, or `None` when no snapshot
/// exists yet (first run: nothing to diff against, only a baseline to write).
pub fn read_snapshot(repo: &Path) -> Option<HashMap<String, String>> {
    let path = snapshot_path(repo)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Overwrite the snapshot with the repo's current branch → tip map.
pub fn write_snapshot(repo: &Path, branches: &HashMap<String, String>) {
    let Some(path) = snapshot_path(repo) else {
        return;
    };
    let Ok(json) = serde_json::to_string_pretty(branches) else {
        return;
    };
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(&path, json));
    if let Err(e) = result {
        eprintln!(
            "git-vista: couldn't write the ref snapshot at {}: {e}",
            path.display()
        );
    }
}

/// Drop one branch from the snapshot immediately. Called by the app's own
/// delete endpoints (which journal the deletion themselves), so the feed's
/// snapshot diff can't also synthesize a duplicate "deleted outside the app"
/// event for a deletion the app performed.
pub fn remove_from_snapshot(repo: &Path, branch: &str) {
    if let Some(mut snapshot) = read_snapshot(repo) {
        if snapshot.remove(branch).is_some() {
            write_snapshot(repo, &snapshot);
        }
    }
}

/// Wipe the journal and the branch snapshot. Used by the test-repo reset: its
/// whole point is that the recorded history no longer describes the repo, and
/// keeping it would resurface undone events (with dead undo targets) in the
/// feed. Both files regenerate naturally — the journal on the next app write,
/// the snapshot on the next feed read. Best-effort, like the other writers.
pub fn clear(repo: &Path) {
    for path in [journal_path(repo), snapshot_path(repo)]
        .into_iter()
        .flatten()
    {
        if path.exists() {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("git-vista: couldn't clear {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_core::activity::HeadAtEvent;
    use git_vista_core::activity::{ActivityKind, ActivitySource};
    use std::process::Command;

    /// #519: two processes journalling their first batch in the same clock
    /// tick minted identical ids (`<nanos>-0`), and
    /// [`git_vista_core::activity::refs_at`] resolves a referrer to the
    /// FIRST matching anchor — potentially the other process's ref map. The
    /// pid segment is what keeps concurrently-live processes distinct, so
    /// its presence is pinned here. This is the one deliberate exception to
    /// "the id is opaque": the test compares against `std::process::id()`
    /// itself (ground truth), not against a re-run of the minting code.
    ///
    /// MUTATION: drop the pid from the format string — this goes red.
    #[test]
    fn a_batch_id_embeds_this_process_id_so_concurrent_processes_cannot_collide() {
        let id = mint_batch_id();
        assert!(
            id.contains(&format!("-{:x}-", std::process::id())),
            "batch id {id:?} must carry the pid between the clock and the counter"
        );
    }

    /// A tempdir with a real `.git` directory (git init), since the state dir
    /// deliberately requires one.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git runs")
            .success());
        dir
    }

    fn event(summary: &str) -> ActivityEvent {
        ActivityEvent {
            time: 42,
            kind: ActivityKind::Commit,
            ref_name: Some("main".into()),
            summary: summary.into(),
            old_oid: Some("a".into()),
            new_oid: Some("b".into()),
            source: ActivitySource::App,
            undo: None,
            refs: None,
        }
    }

    /// Commit once so the repo actually has a branch to capture.
    fn commit(dir: &Path, branch: &str) {
        for args in [
            vec!["checkout", "-q", "-B", branch],
            vec!["commit", "-q", "--allow-empty", "-m", "x"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .current_dir(dir)
                .status()
                .expect("git runs")
                .success());
        }
    }

    /// #131's core promise: an appended event carries the branch -> tip map,
    /// so a replayer can reconstruct the moment without the reflog.
    ///
    /// MUTATION: drop the `refs: Some(capture_refs(repo))` fill-in from
    /// `append` and this goes red — the whole feature reduces to a no-op.
    #[test]
    fn an_appended_event_carries_the_branch_tips_of_its_moment() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "feature");
        append(dir.path(), &event("recorded"));

        let read = read_all(dir.path());
        let RefsAtEvent::Captured {
            branches,
            truncated_at,
            ..
        } = read[0].refs.clone().expect("a capture is attached")
        else {
            panic!("a readable repo must capture, not fail");
        };
        assert_eq!(truncated_at, None, "two branches is under any cap");
        assert!(branches.contains_key("main"));
        assert!(branches.contains_key("feature"));
        assert_eq!(branches.len(), 2);
    }

    /// The lossless part: a branch deleted AFTER the event still appears in
    /// that event's capture. This is what the reflog cannot give us — git
    /// deletes a branch's reflog together with the branch.
    ///
    /// MUTATION: have the replay read live refs instead of the stored map and
    /// this goes red.
    #[test]
    fn a_deleted_branch_survives_in_the_event_that_predates_its_deletion() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "doomed");
        append(dir.path(), &event("before the deletion"));
        assert!(Command::new("git")
            .args(["checkout", "-q", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["branch", "-qD", "doomed"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { branches, .. } =
            read[0].refs.clone().expect("capture attached")
        else {
            panic!("expected a capture");
        };
        assert!(
            branches.contains_key("doomed"),
            "the journal must still know the branch existed, and at which tip"
        );
    }

    /// The third state, and the reason this field is an enum. A repo whose
    /// refs cannot be read must record CaptureFailed — never an empty map,
    /// which a replayer would read as "every branch was deleted here".
    ///
    /// MUTATION: change `capture_refs`'s Err arm to
    /// `RefsAtEvent::Captured { branches: BTreeMap::new(), truncated_at: None }`
    /// and this goes red.
    #[test]
    fn an_unreadable_repo_records_capture_failed_never_an_empty_map() {
        let dir = repo();
        // Destroy the ref store, keeping .git a directory so journaling still
        // engages — the failure must reach the record, not skip it.
        std::fs::remove_dir_all(dir.path().join(".git/refs")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();

        let captured = capture_refs(dir.path());
        match captured {
            RefsAtEvent::CaptureFailed { reason } => {
                assert!(!reason.is_empty(), "the failure must say what happened");
            }
            RefsAtEvent::Captured { branches, .. } => panic!(
                "a failed read must not masquerade as an observation of {} branches",
                branches.len()
            ),
            RefsAtEvent::InBatch { batch } => panic!(
                "`capture_refs` reads refs; only `append_all` hands out batch \
                 references, and it got {batch}"
            ),
            RefsAtEvent::Unknown => panic!(
                "`capture_refs` builds this value in this process; `Unknown` is \
                 what a *reader* produces for a capture written by some other \
                 binary (#521), and cannot arrive from a fresh read"
            ),
        }
    }

    /// An empty capture is a real answer — a repo before its first commit
    /// genuinely has no branches — and must stay distinct from a failure.
    ///
    /// MUTATION: make `capture_refs` return CaptureFailed for an empty
    /// branch set and this goes red.
    #[test]
    fn a_repo_with_no_branches_captures_an_empty_map_not_a_failure() {
        let dir = repo(); // git init, no commits: readable, zero branches
        match capture_refs(dir.path()) {
            RefsAtEvent::Captured {
                branches,
                truncated_at,
                ..
            } => {
                assert!(branches.is_empty());
                assert_eq!(truncated_at, None);
            }
            RefsAtEvent::CaptureFailed { reason } => {
                panic!("a readable empty repo is an observation, not a failure: {reason}")
            }
            RefsAtEvent::InBatch { batch } => panic!(
                "`capture_refs` reads refs; only `append_all` hands out batch \
                 references, and it got {batch}"
            ),
            RefsAtEvent::Unknown => panic!(
                "`capture_refs` builds this value in this process; `Unknown` is \
                 what a *reader* produces for a capture written by some other \
                 binary (#521), and cannot arrive from a fresh read"
            ),
        }
    }

    /// Run a git command in `dir`, with a fixed identity, asserting success.
    fn git_ok(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .current_dir(dir)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?} failed"
        );
    }

    /// Ask **git** what a revision resolves to.
    ///
    /// Every assertion about a captured oid compares against this, never
    /// against a second call into the capture code: a capture that agrees with
    /// itself proves only that it is consistent, which is the "assert a
    /// mapping by calling the function that defines it" trap.
    fn git_says(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// One capture, unwrapped into its parts — or a panic naming the failure.
    /// Named rather than a tuple so a test that reads `tags` cannot quietly be
    /// reading `remotes`.
    struct Capture {
        branches: BTreeMap<String, String>,
        truncated_at: Option<usize>,
        head: HeadAtEvent,
        tags: CapturedRefs,
        remotes: CapturedRefs,
    }

    fn captured(repo: &Path) -> Capture {
        match capture_refs(repo) {
            RefsAtEvent::Captured {
                branches,
                truncated_at,
                head,
                tags,
                remotes,
                // A single capture anchors no batch; `a_lone_append_...`
                // is where that is asserted rather than assumed.
                batch: _,
            } => Capture {
                branches,
                truncated_at,
                head: head.expect("#449: a fresh capture always records HEAD"),
                tags: tags.expect("#449: a fresh capture always records tags"),
                remotes: remotes.expect("#449: a fresh capture always records remotes"),
            },
            RefsAtEvent::CaptureFailed { reason } => panic!("expected a capture: {reason}"),
            RefsAtEvent::InBatch { batch } => panic!(
                "`capture_refs` reads refs; only `append_all` hands out batch \
                 references, and it got {batch}"
            ),
            RefsAtEvent::Unknown => panic!(
                "`capture_refs` builds this value in this process; `Unknown` is \
                 what a *reader* produces for a capture written by some other \
                 binary (#521), and cannot arrive from a fresh read"
            ),
        }
    }

    /// #449's headline: the snapshot exists so a scrubber can replay the HEAD
    /// moving, and until now it recorded local branches only — so it could not
    /// say which branch HEAD was on at any event in its own history.
    ///
    /// MUTATION-a: drop the `head` fill-in from `capture_refs` (`head: None`)
    /// and this goes red — the replay loses the one fact it is for.
    /// MUTATION-b: record the *short* branch name instead of the full ref name
    /// and this goes red on the exact-string assertion. That is why the
    /// assertion compares the whole string rather than `contains("feature")`.
    #[test]
    fn a_capture_records_which_branch_head_was_on_and_where_that_branch_was() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "feature");
        let tip = git_says(dir.path(), &["rev-parse", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(
            c.head,
            HeadAtEvent::OnBranch {
                symbolic: "refs/heads/feature".to_string(),
                oid: tip.clone(),
            },
            "the full ref name, so a replay can tell a branch from any other ref"
        );
        assert_eq!(c.branches.get("feature"), Some(&tip));
        assert!(
            c.branches.contains_key("main"),
            "sibling branches still captured"
        );
    }

    /// A detached HEAD is a different state from being on a branch, and the
    /// record has to keep them apart: a replay that reads a detached HEAD as
    /// "on the branch that happens to share the commit" draws a checkout that
    /// never happened.
    ///
    /// MUTATION-a: map a `None` symbolic name to `OnBranch { symbolic: "HEAD" }`
    /// and this goes red.
    /// MUTATION-b: fall through to `Unresolvable` when the oid is present and
    /// this goes red, differently — the commit is known, and dropping it throws
    /// away a fact the repo gave us.
    #[test]
    fn a_detached_head_is_recorded_as_detached_not_as_the_branch_it_sits_on() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["checkout", "-q", "--detach"]);
        let at = git_says(dir.path(), &["rev-parse", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(c.head, HeadAtEvent::Detached { oid: at.clone() });
        assert_eq!(
            c.branches.get("main"),
            Some(&at),
            "detached at main's commit — the same commit, recorded as a different state"
        );
    }

    /// A repo before its first commit has a HEAD that names a branch which does
    /// not exist yet. That is an observation, not a failure and not a detached
    /// HEAD: the branch name is real and worth recording, the commit genuinely
    /// is not there.
    ///
    /// MUTATION-a: treat an unresolved HEAD as `CaptureFailed` and this goes
    /// red — a fresh repo is readable, and saying otherwise also loses the
    /// empty observation `a_repo_with_no_branches_captures_an_empty_map_not_a_failure`
    /// pins.
    /// MUTATION-b: record it as `Unresolvable`, discarding the symbolic name,
    /// and this goes red — the name is the whole content of this state.
    #[test]
    fn an_unborn_head_records_the_branch_it_names_with_no_commit() {
        let dir = repo(); // git init, no commits
                          // Don't assume the host's `init.defaultBranch`; ask git what it chose.
        let expected = git_says(dir.path(), &["symbolic-ref", "HEAD"]);

        let c = captured(dir.path());
        assert_eq!(
            c.head,
            HeadAtEvent::Unborn { symbolic: expected },
            "the branch HEAD would create, with no commit — not a failure"
        );
        assert!(c.branches.is_empty() && c.tags.entries.is_empty() && c.remotes.entries.is_empty());
    }

    /// HEAD read fine and held an object id nothing resolves. Neither a name
    /// nor a commit — and forcing it into `Detached` would mean inventing an
    /// oid to put there.
    ///
    /// MUTATION-a: `CaptureFailed` on the both-absent case and this goes red —
    /// the branches read perfectly well and must survive.
    /// MUTATION-b: collapse it into `Detached { oid: String::new() }` and this
    /// goes red on the variant, having manufactured a commit that never was.
    #[test]
    fn a_head_pointing_at_nothing_is_unresolvable_and_the_branches_survive() {
        let dir = repo();
        commit(dir.path(), "main");
        let tip = git_says(dir.path(), &["rev-parse", "main"]);
        // A well-formed object id with no object behind it.
        std::fs::write(dir.path().join(".git/HEAD"), "0".repeat(40) + "\n").unwrap();

        let c = captured(dir.path());
        assert_eq!(c.head, HeadAtEvent::Unresolvable);
        assert_eq!(
            c.branches.get("main"),
            Some(&tip),
            "the readable half of the repo is still an observation worth keeping"
        );
    }

    /// The state the design's probe did not reach, and the reason this enum has
    /// a fifth variant: the ref store opens and lists normally while HEAD
    /// *itself* will not read. Recording that as "no HEAD" would be the same
    /// lie the record-level enum forbids — and failing the whole capture would
    /// throw away branches that read perfectly well.
    ///
    /// MUTATION-a: let the HEAD read error propagate as a `RepoError` (what
    /// `read_history_materials` does) and this goes red — `main` disappears
    /// with it.
    /// MUTATION-b: record the failure as `Unresolvable`, dropping the reason,
    /// and this goes red — "we could not read it" and "it pointed nowhere" are
    /// different answers.
    #[test]
    fn an_unreadable_head_records_the_reason_while_the_branches_still_capture() {
        let dir = repo();
        commit(dir.path(), "main");
        let tip = git_says(dir.path(), &["rev-parse", "main"]);
        // Corrupt HEAD only — `.git/refs` stays intact, so the ref store opens
        // and lists as usual and the failure is HEAD's alone.
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();

        let c = captured(dir.path());
        let HeadAtEvent::Unreadable { reason } = &c.head else {
            panic!(
                "a HEAD that will not read must say so, not go quiet: {:?}",
                c.head
            );
        };
        assert!(!reason.is_empty(), "the failure must say what happened");
        assert_eq!(c.branches.get("main"), Some(&tip));
    }

    /// Tags, the other half of #449's gap. Both spellings are captured, and an
    /// annotated tag records the *commit* it peels to — not the tag object,
    /// which is on no commit graph a replay can draw.
    ///
    /// MUTATION-a: drop `RefKind::Tag` from the partition and this goes red.
    /// MUTATION-b: record the unpeeled id and this goes red on the annotated
    /// tag alone — which is why the fixture carries both flavours and asserts
    /// the tag object's own id is *not* what was stored.
    #[test]
    fn tags_are_captured_and_an_annotated_tag_records_the_commit_it_peels_to() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["tag", "light"]);
        git_ok(dir.path(), &["tag", "-a", "annot", "-m", "annotated"]);

        let commit_oid = git_says(dir.path(), &["rev-parse", "annot^{commit}"]);
        let tag_object = git_says(dir.path(), &["rev-parse", "annot"]);
        assert_ne!(
            commit_oid, tag_object,
            "fixture check: an annotated tag must really be a separate object"
        );

        let c = captured(dir.path());
        assert_eq!(c.tags.entries.get("light"), Some(&commit_oid));
        assert_eq!(
            c.tags.entries.get("annot"),
            Some(&commit_oid),
            "an annotated tag records the commit it peels to"
        );
        assert_ne!(
            c.tags.entries.get("annot"),
            Some(&tag_object),
            "never the tag object's own id"
        );
        assert_eq!(c.tags.entries.len(), 2);
    }

    /// The distinction that makes these fields `Option` rather than bare maps,
    /// pinned from both sides: a line that predates #449 claims nothing, and a
    /// repo genuinely observed to have no tags records an empty map.
    ///
    /// Making them bare `BTreeMap`s does not compile — every construction site
    /// would have to claim an observation it does not have, so the type refuses
    /// the collapse before a test can. What a test still has to catch is the
    /// same lie told through serde, and its mirror image.
    ///
    /// MUTATION-a: give `tags` `#[serde(default = "..")]` returning
    /// `Some(CapturedRefs::default())` — "absent means there were none", the
    /// natural reading and the wrong one — and the first half goes red.
    /// MUTATION-b: emit `None` for a genuinely tagless repo (the "don't write
    /// empty objects" optimisation) and the second half goes red.
    #[test]
    fn absent_and_observed_empty_are_different_answers_about_tags() {
        // Half one: a pre-#449 line claims nothing.
        let old = repo();
        let path = old.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"time\":1,\"kind\":\"Commit\",\"ref_name\":\"main\",\"summary\":\"old\",\
             \"old_oid\":\"a\",\"new_oid\":\"b\",\"source\":\"App\",\
             \"refs\":{\"status\":\"captured\",\"branches\":{\"main\":\"aaa\"}}}\n",
        )
        .unwrap();
        let read = read_all(old.path());
        assert_eq!(
            read.len(),
            1,
            "a #131-era line must not be dropped as corrupt"
        );
        let RefsAtEvent::Captured {
            branches,
            head,
            tags,
            remotes,
            ..
        } = read[0]
            .refs
            .clone()
            .expect("the branch capture still parses")
        else {
            panic!("expected a capture");
        };
        assert_eq!(branches.get("main").map(String::as_str), Some("aaa"));
        assert_eq!(head, None, "absent HEAD means nobody recorded one");
        assert_eq!(tags, None, "absent tags is not the observation 'no tags'");
        assert_eq!(remotes, None);

        // Half two: a real repo with no tags records an observation.
        let live = repo();
        commit(live.path(), "main");
        let c = captured(live.path());
        assert_eq!(
            c.tags,
            CapturedRefs {
                entries: BTreeMap::new(),
                truncated_at: None
            },
            "observed-and-empty, never absent — absent means nobody looked"
        );
        assert_eq!(c.remotes.entries, BTreeMap::new());
    }

    /// Remote-tracking refs are recorded (ADR 0070): the story a scrubber
    /// mostly tells is divergence, and local branches alone cannot tell it. The
    /// remote's symbolic default-branch pointer is not a tip and stays out.
    ///
    /// MUTATION-a: drop `RefKind::RemoteBranch` from the partition and this
    /// goes red.
    /// MUTATION-b: remove the `/HEAD` skip in the ref classification and this
    /// goes red on the exclusion assertion instead.
    #[test]
    fn remote_tracking_refs_are_captured_and_origin_head_is_not() {
        let origin = repo();
        commit(origin.path(), "main");
        let clone = tempfile::tempdir().unwrap();
        let dest = clone.path().join("work");
        git_ok(
            clone.path(),
            &[
                "clone",
                "-q",
                origin.path().to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        // Fixture check: the pointer this test excludes must really exist.
        assert!(
            git_says(&dest, &["symbolic-ref", "refs/remotes/origin/HEAD"]).starts_with("refs/"),
            "fixture check: the clone must have created refs/remotes/origin/HEAD"
        );
        let tip = git_says(&dest, &["rev-parse", "refs/remotes/origin/main"]);

        let c = captured(&dest);
        assert_eq!(c.remotes.entries.get("origin/main"), Some(&tip));
        assert!(
            !c.remotes.entries.keys().any(|k| k.ends_with("/HEAD")),
            "the remote's symbolic default pointer is not a tip worth recording"
        );
    }

    /// Caps are per map, so one kind overflowing cannot evict another's
    /// entries, and each map reports its own overflow.
    ///
    /// MUTATION-a: share one budget across the maps (cap tags at what is left
    /// after branches) and this goes red — with 501 tags the branches are
    /// evicted.
    /// MUTATION-b: cap without setting `truncated_at` and this goes red — the
    /// silent-truncation defect the cap's own doc comment names.
    #[test]
    fn caps_are_per_map_and_each_reports_its_own_overflow() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "second");
        let tip = git_says(dir.path(), &["rev-parse", "HEAD"]);

        // 501 real tags, created in one git process — a genuine fixture.
        let over = REFS_PER_EVENT_CAP + 1;
        let mut stdin = String::new();
        for i in 0..over {
            stdin.push_str(&format!("create refs/tags/v{i:04} {tip}\n"));
        }
        let mut child = Command::new("git")
            .args(["update-ref", "--stdin"])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "git update-ref failed");

        let c = captured(dir.path());
        assert_eq!(c.tags.entries.len(), REFS_PER_EVENT_CAP);
        assert_eq!(
            c.tags.truncated_at,
            Some(over),
            "the true count travels with the capped map"
        );
        assert_eq!(
            c.branches.len(),
            2,
            "one map's overflow must not evict another's entries"
        );
        assert_eq!(c.truncated_at, None, "branches did not overflow");
    }

    /// The lossless promise extends to the new kinds. Mirrors
    /// `a_deleted_branch_survives_in_the_event_that_predates_its_deletion`: git
    /// deletes a tag outright, so the journal is the only place its tip
    /// survives.
    ///
    /// MUTATION-a: have the read consult live refs instead of the stored map
    /// and this goes red.
    /// MUTATION-b: capture at read time rather than at append time and this
    /// goes red — the deletion would already have happened.
    #[test]
    fn a_deleted_tag_survives_in_the_event_that_predates_its_deletion() {
        let dir = repo();
        commit(dir.path(), "main");
        git_ok(dir.path(), &["tag", "doomed"]);
        let tip = git_says(dir.path(), &["rev-parse", "doomed^{commit}"]);
        append(dir.path(), &event("before the deletion"));
        git_ok(dir.path(), &["tag", "-d", "doomed"]);
        assert!(
            !git_says(dir.path(), &["tag", "--list"]).contains("doomed"),
            "fixture check: the tag must really be gone from the repo"
        );

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { tags, .. } = read[0].refs.clone().expect("capture attached")
        else {
            panic!("expected a capture");
        };
        assert_eq!(
            tags.expect("tags recorded").entries.get("doomed"),
            Some(&tip),
            "the journal must still know the tag existed, and at which commit"
        );
    }

    /// An event that already carries a capture keeps it. The feed's
    /// synthesized external-deletion event depends on this: it must record the
    /// map from BEFORE the deletion it just noticed, not the live present that
    /// has already lost that branch.
    ///
    /// MUTATION: make `append` overwrite `refs` unconditionally and this goes
    /// red — and the external-deletion event silently stops recording the very
    /// branch it exists to remember.
    #[test]
    fn a_caller_supplied_capture_is_never_overwritten() {
        let dir = repo();
        commit(dir.path(), "main");
        let mut e = event("synthesized");
        e.refs = Some(RefsAtEvent::Captured {
            branches: BTreeMap::from([("long-gone".to_string(), "deadbeef".to_string())]),
            truncated_at: None,
            head: None,
            tags: None,
            remotes: None,
            batch: None,
        });
        append(dir.path(), &e);

        let read = read_all(dir.path());
        let RefsAtEvent::Captured { branches, .. } = read[0].refs.clone().unwrap() else {
            panic!("expected the caller's capture");
        };
        assert!(
            branches.contains_key("long-gone"),
            "append must not replace a capture the caller deliberately supplied"
        );
        assert!(
            !branches.contains_key("main"),
            "and must not merge live refs in"
        );
    }

    /// A journal line written before #131 has no `refs` field at all. It must
    /// still parse, and must read as None — "no capture recorded" — rather
    /// than any claim about branches.
    ///
    /// MUTATION: drop `#[serde(default)]` from the field and this goes red.
    #[test]
    fn a_pre_131_journal_line_still_parses_and_claims_nothing() {
        let dir = repo();
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"time\":1,\"kind\":\"Commit\",\"ref_name\":\"main\",\"summary\":\"old\",\"old_oid\":\"a\",\"new_oid\":\"b\",\"source\":\"App\"}\n",
        )
        .unwrap();

        let read = read_all(dir.path());
        assert_eq!(read.len(), 1, "an old line must not be dropped as corrupt");
        assert_eq!(read[0].summary, "old");
        assert!(
            read[0].refs.is_none(),
            "absent means no capture recorded — never an empty observation"
        );
    }

    #[test]
    fn journal_round_trips_events_in_order() {
        let dir = repo();
        append(dir.path(), &event("first"));
        append(dir.path(), &event("second"));
        let read = read_all(dir.path());
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].summary, "first");
        assert_eq!(read[1].summary, "second");
        assert_eq!(read[0].source, ActivitySource::App);
        // The undo field is never journaled (recomputed per read).
        assert!(read[0].undo.is_none());
    }

    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let dir = repo();
        append(dir.path(), &event("good"));
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json}\n");
        std::fs::write(&path, text).unwrap();
        append(dir.path(), &event("after"));
        let read = read_all(dir.path());
        assert_eq!(read.len(), 2, "good lines on both sides of the corruption");
    }

    /// A journal file holding `count` events, summaries `{prefix}0` upward,
    /// written directly rather than through `append` — 1,000+ real captures
    /// would spend the whole test in git.
    fn seed_journal(dir: &Path, prefix: &str, count: usize, trailing_newline: bool) -> PathBuf {
        let path = dir.join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for i in 0..count {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&serde_json::to_string(&event(&format!("{prefix}{i}"))).unwrap());
        }
        if trailing_newline {
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A `Read + Seek` source that tallies every byte handed out, so a test
    /// can assert on I/O volume rather than on the answer alone.
    struct CountingCursor {
        inner: std::io::Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl CountingCursor {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: std::io::Cursor::new(bytes),
                bytes_read: 0,
            }
        }
    }

    impl Read for CountingCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl Seek for CountingCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// #464, half one: the window is the NEWEST `JOURNAL_READ_CAP` events and
    /// its boundary is exact — including the case a naive backward newline
    /// count gets wrong, a file of exactly the cap with no trailing newline.
    ///
    /// This is the off-by-one net. It says nothing about how much was read;
    /// `the_tail_window_reads_only_the_tail_of_the_journal` owns that.
    ///
    /// MUTATION: `.max(1)` on the window start — "always drop the first line,
    /// it might be a fragment", the natural wrong fix — and both halves go
    /// red. The fragment must be discarded by the cap arithmetic, never by a
    /// rule of its own, or a journal at or under the cap loses its oldest
    /// event.
    #[test]
    fn the_read_window_is_the_newest_events_and_its_boundary_is_exact() {
        let dir = repo();
        seed_journal(dir.path(), "e", JOURNAL_READ_CAP + 50, true);
        let read = read_all(dir.path());
        assert_eq!(read.len(), JOURNAL_READ_CAP, "the cap bounds the answer");
        assert_eq!(
            read[0].summary, "e50",
            "the oldest kept event is the 51st, not the 52nd"
        );
        assert_eq!(
            read[JOURNAL_READ_CAP - 1].summary,
            format!("e{}", JOURNAL_READ_CAP + 49),
            "the newest event is the last line of the file"
        );

        // Exactly the cap, and no trailing newline: the shape that costs a
        // naive implementation its oldest line.
        let dir = repo();
        seed_journal(dir.path(), "x", JOURNAL_READ_CAP, false);
        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            JOURNAL_READ_CAP,
            "a missing trailing newline must not cost an event"
        );
        assert_eq!(read[0].summary, "x0");
    }

    /// #464's actual defect: the cap bounded *parsing*, not I/O — the whole
    /// journal was read into memory first, so disk cost grew without limit.
    ///
    /// The only test here that can tell the two implementations apart: a
    /// whole-file read returns exactly the same events, so it can only be
    /// caught by counting bytes.
    ///
    /// MUTATION: restore the old `read_to_string` body and this goes red on
    /// the byte count while every other journal test stays green.
    #[test]
    fn the_tail_window_reads_only_the_tail_of_the_journal() {
        // A pre-window prefix that dwarfs the window: 200 padded events the
        // cap must push out of view.
        let pad = "p".repeat(8 * 1024);
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..200 {
            writeln!(bytes, "{{\"old\":{i},\"pad\":\"{pad}\"}}").unwrap();
        }
        let prefix_len = bytes.len();
        for i in 0..(JOURNAL_READ_CAP + 5) {
            writeln!(bytes, "{{\"new\":{i}}}").unwrap();
        }
        let total = bytes.len();

        let mut source = CountingCursor::new(bytes);
        let window = tail_window(&mut source, JOURNAL_READ_CAP).unwrap();

        assert!(
            source.bytes_read < prefix_len,
            "read {} bytes of a {total}-byte journal whose pre-window prefix \
             alone is {prefix_len} — the read is not bounded by the cap",
            source.bytes_read
        );
        assert!(
            source.bytes_read <= TAIL_CHUNK.saturating_mul(4),
            "the window is ~14 KiB; reading {} bytes for it means the backward \
             scan is not stopping where it should",
            source.bytes_read
        );

        // The tail may overshoot into the prefix by up to a chunk — that is
        // the design. What must hold is that the capped window inside it is
        // entirely post-prefix, and that it ends at the end of the file.
        let lines: Vec<&str> = window.lines().collect();
        assert!(
            lines.len() > JOURNAL_READ_CAP,
            "the window must overshoot the cap by at least one line so the \
             leading partial line is always trimmed away"
        );
        let capped = &lines[lines.len() - JOURNAL_READ_CAP..];
        assert!(
            capped.iter().all(|l| l.starts_with("{\"new\":")),
            "the capped window must hold only events newer than the prefix"
        );
        assert_eq!(
            *capped.last().unwrap(),
            format!("{{\"new\":{}}}", JOURNAL_READ_CAP + 4),
            "the tail must end at the end of the file"
        );
    }

    /// A consequence of the tail read, and an intended one: corruption older
    /// than the window can no longer blank the feed. `read_to_string` refused
    /// the whole file over one invalid byte anywhere in it, which contradicted
    /// this module's own rule that one bad line must not hide the history.
    ///
    /// MUTATION: decode the window with `String::from_utf8` (strict) instead
    /// of `from_utf8_lossy` and this goes red whenever the chunk boundary
    /// lands inside the corrupt prefix.
    #[test]
    fn corruption_older_than_the_window_no_longer_blanks_the_feed() {
        let dir = repo();
        let path = seed_journal(dir.path(), "c", JOURNAL_READ_CAP + 2, true);
        let good = std::fs::read(&path).unwrap();
        let mut bytes: Vec<u8> = vec![0xff, 0xfe, 0xff, b'\n'];
        bytes.extend_from_slice(&good);
        std::fs::write(&path, bytes).unwrap();

        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            JOURNAL_READ_CAP,
            "invalid bytes older than the window must not cost the feed"
        );
        assert_eq!(read[0].summary, "c2");
    }

    /// The lossy decode's `Err` branch, which nothing reached before (#468).
    ///
    /// `corruption_older_than_the_window_no_longer_blanks_the_feed` puts its
    /// invalid bytes BEFORE the window, so the window itself is clean and a
    /// strict decode of it would succeed — that test passes whether the decode
    /// is lossy or not. The bytes have to land INSIDE the window for the
    /// fallback to be exercised at all, which is what this does.
    ///
    /// Why the fallback must exist: `tail_window` starts at an arbitrary byte
    /// offset, so a multi-byte character can be cut in half at the window's
    /// leading edge. Refusing the whole window over that would blank the feed —
    /// the exact behaviour #466 removed.
    ///
    /// MUTATION: decode with `String::from_utf8(window).expect(...)` — red,
    /// the read panics instead of skipping one line.
    #[test]
    fn invalid_bytes_inside_the_window_cost_their_line_and_nothing_else() {
        let dir = repo();
        let path = seed_journal(dir.path(), "w", 12, true);
        let mut bytes = std::fs::read(&path).unwrap();

        // Corrupt one byte of the 6th line's payload — inside the window, so
        // the decode itself has to cope, not just the JSON parse.
        let sixth = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .nth(4)
            .map(|(i, _)| i + 1)
            .expect("the fixture has at least six lines");
        bytes[sixth + 10] = 0xff;
        std::fs::write(&path, bytes).unwrap();

        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            11,
            "one corrupt line must cost exactly itself — the other 11 events \
             must survive the decode"
        );
        assert!(
            !read.iter().any(|e| e.summary == "w5"),
            "the corrupted event is the one that should be missing"
        );
    }

    /// #468: the window is decoded by taking ownership of the bytes, not by
    /// borrowing them and copying.
    ///
    /// `String::from_utf8_lossy(&window)` hands back a `Cow::Borrowed` whenever
    /// the window is valid UTF-8 — which is every normal journal — and
    /// `into_owned()` then allocates a second buffer and copies the whole thing
    /// into it. The bytes were already owned, contiguous and valid; the copy is
    /// paid only because they were passed as a slice.
    ///
    /// The budget is measured, not guessed: 3.00x the window before, 2.00x
    /// after (the chunks, the joined buffer, and — before the fix — the decode).
    /// The bar sits in the middle of that gap.
    ///
    /// This is deliberately a separate test from
    /// `the_tail_window_joins_its_chunks_in_one_pass`, whose 10x bar is the net
    /// for the quadratic join (#467). One assertion, one reason to fail: a
    /// 3.00x regression and a 34x one are different defects and should not
    /// report as the same failure.
    ///
    /// MUTATION: restore `String::from_utf8_lossy(&window).into_owned()` — red
    /// here at 3.0x, while the #467 test stays green at its 10x bar.
    #[test]
    fn the_window_is_decoded_by_taking_ownership_not_by_copying_it_again() {
        let pad = "p".repeat(4 * 1024);
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..(JOURNAL_READ_CAP + 20) {
            writeln!(bytes, "{{\"n\":{i},\"pad\":\"{pad}\"}}").unwrap();
        }
        let mut source = std::io::Cursor::new(bytes);

        let (window, allocated) = alloc_probe::bytes_allocated_by(|| {
            tail_window(&mut source, JOURNAL_READ_CAP).expect("the tail reads")
        });

        assert!(
            window.lines().count() > JOURNAL_READ_CAP,
            "an empty window is cheap and wrong"
        );

        // 2.5x: measured 2.00x with the fix, 3.00x without it.
        let budget = window.len().saturating_mul(5) / 2;
        assert!(
            allocated <= budget,
            "decoding a {:.1} MB window allocated {:.1} MB ({:.2}x the window, \
             budget {:.2}x) — the window is being copied again on the way out \
             instead of being handed over",
            window.len() as f64 / 1_048_576.0,
            allocated as f64 / 1_048_576.0,
            allocated as f64 / window.len() as f64,
            budget as f64 / window.len() as f64,
        );
    }

    #[test]
    fn snapshot_round_trips_and_removes() {
        let dir = repo();
        assert!(read_snapshot(dir.path()).is_none(), "no baseline yet");
        let branches = HashMap::from([
            ("main".to_string(), "aaa".to_string()),
            ("feat".to_string(), "bbb".to_string()),
        ]);
        write_snapshot(dir.path(), &branches);
        assert_eq!(read_snapshot(dir.path()).unwrap(), branches);

        remove_from_snapshot(dir.path(), "feat");
        let after = read_snapshot(dir.path()).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after.contains_key("main"));
    }

    #[test]
    fn missing_git_dir_degrades_to_no_ops() {
        let dir = tempfile::tempdir().unwrap(); // no .git at all
        append(dir.path(), &event("ignored"));
        assert!(read_all(dir.path()).is_empty());
        assert!(read_snapshot(dir.path()).is_none());
        write_snapshot(dir.path(), &HashMap::new()); // must not create anything
        assert!(!dir.path().join(".git").exists());
    }

    /// A counting allocator, test-only, used by
    /// `the_tail_window_joins_its_chunks_in_one_pass` (#467).
    ///
    /// The tally is **thread-local and opt-in**: the rest of the suite runs in
    /// parallel on other threads, and those threads never enter the `Some`
    /// arm, so their allocations can never land in a measurement. The cell is
    /// `const`-initialized and holds no `Drop` type, so touching it from
    /// inside `alloc` cannot allocate and cannot recurse.
    ///
    /// Only `alloc`/`dealloc` are implemented on purpose: `GlobalAlloc`'s
    /// default `realloc` and `alloc_zeroed` are written in terms of
    /// `self.alloc`, so a `Vec` growing or a `vec![0u8; n]` is counted through
    /// the same path rather than needing its own arm.
    mod alloc_probe {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static COUNTED: Cell<Option<usize>> = const { Cell::new(None) };
        }

        pub struct Counting;

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let _ = COUNTED.try_with(|c| {
                    if let Some(n) = c.get() {
                        c.set(Some(n + layout.size()));
                    }
                });
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        /// Runs `f`, returning its value and the bytes allocated on this
        /// thread while it ran.
        pub fn bytes_allocated_by<T>(f: impl FnOnce() -> T) -> (T, usize) {
            COUNTED.with(|c| c.set(Some(0)));
            let out = f();
            let n = COUNTED.with(|c| c.replace(None)).unwrap_or(0);
            (out, n)
        }
    }

    /// A crate may have exactly one of these, so this is the test build's
    /// only `#[global_allocator]` and a second one anywhere in
    /// `git-vista-server`'s tests is a compile error, not a runtime surprise.
    /// If another test needs allocation numbers, extend
    /// [`alloc_probe::bytes_allocated_by`] rather than adding a second.
    #[global_allocator]
    static COUNTING_ALLOCATOR: alloc_probe::Counting = alloc_probe::Counting;

    /// #467: `tail_window`'s *read* is bounded by the cap — that is #464, and
    /// it holds. What is not bounded is the *join*. Growing the window with
    /// `chunk.append(&mut window)` copies everything accumulated so far on
    /// every backward step, so the bytes moved are quadratic in the size of
    /// the window.
    ///
    /// Neither the answer nor `bytes_read` changes, which is exactly why
    /// #466's own tests could not see it. The only axis that separates a
    /// one-pass join from a quadratic one is how much gets **allocated**, so
    /// that is what this measures.
    ///
    /// The window below is ~4 MB — a size a post-#449 repository reaches
    /// today (ADR 0070: ~3.5 KB a line, ~90 KB worst case, against a
    /// 1,000-line cap). A one-pass join lands near 3.5x the window; the
    /// quadratic one near 29x. The 10x bar sits in that gap, and the gap
    /// widens with every extra chunk rather than narrowing.
    ///
    /// MUTATION 1: join by prepending each chunk to what is already held —
    ///   the quadratic shape this fixes, restored. Red on the byte budget.
    /// MUTATION 2: drop the `.rev()` and join the chunks in scan order. Red on
    ///   the last line, which is no longer the last line of the file.
    ///
    /// The two fail through different assertions on purpose: the first breaks
    /// what the join *costs*, the second breaks what it *returns*, and either
    /// alone would leave half of this test unproven. A pre-size mutation
    /// (`Vec::new()` for `Vec::with_capacity(total)`) is deliberately **not**
    /// claimed here — doubling growth is amortized linear, so it survives, and
    /// naming a mutation this test cannot catch is the failure mode the
    /// mutation rule exists to prevent.
    #[test]
    fn the_tail_window_joins_its_chunks_in_one_pass() {
        let pad = "p".repeat(4 * 1024);
        let mut bytes: Vec<u8> = Vec::new();
        for i in 0..(JOURNAL_READ_CAP + 20) {
            writeln!(bytes, "{{\"n\":{i},\"pad\":\"{pad}\"}}").unwrap();
        }
        let mut source = std::io::Cursor::new(bytes);

        let (window, allocated) = alloc_probe::bytes_allocated_by(|| {
            tail_window(&mut source, JOURNAL_READ_CAP).expect("the tail reads")
        });

        // The window must be the real thing before its cost means anything:
        // an empty answer is cheap and wrong.
        assert!(
            window.lines().count() > JOURNAL_READ_CAP,
            "the window must overshoot the cap, else there is nothing to join"
        );
        assert_eq!(
            window.lines().last().unwrap(),
            format!("{{\"n\":{},\"pad\":\"{pad}\"}}", JOURNAL_READ_CAP + 19),
            "the window must still end at the end of the file"
        );

        let budget = window.len().saturating_mul(10);
        assert!(
            allocated <= budget,
            "joining a {:.1} MB window allocated {:.1} MB ({:.1}x the window, \
             budget {:.1} MB) — the chunks are being copied on every backward \
             step instead of concatenated once",
            window.len() as f64 / 1_048_576.0,
            allocated as f64 / 1_048_576.0,
            allocated as f64 / window.len() as f64,
            budget as f64 / 1_048_576.0,
        );
    }
    // -----------------------------------------------------------------------
    // #485 — one capture per operation, not one per ref (ADR 0080)
    // -----------------------------------------------------------------------

    /// Point `n` remote-tracking refs at HEAD, in one `git update-ref --stdin`
    /// process. A fetch's ref count without a fetch's network, and cheap
    /// enough that a test can afford the counts the defect showed up at.
    fn seed_remote_refs(dir: &Path, n: usize) {
        let head = git_says(dir, &["rev-parse", "HEAD"]);
        let mut stdin = String::new();
        for i in 0..n {
            stdin.push_str(&format!("create refs/remotes/origin/b{i:04} {head}\n"));
        }
        let mut child = Command::new("git")
            .args(["update-ref", "--stdin"])
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(stdin.as_bytes())
            .expect("write");
        assert!(child.wait().expect("git runs").success());
    }

    /// The `refs` of every line in the journal, in file order.
    fn refs_of(repo: &Path) -> Vec<Option<RefsAtEvent>> {
        read_all(repo).into_iter().map(|e| e.refs).collect()
    }

    /// The raw journal lines, for the questions that are about bytes rather
    /// than about parsed values.
    fn raw_lines(repo: &Path) -> Vec<String> {
        std::fs::read_to_string(journal_path(repo).expect("a state dir"))
            .expect("the journal exists")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// **#485's mechanism, stated as a count.** One operation that journals
    /// many events reads the refs *once*: exactly one line of the batch
    /// carries the maps, and every other line carries a reference to it.
    ///
    /// The count is the assertion, deliberately. "Some line has a capture"
    /// would pass unchanged if the batching were removed entirely.
    ///
    /// MUTATION (remove the batching): give every needing event its own
    /// `capture_refs(repo)` in `append_all` — 8 anchors, red.
    /// MUTATION (weaken it): anchor every second event instead of the last
    /// one — 4 anchors, red.
    #[test]
    fn a_batch_stores_one_capture_and_the_other_lines_reference_it() {
        let dir = repo();
        commit(dir.path(), "main");
        let events: Vec<ActivityEvent> = (0..8).map(|i| event(&format!("ref {i}"))).collect();

        append_all(dir.path(), &events);

        let refs = refs_of(dir.path());
        assert_eq!(refs.len(), 8, "one line per event, batching or not");
        let anchors: Vec<&RefsAtEvent> = refs
            .iter()
            .flatten()
            .filter(|r| matches!(r, RefsAtEvent::Captured { .. }))
            .collect();
        assert_eq!(
            anchors.len(),
            1,
            "8 events must cost ONE ref read and store ONE snapshot; found \
             {} lines carrying maps of their own",
            anchors.len()
        );
        let RefsAtEvent::Captured {
            batch: Some(id),
            branches,
            ..
        } = anchors[0]
        else {
            panic!(
                "the anchor of a batch must carry the batch id: {:?}",
                anchors[0]
            );
        };
        assert!(
            branches.contains_key("main"),
            "the one stored snapshot must be a real observation of the repo"
        );
        let referrers: Vec<&String> = refs
            .iter()
            .flatten()
            .filter_map(|r| match r {
                RefsAtEvent::InBatch { batch } => Some(batch),
                _ => None,
            })
            .collect();
        assert_eq!(
            referrers.len(),
            7,
            "the other seven lines must say a capture exists and where — not \
             go silent, which reads as 'no capture attempted'"
        );
        assert!(
            referrers.iter().all(|b| *b == id),
            "every referrer must name the anchor of its own batch"
        );
    }

    /// The anchor is the **last** line of its batch, because [`read_all`]
    /// returns the newest [`JOURNAL_READ_CAP`] lines and a window may begin
    /// in the middle of a batch. Anchor-first would leave the window holding
    /// referrers whose capture was trimmed away.
    ///
    /// Asserted on the file rather than on `append_all`'s internals: the
    /// order that matters is the order on disk.
    ///
    /// MUTATION: anchor the first needing event instead of the last — red.
    #[test]
    fn the_batchs_capture_is_written_last_so_a_trimmed_window_keeps_it() {
        let dir = repo();
        commit(dir.path(), "main");
        let events: Vec<ActivityEvent> = (0..5).map(|i| event(&format!("ref {i}"))).collect();

        append_all(dir.path(), &events);

        let refs = refs_of(dir.path());
        assert!(
            matches!(refs.last(), Some(Some(RefsAtEvent::Captured { .. }))),
            "the capture must be on the batch's last line: {refs:#?}"
        );
        // And the property it buys: every window that ends at the file's end
        // — which is every window `read_all` can return — holds the anchor.
        for start in 0..refs.len() {
            let window = &refs[start..];
            assert!(
                window
                    .iter()
                    .flatten()
                    .any(|r| matches!(r, RefsAtEvent::Captured { .. })),
                "a window starting at line {start} lost the capture its \
                 referrers point at"
            );
        }
    }

    /// The other twenty-odd endpoints record one event apiece, and #485 must
    /// not have changed a byte of what they write: a lone append still stores
    /// its own maps, and anchors no batch.
    ///
    /// This is also what keeps the count in
    /// `a_batch_stores_one_capture_and_the_other_lines_reference_it` honest —
    /// an implementation that dropped captures altogether would satisfy "one
    /// anchor per batch" by having none anywhere, and would fail here.
    #[test]
    fn a_lone_append_still_stores_its_own_capture_and_names_no_batch() {
        let dir = repo();
        commit(dir.path(), "main");

        append(dir.path(), &event("on its own"));

        let refs = refs_of(dir.path());
        assert_eq!(refs.len(), 1);
        let Some(RefsAtEvent::Captured {
            branches, batch, ..
        }) = &refs[0]
        else {
            panic!("a single event still captures its own refs: {refs:#?}");
        };
        assert!(branches.contains_key("main"));
        assert_eq!(
            *batch, None,
            "a batch of one anchors nothing — and a `batch` field on this \
             line would change what every pre-#485 single-event line means"
        );
    }

    /// A batch whose ref read **failed** copies the reason onto every line
    /// rather than anchoring it. The reason is a string, so sharing it saves
    /// nothing — and a referrer pointing at a line that carries no maps would
    /// resolve to "no information" when the truth ("we tried and could not
    /// read the refs") is available and different.
    ///
    /// MUTATION: anchor the failure like a capture — the other four lines
    /// become `InBatch` and this goes red.
    #[test]
    fn a_batch_whose_ref_read_failed_records_the_failure_on_every_line() {
        let dir = repo();
        // Destroy the ref store, keeping .git a directory so journaling still
        // engages — the same fixture the single-event failure test uses.
        std::fs::remove_dir_all(dir.path().join(".git/refs")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();
        let events: Vec<ActivityEvent> = (0..4).map(|i| event(&format!("ref {i}"))).collect();

        append_all(dir.path(), &events);

        let refs = refs_of(dir.path());
        assert_eq!(refs.len(), 4);
        for (i, r) in refs.iter().enumerate() {
            match r {
                Some(RefsAtEvent::CaptureFailed { reason }) => {
                    assert!(!reason.is_empty(), "line {i} must say what happened")
                }
                other => panic!(
                    "line {i} must carry the failure itself, not a reference to \
                     a capture that does not exist: {other:?}"
                ),
            }
        }
    }

    /// An event that arrives carrying its own `refs` keeps them inside a
    /// batch, and takes no part in it — the feed's synthesized
    /// external-deletion event attaches the map as it stood *before* the
    /// deletion, and a batch must not overwrite that with the live present.
    #[test]
    fn an_event_with_its_own_capture_keeps_it_and_stays_out_of_the_batch() {
        let dir = repo();
        commit(dir.path(), "main");
        let mut own = event("brought its own");
        own.refs = Some(RefsAtEvent::Captured {
            branches: BTreeMap::from([("long-gone".to_string(), "deadbeef".to_string())]),
            truncated_at: None,
            head: None,
            tags: None,
            remotes: None,
            batch: None,
        });
        let events = vec![event("a"), own, event("b"), event("c")];

        append_all(dir.path(), &events);

        let refs = refs_of(dir.path());
        let Some(RefsAtEvent::Captured {
            branches, batch, ..
        }) = &refs[1]
        else {
            panic!("the caller's own capture must survive the batch: {refs:#?}");
        };
        assert!(branches.contains_key("long-gone"));
        assert!(!branches.contains_key("main"), "no live refs merged in");
        assert_eq!(*batch, None, "and it anchors nothing");
        // The other three still form a batch of their own: one anchor, two
        // referrers.
        assert_eq!(
            refs.iter()
                .flatten()
                .filter(|r| matches!(r, RefsAtEvent::InBatch { .. }))
                .count(),
            2
        );
    }

    /// The round trip a replayer actually makes: read the journal back and
    /// resolve each line's capture through
    /// [`git_vista_core::activity::refs_at`]. Every event of the batch must
    /// answer with the same maps, and those maps must be what **git** says —
    /// not what a second call into `capture_refs` says.
    #[test]
    fn every_line_of_a_batch_resolves_to_the_same_maps_and_git_agrees() {
        let dir = repo();
        commit(dir.path(), "main");
        commit(dir.path(), "feature");
        let events: Vec<ActivityEvent> = (0..6).map(|i| event(&format!("ref {i}"))).collect();
        append_all(dir.path(), &events);

        let read = read_all(dir.path());
        let main = git_says(dir.path(), &["rev-parse", "refs/heads/main"]);
        let feature = git_says(dir.path(), &["rev-parse", "refs/heads/feature"]);
        assert_eq!(read.len(), 6);
        for (i, e) in read.iter().enumerate() {
            let Some(RefsAtEvent::Captured { branches, .. }) =
                git_vista_core::activity::refs_at(e, &read)
            else {
                panic!("line {i} did not resolve to a capture: {:?}", e.refs);
            };
            assert_eq!(branches.get("main"), Some(&main), "line {i}");
            assert_eq!(branches.get("feature"), Some(&feature), "line {i}");
        }
    }

    /// **Acceptance #1 for #485, as an assertion.** The bytes one journal
    /// *line* costs must stop growing with the number of refs the operation
    /// moved.
    ///
    /// Both writers run against the same fixture, which is what makes the
    /// claim readable: the per-event path (`append` in a loop — what a fetch
    /// did before #485) writes N lines that each embed the whole ref set, and
    /// the batched path writes one that does and N-1 that do not.
    ///
    /// The contrast leg is not decoration. Without it, a fixture whose refs
    /// were too few to inflate a line would let the batched leg pass while
    /// proving nothing at all — so the old path is asserted to be *fat* on
    /// this very fixture before the new one is asserted to be thin.
    ///
    /// 120 refs rather than 500: enough that a captured line is an order of
    /// magnitude over a referrer line, cheap enough for the gate. The 500-ref
    /// row is in `measure_the_journalling_cost_of_one_fetch`.
    #[test]
    fn a_batched_line_stops_growing_with_the_refs_the_operation_moved() {
        const REFS: usize = 120;
        // A referrer line is the event's own fields plus a batch id: a few
        // hundred bytes, and constant. A captured line at 120 remote refs is
        // ~60 bytes per entry, so several kilobytes.
        const THIN: usize = 700;
        const FAT: usize = 4_000;

        let per_event = repo();
        commit(per_event.path(), "main");
        seed_remote_refs(per_event.path(), REFS);
        let events: Vec<ActivityEvent> = (0..REFS).map(|i| event(&format!("ref {i}"))).collect();
        for e in &events {
            append(per_event.path(), e);
        }
        let old = raw_lines(per_event.path());
        assert_eq!(old.len(), REFS);
        assert!(
            old.iter().all(|l| l.len() > FAT),
            "the fixture must actually inflate a per-event line, or the \
             batched leg below proves nothing: shortest was {} bytes",
            old.iter().map(String::len).min().unwrap_or(0)
        );

        let batched = repo();
        commit(batched.path(), "main");
        seed_remote_refs(batched.path(), REFS);
        append_all(batched.path(), &events);
        let new = raw_lines(batched.path());
        assert_eq!(new.len(), REFS, "still one line per ref");

        let mut sizes: Vec<usize> = new.iter().map(String::len).collect();
        sizes.sort_unstable();
        let (referrers, anchor) = sizes.split_at(REFS - 1);
        assert!(
            anchor[0] > FAT,
            "one line must still store the whole snapshot — the history is \
             not what #485 economises on"
        );
        assert!(
            referrers.iter().all(|l| *l < THIN),
            "every other line must be flat in the ref count; largest was {} \
             bytes against a {THIN}-byte bound",
            referrers.last().copied().unwrap_or(0)
        );

        let old_total: usize = old.iter().map(String::len).sum();
        let new_total: usize = new.iter().map(String::len).sum();
        assert!(
            new_total * 8 < old_total,
            "at {REFS} refs the batch wrote {new_total} bytes against the \
             per-event path's {old_total} — not the collapse #485 is for"
        );
    }

    /// The filed table's method, re-run against both writers, printing the
    /// row this issue is judged on.
    ///
    /// `#[ignore]` because the per-event leg at 500 refs is the 27.6-second
    /// cost the issue is about, and a gate must not pay it on every run. Run
    /// with:
    ///
    /// ```text
    /// cargo test -p git-vista-server --bin git-vista-server -- \
    ///     --ignored --nocapture measure_the_journalling_cost_of_one_fetch
    /// ```
    #[test]
    #[ignore = "takes ~30s at 500 refs — this is the cost being measured"]
    fn measure_the_journalling_cost_of_one_fetch() {
        println!("| refs moved | writer | journal bytes | bytes/line | journalling time |");
        println!("| ---: | --- | ---: | ---: | ---: |");
        for n in [1usize, 94, 500] {
            for batched in [false, true] {
                let dir = repo();
                commit(dir.path(), "main");
                seed_remote_refs(dir.path(), n);
                let events: Vec<ActivityEvent> =
                    (0..n).map(|i| event(&format!("ref {i}"))).collect();

                let start = std::time::Instant::now();
                if batched {
                    append_all(dir.path(), &events);
                } else {
                    for e in &events {
                        append(dir.path(), e);
                    }
                }
                let elapsed = start.elapsed();

                let lines = raw_lines(dir.path());
                let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
                assert_eq!(lines.len(), n, "one line per ref, either way");
                println!(
                    "| {n} | {} | {bytes} | {} | {:.1} ms |",
                    if batched {
                        "append_all"
                    } else {
                        "append (pre-#485)"
                    },
                    bytes / n,
                    elapsed.as_secs_f64() * 1000.0,
                );
            }
        }
    }

    // ---------------------------------------------------------------------
    // #521 / ADR 0085 — the rollback story: what an older binary loses, and
    // what a line now says about the format that wrote it.
    // ---------------------------------------------------------------------

    /// The defect #521 is about, demonstrated rather than asserted in prose.
    ///
    /// A binary built before #485 has a `RefsAtEvent` with two variants and no
    /// catch-all. The types below are that enum and that event, copied from
    /// `git show 6485a9f^:crates/git-vista-core/src/activity.rs` with their
    /// serde attributes intact — a replica, because the real thing is a
    /// shipped binary and cannot be linked here.
    ///
    /// What it shows is the part that is easy to get wrong when reasoning
    /// about this from the field name: the loss is **not** confined to the
    /// capture. `refs` is one field of a whole event and `read_all` parses a
    /// line at a time, so an unreadable `status` discards the event's time,
    /// kind, ref name, summary and both tips with it. That is why a 100-ref
    /// fetch renders as one row on a rolled-back binary rather than as a
    /// hundred rows with thin captures.
    ///
    /// Nothing in this change repairs that, and the test is written to say so:
    /// it asserts the old reader **fails**, which is the cost ADR 0085 D1
    /// accepts and writes down. What #521 adds is that the *next* variant is
    /// absorbed instead — pinned by
    /// [`a_capture_this_binary_cannot_read_costs_the_capture_not_the_line`],
    /// whose subject is the same line shape read by a reader that has the
    /// catch-all.
    #[test]
    fn a_pre_485_reader_drops_the_whole_line_not_just_its_capture() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(tag = "status", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum PreBatchRefs {
            Captured {
                branches: BTreeMap<String, String>,
                #[serde(default)]
                truncated_at: Option<usize>,
            },
            CaptureFailed {
                reason: String,
            },
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct PreBatchEvent {
            time: i64,
            summary: String,
            #[serde(default)]
            refs: Option<PreBatchRefs>,
        }

        let anchor = r#"{"time":1,"kind":"Commit","ref_name":"main","summary":"anchor","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"captured","branches":{"main":"aaa"},"batch":"cafe-1-0"}}"#;
        let referrer = r#"{"time":1,"kind":"Commit","ref_name":"other","summary":"referrer","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"in_batch","batch":"cafe-1-0"}}"#;

        // The anchor survives a rollback: `captured` is a status it has always
        // known, and `batch` is an unknown *field*, which serde ignores.
        let read = serde_json::from_str::<PreBatchEvent>(anchor)
            .expect("a pre-#485 reader still reads the anchor line");
        assert_eq!(read.summary, "anchor");

        // The referrer does not, and the message names the mechanism.
        let err = serde_json::from_str::<PreBatchEvent>(referrer)
            .expect_err("a pre-#485 reader cannot read `in_batch` — this is #521");
        assert!(
            err.to_string().contains("unknown variant `in_batch`"),
            "the loss is an unknown enum tag, not a missing field: {err}"
        );

        // And the same line read by THIS binary keeps everything.
        let now: ActivityEvent =
            serde_json::from_str(referrer).expect("the current reader reads it");
        assert_eq!(now.summary, "referrer");
    }

    /// The mixed-line fixture ADR 0085 requires: one file holding a pre-#131
    /// line with no capture, a #131-era line carrying its own, a #485
    /// referrer and its anchor, and a #521-stamped line — all read, in order,
    /// each meaning what it meant when it was written.
    ///
    /// **Mixed files are the normal case.** The journal is append-only and
    /// every binary that ever ran against a repository appended to it,
    /// including — after a rollback and a re-upgrade — an older one and then a
    /// newer one again. A format change that only works on a file written
    /// entirely by one version has not been tested against any real journal.
    ///
    /// The v1 line here is a **literal**, so what "v1 on disk" looks like is
    /// pinned by a byte string rather than by re-running the writer.
    ///
    /// MUTATION: drop `#[serde(default)]` from `ReadLine::v` and the unstamped
    /// lines stop parsing — this goes red on the first assertion.
    #[test]
    fn one_journal_file_holds_every_generation_of_line_and_reads_all_of_them() {
        let dir = repo();
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                // Pre-#131: no capture field at all.
                r#"{"time":1,"kind":"Commit","ref_name":"main","summary":"pre-131","old_oid":"a","new_oid":"b","source":"App"}"#,
                "\n",
                // #131/#449: its own capture, anchoring nothing.
                r#"{"time":2,"kind":"Commit","ref_name":"main","summary":"own-capture","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"captured","branches":{"main":"aaa"}}}"#,
                "\n",
                // #485: a referrer, written before its anchor (ADR 0080 D3).
                r#"{"time":3,"kind":"Fetch","ref_name":"origin/x","summary":"referrer","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"in_batch","batch":"cafe-1-0"}}"#,
                "\n",
                // #485: the anchor, carrying the batch's one capture.
                r#"{"time":3,"kind":"Fetch","ref_name":"origin/y","summary":"anchor","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"captured","branches":{"main":"bbb"},"batch":"cafe-1-0"}}"#,
                "\n",
                // #521: stamped, and otherwise exactly a #131-era line.
                r#"{"v":1,"time":4,"kind":"Commit","ref_name":"main","summary":"stamped","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"captured","branches":{"main":"ccc"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let read = read_all(dir.path());
        assert_eq!(
            read.iter().map(|e| e.summary.as_str()).collect::<Vec<_>>(),
            vec!["pre-131", "own-capture", "referrer", "anchor", "stamped"],
            "every generation of line must survive, in file order"
        );

        // Each line still means what it meant when it was written.
        assert!(
            read[0].refs.is_none(),
            "absent stays absent — no capture was attempted, not an empty one"
        );
        assert!(
            git_vista_core::activity::refs_at(&read[0], &read).is_none(),
            "and it resolves to no information"
        );
        let Some(RefsAtEvent::Captured { branches, .. }) =
            git_vista_core::activity::refs_at(&read[1], &read)
        else {
            panic!("a #131-era line keeps its own capture: {:?}", read[1].refs);
        };
        assert_eq!(branches.get("main").map(String::as_str), Some("aaa"));

        // The referrer resolves across the file to its anchor's maps — the
        // whole point of #485, still working with a stamped line in the file.
        let Some(RefsAtEvent::Captured { branches, .. }) =
            git_vista_core::activity::refs_at(&read[2], &read)
        else {
            panic!("the referrer must resolve: {:?}", read[2].refs);
        };
        assert_eq!(
            branches.get("main").map(String::as_str),
            Some("bbb"),
            "the referrer must resolve to ITS anchor, not to the line above it"
        );

        let Some(RefsAtEvent::Captured { branches, .. }) =
            git_vista_core::activity::refs_at(&read[4], &read)
        else {
            panic!("the stamped line keeps its capture: {:?}", read[4].refs);
        };
        assert_eq!(branches.get("main").map(String::as_str), Some("ccc"));

        // And a file of ordinary lines says nothing out loud: no corruption,
        // nothing newer than this binary, no capture it could not read.
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            parse_window(&lines).report,
            WindowReport::default(),
            "the normal case must be silent — a notice on every read is a \
             notice nobody reads"
        );
    }

    /// Every line this binary appends says which format wrote it, through both
    /// writers — the single-event `append` and the batching `append_all`.
    ///
    /// The stamp is read back off disk as raw JSON rather than through
    /// [`ReadLine`], so what is asserted is the bytes on the line and not this
    /// module's own opinion of how to parse them.
    ///
    /// MUTATION: stamp `JOURNAL_FORMAT_VERSION + 1` in `append_all` — this
    /// goes red on the version assertion below.
    #[test]
    fn every_line_this_binary_appends_carries_the_format_version_it_writes() {
        let dir = repo();
        commit(dir.path(), "main");
        append(dir.path(), &event("single"));
        append_all(dir.path(), &[event("batched-a"), event("batched-b")]);

        let text = std::fs::read_to_string(dir.path().join(".git/git-vista/journal.jsonl"))
            .expect("the journal exists");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event, as ever");
        for line in &lines {
            let raw: serde_json::Value = serde_json::from_str(line).expect("a line is JSON");
            assert_eq!(
                raw.get("v").and_then(serde_json::Value::as_u64),
                Some(u64::from(JOURNAL_FORMAT_VERSION)),
                "every appended line carries this binary's format version: {line}"
            );
            // The stamp is additive: the event's own fields are still there,
            // at the top level, exactly where they have always been.
            assert!(
                raw.get("summary").is_some() && raw.get("source").is_some(),
                "the stamp wraps the event, it does not nest it: {line}"
            );
        }
    }

    /// The compatibility pin ADR 0085 D2 turns on: adding `v` must cost every
    /// reader that does not know about it **nothing**.
    ///
    /// Asserted through `serde_json::from_str::<ActivityEvent>` — the bare
    /// path, not [`ReadLine`] — because that is literally the line of code
    /// `read_all` used to be and that a pre-#485 binary still is. Reading a
    /// stamped line through the envelope that writes the stamp would prove
    /// only that this module agrees with itself.
    #[test]
    fn a_stamped_line_still_parses_through_the_bare_activity_event_path() {
        let dir = repo();
        commit(dir.path(), "main");
        append(dir.path(), &event("stamped"));
        let text = std::fs::read_to_string(dir.path().join(".git/git-vista/journal.jsonl"))
            .expect("the journal exists");
        let line = text.lines().next().expect("a line");
        assert!(line.contains("\"v\":"), "precondition: the line is stamped");

        let bare: ActivityEvent = serde_json::from_str(line)
            .expect("a reader that has never heard of `v` must still read the line");
        assert_eq!(bare.summary, "stamped");
        assert_eq!(bare.source, ActivitySource::App);
        assert!(
            matches!(bare.refs, Some(RefsAtEvent::Captured { .. })),
            "and its capture, unchanged: {:?}",
            bare.refs
        );
    }

    /// The change that makes the *next* format change survivable: a `status`
    /// this binary has no reading for costs the line its **capture**, not the
    /// line.
    ///
    /// The fixture line is what a future git-vista might write — an unknown
    /// capture status on an otherwise ordinary event. Before #521 this line
    /// failed `ActivityEvent` outright and `read_all` dropped it, taking the
    /// time, kind, ref name, summary and both tips with it.
    ///
    /// The reading is `None` — *no information* — and never an empty map, so a
    /// replayer concludes nothing rather than "every branch was deleted".
    ///
    /// MUTATION: delete the `#[serde(other)] Unknown` arm from `RefsAtEvent`
    /// and this goes red on the first assertion — the line disappears again.
    #[test]
    fn a_capture_this_binary_cannot_read_costs_the_capture_not_the_line() {
        let dir = repo();
        let path = dir.path().join(".git/git-vista/journal.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"v":9,"time":7,"kind":"Commit","ref_name":"main","summary":"from the future","old_oid":"a","new_oid":"b","source":"App","refs":{"status":"replayed_from_pack","pack":"deadbeef"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let read = read_all(dir.path());
        assert_eq!(
            read.len(),
            1,
            "an unreadable capture must not take the whole event with it"
        );
        assert_eq!(
            read[0].summary, "from the future",
            "the fields this binary DOES understand are all still there"
        );
        assert_eq!(
            read[0].refs,
            Some(RefsAtEvent::Unknown),
            "and the capture is recorded as one it cannot read — not as absent"
        );
        assert!(
            git_vista_core::activity::refs_at(&read[0], &read).is_none(),
            "which resolves to no information, never an empty observation"
        );
    }

    /// The read-time answer ADR 0085 D2 buys, exercised rather than asserted:
    /// a line stamped newer than this binary writes is **read**, and the
    /// reader names the version instead of leaving an operator with a serde
    /// error and a guess.
    ///
    /// Driven through [`parse_window`], whose report is a value, so what is
    /// checked is the sentence itself and not that stderr was written to.
    #[test]
    fn a_line_from_a_newer_format_is_read_and_the_reader_says_which_version() {
        let newer = JOURNAL_FORMAT_VERSION + 1;
        let line = format!(
            r#"{{"v":{newer},"time":7,"kind":"Commit","ref_name":"main","summary":"from the future","old_oid":"a","new_oid":"b","source":"App","refs":{{"status":"replayed_from_pack"}}}}"#
        );
        let window = parse_window(&[line.as_str(), "{not json}"]);

        assert_eq!(
            window.events.len(),
            1,
            "a newer line is read as far as it is understood, never refused"
        );
        assert_eq!(window.report.from_newer, 1);
        assert_eq!(window.report.newest_version, Some(newer));
        assert_eq!(window.report.unreadable_captures, 1);
        assert_eq!(
            window.report.unreadable.len(),
            1,
            "and a genuinely corrupt line is still skipped loudly, per line"
        );

        let said = window.report.notices().join("\n");
        assert!(
            said.contains(&format!("journal format v{newer}"))
                && said.contains(&format!("this binary writes v{JOURNAL_FORMAT_VERSION}")),
            "the notice must name both versions, or it is not an answer: {said}"
        );
        assert!(
            said.contains("never as \"nothing was there\""),
            "and must say what the missing capture does NOT mean: {said}"
        );
        assert!(
            said.contains("skipping an unreadable journal line"),
            "the pre-existing loud skip survives the rewrite: {said}"
        );
    }

    /// A version stamp must still explain a line whose event shape this binary
    /// cannot deserialize. Probing `v` only after [`ReadLine`] succeeds makes
    /// the stamp useless in exactly the case it exists to diagnose.
    #[test]
    fn incompatible_newer_lines_still_report_their_writer_version() {
        let newer = JOURNAL_FORMAT_VERSION + 1;
        let future_kind = format!(
            r#"{{"v":{newer},"time":7,"kind":"FutureKind","ref_name":"main","summary":"future kind","old_oid":"a","new_oid":"b","source":"App"}}"#
        );
        let future_required_field = format!(
            r#"{{"v":{newer},"time":8,"kind":"Commit","ref_name":"main","summary":"future required field","old_oid":"a","new_oid":"b"}}"#
        );

        let kind_error = serde_json::from_str::<ReadLine>(&future_kind)
            .err()
            .expect("the future event kind must be incompatible");
        assert!(
            kind_error
                .to_string()
                .contains("unknown variant `FutureKind`"),
            "fixture must fail on the future kind, not malformed JSON: {kind_error}"
        );
        let field_error = serde_json::from_str::<ReadLine>(&future_required_field)
            .err()
            .expect("the missing required event field must be incompatible");
        assert!(
            field_error.to_string().contains("missing field `source`"),
            "fixture must fail on a required event field: {field_error}"
        );

        let window = parse_window(&[&future_kind, &future_required_field]);
        assert!(
            window.events.is_empty(),
            "neither incompatible event may be invented"
        );
        assert_eq!(window.report.unreadable.len(), 2);
        assert_eq!(
            window.report.from_newer, 2,
            "the envelope version must be counted independently of event decoding"
        );
        assert_eq!(window.report.newest_version, Some(newer));
        let said = window.report.notices().join("\n");
        assert!(
            said.contains("newer journal formats")
                && said.contains(&format!("journal format v{newer}")),
            "the version notice must explain the two unreadable lines: {said}"
        );
        assert!(
            said.contains("Compatible newer events were retained")
                && said.contains("incompatible newer events were skipped"),
            "the notice must distinguish retained compatible events from the two skipped incompatible events: {said}"
        );
        assert!(
            !said.contains("They were read as far as this binary understands them"),
            "the aggregate must not claim skipped events were partially retained: {said}"
        );
    }

    /// Mixed generations are normal in an append-only file. An aggregate
    /// count paired with only the maximum version must not claim every line
    /// came from that one maximum writer.
    #[test]
    fn mixed_future_versions_are_attributed_to_newer_formats_not_only_the_newest() {
        let v2 = JOURNAL_FORMAT_VERSION + 1;
        let v3 = JOURNAL_FORMAT_VERSION + 2;
        let line = |version, summary| {
            format!(
                r#"{{"v":{version},"time":7,"kind":"Commit","ref_name":"main","summary":"{summary}","old_oid":"a","new_oid":"b","source":"App"}}"#
            )
        };
        let line_v2 = line(v2, "from v2");
        let line_v3 = line(v3, "from v3");
        let window = parse_window(&[&line_v2, &line_v3]);

        assert_eq!(window.events.len(), 2);
        assert_eq!(window.report.from_newer, 2);
        assert_eq!(window.report.newest_version, Some(v3));
        let said = window.report.notices().join("\n");
        assert!(
            said.contains("2 journal line(s) were written by newer journal formats"),
            "the aggregate must not attribute both lines to one version: {said}"
        );
        assert!(
            said.contains(&format!("newest was journal format v{v3}")),
            "the notice must still name the newest version: {said}"
        );
        assert!(
            !said.contains(&format!(
                "2 journal line(s) were written by journal format v{v3}"
            )),
            "mixed v{v2}/v{v3} lines must not be blamed wholly on v{v3}: {said}"
        );
    }
}
