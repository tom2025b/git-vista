# Git-Vista iPad Design

Status: proposed

Git-Vista should not be a desktop Git UI made larger. Its primary interface is a
touch surface that may also have an Apple Pencil, trackpad, keyboard, split-screen
constraint, or external display. Every essential workflow must remain complete
with one finger.

## Interaction Principles

1. **Touch is complete.** Hover, right-click, and keyboard shortcuts are optional
   accelerators.
2. **Selection is not execution.** Selecting a commit, range, hunk, or rebase row
   never mutates a repository.
3. **Plans are tangible.** History-changing operations become editable visual
   plans followed by a review and explicit execute step.
4. **Context remains visible.** The current repository, worktree, branch, dirty
   state, and running operation remain visible during navigation.
5. **Density adapts to space, not device names.** Use container queries and input
   capabilities rather than an `iPad` user-agent branch.
6. **Precision is optional.** Pencil precision improves range and hunk selection,
   but no command requires a Pencil.
7. **Recovery is part of the interaction.** A completed destructive operation
   shows its result and available recovery action in place.

Use a minimum interactive target of 44 by 44 CSS pixels, adequate separation,
visible pressed/selected/focus states, and no destructive edge swipe.

## Adaptive Shell

### Wide Landscape or External Monitor

```text
+----------------+-----------------------------+----------------------+
| repositories   | history / status / diff     | inspector / plan     |
| and worktrees  |                             |                      |
|                | persistent action bar       | operation progress   |
+----------------+-----------------------------+----------------------+
```

- The left rail selects repository and worktree; it is not a file explorer.
- The center canvas owns graph navigation, status, or comparison.
- The inspector preserves selected-object context while the graph remains usable.
- A hardware keyboard may expose a command palette and shortcuts.

### Portrait

```text
+----------------------------------------------+
| repository / worktree / branch / sync state  |
+----------------------------------------------+
|                                              |
| primary graph, status, or diff surface       |
|                                              |
+----------------------------------------------+
| contextual action dock                       |
+----------------------------------------------+
| inspector or operation plan as bottom sheet  |
+----------------------------------------------+
```

The bottom sheet has detents for summary, half-height, and full-height content.
It must not cover the selected graph item without an obvious way to restore
context.

### Split Screen and Narrow Stage Manager Windows

- Collapse repository navigation into a modal sheet.
- Replace persistent inspector with a full-height sheet.
- Present one primary task at a time: graph, status, diff, plan, or result.
- Keep branch and dirty-state indicators in the compact header.
- Preserve selected commit, viewport, and unfinished form when the width changes.

### External Monitor

- Increase information density; do not simply scale controls and whitespace.
- Permit graph plus side-by-side diff plus inspector when width allows.
- Maintain touch-sized actions because the external display may still be touch
  adjacent or controlled from the iPad.
- Never move a critical confirmation to a different display unexpectedly.

## Input Model

Use Pointer Events as the unified input layer. Inspect `pointerType`, pressure,
tilt, and contact geometry only to enhance an interaction. Preserve a click,
touch, and keyboard path for the same command.

| Input | Primary behavior |
| --- | --- |
| One-finger tap | Select; a second explicit control opens or executes |
| One-finger drag | Pan graph or scroll a list; never reorder unless a handle is held |
| Pinch | Zoom graph around the gesture centroid |
| Long press | Open contextual actions with the selected object identified |
| Two-finger tap | Optional back/cancel accelerator; never the only path |
| Pencil tap | Precise selection of commit, connector, line, or hunk |
| Pencil drag | Select a range, annotate a teaching diagram, or reorder by handle |
| Trackpad/mouse | Hover previews and contextual menu as enhancements |
| Keyboard | Command palette, navigation, range selection, and shortcuts |

Pressure must never confirm, delete, push, or rewrite history. Pencil hover is
not available on all devices and cannot expose required information exclusively.

## Core Workflows

### Graph Navigation

- Tap a commit to select it and show a summary sheet.
- Expand the sheet for metadata, refs, changed files, and operation entry points.
- Pinch zoom changes semantic detail, not only geometric scale.
- A visible minimap or position rail supports very large histories.
- Branch/ref filters are chips in compact mode and a panel in wide mode.
- Range selection uses an explicit "Select range" mode with clear endpoints.

### Status and Partial Staging

Desktop checkbox lists are insufficient for touch. Use file cards with status,
summary, and an explicit stage control. Opening a file shows a virtualized diff:

- Tap a hunk header to select the hunk.
- Use Pencil or a range handle to refine to lines where Git permits it.
- Keep selected additions/deletions visually distinct from diff syntax colors.
- Show the generated patch preview before applying a partial stage/discard.
- Make discard harder than unstage and state exactly which content will be lost.

### Commit

- Keep staged scope visible beside or above the commit form.
- Preserve draft messages across suspension and layout changes locally, but do
  not sync them to other devices by default.
- Surface hooks and signing as operation progress rather than a frozen button.
- Make amend a distinct mode with the affected commit and force-push consequence
  visible before execution.

### Interactive Rebase

Represent rebase as a board of ordered commit cards. Each row has a large reorder
handle and an action selector for pick, reword, edit, squash, fixup, or drop.
Dragging only edits the plan. A review screen shows:

