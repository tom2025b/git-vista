# grok-review

Scratch folder holding review prompts for an external LLM (Grok) with GitHub read
access to this repo. Not application code, not durable project documentation — it
exists so a reviewer can be pointed at one folder instead of manually pasted files.

Each subfolder is one review round: a `PROMPT.md` naming real files in this repo at
their current paths on `main`, plus the specific question that round exists to answer.
Point your GitHub-connected session at the folder and let it fetch the named files
itself — that avoids the upload/attachment path entirely (a prior round lost its
source files to an iOS file-picker bug turning `.rs` uploads into empty bookmarks).

Rounds already answered and fixed are removed rather than left to confuse a fresh
pass — check `git log -- grok-review/` if you want the history of what was asked
and resolved.

**Signed:** thomas2025 · 2026-08-01
