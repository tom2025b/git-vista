# RESUME — titan / #594

**Branch:** `feature/m10.08-a6-wire-the-graph-preview-into-the-app-t`
**Pushed head:** `ebad7676` — everything below is on the remote.

## Done

1. **Merge-down** (`36c516fc`). 41 commits of main. One conflict
   (`docs/adr/README.md`); main's rows kept verbatim in main's order, this
   branch's appended last.
2. **ADR renumbered 0100 → 0104** (same commit). Main's 0100 is #599's.
3. **Criterion 1 — cherry-pick preview** (`c68fe5de`). The arm the branch's own
   comments promised once #596 closed.
4. **`preview_subject` moved to `features::dialogs::core`** (`7b1e66a0`) — it
   was in wasm-gated `confirm.rs`, which CI compiles and lints but never
   executes. There is no wasm test runner here, so it was unprovable by
   construction. Now host-tested.
5. **Six mutations, six caught** (`92ab2dc9`, `ebad7676`). atlas 57–62, all
   conclusive against a clean tree.

804 tests pass.

## In flight right now

`./dev gate` (fmt → clippy → wasm-clippy → test → trunk build → browser),
running in the background. Nothing uncommitted.

## Single next command

```
# ON TITAN
./dev gate
```

## Not started

PR with `Closes #594`. Criteria 2–5 audit is written up in the decision log
beside this file but not yet posted.

---
**Signed:** max · 2026-09-02T12:30:00-04:00
