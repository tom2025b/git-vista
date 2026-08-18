# ADR 0059 — Plain-commit failures get a typed `CommitFailureKind`, split finer than amend's on signing

Date: 2026-08-18
Status: Accepted — implemented

Fulfills the follow-up ADR 0058 named and deferred ("Typing `/api/commit`'s
refusal to match amend's `AmendFailureKind`... Named as a follow-up, not
scope creep now"). Extends ADR 0040's typed-failure-kind pattern
(`AmendFailureKind`) to `POST /api/commit`'s own execution failures. Reuses
the GnuPG status-fd read ADR (none — `classify_sign_failure`, landed
untracked-by-ADR alongside the M2.21e tag-signing work) proved for signed
tags. Supersedes nothing.

## Context

`exec_commit_on_head` (`planner.rs`) answered every failed `git commit` —
a missing signing key, an unreachable signing agent, a rejected hook, an
empty index, anything else — with the same shape: a bare `400` carrying
git's raw stderr (or, since ADR 0058, the hook-timeout arm's own prose) as
plain text. The client (`create_commit_request` in `api.rs`) unwrapped it
through the generic envelope path and showed it verbatim in a modal.
Readable, but not actionable: nothing in the wire told the client whether
this was a signing problem (fixed in `git config`), a hook problem (fixed
in `.git/hooks`), an empty working tree (fixed by staging something), or
something neither classifies — so the client had every failure read the
same, and could not say what to do about any of them.

Issue #72 (M2.19)'s acceptance criterion for this slice: *"Signing failures
are actionable."* `exec_amend_commit` already solved this shape for the
amend route (ADR 0040, `AmendFailureKind`): a closed, typed classification
plus git's own words, read back into client-side guidance
(`AmendRefusal`/`phase_view`). The obvious move is the same pattern for
plain commit — but amend's signing classification is coarse by design: a
single `SigningFailed` for "gpg failed to sign the data" (exact string) or,
guarded by a `signing_requested` probe, the ssh-format's generic "failed to
write commit object". That was the right call for amend's original scope,
but it throws away information `git commit` actually offers.

**Measured before writing any classifier** (scratch repos, git 2.43,
2026-08-18 — the standing "measure sandbox behaviour; never assert it"
rule from `docs/SECURITY_MODEL.md`, applied here to git's own behaviour
rather than the sandbox):

```
git -c commit.gpgsign=true -c user.signingkey=DOESNOTEXIST -c gpg.format=openpgp commit -m x
```

prints, verbatim:

```
error: gpg failed to sign the data:
gpg: skipped "DOESNOTEXIST": No secret key
[GNUPG:] INV_SGNR 9 DOESNOTEXIST
[GNUPG:] FAILURE sign 17
gpg: signing failed: No secret key

fatal: failed to write commit object
```

The `[GNUPG:] FAILURE sign 17` / `INV_SGNR` lines are GnuPG's own
machine-readable status-fd protocol — the exact one
`classify_sign_failure` already reads for `git tag -s`, because git's
`gpg-interface.c` invokes gpg with `--status-fd=2` for every signing
call, tag or commit, and captures the whole stream into its own stderr.
`git commit`'s signing path is not a different code path from tag
signing's; it is the same one. Discarding that precision to match amend's
older, coarser set would be throwing away a real distinction the wire
already carries for free: `17` is `GPG_ERR_NO_SECKEY` (no key — a config
fix), `77`/`78`/the `257..=281` libassuan range are agent/IPC failures
(the sandbox denying `gpg-agent`'s socket — not fixable by the user at
all, a different remedy sentence entirely).

The ssh-format signer (`gpg.format=ssh`) was also measured and, as
expected, carries no status-fd protocol on either path — ssh signing
doesn't speak it — so its one marker, `"failed to write commit object"`,
stays as generic as amend's own ssh leg and needs the same
`signing_requested` guard (the identical text is also plain git's
ordinary "couldn't write the object to `.git/objects`" disk-failure
message when nothing was configured to sign at all).

Also measured: `git commit`'s "nothing staged" case has **three** distinct
stdout shapes, not one — `"nothing to commit, working tree clean"`,
`"no changes added to commit"` (unstaged tracked changes exist), and
`"nothing added to commit but untracked files present"` — all on stdout
with a non-zero exit, never stderr.

## Decision

1. **A new `CommitFailureKind`** (`git-vista-protocol/src/dto.rs`), not a
   widened `AmendFailureKind` and not a reuse of `AmendFailureKind`
   itself: six variants — `SigningKeyMissing`, `SigningAgentUnavailable`,
   `HookRejected`, `HookTimedOut`, `NothingStaged`, `Other` — paired with a
   `CommitError { kind, message }` response DTO, same shape as
   `AmendCommitError`.
2. **Finer than amend on signing, on purpose.** `SigningKeyMissing` /
   `SigningAgentUnavailable` is the two-way split `SignTagFailureKind`
   already proved for signed tags, collapsed from four
   (`NoSecretKey`/`AgentUnreachable`/`GpgNotInstalled`/`Other`) to two
   because `GpgNotInstalled` and `AgentUnreachable` share one actionable
   answer from the user's vantage point on this server ("signing
   structurally cannot happen here" — the sandbox denies `gpg-agent`'s
   `AF_UNIX` socket identically to how it denies `ssh-agent`'s, #188).
   `AmendFailureKind::SigningFailed` is left exactly as it is — this ADR
   does not touch the amend route.
