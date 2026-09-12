#!/usr/bin/env bash
set -uo pipefail
REPO=/home/tom/projects/Git-Vista
SCRATCH=/tmp/claude-1000/-home-tom-projects-Git-Vista/27c9f57f-3e25-4748-8d6c-32672e51f7e0/scratchpad
cd "$REPO"

served_repo_from_process() {
  local proc cwd repo
  local -a argv matches=()

  for proc in /proc/[0-9]*; do
    [[ -r $proc/cmdline ]] || continue
    argv=()
    mapfile -d '' -t argv < "$proc/cmdline" 2>/dev/null || continue
    [[ ${#argv[@]} -gt 0 ]] || continue
    [[ ${argv[0]##*/} == git-vista-server ]] || continue
    if [[ ${#argv[@]} -lt 2 || -z ${argv[1]} ]]; then
      echo "ERROR: running git-vista-server PID ${proc##*/} has no repository argument" >&2
      return 2
    fi
    repo=${argv[1]}
    if [[ $repo != /* ]]; then
      if ! cwd=$(readlink -f "$proc/cwd"); then
        echo "ERROR: cannot resolve cwd for git-vista-server PID ${proc##*/}" >&2
        return 2
      fi
      repo=$cwd/$repo
    fi
    matches+=("$repo")
  done

  case ${#matches[@]} in
    0) return 1 ;;
    1) printf '%s\n' "${matches[0]}" ;;
    *)
      echo "ERROR: multiple git-vista-server processes are running; refusing to guess which checkout to refresh" >&2
      printf '  %s\n' "${matches[@]}" >&2
      return 2
      ;;
  esac
}

refresh_app_mirror() {
  local served_repo=${1:-} branch rc

  if [[ -z $served_repo ]]; then
    served_repo=$(served_repo_from_process)
    rc=$?
    if [[ $rc -eq 1 ]]; then
      echo "app checkout refresh skipped: no git-vista-server process is running"
      return 0
    fi
    [[ $rc -eq 0 ]] || return "$rc"
  fi

  if [[ ! -d $served_repo ]] || ! git -C "$served_repo" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "ERROR: served app repository is not a Git checkout: $served_repo" >&2
    return 1
  fi
  branch=$(git -C "$served_repo" symbolic-ref --quiet --short HEAD 2>/dev/null || true)
  if [[ $branch != main ]]; then
    echo "ERROR: served app repository is on '${branch:-detached HEAD}', not main: $served_repo" >&2
    return 1
  fi

  echo "refreshing served app checkout: $served_repo"
  if ! git -C "$served_repo" pull -q --ff-only origin main; then
    echo "ERROR: failed to fast-forward served app checkout: $served_repo" >&2
    return 1
  fi
  echo "served app checkout refreshed: $served_repo"
}

land() {
  n=$1; br=$2; subj=$3
  echo "=== PR $n ($br) ==="
  state=$(gh pr view "$n" --json state --jq .state 2>/dev/null)
  if [ "$state" = "MERGED" ]; then
    echo "  already MERGED — skipping (this script is a per-run landing log, not safe to blindly re-execute)"
    return 0
  fi
  if [ "$state" = "CLOSED" ]; then
    echo "  CLOSED (not merged) — stopping"; return 1
  fi
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
main() {
  land 796 docs/deadcode-triage-round2-census "docs(census): document 4 round2 name-collision cases in EXEMPT (#796)" || exit 1
  land 781 test/357-ui-interaction-selection "test(#357): cover staging line selection through browser interactions (#781)" || exit 1
  git checkout -q main && git pull -q --ff-only origin main
  if ! refresh_app_mirror; then
    echo "=== LANDING INCOMPLETE: SERVED APP CHECKOUT NOT REFRESHED ===" >&2
    exit 1
  fi
  echo "=== BOTH LANDED ==="
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  main "$@"
fi
