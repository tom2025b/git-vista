//! Pivot detection and the `chapters.txt` sidecar (#325, lane 3).
//!
//! Two deliverables, both derived from the same commit graph the print sheet
//! renders, and both existing for the same reason: a silent 4-minute scroll
//! is useless to someone narrating over it. They tell the owner (a) *when*
//! to talk (`chapters.txt`, read in a video player while recording or
//! editing) and (b) *what to say* (the pivot callout card, baked into the
//! video itself during the dwell `pacing::build_timeline` already carves out
//! — see `pacing.rs` lines 1-19 and the module doc on `Segment`/`Pivot`).
//!
//! This module never decides *how long* to hold or *how fast* to scroll —
//! that is `pacing.rs`'s job and it is already built and tested. This module
//! only decides *which* commits are worth a pivot, ranks them so a busy
//! history doesn't turn the video into one long pause, and writes the text
//! that appears on the callout card and in the sidecar. Producing pixels
//! from that text is lane 2's job (frame rendering), not this file's.

use git_vista_core::model::{CommitSummary, GitRef, GraphRow, RefKind};

use crate::pacing::{Pivot, Segment};

// ---------------------------------------------------------------------------
// Calendar math, dependency-free
// ---------------------------------------------------------------------------

/// Convert a Unix timestamp (seconds) to a proleptic-Gregorian
/// `(year, month, day)`, `month` and `day` both 1-based.
///
/// This crate carries no date/time dependency (`chrono` et al.) for one
/// field's worth of use: turning `CommitSummary::time` into a month number
/// for boundary detection and a human date for the callout card. Howard
/// Hinnant's `civil_from_days` algorithm (public domain, described at
/// <http://howardhinnant.github.io/date_algorithms.html>) does this exactly,
/// in integer arithmetic, correct across the whole proleptic Gregorian
/// calendar including leap years, with no allocation and no external crate.
/// It is short enough to host-test directly against known dates rather than
/// trust by citation alone (see `civil_date_matches_known_reference_dates`
/// below).
fn civil_from_unix(unix_secs: i64) -> (i64, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day-of-era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year-of-era, [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month, prime form [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `"August 2026"` — the label a month-boundary pivot shows.
fn month_label(unix_secs: i64) -> String {
    let (y, m, _) = civil_from_unix(unix_secs);
    format!("{} {y}", MONTH_NAMES[(m - 1) as usize])
}

/// `"Aug 5, 2026"` — compact enough for a one-line card detail.
fn short_date(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_unix(unix_secs);
    format!("{} {d}, {y}", &MONTH_NAMES[(m - 1) as usize][..3])
}

// ---------------------------------------------------------------------------
// Pivot detection and ranking
// ---------------------------------------------------------------------------

/// Why one commit scored the way it did — kept internal (the public contract
/// is [`Pivot`], defined in `pacing.rs`, which only wants a `y`/label/detail,
/// not this crate's reasoning for producing them). Exists so
/// [`significance_score`] and [`render_label`]/[`render_detail`] agree on
/// what happened at a commit without recomputing it three times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Tag,
    Branch,
    RemoteBranch,
    Head,
    Merge,
    MonthBoundary,
}

/// Point weight per reason, tuned so a real landmark always outranks a
/// routine one. A tag is a human deliberately marking a release — the
/// strongest signal available in this graph — so it must outscore a bare
/// merge even though merges are far more numerous. Octopus merges (3+
/// parents) are rare and always deliberate, so they add a per-extra-parent
/// bonus on top of the merge base rather than being treated the same as an
/// ordinary two-parent merge.
///
/// The `rank_pivots_puts_the_highest_scorer_first_before_any_cap_or_resort`
/// and `a_release_tag_survives_a_max_pivots_cap_that_drops_a_routine_merge`
/// tests below are the guard: they are exactly the failure mode a "count
/// refs + count merges" naive rule would produce if it weighted them
/// equally instead of by how much a human actually decided to mark that
/// moment.
fn reason_score(reason: Reason) -> i64 {
    match reason {
        Reason::Tag => 50,
        Reason::Branch => 20,
        Reason::RemoteBranch => 10,
        Reason::Head => 5,
        Reason::Merge => 10,
        Reason::MonthBoundary => 15,
    }
}

fn ref_reason(kind: &RefKind) -> Reason {
    match kind {
        RefKind::Tag => Reason::Tag,
        RefKind::Branch => Reason::Branch,
        RefKind::RemoteBranch => Reason::RemoteBranch,
        RefKind::Head => Reason::Head,
    }
}

/// One scored candidate before ranking/capping. `pub(crate)` only because
/// [`rank_pivots`] (also `pub(crate)`, for the same reason — see its doc
/// comment) has to return something; nothing outside this crate ever sees
/// it. External callers only ever see the capped, sorted `Vec<Pivot>`
/// [`detect_pivots`] returns.
pub(crate) struct Candidate {
    row_idx: usize,
    y: f64,
    score: i64,
    reasons: Vec<Reason>,
}

/// Score one row: sum of every applicable reason. Summed, not
/// max-of-applicable, because a tagged merge (a release commit) really is
/// more significant than either a plain tag or a plain merge alone — the
/// video should hold longest exactly there.
fn significance_score(
    commit: &CommitSummary,
    refs: &[GitRef],
    is_month_boundary: bool,
) -> Vec<Reason> {
    let mut reasons = Vec::new();
    if commit.is_merge() {
        reasons.push(Reason::Merge);
        // Octopus bonus: +2 per parent beyond the second, folded into the
        // base Merge reason's score by pushing it again isn't right (that
        // would double the label text); instead this is applied by the
        // caller via `octopus_bonus`, kept separate so `reasons` stays a
        // list of *kinds* a human can read off, not a padded tally.
    }
    for r in refs {
        reasons.push(ref_reason(&r.kind));
    }
    if is_month_boundary {
        reasons.push(Reason::MonthBoundary);
    }
    reasons
}

fn octopus_bonus(commit: &CommitSummary) -> i64 {
    if commit.parents.len() > 2 {
        2 * (commit.parents.len() as i64 - 2)
    } else {
        0
    }
}

/// Score and rank every pivot candidate in the rendered graph, best-first
/// (highest [`reason_score`] total first, ties broken by `y` ascending —
/// favours the earlier of two equally-significant moments, arbitrary but
/// deterministic). See [`reason_score`]'s doc comment for the specific
/// real-vs-routine failure this ordering defends against.
///
/// Split out of [`detect_pivots`] — which immediately truncates to
/// `max_pivots` and re-sorts into video (`y`-ascending) order — for one
/// reason: once that re-sort has happened, the *ranking* that decided which
/// candidates survived the cap is no longer visible in the output at all.
/// Any test that only inspects `detect_pivots`'s final `Vec<Pivot>` with
/// `max_pivots >= candidates.len()` passes regardless of which candidates
/// "won", because nothing was truncated to make the ranking observable. This
/// function is the scored intermediate a test (or a future in-crate caller
/// that needs the *why*, not just the final cut) can assert on directly:
/// `rank_pivots(...)[0]` is unconditionally the single most significant
/// candidate, full stop, before any cap or re-sort touches it.
///
/// `rows` and `commit_ys` must be the same length and in the same order —
/// this is `capture::CaptureResult`'s own contract (`commit_ys`: "One
/// `CommitY` per commit node found on the sheet, in row order"), so
/// `rows[i]` and `commit_ys[i]` describe the same commit. Mismatched lengths
/// are a caller bug (a capture that silently dropped or duplicated a node),
/// not a data condition this function should paper over, so it asserts
/// rather than truncating one to fit the other.
pub(crate) fn rank_pivots(rows: &[GraphRow], commit_ys: &[CommitY]) -> Vec<Candidate> {
    assert_eq!(
        rows.len(),
        commit_ys.len(),
        "rows and commit_ys must be parallel (same length, same order) — \
         see capture::CaptureResult::commit_ys's doc comment"
    );

    let mut prev_month: Option<(i64, u32)> = None;
    let mut candidates: Vec<Candidate> = Vec::new();

    for (idx, row) in rows.iter().enumerate() {
        let (year, month, _) = civil_from_unix(row.commit.time);
        let is_month_boundary = match prev_month {
            Some(prev) => prev != (year, month),
            // The very first row never counts as a boundary — there is
            // nothing before it to transition from, and marking row 0 would
            // just be "the video started", which is not a landmark.
            None => false,
        };
        prev_month = Some((year, month));

        let reasons = significance_score(&row.commit, &row.refs, is_month_boundary);
        if reasons.is_empty() {
            continue;
        }
        let score: i64 =
            reasons.iter().map(|r| reason_score(*r)).sum::<i64>() + octopus_bonus(&row.commit);

        candidates.push(Candidate {
            row_idx: idx,
            y: commit_ys[idx].y,
            score,
            reasons,
        });
    }

    // Highest score first; ties broken by y ascending (earlier moment wins,
    // deterministically — see this function's doc comment above).
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(a.y.partial_cmp(&b.y).unwrap()));
    candidates
}

/// Detect and rank pivot candidates from the rendered graph, returning at
/// most `max_pivots`, in ascending `y` (top-to-bottom, i.e. the order the
/// scroll actually encounters them in).
///
/// Cap exists because the owner's own ask was explicit about the failure
/// mode: *"a 477-commit repo with a merge every third commit would produce
/// a video that is mostly paused."* A merge-every-third-commit history has
/// roughly 160 merge-only candidates; capping to, say, 12 means only the
/// dozen most-deliberate moments (tags, octopus merges, tagged merges, real
/// month transitions) survive, and the rest scroll through at the density
/// pacing already computes for them.
///
/// Ranking itself (which candidates are the "most-deliberate" ones that
/// survive the cap) is [`rank_pivots`]'s job, not this function's — see its
/// doc comment for why the two are kept separate.
pub fn detect_pivots(rows: &[GraphRow], commit_ys: &[CommitY], max_pivots: usize) -> Vec<Pivot> {
    let mut candidates = rank_pivots(rows, commit_ys);
    candidates.truncate(max_pivots);
    // Re-sort into video order (ascending y) now that the cap has been
    // applied — the scroll encounters pivots top-to-bottom regardless of
    // which order they won the ranking in.
    candidates.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap());

    candidates
        .into_iter()
        .map(|c| {
            let row = &rows[c.row_idx];
            Pivot {
                y: c.y,
                label: render_label(row, &c.reasons),
                detail: render_detail(row, &c.reasons),
            }
        })
        .collect()
}

