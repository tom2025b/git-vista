# ADR 0006 — Visualize / Active is chosen per open, every time

- **Status:** Accepted — implementation pending (`feature/repo-picker-modes`)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
- **Supersedes / superseded by:** —

## Context

Git-Vista is growing from "the repo it was launched in" to opening third-party
local repos and cloned public repos. Repos that are not the operator's own
work should default to a look-only **visualizer** experience, while the
operator's own repos need the full **active** mode. Someone has to decide,
per repo, which experience applies — and a wrong silent guess either blocks
real work or arms write buttons on a repo the operator only meant to read.

## Decision

Every time a repository is opened, the app shows a two-button mode screen:
**Visualize** or **Active**. The choice is not persisted; reopening the same
repo asks again. A topbar mode badge shows the current mode and reopens the
mode screen for the current repo.

## Alternatives considered

- **Remember the last choice per repo.** Rejected for now: it adds a
  persistence surface and a stale-answer failure mode (a repo that *was*
  someone else's becomes your fork) for the price of one tap saved. Can be
  revisited once real usage shows the prompt is friction.
- **A global default with a toggle** (e.g. everything visualize unless
  switched). Rejected: the dangerous mistake — active mode on a repo you
  meant to look at — becomes one stale global setting away.
- **Infer the mode** (e.g. own-remote heuristics). Rejected: remote
  ownership is a weak proxy for intent, and a wrong inference is invisible
  until a write lands.

## Consequences

- One extra tap on every repo open — accepted cost; the screen uses the
  iPad-proven full-screen-overlay pattern so it is fast on touch.
- No new persistence anywhere; the choice lives in the server-side selection
  (see ADR 0007).
- The LAN listener simply omits the Active button (and serves no write
  routes regardless — ADR 0005).
