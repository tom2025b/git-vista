# Performance budgets

Real, measured numbers behind acceptance criteria that would otherwise be
unfalsifiable prose ("large worktrees stay responsive," "the app feels fast").
Each entry states what was measured, on what, and a concrete number a future
change can be measured against — and can fail. Add a section per endpoint/
subsystem as they get their own criterion; do not fold this into
`docs/RELEASE_GATES.md` (which is about *which checks gate a release*, a
different question) or `docs/GIT_CLIENT_ROADMAP.md` (a milestone roadmap, not
a measurement log).

**Caveat that applies to every number in this file, stated once here rather
than repeated per section:** these are single runs on one development host
(4 cores, this box, debug/unoptimized build unless a section says otherwise),
not a statistically controlled benchmark suite — no warm-up runs, no repeated
trials, no variance reported. Treat every number here as "real and
reproducible," not "precise to the millisecond." A regression test derived
from a budget uses a generous multiple of the measured number specifically
because of this — see each section's own regression-test note.

## `GET /api/status/v2` (#68e, M2.15)

**What was measured.** The real handler seam
(`worktree_status_v2_for_repo` in `crates/git-vista-server/src/handlers/
read.rs`) end to end: the `git status --porcelain=v2 --branch -z` spawn, the
`-z` porcelain read through the capped primitive, `parse_porcelain_v2_z`
(#68b), and the full generation derivation (`read_generation_inputs`'s HEAD +
every ref + index walk, plus the sha256 digest of the read bytes, #68c). Not
just the git spawn alone — the generation derivation walks every ref in the
repository too, and isolating only the parse would have missed that as a real
cost center.

Reproduce with:

```
cargo test -p git-vista-server --bin git-vista-server -- --ignored --nocapture \
  large_worktree_responsiveness_ladder
cargo test -p git-vista-server --bin git-vista-server -- --ignored --nocapture \
  large_worktree_cap_boundary_in_file_count
```

(Both `#[ignore]`d — real file generation and repeated git spawns, no place
in every `cargo test`/CI run. See their doc comments in `read.rs`.)

### The ladder — worktree size vs. wall-clock time

Untracked files (`generate_untracked_files`), debug build, one run each, this
host:

| files changed | elapsed    | cap hit? |
| -------------: | ---------: | :------: |
|            100 |   4.413 ms |    no    |
|          1,000 |   8.963 ms |    no    |
|          5,000 |  17.800 ms |    no    |
|         20,000 |  64.864 ms |    no    |

Roughly linear in file count across two orders of magnitude — no evidence of
a superlinear cost center (e.g. an accidentally quadratic ref walk or parse
step) at these sizes. Untracked entries are the cheapest real record shape
(no mode/hash fields to compute per entry, unlike a staged or unstaged
change), so this ladder is closer to a best case than a worst case for a
given file count — a worktree with the same file count but every file
*modified* (carrying real mode/hash fields per porcelain record) would cost
somewhat more per file, not measured separately here.

### Where the 8 MiB read cap actually bites, in file count

Task 13 (#68c) chose an 8 MiB cap (`STATUS_V2_STDOUT_CAP`) and made a cap hit
a `413`, not a best-effort parse. Nobody had measured what worktree size
that actually corresponds to. Measured: **450,000 untracked files with
uniform 15-character names** (`bench-NNNNNN.txt`, `? ` + name + NUL = 20
bytes/record ⇒ ~8.6 MiB) reliably exceeds the cap — confirmed by
`large_worktree_cap_boundary_in_file_count` asserting the call is refused,
not corrupted-parsed. Total wall time for that run (dominated by creating
450,000 real files, not by `git status` itself) was **72.16 s**.

**This is a lower bound for this specific naming scheme, not a universal
constant.** A real worktree's actual paths are typically longer (directory
nesting, real filenames) than a 15-character uniform name, and every
non-untracked record (staged, unstaged, renamed, conflicted) carries mode and
hash fields the untracked shape doesn't — both push the real-world file count
that trips the cap *down* from 450,000, not up. Treat 450,000 as "the cap
does not bite for any worktree size anyone has actually filed an issue
about," not as "this is exactly where real users will hit it."

### Stated budget

**A `GET /api/status/v2` request against a worktree with up to 20,000 changed
files must complete in well under 1 second** on hardware comparable to this
host. The measured 64.9 ms at 20,000 files leaves roughly 15x headroom before
that budget — deliberately generous, not a tight fit to today's number, so
the regression test below doesn't flake on a loaded CI runner while still
catching a real regression.

**Regression test:** `worktree_status_v2_budget_holds_at_1k_files` in
`read.rs` (not `#[ignore]`d — runs in every `cargo test`) asserts 1,000
changed files complete inside **2 seconds** — roughly 220x the measured 8.96
ms at that size, chosen loose enough to tolerate a slow/loaded runner while
still catching an actual regression (e.g. the generation derivation's ref
walk becoming accidentally quadratic, which would show up as a
multi-second stall at a mere 1,000 files, nowhere close to this budget).