/// Re-exported so callers of this module don't need a separate `use
/// crate::pacing::CommitY` — `detect_pivots`'s signature already needs the
/// type and there is no reason to make callers import it from a second
/// place.
pub use crate::pacing::CommitY;

/// Short label for the callout card's title line — the "what" in one glance.
fn render_label(row: &GraphRow, reasons: &[Reason]) -> String {
    // Priority order for the headline reason, most-specific first: a tag
    // name is the single most useful word to show ("v1.2.0"), then a branch
    // name, then "Merge", then falling back to the month if that's the only
    // reason this row was picked at all.
    if let Some(tag) = row.refs.iter().find(|r| r.kind == RefKind::Tag) {
        return format!("Tag: {}", tag.name);
    }
    if let Some(branch) = row.refs.iter().find(|r| r.is_branch()) {
        return format!("Branch: {}", branch.name);
    }
    if reasons.contains(&Reason::Merge) {
        return "Merge".to_string();
    }
    if reasons.contains(&Reason::MonthBoundary) {
        return month_label(row.commit.time);
    }
    // Unreachable in practice (every row with a nonempty `reasons` matches
    // one of the arms above), but a HEAD-only row falls through here rather
    // than panicking if the reason list ever grows a new variant.
    short_date(row.commit.time)
}

