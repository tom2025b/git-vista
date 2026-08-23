#!/usr/bin/env bash
#
# ci/cold_build_measure.sh — what a COLD build of this workspace actually costs.
#
# # Why this exists
#
# `dev testbed` prints "Expect 10-25 minutes" before a cold build (see the
# cold-build branch in `dev`'s cmd_testbed). Nobody measured that. It is exactly
# the unfalsifiable prose that docs/PERFORMANCE_BUDGETS.md exists to replace —
# a range wide enough that no outcome can contradict it, in front of the one
# build a human is asked to wait through.
#
# # The measurement is ONE-SHOT per host
#
# A cold cache is destroyed by measuring it. There is no "re-run with better
# instrumentation": the second run is warm, and restoring coldness on a
# developer box means deleting a cache that costs the same 10-25 minutes to
# rebuild. So this script captures everything in a single pass and writes it
# to a report file, rather than printing numbers a human has to catch as they
# scroll past.
#
# # Three different things are all called "cold", and they cost differently
#
#   C0  no ~/.cargo/registry, no target/   — pays crate DOWNLOAD + compile
#   C1  warm registry, no target/          — pays compile only
#   C2  warm registry, warm target/        — pays only what changed
#
# `dev testbed` on a fresh port is C1: the developer box has been building this
# workspace for months, so its registry is warm and only the per-port target dir
# is empty. A number measured at C0 and reported as the testbed's cost is wrong
# in the dishonest direction — it folds in a network download the testbed never
# pays. This script therefore times the fetch SEPARATELY from the compile and
# labels the tier it actually observed, instead of asking the operator to
# remember which one they were in.
#
# # Anti-vacuity
#
# The failure mode is a number recorded from a run that quietly reused a cache:
# fast, green, and meaningless. Two guards, neither of which the build decides
# for itself:
#
#   1. The pre-run state of target/ and ~/.cargo/registry is recorded as
#      observed fact, not asserted by flag.
#   2. Every compile phase counts cargo's own "Compiling <crate>" lines and
#      fails if the count is below a floor. A warm rebuild prints ZERO of them,
#      so a cache that was not actually cold cannot produce a passing record.
#
# # Both guards were proven to fire, 2026-08-23, on this repository's own tree
#
#   run with target/ present            -> refused, tier recorded as C2-warm-target
#   guard 1 deleted, run on a warm tree -> the native phase completed rc=0 in 17 s
#                                          having compiled 4 crates, and the floor
#                                          refused it
#
# The second is the one worth reading: without the floor that run produces a
# green, plausible-looking "17 seconds" for a phase that did almost nothing. A
# cold-build number is not distinguishable from a warm one by its own success.
#
# Usage:
#   ci/cold_build_measure.sh [report-path]
#
# Writes key=value lines to the report (default: cold-build-report.txt) and
# leaves cargo's own --timings HTML in target/cargo-timings/.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
cd "$REPO_ROOT"

REPORT="${1:-$REPO_ROOT/cold-build-report.txt}"
: > "$REPORT"

rec() { echo "$*" >> "$REPORT"; echo "cold-build: $*"; }
die() { echo "cold-build: FATAL: $*" >&2; exit 1; }

# A phase that compiled fewer than this many crates was not compiling from
# scratch. The workspace's own Cargo.lock declares ~400 packages; not all of
# them are in every phase's graph, so the floor is deliberately far below any
# real cold count and far above the zero a warm rebuild prints.
COMPILE_FLOOR=50

# Phases whose failure is recorded rather than fatal. trunk's first run in a
# container fetches its own wasm-bindgen, so a blocked egress must not throw
# away the compile numbers that phases 1-3 already paid for.
NONFATAL="trunk"

# ---------------------------------------------------------------------------
# Phase 0 — host and coldness, observed rather than assumed.
# ---------------------------------------------------------------------------
registry_state="absent"
registry_bytes=0
if [[ -d $HOME/.cargo/registry ]]; then
  registry_state="present"
  registry_bytes=$(du -sk "$HOME/.cargo/registry" 2>/dev/null | cut -f1)
  registry_bytes=$(( registry_bytes * 1024 ))
fi
target_state="absent"
[[ -e target ]] && target_state="present"

# The tier is DERIVED from what was observed, so a report can never claim a
# coldness its own preflight contradicts.
if [[ $target_state == present ]]; then
  tier="C2-warm-target"
elif [[ $registry_state == absent ]]; then
  tier="C0-no-registry-no-target"
else
  tier="C1-warm-registry-no-target"
fi

rec "schema=1"
rec "host_nproc=$(nproc)"
rec "host_mem_kib=$(awk '/^MemTotal:/{print $2}' /proc/meminfo)"
rec "host_disk_avail_kib=$(df -k --output=avail . | tail -1 | tr -d ' ')"
rec "rustc=$(rustc --version | tr ' ' '_')"
rec "cargo=$(cargo --version | tr ' ' '_')"
rec "trunk=$(trunk --version 2>/dev/null | tr ' ' '_' || echo not-installed)"
rec "lock_packages=$(grep -c '^\[\[package\]\]' Cargo.lock)"
rec "pre_registry=$registry_state"
rec "pre_registry_bytes=$registry_bytes"
rec "pre_target=$target_state"
rec "tier=$tier"
rec "cargo_incremental=${CARGO_INCREMENTAL:-unset}"

