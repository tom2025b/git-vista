# #329, the fetch flood: what is actually left of it

**Date:** 2026-08-25 · **By:** max · **Issue:** [#329](https://github.com/tom2025b/git-vista/issues/329)
**Shape:** investigation. No behaviour was changed; two doc comments were corrected.

---

## Verdict

**#329's reported symptom is fixed, and I verified it rather than taking the
commit message's word for it.** A fetch that updates 94 remote-tracking refs
produces **one** feed row.

**But #329 should not simply be closed as "fixed".** Verifying the fold turned
up three defects in the same mechanism, one of which makes the fold print a
number that is not true. They are listed as F1–F3 below, with the evidence.

The reproduction throughout is **synthetic**. Tom's own repository journal holds
4 lines; the 94-line state came from a device pass whose `origin` had been
pointed at a large public repository, and no longer exists. A fixture is the only
honest way to reach this, so a fixture is what was used.

---

## Q1 — Does the fold actually cover the reported case?

`fold_ref_update_bursts` (`crates/git-vista-core/src/activity.rs:610`), called
from `assemble_feed` at `activity.rs:543`, collapses a run of `Fetch`/`Pull`
remote-tracking ref updates chained within `FETCH_BURST_GAP`
(`activity.rs:422`, 5s) into one counted row.

Driven over fixtures shaped exactly like one `git fetch` — the app journals one
entry per ref it watched change *and* git writes one reflog line per ref:

| Case | Rows | Result |
| --- | --- | --- |
| 94 refs, journal + reflog, same second | **1** — `fetch — 94 refs updated` | ✅ the reported case |
| 1 ref | 1 — `fetched ‘origin/main’ from origin` | ✅ own words kept |
| fetch at t=100, pull at t=101 | 3 — pull branch move, `pull — 3 refs updated`, `fetch — 3 refs updated` | ✅ grouped separately, branch move survives |
| per-ref entries 6s apart | 6 — unfolded | see F3 |
| unobserved fetch, 4 refs moved | 1 — `fetch — 5 refs updated` | ❌ **F2** |
| 250 refs, realistic journal drift | 1 — `fetch — 297 refs updated` | ❌ **F1** |

So the headline holds: **one fetch, one row, at the reported scale.** The fold
is well-placed — a terminal fetch has reflog lines and no journal entry at all,
which no write-path operation id could ever group, and the fold covers both
sources with one rule.

---

## Q2 — Is the remaining journal volume a defect in its own right?

**Yes, decisively, and it is worse than "the file is a bit bigger".**

### The mechanism

`fetch::journal_updates` (`crates/git-vista-server/src/planner/fetch.rs:462`)
loops over the updated refs, calling `journal_app_event` once per ref. That
reaches `journal::append` (`journal.rs:124`), which — because the event arrives
with `refs: None` (`handlers/mod.rs:78`) — calls `capture_refs`
(`journal.rs:96`) on **every iteration** (`journal.rs:129`).

`capture_refs` performs a **full ref read of the repository** and embeds up to
`REFS_PER_EVENT_CAP` (500, `activity.rs:260`) branches, 500 tags **and 500
remote-tracking refs** into that one line.

So a fetch of N refs performs N full ref reads and writes N lines, each of which
embeds a snapshot that itself grows with N. The journal cost is **quadratic in
the number of refs the fetch moved.**

### Measured — one fetch, by refs updated

Container: this cloud session. Real `git init` repos, real `journal::append`.

| refs moved | lines | journal bytes | bytes/line | journalling time | ms/ref | `read_all` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 538 B | 537 | 2.4 ms | 2.37 | 0.2 ms |
| 10 | 10 | 10.1 KiB | 1,032 | 21.9 ms | 2.19 | 0.5 ms |
| **94** | **94** | **526.8 KiB** | **5,736** | **1,086.7 ms** | 11.56 | 24.1 ms |
| 250 | 250 | 3.5 MiB | 14,622 | 7,408.6 ms | 29.63 | 184.4 ms |
| 500 | 500 | 14.1 MiB | 28,872 | 27,606.2 ms | 55.21 | 645.5 ms |

**The issue's own 94-ref fetch writes 527 KiB and spends 1.1 s journalling.** At
500 refs one fetch writes **14 MiB** and spends **27.6 seconds** journalling.

That time is not background work. `journal_updates(...).await` at
`fetch.rs:333` completes before the executor returns `FetchStep::Completed`, so
it is added directly to the user's fetch latency.

### Measured — the read path at its cap

`JOURNAL_READ_CAP` is 1,000 lines (`journal.rs:33`). 1,200 lines accumulated:

| repo's ref count | journal on disk | `read_all` | `assemble_feed` |
| ---: | ---: | ---: | ---: |
| 10 | 1.2 MB | 44.5 ms | 6.7 ms |
| 94 | 6.9 MB | 231.1 ms | 52.9 ms |
| 500 | 34.7 MB | 1,228.8 ms | 285.2 ms |

**Every feed read costs ~1.5 s in a 500-ref repository**, and the journal
reaches ~35 MB. Yesterday's three performance fixes (24 August) made this read
bounded, linear and single-copy — they did exactly what they claimed, and they
are not the problem. The problem is what each line *contains*.

### F1 — the fold prints a count that is not true

This is where Q2's cost becomes a Q1 correctness bug, which is why the two
questions cannot be answered separately.

Step 3 of `assemble_feed` suppresses a reflog line only when a journal entry
matches it within `JOURNAL_MATCH_SLACK` (`activity.rs:405`, 5s). Git writes
every reflog line at fetch time T. The app stamps journal entry *i* at
roughly T + i × (the per-ref cost measured above). Once that exceeds 5 s, the
later refs stop deduping, their reflog lines survive, and the fold counts both
copies.

Driven with the measured per-ref costs:

| refs really moved | ms/ref | folded row |
| ---: | ---: | --- |
| 94 | 11.56 | `fetch — 94 refs updated` ✅ |
| 250 | 29.63 | `fetch — **297** refs updated` ❌ (+47) |
| 500 | 55.21 | `fetch — **891** refs updated` ❌ (+391) |

The feed stays at one row — #329's symptom remains fixed — but the number it
shows becomes fiction, and it is the journal's own write cost that causes it.
The threshold is roughly N × cost > 5 s, i.e. somewhere near 170–250 refs. **The
94-ref case in the issue sits just under it.** The fix that made the feed
readable is quietly correct only because the reported repository was small
enough.

---

## F2 — the "tips unknown" admission is erased whenever refs actually moved

`fold_ref_update_bursts`'s doc comment claims (`activity.rs:599`–`604`):

> That is also what keeps the "tips unknown — git could not be read" entry
> intact: it is journaled *instead of* per-ref entries, never alongside them, so
> it is always a run of one.

**That reasoning is wrong, and wrong in exactly the way the first #329 attempt
was wrong: it accounts for the journal and forgets git's reflog.**

`journal_unobserved` (`fetch.rs:511`) fires when `git fetch` *succeeded* but the
re-read of `refs/remotes/<remote>/*` failed. If the fetch succeeded it very
likely moved refs — and git wrote a reflog line for each one. The admission
entry carries `new_oid: None`, so it matches no reflog line in step 3 and
suppresses nothing. All the reflog lines survive, the admission joins them, and
the whole run folds.

Measured, 4 refs moved:

```
journal:  1 × "fetched from ‘origin’ … (tips unknown — git could not be read)"
reflog:   4 × "fetch origin: fast-forward"
feed:     1 × "fetch — 5 refs updated"
```

Two harms at once. D5's deliberate admission — the distinction between "there
was no such tip" and "we could not read the tip" — is **replaced by a
confident-sounding count**. And that count is wrong: 4 refs moved, not 5; the
admission entry is itself counted as a ref.

The doc comment's claim holds only when the fetch moved nothing at all, which is
the one case where there are no reflog lines. Verified: with no reflog lines the
admission does survive as a run of one.

---

## F3 — a straddling burst unfolds (noted, not a defect)

Per-ref entries more than `FETCH_BURST_GAP` apart do not chain, and the feed
falls back to one row per ref. For **reflog** lines this is unreachable in
practice: git writes them together during one ref-update phase. It is reachable
through journal drift — but drift produces F1's inflated count rather than a
split, because the journal entries are dense (tens of ms apart) even when the
run as a whole is long.

Recorded so the next reader does not have to rediscover that it was considered.
It is the fold degrading to the old behaviour, not misreporting, and it needs no
fix on its own — F1's fix removes the drift that could reach it.

---

## Q3 — Does anything else write per-item events for one user action?

`journal::append` has exactly **two** production call sites, so this is a bounded
read rather than a survey:

1. `handlers/mod.rs:78` (`journal_app_event`), shadowed for the executors by the
   blocking version at `planner.rs:1055`
2. `crates/git-vista-server/src/activity.rs:89`, the synthesized branch deletion

Every `journal_app_event` call site was read. **Exactly two of them are loops:**

| Site | Kind | Folded? |
| --- | --- | --- |
| `planner/fetch.rs:462` `journal_updates` | `Fetch` | ✅ yes — this is #329 |
| `planner/push.rs:684` `journal_updates` (loop at `:693`) | `Push` | ❌ **no** |

Everything else — `remote_tags.rs`, `branch_exec.rs`, `tag_exec.rs`,
`sequence_exec.rs`, `worktree_exec.rs`, `commit_exec.rs` — journals once per
operation from a `match` arm or an `if`, not from a loop.

### The second shape: push

`push::journal_updates` has the same per-ref structure, and `Push` is **not** a
fold candidate — `fold_ref_update_bursts` matches only
`ActivityKind::Fetch | ActivityKind::Pull`. Verified: a push moving 4
remote-tracking refs produces 4 unfolded rows.

**Severity is low, and worth saying so plainly rather than inflating it.**
git-vista's push endpoint pushes one named branch, so `updated` is normally a
single ref; a push does not update the rest of `refs/remotes/*` the way a fetch
does. The per-ref `capture_refs` cost applies identically, but at N=1 it costs
one ref read. This is a latent shape that matches the fetch flood, not a flood
anyone is currently hitting.

### The loop that is correct

`activity.rs:87`–`89` journals one `BranchDeleted` per branch found missing
against the snapshot. That is N *distinct* user actions noticed at once, not one
action — and folding it would be actively wrong, since `BranchDeleted`'s
`old_oid` is precisely what its undo needs. Left alone.

---

## Undo safety — re-verified, not assumed

#329 asked to confirm nothing in the undo path depends on per-ref `Fetch`
entries. `undo_hint` has no arm for `Fetch` or `Pull`, so neither row has ever
carried a hint and dropping the per-ref oids cannot take one away. This holds
for the fold as it stands and for every fix proposed below. It would **not**
hold for `BranchDeleted`.

---

## Recommendation

**Do not close #329 on the strength of its symptom being fixed.** Close it only
alongside successors for F1 and F2, which are defects in the fix itself:

- **F1 (high)** — per-ref `capture_refs` makes journalling quadratic (14 MiB and
  27.6 s for one 500-ref fetch, blocking the response), and the resulting
  timestamp drift makes the folded count fabricate refs that did not move.
  The natural fix is to capture the ref map **once per operation** and pass it
  to each entry — `append` already honours an event that arrives carrying its
  own `refs` (`journal.rs:129`), so the seam exists. That is a write-path
  change, and it does **not** re-litigate the reverted attempt: it keeps one
  journal entry per ref, so the dedup key that the revert proved load-bearing
  stays exactly where it is.
- **F2 (medium-high)** — exclude the unobserved-fetch admission from folding.
  It is identifiable without heuristics: `ref_name: None` together with both
  oids `None` is the shape `journal_unobserved` alone produces.
- **F3** — no action; folded into F1's fix.
- **Push (low)** — file the shape so it is on record before someone adds a
  multi-ref push path.

**What must not happen** is a third attempt that moves the journalling to a
summary entry. That is what `0a7ba777` reverted, and the reason still stands:
the per-ref journal entries are the dedup key that suppresses git's own reflog
lines, and removing them takes the feed from 94 rows to 95.

---

## Method, stated plainly

- **Synthetic.** Fixtures, not a live repository. There is no live server in a
  cloud session and no 94-line journal left anywhere.
- **Measured, not reasoned.** Every number above came from running code in this
  container: real `git init` repositories, the real `journal::append` and
  `read_all`, and the real `assemble_feed`.
- **Timings are single-run on shared cloud hardware.** Treat the ratios and the
  orders of magnitude as the finding; the absolute milliseconds will differ on
  Tom's box. The quadratic *shape* — bytes/line rising 537 → 28,872 as refs rise
  1 → 500 — is not a timing artefact.
- **No test was added.** The tests that would pin F1 and F2 are red against
  current `main`, and a red test does not belong in a green gate. The fixtures
  are reproduced below so whoever fixes them starts from the reproduction rather
  than rebuilding it.

## Reproduction

Drop into `crates/git-vista-core/tests/` and run with `--nocapture`.

```rust
use git_vista_core::activity::{
    assemble_feed, ActivityEvent, ActivityKind, ActivitySource, ReflogEntry,
};
use std::collections::{HashMap, HashSet};

fn reflog(r: &str, time: i64, old: &str, new: &str, msg: &str) -> ReflogEntry {
    ReflogEntry { ref_name: r.into(), time, old_oid: old.into(), new_oid: new.into(), message: msg.into() }
}
fn jev(kind: ActivityKind, r: Option<&str>, time: i64, old: Option<&str>, new: Option<&str>, s: &str) -> ActivityEvent {
    ActivityEvent {
        time, kind, ref_name: r.map(str::to_string), summary: s.into(),
        old_oid: old.map(str::to_string), new_oid: new.map(str::to_string),
        source: ActivitySource::App, undo: None, refs: None,
    }
}

/// F2: the admission is replaced by a fabricated count.
#[test]
fn f2_unobserved_fetch_admission_is_erased() {
    let rl: Vec<ReflogEntry> = ["main", "dev", "topic", "release"].iter()
        .map(|r| reflog(&format!("origin/{r}"), 100, "o", &format!("n{r}"), "fetch origin: fast-forward"))
        .collect();
    let journal = vec![jev(ActivityKind::Fetch, None, 100, None, None,
        "fetched from ‘origin’ … (tips unknown — git could not be read)")];
    let feed = assemble_feed(journal, rl, &HashMap::new(), &HashSet::new(), 200);
    // Observed on main: 1 row, "fetch — 5 refs updated". The admission is gone
    // and 4 refs moved, not 5.
    println!("{:#?}", feed);
}

/// F1: the count inflates once journalling drift exceeds JOURNAL_MATCH_SLACK.
/// ms/ref values are the measured cost of per-ref `capture_refs`.
#[test]
fn f1_folded_count_inflates_at_scale() {
    for (n, ms_per_ref) in [(94usize, 11.56f64), (250, 29.63), (500, 55.21)] {
        let rl: Vec<ReflogEntry> = (0..n).map(|i| reflog(
            &format!("origin/b{i}"), 100, &format!("o{i}"), &format!("n{i}"),
            "fetch origin: fast-forward")).collect();
        let journal: Vec<ActivityEvent> = (0..n).map(|i| jev(
            ActivityKind::Fetch, Some(&format!("refs/remotes/origin/b{i}")),
            100 + (i as f64 * ms_per_ref / 1000.0) as i64,
            Some(&format!("o{i}")), Some(&format!("n{i}")),
            &format!("fetched ‘origin/b{i}’ from origin"))).collect();
        let feed = assemble_feed(journal, rl, &HashMap::new(), &HashSet::new(), 500);
        // Observed on main: 94 -> "94 refs updated"; 250 -> "297"; 500 -> "891".
        println!("n={n}: {} row(s) -> {}", feed.len(), feed[0].summary);
    }
}
```

The Q2 cost table was produced by appending N `Fetch` events through the real
`journal::append` into a `git init` repository holding N `refs/remotes/origin/*`
refs, from a `#[cfg(test)]` module inside `git-vista-server` (the crate is
binary-only, so `journal` is not reachable from an external test).
