# 0085 — A journal line is a versioned envelope, and "I cannot read this capture" is an answer

- **Status:** Accepted — implemented and tested; browser leg unrun
- **Date:** 2026-08-26
- **Issue:** [#521](https://github.com/tom2025b/git-vista/issues/521). A #485 successor.
- **Handoff:** `docs/handoffs/2026-08-26/CLOUD-2-issue-521-journal-rollback.md`.
- **Extends:** ADR 0080, whose `RefsAtEvent` this widens by one variant and whose
  Consequences this amends with the rollback cost it never weighed. Nothing 0080
  decided is reversed: the batch is still the unit of capture, the anchor is
  still written last, and nothing still leaves the server as a pointer.
- **Extends:** ADR 0070, whose rule — a value must distinguish *not recorded*
  from *recorded* — is what decides the shape of the new variant below.

## Context

### The defect, and its exact size

ADR 0080 (#485) gave `RefsAtEvent` a fourth variant, `InBatch { batch }`, so that
a fetch of N refs stores one ref snapshot instead of N. On disk that is a line
whose capture reads `{"status":"in_batch","batch":"…"}`.

`RefsAtEvent` is an internally tagged enum (`#[serde(tag = "status")]`) and,
before this ADR, it had no catch-all variant. A binary built before #485
therefore **cannot deserialize that value at all**:

```
unknown variant `in_batch`, expected `captured` or `capture_failed`
```

The failure is not confined to the field. `refs` is one field of
`ActivityEvent`, and `journal::read_all` parses a whole line at a time:

```rust
.filter_map(|l| match serde_json::from_str::<ActivityEvent>(l) {
    Ok(event) => Some(event),
    Err(e) => { eprintln!("git-vista: skipping an unreadable journal line: {e}"); None }
})
```

So one unreadable field discards the **entire event** — its time, kind, ref
name, summary and both tips along with the capture it could not read. Roll a
deployment back across #485 and a 100-ref fetch renders as **one** feed row
instead of 100: the anchor survives, the other 99 vanish.

This is precise about what is lost and what is not:

| | |
| --- | --- |
| **Lost** | the *rendering* of N−1 of every N lines of a batch, for as long as the old binary is the one reading |
| **Not lost** | the bytes. The journal is append-only; nothing rewrites or truncates it. Re-upgrade and every line returns |
| **Not corrupted** | an old binary appending its own per-event lines to the same file is fine — mixed-format files are normal and always have been |

It is a persisted-format break, not data loss. ADR 0080 weighed neither, which
is the omission #521 is about.

### The thing that makes it worse than it looks

The old reader cannot even *say* what happened. Its diagnostic is a serde
message about a variant name; there is nothing in the file that tells it "this
line was written by a newer format than you". An operator reading a suddenly
thin feed has a serde error and a guess.

### What nothing can fix

**Old readers are shipped binaries.** Nothing in this repository changes what a
binary built in July does with a file written in August. Every option below is
therefore evaluated on two separate questions, which are easy to blur and are
not the same question:

- **the past** — what does it do for a rollback across #485, today?
- **the future** — what does it do for the *next* format change?

## Decision

### D1 — Accept the #485 rollback cost, and write it down verbatim

**Nothing is added to make a pre-#485 binary read an `in_batch` line.** The
alternatives that would (§Alternatives, first two) all cost the current format
something real and buy the past nothing that a re-upgrade does not already buy.

What ships instead is honesty at the point of the original decision: ADR 0080
gains a `Consequences — rollback` section stating the N−1 loss in the same terms
as above, so the next person weighing a `RefsAtEvent` variant reads the cost
where the decision lives rather than rediscovering it as a bug.

### D2 — A journal line is a versioned envelope, and the version is per line

New lines carry a format stamp:

```json
{"v":1,"time":1756233600,"kind":"Fetch",…}
```

`JOURNAL_FORMAT_VERSION` is **1**. Version 1 is the format as of this ADR: one
JSON object per line, the object being an `ActivityEvent`'s own fields plus this
stamp, its `refs` carrying one of the five answers in D3.

Three properties, each load-bearing:

**Per line, not per file.** A file-level header is unreadable *by construction*
here: `read_all` returns the newest `JOURNAL_READ_CAP` lines via `tail_window`,
which seeks from the **end** of the file. A header sits at the start and is
therefore in the read window only for a journal small enough not to need one.
Any format marker in this journal has to be on the line or it does not exist.

**Absent is not zero.** `v` is `Option<u32>`. Absent means *this line predates
the stamp* — which is every line on disk today — and is not the same claim as a
stamped `0`. That is ADR 0070's rule applied to the envelope instead of to the
capture, and it is the reason the field is optional rather than
`#[serde(default)]`-to-zero.

**The stamp lives on the line, not on the event.** `ActivityEvent` is *also* the
wire DTO of `/api/activity`. A journal format version is a fact about a file on
disk; it has no meaning to a browser, which has its own version negotiation in
`git-vista-protocol`. So the stamp is a server-side envelope
(`journal::WrittenLine` and `journal::ReadLine`, each `v` plus
`#[serde(flatten)]` over the event) and `ActivityEvent` is untouched. **The line is not the event; the line is a
versioned envelope around one.** That is the structural change, and it is what
makes a future format change expressible at all.

**The stamp is invisible to every existing reader.** `ActivityEvent` does not
set `deny_unknown_fields`, so `serde_json::from_str::<ActivityEvent>` — which is
literally the current reader's line of code, and also a pre-#485 binary's —
ignores `"v":1` and parses the rest exactly as before. This is pinned by a test
that deserializes a stamped line through the bare `ActivityEvent` path rather
than through the envelope, because that is the code whose behaviour is being
claimed.

### D3 — An unreadable capture is a fifth answer, not a dropped line

`RefsAtEvent` gains `Unknown`, a `#[serde(other)]` catch-all:

| value | meaning | what a replay may conclude |
| --- | --- | --- |
| field absent (`None`) | no capture was attempted | nothing |
| `CaptureFailed` | attempted, could not read the refs | nothing — and must NOT infer deletions |
| `Captured` | a real observation, possibly of zero refs | the maps are the truth at that instant |
| `InBatch` | a real observation, stored on another line | resolve it, then as `Captured` |
| `Unknown` | **a capture is recorded here in a shape this binary has no reading for** | nothing |

This is the change that means a future `RefsAtEvent` variant costs a line its
**capture** instead of costing the journal the **whole line**. It is the direct
answer to the defect's mechanism: the enum, not the event, is where the format
grows, and the enum can now absorb growth.

`Unknown` and `None` both yield "conclude nothing", and are still kept apart on
disk, for ADR 0070's reason: they are different claims about what was recorded,
and collapsing them at rest would make the difference uncorrectable later.

**In memory and on the wire they do collapse, deliberately.**
`activity::refs_at` — the resolver ADR 0080 D4 made the only correct way to read
`ActivityEvent::refs` — answers `None` for `Unknown`, exactly as it already
answers `None` for a referrer whose anchor is not in the window. `assemble_feed`
step 7 routes `Unknown` through it alongside `InBatch`. So **the wire still
carries maps, a failure, or nothing** — the three answers of ADR 0080 D4,
unchanged, and no protocol version moves.

### D4 — The read path reports what it could not read, instead of guessing

`read_all`'s window parse is split out as a pure function returning a report
beside the events, and `read_all` prints the report. Three things get counted:

- lines skipped because they would not parse at all (the pre-existing loud skip,
  now counted rather than only logged one by one);
- lines stamped **newer** than this binary writes, with the highest version seen;
- events whose capture came back `Unknown`.

A line stamped newer than `JOURNAL_FORMAT_VERSION` is **read, not refused**. The
format has grown additively so far — every field added since #131 is optional,
by ADR 0070's rule, and D3 makes the one enum tolerant — so a newer line is
mostly readable, and refusing it would discard data the reader can still use in
order to be principled about data it cannot. That rule is a review convention,
not something the compiler enforces; a future *required* field would still cost
an old reader the line, and the stamp is then exactly what tells it why. It says
so instead:

```
git-vista: 99 journal line(s) were written by journal format v2; this binary
writes v1. They were read as far as this binary understands them — a field or
ref capture it has no reading for is treated as "not recorded", never as
"nothing was there".
```

with a companion for the capture itself, which can also arrive on an unstamped
line — a corrupt or hand-edited `status`:

```
git-vista: 1 journal line(s) carry a ref capture in a shape this binary cannot
read; those events are shown with no capture at all, which is not a claim that
the repository had no refs.
```

The first sentence is the whole benefit of D2, stated as the thing it produces. The
report is a value with its own tests rather than a `eprintln!` asserted through
captured stderr — ADR 0082's lesson, that a mechanism which "should have run" is
worth nothing unless something exercises it.

## What this buys, split honestly between the past and the future

| | rollback across **#485** (today) | rollback across the **next** format change |
| --- | --- | --- |
| before this ADR | N−1 lines of every batch vanish; the reader cannot say why | the same, again, for whatever variant is added next |
| after this ADR | **unchanged — N−1 lines still vanish** | the line survives; only the part the reader cannot read is dropped, and the reader names the version that wrote it |

The left-hand column not moving is the point, and is stated rather than
softened. **D2 and D3 do nothing whatsoever for a binary that is already
built.** Their entire value is that from this commit forward, every binary
carries a tolerant reader and every line carries its provenance, so the *next*
`RefsAtEvent` variant — the #136 step-through viewer is the likely author of one
— is a capture-sized loss on rollback instead of a line-sized one, and is
diagnosable at read time instead of guessed at.

This is the same lesson #509 is teaching the durable operation store, and the
M5-family reviews flagged both.

## Consequences

**A journal line grows by six bytes.** `"v":1,` on every line `append_all`
writes. Against ADR 0080's measured 225 bytes/line at 500 refs that is 2.7%, and
against a single event's 407 bytes 1.5%. The read cap bounds the total either
way.

**ADR 0080 D1's "byte-for-byte unchanged" claim about `journal::append` is no
longer true, and its doc comment is corrected in this change.** A single-event
line is now a v1 line. What D1 actually promised — that the twenty-odd
single-event endpoints need no code change — still holds: `append` is still
`append_all` with one event.

**The `refs` field of a journal line has one more way to be read as "nothing".**
A corrupt or hand-edited `status` that used to kill the line now yields
`Unknown` and a quieter file. The report in D4 is the compensation: the count is
surfaced on every read rather than the line disappearing with a message.

**Nothing renders a ref capture yet.** As ADR 0070 and 0080 both record, the
capture exists for the #136 step-through viewer. This ADR does not change that,
and the honest statement of what D3 protects is "the next reader of a thing
nothing reads today".

## Alternatives rejected

**Make the referrer readable by an old binary — write `refs: null` on the N−1
lines.** An old reader then keeps all N lines. Rejected on ADR 0080 D2's own
grounds, which #521 does not reopen: `None` means *no capture was attempted*,
and using it for "captured, stored elsewhere" tells every current and future
replayer that N−1 of every N lines carry no history. It buys a rollback window
by making the format permanently lie, and the lie is indistinguishable from the
pre-#131 lines it would be mixed with.

**Write both — the maps on every line *and* the batch pointer.** Old readers
read the maps, new readers follow the pointer. This is #485 undone: the 14 MiB
journal and the 29,350 bytes/line come straight back, which was the entire
defect #485 existed to fix. Paying it permanently to protect a rollback that
costs rendering only, and that a re-upgrade fully reverses, is not a trade worth
making.

**Refuse to read a line stamped newer than this binary writes.** Principled and
worse: it converts "some fields of this line are unreadable" into "this line is
gone", which is the #521 defect with a version number attached. Reading as far
as understood and reporting the gap keeps strictly more information.

**A file-level format header.** Cheaper per line, and unreadable here — `read_all`
seeks from the end of the file, so the header is outside the read window of
exactly the journals big enough to matter (D2).

**Put `v` on `ActivityEvent`.** One fewer type. Rejected: it puts a fact about a
file on disk into the DTO that goes to the browser, where it means nothing and
would have to be stripped by the same server that stamped it (D2).