3. **`classify_commit_failure` reads the GnuPG status-fd protocol
   directly**, duplicating (not calling) `classify_sign_failure`'s parse.
   Calling that function directly was considered and rejected: its own
   empty-stderr fallback (`AgentUnreachable` when no status line and gpg
   is on `PATH`) assumes no hook-shaped alternative explanation exists,
   which is true for tag creation but false for commit — see point 4.
4. **Priority order resolves one genuine ambiguity, structurally, not by
   guessing.** A silently-rejecting hook (empty stderr, non-zero exit —
   the same documented shape `classify_amend_failure` already lives with)
   and a signing agent the sandbox stopped before it could write anything
   (also empty stderr) are indistinguishable from stderr alone when both
   preconditions hold at once — `commit.gpgsign=true` **and** a rejectable
   hook exists. Resolved in this order:
   1. Positive GnuPG status-fd evidence, checked **unconditionally** (not
      gated on the `signing_requested` probe) — a `[GNUPG:]` line can only
      be produced by a real signing attempt, and git's own commit sequence
      runs hooks *before* the object is written and signed, so a positive
      status line can never be a hook's doing.
   2. The ssh-format marker, gated on `signing_requested` (too generic to
      trust unconditionally).
   3. The hook-rejection heuristic (`classify_amend_failure`'s own,
      unchanged) — so an ambiguous empty stderr with *both* a hook present
      and signing requested is attributed to the hook: it is the earlier
      stage in git's sequence, and blaming a signer that structurally
      cannot have been reached yet sends the user to fix a configuration
      that was never consulted.
   4. Only then the sandboxed-signing-agent fallback (signing requested,
      empty stderr, no hook to explain it instead) — the production shape
      this server's sandbox actually produces.
   5. `NothingStaged`, checked **ahead of everything above** — an empty
      working tree can never be a signing or hook problem regardless of
      what else is configured, and matches on stdout, not stderr, across
      all three measured shapes (a translated-locale repository falls
      through to `Other`, same safe-direction residual as every other
      heuristic here).
   6. `Other` — never a canned substitute. #72's own explicit requirement:
      "an explicit unknown/passthrough arm that keeps the raw stderr" —
      unlike `SignTagFailureKind::Other`'s "see the server log", which
      would have silently regressed the wire's information content for
      the one arm the issue named by name.
5. **`message` is always git's own words**, for every kind, mirroring
   amend's posture (not the tag-signing route's canned-prose-per-kind
   posture): `commit_refusal_body(kind, stderr_stdout_or(&output, …))`.
   The "what to do about it" half of #72's "message says what happened
   AND what to do" requirement lives client-side
   (`commit_refusal_guidance`), the same split `phase_view`'s
   `AmendPhase::Refused` match already uses for amend — pulled out as its
   own named, host-tested function here per #72's explicit ask for "a
   pure, host-testable mapping function."
6. **The hook-timeout arm becomes typed too** (`CommitFailureKind::HookTimedOut`),
   closing the gap ADR 0058 left open for this route: that ADR gave the
   *amend* path's timeout a typed kind and left commit's as plain prose,
   deliberately, because commit had no typed contract to fit it into yet.
   Now that it does, the timeout arm's existing `hook_timeout_message`
   text is unchanged — only its wire wrapping moved from
   `(StatusCode::BAD_REQUEST, prose)` to
   `commit_refusal_body(HookTimedOut, prose)`.
7. **No route-level response relabeling needed**, unlike
   `amend_commit`/`amend_route_response`. `middleware::rewrap_error`'s
   #323 fix already sniffs any `(StatusCode, String)` body for a JSON
   *object* and passes it through `application/json`-labeled on any
   route — added after `amend_refusal`/`amend_route_response` were
   written, and already the pattern `sign_refusal_body` relies on.
   `commit_refusal_body` is one plain function, matching `sign_refusal_body`'s
   shape, not amend's two-function split.
8. **The wire contract is deliberately narrower than amend's.** Amend
   converted its handler-level validation refusals (empty message) to the
   same typed JSON, so *every* 400 from that route parses as
   `AmendCommitError`. This slice does **not** convert `create_commit`'s
   empty-message refusal or the branch-stub path's
   (`commit_empty_on_branch`) own compare-and-swap 400 — both stay plain
   prose. `CommitFailureKind`'s own doc comment and `CommitError`'s states
   this boundary explicitly, and the client's
   `classify_create_commit_response` falls back to `Unavailable` (never a
   guessed `Other` classification) for a 400 that fails to parse, so an
   unconverted refusal is never mistaken for a git-execution failure
   nobody classified.

## Alternatives considered

