# 0088 — A signature status says what the crypto said: expiry and revocation get their own variants, and the vocabulary is a protocol bump

**Status:** Accepted — implemented and tested; browser leg unrun
**Date:** 2026-08-26
**Issue:** [#335](https://github.com/tom2025b/Git-Vista/issues/335)
**Relates to:** [0002](0002-versioned-api-contract.md) (the negotiation this
bumps), [0041](0041-tag-operation-vocabulary.md) (where `SignatureStatus` was
declared), [0079](0079-the-stash-endpoints-share-their-dtos.md) and the v7 bump
of #506 (the last time a wire shape moved without the version following it)

---

## Context

`SignatureStatus` is the tag surface's whole vocabulary for "what is known
about this signature". It shipped with five variants — `Unsigned`, `Valid`,
`Invalid`, `UnknownKey`, `Unverifiable` — chosen so that "we could not check"
could never be confused with "we checked and it failed".

`classify_verify_tag_output` maps gpg's status protocol onto it. gpg answers a
verification with exactly one *sig-level* status line, and there are five of
them. Three were handled. Two were not, and one of those two was handled
wrongly:

| gpg status | What gpg means by it | Reported before this ADR |
|---|---|---|
| `GOODSIG` | the bytes check out | `Valid` |
| `BADSIG` | the bytes provably do not check out | `Invalid` |
| `ERRSIG`/`NO_PUBKEY` | never got far enough to say | `UnknownKey` |
| `EXPKEYSIG` | the bytes check out; the signing **key** has since expired | `Valid` |
| `EXPSIG` | the bytes check out; the **signature's** own expiry has passed | `Valid` |
| `REVKEYSIG` | the bytes check out; the key has been **revoked** | `Unverifiable` |

The last three are the defect, and they fail in two different directions.

**The two expiries over-claimed.** `EXPKEYSIG` and `EXPSIG` were explicitly
folded into `good = true` — with a comment saying so — on the reasoning that
the cryptographic check really did pass and inventing `Invalid` would be worse.
That reasoning is right about `Invalid` and wrong about the conclusion: it left
a tag signed by a key abandoned three years ago rendering the badge "signature
valid", byte-identical on the wire to a live, trusted, current signature. The
one question a reader brings to that badge — *should I trust this tag* — was
answered with the more reassuring of two available answers.

**`REVKEYSIG` under-claimed, and did it silently.** No arm matched it. It fell
through a bare `_ => {}` to the "no recognised status line" branch and surfaced
as `Unverifiable` — "signed, not checked". A revoked key is what a signer
publishes when they believe the key is compromised. The single most alarming
thing gpg can say about a signature it *did* check was displayed as a shrug,
and nothing in the codebase knew: the fallthrough could not distinguish "we
considered this keyword and it carries no verdict" from "nobody has ever looked
at this keyword".

Found by adversarial review of #237 — the slice that built the classifier — and
filed as #335.

## Decision

### 1. Three new variants, flat, not one parameterised `Expired`

`SignatureStatus` gains `ValidExpiredKey`, `ValidExpiredSignature` and
`Revoked`, all unit variants alongside the existing five.

The alternative on the table was one `Expired { what }` variant carrying which
thing expired. Rejected on the wire, not on taste: every variant of this enum
serialises as a bare string under `#[serde(rename_all = "snake_case")]`, and a
struct variant would serialise as an *object*, giving one field two encodings
depending on its value. The golden pin asserts "every declared status has a
pinned wire name" — a claim that stops being expressible — and `Copy` on the
whole enum stops being free. Three unit variants keep the encoding uniform and
keep every consumer's `match` exhaustive by compiler, which is the property
that made this change safe to make at all: adding a variant produced a build
error at each of the two sites that had to decide something.

The names are load-bearing:

* the two expiries lead with `Valid…` because the cryptography *did* pass, and
  the badge must not read as though something failed;
* `Revoked` deliberately does **not**, even though gpg's `REVKEYSIG` also means
  the bytes matched. Revocation is a published statement that the key must not
  be trusted; spelling it `ValidRevoked` would invite exactly the reading the
  variant exists to prevent.

### 2. The classifier resolves by an explicit precedence table

`VERDICT_PRECEDENCE` lists the six acted-on keywords most-alarming-first, and
the classifier takes the minimum index seen across the whole run. This
generalises the rule the previous code had for `BADSIG` alone ("scan every
line; `BADSIG` outranks everything regardless of order") to all six, so
`REVKEYSIG` beside a `GOODSIG` cannot resolve to `Valid` on line order.

Real gpg emits one sig-level line per signature, so on real input the ordering
is unobservable — which is precisely why it is pinned by a test against a
literal rather than left to a fixture that can never exercise it.

**And why that pin has to be exhaustive over orderings, not illustrative.** The
first version of the ordering test hand-picked five pairs and wrote each with
the milder keyword first, which defends against a first-match-wins reducer and
nothing else. An outside review (codex, 2026-08-26) showed by simulation that a
reducer regressing to last-line-wins passes all five, while `BADSIG` followed by
`REVKEYSIG` returns `Revoked` — a **forged** signature downgraded to a softer
verdict, the single outcome this classifier exists to make impossible. The
production reducer was correct throughout; the proof was half a proof. The test
now generates every ordered pair of distinct table entries and requires the
stronger to win in *both* orders, with an anti-vacuity assertion on the table's
width, because a hand-written list can be complete today and silently partial
after the next keyword lands.

`NO_PUBKEY` stays *below* `GOODSIG`, preserving pre-#335 behaviour: a run that
produced a good signature has answered the question even if some other key in
the same run was missing.

### 3. The fallthrough stays, and stops being silent

A `match` cannot be exhaustive over gpg's status vocabulary — it lives in
another project's source and grows without asking us — so an unmodelled keyword
must not stop a tag from being described. It still classifies `Unverifiable`.
What changes is that it is now *named*:

* `ABSORBED_GPG_STATUS` is a census of every keyword this build deliberately
  ignores, each entry read out of the shipped `gpg` 2.4.4 binary's own string
  table rather than written from memory, and grouped by why it carries no
  verdict.
* Anything in neither list comes back from
  `classify_verify_tag_output_with_census` and is reported on the server's
  stderr, once per keyword per run, Debug-escaped and length-capped because the
  string came from a subprocess reading a repository the operator may not
  trust.
* `every_status_line_in_every_fixture_is_acted_on_or_censused` sweeps all six
  committed fixtures and fails if any carries a keyword in neither list.

That is the honest form of the guard the issue asked for. Nothing can make a
foreign vocabulary exhaustive; what *can* be guaranteed is that the next
`REVKEYSIG` is loud rather than invisible.

### 4. `KEYEXPIRED`/`KEYREVOKED` are absorbed, not read as verdicts

Both ride along with the fixtures for cases 1 and 3, and both are tempting to
key off. Both can also describe a *different* key gpg considered on the way, so
a classifier reading them would report a signature by the state of a key that
did not make it. The sig-level line is the only one that names the key that
did. Pinned by
`a_key_lifetime_line_beside_a_goodsig_does_not_become_the_verdict`.

### 5. This is protocol v8 — a new enum variant is not an additive change

`PROTOCOL_VERSION`, `MIN_CLIENT_PROTOCOL` and `MAX_CLIENT_PROTOCOL` move 7 → 8
together, the whole window, as v5, v6 and v7 did.

This is the case that looks additive and is not, and the distinction is worth
recording because the crate's own M1.02 rule points the other way at a glance.
That rule is about **fields**: response DTOs omit `deny_unknown_fields` so an
older client tolerates a key it does not know. A new **enum variant** is the
mirror image:

* `SignatureStatus` is a closed vocabulary with no `#[serde(other)]` arm, so
  `"revoked"` at a v7 client is an `unknown variant` error;
* `signature` is a **required** field of `TagDetail`, so that error fails the
  entire record, not one field;
* `TagDetail` additionally carries `deny_unknown_fields`, so there is no
  tolerance anywhere in the shape to fall back on.

The user-visible consequence is not a vaguer badge. It is a tag list that stops
rendering, on any repository holding one expired or revoked signature, for a
client that negotiated successfully against a `[7, 7]` window — the exact
situation ADR 0002's negotiation exists to prevent. Hence a hard window move.
`the_variant_extractor`-style derivation now also guards the vocabulary:
`declared_signature_status_tags()` reads the variants out of `dto.rs`, so a
future variant with no pinned wire name fails the golden test by name rather
than passing at a stale count.

### 6. The badge says what happened

| Status | Badge |
|---|---|
| `Valid` | `signature valid` |
| `ValidExpiredKey` | `signed, key since expired` |
| `ValidExpiredSignature` | `signed, signature expired` |
| `Invalid` | `signature invalid` |
| `Revoked` | `signed, key REVOKED` |
| `UnknownKey` | `signed, key unknown` |
| `Unverifiable` | `signed, not checked` |

A badge is the whole of what a reader gets — the row has no room for a
sentence — so "expired" alone is not acceptable: it leaves the reader to guess
which thing expired and whether the tag is still worth anything. Each badge
leads with `signed,` because in all three new cases the cryptography passed,
and then names its subject.

`REVOKED` is capitalised, and that is the part worth defending. The tag band
renders every badge with the same neutral `act-pill` class; there is no
severity colour in this surface, and adding one means editing `styles.css`,
which is outside this change's boundary and under the a11y stylesheet census.
So wording is the only channel available to carry weight, and it is used rather
than left unused. If a severity treatment is added to the tag band later, this
badge is the first candidate to take it and the capitals should go.

## Alternatives weighed

* **One `Expired { what: ExpiredWhat }` variant.** Rejected: mixes string and
  object encodings within one wire field, breaks the "every status has a pinned
  wire name" golden pin, and buys nothing — there are exactly two things that
  can expire and no prospect of a third.
* **Report `EXPKEYSIG`/`EXPSIG` as `Invalid`.** Rejected, and it is the failure
  mode the original code was right to avoid: nothing failed. A tag signed
  correctly in 2019 by a key that expired in 2022 is not forged, and reporting
  it as such trains readers to ignore the badge that means forgery.
* **Report `REVKEYSIG` as `Invalid`.** Tempting — it is the most alarming
  outcome — and rejected for the same reason: `Invalid` is a claim about the
  *bytes*, and the bytes match. Collapsing them would destroy the distinction
  ADR 0041 built this vocabulary around, in the direction of over-claiming
  rather than under-claiming, which is not an improvement.
* **Leave the two expiries as `Valid` and fix only `REVKEYSIG`.** Rejected: the
  expiries are the higher-traffic case by a wide margin (keys expire on a
  schedule; revocations are rare), so the badge most readers see is the one
  that would have kept over-claiming.
* **No protocol bump, on the grounds that adding a variant is additive.**
  Rejected on evidence, not on principle — see §5. Pinned by a test that
  deserialises a `TagDetail` carrying an unmodelled status and asserts the
  whole record is refused with `unknown variant`.
* **A `#[serde(other)]` catch-all on `SignatureStatus`, so older clients
  degrade instead of failing.** Rejected, though it is the most interesting
  alternative here. It would make future additions genuinely additive — at the
  cost of every unrecognised status silently becoming whatever the catch-all is
  named, on a surface whose entire purpose is not doing that. A client that
  renders `Revoked` as "unknown status" is a client that has quietly returned
  to the #335 behaviour. Worth revisiting only alongside a decision about what
  such a client should *display*, which is a larger question than this issue.
* **A severity colour for the revoked badge.** Deferred, not rejected: the tag
  band has no severity vocabulary, `styles.css` is outside this change's
  boundary, and a new class would need the a11y stylesheet census to cover it.
  Wording carries it for now, deliberately and visibly.

## Consequences

- Three real gpg outcomes stop being misreported. An expired key no longer
  reads identically to a live one; a revoked key no longer reads as a shrug.
- Every client must speak protocol 8; a cached v7 tab gets the existing
  "Update Required" screen rather than a tag list that fails to parse.
- The classifier's fallthrough is now a documented census plus a runtime
  report, so the next unmodelled gpg status is visible in three places (a red
  sweep test if it reaches a fixture, a stderr line if it reaches production,
  and a named entry when someone adds it deliberately).
- `SignatureStatus` has eight variants. Every consumer's `match` is
  wildcard-free, so a ninth is a compile error at each decision site — the
  property that made this change tractable, and worth preserving.

## Verification

- Three new classifier tests, each against **verbatim** `git verify-tag --raw`
  stderr captured in this container from git 2.43.0 + gpg 2.4.4: a key
  generated and used to sign under `gpg --faked-system-time 20260101T000000!`
  with a one-day lifetime (`EXPKEYSIG`), a five-year key signing through
  `--default-sig-expire 1d` at the same frozen instant (`EXPSIG`), and a live
  key whose own revocation certificate was imported after the tag was signed
  (`REVKEYSIG`).
- A sweep over all six fixtures proving six distinct statuses, and the census
  guard proving no fixture carries an unclassified keyword.
- Two literal pins: the precedence table in order, and the absorbed census —
  neither derivable from the code they guard.
- Mutation-proved twice: reverting `REVKEYSIG` to the fallthrough, and swapping
  the two expiry rows of the precedence table. Each is red at a different
  assertion, and the restore is byte-identical. Evidence in the PR body.
- The ordering contract is mutation-proved in both directions after the review
  finding above: `verdict = Some(rank)` (last-line-wins) is red on
  "`BADSIG` must outrank `REVKEYSIG` when it comes FIRST", and
  `verdict = verdict.or(Some(rank))` (first-line-wins) is red on the same pair
  "when it comes SECOND". Neither mutation is caught by both halves, which is
  the reason both orders are asserted rather than one.
- `cargo fmt --all`; clippy clean at `-D warnings` on both the native and the
  `wasm32-unknown-unknown` targets.
- **The browser leg has not been run.** This change touches the tag band's
  display path and `ci/browser/run.sh` cannot run in a cloud container; the
  wording above is host-tested but has not been seen on screen.
