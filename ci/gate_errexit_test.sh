#!/usr/bin/env bash
#
# Regression test for #434 — `./dev gate` must be able to FAIL.
#
# # Why this exists as a shell test rather than a Rust one
#
# The defect it guards is in `dev` itself: `cmd_gate` turns errexit off around
# the recording pipeline, and between 2026-08-19 (`275652ca`) and #434 the
# `( gate_body )` subshell inherited that. Every gate step then ran regardless
# of the previous one's failure, `gate_body` reached its own
# `echo "dev: ✅ gate green"`, and `rc=${PIPESTATUS[0]}` was that echo's `0`.
# The gate could not say no, and gatehouse recorded a `verified: true` for a
# commit whose wasm build could not compile.
#
# No Rust test can see that. The mechanism is the shell's own errexit state
# across a function/subshell/pipeline boundary, so the only honest test drives
# the REAL script — not a model of it. That is the whole design here: this runs
# `dev gate` unmodified, with the toolchain replaced underneath it.
#
# # What it asserts, and why the second assertion is the load-bearing one
#
#   1. The gate exits non-zero when a step fails.
#   2. The gate STOPS at the first failing step.
#
# (1) alone is not enough and would have passed even against a naive "fix"
# that merely propagated the last command's status: the gate would still run
# every step, and a suite that failed early then "passed" later would still be
# reported however the final step happened to end. (2) is what actually pins
# errexit — it can only hold if the first failure aborted the rest.
#
# Run directly:  ci/gate_errexit_test.sh

set -euo pipefail

repo_root="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
work="$(mktemp -d -t gv-gate-errexit-XXXXXX)"
trap 'rm -rf "$work"' EXIT

fake_bin="$work/bin"
mkdir -p "$fake_bin"

# Every build tool the gate reaches, replaced by a shim that always fails.
# `node`/`npx` are shimmed present-but-failing on purpose: `cmd_browser`'s
# prerequisite checks call `die` when node is MISSING, and that is a different
# exit path (an explicit `exit 1`) which would make this test pass for the
# wrong reason — it was the one enforcement surface still working while the
# gate was broken. Missing tools always failed; failing tests were the thing
# that passed.
for tool in cargo trunk node npx; do
  cat > "$fake_bin/$tool" <<EOF
#!/usr/bin/env bash
echo "FAKE $tool \$* — exiting 101"
exit 101
EOF
  chmod +x "$fake_bin/$tool"
done

# Refuse to record anything. Without this the real gatehouse binary (found via
# PATH or ~/projects) would write this deliberately-failing experiment into the
# real evidence store as a genuine run.
cat > "$fake_bin/gatehouse-mcp" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$fake_bin/gatehouse-mcp"

out="$work/out.txt"
set +e
( cd "$repo_root" && PATH="$fake_bin:$PATH" bash ./dev gate ) > "$out" 2>&1
rc=$?
set -e

fail() {
  echo "FAIL: $*" >&2
  echo "--- gate transcript ---" >&2
  cat "$out" >&2
  exit 1
}

# 1. A failing step must make the gate exit non-zero.
[[ $rc -ne 0 ]] || fail "the gate exited 0 with every build tool failing (#434 has regressed)"

# ...and must not claim success in its own words.
! grep -qF "gate green" "$out" \
  || fail "the gate printed 'gate green' while every step was failing (#434 has regressed)"

# 2. The load-bearing one: it must STOP at the first failure. `fmt` is the
#    first step, so its banner is expected; `clippy (native)` is the second and
#    must never be reached.
grep -qF "── fmt ──" "$out" \
  || fail "the gate did not reach its first step at all — this test is not exercising what it thinks"

! grep -qF "── clippy (native) ──" "$out" \
  || fail "the gate continued past a failing fmt step — errexit is not reaching gate_body (#434 has regressed)"

echo "ok: a failing step fails the gate, and stops it (exit was $rc)"