- **Reuse `AmendFailureKind` directly, one enum for both routes.**
  Rejected: amend's `StaleTip` and the compare-and-swap it names have no
  analogue on `CommitOnHead` (no CAS — HEAD is always the target), and
  collapsing signing to one `SigningFailed` throws away the precision
  `git commit`'s own stderr already offers for free. A shared enum would
  either grow `CommitOnHead`-only dead variants or force this route back
  down to amend's coarser signing split.
- **Collapse to amend's single `SigningFailed`, for consistency with the
  existing pattern.** Rejected: "do not invent a second scheme" (the task
  brief's own instruction) was read as *architectural* consistency —
  typed kind, git's own words in `message`, client-side guidance — not as
  a requirement to throw away a real distinction the wire already
  carries. The two-way signing split costs nothing (the classifier reads
  the same protocol either way) and buys a genuinely different remedy
  sentence for "no key" vs. "can't reach the agent at all."
- **Call `classify_sign_failure` directly instead of duplicating its
  parse.** Rejected: that function's empty-stderr-and-no-status-line
  fallback assumes no hook-shaped alternative exists, which is true for
  tag creation and false for commit (see Decision point 3/4). Reusing it
  as-is would misclassify the hook/signing collision case in signing's
  favor every time, sending users to fix a configuration that was never
  consulted.
- **Match `SignTagFailureKind`'s canned per-kind message, discarding
  git's raw stderr for classified kinds.** Rejected outright by #72's own
  text: "the unknown arm must preserve the original stderr — never lose
  information in the name of friendliness." Applied to every kind here,
  not only `Other`, for the same reason amend's own posture already does.
- **Fold `GpgNotInstalled` in as a third signing variant, matching
  `SignTagFailureKind`'s four-way split exactly.** Rejected: the issue's
  own minimum list names two signing arms
  ("signing key missing, signing agent unavailable"), and `GpgNotInstalled`
  shares `SigningAgentUnavailable`'s actionable answer on this server (gpg
  cannot function here regardless of which specific reason). A third
  variant would be precision with no distinguishable remedy behind it.
- **Skip typing the hook-timeout arm, leave ADR 0058's deferral in place
  for that one case.** Rejected once the typed contract existed anyway:
  leaving one refusal shape untyped while five others are typed would
  make the client's `classify_create_commit_response` guess wrong for
  exactly the shape ADR 0058 already wrote correct prose for — cheaper to
  finish than to leave half-typed.

## Consequences

- A signing failure, a rejected hook, an empty index, and a killed hook
  spawn are now four (five, counting the two signing kinds) distinguishable
  outcomes on the wire, each with its own client-side title and remedy —
  the criterion #72 named.
- `CommitFailureKind::Other` and the branch-stub / handler-validation
  paths are the two places a `/api/commit` 400 can still be untyped prose;
  both are documented on the DTO itself and handled by the client's
  `Unavailable` fallback, never guessed into a classification.
- The GnuPG status-fd parse now exists twice in `planner.rs`
  (`classify_sign_failure` and `classify_commit_failure`) rather than
  once behind a shared helper — a deliberate DRY trade against not
  touching `classify_sign_failure`'s already-proven, ADR-adjacent logic
  in a slice that doesn't need to. Revisit if a third consumer appears.
- Mutation-proof (`failure-atlas` `mutation_check`, `run_key`
  `m2.19-commit-errors-72`): the code-17 signing-kind mapping, the
  hook-before-signing-fallback ordering, the ssh-marker's
  `signing_requested` guard, `commit_refusal_body`'s stderr preservation,
  and the client's kind→refusal mapping are each individually proven
  `caught` — not merely covered by a green suite.

## Where this is implemented

- `CommitFailureKind`, `CommitError` — `crates/git-vista-protocol/src/dto.rs`;
  exported from `lib.rs`; pinned in `tests/dto_golden.rs`
  (`commit_error_signing_key_missing`, `commit_error_other`).
- `classify_commit_failure`, `commit_refusal_body` — `planner.rs`,
  immediately after `classify_amend_failure`. Wired into
  `exec_commit_on_head`'s failure arm and its (now typed) hook-timeout arm.
- `classify_commit_failure_covers_every_branch_with_paired_negatives`,
  `classify_commit_failure_distinguishes_no_secret_key_from_agent_unreachable`,
  `commit_refusal_body_never_swallows_the_unknown_arms_stderr` —
  `planner.rs`'s test module.
- The real HTTP-stack proof (mirroring ADR 0040/#323's amend proof) —
  `state.rs`'s `selection_flow_carries_mode_and_gates_writes`, the
  `/api/commit` section.
- `CommitRefusal`, `CreateCommitOutcome`, `classify_create_commit_response`,
  `commit_refusal_guidance` — `crates/git-vista/src/features/dialogs/commit.rs`,
  after `non_empty_or`; six tests in that file's own test module.
- `create_commit_request` — `crates/git-vista/src/api.rs`, now returning
  `CreateCommitOutcome` instead of `Result<(), String>`.
- The wasm commit dialog's error notice —
  `crates/git-vista/src/dialogs/commit.rs`'s `submit_commit`.

<!-- last_edited_by: max · last_edited_at: 2026-08-18T00:00:00-04:00 -->
