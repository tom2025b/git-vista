# Issue #770: keyboard access to one changed line, and the permanent touch/Pencil decision

2026-09-11. Lane: `feat/770-line-keyboard`, worktree `/home/tom/projects/gv-770`.

## What #770 asked, and what this covers

#770 (M2.17g) was filed as a `#357` follow-up naming two things left open:

1. A touch/Pencil-specific affordance for per-line selection.
2. Keyboard access to **one specific changed line**, distinct from
   `select_all_in_hunk` (the existing Shift+Enter/Space path onto every
   changed line in a hunk, wired by #357).

This lane builds (2) and permanently closes (1) as out of scope for this
install. Both decisions are recorded here.

## Decision 1: touch/Pencil affordances are permanently out of scope

Tom's own words, this session: *"i dont want to do the ipad stuff i dont
have and apple pencil for one thing and i use the app on linux."*

This is not deferred, not parked as a future issue, and not "later" — it is
a standing decision for this install: there is no iPad and no Apple Pencil
available to build or test against, and the app is driven on Linux. Building
a touch/Pencil-specific affordance without a device in the loop would be
guessing, not implementing — exactly the reasoning #215 itself gave for
splitting Pencil-specific work out in the first place (see
`crates/git-vista/src/features/diff/selection.rs`'s module doc, "Pencil-
specific surface deliberately left for a follow-up issue"). That reasoning
does not change by writing more prose at it; it changes only if a device
becomes available, which is Tom's call to make, not a lane's to assume.

No new issue is filed for this. If a device becomes available later, that is
a fresh decision, not a resumption of a parked one.

## Decision 2: keyboard access to one line — the nested-focus-scope design

### The tension #770 named

#357 gave every hunk header a roving-tabindex focus
(`crate::features::a11y::focus::GraphFocus`, #210) with a Shift+Enter/Space
shortcut onto `select_all_in_hunk` — every changed line in that hunk, at
once. It deliberately left keyboard access to *one arbitrary line* unwired,
because, in its own words, that "would need its own roving focus nested
inside the hunk-level one #210 already owns... a design surface in its own
right, not a wiring gap." #770 restates the same tension explicitly: "How
does per-line selection interact with the roving hunk focus (#210)? Both
want arrow keys."

Two arrow-key-driven collections — the list of hunk headers, and (inside
whichever hunk is focused) the list of that hunk's own changed lines — but
only one input event stream to drive them from.

### Prior art considered: `gv-tui`'s flattened cursor

The issue names `gv-tui`'s `panes/staging.rs` as its own prior art. Read
before designing anything (per this lane's brief): it solves an analogous
problem by *flattening* file/hunk/line into one `Vec<Row>` with a single
cursor — `ArrowUp`/`ArrowDown` walk the whole list, whatever row is under
the cursor is what Space/Enter acts on.

That shape does not transplant cleanly onto the web staging view, for three
reasons specific to this surface:

- The terminal pane is one column, one gesture per row, executed
  immediately. The web view is a rendered patch — hunk headers already page
  through the *whole* diff at one elevation while lines are nested markup
  *inside* each hunk's own block. Flattening would turn every context line
  (already excluded from `changed_by_hunk`) into a stop a keyboard user has
  to tab past to reach the next hunk, which #210 never had and this issue's
  own acceptance criteria do not ask for.
- `select_all_in_hunk` already treats "the hunk" and "the changed lines in
  the hunk" as two distinct granularities worth their own affordances, not
  one flattened list a user arrows straight through — a flattened cursor
  would blur that distinction the wire format (`SelectionShape::Hunks` vs
  `SelectionShape::Lines`) itself keeps apart.
- `gv-tui`'s pane owns its own terminal frame; the web staging view is
  rendered inside `crate::viewer`'s existing overlay, sharing focus/DOM
  concerns (Escape-closes-overlay, roving Tab stops) that a full re-flatten
  would have to re-litigate.

### The resolution: a nested focus scope

`ArrowRight` on a roving-focused hunk header drills into that hunk's own
line scope; `ArrowLeft` or `Escape` leaves it, handing arrow keys back to
the header exactly where #210 left it. This is the WAI-ARIA treegrid
pattern (Right expands/enters a nested level, Left collapses/exits it),
applied here instead of invented from nothing.

While the line scope is engaged:

- `ArrowUp`/`ArrowDown` move a **new**, separate cursor
  (`crate::features::diff::selection::LineFocus`) among only that hunk's
  changed lines. `GraphFocus` (the header-level roving position) is never
  consulted and never moves.
- `Home`/`End` jump to the first/last changed line in the hunk.
- `Enter`/`Space` calls `toggle_line` on the one focused line — a third,
  genuinely distinct action from `toggle_hunk` (whole hunk, `Enter`/`Space`
  at header level) and `select_all_in_hunk` (whole hunk's changed lines,
  Shift+`Enter`/`Space` at header level).

Arrow keys therefore belong to exactly one scope at a time. They never
fight over the same keypress, and the two collections (header list, one
hunk's line list) stay two separate, independently host-tested state
machines rather than one that has to reason about both.

### Why a new type instead of extending `GraphFocus`

`GraphFocus` is deliberately flat and reused identically by three unrelated
callers (the canvas, blame rows, the hunk-header list itself) — see its own
module doc. Bolting a second, hunk-scoped cursor onto it would make every
other caller carry state that means nothing to them. `LineFocus`
(`crates/git-vista/src/features/diff/selection.rs`) is standalone: it holds
only the hunk index it is drilled into (`None` when disengaged) and a line
cursor, driven by the same `FocusMove` vocabulary `GraphFocus::mv` already
uses, so a caller already familiar with that type's `Home`/`End` clamping
gets the identical shape here.

### Where the decision logic lives, and what stays wasm-only

Per this repo's own "pure core vs. view" pattern (`cargo test` never
compiles anything behind `#[cfg(target_arch = "wasm32")]`), the actual
focus-scope state machine — `enter`, `exit`, `mv`, clamping, refusing to
engage an empty hunk — lives in
`crates/git-vista/src/features/diff/selection.rs`, a plain host-compiled
module, alongside `DiffSelection`. Only the DOM glue (which element gets
`tabindex="0"`, calling `.focus()`, reading `KeyboardEvent`) lives in
`crates/git-vista/src/features/diff/staging_view.rs`, which is wasm-only and
carries no decision logic of its own — it asks `roving_row_key` (the
existing single source of truth for the arrow/Home/End/Enter/Escape map,
shared with the canvas and blame rows, #653/#660) what a key means, and asks
`LineFocus` what that means for the line cursor. `ArrowLeft` is the one key
`roving_row_key` has no opinion on (nothing else it drives has a nested
scope to leave), so it is decided locally in `staging_view.rs` under the
same `KeyMods::bails_out()` policy every other roving key already uses.

A repo-wide test (`features::graph::core::keyboard_suite::the_staging_view_
asks_core_what_a_row_key_means`) already enforces that `staging_view.rs`
calls `roving_row_key` rather than re-deriving the arrow map — this design
was built to satisfy that test, not around it; the first draft of the line
scope's keydown handler matched `ev.key()` directly against
`ArrowUp`/`ArrowDown`/`Home`/`End`, spelling `FocusMove::Prev` etc. by hand,
and that census test caught it immediately (see the mutation-adjacent
finding below).

### A real, disclosed limitation: `line_focus`'s lifetime

`hunk_focus`/`selection` (the header's `GraphFocus` and the `DiffSelection`)
are created in `crates/git-vista/src/viewer.rs` and threaded into
`staging_body`, specifically so an in-flight Preview/Apply and the current
selection survive a re-render of the staging body (e.g. after the diff
resource refetches). `viewer.rs` is outside this issue's `allowed_paths`
(owned by another surface of the app, not this lane), so `line_focus` is
instead created **locally**, inside `staging_body`, with
`create_rw_signal(LineFocus::new())`.

Consequence: `line_focus` resets to disengaged on every fresh
`staging_body` call — for example, right after `Apply` succeeds and the
diff resource refetches — where `hunk_focus` and `selection` do not reset
the same way. This is an acceptable loss for ephemeral keyboard focus (it
carries no user intent to stage anything, unlike `selection`), but it is a
real, deliberate gap, not an oversight papered over: threading `line_focus`
through `viewer.rs` the same way `hunk_focus` is threaded is a genuine
follow-up for whoever owns that file.

## Mutation proof

`failure-atlas`'s `mutation_check` MCP tool was not available in this
session's toolset (no MCP tools were exposed to this lane at all — only
Read/Edit/Write/Bash). Rather than skip the mutation-proof requirement, it
was performed manually but to the same standard the tool enforces: mutate
the committed `HEAD` state, run the real test suite, confirm the specific
test goes red for the specific reason, then restore byte-for-byte and
reconfirm the full suite is green again. No git write was used for the
restore (a diff-verified `cp` back to a pre-mutation backup), so the
worktree's committed state was never at risk.

Two mutations, on two different mechanisms, both in
`crates/git-vista/src/features/diff/selection.rs`'s `LineFocus`:

1. **Removed the mechanism.** `enter`'s `if changed_len == 0 { return false;
   }` guard was deleted, so entering a hunk with no changed lines would
   silently succeed. `cargo test -p git-vista --bins line_focus` went red on
   exactly `line_focus_enter_refuses_a_hunk_with_no_changed_lines`
   (`assertion failed: !f.enter(2, 0)`), all six other `line_focus_*` tests
   still green. **Caught.**
2. **Weakened a condition.** `mv`'s `Next` arm and its trailing `.min(last)`
   clamp were removed, so the line cursor could walk past the end of a
   hunk's changed lines. The same test run went red on
   `line_focus_mv_clamps_at_both_ends_without_wrapping`
   (`left: Some(3), right: Some(2)`), a different test than mutation 1's,
   confirming the clamp assertion — not some other assertion that happened
   to also fail — is what caught it. **Caught.**

After each mutation the file was restored from a pre-mutation copy and
`cargo test -p git-vista --bins` (the full 1099-test host suite, not just
the `line_focus` filter) was re-run to confirm no residual damage: `test
result: ok. 1099 passed; 0 failed; 2 ignored`, matching the committed
`HEAD` exactly (`git diff` empty, `git status` clean against the commit).

**Note for whoever adjudicates this PR:** the two repo-wide census tests
this lane's changes also had to satisfy —
`reachability_census::every_declared_fn_has_a_real_consumer_or_is_exempt`
(which flagged `LineFocus::is_engaged` as dead code until the hunk header's
own `tabindex` closure was given a real dependency on it, fixing a genuine
double-tab-stop bug in the process) and
`features::graph::core::keyboard_suite::the_staging_view_asks_core_what_a_
row_key_means` (which flagged the first draft's hand-rolled `FocusMove`
match) — are themselves a form of mutation-adjacent proof: they caught real
defects in this lane's first draft before any deliberate mutation was run.
Both are cited from the actual failing test output captured during this
session, not pasted from memory.

## Verification performed

- `cargo test -p git-vista --bins` (host, `--bins` not `--lib` — this crate's
  real target is the `git-vista-ui` binary; `--lib` silently compiles and
  tests nothing): 1099 passed, 0 failed, 2 ignored.
- `cargo build -p git-vista --target wasm32-unknown-unknown`: clean, the new
  DOM wiring in `staging_view.rs` compiles for the real target it ships to.
- `cargo clippy -p git-vista --bins -- -D warnings` and `cargo clippy -p
  git-vista --bins --target wasm32-unknown-unknown -- -D warnings`: both
  clean.
- `cargo fmt -p git-vista`: applied (one formatting fixup after adding the
  header's `tabindex` guard).

## Not verified from this box

No browser is available in this environment (matching #210/#226/#242's own
disclosed pattern, restated in `staging_view.rs`'s module doc). Specifically
unverified here:

- That `ArrowRight`/`ArrowLeft` on a real hunk header and line checkbox
  actually move DOM focus the way `focus_hunk`/`focus_line`'s
  `query_selector` + `.focus()` calls assume.
- That the reactive `tabindex` closures re-render promptly enough in a real
  browser for a screen reader to announce the new tab stop.
- Any VoiceOver/screen-reader behaviour at all.

This is the same gap #210 and #357 already carried and disclosed, not a new
one introduced here — queued for the same eventual testbed pass the rest of
this issue's DOM wiring is queued for.

## Does this close #770?

#770 as filed asked for two things: touch/Pencil affordances, and keyboard
access to one line. This lane builds the second in full and records a
**permanent** decision (Tom's own words, not a deferral) that the first is
out of scope for this install — there is no device to build or test it
against, and there will not be one while this install is Linux-only. A
permanent, explicit "not doing this, here is why" is a real answer to that
half of the issue, not an open item left dangling. On that basis this PR
should close #770 — see the PR body for the actual closing keyword and the
same reasoning stated there for whoever lands it to check independently.

**Signed:** Claude Sonnet 5 · 2026-09-11
