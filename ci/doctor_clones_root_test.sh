#!/usr/bin/env bash
#
# Regression test for #476 — `gv doctor` must MEASURE the managed-clones root
# from the running listener, never print a constant.
#
# # The defect
#
# `gv:290` printed:
#
#     echo "  catalog root (managed clones): ${TMPDIR:-/tmp}/git-vista-clones"
#
# It never read the server's configuration, never read the server's
# environment, and never checked whether the path existed. The line directly
# above it — the launch repository — IS measured, from `$TARGET_FILE` falling
# back to `/proc/$pid/cmdline`. The two are formatted identically and sit
# adjacent, so the fabricated one inherited the credibility of the measured one.
#
# On this box the printed path (`/tmp/git-vista-clones`) has never existed. The
# real root is `~/.local/share/git-vista/clones`, and it holds a registered
# clone. Chasing why a correctly-placed repository did not appear in the picker,
# the doctor sent the search to the wrong directory; reading
# `/proc/<pid>/environ` is what settled it.
#
# That is the failure mode the doctor was written to prevent. A diagnostic that
# fabricates a value is worse than one that omits it: an omitted line sends you
# to read the code, a fabricated line sends you somewhere wrong believing you
# checked.
#
# # Why this is a shell test
#
# The defect is in `gv` itself. Following `gate_errexit_test.sh` and
# `testbed_target_test.sh`, this sources the REAL script and calls
# `clones_root_from_environ` against fixture environ blobs, rather than
# modelling it.
#
# # What it asserts, and which assertions are load-bearing
#
#   1. GIT_VISTA_CLONES_ROOT wins outright.
#   2. XDG_DATA_HOME, when there is no override.
#   3. HOME alone -> ~/.local/share/git-vista/clones.   <- the real case here
#   4. An EMPTY value counts as unset and falls through. <- catches a presence
#                                                           check, which the
#                                                           Rust side does not do
#   5. An unreadable environ -> NON-ZERO and prints NOTHING, so the caller can
#      say "unknown".                                    <- LOAD-BEARING: this is
#                                                           the fabrication itself
#   6. The hardcoded string is gone from `gv`.           <- LOAD-BEARING: pins the
#                                                           defect, not a
#                                                           paraphrase of it
#   7. Sourcing `gv` runs nothing.                       <- the guard this test
#                                                           depends on
#
# (1)-(3) alone would pass against a version that resolved correctly and then
# printed a constant anyway, which is precisely what the old code did with the
# line above it. (5) and (6) are what make this a test of the defect.
#
# # Known gap, stated rather than papered over
#
# This does not drive `doctor` end to end — that needs a live listener, and the
# operator's own server is not a fixture. The composition of the output line is
# therefore covered only by (6). The end-to-end proof is a human running
# `./dev doctor` against the real server, which is why #476 is a local job.
#
# Run directly:  ci/doctor_clones_root_test.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
  echo "doctor_clones_root_test: FAIL — $*" >&2
  exit 1
}

# ---------------------------------------------------------------- (7) sourcing
#
# Asserted first, because every assertion below depends on it. If sourcing `gv`
# ran its command-line flow, this script would try to start a server.
before="$(ls -A "$WORK" | wc -l)"
# shellcheck source=/dev/null
source "$REPO_ROOT/gv" >"$WORK/source.out" 2>"$WORK/source.err"
after="$(ls -A "$WORK" | wc -l)"
[[ "$before" -eq $(( after - 2 )) ]] || fail "sourcing gv created files beyond its own capture"
[[ -s "$WORK/source.out" ]] && fail "sourcing gv printed to stdout: $(cat "$WORK/source.out")"
declare -f clones_root_from_environ >/dev/null \
  || fail "sourcing gv did not define clones_root_from_environ"

# A NUL-delimited environ blob, the shape /proc/<pid>/environ has.
make_environ() {
  local out="$1"; shift
  : >"$out"
  local kv
  for kv in "$@"; do
    printf '%s\0' "$kv" >>"$out"
  done
}

expect() {
  local what="$1" want="$2" got="$3"
  [[ "$got" == "$want" ]] || fail "$what: expected '$want', got '$got'"
}

# ------------------------------------------------------------- (1) the override
make_environ "$WORK/e1" \
  "HOME=/home/someone" \
  "XDG_DATA_HOME=/home/someone/.local/share" \
  "GIT_VISTA_CLONES_ROOT=/srv/clones"
expect "an explicit override wins" \
  "/srv/clones" "$(clones_root_from_environ "$WORK/e1")"

# ---------------------------------------------------------- (2) XDG_DATA_HOME
make_environ "$WORK/e2" \
  "HOME=/home/someone" \
  "XDG_DATA_HOME=/data/xdg"
expect "XDG_DATA_HOME is used when there is no override" \
  "/data/xdg/git-vista/clones" "$(clones_root_from_environ "$WORK/e2")"

# ----------------------------------------------------------------- (3) HOME
make_environ "$WORK/e3" "HOME=/home/someone"
expect "HOME alone resolves under .local/share" \
  "/home/someone/.local/share/git-vista/clones" "$(clones_root_from_environ "$WORK/e3")"

# ------------------------------------------------- (4) empty counts as unset
#
# `XDG_DATA_HOME=` is PRESENT and empty. The Rust side filters empties out
# (`.filter(|p| !p.as_os_str().is_empty())`), so this must fall through to
# HOME. A presence check would return "/git-vista/clones" — an absolute path
# under the filesystem root, which is exactly the kind of confidently wrong
# answer this whole issue is about.
make_environ "$WORK/e4" \
  "XDG_DATA_HOME=" \
  "HOME=/home/someone"
expect "an empty XDG_DATA_HOME counts as unset" \
  "/home/someone/.local/share/git-vista/clones" "$(clones_root_from_environ "$WORK/e4")"

# ------------------------------------------ (5) unreadable environ -> refusal
#
# LOAD-BEARING. The whole defect is a value produced where none could be known.
set +e
got="$(clones_root_from_environ "$WORK/does-not-exist" 2>/dev/null)"
rc=$?
set -e
[[ $rc -ne 0 ]] || fail "an unreadable environ must return non-zero, got rc=0 and '$got'"
[[ -z "$got" ]] || fail "an unreadable environ must print nothing, got '$got'"

# ------------------------------------------- (6) the fabricated line is gone
#
# LOAD-BEARING. Greps the script itself, so the constant cannot come back
# under a different variable name in the same shape.
if grep -n 'git-vista-clones' "$REPO_ROOT/gv" >/dev/null 2>&1; then
  fail "gv still contains the hardcoded 'git-vista-clones' path: $(grep -n 'git-vista-clones' "$REPO_ROOT/gv")"
fi
grep -q 'clones_root_from_environ "/proc/\$pid/environ"' "$REPO_ROOT/gv" \
  || fail "doctor no longer resolves the clones root from the listener's environ"

echo "doctor_clones_root_test: ok — 7 assertions"