- Original and proposed topology.
- Branches and remotes affected.
- Likely conflicts and dirty-worktree blockers.
- The checkpoint/recovery reference.
- Whether a later force push may be required.

### Conflict Resolution

Avoid three tiny side-by-side editors in portrait. Present one conflict at a time
with base, ours, theirs, and result as selectable views. Provide whole-block
choices, then optional line-level editing. The continue/abort bar remains visible
and identifies the active merge, rebase, cherry-pick, or stash operation.

### Worktrees and Stash

Treat worktrees as first-class workspaces in the repository rail. A "switch task"
flow should recommend a worktree, commit, or stash based on the user's intent and
explain the tradeoff rather than hiding everything behind a generic stash action.

## Feedback, Motion, and Failure

- Use motion to preserve spatial context when opening an inspector or changing
  graph scale. Respect `prefers-reduced-motion`.
- Do not rely on platform haptics; browsers cannot provide consistent haptic
  feedback.
- Show optimistic selection but never optimistically report a Git mutation as
  complete.
- Running operations survive navigation. Progress belongs to a global operation
  center as well as the initiating screen.
- On reconnect, reconcile by operation ID and repository generation rather than
  replaying the last request.
- Restore viewport and selection after Safari suspends or reloads the PWA.

## Accessibility

- Expose the commit graph as both a visual canvas and a navigable semantic list.
- Describe parent relationships, refs, and current branch in accessible text.
- Provide non-color indicators for lanes, selection, staged state, and conflicts.
- Support Dynamic Type-like browser text scaling without clipping actions.
- Make every gesture workflow available through visible controls and keyboard.
- Announce operation start, progress changes, completion, and recoverable failure.
- Test VoiceOver with touch exploration; pointer support alone is not access.

## Browser and PWA Requirements

- Use safe-area insets and modern dynamic viewport units for installed mode.
- Handle `visibilitychange`, network loss, process restart, and stale service
  worker assets explicitly.
- Pin protocol compatibility between the cached frontend and running server.
- Keep app-shell caching separate from private repository data caching.
- Virtualize graph rows and diffs, bound decoded payloads, and avoid retaining
  entire repository histories in WASM memory.
- Test current Safari/iPadOS plus Chromium and Firefox desktop; do not depend on
  a WebKit-only feature for correctness.

## Validation Matrix

Every release candidate should cover:

- 11-inch iPad portrait and landscape with finger only.
- iPad split screen at narrow and medium widths.
- Stage Manager resize while a form and graph selection are active.
- Apple Pencil selection plus finger panning in the same session.
- Magic Keyboard and trackpad.
- External display at desktop width.
- VoiceOver, text zoom, reduced motion, and increased contrast.
- 200 ms latency through an SSH tunnel and a mid-operation disconnect.
- A large repository with long history, large diffs, and binary files.
- Offline (browser reports no network, or the tunnel dies mid-session): the
  browser's own network error surfaces (`network_error_text`), 22a's write
  guard refuses mutations before they hit the wire, and 22b's banner and
  disabled controls appear. Per ADR 0032, this is a test that the app **fails
  loudly** — not a test of a cached or offline-readable view, which the ADR
  rejects outright.

> **Note on #75's cache criteria.** #75's original acceptance criteria "Private
> diffs are not cached by default" and "Cache clear and export controls exist"
> are satisfied *vacuously* here: the frontend has no client-side cache of
> `/api` data (diffs, commits, graph payloads) to clear or export in the first
> place. Verified against `crates/git-vista/src` (2026-08-07, corrected count):
> `grep -rn "localStorage\|sessionStorage" crates/git-vista/src` turns up
> **three** `localStorage` entries plus `sessionStorage` use, not the two
> booleans this note previously claimed:
> - `prefs.rs` — two UI-preference booleans in `localStorage` (icon style,
>   per-node icons).
> - `prefs.rs` `INFLIGHT_REMOTE_OP_KEY` — a third `localStorage` entry (#232,
>   M2.20f): a small JSON record (`remote`, `branch`, merge `strategy`) naming
>   the one Fetch/Pull in flight, so a reload can resume tracking it. Not `/api`
>   response data and not a diff, but it is more than a UI toggle, so it belongs
>   in this inventory.
> - `features/dialogs/{signals,commit}.rs` and `core.rs` — commit-message
>   drafts in `sessionStorage` (#226), deliberately session-scoped rather than
>   `localStorage` so the draft dies with the tab.
>
> None of these cache `/api` diff or commit data, so the "private diffs are not
> cached" reading still holds. `grep -rn "IndexedDb\|caches\." crates/git-vista/src`
> turns up nothing. ADR 0032 forbids adding a cache of `/api` data, so that part
> is expected to stay true, not a gap to close.
>
> This is the narrower, ADR-consistent reading: ADR 0032 rules out a service
> worker and any "make offline look normal" outcome, but it does not
> unambiguously rule out every possible non-service-worker, application-level
> store. Whoever closes #75 (22d) should treat this as an explicit invitation to
> override — if the original intent behind those two criteria was such a store,
> say so and reopen the question rather than letting this reading stand by
> default.

## Reference

Pointer Events provide the cross-device pointer model, including touch, pen,
mouse, pressure, and tilt: <https://www.w3.org/TR/pointerevents3/>.

