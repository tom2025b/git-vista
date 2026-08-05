#!/usr/bin/env bash
set -uo pipefail
REPO=/home/tom/projects/Git-Vista
SCRATCH=/tmp/claude-1000/-home-tom-projects-Git-Vista/27c9f57f-3e25-4748-8d6c-32672e51f7e0/scratchpad
cd "$REPO"
land() {
  n=$1; br=$2; subj=$3
  echo "=== PR $n ($br) ==="
  git fetch --force origin '+refs/heads/*:refs/remotes/origin/*' >/dev/null 2>&1
  WT=$SCRATCH/land2-$n; rm -rf "$WT"
  git worktree add --detach "$WT" "origin/$br" >/dev/null 2>&1
  if ! git -C "$WT" -c user.name=Claude_Max -c user.email=262510778+tom2025b@users.noreply.github.com merge origin/main --no-edit >/dev/null 2>&1; then
    echo "  CONFLICT — stopping"; return 1
  fi
  git -C "$WT" push origin "HEAD:$br" >/dev/null 2>&1
  for i in $(seq 1 90); do
    out=$(gh pr checks "$n" 2>&1)
    n_ck=$(echo "$out" | grep -cE 'pass|fail|pending')
    if [ "$n_ck" -ge 7 ] && ! echo "$out" | grep -q pending; then break; fi
    sleep 25
  done
  fails=$(gh pr checks "$n" 2>&1 | grep -c fail)
  [ "$fails" -gt 0 ] && { echo "  RED — stopping"; gh pr checks "$n" | grep fail; return 1; }
  for t in 1 2 3 4 5; do
    st=$(gh pr view "$n" --json mergeStateStatus --jq .mergeStateStatus)
    [ "$st" = "CLEAN" ] && break; sleep 10
  done
  gh pr merge "$n" --merge --subject "$subj" >/dev/null 2>&1
  sleep 4
  echo "  $(gh pr view "$n" --json state,mergeCommit --jq '"\(.state) \(.mergeCommit.oid[0:7] // "-")"')"
}
land 322 fix/316-error-surfacing "fix(#316): errors as words in the app's modal, never wire JSON in an alert() (#322)" || exit 1
git checkout -q main && git pull -q --ff-only origin main
git -C ~/projects/git-vista-mirror pull -q --ff-only && echo "mirror refreshed"
echo "=== BOTH LANDED ==="
