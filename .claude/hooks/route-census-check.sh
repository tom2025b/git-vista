#!/usr/bin/env bash
# PostToolUse hook: run the route-census test whenever route_authz.rs or
# main.rs changes. EXPECTED_ROUTE_COUNT went stale three times in one night
# (42→44→45→46, 2026-08-05) — a stale usize merges silently, so the trap must
# spring at edit time, not merge time.
set -uo pipefail
payload=$(cat)
file=$(printf '%s' "$payload" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tool_input',{}).get('file_path',''))" 2>/dev/null)
case "$file" in
  */git-vista-server/src/route_authz.rs|*/git-vista-server/src/main.rs) ;;
  *) exit 0 ;;
esac
repo=$(cd "$(dirname "$file")" && git rev-parse --show-toplevel 2>/dev/null) || exit 0
if ! out=$(cd "$repo" && cargo test -q -p git-vista-server every_registered_route_is_classified 2>&1 | tail -8); then
  echo "route census FAILED after editing $file — EXPECTED_ROUTE_COUNT is stale or a route is unclassified:" >&2
  printf '%s\n' "$out" >&2
  exit 2
fi
exit 0
