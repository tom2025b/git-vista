#!/usr/bin/env bash
# test-adr-reserve.sh — integration test for scripts/adr-reserve.sh and
# adr-release.sh (#697, ADR 0132).
#
# Exercises the real push-race mechanics against a throwaway LOCAL bare
# repository — never real GitHub. `failure-atlas`'s `mutation_check` does
# not apply here (it drives cargo, this is bash orchestrating git), so this
# is the proof: run clean first, then apply each of the two mutations ADR
# 0132's "Mutation proof" section describes and confirm this test goes red
# for each, in a different place, before reverting.
#
# Usage: scripts/test-adr-reserve.sh
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
SCRIPT="$repo_root/scripts/adr-reserve.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
check() {
	# check <actual> <expected> <description>
	if [ "$1" = "$2" ]; then
		echo "ok   $3"
		pass=$((pass + 1))
	else
		echo "FAIL $3 (expected '$2', got '$1')"
		fail=$((fail + 1))
	fi
}

# ---- Build a minimal fake "origin" with the two ledger files ----
git init --quiet --bare "$WORK/origin.git"
git clone --quiet "$WORK/origin.git" "$WORK/seed"
(
	cd "$WORK/seed"
	mkdir -p docs/adr
	cat >docs/adr/README.md <<'EOF'
# Architecture Decision Records
| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-x.md) | X | Accepted |
EOF
	cat >docs/adr/RESERVED.md <<'EOF'
# ADR number reservations
| Number | Reserved (UTC) | Branch | Working title |
| --- | --- | --- | --- |
EOF
	git -c user.name=t -c user.email=t@example.com add -A
	git -c user.name=t -c user.email=t@example.com commit --quiet -m seed
	git branch -M main
	git push --quiet origin HEAD:main
)
# The bare repo's HEAD defaults to a ref ("master") nobody ever created, so
# a plain `git clone` afterward checks out nothing — set it explicitly.
git -C "$WORK/origin.git" symbolic-ref HEAD refs/heads/main

# ---- Test 1: sequential claims never collide, even without a real race ----
git clone --quiet "$WORK/origin.git" "$WORK/laneA"
A="$(cd "$WORK/laneA" && "$SCRIPT" feature/lane-a "Lane A")"
check "$A" "0002" "first claim gets the number right after README's max"

git clone --quiet "$WORK/origin.git" "$WORK/laneB"
B="$(cd "$WORK/laneB" && "$SCRIPT" feature/lane-b "Lane B")"
check "$B" "0003" "second claim, made after the first landed, gets the number after IT — not a duplicate of 0002"

# ---- Test 2: idempotency — re-running for the same branch reprints, never re-claims ----
A2="$(cd "$WORK/laneA" && "$SCRIPT" feature/lane-a "Lane A, second call")"
check "$A2" "$A" "calling reserve twice for the same branch returns the same number, not a new one"

# ---- Test 3: a real concurrent race — two lanes fetch before either pushes ----
# A plain `&`-backgrounded race on a local bare repo is too fast to reliably
# collide (worktree+commit+push all finish in well under a second), so it
# is not a race a test can depend on. Force one deterministically instead:
# every `git push` made through laneD's PATH sleeps first, so laneC's
# unhindered fetch-build-push always lands first and laneD's first push is
# always against stale state — a real non-fast-forward rejection, not a
# simulated one.
# Every `git fetch` sleeps briefly first, so both lanes' fetches land close
# together regardless of shell-fork ordering; every `git push` sleeps
# longer, but ONLY when SLOW_PUSH=1 — set for laneD alone, so laneC's
# unhindered push always lands first and laneD's, built on what is now
# stale state, is genuinely rejected rather than merely simulated.
mkdir -p "$WORK/slowbin"
cat >"$WORK/slowbin/git" <<'WRAP'
#!/usr/bin/env bash
# The real invocations are `git fetch ...` but `git -C <dir> push ...`, so
# the subcommand is not reliably $1 — search the whole argument list.
for a in "$@"; do
	if [ "$a" = "fetch" ]; then
		sleep 0.3
	fi
	if [ "$a" = "push" ] && [ "${SLOW_PUSH:-}" = "1" ]; then
		sleep 1
	fi
done
exec /usr/bin/git "$@"
WRAP
chmod +x "$WORK/slowbin/git"

git clone --quiet "$WORK/origin.git" "$WORK/laneC"
git clone --quiet "$WORK/origin.git" "$WORK/laneD"
(
	cd "$WORK/laneC" && PATH="$WORK/slowbin:$PATH" "$SCRIPT" feature/lane-c "Lane C" >"$WORK/c.out" 2>"$WORK/c.err"
	echo "$?" >"$WORK/c.rc"
) &
PIDC=$!
(
	cd "$WORK/laneD" && PATH="$WORK/slowbin:$PATH" SLOW_PUSH=1 "$SCRIPT" feature/lane-d "Lane D" >"$WORK/d.out" 2>"$WORK/d.err"
	echo "$?" >"$WORK/d.rc"
) &
PIDD=$!
wait "$PIDC" || true
wait "$PIDD" || true
C="$(cat "$WORK/c.out")"
D="$(cat "$WORK/d.out")"
RC_C="$(cat "$WORK/c.rc" 2>/dev/null || echo unknown)"
RC_D="$(cat "$WORK/d.rc" 2>/dev/null || echo unknown)"
# Both sides must have actually succeeded AND produced a real 4-digit
# number — "not equal" alone would also pass if one side crashed and left
# an empty string, which is a different, worse failure than a duplicate.
if [ "$RC_C" = "0" ] && [ "$RC_D" = "0" ] \
	&& [[ "$C" =~ ^[0-9]{4}$ ]] && [[ "$D" =~ ^[0-9]{4}$ ]] \
	&& [ "$C" != "$D" ]; then
	check "distinct" "distinct" "a real concurrent race still yields two distinct, real numbers ($C, $D)"
else
	check "C=rc${RC_C}:'${C}' D=rc${RC_D}:'${D}'" "distinct" "a real concurrent race still yields two distinct, real numbers"
fi
if grep -q "push rejected" "$WORK/c.err" "$WORK/d.err" 2>/dev/null; then
	check "saw_retry" "saw_retry" "the losing side's stderr shows at least one retry after a real rejection"
else
	check "no_retry" "saw_retry" "the losing side's stderr shows at least one retry after a real rejection"
fi

# ---- Test 4: release removes a reservation, then the number is claimable again ----
RELEASE="$repo_root/scripts/adr-release.sh"
(cd "$WORK/laneA" && "$RELEASE" "$A" >/dev/null)
git -C "$WORK/seed" fetch --quiet origin main
still_there="$(git -C "$WORK/seed" show origin/main:docs/adr/RESERVED.md | grep -cE "^\| ${A} " || true)"
check "$still_there" "0" "a released reservation's row is actually gone from origin/main"

# ---- Test 5: release refuses to touch a number that landed for real ----
git clone --quiet "$WORK/origin.git" "$WORK/lander"
(
	cd "$WORK/lander"
	printf '| [0002](0002-y.md) | Y | Accepted |\n' >>docs/adr/README.md
	git -c user.name=t -c user.email=t@example.com add docs/adr/README.md
	git -c user.name=t -c user.email=t@example.com commit --quiet -m "land 0002 for real"
	git push --quiet origin HEAD:main
)
if (cd "$WORK/laneB" && "$RELEASE" 0002 >/dev/null 2>"$WORK/refuse.err"); then
	check "released" "refused" "release refuses a number that has a real README.md row"
else
	check "refused" "refused" "release refuses a number that has a real README.md row"
fi

echo
echo "${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ]
