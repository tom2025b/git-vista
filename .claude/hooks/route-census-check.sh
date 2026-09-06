#!/usr/bin/env bash
# PostToolUse hook: run the route-census tests whenever route_authz.rs,
# main.rs, or contract_suite.rs changes. EXPECTED_ROUTE_COUNT went stale
# three times in one night (42→44→45→46, 2026-08-05) — a stale usize merges
# silently, so the trap must spring at edit time, not merge time.
#
# #690: this used to run only `every_registered_route_is_classified` —
# route_authz.rs's own census against main.rs. That is why #584/PR#688's new
# route reached ROUTE_AUTHZ immediately (this hook caught it) but silently
# missed contract_suite.rs's separate POST-route table entirely: the hook
# fired, ran the one test it knew about, and said nothing about the second
# table, which only failed later in CI's `M1.06 write contract +` job. Now
# also runs `route_authz_and_write_contract_agree_on_every_post_route`
# (ADR 0131's meta-census, which reads both tables directly) so an edit to
# either side of that relationship gets checked against the other at edit
# time, not merge time.
set -uo pipefail
payload=$(cat)
file=$(printf '%s' "$payload" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tool_input',{}).get('file_path',''))" 2>/dev/null)
case "$file" in
  */git-vista-server/src/route_authz.rs|*/git-vista-server/src/main.rs|*/git-vista-server/src/planner/contract_suite.rs) ;;
  *) exit 0 ;;
esac
repo=$(cd "$(dirname "$file")" && git rev-parse --show-toplevel 2>/dev/null) || exit 0
if ! out=$(cd "$repo" && cargo test -q -p git-vista-server -- \
    every_registered_route_is_classified \
    route_authz_and_write_contract_agree_on_every_post_route 2>&1 | tail -8); then
  echo "route census FAILED after editing $file — a route is unclassified, a table is stale, or the two censuses disagree:" >&2
  printf '%s\n' "$out" >&2
  exit 2
fi
exit 0
