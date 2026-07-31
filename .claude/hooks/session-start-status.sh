#!/usr/bin/env bash
# SessionStart hook — print the state a session would otherwise spend several tool
# calls rediscovering, and warn loudly about the two things that have actually cost
# work on this project.
#
# Why this exists:
#   1. A checkpointer died silently overnight and nobody noticed for hours. Every
#      session used to rediscover its own state by hand.
#   2. "Committed" is not "safe" — only pushed is. An unpushed branch dies with the box,
#      and this machine has been through a power outage and a tornado warning mid-session.
#
# Never fails the session: every command is guarded, and the hook always exits 0.
# A broken status line must not stop work.

set -uo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../.." 2>/dev/null || exit 0
command -v git >/dev/null 2>&1 || exit 0
git rev-parse --git-dir >/dev/null 2>&1 || exit 0

echo "── Git-Vista session state ─────────────────────────────"

branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')
head=$(git log --oneline -1 2>/dev/null || echo '?')
echo "branch  $branch"
echo "head    $head"

# Uncommitted work. Only meaningful alongside the checkpointer line below.
dirty=$(git status --porcelain 2>/dev/null | wc -l)
[[ $dirty -gt 0 ]] && echo "dirty   $dirty file(s) uncommitted"

# Pushed state. This is the line that matters for durability: a local commit is not
# a backup. `@{u}` fails when there is no upstream, which is itself worth saying.
if upstream=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null); then
  ahead=$(git rev-list --count "$upstream..HEAD" 2>/dev/null || echo 0)
  if [[ ${ahead:-0} -gt 0 ]]; then
    echo "⚠ UNPUSHED  $ahead commit(s) not on $upstream — a power cut loses them"
  else
    echo "pushed  in sync with $upstream"
  fi
else
  echo "⚠ NO UPSTREAM — nothing is backed up to GitHub"
fi

# The checkpointer. Matched on the script's own path so this hook's own command line
# can never match itself. Never use `pkill -f autocheckpoint` to stop one: that pattern
# matches the calling shell and kills it — a footgun that has fired here.
ckpt=$(pgrep -af 'local/bin/autocheckpoint' 2>/dev/null | grep -c "$PWD" || true)
if [[ ${ckpt:-0} -eq 0 ]]; then
  echo "⚠ NO CHECKPOINTER RUNNING — uncommitted work is the only work that gets lost."
  n=$(git log --oneline -200 2>/dev/null | grep -oP 'auto-checkpoint \K\d+' | head -1)
  echo "  start one, continuing the series from ${n:-<read git log>}:"
  echo "    START_N=\$(( ${n:-0} + 1 )) MAX_ROUNDS=480 setsid nohup ~/.local/bin/autocheckpoint \\"
  echo "      $PWD /tmp/gv-ckpt 60 66 > /tmp/gv-ckpt/ac.log 2>&1 < /dev/null & disown"
elif [[ ${ckpt:-0} -gt 1 ]]; then
  echo "⚠ $ckpt CHECKPOINTERS on this repo — two racing on one git index corrupt each"
  echo "  other. Kill all but one BY PID (never \`pkill -f autocheckpoint\`)."
else
  last=$(git log -1 --format=%cr --grep='auto-checkpoint' 2>/dev/null || echo '?')
  echo "ckpt    1 running · last auto-checkpoint $last"
fi

# handoff.md is the human-readable map; the commits are the durable bytes.
if [[ -f handoff.md ]]; then
  age=$(( ( $(date +%s) - $(stat -c %Y handoff.md 2>/dev/null || date +%s) ) / 3600 ))
  hdr=$(head -1 handoff.md 2>/dev/null | cut -c1-64)
  echo "handoff ${age}h old · $hdr"
  [[ $age -gt 12 ]] && echo "  ⚠ stale — read it, then correct it before trusting it"
else
  echo "⚠ no handoff.md — create one before doing non-trivial work"
fi

cat <<'EOF'

Standing cautions, all earned here:
  · Plan citations have been wrong SIX times, including a function that never existed.
    Verify against source; never paste a citation you have not opened.
  · A green test that proves nothing is worse than a red one — six occurrences.
    Ask: what would make this pass while the mechanism was broken?
  · Never assert a mapping by calling the function that defines it.
  · Running both Claude accounts at once: when one exhausts, BOTH report a session
    limit and every subagent dies while the main agent keeps working. Kill all
    terminals, relaunch from the project dir.
────────────────────────────────────────────────────────
EOF

exit 0
