#!/usr/bin/env bash
#
# Regression test for #469 — `cmd_browser`'s node guard must check the node it
# found is NEW ENOUGH, not merely that one exists.
#
# # Why this exists as a shell test rather than a Rust one
#
# The defect is in `dev` itself, and it is about which `node` a bare `node`
# resolves to. No Rust test can see that, so — following
# `gate_errexit_test.sh` — this drives the REAL script with the toolchain
# replaced underneath it, never a model of it.
#
# # The defect
#
# `cmd_browser` guarded on presence:
#
#     if ! command -v node >/dev/null 2>&1; then die "…"; fi
#
# On this box `command -v node` finds `/usr/bin/node`, v18.19.1. Playwright
# requires >= 20. The guard passed, `cargo build -p git-vista-server` ran, and
# Playwright then refused — so `./dev gate` could never reach green here, and
# the guard whose whole job is to fail early with a useful message instead
# announced that node was fine immediately before node was the problem.
#
# # What it asserts, and which assertion is load-bearing
#
#   1. Too old  -> non-zero, and the message names the VERSION.
#   2. Too old  -> it stops BEFORE the build.          <- load-bearing
#   3. New enough -> the guard gets out of the way.    <- the other half
#   4. Absent   -> the original not-found path survives.
#
# (1) alone would pass against a guard that ran the whole browser suite and
# merely reported a version afterwards. (2) is what pins the guard as a guard.
# (3) is what stops the fix from being "always refuse", which would also
# satisfy (1) and (2) and be worthless.
#
# Run directly:  ci/browser_node_version_test.sh

set -euo pipefail

repo_root="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
work="$(mktemp -d -t gv-browser-node-XXXXXX)"
trap 'rm -rf "$work"' EXIT

fail() {
  echo "FAIL: $*" >&2
  echo "--- transcript ---" >&2
  cat "$out" >&2
  exit 1
}

# One scenario: a `node` reporting $1, a HOME with no nvm to fall back to, and
# shims that record whether the expensive steps were reached.
#
# `node` absent entirely is expressed as version "", which installs no shim.
run_with_node() {
  local version="$1"
  local bin="$work/bin" home="$work/home"
  rm -rf "$bin" "$home"
  mkdir -p "$bin" "$home/.cache/ms-playwright"

  if [[ -n $version ]]; then
    cat > "$bin/node" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  -v|--version) echo "$version" ;;
  *)            echo "FAKE node \$*" ;;
esac
EOF
    chmod +x "$bin/node"
  fi

  # Reaching either of these means the guard let the run proceed.
  for tool in cargo npx npm; do
    cat > "$bin/$tool" <<EOF
#!/usr/bin/env bash
touch "$work/reached-$tool"
echo "FAKE $tool \$*"
EOF
    chmod +x "$bin/$tool"
  done

  rm -f "$work"/reached-*
  out="$work/out-$version.txt"
  set +e
  ( cd "$repo_root" && HOME="$home" PATH="$bin:/usr/bin:/bin" bash ./dev browser ) > "$out" 2>&1
  rc=$?
  set -e
}

# ── 1 + 2. A node that is too old: refused, by version, before the build ──
run_with_node "v18.19.1"

[[ $rc -ne 0 ]] \
  || fail "dev browser exited 0 with node v18.19.1, which Playwright refuses (#469)"

grep -qiE '18\.19\.1' "$out" \
  || fail "the refusal did not name the version it found — the operator cannot act on it (#469)"

grep -qiE '\b20\b' "$out" \
  || fail "the refusal did not name the version it needs (#469)"

# The load-bearing one. A guard that fires after the build is not a guard.
[[ ! -e "$work/reached-cargo" ]] \
  || fail "the guard let 'cargo build' run on a node Playwright will refuse — it checked presence, not version (#469)"

[[ ! -e "$work/reached-npx" ]] \
  || fail "the guard let Playwright start on a node it will refuse (#469)"

# ── 3. A node that is new enough: the guard must get out of the way ──
run_with_node "v22.14.0"

[[ -e "$work/reached-cargo" ]] \
  || fail "the guard refused node v22.14.0, which satisfies Playwright's >= 20 — the fix rejects good versions (#469)"

# ── 4. No node at all: the original not-found path must survive ──
run_with_node ""

[[ $rc -ne 0 ]] \
  || fail "dev browser exited 0 with no node at all"

grep -qiE 'node' "$out" \
  || fail "the no-node path stopped naming node (#469 regressed the original guard)"

[[ ! -e "$work/reached-cargo" ]] \
  || fail "the guard let 'cargo build' run with no node present at all"

echo "ok: the node guard checks the version, stops before the build, and passes a new-enough node"
