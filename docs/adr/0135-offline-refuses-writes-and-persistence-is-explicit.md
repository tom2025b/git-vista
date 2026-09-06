# ADR 0135 — Offline refuses writes and persistence is explicit

- **Status:** Proposed — implemented; physical device verification outstanding
- **Date:** 2026-09-06
- **Issue:** #75; verification follow-up #244
- **Relationship:** Extends ADR 0032; retains its no-service-worker decision.

## Context

Installation metadata, offline UI and the protocol mismatch overlay already
exist. ADR 0032 intentionally rejects a service worker. A disconnected cold
launch reports a connection failure; an already-loaded tab can retain its last
rendered graph in memory. Neither promises current repository data offline.

The reported live failure is a v12 client against a server accepting v9–v9.
Protocol negotiation must remain a fresh read even when static assets persist.
The existing Update Required overlay blocks pointer, keyboard and accessibility
access to the application behind it.

Two gaps surfaced: the immediate write retry did not recheck connectivity, and
the iPad document still described commit drafts as session-only after they moved
to localStorage. Calling the cache controls satisfied "vacuously" hid actual
persisted data.

## Decision

1. Keep **no service worker, no CacheStorage, no IndexedDB response store**.
   Static HTML, hashed JS/WASM/CSS, font, manifest and icons are eligible for
   ordinary browser HTTP caching with `Cache-Control: no-cache` (revalidate
   before reuse). Every API response, including protocol, diffs, blobs, graph
   and authentication, remains `no-store`. No offline response fallback.
2. Mutation endpoints refuse browser-reported offline state before transport.
   The shared write transport checks again **inside each attempt**, including
   its one immediate retry. No background sync, online-event write dispatch or
   persisted write queue. An already-sent operation may finish on the server;
   reconnect does not cancel it. Existing keyed retries while online and
   observation of already-started operations retain their existing contract.
3. Settings **Export preferences** exports only `git-vista.icons` (`nerd`/`text`),
   `git-vista.node-icons` (`on`/`off`), and `git-vista.collapse-wip` (`on`/`off`)
   in version-1 JSON. Unknown keys and malformed values stay out, including
   repository identifiers, comparison choices, operation records, drafts and
   credentials. This is intentionally not a repository-data export.
4. **Clear saved browser data** requires confirmation and removes `git-vista.*`
   and `gv-commit-draft:*` from localStorage and sessionStorage, then reloads to
   discard this tab's in-memory values. Other applications' keys survive.
   Storage errors are surfaced, including possible partial removal. It does
   not cancel a server operation, delete a server token, sign out, or evict HTTP
   cache entries. Browser settings own HTTP-cache eviction. Other tabs can save
   data again, so the control asks the user to close them first.

## Verification and limits

Export filtering and storage ownership are framework-free Rust decisions with
host tests. The transport composition test reads the wasm-only source and pins
the guard inside the attempt closure; host tests do not execute fetch or DOM
code (ADR 0115).

Playwright covers an already-open git confirmation offline, reconnection without
replay, connectivity lost before retry, the v9–v9 mismatch with a loaded client,
no persistent diff response cache, and actual Settings download/clear controls.
The harness replaces its normal forced-online override and also cuts transport.

Failure-atlas 386 admits private keys into export and fails the exact-output
assertion. Run 387 deletes the retry's guard and fails the attempt-composition
assertion. Each executed one passing baseline test and one failing mutated test
against `ca52ec15`; these are disjoint failures. Run 385 failed during temporary
clone setup and is not counted as a proof. All five browser cases passed.

After integrating main `e2bfb312`, workspace all-targets tests passed: 3,355
passed, 20 ignored. One full-gate attempt then hit a localhost connection
refusal in the existing SSH sandbox fixture; its exact isolated recheck passed
(1/1). No source change or skip was used. The full gate is required to pass
again before publishing; a passing recheck alone does not satisfy that gate.

Physical iPad installation/standalone launch and the device input/accessibility
matrix remain **unverified** on titan. Use `Refs #75`, not `Closes #75`. The
literal "cache clear and export controls" criterion is covered for app-owned
saved data only; HTTP-cache clear remains a browser action. No cached repository
browsing or cold offline launch is claimed.

Signed: codex
