# 0080 — A journal line may point at its batch's capture, and nothing leaves the server as a pointer

- **Status:** Accepted — implemented and tested. **Amended 2026-08-26 by ADR 0085 (#521)**: the Consequences gained the rollback section this ADR shipped without. Nothing decided here changed.
- **Date:** 2026-08-25
- **Issue:** [#485](https://github.com/tom2025b/git-vista/issues/485). A #329 successor.
- **Evidence:** `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`,
  Q2 — the measured table this ADR's numbers are the second half of.
- **Handoff:** `docs/handoffs/CLOUD-13-issue-485-journal-quadratic.md`.
- **Extends:** ADR 0070, whose `RefsAtEvent` this widens by one variant and one
  optional field. Nothing 0070 decided is reversed: the three-state honesty, the
  per-kind caps and the peeling loss all stand exactly as written.

## Context

`journal::append` captured the repository's refs itself, on every call. That was
#131's decision and a good one — no write endpoint can forget to record history
if the writer records it for them.

`fetch::journal_updates` calls the journal once per moved ref, because those
per-ref entries are the key by which `assemble_feed` suppresses git's own
per-ref reflog lines; `0a7ba777` reverted an attempt to replace them with a
summary entry and the feed went from 94 rows to 95.

Put together, a fetch of N refs performed **N full ref reads** and wrote **N
lines each embedding the whole ref set** — up to 500 branches, 500 tags and 500
remote-tracking refs apiece (`REFS_PER_EVENT_CAP`). Both costs grow with N, and
the ref set itself grows with the fetch, so the total is quadratic in the refs
moved. It is awaited before `exec_fetch` returns, so it is the user's fetch
latency, not background work.

Measured in this container, real `git init` repositories and the real writer:

| refs moved | journal bytes | bytes/line | journalling time |
| ---: | ---: | ---: | ---: |
| 1 | 407 B | 407 | 2.5 ms |
| 94 | 532.6 KiB | 5,801 | 743.0 ms |
| 500 | 14.0 MiB | **29,350** | **18,444.4 ms** |

`bytes/line` rising 407 → 29,350 is the defect, stated in one column.

## Decision

### D1 — The batch is the unit of capture; the record stays per event

One operation reads the refs **once**. `journal::append_all(repo, &events)` is
the writer; `journal::append` is a batch of one and is byte-for-byte unchanged,
so the twenty-odd endpoints that record a single event are untouched by this.

What is **not** batched is the entry. A fetch of N refs still writes N lines,
each with its own `ref_name`, `old_oid` and `new_oid`. That is the constraint
`0a7ba777` established and it is not reopened here. The economy is in the
capture, not in the record.

### D2 — A line may say "my capture is over there", and that is a fourth answer

The batch's **last** event needing a capture carries the maps, stamped with a
batch id; the others carry `RefsAtEvent::InBatch { batch }`.

This is the decision the handoff asked to be written down, so here is why it is
not simply `None` on the other N-1 lines. `None` already means *no capture was
attempted* — the pre-#131 lines, and lines written where there is no real `.git`
directory. Reusing it for "captured, stored elsewhere" would tell a replayer
that N-1 of every N lines carry no history, which is false, and would be
indistinguishable from the lines for which it is true, which makes it
uncorrectable later. ADR 0070's own rule — a value must distinguish *not
recorded* from *recorded*, or it reintroduces the defect the enum exists to
prevent — decides this the same way it decided `Option<CapturedRefs>`.

So `RefsAtEvent` gains a fourth answer:

| value | meaning | what a replay may conclude |
| --- | --- | --- |
| field absent (`None`) | no capture was attempted | nothing |
| `CaptureFailed` | attempted, could not read the refs | nothing — and must NOT infer deletions |
| `Captured` | a real observation, possibly of zero refs | the maps are the truth at that instant |
| `InBatch` | a real observation, stored on another line | resolve it, then as `Captured` |

`Captured` gains `batch: Option<String>`. `None` is the ordinary single-event
capture that anchors nothing — which is exactly what every line written before
this change is, so no line already on disk changes meaning.

### D3 — The anchor is written last, because the read is a tail window

`read_all` returns the newest `JOURNAL_READ_CAP` lines, so a window can begin in
the middle of a batch. With the anchor first, such a window holds referrers
whose capture was trimmed away. With it last, every referrer the window keeps is
followed by its anchor inside the same window.

The failure is survivable either way — an unresolvable referrer answers `None`,
*no information* — but survivable is not the same as unnecessary, and the choice
costs nothing.

A **failed** capture is copied onto every line of its batch rather than
anchored. It is a reason string, not three maps, so sharing it saves nothing,
and a batch anchored on a failure would have referrers pointing at a line with
no maps to resolve to.

### D4 — Nothing leaves the server as a pointer

`git_vista_core::activity::refs_at(event, journal)` is the only correct way to
read `ActivityEvent::refs`, and `assemble_feed` calls it before returning: a row
on the wire carries its maps, its failure, or nothing. A client has no journal
to follow a reference with, and would have no reading of `in_batch` available to
it other than "no history".

The resolution happens **after** the burst fold and the truncation, and that
ordering is load-bearing. Resolving on the way in would copy one batch's
snapshot onto every line of the batch, reinstating in memory, on every feed
read, exactly the duplication this ADR takes out of the file. After the
truncation it is copied onto at most `limit` rows, and folded rows already carry
no capture at all.

### D5 — One timestamp for the batch

Every entry of a batch is stamped with one `now_secs()` reading, because they
describe one action.

This is also a correctness fix, and a small one for a defect measured large.
`assemble_feed` attributes a journal entry to git's reflog line for the same
movement only within `JOURNAL_MATCH_SLACK`; each entry previously took its own
reading *after* its own full ref read, so entry *i* drifted further and further
from the reflog line git wrote for it. Past roughly 170 refs the later entries
stopped matching, their reflog lines survived attribution, and the fold counted
both copies — 500 refs reported as 891 (investigation, F1).

It does **not** repair the fold, which double-counts whatever drifts, from any
cause. That defect stays pinned as an expected failure in
`git_vista_core::activity`, with its comment corrected to describe the fold
rather than a driver that no longer exists.

## What a journal line carries after this change, and what reads it

A line carries what it always carried — time, kind, ref name, summary, both
tips, source — plus one of the four `refs` answers in D2's table.

**What reads it, honestly stated:** nothing yet renders a ref capture. It exists
for the #136 step-through viewer, as ADR 0070 records, and until that lands the
capture is written and never drawn. What #485 adds is that the field is no
longer safe to read directly, so the reader is now a named function rather than
a field access:

- `git_vista_core::activity::refs_at` — the resolver. Every reader goes through
  it. Reading `event.refs` directly was correct while every line carried its own
  maps; it now silently sees "no maps" on the N-1 referrer lines of a batch.
- `git_vista_core::activity::assemble_feed`, step 7 — the one production caller
  today (D4). It is what makes `/api/activity`'s payload self-contained.
- `journal::append` / `append_all` — the writers, and the only code that mints a
  batch id.

## Consequences

Measured on the same fixtures, both writers, after the change:

| refs moved | writer | journal bytes | bytes/line | journalling time |
| ---: | --- | ---: | ---: | ---: |
| 1 | per-event (pre-#485) | 407 B | 407 | 2.5 ms |
| 1 | `append_all` | 407 B | 407 | 2.0 ms |
| 94 | per-event (pre-#485) | 532.6 KiB | 5,801 | 743.0 ms |
| 94 | `append_all` | 20.8 KiB | 226 | 9.9 ms |
| 500 | per-event (pre-#485) | 14.0 MiB | 29,350 | 18,444.4 ms |
| 500 | `append_all` | **110.0 KiB** | **225** | **44.5 ms** |

`bytes/line` is flat — 407 at one ref, 225 at five hundred, and *lower* at scale
because 499 of the 500 lines carry no maps at all. A 500-ref fetch's journalling
is 22× a single ref's, not 7,400×.

The read path follows: `JOURNAL_READ_CAP` lines of a 500-ref repository were
34.7 MB and ~1.2 s per feed read when every line embedded a snapshot. The lines
are now flat, so both fall with them. That is a consequence of this change, not
a claim measured here.

**#487 (push journals per remote-tracking ref) is not made moot.**
`push::journal_updates` has the same per-ref shape and has not been changed —
it is outside this task's paths. The mechanism it needs now exists
(`journal::append_all`, `handlers::journal_app_events`), so adopting it is a
small change rather than a design question, but until it is adopted a push that
moves many refs still pays what a fetch used to.

### Consequences — rollback (added 2026-08-26, #521, ADR 0085)

**This ADR shipped without weighing what a rollback across it costs, and it
costs something.** Recorded here, at the decision, rather than left to be
rediscovered as a bug.

`RefsAtEvent` is internally tagged and, as this ADR left it, had no catch-all
variant. A binary built before #485 therefore fails to deserialize
`{"status":"in_batch",…}` with ``unknown variant `in_batch` ``. That failure is
not confined to the field: `refs` is one field of an `ActivityEvent`, and
`journal::read_all` parses a whole line at a time, so **the entire event is
discarded** — its time, kind, ref name, summary and both tips along with the
capture. Roll a deployment back across #485 and a 100-ref fetch renders as
**one** feed row: the anchor survives and the other 99 vanish, for as long as
the old binary is the one reading.

What is *not* lost: the bytes. The journal is append-only and nothing rewrites
or truncates it, so a re-upgrade restores every line. An old binary appending
its own per-event lines to the same file is also fine — mixed-format journals
are normal. This is a persisted-format break and a rendering loss, not data
loss.

D2's reasoning is not reopened by this. Writing the referrers as `None` would
have kept them readable by an old binary at the price of telling every current
and future replayer that N−1 of every N lines carry no history — a permanent
lie bought for a temporary window, and #521 re-examined and re-rejected it.

**What #521 changed, and what it did not.** ADR 0085 adds a `RefsAtEvent`
catch-all and a per-line journal format stamp, so that the *next* variant costs
a line its capture rather than the line, and so that a reader can name the
format that wrote a line it cannot fully read. Neither does anything for a
rollback across *this* ADR: old readers are binaries already built. The N−1
loss above stands exactly as stated.

## Alternatives rejected

**Clone the one capture onto every line.** Removes the N ref reads and none of
the bytes: the journal is still 14 MB at 500 refs and `bytes/line` is still
29,350. It fixes the half of the issue that is easy to measure and leaves the
half named first in the acceptance criteria.

**Store captures in a side file, one per batch.** Genuinely flat lines, and it
avoids the trimmed-window question entirely — but it adds an unbounded second
artefact with its own lifecycle, and nothing prunes it. Keeping the snapshot in
the journal means the read cap prunes it for free.

**Point at the anchor by `(time, ref_name)` instead of a minted id.** Needs no
new field on `Captured`, which was its whole appeal. Rejected: two fetches of
one remote in the same second give two lines the same key, and "essentially the
same instant" is not a property to build a replay on.
