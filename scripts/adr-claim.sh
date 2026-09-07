#!/usr/bin/env bash
# adr-claim.sh — claim an ADR number with one git push, before writing anything.
#
# #697: the process "read docs/adr/README.md on current main, take the next free
# number" is a check-then-claim race. A lane reads the index at time T, works for
# 30-90 minutes, and a faster lane has already taken the same number by T+10.
# Six collisions landed in one day (#689's table of five, plus #694 and #695 both
# claiming 0130 while the issue that names the problem was being written).
#
# ADR 0130 shipped the two mitigations that make a collision CHEAP — a heading
# test that catches drift immediately, and `scripts/adr-renumber.sh`, which turns
# the fix from a five-place hand-edit into one command. It explicitly deferred
# the fix that makes a collision IMPOSSIBLE, and named recurrence as the trigger
# to revisit. This script is that fix.
#
# # The mechanism, in one sentence
#
# The claim is a single `git push` of an orphan commit to
# `refs/adr-claims/NNNN` on the remote, carrying
# `--force-with-lease=refs/adr-claims/NNNN:` — an EMPTY expected value, which
# means "this ref must not exist". The remote applies that as a compare-and-swap
# against the ref's current value, so two lanes racing for one number produce
# exactly one winner and one outright rejection. There is no window between
# reading and claiming, because the read the remote performs and the write it
# performs are the same atomic operation.
#
# # Why a ref, and not a commit on `main`
#
# #697 sketches the reservation as a one-line commit pushed to `main` (an entry
# in a `docs/adr/RESERVED.md`). That works — `main` is unprotected on this
# repository, and a non-fast-forward rejection would arbitrate identically — but
# it is worse in four measured ways, each of which the ref namespace avoids:
#
#   1. A tracked `RESERVED.md` is a merge-conflict surface. Every lane that
#      merges `main` down mid-flight collides on it. The race would be gone and
#      a textual conflict would take its place, in the one file every lane
#      touches.
#   2. A push to `main` is a push to a branch, so GitHub fires a `push` event
#      and the full seven-check gate runs for a reservation that changes no
#      code. Refs outside `refs/heads/*` and `refs/tags/*` fire nothing.
#      (Verified against this repository's own remote, 2026-09-07.)
#   3. A rejected push to `main` must be resolved by fetch-rebase-retry, and
#      each retry re-runs that gate. A rejected ref push is resolved by trying
#      the next number: no rebase, no history, no CI.
#   4. Reservations would sit in `main`'s history forever, one commit per ADR
#      ever started, including the abandoned ones. A ref is deleted with
#      `--release` and leaves nothing behind.
#
# `refs/tags/*` was the other candidate and is rejected for a narrower reason:
# tags are fetched by every clone by default and are listed in GitHub's UI, so
# the repository would grow visible clutter for a purely internal bookkeeping
# device. `refs/adr-claims/*` is invisible to `git ls-remote --heads --tags`,
# is not fetched by a default `git fetch`, and appears in no UI.
#
# # Why the claim commit has no parent
#
# This is load-bearing and was proved by experiment before it was written down.
# A plain `git push` to an existing ref is rejected only when the push is NOT a
# fast-forward. If a lane's claim commit happened to be a DESCENDANT of the
# winner's claim commit, a plain push would be accepted and would silently steal
# a number that was already claimed — measured, against this remote: the steal
# succeeded, exit 0, with the ref left pointing at the thief.
#
# Two independent guards, because either alone is a single point of failure:
#
#   * The claim commit is an ORPHAN over the empty tree (`commit-tree` with no
#     `-p`), so it can never be a descendant of anything and no push of it can
#     ever be a fast-forward.
#   * The push carries the empty lease, which refuses even a genuine
#     fast-forward. This is the guard that does not depend on a future caller
#     remembering to build the commit correctly.
#
# # What this does NOT do
#
# It does not write to `docs/adr/README.md`. A claim is not an index row, and
# must not become one: `every_index_row_has_an_adr_file` (#578) fails a row with
# no file behind it, and ADR 0086 names "claiming a number when the intent
# exists rather than when the file does" as the habit that costs. The claim
# lives off to the side, on the remote, where it can be taken back for free.
#
# # Usage
#
#   scripts/adr-claim.sh [--issue N] [--slug SLUG]
#         Claim the next free number. Prints it on stdout, and nothing else, so
#         it can be captured: NNNN=$(scripts/adr-claim.sh --issue 697)
#
#   scripts/adr-claim.sh NNNN [--issue N] [--slug SLUG]
#         Claim one specific number. Fails outright if it is already claimed or
#         already written; never silently picks a different one.
#
#   scripts/adr-claim.sh --list
#         Every live claim: number, who, how old, and whether its ADR has landed
#         on the base branch yet.
#
#   scripts/adr-claim.sh --release NNNN
#         Give a number back. Use it when the ADR merges (the file on the base
#         branch protects the number from then on) or when the work is dropped.
#
#   scripts/adr-claim.sh --sweep [--yes]
#         Show every claim that can be cleaned up — landed ones, and ones older
#         than --stale-days with no file behind them. Lists by default; --yes
#         releases them. This is what keeps an abandoned claim from permanently
#         burning a number.
#
#   scripts/adr-claim.sh --renumber OLD [--issue N]
#         The recovery path, for a lane that wrote an ADR without claiming and
#         collided. Claims a fresh number atomically, then hands it to
#         scripts/adr-renumber.sh to move OLD onto it in one pass.
#
# Options: --remote R (default origin), --base B (default main),
#          --stale-days N (default 7), --max-attempts N (default 10),
#          --dry-run, --help
set -euo pipefail

