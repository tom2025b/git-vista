#!/usr/bin/env bash
#
# Regression test for #748 — the browser harness must execute the binaries
# Cargo just built in CARGO_TARGET_DIR, rather than checking and launching stale
# copies under the worktree's default target/.
#
# This drives the real `dev browser`. Cargo, unshare and Playwright are replaced
# with small shims so the test is hermetic, but the real run.sh, fixture.mjs and
# server.mjs choose and execute the binaries. The marker files below are the
# load-bearing assertions: checking only an exported variable would not prove
# the browser suite actually used the assigned build tree.

set -euo pipefail

repo_root="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../.." && pwd)"
work="$(mktemp -d -t gv-browser-target-XXXXXX)"
trap 'rm -rf "$work"' EXIT

test_repo="$work/repo"
shim_bin="$work/bin"
test_home="$work/home"
mkdir -p "$test_repo/ci/browser/node_modules" \
  "$test_repo/crates/git-vista/dist" "$shim_bin" \
  "$test_home/.cache/ms-playwright"
cp "$repo_root/dev" "$test_repo/dev"
cp "$repo_root/ci/browser/run.sh" "$test_repo/ci/browser/run.sh"
cp "$repo_root/ci/browser/fixture.mjs" "$test_repo/ci/browser/fixture.mjs"
cp "$repo_root/ci/browser/server.mjs" "$test_repo/ci/browser/server.mjs"
touch "$test_repo/crates/git-vista/dist/index.html"

cat > "$work/server-stub" <<'EOF'
#!/usr/bin/env bash
touch "$GV_TARGET_TEST_WORK/server-ran"
exit 17
EOF
cat > "$work/fixture-stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" > "$GV_TARGET_TEST_WORK/fixture-args"
EOF
chmod +x "$work/server-stub" "$work/fixture-stub"

cat > "$shim_bin/node" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == -v || ${1:-} == --version ]]; then
  echo v22.0.0
else
  exec /usr/bin/node "$@"
fi
EOF

cat > "$shim_bin/cargo" <<'EOF'
#!/usr/bin/env bash
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
if [[ $target_dir != /* ]]; then
  target_dir="$PWD/$target_dir"
fi
printf '%s\n' "$target_dir" > "$GV_TARGET_TEST_WORK/cargo-target"
mkdir -p "$target_dir/debug"
cp "$GV_TARGET_TEST_WORK/server-stub" "$target_dir/debug/git-vista-server"
cp "$GV_TARGET_TEST_WORK/fixture-stub" "$target_dir/debug/gv-fixture"
EOF

cat > "$shim_bin/unshare" <<'EOF'
#!/usr/bin/env bash
while [[ $# -gt 0 && $1 != -- ]]; do shift; done
[[ $# -gt 0 ]] && shift
exec "$@"
EOF

cat > "$shim_bin/ip" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

cat > "$shim_bin/npx" <<'EOF'
#!/usr/bin/env bash
exec node ./target-dir-probe.mjs
EOF

cat > "$shim_bin/npm" <<'EOF'
#!/usr/bin/env bash
echo 'FAIL: npm should not run when node_modules exists' >&2
exit 1
EOF
chmod +x "$shim_bin"/*

cat > "$test_repo/ci/browser/target-dir-probe.mjs" <<'EOF'
import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { join } from 'node:path'
import { buildFixture } from './fixture.mjs'
import { SERVER_BIN, startServer } from './server.mjs'

const expectedServer = join(process.env.GV_EXPECTED_TARGET, 'debug', 'git-vista-server')
assert.equal(SERVER_BIN, expectedServer, 'server.mjs did not select the Cargo target binary')

try {
  await startServer({ repoPath: process.cwd(), stateHome: process.env.GV_TARGET_TEST_WORK })
} catch {
  // The test binary exits deliberately; its execution marker is asserted below.
}
assert.ok(existsSync(join(process.env.GV_TARGET_TEST_WORK, 'server-ran')),
  'server.mjs did not execute the selected Cargo target binary')

buildFixture(process.env.GV_TARGET_TEST_WORK)
assert.ok(existsSync(join(process.env.GV_TARGET_TEST_WORK, 'fixture-args')),
  'fixture.mjs did not execute the selected Cargo target binary')
EOF

fail() {
  echo "FAIL: $*" >&2
  [[ ! -f $work/transcript ]] || sed -n '1,200p' "$work/transcript" >&2
  exit 1
}

run_case() {
  local name="$1" expected="$2"
  shift 2
  rm -f "$work/cargo-target" "$work/server-ran" "$work/fixture-args" "$work/transcript"
  rm -rf "$test_repo/target" "$work/assigned-target"

  set +e
  ( cd "$test_repo" && \
    env HOME="$test_home" PATH="$shim_bin:/usr/bin:/bin" \
      GV_TARGET_TEST_WORK="$work" GV_EXPECTED_TARGET="$expected" \
      "$@" bash ./dev browser ) > "$work/transcript" 2>&1
  rc=$?
  set -e

  [[ $rc -eq 0 ]] || fail "$name exited $rc"
  [[ $(cat "$work/cargo-target") == "$expected" ]] \
    || fail "$name built into $(cat "$work/cargo-target"), expected $expected"
  [[ -e $work/server-ran ]] \
    || fail "$name did not execute the server from $expected"
  [[ -e $work/fixture-args ]] \
    || fail "$name did not execute the fixture catalogue from $expected"
}

run_case 'explicit CARGO_TARGET_DIR' "$work/assigned-target" \
  env CARGO_TARGET_DIR="$work/assigned-target"
run_case 'default target directory' "$test_repo/target" env -u CARGO_TARGET_DIR

echo 'ok: browser builds and executes both binaries from Cargo target dir (with default fallback)'
