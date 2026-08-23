#!/usr/bin/env bash
#
# Regression test for #331 (follow-up) — `dev testbed` must build onto the
# scratch SSD when it is attached, and must NEVER fall back into the caller's
# current directory when it is not.
#
# # Why this is a shell test, and why it sources `dev`
#
# The decision lives in `dev`'s own `testbed_target_for`, and its inputs are
# filesystem facts — is `target` a symlink, does the path it names exist, is
# that path absolute. No Rust test can see any of that. Following the precedent
# in `ci/gate_errexit_test.sh`, this drives the REAL function rather than a
# model of it: it sources `dev` and calls `testbed_target_for` directly, so a
# change to the script that this test does not follow makes the test fail
# rather than quietly keep testing a copy.
#
# # The load-bearing case is case 3, and mutation says so
#
# Cases 1, 2 and 4 assert placement. Case 3 asserts the FALLBACK: with a bare
# `-n && -d` guard, a dangling symlink makes `dirname ""` produce "." — a
# directory — so the guard passes and the build lands wherever the operator
# happened to be standing. The unmounted-disk path is the entire reason the
# fallback exists, so it is the case that must not be assumed.
#
# Proven two ways, 2026-08-23, both `caught` and failing differently:
#
#   removing `== /*` from the guard  -> cases 3 and 4 red, actual `./git-vista-testbed-8081`
#   dropping `-$port` from the path  -> case 1 red, two testbeds share one dir
#
# A third mutation SURVIVED and is recorded because it corrected a wrong belief:
# swapping `readlink` for `readlink -f` changes nothing this suite can see, since
# `== /*` rejects its empty output anyway. The link-text read is a secondary
# choice, not the mechanism — see the note in `dev`.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"

# Sourcing `dev` with no arguments takes its help branch, which prints and
# returns without exiting. Silence it; we want only the function definitions.
# shellcheck source=/dev/null
source "$REPO_ROOT/dev" >/dev/null 2>&1

command -v testbed_target_for >/dev/null \
  || { echo "FAIL: dev does not define testbed_target_for"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fails=0
check() { # check <name> <expected> <actual>
  if [[ $2 == "$3" ]]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1"
    echo "         expected: $2"
    echo "         actual:   $3"
    fails=$((fails + 1))
  fi
}

echo "testbed_target_for:"

# 1 — target is a symlink to an EXISTING absolute path: use that disk.
mkdir -p "$TMP/ssd/cargo-target" "$TMP/one"
ln -s "$TMP/ssd/cargo-target/git-vista" "$TMP/one/target"
check "symlink to an attached disk lands beside the main tree's target" \
  "$TMP/ssd/cargo-target/git-vista-testbed-8081" \
  "$(testbed_target_for "$TMP/one" 8081)"

# 2 — target is a real directory, not a symlink: stay in the repo root.
mkdir -p "$TMP/two/target"
check "a plain target directory falls back to the repo root" \
  "$TMP/two/target-testbed-8081" \
  "$(testbed_target_for "$TMP/two" 8081)"

# 3 — target is a DANGLING symlink (the SSD is unplugged). This is the one.
mkdir -p "$TMP/three"
ln -s /nonexistent-mount/cargo-target/git-vista "$TMP/three/target"
actual3="$(cd "$TMP" && testbed_target_for "$TMP/three" 8081)"
check "an unmounted disk falls back to the repo root" \
  "$TMP/three/target-testbed-8081" "$actual3"
case "$actual3" in
  .*|"") echo "  FAIL a dangling symlink resolved into the caller's CWD: $actual3"
         fails=$((fails + 1)) ;;
  *)     echo "  ok   the fallback is not relative to the CWD" ;;
esac

# 4 — target is a RELATIVE symlink: not usable as given, fall back.
mkdir -p "$TMP/four"
ln -s ../elsewhere/cargo-target/git-vista "$TMP/four/target"
check "a relative symlink falls back to the repo root" \
  "$TMP/four/target-testbed-8081" \
  "$(testbed_target_for "$TMP/four" 8081)"

# 5 — the port is part of the path, or two concurrent testbeds collide.
mkdir -p "$TMP/five/target"
p1="$(testbed_target_for "$TMP/five" 8081)"
p2="$(testbed_target_for "$TMP/five" 8090)"
if [[ $p1 == "$p2" ]]; then
  echo "  FAIL two ports resolved to the same target dir: $p1"
  fails=$((fails + 1))
else
  echo "  ok   different ports get different target dirs"
fi

if (( fails )); then
  echo "testbed_target_for: $fails assertion(s) failed"
  exit 1
fi
echo "testbed_target_for: all assertions passed"
