# #459 — terminal working-tree status and staging decision log

**Status:** Implemented; verification complete
**Base:** `99d203faeeb11391db62036219673a0123eacd80`  
**Branch:** `feature/m10.04-459-terminal-status-staging`

## 2026-09-02 — existing boundaries verified

- The status read is `GET /api/status/v2`, returning the shared
  `WorktreeStatus` vocabulary: changed entries with staged/unstaged/both
  sides, rename-with-optional-further-edit, untracked, ignored, conflicted,
  submodule flags, and binary flags.
- A failed status read is a third state, not “clean”. The terminal keeps the
  last successful rows and reports the refusal separately, matching the
  shell's existing stale-data posture and the repository's `third-state-*`
  fixes (failed observation must not collapse into an affirmative answer).
- Activating a repository must first `POST /api/select` in Active mode.
  History reads can address a `?repo=` directly, but all staging and generic
  plan routes intentionally use the authenticated session's selection.
- Whole-tree stage/unstage and guarded discard use the generic pair:
  `POST /api/plan` with a typed `GitOperation`, show the returned `Plan`, then
  submit that exact value to `POST /api/execute-plan` only after approval.
- File/hunk/line staging uses the already-shared partial-staging path:
  `GET /api/staging/diff`, construct a `PatchPlan` from the served generation
  and parsed coordinates, `POST /api/staging/preview`, show its exact patch
  and whole-file pathspecs, then submit the unchanged `PatchPlan` to
  `POST /api/staging/apply` only after approval. The apply handler builds
  `GitOperation::StageSelection` and enters the shared planner; the TUI never
  constructs git argv.
- `git diff` does not contain untracked paths, so the existing partial-staging
  vocabulary cannot truthfully preview a single untracked file. This slice
  will offer untracked content through the existing whole-tree `StageAll`
  operation and will not invent a second staging implementation.

## 2026-09-02 — terminal interaction

- The unused `Branches` placeholder becomes `Working Tree`, the pane #459
  was reserved to fill. It shows the same five browser sections in the same
  priority order: conflicted, staged, unstaged, untracked, ignored.
- `Enter` on a staged/unstaged tracked row opens the corresponding shared
  staging diff (`index → HEAD` for unstage, `worktree → index` for stage).
- `Space` previews the selected file in Working Tree, or the selected
  file/hunk/changed line in the staging diff. `a` previews stage-all or
  unstage-all according to the selected status section.
- `d` on an eligible tracked row first opens a destructive confirmation whose
  copy names the path and says its uncommitted work will be permanently lost.
  Confirming that guard only asks the server to build a plan; the returned
  destructive plan still requires its own explicit approval before execution.
- `y` approves the currently visible confirmation/review, `n` or `Esc`
  refuses it. No write key directly emits an execution request.

## 2026-09-02 — first wired checkpoint

- Repository activation is deliberately sequenced: `POST /api/select` must
  answer before History and Status requests are dispatched. The reducer test
  pins the order and the transport test pins Active mode, cookie, and CSRF.
- Preview and execution are separate reducer states. Refusing while a preview
  request is outstanding invalidates its late answer; refusing after an
  approved execution has already been submitted does not lie that it was
  cancelled and instead tells the user to wait for the outcome.
- Generic writes use an idempotency key derived from the server Plan's
  operation hash and issue time. Patch writes use UUIDv5 of the serialized
  shared PatchPlan, so retrying the same approved selection is stable while a
  different selection cannot collide by reusing a constant key.
- `cargo test -p gv-tui --bins`: **90 passed, 0 failed, 0 ignored**. This is
  the binary target containing the implementation, not the empty-library
  false green.

## 2026-09-02 — mutation proof

The mutation workspace was `/tmp/gv459-mutations.g1SUT4`, with its own
`target/`. Before every mutant, the complete committed tree was restored from
`git archive HEAD`; each result below is an assertion-level targeted-test
failure, not a compile failure. The first attempt restored only the file under
mutation and was discarded when cross-file residue was noticed.

