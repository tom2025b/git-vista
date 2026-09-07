#!/usr/bin/env bash
#
# Guard for #697 — `scripts/adr-claim.sh` must make an ADR number claimable by
# exactly one lane, with a single git operation that fails outright for the
# loser.
#
# # The defect
#
# "Read `docs/adr/README.md` on current main, take the next free number" is a
# check-then-claim race. The read and the write are minutes to hours apart, and
# nothing on the remote holds the number in between. #689 tabulated five
# collisions in one day; #697 was written after the sixth, which happened on the
# pull request meant to fix the first five.
#
# # Why this is a shell test
#
# What is being tested is a git remote's ref-update semantics under contention.
# No Rust test can observe that, and a model of it would be a model of the exact
# thing that must not be assumed. Following `gate_errexit_test.sh`,
# `testbed_target_test.sh` and `doctor_clones_root_test.sh`, this sources the
# REAL script and drives its real functions against a real bare repository
# acting as the remote.
#
# It is hermetic and network-free: the "remote" is a bare repo in a temp dir,
# and every claim really is pushed to it. `cargo test` runs it through
# `crates/git-vista-server/tests/dev_script_guards.rs`, so it cannot become a
# guard nobody invokes (the gap #469 closed).
#
# # What it asserts, and which assertions are load-bearing
#
#    1. `numbers_from_ls_remote` reads four-digit tails and drops the rest.
#    2. `numbers_from_filenames` reads NNNN and drops README.md.
#    3. `next_number_after` is highest-plus-one, zero-padded.
#    4. EMPTY input to `next_number_after` FAILS and prints no number.
#                                                   <- LOAD-BEARING: a failed
#                                                      read must never look like
#                                                      "the corpus is empty, take
#                                                      0001"
#    5. A gap in the corpus is left alone (0134's real shape today).
#    6. Two lanes pushing the same number: exactly one wins, one is REJECTED.
#                                                   <- LOAD-BEARING: this is the
#                                                      race itself
#    7. A DESCENDANT claim commit is rejected too.  <- LOAD-BEARING: measured, a
#                                                      plain push accepts this
#                                                      and silently steals the
#                                                      number; the empty lease is
#                                                      what refuses it
#    8. N concurrent `cmd_claim` runs return N DISTINCT numbers.
#                                                   <- LOAD-BEARING: the promise
#                                                      the issue actually makes
#    9. A push failure that is NOT contention exits 2, not 1.
#                                                   <- LOAD-BEARING: exit 1 makes
#                                                      the caller advance to the
#                                                      next number on a network
#                                                      blip, quietly burning one
#   10. `--release` puts the number back in the pool.
#   11. Claiming a TAKEN specific number fails outright, never silently substitutes.
#   12. A claim writes nothing to docs/adr/ — a reservation is not an index row.
#                                                   <- LOAD-BEARING: #578's
#                                                      `every_index_row_has_an_adr_file`
#                                                      fails a row with no file,
#                                                      and ADR 0086 names the habit
#   13. Sourcing the script runs nothing.           <- the guard this test needs
#
# Assertions 1-3, 5, 10 and 11 would all pass against a script that read the
# index and wrote a file with no locking at all. 6, 7, 8, 9 and 12 are the ones
# that test the defect.
#
# Run directly:  ci/adr_claim_race_test.sh
set -uo pipefail

