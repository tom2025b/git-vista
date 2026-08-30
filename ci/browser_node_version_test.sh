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

# A system PATH with every real `node`/`npm`/`npx` removed, so scenario 4 can
# state "no node at all" as a FACT about the environment it built rather than a
# hope about the host's.
#
# Why this exists: scenario 4 used to run with PATH="$bin:/usr/bin:/bin" and
# simply not install a node shim. On a host with node in /usr/bin — which is
# most hosts, and is this one — `command -v node` then found the SYSTEM node,
# the guard correctly let the run proceed, and the test reported
# "the guard let 'cargo build' run with no node present at all". That is a
# false accusation against working code: the premise was never established, and
# the failure named the mechanism instead of the missing precondition. The
# sandbox escape battery's CI preflight already draws this distinction
# correctly (see crates/git-vista-server/src/sandbox/escape_contract.rs) — "this
# runner was not set up" must never arrive disguised as "the thing under test is
# broken". This is that discipline, applied here.
#
# A symlink farm rather than a curated list of needed utilities: `dev` and
# run.sh reach for a moving set of coreutils, and a curated list silently rots
# into a PATH that is missing something unrelated, which fails as a confusing
# guard error rather than as "you forgot to symlink sort".
sysbin="$work/sysbin"
mkdir -p "$sysbin"
for d in /usr/bin /bin /usr/local/bin; do
  [[ -d $d ]] || continue
  for f in "$d"/*; do
    b="${f##*/}"
    case "$b" in node|npm|npx|nodejs|corepack) continue ;; esac
    [[ -e "$sysbin/$b" ]] || ln -s "$f" "$sysbin/$b" 2>/dev/null || true
  done
done
if PATH="$sysbin" command -v node >/dev/null 2>&1; then
  echo "FAIL: could not build a node-free PATH — $(PATH="$sysbin" command -v node) is still reachable." >&2
  echo "      Scenario 4 cannot state its premise on this host, so it refuses to render a verdict" >&2
  echo "      about the guard rather than blame the guard for the harness's own gap." >&2
  exit 1
fi

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
  # ${SYS_PATH:-} lets scenario 4 substitute the node-free farm built above;
  # every other scenario installs its own node shim in $bin and is unaffected by
  # whatever the host happens to carry.
  ( cd "$repo_root" && HOME="$home" PATH="$bin:${SYS_PATH:-/usr/bin:/bin}" bash ./dev browser ) > "$out" 2>&1
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

# ── 5. THIS BOX. A too-old node on PATH, a new-enough one only under nvm ──
#
# The case the whole issue is about, and the one that pins the PATH export:
# finding a good node is not enough, because run.sh reaches `npx`, not `node`.
# A fix that inspects the nvm node and then lets PATH's v18 `npx` run would
# satisfy every assertion above and still leave the gate exactly as broken.
nvm_bin="$work/home/.local/share/nvm/v24.18.0/bin"
run_with_node_and_nvm() {
  local path_version="$1"
  local bin="$work/bin" home="$work/home"
  rm -rf "$bin" "$home"
  mkdir -p "$bin" "$home/.cache/ms-playwright" "$nvm_bin"

  cat > "$bin/node" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  -v|--version) echo "$path_version" ;;
  *)            echo "FAKE path node \$*" ;;
esac
EOF
  cat > "$nvm_bin/node" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  -v|--version) echo "v24.18.0" ;;
  *)            echo "FAKE nvm node \$*" ;;
esac
EOF

  # cargo is the first thing `cmd_browser` runs after the guard, and it is a
  # shim — so it is the last hermetic point at which the exported PATH can be
  # observed. Everything past it (`run.sh`) needs a built server binary and a
  # built web bundle, which a fresh clone does not have; asserting out there
  # made this test pass on repo state rather than on the fix.
  cat > "$bin/cargo" <<EOF
#!/usr/bin/env bash
touch "$work/reached-cargo"
printf '%s' "\$PATH" > "$work/cargo-path"
EOF
  for dir in "$bin" "$nvm_bin"; do
    for tool in npx npm; do
      printf '#!/usr/bin/env bash\n' > "$dir/$tool"
    done
  done
  chmod +x "$bin"/* "$nvm_bin"/*

  rm -f "$work"/reached-* "$work/cargo-path"
  out="$work/out-nvm.txt"
  set +e
  # ${SYS_PATH:-} lets scenario 4 substitute the node-free farm built above;
  # every other scenario installs its own node shim in $bin and is unaffected by
  # whatever the host happens to carry.
  ( cd "$repo_root" && HOME="$home" PATH="$bin:${SYS_PATH:-/usr/bin:/bin}" bash ./dev browser ) > "$out" 2>&1
  rc=$?
  set -e
}

run_with_node_and_nvm "v18.19.1"

[[ -e "$work/reached-cargo" ]] \
  || fail "a v24 under nvm was not found, so the guard refused a box that can actually run these tests (#469)"

exported_path="$(cat "$work/cargo-path")"

[[ "$(cut -d: -f1 <<<"$exported_path")" == "$nvm_bin" ]] \
  || fail "the guard accepted the nvm node but did not put it first on PATH; PATH began with $(cut -d: -f1 <<<"$exported_path") (#469)"

# The invariant that actually matters: `run.sh` invokes `npx`, never `node`.
# A fix that inspects one binary and then runs a different one leaves the gate
# exactly as broken as it found it.
resolved_npx="$(PATH="$exported_path" command -v npx || true)"
[[ "$resolved_npx" == "$nvm_bin/npx" ]] \
  || fail "with the exported PATH, npx resolves to '$resolved_npx', not the accepted toolchain's $nvm_bin/npx (#469)"

# ── 4. No node at all: the original not-found path must survive ──
#
# SYS_PATH is the node-free farm, so "absent" is a property of the environment
# this test constructed, not an accident of the host's packaging.
SYS_PATH="$sysbin" run_with_node ""

[[ $rc -ne 0 ]] \
  || fail "dev browser exited 0 with no node at all"

grep -qiE 'node' "$out" \
  || fail "the no-node path stopped naming node (#469 regressed the original guard)"

[[ ! -e "$work/reached-cargo" ]] \
  || fail "the guard let 'cargo build' run with no node present at all"

echo "ok: the node guard checks the version, stops before the build, and passes a new-enough node"
