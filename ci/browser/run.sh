#!/usr/bin/env bash
# Run the browser tests inside their own network namespace.
#
# WHY A NAMESPACE. The server's listen address is a compile-time constant
# (crates/git-vista-server/src/state.rs: `PORT: u16 = 8080`), and
# `parse_bind_addr` refuses any other address on purpose -- binding beyond
# loopback is a security decision, not a configuration knob. So a test server
# cannot simply pick a free port, and `dev testbed` pays for its own port with a
# 10-25 minute rebuild of the whole tree.
#
# `unshare --net` gives this process tree its own loopback, so the tests get
# their own 8080 that is invisible to the operator's server on the host's 8080.
# Nothing is rebuilt, the bind guard is not weakened, and the binary under test
# is the real one, unmodified.
#
# Everything -- server, Chromium, Playwright -- runs inside the namespace,
# because a browser outside it could not reach a loopback inside it.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

bin="$repo/target/debug/git-vista-server"
if [[ ! -x $bin ]]; then
  echo "browser tests: no server binary at $bin" >&2
  echo "               build it first:  cargo build -p git-vista-server" >&2
  exit 1
fi

# Since #448 the fixtures are built by the Rust catalogue rather than in
# JavaScript (ADR 0076), so this binary is as much a prerequisite as the server.
# Checked here rather than left to the first spec: a missing binary otherwise
# surfaces as a spec failing against an empty directory, which reads as a
# product defect instead of a missing build step.
fixture_bin="$repo/target/debug/gv-fixture"
if [[ ! -x $fixture_bin ]]; then
  echo "browser tests: no fixture binary at $fixture_bin" >&2
  echo "               build it first:  cargo build -p git-vista-fixtures" >&2
  exit 1
fi

if [[ ! -f $repo/crates/git-vista/dist/index.html ]]; then
  echo "browser tests: no web bundle at crates/git-vista/dist" >&2
  echo "               build it first:  trunk build --config crates/git-vista/Trunk.toml" >&2
  exit 1
fi

if [[ ! -d $here/node_modules ]]; then
  echo "browser tests: installing Playwright (first run only)"
  ( cd "$here" && npm install --no-audit --no-fund --silent )
fi

# TESTING A CANDIDATE BUNDLE WITHOUT DISTURBING A RUNNING SERVER.
#
# `DIST_DIR` is compiled in relative to the server crate (state.rs:
# `concat!(env!("CARGO_MANIFEST_DIR"), "/../git-vista/dist")`), so EVERY build of
# the server reads that one path -- including the operator's server on the host's
# 8080. Rebuilding the bundle to verify a UI fix would swap the app out from
# under whoever is driving it.
#
# `--mount` gives this process tree its own mount namespace, so a bind mount over
# crates/git-vista/dist is visible ONLY to these tests. The operator's server
# keeps serving the real bundle from the real path, unaware.
#
#   GV_DIST=/path/to/candidate/dist ci/browser/run.sh
#
# Unset, nothing is mounted and the tests read the normal bundle.
dist_override="${GV_DIST:-}"
if [[ -n $dist_override ]]; then
  dist_override="$(cd "$dist_override" && pwd)"
  if [[ ! -f $dist_override/index.html ]]; then
    echo "browser tests: GV_DIST=$dist_override has no index.html" >&2
    exit 1
  fi
fi

# --map-root-user is what makes an unprivileged user namespace possible here. It
# is why `ip` can bring loopback up (a fresh netns starts with `lo` DOWN, and
# nothing reaches 127.0.0.1 until it is), and why `mount --bind` is permitted:
# CAP_SYS_ADMIN is held inside the new user namespace only, never on the host.
exec unshare --user --map-root-user --net --mount -- bash -c '
  set -euo pipefail
  ip link set lo up
  if [[ -n $3 ]]; then
    mount --bind "$3" "$2/crates/git-vista/dist"
    echo "browser tests: serving candidate bundle from $3 (this namespace only)"
  fi
  cd "$1"
  exec npx playwright test -c playwright.config.mjs "${@:4}"
' _ "$here" "$repo" "$dist_override" "$@"
