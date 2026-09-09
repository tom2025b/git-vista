# Per-line staging selection: browser interaction proof

Refs #357. Touch/Pencil and arbitrary single-line keyboard focus remain tracked
by #770. This delivery does not change either issue's state.

Base: `fb3708aa1a6756a5cae13c074e1585fbbb4b93ea`.
Branch: `test/357-ui-interaction-selection` (KEEP).

## Coverage

`ci/browser/tests/staging-selection.spec.mjs` adds three Chromium tests against
the compiled application, reached through the repository picker, Active mode,
HEAD context menu, and **Select Changes to Stage…**:

- Pointer clicks select and deselect added and removed lines independently,
  accumulate selections across hunks and files, and disable Preview/Clear when
  the last line is deselected.
- Shift+Enter and Shift+Space each select every changed line in the focused
  hunk, preserve that set on repeated activation, and allow a subsequent pointer
  click to deselect one line. Plain Activate still selects a whole hunk;
  Shift-Activate narrows that selection to explicit lines.
- Every state assertion checks all eight line buttons' `aria-pressed` values
  and visible checkmarks, including unselected siblings. Real Preview clicks
  must emit the exact file, hunk anchor, line indices, and `select: "lines"`
  shape. Context lines contribute to local indices but must never be selected.

The shared helper supplies a deterministic HTTP diff with two hunks in
`alpha.txt` and another in `beta.txt`, and stubs the preview response. Rendering,
input handlers, reactive selection state, action gating, and outgoing plan
serialization are the real wasm application. This is a UI wiring test, not a
test of Git's patch execution, and it never clicks Apply.

`harness-selfcheck.spec.mjs` adds two checks using the same assertions:
blocking a real line click must fail the exact state assertion, and stripping
Shift from activation must fail the exact preview-plan assertion. Setup is
outside the expected-failure catch, and each check requires its named assertion
failure rather than accepting any exception.

The browser job discovers these specs automatically. Its minimum executed-test
count rises from 120 to 125 for the five added tests; collection reports 135
tests in 34 files.

## Validation

Using `CARGO_TARGET_DIR=/home/tom/.cargo-targets/gv-357-uitest`:

```sh
cargo build --offline -p git-vista-server -p git-vista-fixtures
env -u NO_COLOR CARGO_NET_OFFLINE=true trunk build --config crates/git-vista/Trunk.toml
ci/browser/run.sh staging-selection harness-selfcheck hunk-keyboard
```

The build succeeds and the browser run passes **18 tests**, with no skips.
The new JavaScript files and modified self-check pass `node --check`;
`git diff --check` passes. This is the focused browser regression suite, not a
claim that the full repository gate was run.

## Source mutation recipes

Each mutation is applied alone to
`crates/git-vista/src/features/diff/staging_view.rs`, then the wasm bundle is
rebuilt before running the indicated browser test. The replacement must match
exactly once. A compilation or harness startup failure does not count as a
caught mutation.

1. Disconnect the pointer transition: replace
   `selection.update(|s| s.toggle_line(&file, anchor, local));`
   with `let _ = (&file, anchor, local);`.
   Run `ci/browser/run.sh staging-selection --grep 'pointer clicks'`.
2. Weaken Shift-Activate's enumeration: replace
   `s.select_all_in_hunk(&file, anchor, changed_lines.iter().copied())`
   with
   `s.select_all_in_hunk(&file, anchor, changed_lines.iter().copied().take(1))`.
   Run `ci/browser/run.sh staging-selection --grep 'Shift\+'`.

Both mutated bundles compiled successfully. The pointer mutation failed its
one test; the weakened enumeration failed both Shift+Enter and Shift+Space.
All three failures came from `#357 exact line selection and glyphs`, showing
unselected lines where the interaction required them selected. Both test
commands exited 1, so these are assertion failures rather than build failures.

The original Rust source was restored byte-for-byte and its bundle rebuilt
successfully. A read-only Git diff confirms no production change remains.
The final restored-bundle run,
`ci/browser/run.sh staging-selection harness-selfcheck --grep '#357'`, passes
**all five new tests**, with no skips. Local mutation build/test logs are in
`/tmp/gv-357-mutation-evidence/`.

Signed: codex · 2026-09-09
