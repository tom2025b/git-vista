#!/usr/bin/env bash
# Install the three reviewed snapshots using sibling temp files and atomic mv.
# Run outside the sandbox PID namespace so pgrep sees the actual consumers.
# --rollback restores the preserved originals, with the same live-process gate.
set -euo pipefail
umask 077
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DEST=/home/tom/.local/bin
MODE="${1:-install}"
[[ "$MODE" = install || "$MODE" = --rollback ]] || { echo 'usage: install.sh [--rollback]' >&2; exit 2; }
read -r init_comm < /proc/1/comm
case "$init_comm" in codex*) echo 'refusing: process check must run outside the sandbox PID namespace' >&2; exit 1 ;; esac
scripts=(batch-land buildlock lane-pr-requests)
declare -a staged=()
cleanup() {
  local tmp
  for tmp in "${staged[@]}"; do [ ! -e "$tmp" ] || rm -f -- "$tmp"; done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Check the complete set before staging any replacement. Never overwrite bytes
# another coordinator changed since our original snapshot (or since install).
for script in "${scripts[@]}"; do
  source="$HERE/$script"; expected="$HERE/$script.before"
  if [ "$MODE" = --rollback ]; then source="$expected"; expected="$HERE/$script"; fi
  [ -f "$DEST/$script" ] && [ ! -L "$DEST/$script" ] || { echo "refusing nonregular target: $script" >&2; exit 1; }
  cmp -s "$expected" "$DEST/$script" || { echo "refusing changed live bytes: $script" >&2; exit 1; }
  bash -n "$source"
  tmp=$(mktemp "$DEST/.daybreak-$script.XXXXXXXX")
  staged+=("$tmp")
  cp -- "$source" "$tmp"
  chmod --reference="$DEST/$script" "$tmp"
  cmp -s "$source" "$tmp"
done

for i in "${!scripts[@]}"; do
  script="${scripts[$i]}"
  expected="$HERE/$script.before"
  [ "$MODE" != --rollback ] || expected="$HERE/$script"
  cmp -s "$expected" "$DEST/$script" || { echo "refusing changed live bytes: $script" >&2; exit 1; }
  echo "TARGET: $DEST/$script"
  date -u '+CHECKED: %Y-%m-%dT%H:%M:%SZ'
  echo "COMMAND: pgrep -af 'batch-land|buildlock|lane-pr-requests'"
  if processes=$(pgrep -af 'batch-land|buildlock|lane-pr-requests'); then
    printf '%s\n' "$processes"
    echo 'REFUSED: a matching process is present; no further installation performed' >&2
    exit 1
  else
    rc=$?
    [ "$rc" -eq 1 ] || { echo "pgrep failed: $rc" >&2; exit 1; }
    printf '%s' "$processes"
    echo 'PGREP STDOUT: <empty>; exit status: 1'
  fi
  # Nothing opens or writes the live destination before this atomic rename.
  mv -T -- "${staged[$i]}" "$DEST/$script"
  sha256sum "$DEST/$script"
  echo 'ATOMIC REPLACE: complete'
done
