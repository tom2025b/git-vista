# #770: single-line keyboard selection in the staging viewer

Status: proposed, design only. Author: codex · 2026-09-09.
Inspected base: `4d4132d9b226b3b494137b78f1271094ebe6a72b`.
Branch: `docs/770-keyboard-design` (KEEP).
Refs [#770](https://github.com/tom2025b/git-vista/issues/770),
[#357](https://github.com/tom2025b/git-vista/issues/357), and
[#210](https://github.com/tom2025b/git-vista/issues/210).
This proposal closes none of them. Touch/Pencil affordances remain deferred,
without a design recommendation or a claim of device verification.

## Recommendation

Keep Up/Down as hunk-to-hunk navigation at a header. Right enters that hunk's
changed lines; Up/Down then moves among those lines; Left or Escape returns to
the owning header. Implement this hierarchy with **one typed focus owner**,
not two independently engaged roving scopes. Enter/Space toggles the focused
selection target, and moving focus never changes the accumulated selection.

This is a controlled version of the nested-navigation option, not a claim
that nesting disappears because the state is represented by one enum. It adds
a navigation level, with an explicit entry/exit contract and a discoverability
cost. It does not add a selection mode: checkboxes and the existing selection
remain available throughout.

The deciding local constraint is that today's arrows traverse only hunk
headers, including across files. Keeping that efficient path lets a user
collect hunks, refine one hunk to lines, then continue collecting. A flat list
can support the same accumulating selection, but changes that existing arrow
stride on every hunk containing changed lines. Multi-select alone does **not**
rule out flattening; preservation of the existing navigation is why I prefer
the hierarchical version here.

## What the source actually does

All source statements below refer to the inspected base, not to presumed
current behavior on another branch.

- [`staging_view.rs`](../../crates/git-vista/src/features/diff/staging_view.rs):
  `hunk_row` gives one header `tabindex="0"`, drives `GraphFocus`, and calls
  `focus_hunk` after a move. Header clicks only focus; the adjacent button
  toggles the whole hunk. Enter/Space on the header also toggles the whole
  hunk; Shift+Enter/Space calls `select_all_in_hunk`. `line_check` already
  renders a native button with `aria-pressed`, but every line button has
  `tabindex="-1"` and a generic accessible name. Its click calls `toggle_line`.
- [`focus.rs`](../../crates/git-vista/src/features/a11y/focus.rs) owns one
  contiguous index space, clamps at either end, and distinguishes the
  remembered Tab stop from engaged focus. It knows no parent/child relation.
  [`graph/core.rs`](../../crates/git-vista/src/features/graph/core.rs)'s
  `roving_row_key` recognizes Up/Down, Home/End, Enter/Space and Escape.
  Ctrl/Meta/Alt make it yield; Shift is allowed. Left/Right are not row
  intents. Thus #210 does not already arbitrate a line level: neither its
  index set nor its key map describes one.
- `staging_patch_view` walks **all** `patch.lines()` into one `collect_view()`;
  its navigable coordinates come from `selectable_hunks` and
  `selectable_hunk_lines`. There is no viewport window in this path. Verified
  at `4d4132d9`: #210's historical unmount-on-scroll problem is not a blocker
  for staging. Separately, [`detail.rs`](../../crates/git-vista/src/detail.rs)
  uses `accessible_rows_window` and already contains a reveal/focus/animation
  frame path at this base. This proposal does not infer its current defect
  status from the older issue comments. Fully mounted staging still needs
  focus lifecycle handling when a fetch or a view replacement removes nodes.
- [`selection.rs`](../../crates/git-vista/src/features/diff/selection.rs)
  keeps selections across hunks and files. `toggle_line` adds/removes an
  explicit line, but on a whole-selected hunk it narrows to **only that line**.
  `is_line_selected` reports explicit lines only: whole-hunk selection does
  not check every line button. `select_all_in_hunk` replaces that hunk with
  its explicit changed-line set and is idempotent.
- `staging_body` builds Preview and Apply plans from `DiffSelection`.
  Selection changes hide stale preview content by comparing plans. Despite
  the source's “Preview → Apply” shorthand, `core::staging_actions` does
  **not** require a fresh preview before Apply: both require a nonempty
  selection, identity and an idle request state. Preserve this behavior;
  navigation must neither send requests nor introduce a new approval rule.
  [`viewer.rs`](../../crates/git-vista/src/viewer.rs) owns selection and
  preview signals outside rendering and clears them when a staging fetch
  starts, including on Stage/Unstage switches.

## Options and their costs

| Option | Reaching an arbitrary changed line | Benefit here | Cost here |
| --- | --- | --- | --- |
| Two nested roving scopes | Enter a hunk's line scope; use its arrows; exit to header | Preserves current hunk arrows | Two focus models need arbitration, synchronized tab stops, return focus and reset handling; independently active handlers can both consume a key. |
| Flattened single cursor, following gv-tui | Up/Down through file/header/line rows | One ordering and no entry gesture; easy traversal across hunk boundaries | Ordinary hunk navigation must now cross line rows. A hunk-skip command needs another documented binding. File actions and nonselectable rows also need a deliberate browser policy. |
| **One owner with explicit Hunk/Line target (recommended)** | Header Right → line arrows → Left back | Keeps hunk stride and has only one active key dispatcher and one patch Tab stop | Still hierarchical navigation. Needs new pure state and bindings; line traversal clamps within the hunk, so crossing hunks takes an exit and re-entry. |
| Every line checkbox in normal Tab order | Tab repeatedly to a line | Native button activation; little custom focus logic | Adds potentially thousands of Tab stops, defeats the existing one-stop patch contract and makes reaching surrounding actions expensive. |

The recommended option shares the first option's navigation semantics. Its
advantage over two scopes is reduced synchronization, not fewer concepts for
the user. A modifier-only line traversal alternative is less attractive:
Ctrl/Meta/Alt are explicitly reserved by the shared policy, and Shift+arrows
already move normally. Repurposing either would change another local contract.

### What to borrow from gv-tui, and what not to copy

[`gv-tui/src/panes/staging.rs`](../../crates/gv-tui/src/panes/staging.rs)
flattens file, hunk, changed-line, context and metadata rows. A row has an
optional `Target`; `plan_for_row` produces one file, one hunk, or one line's
plan. Context and metadata can occupy cursor positions but cannot produce a
plan. The pane is the projection, not the keyboard controller:
[`app.rs`](../../crates/gv-tui/src/app.rs) owns the pane cursor and
`preview_selection`, while [`keys.rs`](../../crates/gv-tui/src/keys.rs) maps
Space to `PreviewSelection`. Space requests a preview for the current row;
approval later submits the reviewed plan. Enter on an open staging pane does
not select a line. “Immediate single-row action” therefore means immediate
plan/preview construction from one target, **not immediate application**.

Borrow its explicit target coordinates and single cursor ownership. Do not
copy `plan_for_row` as browser activation: selecting line A, then line B in
another hunk/file must retain A, and a later Preview must include both. The
browser already has the separate state needed for that in `DiffSelection`.
Even if Tom chooses the flattened alternative, its activation must still
call that model rather than replace it with the cursor's singleton plan.
Keep the staging raw-walk coordinates: copying gv-tui's parser projection
wholesale would change a contract `staging_patch_view` explicitly preserves.

## Proposed keyboard and focus contract

“Line” below means a mapped added/removed body line. Context, file metadata,
combined headers/bodies and EOF markers remain readable text, outside the
roving target set. File headings gain no new whole-file action in this lane.
The hunk's changed lines are sorted by raw display order; their selection
coordinates retain local indices that count context but exclude EOF markers.

| Key | Hunk target | Line target |
| --- | --- | --- |
| Up / Down | Previous / next hunk across the patch | Previous / next changed line in this hunk |
| Home / End | First / last hunk in patch | First / last changed line in this hunk |
| Right | First changed line in this hunk; stay if none | Stay |
| Left | Stay | Owning hunk header |
| Enter / Space | Existing whole-hunk toggle | Existing `toggle_line` for this line |
| Shift+Enter / Shift+Space | Existing explicit select-all in hunk | Explicit select-all in owning hunk; keep line focus |
| Escape | Existing disengage and blur; consume the event | Owning header; consume the event |
| Tab / Shift+Tab | Leave the patch by ordinary document order | Leave the patch by ordinary document order |

Moves clamp, never wrap. Shift+movement behaves like movement, without range
selection, as today's modifier policy does. Ctrl/Meta/Alt combinations remain
unhandled, including for new Left/Right bindings. Right on a line and Left on
a header are handled no-ops, so horizontal arrow behavior does not depend on
text width. This is a proposed extension, not something `roving_row_key`
already implements. Put its interpretation in a staging-specific pure helper
that reuses `roving_row_key` and `KeyMods::bails_out`; do not change graph or
blame bindings globally.

Proposed state: a remembered `Hunk(hunk_idx)` or
`Line { hunk_idx, local }` target plus engaged state, with one ordered hunk
table and its changed-line coordinates. These are new types/transitions.
Existing `GraphFocus` can supply movement concepts; it does not already
provide this hierarchy. A single dispatcher resolves a key against the typed
target. The hunk header and line buttons are sibling rendered elements today;
do not assume DOM ancestry supplies parent navigation.

Exactly one header **or** line button in a nonempty patch has `tabindex="0"`.
When lines own it, all headers have `-1`; inactive line buttons have `-1`.
No separate hunk Tab stop remains active. Tab exit preserves the remembered
target; re-entry resumes it. Actual focus leaving the patch disengages it.
Click/focus on a line button updates that target; click on header text returns
to `Hunk` without changing selection. The hunk checkbox still toggles its
selection; if it receives pointer or assistive-technology focus, synchronize
the owning hunk and give its keyboard events the same hunk semantics without
adding a second Tab stop. Do not forcibly move focus during Tab exit.

Use the existing native line button as the focused control. Ensure a keyboard
activation invokes one transition, not both a custom keydown toggle and a
browser-generated click. Handle Enter/Space keydown once with default
activation suppressed; retain click for pointer/assistive activation. Consume
handled events locally so the window-level overlay Escape handler cannot
also dismiss the viewer. The current header already uses undelegated keydown
with `prevent_default` and `stop_propagation`; carry that boundary to lines.

Expose a visible keyboard hint and matching accessible description for entry,
exit and select-all. Give each line button an identifying name/description:
file, hunk, added/removed, old or new line number as appropriate, and line
text. Derive those numbers from the same raw hunk walk; a local index alone
is not a source line number. Announce whether the hunk is whole-selected or
uses explicit lines, and explain that selecting a line in a whole hunk narrows
to that line. Keep the actual `aria-pressed` tied to explicit line selection.
Use a visible focus outline distinct from the selected checkmark/background.

The [W3C keyboard-interface guidance](https://www.w3.org/WAI/ARIA/apg/practices/keyboard-interface/)
supports separating focus from selection and managing a composite's Tab stop.
It does not certify this custom hierarchy or its VoiceOver behavior. Keep
native buttons; do not declare a tree, listbox, grid or application role merely
to persuade assistive technology to pass arrows through. Test the resulting
semantics with VoiceOver before claiming #770's accessibility acceptance.

Own staging focus above rendering in `viewer.rs`, separate from the hunk
signal used for other viewer document types. Reset to the first valid header,
disengaged, on a fresh staging fetch/direction/repository context. Clear
selection and preview at the existing boundary. Never reuse a local index
across generations or steal focus back from an action button after a fetch.
Within the same diff, selection/preview changes must preserve the focused
node or restore its target after replacement, only if focus was still inside
the patch. An empty target list contributes no Tab stop. Focus the mounted
target and verify it is revealed in the scrolling viewer, including long
patches; the absence of virtualization is not itself a visibility test.

## Selection semantics: required integration work

Concrete refinement with the existing browser fixture: focus alpha's first
hunk, Shift+Space selects local lines `[1, 2, 4, 5]`; Right enters local `1`;
Down reaches `2`; Space removes it, leaving `[1, 4, 5]`. Left returns to the
header; Down then Right reaches the next hunk's local `1`. Space adds that
line while preserving the first hunk's set. The same accumulation crosses
files. A plain whole-hunk toggle followed by a line toggle has different,
existing semantics: only the toggled line survives in that hunk. Entry,
exit and arrows never implicitly convert a whole hunk to explicit lines.

There is also a real serialization gap at this base, independent of navigation.
`DiffSelection::to_patch_plan` switches a file to `SelectionShape::Lines` if
any selected hunk uses explicit lines, then filters out whole-selected hunks
in that file. The test
`to_patch_plan_drops_whole_hunks_mixed_into_a_line_selected_file` explicitly
pins this loss; its “line selection is unwired” explanation is now stale.
Thus selecting whole hunk 0 and one line of hunk 1 in alpha currently produces
a plan containing only hunk 1, although hunk 0 remains checked. Neither a flat
cursor nor a nested cursor fixes this.

This is already reachable with today's mouse controls: click hunk 0's
checkbox, then a line checkbox in hunk 1 of the same file. It affects both
Preview and Apply; preview comparison cannot detect omitted intent when both
paths use the same lossy builder. It deserves separate defect tracking even
if #770's keyboard work is postponed. The #770 handoff comment will flag it;
this documentation lane does not silently treat it as a future-only concern.

Treat preserving mixed selections as an implementation prerequisite, not an
already solved property. Recommended repair: let plan construction enumerate
the complete changed-line set of each whole-selected hunk **when its file must
serialize as Lines**, preserving explicit subsets and other files. Keep the
whole/explicit distinction in UI state and the existing header toggle rules;
pure whole-hunk files continue to serialize as Hunks. Feed plan construction
verified coordinates from the pinned patch through a pure API, rather than
guessing line counts inside `DiffSelection`. Preview, stale-preview comparison
and Apply must all use that same lossless builder.

The raw walk does not currently expose a completeness flag. It must verify a
hunk's declared old/new counts have been satisfied before claiming its full
changed-line set is known. For an incomplete/truncated hunk or another
unrepresentable conversion, refuse the mixed plan with an explanatory message;
do not silently omit it or pretend the visible subset is the whole hunk.
Global `d.truncated` alone does not identify which earlier hunks are complete.
Add plan-build-error handling to the action gate while keeping Clear usable.
The existing wire shape suffices; this is client-side plan construction work,
with server round-trip validation needed to establish semantic equivalence.

That feasibility claim is grounded in
[`patch_plan.rs`](../../crates/git-vista-protocol/src/patch_plan.rs)'s
`Lines { hunks: Vec<HunkLines> }` and
[`patch_build.rs`](../../crates/git-vista-protocol/src/patch_build.rs)'s
`append_file_patch_lines`, which validates and emits each selected hunk.
The existing
[`hunk_staging_suite.rs`](../../crates/git-vista-server/src/planner/hunk_staging_suite.rs)
test `a_multi_hunk_line_level_selection_stages_both_hunks_lines` exercises
multiple explicit hunk subsets. It does not prove the proposed client-side
whole-to-lines conversion; that still needs the acceptance checks below.

This prerequisite is broader than just key wiring, but follows directly from
promising accumulating multi-select. Tom should confirm its delivery scope
(same implementation PR or a prerequisite PR). Shipping new navigation while
claiming lossless mixed accumulation would be inaccurate at this base.

## Implementation map (no implementation in this PR)

Every existing path below was read from this worktree. Proposed new code can
live in the existing pure modules; no new production file is assumed to exist.

| File | Required work if this recommendation is accepted |
| --- | --- |
| `crates/git-vista/src/features/diff/core.rs` | Add typed staging navigation state, target ordering, entry/exit, pure key interpretation using the shared policy, and reset/clamp transitions. Extend the existing raw walk with completeness and accessible line-location derivation; expose complete changed-line sets for plan construction. Add host tests here or in a new sibling suite. |
| `crates/git-vista/src/features/diff/staging_view.rs` | Replace staging's hunk-only focus wiring with the one-owner dispatcher; give existing line buttons roving tabindex, identifying labels, focus tracking and exact-once activation. Add hints, selection-mode descriptions, local Escape handling and focus reveal. Feed coordinates into the shared lossless plan builder and surface conversion errors. Correct the module's keyboard deferral text after implementation. |
| `crates/git-vista/src/viewer.rs` | Own a separate staging navigation signal above render closures; reset it at the staging fetch/context boundary and pass it through `staging_body`. Preserve focus on same-diff updates without pulling it back from toolbar actions. |
| `crates/git-vista/src/features/diff/selection.rs` | Add coordinate-aware lossless mixed-plan construction and replace the test that expects whole hunks to be dropped. Retain existing toggle/narrow/select-all semantics; correct stale comments claiming line selection has no consumer. |
| `crates/git-vista/styles.css` | Give the existing line control a distinct focus indication and style keyboard hints/state descriptions. This is no decision about new touch geometry or pen gating. |
| `crates/git-vista/src/features/graph/core/keyboard_suite.rs` | Update the staging consumer census if it now calls a staging helper; continue proving that helper consumes `roving_row_key` and the common modifier policy. Keep graph/blame behavior intact. |
| `crates/git-vista/src/features/diff/core/staging_actions_suite.rs` | Cover conversion-error gating if the action-gate API is extended; Clear must remain available. |
| `ci/browser/tests/staging-selection.mjs` | Reuse the two-file, three-hunk fixture and exact plan assertions; adapt selectors to identifying line names. Extend the helper for Unstage and incomplete/long patches. |
| `ci/browser/tests/staging-selection.spec.mjs` | Add the keyboard-only journeys and focus/serialization assertions below while retaining the existing pointer and Shift+Activate coverage. |
| `crates/git-vista/src/features/a11y/audit.rs` | Check the control census if markup adds targets. Existing line buttons are already listed separately from 44px hunk targets; keyboard reachability is no basis for claiming touch verification. |

`features/a11y/focus.rs`, `features/graph/core.rs`, `detail.rs`, `gestures.rs`
and gv-tui supply patterns/contracts to preserve; no global key remapping or
TUI change is required. No protocol/server change is proposed. Read their
current contracts again when implementation starts if the base has moved.

## Acceptance and unresolved decisions

Implementation validation must prove:

- Pure target transitions have one Tab owner, clamp at boundaries, skip
  unselectable text, preserve local coordinates across context/EOF markers,
  honor all modifiers, handle empty hunks and reset across generations.
- In the compiled browser, enter via Tab, reach one chosen added and removed
  line without pointer assistance, select/deselect each exactly once, exit
  to its header, navigate across hunks/files, Tab out and resume the remembered
  target. Assert actual focus and all tab indices, not just the pure cursor.
  Escape from a line reaches its header without closing; header Escape keeps
  its existing blur behavior. Pointer/assistive focus must synchronize the
  owner without double activation.
- Focus moves send no staging requests and leave selection unchanged. Exact
  Preview payloads retain multiple explicit selections and mixed whole/line
  selections within one file, in both Stage and Unstage directions. A source
  or behavior mutation that drops a selected hunk must fail these assertions.
  Refuse incomplete mixed conversion with a visible reason and working Clear.
- Existing Shift+Activate remains idempotent; header and pointer behavior
  retain their current meaning. Selection changes invalidate old preview text.
  Clear, preview completion, long-patch scrolling and fresh diff loads do not
  strand focus on body or silently reuse a removed line coordinate. Validate
  the mixed plan against server patch preview for complete fixture hunks.
- A real VoiceOver pass checks control naming, spoken selection state,
  discoverability and whether arrow delivery/activation works with the chosen
  interaction. DOM and Chromium tests cannot establish that result.

Tom's decision: accept the explicit navigation level and proposed bindings,
or prefer a flat list despite the change to hunk-arrow stride; also assign the
mixed-plan repair's delivery scope. The pair review below adds a third explicit
decision: retain today's whole-to-one-line activation semantics, or separately
change refinement for all input methods. These are product/scope decisions, not
claims of missing state-machine feasibility. My recommendation is the one-owner
hierarchy plus the lossless builder prerequisite. Device-dependent VoiceOver
behavior remains unverified. Touch/Pencil hit areas, gestures and visibility
gating remain wholly deferred to #770's device work.

## Pair check and validation of this proposal

The requested `claude -p` review completed with exit 0. The initial sandboxed
invocation emitted no output and was terminated; a retry outside the sandbox
with a 180-second limit returned the review. That does not establish the cause
of the first invocation's silence.

Claude's verdict: the recommendation follows from the source, with no
asserted existing mechanism it could not find. It verified the hunk-only
arrow stride, missing hierarchical state, fixture coordinates, mixed-plan
loss, preview gating, missing completeness flag, fetch reset of selection,
and the accessibility census. It judged the accumulating-selection versus
single-row-preview distinction honest, specifically accepting that the TUI
does not immediately apply. Its concerns and my disposition are:

- **The mixed-plan defect was understated.** Claude pointed out that it is
  reachable by mouse today and deserves its own issue. I agree; the selection
  section now spells out that reproduction and its impact on both requests.
  The issue comment will surface it. The builder repair remains a prerequisite
  whichever keyboard option is chosen.
- **The proposed refinement gesture can surprise users.** Header Space,
  Right, Down, Space narrows to one line instead of selecting everything
  except that line. Claude suggested expanding the whole hunk to explicit
  changed lines before toggling. I agree this is a substantive alternative,
  not something an announcement alone resolves. I retain the existing
  `toggle_line` contract in this recommendation to keep keyboard and pointer
  activation equivalent. Changing it should cover both inputs and their
  checked-state semantics together, using the complete coordinate set; making
  only keyboard Space subtract from an implicit whole selection would give
  the same line button two selection meanings. Tom must accept the existing
  narrowing behavior or choose that broader refinement change before wiring.
  This is an unresolved product disagreement, not a source-reading dispute.
- **Line buttons read unpressed inside a whole-selected hunk.** Claude
  confirmed the proposal describes this honestly, but stressed that the
  accessibility acceptance cannot follow from the design alone. Agreed:
  identifying labels and a spoken hunk-state description are proposals, and
  VoiceOver validation remains outstanding. The refinement alternative would
  also need a coherent change to effective checked-state reporting.

Claude explicitly did not verify the sentence about Enter in the TUI staging
pane. I independently checked it: `keys.rs::dispatch` maps Enter to Activate,
and `app.rs::activate` returns no requests for Main when staging is present.
It also observed that today's shared viewer hunk focus is only re-clamped,
not reset at a staging fetch; the proposed separate state addresses that.

After review, the additions clarify the live-defect reproduction and record
the disagreement; they do not change the recommended navigation. Twelve
relative source links and ten existing implementation-map paths were checked
for existence. Staged whitespace checks pass, and the change is confined to
this document. No production code or browser test was changed or run by this
design-only lane. The review is source analysis, not a device or implementation
test.

Signed: codex