REMOTE="${ADR_CLAIM_REMOTE:-origin}"
BASE="${ADR_CLAIM_BASE:-main}"
CLAIM_NS="refs/adr-claims"
STALE_DAYS=7
MAX_ATTEMPTS=10
DRY_RUN=0
ISSUE=""
SLUG=""

# ---------------------------------------------------------------------------
# Pure helpers. Everything in this block is a function of its stdin or its
# arguments and touches no network, so ci/adr_claim_race_test.sh drives the
# real ones rather than modelling them.
# ---------------------------------------------------------------------------

# refs/adr-claims/0136 for 0136.
claim_ref() {
	printf '%s/%s\n' "$CLAIM_NS" "$1"
}

# Four-digit numbers out of `git ls-remote` output on the claim namespace.
# Reads stdin. A line whose ref tail is not exactly four digits is dropped
# rather than guessed at.
numbers_from_ls_remote() {
	local _sha ref tail
	while read -r _sha ref; do
		tail="${ref##*/}"
		[[ "$tail" =~ ^[0-9]{4}$ ]] && printf '%s\n' "$tail"
	done
	return 0
}

# Four-digit numbers out of a list of ADR paths or filenames. Reads stdin.
# `README.md` has no leading number and falls out here rather than needing to
# be named — the same filter `adr_index_matches_the_files.rs` uses.
numbers_from_filenames() {
	local path base
	while read -r path; do
		base="${path##*/}"
		[[ "$base" == *.md ]] || continue
		[[ "${base:0:4}" =~ ^[0-9]{4}$ ]] && printf '%s\n' "${base:0:4}"
	done
	return 0
}

