# ADR 0014 — LAN-view sessions present read-only affordances only

- **Status:** Accepted — implemented 2026-07-22 (`fix/gv-lan-view-token-message`,
  commits `796e0c5` / `519042f`)
- **Date:** 2026-07-22
- **Milestone / issue:** Post-ADR-0005 hardening — surfaced by the first
  sustained live use of the LAN view from an iPad
- **Supersedes / superseded by:** — (amends the *client-side* consequences of
  ADR 0005; the server contract is unchanged)

## Context

ADR 0005 made the LAN listener structurally read-only: the write / selection /
clone / rescan routes are never registered on it, so a `POST` there falls
through to the static-file fallback and dies as a bare `405 Method Not Allowed`
with an empty body (the ADR's own implementation note records this).

ADR 0005's consequence line — "the client's mode screen offers Visualize only
on LAN" — was implemented (`d29d396`), but that was the *only* client
concession to the LAN profile. Everything else still rendered as if writes
were possible. First real iPad use (2026-07-22) hit every resulting wall in
one afternoon:

1. **Blank failure dialogs.** "Clone URL…" was offered on a LAN session;
   submitting surfaced the bare 405's empty body as `Couldn't clone:` followed
   by nothing. The operator retried four repositories, concluding the app was
   broken rather than the operation forbidden.
2. **A dead-end ask-every-time picker.** ADR 0006's picker auto-opened on
   load. On a LAN session every choice it offers leads to `POST /api/select` —
   a route the LAN listener doesn't have — so the picker was a lobby of doors
   that don't open.
3. **A trap, not just a dead end.** With a ~20-repo root the picker's action
   row (including Cancel) sat below the scroll fold on iPad; the operator
   could not reliably dismiss the overlay at all.

## Decision

Three coordinated client-side rules; the server's route absence remains the
actual security boundary (per ADR 0005's "an unregistered route cannot
regress"):

1. **A client chokepoint mirrors the structural absence.** `api.rs` gains
   `refuse_if_lan_view()` — the ADR 0007 chokepoint pattern — in front of
   `clone_request`, `select_request`, `rescan_request`, and
   `delete_clone_request`. A LAN session gets a one-line, human-readable
   refusal instead of a wire 405. The clone error path also goes through the
   shared `response_error()`, so even an empty error body reports
   `HTTP <status>` rather than nothing.
2. **Affordances that cannot succeed are not rendered.** On a LAN session the
   topbar's "Open URL…", and the picker's "Clone URL…" / "Rescan" / per-row
   "Delete" buttons don't render; picker repo rows become inert labels. Each
   site keys on the session resource (or the post-session `reload` bump), so
   the flag is settled before the affordance is drawn.
3. **The ask-every-time picker yields to the LAN profile.** ADR 0006's
   auto-open still applies to loopback sessions, but a LAN session closes the
   picker as soon as the session lands and shows the served repository's graph
   immediately. Separately (all sessions), the picker modal became a flex
   column whose repo list is the only scrolling region, so the action row can
   never scroll out of reach again.

## Alternatives considered

- **A JSON-error catch-all for absent write routes on the LAN router.** Would
  fix the blank dialogs at the wire instead of in the client, and help unknown
  future clients. Deferred, not rejected: it reintroduces per-route
  registration on the LAN listener, exactly what ADR 0005's "structurally
  absent" stance avoids, and the client chokepoint covers today's only client.
- **Keeping the affordances visible but disabled with an explanation.**
  More discoverable, but a grid of permanently dead buttons on a view-only
  profile reads as breakage — and ADR 0005's spirit is that the LAN view *is*
  a different, smaller product surface, not a degraded full one.
- **Doing nothing and documenting the 405.** The afternoon's live session is
  the counter-evidence: four clone retries and an untrappable overlay are not
  a documentation problem.

## Consequences

- A LAN-view session now looks like what it is: a read-only window onto the
  repository the server is currently showing. View, pan, print, follow forge
  links; nothing else is offered.
- The refusal text ("This is a read-only LAN view session — open the localhost
  (SSH-tunnel) link to clone, rescan, or switch repositories.") names the
  sanctioned alternative, teaching the two-profile model at the exact moment
  it matters.
- **Known remaining gap:** the commit-node context menu still offers
  write actions (create branch, rebase, stage/commit) on a LAN session when
  the current repository's mode is Active, and the topbar mode badge shows
  "Active" — both read from the repo's server-side mode, which is
  session-independent. Those paths still dead-end in client-side mode/CSRF
  refusals rather than blank dialogs, but the affordances should follow this
  ADR's rule 2 in a follow-up.
- The picker's pinned action row applies to every session type — a general
  iPad usability repair that rode along with the LAN work.
