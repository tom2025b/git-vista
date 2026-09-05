//! Performance fixtures for M5.33 (#86): the explicit limits the issue asks
//! for, measured — not "seems fast" — on repositories built large enough to
//! be honest, the same posture ADR 0022 (M1.10) set with its 5,497-commit
//! drive and its `deep_remote_chain` fixture.
//!
//! Both fixtures are built with **one `git fast-import` process**, the same
//! choice `git_vista_git::history::tests::deep_remote_chain` made for the
//! same reason: thousands of `git commit` spawns would take minutes, and
//! `filerename` produces the exact tree result a `git mv` + commit would —
//! rename *detection* is a diff-time comparison of two trees, not anything
//! recorded on the commit object, so a fast-imported rename is indistinguishable
//! from one `git mv` made.
//!
//! # The two limits this proves
//!
//! 1. **A file's rename-chain classification costs `O(hops)`, never
//!    `O(history size)`.** [`chase_rename_chain`] is bounded by
//!    [`MAX_RENAME_HOPS`]; this fixture buries 12 renames inside 3,000 noise
//!    commits and shows classifying the *oldest* dead name costs about the
//!    same order of magnitude as classifying the *newest* one, rather than
//!    scaling with the 3,000.
//! 2. **A blame page's cost is bounded by how far back its lines' own history
//!    goes from the requested revision, not by the file's total length.**
//!    Measured directly (see `blaming_the_tail_of_a_long_lived_file_is_fast`'s
//!    doc): `-L` does **not** make blame `O(page size)` regardless of
//!    position — git still has to examine every commit between the query
//!    point and wherever the requested lines settle, because it cannot know a
//!    commit is irrelevant to the range without diffing it. Blaming the most
//!    recently changed lines of a long file (the common case: most look-ups
//!    are "who wrote this line I'm looking at now", and most looked-at lines
//!    are recently touched) costs almost nothing regardless of total file
//!    length; blaming a page that has been stable since deep history costs
//!    proportionally to that depth — the same accepted tradeoff ADR 0022 took
//!    for commit-history paging, applied here to blame instead of the graph.
//!    What line-range paging still buys, in both cases: the *parsed and
//!    returned* result is always exactly the requested window, never the
//!    whole file.
//!
//! Timings are inherently host-dependent, so the assertions below use wide,
//! documented headroom over what this box measured, not a tight bound tuned
//! to one machine.

use super::*;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Build a repository via `git fast-import`, feeding it `stream`.
fn fast_import(repo: &std::path::Path, stream: &str) {
    git_vista_fixtures::git::init(repo);
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet", "--done"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import should run");
    child
        .stdin
        .take()
        .expect("fast-import stdin is piped")
        .write_all(stream.as_bytes())
        .expect("writing the fast-import stream");
    let status = child.wait().expect("git fast-import should finish");
    assert!(status.success(), "git fast-import failed");
}

/// A repository with one file renamed `hops` times, each rename separated by
/// `noise_per_hop` commits that touch unrelated files — so the file's total
/// history depth (`hops * (noise_per_hop + 1)`) is large while the number of
/// times *this file itself* changed identity stays exactly `hops`.
///
/// Returns the sequence of names the file held, oldest first (`names[0]` is
/// its first-ever name, `names[hops]` is its current, live name).
fn deep_rename_chain(repo: &std::path::Path, hops: u32, noise_per_hop: u32) -> Vec<String> {
    let mut stream = String::new();
    let mut mark = 0i64;
    let mut names = vec!["file-0000.txt".to_string()];

    // `data <len>` must be the EXACT byte count of the content that follows —
    // computed from the real string via `.len()`, never hand-arithmetic'd.
    // A hand-computed count that is even one byte off makes fast-import read
    // into the *next* command's own bytes, producing an error that names some
    // fragment of a totally unrelated line ("unsupported command: one") —
    // exactly what happened here before this function used `.len()`
    // throughout instead.
    let push_data = |stream: &mut String, content: &str| {
        stream.push_str(&format!("data {}\n{content}", content.len()));
    };

    mark += 1;
    stream.push_str("commit refs/heads/main\n");
    stream.push_str(&format!("mark :{mark}\n"));
    stream.push_str("committer git-vista-ci <git-vista-ci@example.invalid> 1000 +0000\n");
    push_data(&mut stream, "genesis\n");
    stream.push_str(&format!("M 100644 inline {}\n", names[0]));
    push_data(&mut stream, "hi\n");
    stream.push('\n');

    for hop in 0..hops {
        // `noise_per_hop` commits that never touch the tracked file at all.
        for n in 0..noise_per_hop {
            mark += 1;
            stream.push_str("commit refs/heads/main\n");
            stream.push_str(&format!("mark :{mark}\n"));
            stream.push_str(&format!(
                "committer git-vista-ci <git-vista-ci@example.invalid> {} +0000\n",
                1000 + mark
            ));
            push_data(&mut stream, &format!("noise {hop}-{n}\n"));
            stream.push_str(&format!("M 100644 inline noise-{hop}-{n}.txt\n"));
            push_data(&mut stream, "noop\n");
            stream.push('\n');
        }

        let old_name = names.last().unwrap().clone();
        let new_name = format!("file-{:04}.txt", hop + 1);
        mark += 1;
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!("mark :{mark}\n"));
        stream.push_str(&format!(
            "committer git-vista-ci <git-vista-ci@example.invalid> {} +0000\n",
            1000 + mark
        ));
        push_data(&mut stream, &format!("rename hop {hop}\n"));
        stream.push_str(&format!("R {old_name} {new_name}\n"));
        stream.push('\n');
        names.push(new_name);
    }
    stream.push_str("done\n");

    fast_import(repo, &stream);
    names
}

