#!/usr/bin/env bash
# adr-release.sh — release an abandoned ADR-number reservation (#697, ADR
# 0132). Use this when a lane claimed a number with scripts/adr-reserve.sh
# and never finished (branch abandoned, PR closed unmerged) — never for a
# number that a real ADR actually landed under, which this script refuses
# to touch.
#
# The normal cleanup path does NOT go through this script: the PR that
# lands the real docs/adr/NNNN-slug.md and its docs/adr/README.md row
# deletes its own docs/adr/RESERVED.md row in the same commit. This script
# exists only for the case where that PR never happens.
#
# Usage: scripts/adr-release.sh <4-digit-number>
#
# Same atomicity as adr-reserve.sh: the release is a single push to `main`,
# retried on a non-fast-forward rejection.
set -euo pipefail

NUMBER="${1:?usage: adr-release.sh <4-digit-number>}"
if ! [[ "$NUMBER" =~ ^[0-9]{4}$ ]]; then
	echo "error: '$NUMBER' is not a four-digit ADR number" >&2
	exit 1
fi
MAX_ATTEMPTS=10

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

for attempt in $(seq 1 "$MAX_ATTEMPTS"); do
	git fetch origin main --quiet

	if git show origin/main:docs/adr/README.md 2>/dev/null | grep -qF "[${NUMBER}]("; then
		echo "adr-release: ${NUMBER} is a real, merged ADR — refusing to release it" >&2
		exit 1
	fi

	if ! git show origin/main:docs/adr/RESERVED.md 2>/dev/null | grep -qE "^\| ${NUMBER} "; then
		echo "adr-release: ${NUMBER} has no active reservation on origin/main — nothing to do"
		exit 0
	fi

	tmpdir="$(mktemp -d)"
	git worktree add --detach --quiet "$tmpdir" origin/main

	grep -vE "^\| ${NUMBER} " "$tmpdir/docs/adr/RESERVED.md" > "$tmpdir/docs/adr/RESERVED.md.tmp"
	mv "$tmpdir/docs/adr/RESERVED.md.tmp" "$tmpdir/docs/adr/RESERVED.md"
	git -C "$tmpdir" \
		-c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
		add docs/adr/RESERVED.md
	git -C "$tmpdir" \
		-c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com \
		commit --quiet -m "chore(adr): release abandoned reservation ${NUMBER}"

	push_err="$(mktemp)"
	if git -C "$tmpdir" push origin HEAD:main --quiet 2>"$push_err"; then
		git worktree remove --force "$tmpdir" >/dev/null 2>&1 || true
		rm -f "$push_err"
		echo "adr-release: ${NUMBER} released"
		exit 0
	fi

	rejected="$(cat "$push_err" 2>/dev/null || true)"
	rm -f "$push_err"
	git worktree remove --force "$tmpdir" >/dev/null 2>&1 || true
	echo "adr-release: push rejected (attempt ${attempt}/${MAX_ATTEMPTS}), retrying: ${rejected}" >&2
done

echo "adr-release: gave up after ${MAX_ATTEMPTS} attempts" >&2
exit 1