| Invariant test | Mechanism removed | Mechanism weakened |
|---|---|---|
| repository selection sequencing | Select became immediate Status | post-select Status follow-up omitted |
| keyboard reachability/scope | staging Space made inert | all-tree `a` accepted in every pane |
| every status state | conflicts dropped | unstaged half of `Both` dropped |
| failed-status third state | prior rows cleared | failure labelled Ready |
| stable status order | sorting removed | path-only sort ignored section priority |
| file/hunk/line plan shapes | changed-line targets removed | hunk ordinals shifted by one |
| stage/unstage direction propagation | all partial plans forced to Stage | file only forced to Stage |
| context-line inertness | all context selectable | one context prefix selectable |
| pinned file containment | first file accepted without comparison | `.txt` extension treated as identity |
| whole-tree review gate | returned Plan discarded | Unstage mapped to StageAll |
| discard guard | eligibility guard inverted | permanent/uncommitted wording removed |
| file preview gate | pending-file preview dropped | file coerced to first hunk |
| cancellation of pending previews | pending marker retained on cancel | PatchPreview accepted without marker |
| full review visibility | serialized Plan omitted | exact patch replaced by byte count |
| preview generation equality | equality check removed | only `diff-v1:` prefix compared |
| shared read routes | `/api/select` removed | status sent to legacy route |
| exact typed write transport | operation sent instead of Plan | constant idempotency key reused |
| terminal rendering | Working Tree draw removed | Main rendered only a Plan summary |

Result: **36/36 mutants RED across 18 invariants, exactly two independent
breaks each**. Direction propagation became its own invariant after the audit
caught that the first version of the plan-shape test exercised only Stage.

## Final acceptance ledger

1. **MET — status list shows the browser-equivalent states, including third
   states.** The terminal reads the shared DTO at
   `crates/gv-tui/src/data.rs:167`; its five sections and action directions
   are defined at `crates/gv-tui/src/panes/worktree.rs:19`, and every shared
   entry variant (including both-sided and renamed-then-edited duplication,
   submodule, and binary detail) is projected at
   `crates/gv-tui/src/panes/worktree.rs:169`. Loading/Ready/Failed is retained
   at `crates/gv-tui/src/panes/worktree.rs:90` and failures preserve the last
   snapshot at `crates/gv-tui/src/panes/worktree.rs:119`. The actual Ratatui
   buffer proves all five visible at `crates/gv-tui/src/ui.rs:862`.
2. **MET — stage/unstage whole tree, file, hunk, and line through shared
   planning, never raw git argv from the TUI.** Keyboard routes are at
   `crates/gv-tui/src/keys.rs:43`; whole-tree operations enter `BuildPlan` at
   `crates/gv-tui/src/app.rs:916`; file/hunk/line selections become the shared
   `PatchPlan` shapes with pinned generation and direction at
   `crates/gv-tui/src/panes/staging.rs:79`. Preview and approved submission use
   only `/api/plan`, `/api/staging/preview`, `/api/execute-plan`, and
   `/api/staging/apply` at `crates/gv-tui/src/data.rs:185`. The existing apply
   handler enters the planner's single funnel at
   `crates/git-vista-server/src/handlers/staging.rs:98`. The crate-wide guard
   forbidding production process spawn (`Command::new`) is at
   `crates/gv-tui/src/main.rs:377`.
3. **MET — guarded discard retains its guard and says exactly what is lost.**
   Eligibility, path validation, the permanent-uncommitted-work warning, and
   first confirmation are at `crates/gv-tui/src/app.rs:937`; approval then
   requests a server Plan rather than executing at
   `crates/gv-tui/src/app.rs:978`. The two-ceremony test is at
   `crates/gv-tui/src/app.rs:1684`.
4. **MET — every write plan is visible before it runs.** Returned Plans and
   PatchPreviews become review state only at `crates/gv-tui/src/app.rs:610`;
   full Plan/PatchPlan JSON, whole-file pathspecs, exact patch bytes, and the
   explicit approval line render at `crates/gv-tui/src/app.rs:1099`. Only the
   unchanged value held by Review can become an execution request at
   `crates/gv-tui/src/app.rs:978`, and Main prioritizes that review surface at
   `crates/gv-tui/src/ui.rs:216`. Full-field/exact-byte evidence is at
   `crates/gv-tui/src/app.rs:1858`; stale preview generation refusal is at
   `crates/gv-tui/src/app.rs:1904`.
