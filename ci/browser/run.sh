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

if [[ ! -f $repo/crates/git-vista/dist/index.html ]]; then
  echo "browser tests: no web bundle at crates/git-vista/dist" >&2
  echo "               build it first:  trunk build --config crates/git-vista/Trunk.toml" >&2
  exit 1
fi

if [[ ! -d $here/node_modules ]]; then
  echo "browser tests: installing Playwright (first run only)"
  ( cd "$here" && npm install --no-audit --no-fund --silent )
fi

# --map-root-user is what makes an unprivileged user namespace possible here; it
# is also why `ip` can bring loopback up inside it. Without `lo` up, nothing can
# connect to 127.0.0.1 at all -- a fresh netns starts with loopback DOWN.
exec unshare --user --map-root-user --net -- bash -c '
  set -euo pipefail
  ip link set lo up
  cd "$1"
  exec npx playwright test -c playwright.config.mjs "${@:2}"
' _ "$here" "$@"