/// The card's body text: what happened, when, who — capped to roughly a
/// dozen words.
///
/// Why a dozen: the dwell that holds the scroll while this shows is
/// `pacing::DEFAULT_DWELL_SECS` (pacing.rs line ~66) = 3.0 seconds. Average
/// silent-reading speed for a short on-screen caption (not dense prose) is
/// commonly cited around 200-250 words/minute, i.e. roughly 3-4 words per
/// second; three seconds of *comfortable* (not speed-) reading is therefore
/// about 10-12 words, with a little margin for the reader's eye to also
/// land on the callout box itself before reading starts. Twelve is the cap
/// this function enforces, not a suggestion left to whoever writes the copy.
const CARD_MAX_WORDS: usize = 12;

fn render_detail(row: &GraphRow, reasons: &[Reason]) -> String {
    let what = if reasons.contains(&Reason::Merge) {
        format!("Merge: {}", row.commit.summary)
    } else {
        row.commit.summary.clone()
    };
    let full = format!(
        "{what} — {}, {}",
        row.commit.author,
        short_date(row.commit.time)
    );
    cap_words(&full, CARD_MAX_WORDS)
}

/// Truncate to at most `max_words` words, appending an ellipsis if anything
/// was cut. Splits on whitespace only (no attempt at sentence-aware
/// truncation) — this is a 3-second card, not a summary, and the exact cut
/// point matters far less than the cap being enforced at all.
fn cap_words(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max_words {
        return text.to_string();
    }
    format!("{}…", words[..max_words].join(" "))
}