FAILURES=0
fail() {
	echo "adr_claim_race_test: FAIL — $*" >&2
	FAILURES=$((FAILURES + 1))
}
ok() { echo "  ok — $*"; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/adr-claim.sh"

[[ -f "$SCRIPT" ]] || {
	echo "adr_claim_race_test: $SCRIPT is missing — it is not optional" >&2
	exit 1
}

WORK=$(mktemp -d -t adr-claim-test-XXXXXX)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

GIT_ID=(-c user.name=adr-test -c user.email=adr-test@example.invalid)
export GIT_AUTHOR_NAME=adr-test GIT_AUTHOR_EMAIL=adr-test@example.invalid
export GIT_COMMITTER_NAME=adr-test GIT_COMMITTER_EMAIL=adr-test@example.invalid

# ---------------------------------------------------------------------------
# (13) first: sourcing must not run the command-line flow. Every assertion
# below depends on this, so it is checked before anything else sources it.
# ---------------------------------------------------------------------------
echo "sourcing guard"
(
	cd "$WORK"
	# No git repo here, no arguments. If sourcing ran the flow it would reach
	# `git rev-parse --show-toplevel` and fail loudly.
	# shellcheck disable=SC1090
	source "$SCRIPT" >/dev/null 2>&1
) && ok "(13) sourcing runs nothing" || fail "(13) sourcing the script ran its command-line flow"

# From here on the real functions are in this shell.
# shellcheck disable=SC1090
source "$SCRIPT"
# Sourcing it also turned on `errexit` in THIS shell — `scripts/adr-claim.sh`
# opens with `set -euo pipefail`, and a sourced file's shell options are the
# caller's. Almost every assertion below deliberately runs something that
# fails, so leaving errexit on would abort the run at the first `ok` instead of
# reporting. `doctor_clones_root_test.sh` handles the same thing the same way.
set +e

# ---------------------------------------------------------------------------
# The pure helpers (1-5).
# ---------------------------------------------------------------------------
echo "pure helpers"

got=$(printf '%s\n' \
	"aaaa	refs/adr-claims/0136" \
	"bbbb	refs/adr-claims/0141" \
	"cccc	refs/adr-claims/not-a-number" \
	"dddd	refs/heads/main" | numbers_from_ls_remote | tr '\n' ' ')
[[ "$got" == "0136 0141 " ]] \
	&& ok "(1) numbers_from_ls_remote -> $got" \
	|| fail "(1) numbers_from_ls_remote gave '$got', wanted '0136 0141 '"

got=$(printf '%s\n' \
	"docs/adr/0130-a-claimed-number.md" \
	"docs/adr/README.md" \
	"docs/adr/0135-offline.md" \
	"docs/adr/pdf" | numbers_from_filenames | tr '\n' ' ')
[[ "$got" == "0130 0135 " ]] \
	&& ok "(2) numbers_from_filenames -> $got" \
	|| fail "(2) numbers_from_filenames gave '$got', wanted '0130 0135 '"

got=$(printf '%s\n' 0001 0135 0086 | next_number_after)
[[ "$got" == "0136" ]] \
	&& ok "(3) next_number_after -> 0136" \
	|| fail "(3) next_number_after gave '$got', wanted 0136"

# (4) The anti-vacuity assertion. An empty read must FAIL, not return 0001.
got=$(printf '' | next_number_after 2>/dev/null)
rc=$?
if [[ "$rc" -ne 0 && -z "$got" ]]; then
	ok "(4) empty input fails (exit $rc) and prints no number"
else
	fail "(4) empty input gave exit $rc and printed '$got' — a failed read must not look like an empty corpus"
fi

# (5) A gap is not free. 0134 is missing from main today because a lane
# renumbered onto it in a WIP commit that never landed.
got=$(printf '%s\n' 0130 0131 0132 0133 0135 | next_number_after)
[[ "$got" == "0136" ]] \
	&& ok "(5) the 0134 gap is left alone -> 0136" \
	|| fail "(5) gave '$got' — a gap was handed out as free"

# ---------------------------------------------------------------------------
# A real remote, and two lanes.
# ---------------------------------------------------------------------------
echo "fixture"
# `-b main`: without it the bare repo's HEAD stays on `master`, every
# clone below lands with no working tree, and the assertions that need
# `scripts/adr-claim.sh` on disk pass vacuously instead of running.
git init -q --bare -b main "$WORK/remote.git"

git init -q -b main "$WORK/seed"
mkdir -p "$WORK/seed/docs/adr" "$WORK/seed/scripts"
for n in 0130 0131 0132 0133 0135; do
	printf '# ADR %s — fixture\n' "$n" >"$WORK/seed/docs/adr/${n}-fixture.md"
done
printf '# Architecture Decision Records\n\n| ADR | Title | Status |\n| --- | --- | --- |\n' \
	>"$WORK/seed/docs/adr/README.md"
cp "$SCRIPT" "$WORK/seed/scripts/adr-claim.sh"
chmod +x "$WORK/seed/scripts/adr-claim.sh"
git -C "$WORK/seed" add -A >/dev/null
git -C "$WORK/seed" "${GIT_ID[@]}" commit -q -m "seed"
git -C "$WORK/seed" remote add origin "$WORK/remote.git"
git -C "$WORK/seed" push -q origin main

lane() { # lane <name>
	local d="$WORK/$1"
	[[ -d "$d" ]] || git clone -q "$WORK/remote.git" "$d"
	echo "$d"
}
A=$(lane laneA)
B=$(lane laneB)
ok "remote + two lanes at $WORK"

# ---------------------------------------------------------------------------
# (6) The race.
# ---------------------------------------------------------------------------
echo "the race"
race_push() { # race_push <lane-dir> <number> ; echoes the try_claim rc
	local d="$1" n="$2"
	(
		cd "$d"
		# shellcheck disable=SC1090
		source scripts/adr-claim.sh
		set +e # sourcing re-armed errexit; try_claim is MEANT to return non-zero
		REMOTE=origin
		sha=$(mint_claim_commit "$n")
		try_claim "$n" "$sha"
		echo "$?"
	) | tail -1
}
rcA=$(race_push "$A" 0136)
rcB=$(race_push "$B" 0136)
if [[ "$rcA" == 0 && "$rcB" == 1 ]]; then
	ok "(6) lane A won 0136 (rc 0), lane B was rejected (rc 1)"
else
	fail "(6) rcA=$rcA rcB=$rcB — wanted exactly one winner (0) and one rejection (1)"
fi

owners=$(git -C "$A" ls-remote origin 'refs/adr-claims/*' | wc -l)
[[ "$owners" == 1 ]] \
	&& ok "(6) the remote holds exactly one claim for 0136" \
	|| fail "(6) the remote holds $owners claim refs, wanted 1"

# ---------------------------------------------------------------------------
# (7) The steal guard: a DESCENDANT of the winner's claim. Measured against
# GitHub, a plain `git push` ACCEPTS this and takes the number. The empty
# lease is the only thing that refuses it.
# ---------------------------------------------------------------------------
echo "the steal guard"
steal=$(
	cd "$B"
	git fetch -q origin 'refs/adr-claims/0136:refs/steal' 2>/dev/null
	empty=$(git hash-object -t tree /dev/null)
	child=$(git commit-tree "$empty" -p refs/steal -m "steal 0136")
	# Exactly the push the real script makes.
	git push --quiet --force-with-lease=refs/adr-claims/0136: \
		origin "$child:refs/adr-claims/0136" >/dev/null 2>&1
	echo "$?"
)
[[ "$steal" != 0 ]] \
	&& ok "(7) a descendant claim is rejected (exit $steal)" \
	|| fail "(7) a descendant claim was ACCEPTED — the number was silently stolen"

# The same push WITHOUT the lease, to prove assertion 7 is not vacuous: if this
# is also rejected, the lease is not what is doing the work and (7) proves
# nothing about it.
nolease=$(
	cd "$B"
	git fetch -q origin 'refs/adr-claims/0136:refs/steal2' --force 2>/dev/null
	empty=$(git hash-object -t tree /dev/null)
	child=$(git commit-tree "$empty" -p refs/steal2 -m "steal 0136 unleashed")
	git push --quiet origin "$child:refs/adr-claims/0136" >/dev/null 2>&1
	echo "$?"
)
[[ "$nolease" == 0 ]] \
	&& ok "(7) …and the SAME push without the lease succeeds — the lease is load-bearing" \
	|| fail "(7) the unleased descendant push also failed ($nolease) — assertion 7 proves nothing"

# Put 0136 back in a known state for the rest of the run.
git -C "$A" push -q origin ":refs/adr-claims/0136" 2>/dev/null || true

# ---------------------------------------------------------------------------
# (8) N concurrent claims must return N distinct numbers. This is the promise.
# ---------------------------------------------------------------------------
echo "concurrent claims"
for i in 1 2 3 4; do lane "conc$i" >/dev/null; done
pids=()
for i in 1 2 3 4; do
	(
		cd "$WORK/conc$i"
		scripts/adr-claim.sh --remote origin --base main 2>/dev/null
	) >"$WORK/conc$i.out" &
	pids+=($!)
done
for p in "${pids[@]}"; do wait "$p" || true; done
claimed=$(cat "$WORK"/conc*.out | grep -E '^[0-9]{4}$' | sort)
n_claimed=$(wc -l <<<"$claimed")
n_distinct=$(sort -u <<<"$claimed" | wc -l)
if [[ "$n_claimed" == 4 && "$n_distinct" == 4 ]]; then
	ok "(8) 4 concurrent lanes claimed 4 distinct numbers: $(tr '\n' ' ' <<<"$claimed")"
else
	fail "(8) 4 concurrent lanes produced $n_claimed claims, $n_distinct distinct: $(tr '\n' ' ' <<<"$claimed")"
fi

# ---------------------------------------------------------------------------
# (9) A non-contention failure must exit 2, never 1.
# ---------------------------------------------------------------------------
echo "error vs lost race"
rc_err=$(
	cd "$A"
	# shellcheck disable=SC1090
	source scripts/adr-claim.sh
	set +e # sourcing re-armed errexit; try_claim is MEANT to return non-zero
	REMOTE="$WORK/there-is-no-remote-here.git"
	sha=$(mint_claim_commit 0199)
	try_claim 0199 "$sha" >/dev/null 2>&1
	echo "$?"
)
[[ "$rc_err" == 2 ]] \
	&& ok "(9) an unreachable remote gives rc 2 (error), not rc 1 (lost race)" \
	|| fail "(9) an unreachable remote gave rc $rc_err — wanted 2; rc 1 would burn a number on a network blip"

# ---------------------------------------------------------------------------
# (10) --release returns the number to the pool.
# ---------------------------------------------------------------------------
echo "release"
first=$(cd "$A" && scripts/adr-claim.sh --remote origin --base main 2>/dev/null)
(cd "$A" && scripts/adr-claim.sh --remote origin --base main --release "$first" >/dev/null 2>&1)
again=$(cd "$A" && scripts/adr-claim.sh --remote origin --base main 2>/dev/null)
[[ -n "$first" && "$first" == "$again" ]] \
	&& ok "(10) $first was released and handed out again — an abandoned claim does not burn a number" \
	|| fail "(10) claimed '$first', released it, then got '$again' — the number did not return to the pool"

# ---------------------------------------------------------------------------
# (11) A taken specific number fails outright.
# ---------------------------------------------------------------------------
echo "specific number"
out=$(cd "$B" && scripts/adr-claim.sh --remote origin --base main "$again" 2>/dev/null)
rc=$?
if [[ "$rc" -ne 0 && "$out" != "$again" ]]; then
	ok "(11) claiming taken $again failed outright (exit $rc), printed no number"
else
	fail "(11) claiming taken $again gave exit $rc and printed '$out' — it must refuse, not substitute"
fi

# ---------------------------------------------------------------------------
# (12) A claim writes nothing into docs/adr/. A reservation is not an index row.
# ---------------------------------------------------------------------------
echo "the index is untouched"
n_adr=$(cd "$B" && find docs/adr -type f | wc -l)
[[ "$n_adr" -gt 0 ]] \
	|| fail "(12) the lane has no docs/adr/ at all — this assertion would pass vacuously"
sum_before=$(cd "$B" && find docs/adr -type f | sort | xargs md5sum 2>/dev/null | md5sum)
(cd "$B" && scripts/adr-claim.sh --remote origin --base main >/dev/null 2>&1) || true
sum_after=$(cd "$B" && find docs/adr -type f | sort | xargs md5sum 2>/dev/null | md5sum)
dirty=$(git -C "$B" status --porcelain -- docs/adr | wc -l)
if [[ "$sum_before" == "$sum_after" && "$dirty" == 0 ]]; then
	ok "(12) docs/adr/ ($n_adr files) is byte-identical after a claim, tree clean"
else
	fail "(12) a claim modified docs/adr/ ($dirty changed path(s)) — a reservation must not become an index row"
fi

echo
if [[ "$FAILURES" -eq 0 ]]; then
	echo "adr_claim_race_test: PASS"
	exit 0
fi
echo "adr_claim_race_test: $FAILURES assertion(s) failed" >&2
exit 1
