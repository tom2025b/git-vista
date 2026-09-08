#!/usr/bin/env bash
# PostToolUse hook: run the route-census tests whenever route_authz.rs,
# main.rs, contract_suite.rs or security.rs changes. EXPECTED_ROUTE_COUNT went stale
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
# (ADR 0134's meta-census, which reads both tables directly) so an edit to
# either side of that relationship gets checked against the other at edit
# time, not merge time.
#
# #705: security.rs joins the trigger list for the same reason one level
# over. route_authz.rs's EXPECTED_UNAUTHENTICATED pins the pre-session
# allowlist, but security.rs's `session_exempt` is what actually enforces
# it, and the two drifted — the table reasoned in (path, method) pairs
# while the runtime exempted a whole path. So the hook also runs
# `the_pre_session_exemption_is_method_qualified`, and fires on an edit to
# security.rs, not only to the table that describes it.
set -uo pipefail
payload=$(cat)
file=$(printf '%s' "$payload" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tool_input',{}).get('file_path',''))" 2>/dev/null)
case "$file" in
  */git-vista-server/src/route_authz.rs|*/git-vista-server/src/main.rs|*/git-vista-server/src/planner/contract_suite.rs|*/git-vista-server/src/security.rs) ;;
  *) exit 0 ;;
esac
repo=$(cd "$(dirname "$file")" && git rev-parse --show-toplevel 2>/dev/null) || exit 0
out=$(cd "$repo" && cargo test -p git-vista-server -- \
    every_registered_route_is_classified \
    unauthenticated_routes_are_a_pinned_short_allowlist \
    the_pre_session_exemption_is_method_qualified \
    route_authz_and_write_contract_agree_on_every_post_route 2>&1)
rc=$?
if [[ $rc -ne 0 ]]; then
  echo "route census FAILED after editing $file — a route is unclassified, a table is stale, the two censuses disagree, or the pre-session allowlist and its runtime enforcement have drifted:" >&2
  printf '%s\n' "$out" | tail -20 >&2
  exit 2
fi

# A libtest filter that matches nothing exits 0 with "0 passed". Rename or
# delete any of the four tests above and an unguarded run here would print
# nothing, exit 0, and leave this hook looking intact while checking nothing —
# "structurally complete, semantically inert", the exact shape ADR 0134 is
# about, occurring in the guard ADR 0134 installed. So count what actually ran
# rather than reading the word "ok": the census is only checked if all four
# tests executed.
ran=$(printf '%s\n' "$out" | awk '/^test result:/ {
  for (i = 2; i <= NF; i++) if ($i == "passed;") s += $(i - 1)
} END { print s + 0 }')
if [[ "$ran" -ne 4 ]]; then
  echo "route census hook is INERT after editing $file — $ran of 4 census tests ran, so the census was NOT checked. A libtest filter matching nothing exits 0, which is why this counts instead of trusting the exit code. A test was renamed or deleted: fix the names in $0 (do not delete the check)." >&2
  exit 2
fi
exit 0
