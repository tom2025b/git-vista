#!/usr/bin/env bash
# PostToolUse hook: rustfmt --check the .rs file Claude just edited.
# Formatting broke CI twice on 2026-08-05 alone (PR #309's red Lint, then an
# unformatted #308 commit) — each cost a full CI round-trip a 200ms local
# check would have caught. Exit 2 feeds the diff back to Claude to fix now.
#
# 2026-09-17: two defects fixed, both observed first-hand on this box.
#
#   (a) SCOPE. The hook is registered as "$CLAUDE_PROJECT_DIR/.claude/hooks/…",
#       and CLAUDE_PROJECT_DIR is "the project root where the session started"
#       (Claude Code hooks reference) — it does NOT follow cwd and does NOT
#       follow the edited file. A session started in ~/projects/git-vista that
#       went on to work in ~/projects/starnav therefore ran THIS hook, from
#       git-vista, against starnav's files. A PostToolUse matcher is a tool
#       name, never a path, so nothing else stopped it. Fix: the hook works out
#       which project it belongs to from its OWN location and stays silent
#       about files that belong to somebody else.
#
#   (b) EDITION. `--edition 2021` was hardcoded. rustfmt's CLI --edition beats
#       rustfmt.toml's `edition`, and `style_edition` defaults to the edition,
#       so an edition-2024 crate that `cargo fmt` has already formatted
#       correctly is reported unformatted here — forever, because rewriting it
#       to satisfy this hook is what makes `cargo fmt` (and CI) unhappy.
#       Measured: `use foo::{_priv, Bar, Qux2, Qux10, baz};` is correct under
#       style_edition 2024 and "wrong" under 2021. Fix: take the edition from
#       the file's own crate, which is exactly what `cargo fmt` — and therefore
#       CI's Lint gate — does.
#
# Design note: deriving project context from the FILE rather than from an
# environment variable is the shape route-census-check.sh in this same
# directory already uses (it does `git rev-parse --show-toplevel` from the
# file's directory). This brings rs-fmt-check into line with it.
#
# Adversarial review, same night, found three more and they are fixed here:
#
#   (c) An edition read from a Cargo.toml is UNTRUSTED INPUT as far as rustfmt
#       is concerned. `rustfmt --edition 2027` answers "Invalid value for
#       `--edition`" with exit 1, which this hook would have reported as "file
#       is unformatted" — an exit 2 no amount of `cargo fmt` can clear, i.e.
#       exactly the unbreakable loop this repair exists to remove. Only the
#       four editions rustfmt accepts are passed through; anything else is
#       treated as "cannot determine" and the hook says nothing.
#
#   (d) The library escape hatch keyed off an environment variable, so an
#       exported RS_FMT_CHECK_LIB made the hook run `return` at top level —
#       printing "return: can only `return' from a function or sourced script"
#       into the very stderr that gets fed back to Claude. It now detects real
#       sourcing instead, and there is no environment variable to leak.
#
#   (e) The ownership decision lived inline, so the branch that KEEPS coverage
#       (a file in a sibling worktree of the same repo is still mine) could
#       only be unit-tested indirectly. It is a function now, and the
#       regression suite exercises it end-to-end against the real checkouts.

set -uo pipefail

# A GIT_DIR/GIT_WORK_TREE/GIT_COMMON_DIR exported into this process's
# environment would redirect every `git rev-parse` below at a repo that
# has nothing to do with the file being checked, which could make
# project_key() misidentify an unrelated file as belonging to this repo
# (2026-09-18 review). Neither is something this hook itself ever sets;
# clear anything inherited before the first git call.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR

# --------------------------------------------------------------- helpers ---

# project_key <path> — an identity for "which project does this path belong
# to". For a git checkout it is the common git dir, so a repo and every one of
# its worktrees share one key (a session in git-vista SHOULD still format-check
# a file it edits in the gv-836 worktree). For anything outside git it is the
# resolved directory itself.
project_key() {
  local dir=$1 key=""
  [ -d "$dir" ] || dir=$(dirname -- "$dir")
  key=$(cd -- "$dir" 2>/dev/null && git rev-parse --git-common-dir 2>/dev/null) || key=""
  if [ -n "$key" ]; then
    # --git-common-dir may come back relative to the checkout; resolve it there.
    key=$(cd -- "$dir" 2>/dev/null && readlink -f -- "$key" 2>/dev/null) || key=""
  fi
  [ -n "$key" ] || key=$(readlink -f -- "$dir" 2>/dev/null) || key=$dir
  printf '%s\n' "$key"
}

