# CLOUD-2 — cold build measurement

**Status:** design + first measurement, taken 2026-08-23 in a cloud container.
**Branch:** `claude/cold-build-measurement-design-1lyhj4`
**Artefacts:** `ci/cold_build_measure.sh` (the harness), a new section in
`docs/PERFORMANCE_BUDGETS.md` (the number), this file (the reasoning).

> `/design-docs/` is gitignored as "session artifacts, not product docs"
> (`.gitignore` line 71). This file is force-added, following the precedent of
> `design-docs/2026-08-18-wf-78-map-results.md`, which is tracked under the same
> rule. Once tracked, later edits need no `-f`; the ignore only governs
> untracked files. Move it under `docs/` if the reasoning is judged durable
> enough to be product documentation.

---

## 1. The claim being replaced

`dev`, line 614, in the cold-build branch of `cmd_testbed`:

```
echo "dev: so every dependency compiles from scratch. Expect 10-25 minutes."
```

That is the only estimate the project has for a from-scratch build, it sits in
front of the one build a human is explicitly asked to wait through, and it has
never been measured. A range that wide cannot be contradicted by any outcome —
7 minutes and 40 minutes are both "roughly what it said" to a reader who is
already committed to waiting. It is the same species of unfalsifiable prose
that `docs/PERFORMANCE_BUDGETS.md` opens by naming ("large worktrees stay
responsive," "the app feels fast") and exists to replace.

Two recent changes make the gap worse rather than academic:

- **#331** (`985dc77`, "testbed builds follow the main tree onto the scratch
  SSD") moved where a cold testbed build lands. Its rationale in `dev` cites a
  measured *disk* number — 5.8 GB across two ports on the system disk — but no
  *time* number, so the change's effect on the 10-25 minutes is unknown in both
  direction and size.
- The testbed cold build runs **two** builds into one `CARGO_TARGET_DIR`
  (`trunk build`, then `cargo build -p git-vista-server`; `dev` lines 621-624).
  The wasm32 half is not a rounding error on the native half — measured, it is
  the *larger* half (§6), and 51 packages are compiled in both. Any estimate
  derived from watching `cargo build` alone understates it by more than 2×.

## 2. Why this is a cloud-session task

A cold cache is destroyed by measuring it, and it cannot be cheaply restored.
On the development box the only way to get back to cold is to delete a warm
`target/` that costs the very 10-25 minutes under investigation to rebuild —
so the measurement charges the developer twice and takes the machine out of
service while it runs.

A fresh cloud container arrives cold for free. `~/.cargo/registry` is absent,
`target/` does not exist, and nothing is lost when the container is reclaimed.
That is the entire argument for doing this here: the scarce resource is a cold
cache attached to a machine nobody is waiting on, and a cloud session is the
only place the project has one.

## 3. The one-shot constraint, and what it forces

Because the container is cold exactly once, there is no "re-run with better
instrumentation." Everything the measurement will ever need has to be captured
in a single pass. This is not a detail of the harness; it is the reason the
harness exists at all rather than a human running `time cargo build` and
reading the terminal:

- numbers go to a **report file**, not to stdout for someone to catch as it
  scrolls;
- each phase is timed **separately**, because a single total cannot be
  decomposed after the fact;
- `cargo build --timings` runs on the expensive phase, so the follow-up
  question "what actually dominates this?" has an answer that does not require
  a second cold cache to obtain.

## 4. Three different things are called "cold"

They cost differently, and conflating them is the specific way this
measurement can be wrong while looking right.

| tier | `~/.cargo/registry` | `target/` | pays for |
| --- | --- | --- | --- |
| **C0** | absent | absent | crate download **and** compile |
| **C1** | warm | absent | compile only |
| **C2** | warm | warm | only what changed |

**`dev testbed` on a fresh port is C1, not C0.** The development box has been
building this workspace for months; its registry is warm, and only the
per-port target directory (`testbed_target_for`, `dev` lines 511-519) is empty.
A C0 total reported as "what the testbed costs" folds in a network download the
testbed never pays.

The harness therefore times the fetch separately from the compile and **derives
the tier from what it observed** rather than from a flag the operator sets, so
a report cannot claim a coldness its own preflight contradicts.

As it turns out (§6) the fetch is small enough on this host that C0 and C1 are
within noise of each other — but that is a *measured result*, not an assumption
the design was allowed to make.

## 5. Anti-vacuity

The failure mode is a number recorded from a run that quietly reused a cache:
fast, green, and meaningless. Two guards, neither decided by the build itself:

1. **State is observed, not asserted.** The pre-run condition of `target/` and
   `~/.cargo/registry` is recorded as fact and the tier derived from it. A warm
   `target/` aborts the run outright — a warm host cannot measure a cold build,
   and the honest response is to refuse rather than to produce a number.
2. **Every compile phase counts cargo's own `Compiling <crate>` lines** and
   fails below a floor of 50. A warm rebuild prints **zero** of them, so a
   phase that reused a cache cannot produce a passing record. The floor is set
   far below any real cold count (Cargo.lock declares 405 packages) and far
   above the zero that a warm run yields, so it distinguishes the two states
   without being sensitive to which crates are in a given phase's graph.

**Both guards were proven to fire**, 2026-08-23, against this tree:

- run with `target/` present → refused, tier recorded as `C2-warm-target`;
- guard 1 deleted, run on the now-warm tree → the native phase **succeeded**,
  `rc=0`, in 17 seconds, having compiled 4 crates — and the floor refused it.

The second is the one worth reading. Without the floor, that run yields a
green, plausible-looking "17 seconds" for a phase that did almost nothing.
A cold-build number is not distinguishable from a warm one by its own success,
which is precisely why the guard cannot be the build's exit code.

The `--offline` flag on the compile phases is the third guard and is not a
speed trick: it is the proof that the fetch phase covered the whole graph. If
anything still needed the network, the compile phase fails rather than silently
folding a download into a number labelled "compile."

## 6. Measured

### The host

One cloud container, 2026-08-23: 4 × Intel Xeon @ 2.10 GHz, 15.7 GiB RAM,
29.8 GiB free disk, rustc/cargo 1.98.0, trunk 0.21.7. `~/.cargo/registry` and
`target/` both absent at start, so the harness derived **tier C0** — the
strictly more expensive tier.

### The phases

| phase | what it is | wall-clock | crates compiled | `target/` after |
| --- | --- | ---: | ---: | ---: |
| fetch | `cargo fetch --locked`, 405 locked packages | **6 s** | 0 | 0 |
| native | `cargo build -p git-vista-server` (debug, offline) | **39 s** | 178 | 771 MiB |
| wasm | `cargo build -p git-vista --target wasm32-unknown-unknown` | **49 s** | 176 | 1.88 GiB |
| trunk | `trunk build` — wasm-bindgen, hashing, `dist/` | **6 s** | 0 | 1.90 GiB |
| **total** | | **100 s (1.7 min)** | **354** | **1.90 GiB** |

Cargo's own totals agree with the harness's wall-clock (`Finished ... in
38.73s` and `in 48.79s` in the phase logs), so the timing is not an artefact of
how the harness brackets the commands.

The run is not vacuous by its own guards: 178 and 176 `Compiling` lines against
a floor of 50, a 146 MB `target/debug/git-vista-server` produced, and 39
distinct `gix-*` crates plus `tokio`, `axum` and `serde` named in the native
log. A cache reuse cannot produce any of that.

### Three things the numbers actually say

**1. The estimate in `dev` is wrong by roughly an order of magnitude — on this
hardware.** 1.7 minutes against "10-25 minutes". This does **not** license
editing that line (§7, §8): the container has a fast virtual disk, and #331's
comment records that the box the estimate describes was writing testbed builds
onto a spinning `/dev/mapper/vgmint-root`. A cold build that writes 1.9 GiB is
plausibly disk-bound there and CPU-bound here, which is enough to explain the
whole gap without either number being wrong. What the measurement establishes
is that the range is untethered from any observation, not that it is too high.

**2. The wasm32 half is the larger half — 49 s against 39 s, 56% of compile
time.** Any estimate formed by watching `cargo build -p git-vista-server` and
assuming the frontend is a trailing detail understates the testbed's cold build
by more than a factor of two. `dev` runs `trunk build` *first* (lines 621-624),
so an operator who watches the first phase finish and extrapolates is
extrapolating from the more expensive one — the error is at least in the
forgiving direction.

**3. 51 packages are compiled in both phases.** Comparing the `Compiling`
lines by name-and-version, 51 of the native phase's 178 and the wasm phase's
176 appear in both — roughly 29% of each graph, paid twice into the same
`CARGO_TARGET_DIR`. The two halves are therefore mostly *disjoint* (axum/gix on
one side, leptos on the other) rather than one graph built twice. The
duplicated 51 are what a shared target dir does not save; the mechanism is not
isolated here (target triple and per-phase feature unification would both
produce this signature) and would need its own measurement to attribute.

### Disk

`target/` reaches **1.90 GiB** and `~/.cargo/registry` **478 MiB**. Both are
per-testbed-port under `testbed_target_for`, so #331's measured 5.8 GB across
two ports is the same order of magnitude as two of these plus whatever
`cargo test` adds on top — consistent, not confirmatory, since that figure was
taken on a different tree state.

### Raw report

```
schema=1
host_nproc=4
host_mem_kib=16461084
host_disk_avail_kib=31279604
rustc=rustc_1.98.0_(88d9e12ae_2026-08-18)
cargo=cargo_1.98.0_(797e8a9bc_2026-08-05)
trunk=trunk_0.21.7
lock_packages=405
pre_registry=absent
pre_registry_bytes=0
pre_target=absent
tier=C0-no-registry-no-target
cargo_incremental=0
fetch_rc=0
fetch_seconds=6
fetch_compiled_crates=0
fetch_target_kib=0
native_rc=0
native_seconds=39
native_compiled_crates=178
native_target_kib=789448
wasm_rc=0
wasm_seconds=49
wasm_compiled_crates=176
wasm_target_kib=1975372
trunk_rc=0
trunk_seconds=6
trunk_compiled_crates=0
trunk_target_kib=1992664
total_seconds=100
total_minutes=1.7
final_target_kib=1992664
```

### C0 versus C1, now measured rather than assumed

The fetch phase is **6 s of 100** — 6% of the total. On this host the
distinction §4 was careful to preserve turns out to be nearly free, so the C0
figure above is usable as a C1 figure to within 6 seconds. That is a property
of this container's network, not a general result: the separation stays in the
harness because a host on a slow link would make the same conflation cost
minutes rather than seconds, and the harness cannot know in advance which host
it is on.

## 7. What is deliberately NOT measured here

Stated so the numbers above are not read as covering more than they do.

- **The development box.** Every number here is from a 4-core cloud container.
  It is *not* a replacement for `dev`'s "10-25 minutes", because that line
  describes Tom's machine on its own disk, and substituting a cloud number for
  a claim about different hardware is exactly the dishonest move §4 exists to
  prevent. See §8.
- **`--release`.** The testbed builds debug on purpose (`dev`'s own comment:
  "this build exists to be driven by a human for a few minutes"). Release
  carries `lto = true` and `codegen-units = 1` (root `Cargo.toml`) and would be
  substantially slower; that is a separate measurement with a separate purpose.
- **`cargo test --workspace`.** Test builds compile dev-dependencies and test
  harnesses that a plain `cargo build` never touches, so the CI gate's cold
  cost is strictly higher than the figure here and is not derivable from it.
- **Variance.** One run, one host, no warm-up, no repeated trials — the caveat
  at the top of `docs/PERFORMANCE_BUDGETS.md` applies verbatim, and applies
  harder here, because unlike the millisecond-scale entries in that file this
  one *cannot* be repeated on the same machine.
- **Disk placement.** #331's move onto the scratch SSD is a claim about I/O on
  a specific box with two specific disks. A container with one virtual disk
  cannot speak to it either way.

## 8. Next steps

- [x] Harness written and self-checking (`ci/cold_build_measure.sh`)
- [x] First cold measurement taken, C0, cloud container
- [x] Number recorded in `docs/PERFORMANCE_BUDGETS.md`
- [ ] **Run `ci/cold_build_measure.sh` once on the development box.** This is
      the measurement that can actually replace `dev`'s "10-25 minutes", and it
      is now a one-command job. It must be run at **C1** (registry warm,
      `target/` moved aside — `mv target target.warm`, not deleted, so the warm
      cache is restored by moving it back rather than rebuilt). The harness
      refuses to run with `target/` present, which is what forces that to be a
      deliberate act.
- [ ] Only after that: replace the `10-25 minutes` string in `dev` with the
      measured figure plus a pointer to the budgets entry. Not before — see §7.
- [ ] Optional, and the reason `--timings` is captured: if the per-crate
      breakdown shows one or two dependencies dominating, that is an actionable
      finding about the dependency graph rather than about build machinery.

## 9. Reproducing

```
ci/cold_build_measure.sh [report-path]
```

Requires a host with no `target/` (the script refuses otherwise). Writes
`key=value` lines to the report — default `cold-build-report.txt` — and leaves
cargo's per-crate breakdown in `target/cargo-timings/`. `trunk` is optional;
its phase records itself as skipped when the binary is absent, and is
non-fatal when it fails, so a blocked egress cannot throw away compile numbers
that a one-shot cold cache already paid for.
