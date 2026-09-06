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
# Usage: scripts/adr-renumber.sh OLD_NNNN NEW_NNNN
#
# Run from the repository root. Requires a clean working tree (refuses
# otherwise, so a failed run is trivially recoverable with `git checkout .`).
set -euo pipefail

if [ "$#" -ne 2 ]; then
	echo "usage: $0 OLD_NNNN NEW_NNNN" >&2
	exit 1
fi

old="$1"
new="$2"

for n in "$old" "$new"; do
	if ! [[ "$n" =~ ^[0-9]{4}$ ]]; then
		echo "error: '$n' is not a four-digit ADR number" >&2
		exit 1
	fi
done

if [ "$old" = "$new" ]; then
	echo "error: old and new numbers are the same ($old)" >&2
	exit 1
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [ -n "$(git status --porcelain)" ]; then
	echo "error: working tree is not clean — commit or stash first" >&2
	exit 1
fi

adr_dir="docs/adr"
old_md=$(find "$adr_dir" -maxdepth 1 -name "${old}-*.md" | head -1)
if [ -z "$old_md" ]; then
	echo "error: no file matching $adr_dir/${old}-*.md" >&2
	exit 1
fi
new_md_check=$(find "$adr_dir" -maxdepth 1 -name "${new}-*.md" | head -1)
if [ -n "$new_md_check" ]; then
	echo "error: $new is already taken by $new_md_check — pick a free number" >&2
	exit 1
fi

old_basename=$(basename "$old_md")
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
mapfile -t hit_files < <(
	grep -rlF -e "$old_basename" -e "ADR ${old}" \
		--include='*.md' --include='*.rs' --include='*.html' \
		. 2>/dev/null | grep -v '^\./target/' | grep -v '^\./node_modules/' | sort -u
)
for f in "${hit_files[@]}"; do
	[ "$f" = "./$new_md" ] && continue
	sed -i \
		-e "s|${old_basename}|${new_basename}|g" \
		-e "s|ADR ${old}\\b|ADR ${new}|g" \
		"$f"
	echo "cross-reference updated: $f"
done

echo
echo "done. review with: git status --porcelain docs/adr; git diff --stat"
echo "then run: cargo test -p git-vista-server --test adr_index_matches_the_files"