# The next number after the highest one on stdin.
#
# Highest-plus-one, deliberately, rather than lowest-free. A gap in the corpus
# is not proof the number is free: `0134` is missing from `main` today because
# a lane on #690 renumbered onto it in a WIP commit that never landed, and
# handing 0134 to the next lane would collide with that branch the moment it
# revives. Gaps stay gaps; ADR 0086 is a tombstone that says so in prose.
#
# EMPTY INPUT IS AN ERROR, not 0001. Every caller unions in the ADR files that
# certainly exist, so an empty set means a read failed — and silently returning
# "the first number" on a failed read is exactly the vacuous-success shape this
# repository keeps finding. Refuse instead.
next_number_after() {
	local n highest=""
	while read -r n; do
		[[ "$n" =~ ^[0-9]{4}$ ]] || continue
		if [[ -z "$highest" || "$n" > "$highest" ]]; then
			highest="$n"
		fi
	done
	if [[ -z "$highest" ]]; then
		echo "adr-claim: refusing to guess — no ADR numbers were read at all." >&2
		echo "  That means a read failed, not that the corpus is empty." >&2
		return 1
	fi
	local next=$((10#$highest + 1))
	if [[ "$next" -gt 9999 ]]; then
		echo "adr-claim: next number would be $next — the four-digit scheme is full." >&2
		return 1
	fi
	printf '%04d\n' "$next"
}

# The claim commit's message: a subject a human can read in `git log`, and
# fields a sweep can parse. This IS the cleanup trace the issue asks for — an
# abandoned claim says who abandoned it and when, which the process it replaces
# recorded nowhere at all.
claim_message() {
	local number="$1"
	printf 'adr-claim: %s\n\n' "$number"
	printf 'Number: %s\n' "$number"
	# Same precedence git itself uses when it writes the commit, so the trace
	# names whoever the commit object names. Reading git config alone would
	# print a different person than the commit was authored by whenever a lane
	# overrides its identity per-invocation, which is this repository's
	# documented way of committing.
	printf 'Claimed-by: %s <%s>\n' \
		"${GIT_AUTHOR_NAME:-$(git config user.name || echo unknown)}" \
		"${GIT_AUTHOR_EMAIL:-$(git config user.email || echo unknown)}"
	printf 'Claimed-at: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
	printf 'Host: %s\n' "${HOSTNAME:-unknown}"
	printf 'Branch: %s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
	# A nonce, and it is not decoration.
	#
	# `try_claim` treats "the ref already points at MY sha" as a win, because a
	# push can land and still report failure (a dropped connection on the
	# response). That is only safe if two lanes can never mint the same sha.
	# Without this line they can, and the test fixture proved it: a commit
	# object is a pure function of (tree, parents, author, committer, message),
	# every one of which two lanes can share — same empty tree, no parents,
	# same configured identity, same branch name, same number, same second.
	# Both lanes then push byte-identical objects, both pushes succeed, and
	# BOTH lanes believe they own the number. That is the exact collision this
	# script exists to prevent, reintroduced by the deduplication that makes
	# git fast.
	printf 'Nonce: %s\n' "$$-${RANDOM}${RANDOM}-$(date -u +%s%N)"
	[[ -n "$ISSUE" ]] && printf 'Issue: #%s\n' "${ISSUE#\#}"
	[[ -n "$SLUG" ]] && printf 'Slug: %s\n' "$SLUG"
	printf 'Release-with: scripts/adr-claim.sh --release %s\n' "$number"
	return 0
}

# ---------------------------------------------------------------------------
# The network side.
# ---------------------------------------------------------------------------

# Every number that is spoken for: live claims on the remote, ADR files on the
# base branch, and ADR files in this working tree. The union, because each
# source misses something the others catch — the remote claims miss an ADR that
# landed and released, the base branch misses everything in flight, and the
# working tree misses every other lane.
taken_numbers() {
	local base_ref
	git fetch --quiet "$REMOTE" "$BASE" || {
		echo "adr-claim: could not fetch $REMOTE/$BASE — refusing to pick a number blind." >&2
		return 1
	}
	base_ref=$(git rev-parse FETCH_HEAD)

	git ls-remote "$REMOTE" "$CLAIM_NS/*" | numbers_from_ls_remote
	git ls-tree --name-only "$base_ref" docs/adr/ | numbers_from_filenames
	ls docs/adr/ 2>/dev/null | numbers_from_filenames
}

# An orphan commit over the empty tree. No parent: see the header.
mint_claim_commit() {
	local number="$1" empty_tree
	empty_tree=$(git hash-object -t tree /dev/null)
	claim_message "$number" | git commit-tree "$empty_tree"
}

# Attempt one claim. The whole race lives in this single push.
#
#   0 — won
#   1 — lost the race; somebody else owns this number now
#   2 — the push failed for some OTHER reason (auth, network, a refusing remote)
#
# The three are distinguished by ASKING THE REMOTE who owns the ref, never by
# matching git's error text. Message matching would silently reclassify an auth
# failure as a lost race on a different git version or a non-English locale, and
# the caller's response to those two is opposite: advance to the next number, or
# stop immediately.
try_claim() {
	local number="$1" sha="$2" ref owner
	ref=$(claim_ref "$number")

	if git push --quiet \
		--force-with-lease="$ref:" \
		"$REMOTE" "$sha:$ref" 2>/dev/null; then
		return 0
	fi

	owner=$(git ls-remote "$REMOTE" "$ref" | cut -f1)
	if [[ -z "$owner" ]]; then
		return 2
	fi
	if [[ "$owner" == "$sha" ]]; then
		return 0
	fi
	return 1
}

# ---------------------------------------------------------------------------
# Commands.
# ---------------------------------------------------------------------------

cmd_claim() {
	local wanted="${1:-}"
	local attempt=0 number sha rc

	while :; do
		attempt=$((attempt + 1))
		if [[ -n "$wanted" ]]; then
			number="$wanted"
		else
			number=$(taken_numbers | next_number_after)
		fi

		if [[ "$DRY_RUN" == 1 ]]; then
			echo "adr-claim: would claim $number on $REMOTE $(claim_ref "$number")" >&2
			printf '%s\n' "$number"
			return 0
		fi

		sha=$(mint_claim_commit "$number")
		set +e
		try_claim "$number" "$sha"
		rc=$?
		set -e

		case "$rc" in
		0)
			echo "adr-claim: claimed $number on $REMOTE ($(claim_ref "$number"))" >&2
			echo "adr-claim: release it with: scripts/adr-claim.sh --release $number" >&2
			printf '%s\n' "$number"
			return 0
			;;
		1)
			if [[ -n "$wanted" ]]; then
				echo "adr-claim: $number is already claimed by another lane — refusing." >&2
				echo "  Run without a number to take the next free one." >&2
				return 1
			fi
			echo "adr-claim: lost the race for $number, trying the next number…" >&2
			if [[ "$attempt" -ge "$MAX_ATTEMPTS" ]]; then
				echo "adr-claim: gave up after $MAX_ATTEMPTS attempts." >&2
				return 1
			fi
			;;
		*)
			echo "adr-claim: the push to $(claim_ref "$number") failed, and the number is" >&2
			echo "  still unclaimed on $REMOTE — so this is NOT a lost race. Check your" >&2
			echo "  credentials and network, then rerun. Nothing was claimed." >&2
			return 2
			;;
		esac
	done
}

