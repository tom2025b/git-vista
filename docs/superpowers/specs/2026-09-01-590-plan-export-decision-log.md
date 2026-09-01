# #590 plan export decision log

- 2026-09-01 — Continue the rescue branch's shared `Vec<String>` argv builders because executor and export must consume one value-producing mechanism.
- 2026-09-01 — Keep runtime-selected, stdin-fed, file-write, and prior-output-dependent operations explicit instead of printing a guessed command that the app might not run.
- 2026-09-01 — Ship each requested slice as a separate commit so checklist behavior is reviewable and revertible before script and runner scope is added.
- 2026-09-01 — Put renderer behavior in the pure, wasm-safe protocol crate so browser, MCP, and native runner can consume the same export without another command reconstruction.
- 2026-09-01 — Expose slice 1 as a local MCP tool consuming the exact `plan_*` result because that is the existing user-reachable review workflow and needs no second server plan endpoint.