[[ $target_state == present ]] && die "target/ already exists — this host is warm, and a warm host cannot measure a cold build. Move it aside deliberately if you mean to."

# ---------------------------------------------------------------------------
# run_phase <name> <compile-floor|nofloor> -- <command...>
#
# Times one phase, counts the crates cargo says it compiled, and records the
# target/ size that phase left behind. `time` is wall-clock on purpose: the
# claim being replaced ("expect 10-25 minutes") is a claim about how long a
# human waits, not about CPU seconds.
# ---------------------------------------------------------------------------
run_phase() {
  local name="$1" floor="$2"; shift 3
  local log start end elapsed compiled rc
  # A phase named in NONFATAL records its failure and lets the run finish, so
  # one optional step cannot cost the whole one-shot measurement.
  log="$(mktemp -t gv-cold-"$name"-XXXXXX.log)"

  echo "cold-build: ── $name ──────────────────────────────"
  start=$(date +%s)
  "$@" > "$log" 2>&1
  rc=$?
  end=$(date +%s)
  elapsed=$(( end - start ))

  compiled=$(grep -cE '^[[:space:]]*Compiling ' "$log")
  rec "${name}_rc=$rc"
  rec "${name}_seconds=$elapsed"
  rec "${name}_compiled_crates=$compiled"
  rec "${name}_target_kib=$( [[ -d target ]] && du -sk target | cut -f1 || echo 0 )"
  rec "${name}_log=$log"

  if [[ $rc -ne 0 ]]; then
    echo "cold-build: phase $name FAILED (rc=$rc); tail of $log:" >&2
    tail -30 "$log" >&2
    [[ " $NONFATAL " == *" $name "* ]] || die "phase $name failed"
    rec "${name}_note=failed-but-nonfatal"
    return 0
  fi
  if [[ $floor != nofloor && $compiled -lt $floor ]]; then
    die "phase $name compiled only $compiled crate(s), below the floor of $floor — this phase reused a cache, so its time is not a cold-build number"
  fi
}

# ---------------------------------------------------------------------------
# Phase 1 — the network. Separated from compile precisely so a C0 host can
# still report a compile number comparable with a C1 host's.
#
# `cargo fetch --locked` populates the registry for EVERY target the workspace
# resolves for, so the wasm phase below does not pay a second download.
# ---------------------------------------------------------------------------
run_phase fetch nofloor -- cargo fetch --locked

# ---------------------------------------------------------------------------
# Phase 2 — the native debug build of the server, offline.
#
# --offline is not a speed trick: it is the proof that phase 1 covered the whole
# graph. If anything still needed the network, this phase fails rather than
# silently folding a download into a number labelled "compile".
#
# --timings leaves the per-crate breakdown in target/cargo-timings/ so the
# question "what actually dominates this build" has an answer that does not
# require re-running a build that can only be run once.
# ---------------------------------------------------------------------------
run_phase native "$COMPILE_FLOOR" -- \
  cargo build --offline --timings -p git-vista-server

# ---------------------------------------------------------------------------
# Phase 3 — the wasm32 half, offline, into the SAME target dir.
#
# This is the part a native-only measurement misses, and it is not small: a
# dependency shared between the server and the frontend is compiled ONCE PER
# TARGET TRIPLE. `dev testbed` runs trunk and `cargo build -p git-vista-server`
# against one CARGO_TARGET_DIR, so its cold build pays both. Measuring only the
# native half would understate the testbed's real cost.
# ---------------------------------------------------------------------------
run_phase wasm "$COMPILE_FLOOR" -- \
  cargo build --offline -p git-vista --target wasm32-unknown-unknown

# ---------------------------------------------------------------------------
# Phase 4 — trunk, the step that turns the wasm32 artifact into the bundle the
# browser loads (wasm-bindgen, asset hashing, dist/). Phase 3 left the wasm
# compile warm, so this measures trunk's OWN post-processing, not a rebuild.
#
# No compile floor: by construction almost nothing should recompile here, and
# a floor would fail the run for behaving correctly.
# ---------------------------------------------------------------------------
if command -v trunk >/dev/null; then
  run_phase trunk nofloor -- trunk build --config crates/git-vista/Trunk.toml
else
  rec "trunk_rc=skipped-not-installed"
fi

# ---------------------------------------------------------------------------
# Totals.
# ---------------------------------------------------------------------------
total=0
for k in fetch native wasm trunk; do
  v=$(awk -F= -v k="${k}_seconds" '$1==k{print $2}' "$REPORT")
  [[ -n ${v:-} ]] && total=$(( total + v ))
done
rec "total_seconds=$total"
rec "total_minutes=$(awk -v t="$total" 'BEGIN{printf "%.1f", t/60}')"
rec "final_target_kib=$(du -sk target | cut -f1)"

echo
echo "cold-build: report written to $REPORT"