# Fetch the claim refs so their messages can be read locally.
fetch_claims() {
	git fetch --quiet --prune --force "$REMOTE" \
		"+$CLAIM_NS/*:$CLAIM_NS/*" 2>/dev/null || true
}

# `field Claimed-by <sha>` -> the value, or empty.
claim_field() {
	git log -1 --format=%B "$2" 2>/dev/null \
		| sed -n "s/^$1: //p" | head -1
}

claim_age_days() {
	local when now
	when=$(git log -1 --format=%ct "$1" 2>/dev/null || echo 0)
	now=$(date -u +%s)
	echo $(((now - when) / 86400))
}

# Numbers that have an ADR file on the base branch.
landed_numbers() {
	local base_ref
	git fetch --quiet "$REMOTE" "$BASE" >/dev/null 2>&1 || return 1
	base_ref=$(git rev-parse FETCH_HEAD)
	git ls-tree --name-only "$base_ref" docs/adr/ | numbers_from_filenames
}

cmd_list() {
	fetch_claims
	local landed
	landed=$(landed_numbers || true)

	local any=0 ref number age by state
	while read -r _sha ref; do
		[[ -n "${ref:-}" ]] || continue
		number="${ref##*/}"
		[[ "$number" =~ ^[0-9]{4}$ ]] || continue
		any=1
		age=$(claim_age_days "$ref")
		by=$(claim_field Claimed-by "$ref")
		if grep -qx "$number" <<<"$landed"; then
			state="LANDED — safe to release"
		elif [[ "$age" -ge "$STALE_DAYS" ]]; then
			state="STALE ${age}d — no file on $BASE; abandoned?"
		else
			state="in flight (${age}d)"
		fi
		printf '%s  %-40s %s\n' "$number" "${by:-unknown}" "$state"
	done < <(git ls-remote "$REMOTE" "$CLAIM_NS/*")

	[[ "$any" == 1 ]] || echo "adr-claim: no live claims on $REMOTE."
	return 0
}

cmd_release() {
	local number="$1" ref
	[[ "$number" =~ ^[0-9]{4}$ ]] || {
		echo "adr-claim: '$number' is not a four-digit ADR number" >&2
		return 1
	}
	ref=$(claim_ref "$number")
	if [[ -z "$(git ls-remote "$REMOTE" "$ref")" ]]; then
		echo "adr-claim: $number is not claimed on $REMOTE — nothing to release." >&2
		return 0
	fi
	fetch_claims
	# Say whose claim is being dropped before dropping it. Releasing another
	# lane's claim is a legitimate operation (that is what --sweep is for), but
	# it should never happen without the operator seeing the name.
	echo "adr-claim: releasing $number, claimed by $(claim_field Claimed-by "$ref" || echo unknown)" >&2
	if [[ "$DRY_RUN" == 1 ]]; then
		echo "adr-claim: (dry run) would run: git push $REMOTE :$ref" >&2
		return 0
	fi
	git push --quiet "$REMOTE" ":$ref"
	git update-ref -d "$ref" 2>/dev/null || true
	echo "adr-claim: $number released — it is back in the pool." >&2
	return 0
}