# owns_file <resolved-file> <hook-root> — is this file mine to police?
# Yes when it sits under my own project root, or when it belongs to the same
# git repository (which is what covers every worktree of it). Anything else
# belongs to another project with its own rules and its own hooks.
# Exit 0 = mine, 1 = not mine. Kept as a function so the regression suite can
# drive it directly against real checkouts without creating any git state.
owns_file() {
  local file=$1 root=$2
  case "$file" in
    "$root"/*) return 0 ;;
  esac
  [ "$(project_key "$file")" = "$(project_key "$root")" ]
}

# find_up <start-dir> <filename> <stop-dir> — nearest <filename> at or above
# <start-dir>, never walking past <stop-dir> (or /).
find_up() {
  local dir stop=$3
  dir=$(readlink -f -- "$1" 2>/dev/null) || return 1
  while :; do
    [ -f "$dir/$2" ] && { printf '%s\n' "$dir/$2"; return 0; }
    [ "$dir" = "$stop" ] && return 1
    [ "$dir" = "/" ] && return 1
    dir=$(dirname -- "$dir")
  done
}

# toml_edition <Cargo.toml> <section> — the quoted edition of one table, e.g.
# `package` or `workspace.package`. Prints nothing when the table has no
# literal edition (`edition.workspace = true` and `edition = { workspace =
# true }` both deliberately print nothing, so the caller keeps walking up).
toml_edition() {
  awk -v want="$2" '
    /^[[:space:]]*\[/ { s = $0; gsub(/[][ \t]/, "", s); section = s; next }
    section == want && /^[[:space:]]*edition[[:space:]]*=/ {
      if (match($0, /"[0-9]+"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
    }
  ' "$1" 2>/dev/null
}

# rustfmt_knows_edition <string> — true only for an edition this rustfmt will
# actually accept. Everything else is "cannot determine": passing an unknown
# edition through makes rustfmt exit 1 on a USAGE error, which this hook would
# then report as unformatted code and no `cargo fmt` could ever satisfy.
rustfmt_knows_edition() {
  case "$1" in
    2015|2018|2021|2024) return 0 ;;
    *) return 1 ;;
  esac
}

# crate_edition <file> <stop-dir> — the edition rustfmt should parse and style
# <file> with: the nearest enclosing [package] edition, falling back to the
# workspace's [workspace.package] edition further up. Prints nothing if no
# Cargo.toml in scope declares one — in that case the hook says nothing rather
# than guessing, because guessing is the bug this replaces.
crate_edition() {
  local dir manifest ed stop=$2
  dir=$(dirname -- "$1")
  while manifest=$(find_up "$dir" Cargo.toml "$stop"); do
    ed=$(toml_edition "$manifest" package)
    [ -n "$ed" ] || ed=$(toml_edition "$manifest" workspace.package)
    if [ -n "$ed" ]; then printf '%s\n' "$ed"; return 0; fi
    dir=$(dirname -- "$(dirname -- "$manifest")")
    [ "$manifest" = "$stop/Cargo.toml" ] && return 1
    [ "$dir" = "/" ] && return 1
  done
  return 1
}

# Sourcing this file loads the helpers and stops, so the regression suite can
# exercise project_key / owns_file / crate_edition directly. `return` is legal
# only in a sourced file or a function, so the subshell below succeeds exactly
# when we are being sourced — no environment variable is consulted, because an
# exported one would silently disable the hook and print a bash error into the
# feedback channel.
if (return 0 2>/dev/null); then
  return 0
fi

# ------------------------------------------------------------- the check ---

payload=$(cat)
file=$(printf '%s' "$payload" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tool_input',{}).get('file_path',''))" 2>/dev/null)
case "$file" in
  *.rs) ;;
  *) exit 0 ;;
esac
[ -f "$file" ] || exit 0
file=$(readlink -f -- "$file" 2>/dev/null) || exit 0

# Where this hook itself lives: <project>/.claude/hooks/rs-fmt-check.sh.
self=$(readlink -f -- "${BASH_SOURCE[0]:-$0}" 2>/dev/null) || exit 0
hook_root=$(cd -- "$(dirname -- "$self")/../.." 2>/dev/null && pwd -P) || exit 0

owns_file "$file" "$hook_root" || exit 0

# Never walk out of the checkout looking for a manifest. Bound the search at the
# FILE's own checkout root, not the hook's: a session in the main checkout may
# legitimately edit a file in a worktree, and the worktree is where that file's
# Cargo.toml lives.
stop=$(cd -- "$(dirname -- "$file")" 2>/dev/null && git rev-parse --show-toplevel 2>/dev/null) || stop=""
[ -n "$stop" ] || stop=$hook_root

edition=$(crate_edition "$file" "$stop") || exit 0
[ -n "$edition" ] || exit 0
rustfmt_knows_edition "$edition" || exit 0

if ! out=$(rustfmt --edition "$edition" --color=never --check -- "$file" 2>&1); then
  echo "rustfmt (edition $edition): $file is unformatted — run 'cargo fmt' before committing (CI's Lint gate will reject it):" >&2
  printf '%s\n' "$out" | head -20 >&2
  exit 2
fi
exit 0
