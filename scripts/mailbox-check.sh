#!/usr/bin/env bash
# Mailbox validator (Q1) — turns the parallel-lane fences from promises into
# checks. Delegates to mailbox_check.py (dependency-free stdlib Python) so the
# glob/YAML-lite parsing isn't hand-rolled in bash; this wrapper just resolves
# paths and forwards args, matching the project's other `./dev`/`./gv`
# shell-wrapper-over-real-logic shape.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
exec python3 "$SCRIPT_DIR/mailbox_check.py" "$@"
