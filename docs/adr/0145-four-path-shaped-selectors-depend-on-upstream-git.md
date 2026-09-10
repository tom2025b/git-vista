# ADR 0145 — Four path-shaped selectors depend on upstream Git, with a tripwire

- **Status:** Accepted
- **Date:** 2026-09-10
- **Issue:** #817
- **Extends:** [ADR 0144](0144-network-spawns-use-server-authored-transport-programs.md) — narrows its #779 amendment's per-route claim. Nothing in 0144 is retracted.

## Context

ADR 0144's #779 amendment says the fixed
`GIT_ALLOW_PROTOCOL=http:https:ssh:git:file` policy covers helper selection
through `remote.<name>.vcs`, URLs, push URLs and URL rewrites. That statement
is too broad for the path-shaped case added by #804. Five green selector
routes do not have one common load-bearing guard.

`network_command`, `network_command_with_credential` and
`network_command_without_credential` all call
`with_network_transport_policy()`. At completion, that sets the fixed
`GIT_ALLOW_PROTOCOL` value in `sandbox/spawn.rs`; there is no production branch
that recognizes one selector route differently from another.

The behavioral tests in `sandbox/network_exec.rs` nevertheless demonstrate
two different outcomes:

| Path-shaped route | Layer that prevents helper execution today |
|---|---|
| `remote.<name>.vcs` | Git-Vista's fixed `GIT_ALLOW_PROTOCOL` policy |
| `url` | upstream Git's `<scheme>::<address>` parser |
| `insteadOf` | upstream Git's `<scheme>::<address>` parser |
| `pushurl` | upstream Git's `<scheme>::<address>` parser |
| `pushInsteadOf` | upstream Git's `<scheme>::<address>` parser |

For `remote.<name>.vcs`, the unhardened control executes a tracked
worktree-relative helper and the hardened run reports a protocol denial. For
the other four routes, the test removes Git-Vista's transport policy and the
helper still does not execute. Git rejects `/` in the would-be scheme, treats
the complete value as a local path, and fails before helper dispatch. The test
also proves each rewrite fired and requires that hardened failures are not
`GIT_ALLOW_PROTOCOL` denials.

The mutation results that prompted this decision agree with those controls.
Removing `with_network_transport_policy()`, and separately widening
`GIT_ALLOW_PROTOCOL` to admit `../r1`, were both caught by the `vcs` test and
both survived the four-route test. The survivors are evidence about ownership:
Git-Vista's pin is not the mechanism producing those four current outcomes.

## Decision

Accept upstream Git's scheme parser as the runtime guard for path-shaped
`url`, `insteadOf`, `pushurl` and `pushInsteadOf` selectors. Retain
`path_shaped_url_based_selectors_never_reach_helper_dispatch` as a tripwire on
that dependency. Do not add a first-party parser or config filter now.

This is an acceptable dependency because Git already owns remote URL parsing,
URL-rewrite precedence and helper dispatch. The test exercises those real Git
semantics rather than a Git-Vista model: all four routes, both fetch and push,
an installed marker at the path dispatch would reach, an unhardened control,
and every Network command wrapper. It checks both that the selector took effect
and that the helper did not run. A supported Git upgrade that starts treating a
slash-shaped prefix as a helper scheme therefore makes the test fail loudly.

The layer is stated narrowly. For these four routes, upstream parsing is the
only layer proved to be holding. The tripwire is detection, not runtime
defence. Git-Vista's fixed `GIT_ALLOW_PROTOCOL` remains in place for every
Network spawn, but the current four-route test cannot prove what that pin would
do after Git changes the parsing rule that currently runs first. Layered
defence means a future change must not remove the pin; it does not permit this
record to credit an unproved fallback as a second layer.

Reconsider this decision before accepting any of these events:

- a supported Git version accepts `/` in the helper-scheme position;
- the tripwire observes a transport-policy denial instead of the parser's
  local-path rejection, even if the marker remains absent;
- Git-Vista begins interpreting, normalizing or reconstructing these selector
  values instead of handing them to Git; or
- Git-Vista deliberately adds path-shaped custom helper schemes as a supported
  capability.

At that point, do not update the expected error and move on. Either add a
first-party enforcement layer, or establish the existing transport pin as the
new load-bearing layer with a positive control and two distinct mutations.

## Alternatives considered, and why they lost

### Add first-party control for all four routes now

A first-party boundary could resolve the fetch and push URLs selected by the
named remote, reject path-shaped helper schemes, and pass only sealed values to
the Network spawn. This is attractive because it would put the decision under
Git-Vista's control and leave upstream parsing as an additional layer.

Rejected for now because a sound implementation is not a four-string check.
It must reproduce or safely delegate Git's config precedence, multiple URL
values, longest-match `insteadOf`/`pushInsteadOf` rewriting, fetch-versus-push
selection, includes and worktree config. A preflight read alone also races the
same mutable repository configuration the later Git process consumes; closing
that race requires sealing a resolved remote description or constructing a
sanitized config for the child. Either shape adds a new parser/config boundary,
compatibility policy and mutation-proved test matrix. That cost is not
justified while real Git makes the route unreachable and a tripwire watches
the exact dependency.

### Treat `GIT_ALLOW_PROTOCOL` as the four routes' second layer

Rejected as a present-tense guarantee. It is plausible that the fixed allowlist
would reject a newly accepted slash-shaped scheme, but today's Git never lets
that value reach the protocol check. Both transport-policy mutations surviving
is exactly why the existing test cannot prove the fallback. Calling it layered
defence would turn an unexercised hypothesis into a sandbox claim.

### Collapse the five routes into one statement that the sandbox blocks them

Rejected because it erases the decision this ADR exists to preserve. All five
helpers stay absent, but only `remote.<name>.vcs` currently needs Git-Vista's
transport policy to make that true. Equal outcomes are not evidence of equal
guards.

## Consequences

- No production behavior changes. The fixed transport allowlist remains
  mandatory on Network commands and continues to hold the path-shaped `vcs`
  route closed.
- Four routes deliberately depend on a third-party parser guarantee. A Git
  parser change becomes a review event rather than an expected-output update.
- The tripwire must continue to assert both the absent marker and the
  non-allowlist failure signature. Weakening either would make a change of
  enforcement layer quiet.
- Supporting older or newer Git releases carries the cost of running this
  behavioral tripwire against the real binary; a host test of Git-Vista code
  alone cannot establish upstream parser behavior.
- Adding a first-party layer later must be additive: upstream Git's rejection
  and the fixed `GIT_ALLOW_PROTOCOL` pin stay in place wherever they still
  apply.

## Verification

The decision was checked against the production constructors and completion
policy in `crates/git-vista-server/src/sandbox/network_exec.rs` and
`crates/git-vista-server/src/sandbox/spawn.rs`, and against both selector tests'
unhardened and hardened controls. No production code or pinning test is added
by this ADR.

Signed: **codex** · 2026-09-10
