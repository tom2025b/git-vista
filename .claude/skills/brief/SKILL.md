---
name: brief
description: Produce a fresh living-brief snapshot for this repo via the brief-mcp server - carrying the previous document forward rewritten for the new situation, storing it in SQLite, rendering md+PDF, and updating the stable current.pdf. Use after closing an issue, hitting a milestone or roadblock, before/after a workflow (pre-flights MUST be recorded here before launch), or whenever the user asks for a brief or the living doc.
---

# The Living Brief

One brief per repo, served by brief-mcp (registered for both Claude accounts
and codex). The owner reads exactly one path:
`~/Documents/briefs/Git-Vista/current.pdf` — the server overwrites it on every
render of the latest snapshot.

## Rules that are not optional

- **Fresh document each time**: read the prior snapshot, carry forward what
  still matters, REWRITE it for the new situation. Never an append log.
- **Real local timestamp** in the header AND the signature
  (`date --iso-8601=seconds`); the rendered filename carries it automatically.
- **Workflow pre-flights go IN the brief BEFORE launching** (owner's global
  rule): tier, agents, models, jobs, stopping condition, what it will NOT do.
  Keep them after the run, paired with the outcome.
- Standing sections ride forward: the command card, the honest milestone bars
  (regenerate counts when they move — shipped vs cut split, cuts outside the
  bar), open threads.
- Sign as your session account tag. Trigger field: freeform, honest.

## Mechanics

1. `brief_current` (repo "Git-Vista") -> prior content + the id to pass as
   `parent_id`.
2. Compose the fresh full markdown.
3. `brief_generate` with content, parent_id, repo, trigger, author. A
   conflict reply means another agent published first: re-read, reconcile,
   retry with the new parent — never overwrite.
4. `brief_render` (repo "Git-Vista") -> timestamped md+pdf in history plus
   the current.pdf slot update.
5. Send the PDF to the owner when the update is one they asked about.
