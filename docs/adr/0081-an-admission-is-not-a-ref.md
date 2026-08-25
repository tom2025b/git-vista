# 0081 — An admission that git could not be read is not a ref, and never folds into a count

**Status:** Accepted — implemented and tested
**Date:** 2026-08-25
**Issue:** [#486](https://github.com/tom2025b/Git-Vista/issues/486)

---

## Context

`fold_ref_update_bursts` collapses a run of remote-tracking ref updates into one counted row — "fetch — 94 refs updated" — because #329 put 94 rows in the feed and buried the revert the user was looking for.

`planner::fetch::journal_unobserved` writes one journal entry for the one fetch outcome the fetch module cannot name: `git fetch` ran to completion and the re-read of `refs/remotes/<remote>/*` that would say what it moved failed. The entry carries no `ref_name`, because the ref set is precisely what is unknown, and `Obs::Unknown` on both tips rather than `Obs::Absent`, because "git could not be read" is not "there is no such tip". `journal_app_event` turns that into the visible suffix "(tips unknown — git could not be read)".

`fold_ref_update_bursts`'s doc comment used to argue that the two never met:

> That is also what keeps the "tips unknown — git could not be read" entry intact: it is journaled *instead of* per-ref entries, never alongside them, so it is always a run of one.

**That reasoning accounts for the journal and forgets git's reflog** — the same shape of mistake that got the first #329 attempt reverted in `0a7ba777`. The fetch *succeeded*, so git logged every ref it moved. The admission carries no `new_oid`, so it matches no reflog line in `assemble_feed`'s attribution step and suppresses none of them. They all survived, the admission joined them, and the whole run folded. Measured 2026-08-25, four refs moved:

```
journal:  1 × "fetched from ‘origin’ … (tips unknown — git could not be read)"
reflog:   4 × "fetch origin: fast-forward"
feed:     1 × "fetch — 5 refs updated"
```

Two harms in one line of output. A deliberate admission of ignorance was replaced by a confident count — the exact distinction `Obs::Unknown` exists to preserve, destroyed in the direction of false confidence. And the count was wrong anyway: four refs moved, and the admission was counted as the fifth.

Evidence: `docs/investigations/2026-08-25-issue-329-fetch-feed-volume.md`, finding F2, which was pinned in `activity.rs` as a `#[should_panic]` expected-failure test until this change.

## Decision

**The admission is excluded from the fold outright, on its whole shape rather than on any one field.**

```rust
fn admits_it_could_not_read_the_refs(event: &ActivityEvent) -> bool {
    event.ref_name.is_none() && event.old_oid.is_none() && event.new_oid.is_none()
}
```

evaluated only for `ActivityKind::Fetch` and `ActivityKind::Pull`, the two kinds the fold looks at.

### Why that shape, and the check that it is not shared

The handoff for this issue asked the obvious question before shipping: is `journal_unobserved` genuinely the only producer of that shape? If some other path emits a Fetch/Pull event with no ref name and no oids, excluding the shape excludes those too, and a burst that *should* fold stops folding. Every construction site was read rather than trusted:

| Producer | Kind | Ref name | Oids |
| --- | --- | --- | --- |
| `assemble_feed` step 1, from a `ReflogEntry` | any | always `Some` | always both `Some` |
| `planner::fetch::journal_updates`, one per moved ref | `Fetch` | always `Some` | `Obs::Known`/`Obs::Absent` |
| `planner::fetch::journal_unobserved` | `Fetch` | **`None`** | **both `None`** |
| `planner::branch_exec::exec_merge`/`exec_rebase` (pull's integration half) | `Pull` | always `Some` (the branch pulled into) | `Obs` of a real read |
| `planner::worktree_exec`, two all-`None` admissions | `Other` | `None` | both `None` |
| `planner::push`, one all-`None` admission | `Push` | `None` | both `None` |

Reflog-derived events carry a ref name and both oids **by construction** — a `ReflogEntry` has no optional fields, so every one of them fails all three tests and no reflog line can ever be mistaken for an admission. The two other all-`None` journal writers carry kinds this fold never looks at; they are recorded here because they are the same shape and a future fold over `Push` would meet the same question.

So within `Fetch | Pull`, `journal_unobserved` is the sole producer, and no narrower discriminator — an explicit marker field on `ActivityEvent`, say — is needed to tell it apart. The shape *is* the discriminator, and this table is why.

### Why not `ref_name: None` alone

Because it is wider than the fact being used. A Fetch row that names no ref but does know where the ref landed is not an admission of ignorance — it knows what moved, and belongs in the count. Nothing builds one today, which is exactly why the narrower discriminator would go unnoticed: on every fixture a real code path can produce, `ref_name.is_none()` and the full shape are behaviourally identical. Two synthetic fixture events exist in the test module for that reason alone — `ref_name: None` with a known oid, and `ref_name: Some` with neither oid — so both halves of the conjunction have something to fail.

`Obs::Absent` flattens to `None` exactly as `Obs::Unknown` does, so the oid pair cannot carry the decision on its own either. The `ref_name` is what separates a journalled ref update from the admission; the oids are what separate the admission from a row that observed something. Both halves are load-bearing, and each is pinned by its own test.

## Consequence — what the four refs now render as

The refs that really moved still fold, from their reflog lines, into a row of their own. The feed shows **two rows**:

```
fetch — 4 refs updated                                    (External — git's reflog)
fetched from ‘origin’, but refs/remotes/origin could not be
  re-read afterwards … (tips unknown — git could not be read)   (App)
```

This is a deliberate choice and the one place a reviewer may reasonably want a different answer, so it is argued rather than assumed.

**The two rows are two different facts from the feed's two different sources.** git's reflog saw four ref movements and says so; the app says it could not confirm any of them. Neither statement is evidence for or against the other, and the feed exists precisely to merge those two sources.

**Making the admission swallow the reflog rows was rejected.** It would need the admission to suppress lines it has no oid to match — i.e. to claim, on timing alone, that those four movements are the ones it failed to observe. That is the move `0a7ba777` reverted: an entry with no `new_oid` matches nothing in attribution, and treating it as though it did is how the first #329 attempt took the feed from 94 rows to 95. It would also hide four real, known ref movements behind an admission of ignorance *about those very movements* — under-claiming where the old behaviour over-claimed, which is not obviously the better error.

What is not acceptable, and what the tests now forbid, is a row saying "4 refs updated" **in place of** the admission — the naive repair that fixes the count and keeps the erasure. The user must still see that something could not be read.

A fetch that moved nothing at all — the one case the old comment's claim was actually true for, since there are no reflog lines — is unchanged: the admission is a run of one and is returned untouched.

## Verification

The `#[should_panic]` on `an_unobserved_fetch_keeps_its_admission_instead_of_being_counted` is deleted and the test passes on the assertions it was written with, unedited.

Every test pinning this was proved able to go red, each in at least two independent ways:

| Mutation | Red |
| --- | --- |
| A — remove the exclusion from the `partition` predicate | the two admission tests |
| B — weaken to `ref_name.is_none()` alone | `a_fetch_that_names_no_ref_but_knows_an_oid_is_still_counted` |
| C — weaken to both-oids-`None` alone | `a_fetch_that_names_a_ref_without_oids_is_still_counted` |
| D — `admits_it_could_not_read_the_refs` returns `false` | the two admission tests |
| F — the admission is dropped rather than passed through | all three admission tests |
| G — the naive repair: fold it away but do not count it | all three admission tests |
| H — `admits_it_could_not_read_the_refs` returns `true` | both synthetic tests, and six pre-existing fold tests |
| I — `\|\|` instead of `&&` in the discriminator | both synthetic tests |

One `caught` verdict is not proof: a Git-Vista test survived one mutation and caught another on 2026-08-22, and either alone gives the wrong verdict.

**The write-path half of the invariant was already pinned before this change, and is not re-derived here.** `planner::fetch_suite::a_fetch_whose_outcome_cannot_be_observed_is_journaled_as_unknown` drives a real fetch against a real repository blinded after the fetch, and asserts each of the three fields the discriminator reads:

```
assert_eq!(entry.ref_name, None, "which ref moved is precisely what is unknown …");
assert_eq!(entry.old_oid, None, …);
assert_eq!(entry.new_oid, None, …);
```

So the shape this ADR keys on cannot drift silently at the producer: a change to `journal_unobserved` that named a ref or invented an oid fails that test, in the crate where the change would be made. `journal_unobserved`'s doc comment now says so beside the code.

`cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` are clean, and `cargo doc -p git-vista-core --no-deps` adds no warning. `cargo test --workspace` is green in every crate except `git-vista-server`, which cannot run here: this container reports `landlock_abi=-1` — the capability probe's own words are *"this host is known to support Landlock; got abi=-1"* — and INV-13 gives the server no degraded tier, so every test that runs real git through the sandbox launcher fails. That is 320 tests, and it is environmental, not this change: the failing set on a stashed clean tree is **byte-identical** to the failing set with this change applied (320 = 320, `diff` empty), and `a_fetch_whose_outcome_cannot_be_observed_is_journaled_as_unknown` is one of the 320 on both.

**Confirmed on a Landlock host.** CI's `Core (check + test)` job runs `cargo test --workspace` (`.github/workflows/ci.yml`, "Test core + git + protocol + server + frontend crates"), and it passed on this branch — so the whole server suite, that write-path pin included, is green where the sandbox can actually be built.

The browser leg was not run either, for the same reason. This change is confined to the pure core and one doc comment; it reaches no frontend code.

## What this does not fix

F1 — the folded count inflating at scale — is untouched **here**, and was never this ADR's to fix. It was still a `#[should_panic]` pin when this change was written; #485 (ADR 0080) landed hours later and fixed it at the writer, so `a_slow_fetch_still_counts_only_the_refs_that_moved` is now a live regression test rather than a pin. Both statements are recorded because the ordering matters to a later reader: this ADR was authored against a tree where F1 was still pinned, and `main` was merged down afterwards.

The fold itself still counts both copies of any Fetch/Pull entry that drifts past `JOURNAL_MATCH_SLACK`; #485 removed every writer that can produce such drift rather than making the fold robust to it. That is ADR 0080's argument, not this one's.

With F1 and F2 both fixed, #329's two known successor defects are closed — but this ADR closes only #486.
