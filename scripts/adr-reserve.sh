#!/usr/bin/env bash
# adr-reserve.sh — atomically claim the next free ADR number, before
# starting work on the real document (#697, ADR 0132).
#
# The collision this closes: "read docs/adr/README.md, take the next free
# number" has a gap between the read and the eventual push — 30-90 minutes
# on a real lane — during which a second lane can read the same stale state
# and pick the same number. This script closes the gap by making the claim
# itself a single git push directly to `main`, appending a row to
# `docs/adr/RESERVED.md`. Git's own non-fast-forward rejection is the
# arbiter: if two lanes race, only the first push can land; the second is
# rejected outright and this script retries with a freshly-computed number.
# No lock service, no polling loop watching a shared file — the atomicity is
# git's, not this script's.
#
# This is prevention. `scripts/adr-renumber.sh` (#689/#694) remains the
# recovery for the case where a lane skips this step and collides anyway —
# the two are independent and do not need each other to work.
#
# Usage: scripts/adr-reserve.sh <branch-name> [working-title]
#
#   branch-name    the branch that will carry the real ADR — recorded so an
#                  abandoned reservation can be traced back to its owner and
#                  released with scripts/adr-release.sh.
#   working-title  optional short human-readable title, for readability
#                  only; never parsed back out.
#
# On success, prints the claimed 4-digit number to stdout and exits 0.
# Idempotent: re-running for a branch that already holds a reservation on
# origin/main reprints that same number rather than claiming a second one —
# safe to call again after a crash mid-attempt.
#
# Requires docs/adr/RESERVED.md to already exist on origin/main (ADR 0132's
# own PR creates it) and push access to `origin`.
set -euo pipefail

BRANCH="${1:?usage: adr-reserve.sh <branch-name> [working-title]}"
TITLE="${2:-(untitled)}"
MAX_ATTEMPTS=10

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

for attempt in $(seq 1 "$MAX_ATTEMPTS"); do
	git fetch origin main --quiet

	# Idempotency: this branch may already hold a reservation from an
	# earlier, crashed attempt at this same script.
	existing="$(git show origin/main:docs/adr/RESERVED.md 2>/dev/null \
		| grep -F "| ${BRANCH} |" \
		| grep -oE '^\| [0-9]{4} ' \
		| grep -oE '[0-9]{4}' || true)"
	if [ -n "$existing" ]; then
		echo "$existing"
		exit 0
	fi

	readme_max="$(git show origin/main:docs/adr/README.md \
		| grep -oE '^\| \[[0-9]{4}\]' \
		| grep -oE '[0-9]{4}' \
		| sort -n | tail -1)"
	reserved_max="$(git show origin/main:docs/adr/RESERVED.md 2>/dev/null \
		| grep -oE '^\| [0-9]{4} ' \
		| grep -oE '[0-9]{4}' \
		| sort -n | tail -1 || true)"
	readme_max="${readme_max:-0}"
	reserved_max="${reserved_max:-0}"
	# Force base-10: a number like 0130 would otherwise be read as invalid
	# octal by bash arithmetic.
	next=$((10#$readme_max > 10#$reserved_max ? 10#$readme_max + 1 : 10#$reserved_max + 1))
	number="$(printf '%04d' "$next")"

	tmpdir="$(mktemp -d)"
	git worktree add --detach --quiet "$tmpdir" origin/main

	stamp="$(date -u +%Y-%m-%dT%H:%MZ)"
	printf '| %s | %s | %s | %s |\n' "$number" "$stamp" "$BRANCH" "$TITLE" \
		>> "$tmpdir/docs/adr/RESERVED.md"
	git -C "$tmpdir" \
		-c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
		add docs/adr/RESERVED.md
	git -C "$tmpdir" \
		-c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
		commit --quiet -m "chore(adr): reserve ${number} for ${BRANCH}"

	push_err="$(mktemp)"
	if git -C "$tmpdir" push origin HEAD:main --quiet 2>"$push_err"; then
		git worktree remove --force "$tmpdir" >/dev/null 2>&1 || true
		rm -f "$push_err"
		echo "$number"
		exit 0
	fi

	# Non-fast-forward: someone else's push landed in the meantime — maybe
	# this exact number, maybe an unrelated main advance. Either way, the
	# right move is to re-fetch and recompute from scratch, not to try to
	# resolve a conflict in a file that is pure append.
	rejected="$(cat "$push_err" 2>/dev/null || true)"
	rm -f "$push_err"
	git worktree remove --force "$tmpdir" >/dev/null 2>&1 || true
	echo "adr-reserve: push rejected (attempt ${attempt}/${MAX_ATTEMPTS}), retrying: ${rejected}" >&2
done

echo "adr-reserve: gave up after ${MAX_ATTEMPTS} attempts — repo is unusually contested, or something else is wrong" >&2
exit 1
