# 0079 — The stash endpoints share their DTOs, and the wire carries the selector rather than the position

**Status:** Accepted — implemented and tested; browser leg unrun (see Consequences)
**Date:** 2026-08-25
**Issue:** [#495](https://github.com/tom2025b/Git-Vista/issues/495) (a [#77](https://github.com/tom2025b/Git-Vista/issues/77) follow-up)

---

## Context

Every other read in this application deserialises a type from `git-vista-protocol` on both ends. The stash endpoints alone had every field name written twice, with nothing forcing the two copies to agree:

- **The listing built its JSON by hand.** `handlers::stash::stash_list` composed a `serde_json::json!` object per entry — `entry`, `index`, `oid`, `message`, `time` — spelled as string literals in the handler.
- **Each write declared a local struct** in that same file: `PushStashRequest`, `StashEntryRequest`, `BranchFromStashRequest`. Two of the three tolerated unknown fields, unlike every other write DTO in the workspace.
- **The client declared its own again**: `PushStashBody`, `StashEntryBody`, `BranchFromStashBody` in `api/stash.rs`, and a third transcription of the listing as `features::stash::core::StashEntry`.

### Why the failure mode is worse than a broken build

A rename on either side is not an error. A field serde cannot find is a field that was not sent, so a renamed listing key deserialises as **absent**, and the stash drawer renders **empty** — not a 400, not a message, just "Nothing stashed."

That is precisely the distinction `git_vista_git::stash::read_stashes` goes out of its way to preserve, and says so in its own doc comment:

> An empty `Vec` means the drawer was **read and is empty** … A failure to read returns `Err`; the two are never merged, because "no stashes" and "couldn't look" authorise very different things.

A hand-built JSON object launders that distinction back in one layer up.

### The worked example that made this urgent

On 2026-08-25 the drawer's "Show changes" control was dead on arrival in the shipped app. The server's `ShowStashQuery` was `deny_unknown_fields` while the client appends a `?t=` cache-buster to every GET, so every click answered ``unknown field `t`, expected `entry` `` and the drawer rendered that JSON where the patch belonged. Nothing in the Rust suite could see it: the handler was only ever called with a query a test had composed by hand, never the one the browser sends. Different mechanism from a rename, identical root shape — **the two ends of the wire had no single author.**

---

## Decision

### 1. Four shapes move to `git-vista-protocol::dto`, and both ends deserialise them

| type | endpoint(s) | strict? |
|---|---|---|
| `StashEntry` | `GET /api/stashes` (response) | no — additive rule (M1.02) |
| `StashTarget` | `POST /api/stash/apply`, `POST /api/stash/drop` (whole body) | yes |
| `PushStashRequest` | `POST /api/stash/push` | yes |
| `BranchFromStashRequest` | `POST /api/stash/branch` | yes |

Both write bodies that previously tolerated unknown fields are now closed, matching `dto.rs`'s stated rule for everything that reaches a git argv. The response DTO deliberately stays open, so a client older than its server keeps parsing when a field is added.

### 2. The fields carry the newtypes, not strings

`entry` is a `StashSelector`, `expected_oid` and `oid` are `CommitOid`, `name` is a `BranchName`, `message` is a `StashMessage`. Each validates from its own `Deserialize`, so a malformed value is a wire-boundary refusal and **the handlers have no validation left**: `parse_entry`, two `::new` calls in `branch_from_stash`, and the push handler's blank-message branch are all deleted, not moved.

That is the structural half of the argument, and `ResolveConflictRequest` already made it here in prose after paying for it: *"a handler-side `WorktreePath::new(...)` is a step someone can delete, and a test that calls `WorktreePath::new` directly will not notice — it is testing the newtype, not the endpoint. Verified the hard way: that exact mutation SURVIVED."*

The one field left as a `String` is the listing's `message`. A stash list is a reflog and any tool may have written a line into it, including one that left it blank; typing it as `StashMessage` would make an odd entry fail the whole listing, which is strictly worse than showing the row.

### 3. The wire carries `entry`, and `index` is gone

This is the question the handoff asked to be settled explicitly. The listing used to send both, with the first *derived from the second one line earlier*:

```rust
"entry": format!("stash@{{{}}}", s.index),
"index": s.index,
```

`entry` survives, `index` does not, for three reasons:

1. **It is the only form git accepts.** `git stash drop <oid>` is not a command; apply, drop and branch all take a reflog selector, and the selector is what every write echoes back.
2. **No client read `index`.** Not the drawer, not a row, not the browser suite. Dropping it moved no work anywhere — the objection that dropping a derivable field pushes work onto the client simply does not apply here.
3. **The two can genuinely disagree.** Selectors renumber on every drop, so a listing read moments ago can be stale in exactly the way that makes a derived field wrong. Carrying both is a second place for one fact to be wrong.

The mapping is not lost and is not a client's to re-derive by parsing: `StashSelector::at(index)` is now the **only** place the `stash@{N}` spelling is produced, and `StashSelector::index()` reads it back. One author, both directions, tested at `usize::MAX` and at a digit run that overflows a `usize` (`None`, never a wrong number).

### 4. `BranchFromStashRequest` nests the pair rather than respelling it

Its body becomes `{"name": …, "target": {"entry": …, "expected_oid": …}}`. `#[serde(flatten)]` would have kept the JSON flat but is mutually exclusive with `deny_unknown_fields`, and giving up strictness on the body that reaches `git stash branch` to save one level of nesting is the wrong trade. `TagAnnotation` already nests an operation's inner shape for its own reasons.

### 5. `StashEntry.oid` and `StashTarget.expected_oid` stay two declarations

They are the same 40-hex value moving in opposite directions, and merging them was considered. They are kept apart because they are different claims: the listing's `oid` is a **fact** the server observed, and `expected_oid` is a **belief** the client asserts and the executor compare-and-swaps. Naming the fact "expected" would misdescribe the read; naming the belief "oid" would hide that it is the thing being checked.

### 6. The `show` query is typed but not shared

`ShowStashQuery` stays in the handler. A query string is not a body: the frontend builds that URL with `js_sys::encode_uri_component`, and serialising a shared type into a query would need `serde_urlencoded` as a dependency of the wasm crate — a bigger change than the duplication it removes. What is shared is the part that could be silently wrong: its `entry` field is a `StashSelector`, so it runs the same validator, and the client validates the selector before building the URL at all. The parameter *name* is pinned by the three tests that send the browser's real query string, cache-buster and all.

---

## Alternatives considered, and why they lost

**Keep both `entry` and `index` on the wire.** Rejected: a derived field is a second place to be wrong, and this one can disagree with its source under an ordinary concurrent drop. Nothing read it.

**Send only `index` and let the client format the selector.** Rejected: it moves the `stash@{N}` spelling into the client, which is the duplication this ADR removes, pointed the other way — and `features::stash::core` already carried a test forbidding exactly that.

**Move the structs but leave the fields as `String`.** Rejected: it fixes the names and leaves the validation as handler code someone can delete, which is the mutation that already survived once in this workspace (#429).

**Flatten the branch body to keep its JSON shape.** Rejected: serde makes `flatten` and `deny_unknown_fields` mutually exclusive, and the strictness is worth more than the flat shape.

---

## Consequences

- Every stash field name has one author. The only wire-shape declarations left in the workspace are in `crates/git-vista-protocol/src/dto.rs`; the remaining matches for `entry:` / `expected_oid:` / `keep_index:` are `GitOperation` variants (the internal operation vocabulary, which the handler maps into) and function parameters.
- `stash_list` no longer builds JSON. It maps `StashRecord` → `StashEntry` through `listing_entry`, and a record it cannot express is a **500**, never an entry quietly left out of the list — a shorter list renumbers everything below the gap, and the number is the address the user's next click acts on.
- **A blank stash message is now refused one process earlier.** `Option<StashMessage>` cannot spell `Some("")`, so the server's hand-written sentence about it is unreachable and deleted; the client raises the same sentence before sending. The shipped UI always sends `None` (git writes its own `WIP on <branch>`), so no user-visible path changes.
- **`POST /api/stash/branch`'s body shape changed** (`entry`/`expected_oid` nested under `target`). Both ends move together and no third-party client exists, but it is a wire change and is recorded as one.
- **The browser leg has not been run.** `ci/browser/run.sh` cannot run in a cloud container: the server refuses to start without its strict sandbox tier and that kernel reports `landlock_abi=-1`, which INV-13 gives no degraded mode for. This change rewrites the wire, so the browser suite is exactly what would catch a mistake in it, and it must be run on a host with the sandbox before this merges.

---

## Findings recorded while implementing

**A round trip through a shared type proves nothing on its own.** Once both ends deserialise the same struct, `to_string` → `from_str` agrees with itself through *any* rename — the very drift being guarded against. So every DTO test here pins a **JSON literal**, which is the only thing that can say what a browser receives. Mutating each field's wire name with `#[serde(rename = …)]` is red at runtime on those literals; retyping a field to a bare `String` is red at compile time, because the type is load-bearing in the test as well as in the handler.

**One of those tests was vacuous, and the mutation matrix is what said so.** Retyping `StashEntry.oid` from `CommitOid` to `String` left `the_stash_listing_pins_its_wire_keys_and_their_types` **green**: `String::as_str` exists, and the JSON literal round-trips either way. The test agreed with itself while the guarantee it is named for was gone. It now asserts that a listing carrying an abbreviated oid does not deserialise at all — the property the type actually buys, and the one the compare-and-swap depends on. Recorded rather than quietly fixed, because "the test was red for the other mutation" is exactly the reasoning that would have let it ship.

**The mapping tests are mutated at the mapping, not only at the wire.** Building every selector as `StashSelector::at(0)` instead of `at(record.index)`, and rebuilding the client's row selector from its position in the list, are each red — which is what stops "the field names line up" from being mistaken for "the right value is in the field".