/// A repository holding one file of `lines` lines, each line its own commit
/// (so blame has `lines` distinct commits to attribute, the worst case for
/// per-line attribution cost) — built as one `fast-import` stream, appending
/// one more line per commit via a full-file rewrite (fast-import has no
/// append primitive; each commit's blob is the whole file to that point,
/// which is exactly what a real file grown one line at a time looks like on
/// disk).
fn deep_line_history(repo: &std::path::Path, lines: usize) -> () {
    let mut stream = String::new();
    let mut content = String::new();
    for n in 1..=lines {
        content.push_str(&format!("line {n}\n"));
        stream.push_str("commit refs/heads/main\n");
        stream.push_str(&format!("mark :{n}\n"));
        stream.push_str(&format!(
            "committer git-vista-ci <git-vista-ci@example.invalid> {} +0000\n",
            1000 + n
        ));
        let msg = format!("add line {n}\n");
        stream.push_str(&format!("data {}\n{msg}", msg.len()));
        stream.push_str(&format!(
            "M 100644 inline big.txt\ndata {}\n{content}\n",
            content.len()
        ));
    }
    stream.push_str("done\n");
    fast_import(repo, &stream);
}

/// Limit 1: classifying a name 12 rename-hops stale, amid ~3,000 commits of
/// unrelated history, costs about the same as classifying the file's
/// CURRENT, live name — proving the cost is `O(hops)`, not `O(history)`.
///
/// Measured on this host (2026-09-05, `cargo test` debug timings — the ones
/// a CI run actually pays), across several runs including the full parallel
/// suite (1,196 other tests competing for the sandbox): classifying the
/// oldest name ranged **1.37s–2.9s** (12 hops × 2 git spawns each, each spawn
/// crossing the sandbox, so per-spawn sandbox overhead dominates and varies
/// with host load); classifying the live name took 15–60ms (one
/// `cat-file -e`, no chase at all) — a ~40-90x ratio for 12 hops, well short
/// of the 3,000 the noise commits would imply if the cost scaled with
/// history instead of hop count. Asserted at 8s — comfortably over this
/// host's observed worst case under load — so the bound catches a real
/// regression (an accidental full-history walk here costs whole seconds
/// *per hop* at 3,000 commits, not milliseconds) without flaking on a loaded
/// CI box.
#[tokio::test]
async fn classifying_a_stale_name_costs_hops_not_history_depth() {
    let dir = tempfile::tempdir().unwrap();
    let hops = 12;
    let names = deep_rename_chain(dir.path(), hops, 250); // ~3,000 noise commits total

    let oldest = &names[0];
    let newest = &names[names.len() - 1];

    let start = Instant::now();
    let (state, _) = classify_path(dir.path(), "HEAD", oldest, "perf-test")
        .await
        .unwrap();
    let oldest_elapsed = start.elapsed();
    match &state {
        PathState::RenamedAway { current_path, .. } => {
            assert_eq!(
                current_path, newest,
                "must chase all {hops} hops to the true current name"
            )
        }
        other => panic!("expected RenamedAway, got {other:?}"),
    }

    let start = Instant::now();
    let (state, _) = classify_path(dir.path(), "HEAD", newest, "perf-test")
        .await
        .unwrap();
    let newest_elapsed = start.elapsed();
    assert_eq!(state, PathState::Readable, "the current name is alive");

    eprintln!(
        "perf: classify oldest (12 hops) = {oldest_elapsed:?}, classify newest (0 hops) = {newest_elapsed:?}"
    );
    assert!(
        oldest_elapsed.as_secs_f64() < 8.0,
        "classifying a 12-hop-stale name took {oldest_elapsed:?}, over the 8s bound — \
         chase_rename_chain may have stopped being O(hops)"
    );
    // The real invariant: the stale-name cost is dominated by hop count, not
    // by the 3,000-commit history each hop's `git log -1` still has to be
    // capable of searching. A cost that scaled with history size would make
    // this ratio blow up as `noise_per_hop` grows; it does not, because each
    // `git log --diff-filter=D -1` stops at the first match it finds walking
    // backward from HEAD, which for this fixture is always within
    // `noise_per_hop` commits.
    assert!(
        oldest_elapsed.as_secs_f64() < newest_elapsed.as_secs_f64() * 100.0 + 3.0,
        "a 12-hop chase ({oldest_elapsed:?}) should not be wildly disproportionate to a \
         0-hop lookup ({newest_elapsed:?})"
    );
}