// ---------------------------------------------------------------------------
// chapters.txt sidecar
// ---------------------------------------------------------------------------

/// The minimum gap YouTube's (undocumented but widely relied on) chapter
/// parser enforces between chapter marks, in seconds. Two pivots this close
/// together in final video time would otherwise produce a `chapters.txt`
/// that YouTube silently refuses to turn into chapters at all — better to
/// drop the lower-ranked one ourselves than ship a sidecar that looks right
/// and does nothing.
const MIN_CHAPTER_GAP_SECS: f64 = 10.0;

/// Format `chapters.txt`: one `H:MM:SS Label` (or `HH:MM:SS` past the hour
/// mark) line per chapter, ascending by time, always starting with a
/// synthetic `0:00` entry.
///
/// **Format chosen: YouTube's plain-text video-description chapter
/// convention** (`timestamp<space>title`, one per line, first entry
/// required at `0:00`) rather than WebVTT chapters or an `.srt` twin. Three
/// reasons: (1) it needs no container/sidecar association step — the owner
/// pastes it straight into the upload description and YouTube parses it,
/// which is where "the owner narrates over it" videos actually end up; (2)
/// most desktop/mobile video players that support "jump to chapter" from an
/// external file (VLC, mpv via `--chapters`) also accept this exact
/// plain-text shape or a trivial reformat of it, so it isn't a YouTube-only
/// choice; (3) WebVTT chapters require a `WEBVTT` header, cue identifiers
/// and `-->` timestamp ranges for a feature (`chapters.txt` is a *scrub aid
/// while narrating*, not a subtitle track) that doesn't need VTT's
/// captioning machinery at all.
///
/// A pivot is skipped (not error) if: it has no matching dwell segment in
/// `segments` (a caller passed pivots that were never given to
/// `pacing::build_timeline`, so there's nothing to time it against), or it
/// falls within `MIN_CHAPTER_GAP_SECS` of the previous surviving chapter.
/// The second rule keeps every remaining line meaningful rather than
/// shipping a file YouTube would reject outright.
pub fn format_chapters(pivots: &[Pivot], segments: &[Segment]) -> String {
    let mut lines = vec![(0.0_f64, "Start".to_string())];

    let mut elapsed = 0.0_f64;
    for pivot in pivots {
        // Find this pivot's own dwell segment: the zero-length segment at
        // exactly its y (built by `pacing::build_timeline`'s pivot-splitting
        // loop — see pacing.rs's "A pivot inside this band SPLITS it"
        // comment). We walk `segments` once per pivot rather than
        // precomputing a y->elapsed map because a scrollcast timeline is at
        // most a few hundred segments; this is not a hot path.
        let mut t = 0.0_f64;
        let mut found = false;
        for seg in segments {
            if seg.is_dwell() && seg.y_start == pivot.y {
                found = true;
                break;
            }
            t += seg.duration_secs;
        }
        if !found {
            continue;
        }
        if t - elapsed < MIN_CHAPTER_GAP_SECS && !lines.is_empty() {
            continue;
        }
        elapsed = t;
        lines.push((t, pivot.label.clone()));
    }

    lines
        .into_iter()
        .map(|(t, label)| format!("{} {label}", format_timestamp(t)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `H:MM:SS` under an hour, `HH:MM:SS` at/past it — the exact form YouTube's
/// chapter parser accepts (it also accepts bare `MM:SS`, but the longer form
/// is unambiguous once a video crosses an hour and costs nothing under one).
fn format_timestamp(total_secs: f64) -> String {
    let total = total_secs.round().max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::DEFAULT_DWELL_SECS;
    use git_vista_core::model::Oid;

    fn commit(time: i64, parents: usize, summary: &str) -> CommitSummary {
        CommitSummary {
            id: Oid("deadbeef".repeat(5)),
            parents: (0..parents).map(|i| Oid(format!("p{i}"))).collect(),
            summary: summary.to_string(),
            author: "Ada".to_string(),
            time,
        }
    }

    fn row(time: i64, parents: usize, refs: Vec<GitRef>, summary: &str) -> GraphRow {
        GraphRow {
            commit: commit(time, parents, summary),
            row: 0,
            lane: 0,
            refs,
            color: 0,
            on_remote: false,
        }
    }

    fn tag(name: &str) -> GitRef {
        GitRef {
            name: name.to_string(),
            kind: RefKind::Tag,
            target: Oid("x".into()),
        }
    }

    fn gitref(name: &str, kind: RefKind) -> GitRef {
        GitRef {
            name: name.to_string(),
            kind,
            target: Oid("x".into()),
        }
    }

    // --- civil date math ------------------------------------------------

    #[test]
    fn civil_date_matches_known_reference_dates() {
        // 1970-01-01T00:00:00Z is the epoch itself — the base case every
        // other date's correctness is anchored to.
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        // 2000-03-01 crosses the algorithm's internal March-based year
        // boundary (`mp`/`y` adjustment) right at a leap year — the single
        // trickiest case for any civil-from-days implementation.
        assert_eq!(civil_from_unix(951_868_800), (2000, 3, 1));
        // 2024-02-29 exists only because 2024 is a leap year; a mutation
        // that dropped the `/100`/`/400` leap correction would misdate
        // everything from here onward.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29));
    }

    // --- ranking ----------------------------------------------------------

    #[test]
    fn rank_pivots_puts_the_highest_scorer_first_before_any_cap_or_resort() {
        // Direct test of the scored intermediate itself, before
        // `detect_pivots`'s truncate+resort-by-y ever touches it. Row 0
        // (earlier y) is a routine merge (score 10); row 1 (later y) carries
        // a release tag (score 50). If ranking degenerated to plain y-order
        // (the inverted-comparator mutation), row 0 would still be first
        // here. If the tag's weight were gutted (the 50 -> 1 mutation), the
        // merge's score-10 would outrank it and row 0 would again be first.
        // Neither mutation can pass this: it asserts the winner by both
        // identity (`row_idx`) and its actual score.
        let rows = vec![
            row(1_000, 2, vec![], "Merge branch 'feature'"),
            row(2_000, 1, vec![tag("v1.0.0")], "Release v1.0.0"),
        ];
        let ys = vec![CommitY { y: 10.0 }, CommitY { y: 20.0 }];
        let ranked = rank_pivots(&rows, &ys);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].row_idx, 1, "the tagged release must rank first");
        assert_eq!(ranked[0].score, 50);
        assert_eq!(ranked[1].row_idx, 0);
        assert_eq!(ranked[1].score, 10);
    }

    #[test]
    fn a_release_tag_survives_a_max_pivots_cap_that_drops_a_routine_merge() {
        // The exact failure mode the task calls out: "a trivial merge does
        // not outrank a release tag." Unlike the old version of this test,
        // the cap (`max_pivots: 1`) actually forces a choice between the
        // two candidates — with a cap that never binds, `detect_pivots`'s
        // final re-sort-by-y makes the ranking that produced its output
        // unobservable, which is exactly how this finding's two mutations
        // (inverted comparator, tag weight 50 -> 1) both survived unnoticed.
        let rows = vec![
            row(1_000, 2, vec![], "Merge branch 'feature'"),
            row(2_000, 1, vec![tag("v1.0.0")], "Release v1.0.0"),
        ];
        let ys = vec![CommitY { y: 10.0 }, CommitY { y: 20.0 }];
        let pivots = detect_pivots(&rows, &ys, 1);
        assert_eq!(pivots.len(), 1);
        assert!(
            pivots[0].label.contains("v1.0.0"),
            "the release tag must survive the cap, not the bare merge: {:?}",
            pivots[0].label
        );
    }

    #[test]
    fn max_pivots_keeps_the_high_scorers_not_merely_a_count() {
        // Five candidates with deliberately spread-apart scores (tag 50,
        // branch 20, merge 10, remote-branch 10, head 5 — see
        // `reason_score`), all in the same civil month so no month-boundary
        // reason muddies the numbers, capped to 2. This must keep the tag
        // and the branch specifically — asserting on which *labels* survive,
        // not just that the output length is 2. A length-only assertion
        // (the shape of the pre-existing `max_pivots_caps_output...` test
        // below) cannot distinguish "kept the top 2" from "kept the bottom
        // 2", which is exactly the gap an inverted ranking comparator hides
        // behind.
        let rows = vec![
            row(1_000, 1, vec![tag("v1.0.0")], "release"),
            row(
                1_000,
                1,
                vec![gitref("main", RefKind::Branch)],
                "branch tip",
            ),
            row(1_000, 2, vec![], "routine merge"),
            row(
                1_000,
                1,
                vec![gitref("origin/main", RefKind::RemoteBranch)],
                "remote tip",
            ),
            row(1_000, 1, vec![gitref("HEAD", RefKind::Head)], "head"),
        ];
        let ys: Vec<CommitY> = (0..5).map(|i| CommitY { y: i as f64 * 10.0 }).collect();
        let pivots = detect_pivots(&rows, &ys, 2);
        assert_eq!(pivots.len(), 2);
        let labels: Vec<&str> = pivots.iter().map(|p| p.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.contains("v1.0.0")), "{labels:?}");
        assert!(
            labels
                .iter()
                .any(|l| l.contains("main") && !l.contains("origin")),
            "{labels:?}"
        );
    }

    #[test]
    fn max_pivots_caps_output_even_when_every_row_qualifies() {
        // The task's own stated failure mode: a merge-every-third-commit
        // history must not produce a mostly-paused video. Ten qualifying
        // rows, cap of 3, must yield exactly 3.
        let rows: Vec<GraphRow> = (0..10)
            .map(|i| row(1_000 + i as i64, 2, vec![], "merge"))
            .collect();
        let ys: Vec<CommitY> = (0..10).map(|i| CommitY { y: i as f64 * 10.0 }).collect();
        let pivots = detect_pivots(&rows, &ys, 3);
        assert_eq!(pivots.len(), 3);
    }

    #[test]
    fn a_non_merge_untagged_row_is_never_a_pivot() {
        // Zero applicable reasons must mean zero candidacy, not a
        // score-of-zero pivot that still occupies a dwell slot.
        let rows = vec![row(1_000, 1, vec![], "routine commit")];
        let ys = vec![CommitY { y: 5.0 }];
        assert!(detect_pivots(&rows, &ys, 10).is_empty());
    }

    #[test]
    fn the_first_row_is_never_flagged_as_a_month_boundary() {
        // There is nothing before row 0 to transition from; flagging it
        // would just mark "the video started" as a pivot, which is not a
        // landmark and would waste a dwell on nothing.
        let rows = vec![row(1_000, 1, vec![], "first ever commit")];
        let ys = vec![CommitY { y: 0.0 }];
        assert!(detect_pivots(&rows, &ys, 10).is_empty());
    }

    #[test]
    fn a_genuine_month_boundary_between_two_non_merge_rows_is_a_pivot() {
        let jan = 1_704_067_200; // 2024-01-01T00:00:00Z
        let feb = 1_706_745_600; // 2024-02-01T00:00:00Z
        let rows = vec![
            row(jan, 1, vec![], "january work"),
            row(feb, 1, vec![], "february work"),
        ];
        let ys = vec![CommitY { y: 0.0 }, CommitY { y: 100.0 }];
        let pivots = detect_pivots(&rows, &ys, 10);
        assert_eq!(pivots.len(), 1);
        assert_eq!(pivots[0].label, "February 2024");
    }

    #[test]
    #[should_panic(expected = "rows and commit_ys must be parallel")]
    fn mismatched_row_and_commit_y_lengths_panics_rather_than_silently_misaligning() {
        let rows = vec![row(1_000, 1, vec![], "x")];
        let ys: Vec<CommitY> = vec![];
        let _ = detect_pivots(&rows, &ys, 10);
    }

    // --- card text ----------------------------------------------------------

    #[test]
    fn card_detail_never_exceeds_the_documented_word_cap() {
        let r = row(
            1_000,
            2,
            vec![],
            "This is a very long commit summary line that goes on and on and on past any reasonable card length",
        );
        let detail = render_detail(&r, &[Reason::Merge]);
        let word_count = detail.split_whitespace().count();
        assert!(
            word_count <= CARD_MAX_WORDS + 1, // the trailing "…" attaches to the last word, not a separate token
            "card text has {word_count} words: {detail:?}"
        );
    }

    #[test]
    fn a_short_detail_is_left_untouched_with_no_ellipsis() {
        let r = row(1_000, 1, vec![], "fix typo");
        let detail = render_detail(&r, &[]);
        assert!(!detail.contains('…'));
    }

    // --- chapters.txt ---------------------------------------------------

    #[test]
    fn chapters_txt_always_starts_at_0_00() {
        let out = format_chapters(&[], &[]);
        assert!(out.starts_with("0:00 Start"));
    }

    #[test]
    fn a_pivots_timestamp_matches_its_dwell_segments_elapsed_start_time() {
        let pivots = vec![Pivot {
            y: 100.0,
            label: "Merge".to_string(),
            detail: "".to_string(),
        }];
        let segments = vec![
            Segment {
                y_start: 0.0,
                y_end: 100.0,
                duration_secs: 20.0,
            },
            Segment {
                y_start: 100.0,
                y_end: 100.0,
                duration_secs: DEFAULT_DWELL_SECS,
            },
            Segment {
                y_start: 100.0,
                y_end: 200.0,
                duration_secs: 20.0,
            },
        ];
        let out = format_chapters(&pivots, &segments);
        // 20 seconds of scroll precede the dwell, so the chapter must land
        // at 0:20 — not 0:00 (ignoring elapsed scroll time) and not
        // mid-dwell (which would suggest the callout was already showing
        // before the chapter mark, backwards from what a "jump here" mark
        // should mean).
        assert!(out.contains("0:20 Merge"), "{out}");
    }

    #[test]
    fn a_pivot_with_no_matching_dwell_segment_is_silently_skipped() {
        // A caller bug (pivots not run through build_timeline) must not
        // panic or emit a garbage timestamp.
        let pivots = vec![Pivot {
            y: 999.0,
            label: "Orphan".to_string(),
            detail: "".to_string(),
        }];
        let out = format_chapters(&pivots, &[]);
        assert_eq!(out, "0:00 Start\n");
    }

    #[test]
    fn chapters_closer_than_the_minimum_gap_are_dropped_not_duplicated() {
        let pivots = vec![
            Pivot {
                y: 100.0,
                label: "First".to_string(),
                detail: "".to_string(),
            },
            Pivot {
                y: 105.0,
                label: "TooClose".to_string(),
                detail: "".to_string(),
            },
        ];
        let segments = vec![
            Segment {
                y_start: 0.0,
                y_end: 100.0,
                duration_secs: 20.0,
            },
            Segment {
                y_start: 100.0,
                y_end: 100.0,
                duration_secs: DEFAULT_DWELL_SECS,
            },
            Segment {
                y_start: 100.0,
                y_end: 105.0,
                duration_secs: 1.0, // lands 4s after "First" - inside the 10s floor
            },
            Segment {
                y_start: 105.0,
                y_end: 105.0,
                duration_secs: DEFAULT_DWELL_SECS,
            },
        ];
        let out = format_chapters(&pivots, &segments);
        assert!(out.contains("First"));
        assert!(!out.contains("TooClose"), "{out}");
    }

    #[test]
    fn format_timestamp_rolls_over_into_hh_mm_ss_past_an_hour() {
        assert_eq!(format_timestamp(59.0), "0:59");
        assert_eq!(format_timestamp(60.0), "1:00");
        assert_eq!(format_timestamp(3_661.0), "1:01:01");
    }
}
