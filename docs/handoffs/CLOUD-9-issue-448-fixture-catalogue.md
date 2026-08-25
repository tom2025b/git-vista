# Cloud handoff — #448, one fixture crate that is also the teaching material

**Written:** 2026-08-25 · **By:** max (CLI session on Tom's box) · **For:** a cloud Claude Code session on `tom2025b/git-vista`

> The largest job on today's menu and the cleanest cloud candidate: a new crate
> plus a mechanical migration, touching nothing that exists only on Tom's box.
> Budget roughly four hours. It is also the one most likely to expand — the
> scope fence below is not decoration.

---

```yaml
task_id: gv-448-fixture-catalogue
issue: 448
repo: tom2025b/git-vista
base: main
branch: feature/issue-448-fixture-catalogue
sign_commits_as:
  name: Claude_Max
  email: 262510778+tom2025b@users.noreply.github.com
sign_artifacts_as: max
new_crate: crates/git-vista-fixtures          # working name; the issue's
allowed_paths:
  - crates/git-vista-fixtures/**              # new
  - Cargo.toml                                # workspace members
  - crates/git-vista-server/**                # test modules only — migration
  - ci/browser/fixture.mjs                    # shell out to the Rust builders
  - docs/adr/
forbidden_paths:
  - design-docs/            # untracked here; not in your clone
  - handoff.md
  - crates/git-vista-server/src/planner.rs    # and its modules' NON-test code
acceptance:
  A1: one crate holds every fixture shape; no suite builds a repository by hand
  A2: every shape carries a doc comment written for a reader who does not already know
  A3: the browser harness invokes the Rust builders rather than reimplementing them
  A4: the 20 seeded_repo() call sites are migrated and the duplicate conflict builders are gone
  A5: test counts before and after are IDENTICAL — this is consolidation
```

## Why it is unblocked now

The issue says **"not before M4 closes"**, because this is a large mechanical
refactor across the server's test files with the same blast radius as the
`planner.rs` executor split, and two refactors of that size should not run in one
worktree at once. **M4 is now at 100%**, and the planner split has landed. The
gate is open.

## The counts, verified today rather than quoted

- **20** `fn seeded_repo` implementations, spread over 18 files, all under
  `crates/git-vista-server/src` (`planner/*_suite.rs` mostly, plus `history.rs`,
  `activity.rs`, `git_cmd.rs`, `handlers/read/status_suite.rs`,
  `handlers/read/content_suite.rs`, `handlers/conflicts.rs`).
- **2** independent `fn conflicted_repo` builders (`conflicts.rs`,
  `handlers/conflicts.rs`), plus conflict shapes built inline elsewhere.
- The browser harness already needed a **third** conflict fixture
  (`buildNonTextConflictFixture`, #432) because extending the second would have
  broken specs asserting an exact conflicted count. That is the drift the issue
  is about, already happening.

## The decision that is already made — do not reopen it

**Rust `std::process::Command` is the single implementation, and the browser
harness shells out to it** rather than building repositories in JavaScript.

Two implementations of "a repository broken in shape X" is the drift problem one
layer up. And drift between a *teaching* fixture and a *test* fixture is worse
than drift between two test fixtures: it means the thing being taught is not the
thing the code actually handles. That is the whole argument for this crate, so
implementing it twice would defeat it.

## The shapes

From the issue, several of which already exist and are only being moved:

```
seeded()                  three commits, one file — the baseline ~20 suites rebuild by hand
conflict_add_add()        both sides created it; no common ancestor
conflict_modify_modify()  all three stages present
conflict_delete_modify()  one side deleted, one edited
conflict_binary()         NUL bytes both sides; no line merge exists
sequence_mid_revert()     REVERT_HEAD on disk, conflicted
unrunnable()              git cannot execute (already in git_cmd.rs)
pathological_content()    hostile bytes (already in content_suite.rs)
path_battery()            hostile paths (already in content_suite.rs)
four_mode()               all four diff modes (already in content_suite.rs)
```

Add one more that this repository has proven it needs, because it was hand-built
twice in the last two days: **a repository whose `HEAD` resolves to nothing** —
a well-formed oid in `.git/HEAD` with no object behind it. No ordinary git
command produces that state; the fixture writes the file by hand. It exists today
as `buildBrokenHeadFixture` in `ci/browser/fixture.mjs` (#473) and as a demo repo
built ad hoc on Tom's box.

## The part that makes this worth doing twice over

**Each fixture's `//!` doc explains what is wrong, what git actually put on disk,
and why it matters** — written for a reader who does not already know. That
documentation *is* the teaching material, and because the lesson and the test
fixture are the same artifact, the teaching content cannot drift from what the
code really handles.

This is the objection that got #93 and #94 (an isolated Git simulator) cut: a
parallel fake Git is a second system to maintain, and it can teach something the
real product does not do. A catalogue of real repositories, broken in real ways,
has no such gap.

So A2 is not a documentation chore bolted on at the end. Write the doc first,
then the builder. If you cannot explain in plain words what is wrong with a
shape, the shape is not understood well enough to be a fixture.

## How to keep the blast radius honest

**A5 is the safety property, and it is checkable.** Record the test count before
you touch anything:

```
cargo test --workspace 2>&1 | grep -E "^test result:"
```

Keep that output. At the end, the totals must be **identical**. A changed count
means behaviour moved, and behaviour must not move in a consolidation. If a count
changes and you believe the new number is right, that is a finding to report —
not a thing to accept quietly.

**Migrate in slices, committing each.** Do not rewrite 18 files in one commit; a
green run tells you nothing about which slice broke when you have to bisect it.
A reasonable order: land the crate with `seeded()` and its tests → migrate the
`seeded_repo()` call sites → land the conflict shapes → collapse the two
`conflicted_repo` builders → the remaining shapes → the browser harness.

**Watch for fixtures that are not actually identical.** Twenty hand-rolled
`seeded_repo()`s have almost certainly drifted in small ways — a different file
name, an extra commit, a different branch name — and some suite is quietly
depending on one of those differences. When a migration turns a test red, the
answer is usually a *parameter* on the shared shape, not a special case; but if a
suite genuinely needs a different shape, give it its own named shape rather than
bending the common one.

**Commit identity in the fixtures themselves.** Every builder must set identity
per-invocation and never through repo or global config — this box has
repositories whose local `user.email` is a personal gmail address, and a bare
`git commit` picks it up. Copy the pattern already in `ci/browser/fixture.mjs`:

```
-c user.name=Claude_Max
-c user.email=262510778+tom2025b@users.noreply.github.com
-c commit.gpgsign=false
-c tag.gpgsign=false
```

with `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_SYSTEM=/dev/null` in the
environment. Deterministic timestamps are worth considering too: several suites
assert on ordering.

## Scope fence — what this is NOT

- **Not a Git simulator.** That was #93, it was cut, and the reasoning above is
  why it stays cut.
- **Not a behaviour change.** Nothing outside a test module changes. If you find
  a real defect while migrating, **file it** and name it in the PR; do not fix it
  here, where it would be invisible inside a large mechanical diff.
- **Not the planner.** `crates/git-vista-server/src/planner.rs` and its modules
  are off-limits outside their `#[cfg(test)]` blocks.

## House rules that bind this task

The repository's `CLAUDE.md` is tracked and you will have it. What differs in a
cloud session:

- **`buildlock` is a local wrapper that does not exist for you.** Run `cargo`
  directly.
- **Commit identity per commit** — the same `-c` pair as above, on the commit.
- **Branch → PR → merge, never delete a branch**, no force-push.
- **Write an ADR** under `docs/adr/NNNN-slug.md` (next free number — check, do
  not assume), add its row to `docs/adr/README.md`, sign it `max`. The decision
  worth recording is *one implementation, in Rust, shelled out to by the browser
  harness*, and why a JavaScript twin was refused.
  - Diagrams: every `classDef` that sets a `fill` must also set a `color`, and
    node titles use `<b>title</b>` in a plain label — never `**bold**` inside a
    backtick label, which ignores the class colour and renders unreadable on
    GitHub in dark mode.
  - If you cannot render the PDF twin into `docs/adr/pdf/`, say so; Tom's box
    will render it.
- **`design-docs/` is gitignored and not in your clone.** Do not create it.
- **There is no live server**, and the browser leg may not run in your
  environment (Node ≥ 20, a Chromium build under `~/.cache/ms-playwright`, and an
  unprivileged user namespace for `unshare --net`). **If it cannot run, say so
  plainly and do not call the gate green.** Whether it works in a cloud session
  is genuinely unknown here, so reporting either outcome is useful.

## What "done" looks like

A pushed branch and a PR saying `Closes #448`, whose body carries: the
before/after `test result:` lines side by side, the list of migrated call sites,
any shape that needed to stay special and why, any defect found and filed, and
which gate legs actually ran. If the job proves larger than the window, **land
the slices that are complete and say what is left** — a partial migration with an
honest boundary is far better than a half-migrated tree described as finished.

---

**Signed:** max · 2026-08-25T07:15:00-04:00
