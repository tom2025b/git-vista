# 0070 — A ref capture says which kinds it recorded, and stays silent about the rest

- **Status:** Accepted — implemented and tested
- **Date:** 2026-08-23
- **Issue:** [#449](https://github.com/tom2025b/git-vista/issues/449). Blocks the reduced
  #136 step-through viewer, which replays from exactly this data.
- **Design:** `design-docs/handoffs/CLOUD-1-issue-449-capture-refs.md` on branch
  `claude/capture-refs-design-2fj9i6` (decisions D1–D7, with two executed probes).
  This ADR records D2, D4 and D5 — the three a later reader would otherwise
  re-litigate — plus the one place implementation departed from the design.
- **Supersedes:** nothing. Extends #131's per-event snapshot, whose storage format
  (`RefsAtEvent`) is kept and widened, not replaced.

## Context

#131 added a per-event ref snapshot to the journal so that, in its own doc comment's
words, "a future time scrubber can replay history losslessly". What it recorded was
local branches and nothing else: `capture_refs` filtered the ref read to
`RefKind::Branch`, discarding three of `RefKind`'s four members. So every journal line
carried **no HEAD** — the one thing "watch the HEAD move" needs — **no tags**, and
**no remote-tracking refs**, hence no ahead/behind story over time.

The journal is **append-only**: replayable history exists only from the day capture
starts capturing the right things, so every day the recording stays narrow is a day the
scrubber cannot show. The usual counter-argument — that changing the format breaks
history already on disk — does not apply. #449's census found the only real journal on
the box holding four events, all predating #131, `refs` absent in all: the machinery had
never recorded a real capture outside tests.

## Decision

### D2 — The three-state honesty is a property of every field, not just the enum

`RefsAtEvent` exists so a replayer can tell "no capture attempted" from "capture failed"
from "a real observation, possibly of nothing". The governing rule this ADR adds:

> A new field must distinguish **not recorded** from **recorded as empty**, or it
> reintroduces at the field level exactly the defect `RefsAtEvent` prevents at the
> record level.

Declaring `tags` a bare `BTreeMap` with `#[serde(default)]` would make every journal line
written before #449 deserialize as a confident observation that the repository had zero
tags — a claim never made, produced by the least informative line available. Hence
`Option<CapturedRefs>` and `Option<HeadAtEvent>`: `None` means *this line predates the
field*, and a replayer must conclude nothing from it.

In practice the rule is enforced by the type, not by discipline: making the fields bare
maps **does not compile**, because every construction site would then have to claim an
observation it does not have.

`branches` and `truncated_at` keep their names, positions and meanings exactly.
Everything new is an additional optional field. The resulting shape is asymmetric —
`branches` is a bare map with a sibling `truncated_at` while `tags` and `remotes` are
`CapturedRefs` — and that asymmetry is the price of not rewriting the meaning of lines
already on disk. Uniformity was available only by breaking them.

### D4 — Tags are recorded peeled, and the loss is recorded too

Every entry is peeled to the commit it ultimately points at, because that is what the
graph badges and what #136 replays. The loss, stated so nobody rediscovers it as a bug:
**in the capture a lightweight tag and an annotated tag on the same commit are
indistinguishable, and the tag object's own id is not recoverable.** A replay can show
*that* `v1.0` pointed at commit X; it cannot show the tag's message or tagger. If a
viewer ever needs that, it is a follow-up that adds a field — not a reason to store
unpeeled ids now and make every consumer peel.

### D5 — Remote-tracking refs are captured, in their own map

#449 required this decided deliberately either way. **Decision: capture them.**

- The story #136 exists to tell is largely *divergence* — "your branch moved, origin did
  not". Local branches alone cannot tell it, and the journal is append-only, so the half
  not captured today is unrecoverable tomorrow.
- The graph already badges them, so a replay that omits them redraws a repository the
  user never saw.
- `read_refs` already skips `refs/remotes/<remote>/HEAD`, so the remote's symbolic
  default-branch pointer stays out for free.

They go in their **own** map rather than merged with branches: a fork of a busy upstream
can hold hundreds, and under one shared cap they would evict the local branches — the
data of record — to make room for refs that change rarely. Separate maps also remove
short-name collisions between a branch and a tag of the same name.

### D6 — One cap, applied per map

`REFS_PER_EVENT_CAP` stays 500 and is applied independently to each map, each carrying
its own `truncated_at`. Branches keep exactly the guarantee they had under #131.

Worst case: 1500 entries ≈ **90 KB for one event**. Typical repo (10 branches, 30 tags,
20 remotes) ≈ **3.5 KB**, against roughly 1 KB today. The cap is a bound, not a forecast.

## Alternatives considered

**Replace `branches` with one uniform `refs: BTreeMap<full_ref_name, oid>`.** Genuinely
attractive: one map, no kind asymmetry, and full names so `v1.0`-the-tag and
`v1.0`-the-branch cannot collide. **Rejected because the journal is append-only.** Every
line already on disk has `branches`; under a rename they either vanish or need a
compatibility alias — and an alias means the "uniform" shape has a second spelling
anyway. It also destroys the free by-kind partition a replayer wants, and has no place
at all for HEAD's symbolic-ness.

**Bare maps with `#[serde(default)]` instead of `Option<CapturedRefs>`.** Less nesting,
less noise in the JSON. **Rejected because it lies about every pre-#449 line** — D2.

**Two `Option<String>`s for HEAD instead of an enum.** Simplest possible diff.
**Rejected under ADR 0068**: it is the deletion-flags shape, where the four combinations
are implicit and a consumer is free to render a fact the data never carried. The
both-absent combination is precisely the one such a consumer would mis-render.

**Skip remote-tracking refs; capture HEAD and tags only.** Tempting on size.
**Rejected because it fails the same test #449 applies to the status quo:** "add it
later" means the divergence story is missing from exactly the history the scrubber will
be asked to show first. The size objection is answered by per-map caps, not by dropping
the data.

**One shared 500-entry budget across all three maps, filled branches-first.** Bounds the
worst case at today's number. **Rejected because truncation becomes order-dependent and
hard to read**: whether tags were truncated would depend on how many branches happened to
exist, and a single `truncated_at` could not say which kind lost entries.

**Extend `refs.json` (the deletion-detection snapshot) to match.** **Rejected as out of
scope**: that snapshot's only job is noticing local-branch deletions that happened
outside the app. Nothing about HEAD or tags serves it, and widening it would add a
second, differently-shaped ref record to keep in sync.

**Build the capture on `read_history_materials`, which already returns the HEAD facts.**
**Rejected on two counts**: it also reads `$GIT_DIR/shallow` and hard-errors on malformed
shallow metadata, so a corrupt `shallow` file would turn every event's capture into a
failure for a reason unrelated to refs; and it reads HEAD through `head_name()`, whose
failure is a hard error there — see the departure below.

## Departure from the design: HEAD has a fifth state

The design's D3 specified four HEAD states and called them total, from a probe covering
four repository states. Implementation added a fifth, `Unreadable { reason }`, after a
probe run here reached a state the design's did not:

| repo state | `head_name()` | `head_id()` | variant |
|---|---|---|---|
| `git init`, no commits | `Ok(Some("refs/heads/main"))` | `None` | `Unborn` |
| on a branch with commits | `Ok(Some("refs/heads/main"))` | `Some(c3c136cc…)` | `OnBranch` |
| `checkout --detach HEAD~1` | `Ok(None)` | `Some(a070b160…)` | `Detached` |
| `.git/HEAD` = an oid with no object | `Ok(None)` | `None` | `Unresolvable` |
| **`.git/HEAD` corrupt, `.git/refs` intact** | **`Err`** | `None` | **`Unreadable`** |

The design's `garbage_head` probe corrupted HEAD in a repository whose ref store was also
gone, so `gix::open` failed and the whole capture became `CaptureFailed` — correct, and
the existing test pins it. With `.git/refs` **intact**, `gix::open` succeeds,
`repo.head_name()` returns `Err`, and `read_refs` still returns `main`. Under D7 as
written — HEAD facts taken from `read_history_materials`, where that error is a hard
`RepoError` — the whole capture would become `CaptureFailed`, discarding branches that
read perfectly well. `Unreadable` records the reason and keeps them.

The four states the design specified are unchanged, and all four were re-verified here.

## Consequences

- The #136 viewer can replay the HEAD moving, tags appearing, and remote-tracking refs
  advancing — the whole badge set the graph draws.
- Journal lines grow; see D6 for the bound. The journal file remains unbounded, and
  `read_all` still reads and parses all of it on every feed request — that predates #449
  and is recorded as finding F2 in the design, recommended as its own issue.
- A pre-#449 line still parses and claims nothing about HEAD, tags or remotes. A pre-#131
  line still parses and claims nothing at all.
- `read_refs` and `read_refs_at` disagree about a HEAD holding an unresolvable oid —
  `head()` hands back the raw id, `head_id()` refuses it. That disagreement predates
  #449 (design finding F1) and is deliberately preserved rather than resolved here, so
  the refactor carries no behaviour change of its own.
- `HeadAtEvent` and `CapturedRefs` live in `git-vista-core::activity` and are produced by
  `git-vista-git`, following `ReflogEntry`'s existing precedent.

## Evidence

Mutation-proven rather than asserted: **20 mutations applied to production code and run,
two per claim, each expected to fail differently. All 20 went red.**

| # | mutation | test that caught it |
|---|---|---|
| 1a | HEAD is not recorded (`head: None`) | every HEAD test, and every test using the capture helper |
| 1b | HEAD records the short branch name, not the full ref | `a_capture_records_which_branch_head_was_on_and_where_that_branch_was` |
| 2a | a detached HEAD is given the fabricated name `HEAD` | `a_detached_head_is_recorded_as_detached_not_as_the_branch_it_sits_on` |
| 2b | a detached HEAD falls through to `Unresolvable` | the same test, on the discarded oid |
| 3a | an unborn HEAD fails the whole capture | `an_unborn_head_…`, and `a_repo_with_no_branches_captures_an_empty_map_not_a_failure` |
| 3b | an unborn HEAD discards the branch name | `an_unborn_head_records_the_branch_it_names_with_no_commit` |
| 4a | a HEAD pointing at nothing fails the whole capture | `a_head_pointing_at_nothing_is_unresolvable_and_the_branches_survive` |
| 4b | …becomes `Detached` at an invented empty oid | the same test, on the variant |
| 5a | an unreadable HEAD propagates as a `RepoError` | `an_unreadable_head_records_the_reason_while_the_branches_still_capture` |
| 5b | an unreadable HEAD drops its reason | the same test, on the variant |
| 6a | tags are not collected | `tags_are_captured…`, `caps_are_per_map…`, `a_deleted_tag_survives…` |
| 6b | a ref records its own target instead of the peeled commit | `tags_are_captured_and_an_annotated_tag_records_the_commit_it_peels_to` |
| 7a | absent tags default to an observed empty map | `absent_and_observed_empty_are_different_answers_about_tags` |
| 7b | a tagless repo emits `None` instead of an observation | the same test, other half |
| 8a | remote-tracking refs are not collected | `remote_tracking_refs_are_captured_and_origin_head_is_not` |
| 8b | the `refs/remotes/<remote>/HEAD` skip is removed | the same test, on the exclusion |
| 9a | one shared budget across the maps | `caps_are_per_map_and_each_reports_its_own_overflow` (branches evicted) |
| 9b | truncation is silent | the same test, on `truncated_at` |
| 10a | `append` stops attaching a capture | `a_deleted_branch_survives…`, `a_deleted_tag_survives…` |
| 10b | the capture is taken at read time, not append time | five tests, including both lossless-survival tests |

Per the design's caution, every assertion about an oid compares against `git rev-parse` /
`git symbolic-ref` output taken from the fixture, never against a second call into the
capture code — asserting that `capture_refs` agrees with `capture_refs` proves only that
it is self-consistent.

Two further mutations were run and are recorded because they did **not** behave as first
predicted; the test comments say what is true rather than what was assumed:

- Making `tags` a bare `BTreeMap` **does not compile**. The collapse is refused by the
  type system before a test can catch it — stronger than a red test, but not a red test.
- Dropping `#[serde(default)]` from an `Option` field **stays green**: serde already
  deserializes a missing `Option` field as `None`. The attribute is belt-and-braces
  there, not load-bearing.

A first pass at mutation 10b silently failed to apply (it patched a statement `read_all`
does not contain) and reported green. It is recorded here because the green was a script
artefact, not a test weakness — re-run correctly against the real iterator chain, it goes
red on five tests.

Test evidence: 20 tests in `journal::tests`, 10 of them new. `cargo clippy --workspace
--all-targets -- -D warnings` and the wasm32 frontend clippy both clean; `cargo fmt
--check` clean. `git-vista-server`'s suite went 572 → 582 passing with its pre-existing
failure count unchanged at 320 — those are the sandbox-tier tests this host cannot run
(no `bwrap`, Landlock ABI < 6, refused per ADR 0029), identical on the pristine tree.

**Signed:** thomas2025 · 2026-08-23