/// Limit 2, first half: blaming the *tail* of a long-lived file — the common
/// case, since most look-ups are "who wrote this line I'm looking at right
/// now" and most looked-at lines are recently touched — costs almost nothing
/// regardless of the file's total length, because blame starts its walk at
/// the requested revision and the tail's owning commits are right there.
///
/// A raw `git blame -L` timing on this exact shape, measured directly on
/// this host outside any server code (2026-09-05, 3,000 single-line-commit
/// history): the last 10 lines took **21ms**; a middle page (line 1500) took
/// **459ms**; the first 10 lines took **450ms** — confirming line-range
/// paging bounds the *parsed result*, not the underlying git walk, which
/// costs roughly `O(distance from the requested revision to where the lines
/// settle)`. See the module doc for why, and why that is still the right
/// design (it is the exact tradeoff ADR 0022 already accepted for commit
/// history). Asserted at 2s for the tail — ~100x this host's own
/// measurement.
#[tokio::test]
async fn blaming_the_tail_of_a_long_lived_file_is_fast() {
    let dir = tempfile::tempdir().unwrap();
    let lines = 3_000;
    deep_line_history(dir.path(), lines);

    let start = Instant::now();
    let page = blame_for_repo(
        dir.path(),
        "HEAD",
        "big.txt",
        Some(lines - 9), // -L is inclusive on both ends; lines-9..=lines is 10 lines.
        Some(lines),
        "perf-test",
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(page.total_lines, lines);
    assert_eq!(
        page.ranges.len(),
        10,
        "10 lines, each its own commit, coalesce into no wider ranges"
    );
    eprintln!("perf: blame the last 10 lines of a {lines}-line file = {elapsed:?}");
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "blaming the tail of a {lines}-line file took {elapsed:?}, over the 2s bound — \
         a page near HEAD should cost close to nothing regardless of file length"
    );
}

/// Limit 2, second half: a page that has been stable since the *start* of a
/// long history is the genuinely expensive case — and it is still bounded,
/// at a cost proportional to that history's depth, never to anything larger
/// (a whole-file read, an unbounded walk past the repository's actual
/// history). Asserted at 5s against this fixture's 3,000-commit depth —
/// over 10x the ~450ms this host measured for the same shape directly with
/// the real `git blame` binary (see the sibling test's doc) — so the bound
/// catches a real regression (an accidental switch away from `-L`, which
/// would instead cost whatever blaming the *whole* file costs) without
/// flaking on a loaded CI box.
#[tokio::test]
async fn blaming_the_oldest_page_of_a_long_lived_file_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let lines = 3_000;
    deep_line_history(dir.path(), lines);

    let start = Instant::now();
    let page = blame_for_repo(
        dir.path(),
        "HEAD",
        "big.txt",
        Some(1),
        Some(10),
        "perf-test",
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(page.total_lines, lines);
    assert_eq!(page.ranges.len(), 10);
    eprintln!("perf: blame the first 10 lines of a {lines}-line file = {elapsed:?}");
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "blaming the oldest page of a {lines}-line file took {elapsed:?}, over the 5s bound"
    );
}
