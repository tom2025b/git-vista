# ADR 0010 — Visualizer = the existing read-only views plus forge deep links

- **Status:** Accepted — implementation pending (`feature/repo-picker-modes`)
- **Date:** 2026-07-19
- **Milestone / issue:** Post-M1.05 feature set; design spec
  `docs/superpowers/specs/2026-07-19-repo-modes-lan-visualizer-design.md`
- **Supersedes / superseded by:** —

## Context

With the visualize/active split decided (ADR 0006/0007), the open question
was what the visualizer *shows*. The existing app already renders a strong
read-only surface: graph, commit detail, diffs, file listings. The missing
piece for "looking at someone else's repo" is the jump out to where that
repo actually lives — its forge (GitHub, GitLab, Codeberg, …) — for
everything Git-Vista doesn't render (issues, PRs, CI, blame).

## Decision

Visualize mode is the current read-only view set **plus forge links**:

- `RepositoryDescriptor` / `Graph` gain an optional `remote_web_url`: the
  `origin` remote URL normalized to a browsable https form (GitHub, GitLab,
  Codeberg patterns; unknown hosts get the normalized base URL only). New
  DTO fields use `#[serde(default)]` / skip-serializing-if per the M1.02
  versioned-contract rules.
- UI: "View commit on <host>" in the detail panel, a repo link in the
  topbar, a branch link in the branch context menu. All
  `target="_blank" rel="noopener"`. Absent when the repo has no usable
  remote.

## Alternatives considered

- **A richer bespoke read-only suite** (blame view, file-history browser,
  contributor stats…). Deferred as YAGNI: the forge already renders all of
  it; Git-Vista's value here is the touch-first graph, not re-implementing
  a forge.
- **Embedding forge pages** (iframe/webview). Rejected: forges send
  `X-Frame-Options`/CSP that forbid framing, and our own CSP
  (`frame-ancestors 'none'`, `default-src 'self'`) is deliberately hostile
  to embedding. Links out are honest and cheap.
- **Links only for known forges.** Softened: unknown hosts still get the
  normalized base URL rather than nothing — a best-effort link beats a
  dead end.

## Consequences

- A unit-test matrix for `remote_web_url` normalization (github/gitlab
  ssh-form, https-form, unknown host, no remote).
- No new read endpoints and no scraping; the server only normalizes a URL
  it already knows from the repo config.
- Visualize mode stays useful offline-from-the-forge: everything local
  still renders; only the links go dark.
