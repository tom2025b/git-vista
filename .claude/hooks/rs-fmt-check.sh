#!/usr/bin/env bash
# PostToolUse hook: rustfmt --check the .rs file Claude just edited.
# Formatting broke CI twice on 2026-08-05 alone (PR #309's red Lint, then an
# unformatted #308 commit) — each cost a full CI round-trip a 200ms local
# check would have caught. Exit 2 feeds the diff back to Claude to fix now.
set -uo pipefail
payload=$(cat)
file=$(printf '%s' "$payload" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tool_input',{}).get('file_path',''))" 2>/dev/null)
case "$file" in
  *.rs) ;;
  *) exit 0 ;;
esac
[ -f "$file" ] || exit 0
if ! out=$(rustfmt --edition 2021 --check "$file" 2>&1); then
  echo "rustfmt: $file is unformatted — run 'cargo fmt' before committing (CI's Lint gate will reject it):" >&2
  printf '%s\n' "$out" | head -20 >&2
  exit 2
fi
exit 0
