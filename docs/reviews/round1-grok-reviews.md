# Round 1 · Grok reviews

Reviewer only. Diffs, not PR descriptions. Append as each PR opens.

```yaml
started_at: 2026-09-08T21:57:11-04:00
role: REVIEWER ONLY
tag: CLEAR
sign_as: grok
assigned_issues: [757, 752, 357, 748, 762, 89, 396]
```

## Queue at session start

No open PRs in `tom2025b/Git-Vista`. All seven assigned issues are still open with `closed_by_pull_requests.total_count = 0`. All seven lane worktrees are still on `wip/<issue>-placeholder` at `0944c523` (same as `origin/main`). Nothing to review yet.

Territory note for later: the reviewer handoff said #757/#752 must not touch `crates/git-vista-ui/**`. Claude 2's own #752 handoff explicitly allows `crates/git-vista-ui/src/activity/**` because that is where the unscoped tag/stash reads live. When #752's PR opens, judge it against the lane's `allowed_paths`, not the reviewer summary's sandbox-only blanket.

---

## #702 — spare-capacity bookkeeping (not a coding task)

**Verdict: CLOSE as completed.** Coordinator (max, territory A) can close it; this is not a review of a new PR.

Reason, from the issue graph and what is on `origin/main`, not from anyone's session report:

- #723's own acceptance said "#702 closes only when this lands." #723 is **CLOSED / COMPLETED** by merged PR #729 (`fix: deny pathname AF_UNIX during clone checkout`, merged 2026-09-08T11:59:24Z, `Refs #702, Closes #723`).
- The 16-hour plan also said #702 closes when #735 and #744 are answered. Both are **CLOSED / COMPLETED** (#735 at 20:04Z, #744 at 21:56Z). Sibling PRs: #720 (allowlist, refs #702, merged), #761 (enumeration of Network-tier hook spawns, merged).
- Current `origin/main` carries the checkout-specific seccomp path: `checkout_command_async` / `SeccompProfile::Checkout` / `--seccomp-checkout` in `crates/git-vista-server/src/sandbox/`.
- Leftover branch `origin/fix/702-clone-checkout-credential-allowlist` is an ancestor of `origin/main` (empty diff). No remaining commits.
- #702 itself still has `closed_by_pull_requests.total_count = 0` because no PR used `Closes #702` — they used `Refs #702`. That is stale bookkeeping, not remaining work.

Re-derived from PR #729's merged tree (`b9f08e3c`), not the PR body, after Tom asked the remaining-gap question. Mutation arms were **not** re-run.

**Does #729 actually deny pathname AF_UNIX during clone checkout?** Yes. That is the exact capability #702 said #720 left open (env allowlist removes the locator; a hook can still read a path out of `$HOME` and connect).

The production chain, from the clone handler to the filter:

1. `handlers/clone.rs` post-transfer spawns are only `network_command_without_credential(&CheckoutPolicy, …)` — HEAD check and `checkout -f`. Transfer stays on `network_command_with_credential`.
2. That sealed helper calls `spawn::checkout_command_async` → `full_checkout_argv` → `checkout_sandbox_argv` → `shim_argv(..., SeccompProfile::Checkout)`, which emits `--seccomp-checkout` and `--net-allow`. Ordinary `sandbox_argv` does not emit the flag (pinned in `argv.rs`).
3. Shim `main.rs` maps `(net_allow=true, seccomp_checkout=true)` to `NetScope::Checkout`.
4. `seccomp_filter::rules_for(Checkout)` inserts the same `af_unix_rule` Strict uses on `SYS_socket` and `SYS_socketpair`: first argument `== AF_UNIX` → `EPERM`. `SYS_connect` is not named. For a freshly exec'd hook that is equivalent: you cannot `connect()` a pathname socket without first creating an AF_UNIX fd, and Landlock still does not mediate pathname `AF_UNIX` (that is why this rule exists).
5. The new `checkout_security.rs` test is the composed proof of #702's remaining attack, not an argv scan: parent has no `SSH_AUTH_SOCK`; a fetched `post-checkout` hook sources `$HOME/.keychain/fixture-sh`, exports the path, then `socket(AF_UNIX)` + `connect()`. Expected output is `unix=errno:EPERM` with TCP still connecting.

Ordinary Network transfer still allows AF_UNIX on purpose (#188 / authenticated SSH). That is not a leftover of #702.

**#702 is stale bookkeeping.** Close it.

---

## Reviews

*(none yet — waiting on the first open PR among #757 #752 #357 #748 #762 #89 #396)*

**Signed:** grok · 2026-09-08T22:05:00-04:00