cmd_sweep() {
	local do_it="${1:-0}"
	fetch_claims
	local landed
	landed=$(landed_numbers || true)

	local ref number age releasable=()
	while read -r _sha ref; do
		[[ -n "${ref:-}" ]] || continue
		number="${ref##*/}"
		[[ "$number" =~ ^[0-9]{4}$ ]] || continue
		age=$(claim_age_days "$ref")
		if grep -qx "$number" <<<"$landed"; then
			echo "$number  LANDED   $(claim_field Claimed-by "$ref")" >&2
			releasable+=("$number")
		elif [[ "$age" -ge "$STALE_DAYS" ]]; then
			echo "$number  STALE ${age}d  $(claim_field Claimed-by "$ref")" >&2
			releasable+=("$number")
		fi
	done < <(git ls-remote "$REMOTE" "$CLAIM_NS/*")

	if [[ "${#releasable[@]}" -eq 0 ]]; then
		echo "adr-claim: nothing to sweep." >&2
		return 0
	fi
	if [[ "$do_it" != 1 ]]; then
		echo >&2
		echo "adr-claim: ${#releasable[@]} claim(s) above can be released. Rerun with --yes" >&2
		echo "  to release them, or release one at a time:" >&2
		for number in "${releasable[@]}"; do
			echo "    scripts/adr-claim.sh --release $number" >&2
		done
		return 0
	fi
	for number in "${releasable[@]}"; do
		cmd_release "$number"
	done
	return 0
}

# The recovery path. A lane that skipped the claim step and collided does not
# need a second tool: claim a fresh number atomically, then hand OLD and the
# number just won to scripts/adr-renumber.sh, which already knows how to move
# every one of the five places a number lives.
cmd_renumber() {
	local old="$1" new
	[[ -x scripts/adr-renumber.sh ]] || {
		echo "adr-claim: scripts/adr-renumber.sh is missing or not executable" >&2
		return 1
	}
	new=$(cmd_claim "")
	echo "adr-claim: claimed $new; renumbering $old -> $new" >&2
	if [[ "$DRY_RUN" == 1 ]]; then
		echo "adr-claim: (dry run) would run: scripts/adr-renumber.sh $old $new" >&2
		return 0
	fi
	# If the renumber fails, give the number back rather than burning it.
	if ! scripts/adr-renumber.sh "$old" "$new"; then
		echo "adr-claim: the renumber failed — releasing $new so it is not burned." >&2
		cmd_release "$new" || true
		return 1
	fi
	return 0
}

usage() {
	sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" \
		| sed '$d; s/^# \{0,1\}//'
}

# Sourcing this file defines its functions and runs nothing else, so
# ci/adr_claim_race_test.sh can drive the real functions instead of modelling
# them — the posture gv, dev and their guards already take (#476). Every line
# above is a definition or a plain assignment; everything below is the
# command-line flow.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
	return 0
fi

MODE=claim
ARG=""
SWEEP_YES=0

while [[ $# -gt 0 ]]; do
	case "$1" in
	--help | -h)
		usage
		exit 0
		;;
	--list)
		MODE=list
		shift
		;;
	--release)
		MODE=release
		ARG="${2:-}"
		shift 2
		;;
	--sweep)
		MODE=sweep
		shift
		;;
	--renumber)
		MODE=renumber
		ARG="${2:-}"
		shift 2
		;;
	--yes)
		SWEEP_YES=1
		shift
		;;
	--issue)
		ISSUE="${2:-}"
		shift 2
		;;
	--slug)
		SLUG="${2:-}"
		shift 2
		;;
	--remote)
		REMOTE="${2:-}"
		shift 2
		;;
	--base)
		BASE="${2:-}"
		shift 2
		;;
	--stale-days)
		STALE_DAYS="${2:-}"
		shift 2
		;;
	--max-attempts)
		MAX_ATTEMPTS="${2:-}"
		shift 2
		;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	[0-9][0-9][0-9][0-9])
		ARG="$1"
		shift
		;;
	*)
		echo "adr-claim: unknown argument '$1' (try --help)" >&2
		exit 2
		;;
	esac
done

cd "$(git rev-parse --show-toplevel)"

case "$MODE" in
claim) cmd_claim "$ARG" ;;
list) cmd_list ;;
release)
	[[ -n "$ARG" ]] || {
		echo "adr-claim: --release needs a four-digit number" >&2
		exit 2
	}
	cmd_release "$ARG"
	;;
sweep) cmd_sweep "$SWEEP_YES" ;;
renumber)
	[[ -n "$ARG" ]] || {
		echo "adr-claim: --renumber needs the ADR to move (NNNN or NNNN-slug)" >&2
		exit 2
	}
	cmd_renumber "$ARG"
	;;
esac
