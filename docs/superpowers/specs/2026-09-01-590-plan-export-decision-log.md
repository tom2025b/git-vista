# #590 plan export decision log

- 2026-09-01 — Continue the rescue branch's shared `Vec<String>` argv builders because executor and export must consume one value-producing mechanism.
- 2026-09-01 — Keep runtime-selected, stdin-fed, file-write, and prior-output-dependent operations explicit instead of printing a guessed command that the app might not run.
- 2026-09-01 — Ship each requested slice as a separate commit so checklist behavior is reviewable and revertible before script and runner scope is added.
- 2026-09-01 — Put renderer behavior in the pure, wasm-safe protocol crate so browser, MCP, and native runner can consume the same export without another command reconstruction.
- 2026-09-01 — Expose slice 1 as a local MCP tool consuming the exact `plan_*` result because that is the existing user-reachable review workflow and needs no second server plan endpoint.
- 2026-09-01 — Target generated scripts explicitly at fish and use `or exit $status` after every command because silently placing POSIX `set -e` under Tom's login shell would be a false dialect promise.
- 2026-09-01 — Refuse scripts for runtime-selected, prior-output-dependent, stdin-fed, and file-write operations until their exact execution data has one shared representation.
- 2026-09-01 — Store manifest steps as `program: git` plus argv arrays because the runner must execute data directly, never parse the human shell rendering.
- 2026-09-01 — Isolate YAML and runner code in a native crate so `git-vista-protocol` remains wasm-safe and does not acquire libyaml.
- 2026-09-01 — Use `serde_yaml_ng` 0.10 instead of archived `serde_yaml` or advisory-affected `serde_yml`; keep the schema closed and validate semantic ordering after parse.
- 2026-09-01 — Bind checkpoints to SHA-256 of the exact manifest bytes and write after every successful step so a changed manifest cannot inherit progress and resume skips only a durable prefix.
- 2026-09-01 — Document resume as at-least-once for an interrupted in-flight command because marking completion before a zero exit would skip work that may never have happened.
