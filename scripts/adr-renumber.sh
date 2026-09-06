#!/usr/bin/env bash
# adr-renumber.sh — move an ADR from one number to another in a single pass.
#
# #689: five ADR-number collisions landed in one day because the process
# ("read the index, take the next free number") isn't atomic across lanes
# working 30-90 minutes apart. This script doesn't prevent a collision — it
# makes resolving one cheap and safe, so a renumber touches the filename, the
# tracked PDF, the document's own H1, the index row, and every cross-reference
# in one pass instead of by hand across five places (which is how the 0126
# heading-drift sub-bug entered: the heading edit never got staged).
#
# Usage: scripts/adr-renumber.sh OLD NEW_NNNN
#
#   OLD is one of:
#     NNNN               — the four-digit number, when exactly one file
#                           claims it (the common case)
#     NNNN-slug          — the filename's stem, when NNNN is claimed by more
#                           than one file (the #691-review-shaped case this
#                           script exists to resolve: two files, one number,
#                           the operator must say which one moves)
#     NNNN-slug.md        — the exact filename, also accepted
#
# NEW is always a bare four-digit number: the file being created doesn't
# exist yet, so there's nothing for it to disambiguate against.
#
# Run from the repository root. Requires a clean working tree (refuses
# otherwise, so a failed run is trivially recoverable with `git checkout .`).
set -euo pipefail

if [ "$#" -ne 2 ]; then
	echo "usage: $0 OLD[-slug[.md]] NEW_NNNN" >&2
	exit 1
fi

old_arg="$1"
new="$2"

if ! [[ "$new" =~ ^[0-9]{4}$ ]]; then
	echo "error: '$new' is not a four-digit ADR number" >&2
	exit 1
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [ -n "$(git status --porcelain)" ]; then
	echo "error: working tree is not clean — commit or stash first" >&2
	exit 1
fi

adr_dir="docs/adr"

# Resolve OLD to exactly one file. A bare NNNN is ambiguous precisely when
# two files claim the same number — the collision this script exists to fix
# — so that case refuses and lists the candidates instead of silently
# picking one (the bug grok's #694 review caught: `find | head -1`).
if [[ "$old_arg" =~ ^[0-9]{4}$ ]]; then
	mapfile -t matches < <(find "$adr_dir" -maxdepth 1 -name "${old_arg}-*.md" | sort)
	if [ "${#matches[@]}" -eq 0 ]; then
		echo "error: no file matching $adr_dir/${old_arg}-*.md" >&2
		exit 1
	elif [ "${#matches[@]}" -gt 1 ]; then
		echo "error: $old_arg is claimed by more than one file — say which one moves:" >&2
		for m in "${matches[@]}"; do
			echo "  $(basename "${m%.md}")" >&2
		done
		echo "rerun as: $0 <one of the stems above> $new" >&2
		exit 1
	fi
	old_md="${matches[0]}"
elif [[ "$old_arg" =~ ^([0-9]{4}-[A-Za-z0-9-]+)\.md$ ]]; then
	old_md="$adr_dir/$old_arg"
elif [[ "$old_arg" =~ ^[0-9]{4}-[A-Za-z0-9-]+$ ]]; then
	old_md="$adr_dir/${old_arg}.md"
else
	echo "error: '$old_arg' is not NNNN, NNNN-slug, or NNNN-slug.md" >&2
	exit 1
fi

if [ ! -f "$old_md" ]; then
	echo "error: $old_md does not exist" >&2
	exit 1
fi

old_basename=$(basename "$old_md")
old="${old_basename:0:4}"

if [ "$old" = "$new" ]; then
	echo "error: old and new numbers are the same ($old)" >&2
	exit 1
fi

new_md_check=$(find "$adr_dir" -maxdepth 1 -name "${new}-*.md" | head -1)
if [ -n "$new_md_check" ]; then
	echo "error: $new is already taken by $new_md_check — pick a free number" >&2
	exit 1
fi

slug="${old_basename#${old}-}"
slug="${slug%.md}"
new_basename="${new}-${slug}.md"
new_md="$adr_dir/$new_basename"

echo "renaming $old_md -> $new_md"
git mv "$old_md" "$new_md"

# The H1 heading: both styles present in the corpus, "# ADR NNNN — Title" and
# the older "# NNNN — Title". Rewrite only the first line, only the number.
first_line=$(head -1 "$new_md")
if [[ "$first_line" =~ ^(#\ (ADR\ )?)"$old"(\ .*)$ ]]; then
	new_first_line="${BASH_REMATCH[1]}${new}${BASH_REMATCH[3]}"
	tmp=$(mktemp)
	{
		printf '%s\n' "$new_first_line"
		tail -n +2 "$new_md"
	} >"$tmp"
	mv "$tmp" "$new_md"
	echo "heading updated: $first_line -> $new_first_line"
else
	echo "warning: $new_md's first line doesn't match either known heading style — fix it by hand:" >&2
	echo "  $first_line" >&2
fi

old_pdf="$adr_dir/pdf/${old}-${slug}.pdf"
if [ -f "$old_pdf" ]; then
	new_pdf="$adr_dir/pdf/${new}-${slug}.pdf"
	echo "renaming $old_pdf -> $new_pdf"
	git mv "$old_pdf" "$new_pdf"
fi

# The index row in README.md: the markdown link target and the visible number
# both encode the old number, tied to this specific slug so an unrelated row
# that happens to share no text is left untouched.
readme="$adr_dir/README.md"
if grep -qF "[${old}](${old_basename})" "$readme"; then
	sed -i "s|\[${old}\](${old_basename})|[${new}](${new_basename})|" "$readme"
	echo "README.md index row updated"
else
	echo "warning: no README.md row found linking exactly [${old}](${old_basename}) — check the index by hand" >&2
fi

# Cross-references anywhere else in the tree: the exact old filename (covers
# markdown links and any docs/adr/OLD-slug.md path), and the "ADR OLD" phrase
# (the prose form used in handoffs and specs). Both are tied to this ADR's own
# number-plus-slug or number-plus-word-boundary, so a coincidental OLD digit
# elsewhere (an issue number, a port) is not touched.
#
# Excluded: any OTHER docs/adr/OLD-*.md file. If OLD was ambiguous, its
# sibling(s) still legitimately claim OLD in their own H1 — that is a
# different document, not a reference to this one, and must be left alone
# (mutation-proved: without this exclusion, resolving one half of a two-file
# collision silently rewrites the surviving file's own heading, replacing
# one drift bug with another).
mapfile -t hit_files < <(
	grep -rlF -e "$old_basename" -e "ADR ${old}" \
		--include='*.md' --include='*.rs' --include='*.html' \
		. 2>/dev/null | grep -v '^\./target/' | grep -v '^\./node_modules/' | sort -u
)
for f in "${hit_files[@]}"; do
	case "$f" in
	"./$adr_dir/${old}-"*.md) continue ;;
	esac
	sed -i \
		-e "s|${old_basename}|${new_basename}|g" \
		-e "s|ADR ${old}\\b|ADR ${new}|g" \
		"$f"
	echo "cross-reference updated: $f"
done

echo
echo "done. review with: git status --porcelain docs/adr; git diff --stat"
echo "then run: cargo test -p git-vista-server --test adr_index_matches_the_files"
