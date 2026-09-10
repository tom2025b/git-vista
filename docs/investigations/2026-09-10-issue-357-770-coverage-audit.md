# #357 / #770 coverage audit — the consumer exists; the device and keyboard work stays owned

Reviewed against `6f8daad7` on 2026-09-10.

**Decision:** close [#357](https://github.com/tom2025b/git-vista/issues/357).
Its original no-consumer finding is fixed and browser-tested, and every design
residual that still requires work has an explicit open owner in
[#770](https://github.com/tom2025b/git-vista/issues/770). Closing #357 does not
claim that the touch/Pencil or arbitrary-line keyboard work is complete.

## Claim and residual ledger

| #357 claim or open question | Current disposition | Evidence or owner |
|---|---|---|
| `toggle_line`, `is_line_selected`, and `select_all_in_hunk` had no production consumer. | Fixed. The wasm staging view reads and toggles each line through `line_check`; Shift+Enter/Space on a roving-focused hunk calls `select_all_in_hunk`. The live `ViewerDoc::Staging` result arm calls `staging_body`, which renders `staging_patch_view`. | [`staging_view.rs`](../../crates/git-vista/src/features/diff/staging_view.rs), [`viewer.rs`](../../crates/git-vista/src/viewer.rs), and merged [PR #771](https://github.com/tom2025b/git-vista/pull/771). `reachability_census.rs` records why the three exemptions were removed. |
| The DOM wiring had no browser interaction proof. | Fixed. Chromium coverage drives the compiled application through pointer line toggles and both Shift+Activate keys, asserts visible and accessible state, and checks the exact multi-file preview plan. Its two harness self-checks and two rebuilt source mutations prove the assertions detect disconnected or weakened wiring. | [The landed browser investigation](2026-09-09-issue-357-ui-selection-test.md) and merged [PR #781](https://github.com/tom2025b/git-vista/pull/781). |
| What target or interaction should a finger use for a single ~18px diff line, given the 44px touch floor? | Open, owned by #770. Its first acceptance item requires a device-tested touch/Pencil affordance **or** a documented product decision that finger selection remains hunk-granularity. | [#770, “Touch/Pencil-specific affordances” and “Scope”](https://github.com/tom2025b/git-vista/issues/770). |
| Should Pencil use a distinct range gesture or the same precise line control, and should visibility be gated by pen versus touch? | Open, owned by #770. The issue names the distinct-affordance and pen/touch-gating questions and requires a real device in the loop. The current precise-pointer checkbox is not credited as device validation. | [#770, “Touch/Pencil-specific affordances”](https://github.com/tom2025b/git-vista/issues/770). |
| How can a keyboard or VoiceOver user toggle one arbitrary line while hunk-level roving already owns arrow keys? | Open, owned by #770. Its second acceptance item requires keyboard/VoiceOver access to a specific line and an explicit arrow-sharing design. The landed design proposal recommends one typed focus owner, but remains proposed rather than implementation or accessibility proof. | [#770, “Keyboard access to one arbitrary line by itself”](https://github.com/tom2025b/git-vista/issues/770) and [the design proposal](2026-09-09-issue-770-keyboard-model.md). |
| Must line selection survive row unmounting under virtualization? | No current work to transfer. `staging_patch_view` walks and renders all of `patch.lines()`; it does not use `detail.rs`'s windowed row path. #770 records this correction and says to reassess if staging later becomes windowed. | [`staging_view.rs`](../../crates/git-vista/src/features/diff/staging_view.rs) and [#770's correction](https://github.com/tom2025b/git-vista/issues/770). |

## Why there is no coverage gap

#357 was filed because the authorised per-line follow-up had disappeared from
the tracker and its model had no rendered consumer. PR #771 resolves that
concrete absence. PR #781 proves the new consumer through the real compiled
browser application. Those facts make #357's original no-consumer table stale,
but they do not finish the device- and keyboard-dependent design work.

#770 names that remaining work at the same boundaries #357 and PRs #771/#781
used when they deliberately stayed open:

- finger target/policy and Pencil affordance/gating, established on a real
  device; and
- arbitrary single-line keyboard and VoiceOver access, including ownership of
  the hunk-versus-line arrow-key model.

The virtualization question is not a third open deliverable on the current
surface. It was a conditional risk in #357, and inspection proved its premise
false for staging. Keeping #357 open would therefore add no owner or acceptance
criterion that #770 lacks; it would only duplicate the successor issue.

The mixed whole-hunk/line plan loss found while designing #770 also does not
create a gap. It was filed separately as
[#808](https://github.com/tom2025b/git-vista/issues/808) and is fixed on this
base: `DiffSelection::to_patch_plan` now expands a whole-selected hunk to its
complete changed-line set when a file must serialize as `Lines`, and refuses an
incomplete expansion. That defect is neither silently assigned to #770 nor a
reason to retain #357.

## What closure does not claim

This audit did not run a touch, Pencil, iPad, or VoiceOver pass and does not
choose #770's final interaction model. It did not rerun the browser suite or
exercise Apply against a real repository; it relies on the landed, mutation-
proved #781 record for DOM behavior and checks only that its production path
still exists at the reviewed base. It adds no UI and changes no production
behavior.

Reopen #357 only if its three selection APIs again lose every live production
consumer. New questions about touch/Pencil behavior or arbitrary-line keyboard
access belong on #770 while that issue remains open; later staging
virtualization should be assessed there rather than reviving the obsolete
no-consumer finding.

**Signed:** codex · 2026-09-10
