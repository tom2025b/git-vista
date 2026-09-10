# #755 closeout — transport is decided; checkout remains owned

Reviewed against `d3c238ca` on 2026-09-10.

**Decision:** retire #755 as the parent transport finding. Its transport
claims are fixed or explicitly accepted, and its checkout/filter remainder has
an existing open owner in [#782](https://github.com/tom2025b/git-vista/issues/782).
Closing the parent does not declare that checkout/filter work complete.

## Claim ledger

| #755 claim or residual | Disposition | Evidence or owner |
|---|---|---|
| Repository configuration can name programs that execute during a Network-tier transport spawn even when hooks are disabled. | Measured. Ten reached selectors include pack programs, SSH, credential helper, askpass, fsmonitor, Git proxy, `ext::`, and installed remote helpers. | [ADR 0144](../adr/0144-network-spawns-use-server-authored-transport-programs.md#measurement-markers-not-a-config-source-inference), landed in [PR #775](https://github.com/tom2025b/git-vista/pull/775). |
| Direct arbitrary-command selectors need server-authored values. | Fixed. The shared launcher clears askpass and configured credential helpers, selects ordinary `ssh`, disables fsmonitor and `ext::`, and supplies standard upload/receive-pack programs for the request-selected remote. | [ADR 0144's decision](../adr/0144-network-spawns-use-server-authored-transport-programs.md#decision) and [PR #775](https://github.com/tom2025b/git-vista/pull/775). |
| `core.gitProxy` remained executable while native `git://` stayed enabled. | Fixed without disabling native Git. Every Network command gets an empty `GIT_PROXY_COMMAND`, which bypasses configured proxy commands. | [ADR 0144's #779 amendment](../adr/0144-network-spawns-use-server-authored-transport-programs.md#779-amendment--fixed-transport-policy-2026-09-09) and [PR #797](https://github.com/tom2025b/git-vista/pull/797). |
| Repository policy could select installed or worktree-relative `git-remote-*` helpers. | Fixed for the reachable selector. The sealed `GIT_ALLOW_PROTOCOL=http:https:ssh:git:file` blocks `remote.<name>.vcs`, including a slash-shaped worktree-relative value. | [ADR 0144's #779 amendment](../adr/0144-network-spawns-use-server-authored-transport-programs.md#779-amendment--fixed-transport-policy-2026-09-09) and the behavioral coverage landed in [PR #804](https://github.com/tom2025b/git-vista/pull/804). |
| Path-shaped `url`, `insteadOf`, `pushurl`, and `pushInsteadOf` might dispatch a worktree helper. | Explicitly accepted as an upstream dependency, not credited to Git-Vista's allowlist. Git 2.53.0 rejects these at its `<scheme>::` parser; the real-Git tripwire requires the selector to fire, the helper to remain absent, and the failure not to be an allowlist denial. | [ADR 0145](../adr/0145-four-path-shaped-selectors-depend-on-upstream-git.md) and [PR #804](https://github.com/tom2025b/git-vista/pull/804). |
| Pinning might break legitimate operator configuration. | Decided and documented. Proxy commands, custom helpers, repository credential helpers, SSH wrappers, fsmonitor programs, custom pack programs, and `ext::` remotes are deliberate compatibility losses; inherited operator command environment remains trusted. | [ADR 0144's compatibility record](../adr/0144-network-spawns-use-server-authored-transport-programs.md#compatibility-cost). |
| LFS and general filters can name executables during the separately spawned checkout. | Measured and deliberately not folded into the transport fix. System `filter.lfs.process` and an operator-global generic smudge filter execute under the checkout sandbox. The landed record's unmeasured custom-transfer/extension cases and the policy decision remain owned by the open #782. | [The landed #782 investigation](2026-09-09-issue-782-lfs-selectors.md#5-verdicts), from [PR #816](https://github.com/tom2025b/git-vista/pull/816), and [#782](https://github.com/tom2025b/git-vista/issues/782). |
| A tracked `.lfsconfig` might directly provide those executable selectors. | Not a landed premise at the reviewed base. The landed investigation explicitly left the safe-key claim unverified. PR #823 was open and carried a direct measurement; its eventual disposition does not affect ownership because #782 already owns the question. | [The landed investigation's gap list](2026-09-09-issue-782-lfs-selectors.md#6-honest-gap-list), [#782](https://github.com/tom2025b/git-vista/issues/782), and [PR #823](https://github.com/tom2025b/git-vista/pull/823). |

## Current-source check

The production tree still matches the decisions above:

- `sandbox::policy_for` grants a writable repository and separate commondir,
  so `.git/config` remains inside the Network operation's grant.
- `network_exec::compose_network_args` supplies the five fixed config values
  and the request-independent upload/receive-pack options.
- All three Network constructors call
  `SandboxedCommand::with_network_transport_policy`; its completion-time policy
  supplies the fixed protocol list and empty proxy environment.
- Checkout remains intentionally different rather than accidentally omitted:
  `network_command_without_credential` applies both the transport policy and
  `with_untrusted_checkout_env`, whose allowlist preserves `HOME` and
  `XDG_CONFIG_HOME` so operator filter configuration continues to work.

## Why the parent can retire

#755 has no remaining unowned claim. Keeping it open would duplicate the
transport decisions already recorded in ADRs 0144 and 0145 while obscuring the
more precise boundary: #782 is the sole open owner for checkout/filter
measurement and policy. Conversely, treating #782 as finished would be wrong;
its constraint decision and some direct selector measurements remain live.

No new test or mutation is needed for this documentation-only decision. The
behavioral and two-arm mutation evidence belongs to the implementation records
above; this record changes neither a guard nor its test.

**Signed:** codex · 2026-09-10
