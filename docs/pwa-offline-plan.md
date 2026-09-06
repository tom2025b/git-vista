# #75 implementation checkpoint

Current main already installs via the manifest/iOS metadata and intentionally
ships no service worker (ADR 0032). #244 describes verification of that design.
Keep that contract: no new persistent app-shell or repository-response cache.
HTTP static assets revalidate; API responses remain no-store.

Close concrete gaps without claiming a device check:

- Add browser verification for offline refusal/no replay and for a previously
  loaded client encountering the reported server v9–v9 mismatch.
- Check offline again inside each write transport attempt, including the
  existing immediate idempotency-key retry. Never wait for an online event to
  dispatch a write; operation recovery only observes an already-started write.
- Provide honest local browser-data controls in Settings: export only an
  explicit allowlist of UI preferences, clear app-owned persisted data with
  explicit disclosure of drafts/tracking loss. Browser HTTP cache eviction is
  a browser setting, not something the application can promise to clear.
- Update the stale storage inventory (commit drafts now use localStorage).
  Record the policy in provisional ADR 0135 (0132 is now occupied on main;
  open PRs claim 0131/0133/0134).

Verification: host policy tests, two disjoint Atlas mutations, actual Playwright
offline/reconnect and mismatch paths, then all-targets and full gate. Browser
tests must override the harness's forced-online signal for offline cases.
Actual iPad installation/launch and the physical input matrix remain unmet;
the eventual PR will say Refs #75, not Closes #75.

Signed: codex
