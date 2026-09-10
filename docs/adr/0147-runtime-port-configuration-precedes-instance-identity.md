# ADR 0147 — Runtime port configuration precedes instance identity

- **Status:** Accepted — the server listener contract is implemented by PR #828; the production launcher remains deliberately default-instance-only.
- **Date:** 2026-09-10
- **Issue:** Refs #130
- **Amends:** [ADR 0046](0046-mcp-plan-tool-surface.md) and [ADR 0142](0142-a-ci-job-may-weaken-the-runner-only-for-a-named-reason-it-still-needs.md), whose compile-time-port premises are no longer true.

## Context

The server used one compile-time port, 8080. That prevented an isolated test server from choosing another loopback socket without rewriting Rust source and rebuilding it. Making the listener port configurable does not, by itself, make a second production instance safe.

`gv` still owns one log, PID, token, mode, target, and LAN record; the server still owns one bootstrap-token path and one host-scoped cookie name; and the native session client still targets `127.0.0.1:8080`. Exposing `GIT_VISTA_PORT=8081 gv` at this stage would imply an instance contract those components do not honour. In particular, the launcher's binary-path process scan could signal a server from the same checkout on another port.

## Decision

The server accepts `GIT_VISTA_PORT` as a nonzero `u16`, defaulting to 8080. The effective port is passed to both listener profiles and both `HostPolicy` values. Port configuration does not weaken the host boundary: the control listener remains exactly IPv4 loopback, and an explicit `GIT_VISTA_BIND_ADDR` must equal `127.0.0.1:<effective-port>`.

The production `gv` launcher does not advertise or accept a conflicting port override yet. It pins and exports its assigned `PORT`; `dev testbed` may rewrite that assignment because it also creates a separate worktree, target directory, and `XDG_STATE_HOME`. For historical refs whose server does not read `GIT_VISTA_PORT`, testbed retains the compile-time `state.rs` rewrite as a fallback.

Issue #130 remains open for the actual instance contract: an explicit launcher option, port-scoped lifecycle and runtime/auth/operations state, an instance-scoped cookie, and native-client endpoint/token selection.

## Alternatives considered

| Alternative | Why it lost |
|---|---|
| Advertise the server environment variable through `gv` immediately | It would stop or overwrite another same-checkout instance because the launcher, token, and runtime files are not instance-scoped. |
| Scope only the launcher's process scan by port | It prevents one signal but leaves shared token/state files, cookies, and clients addressing the wrong instance; that is still not the contract the command would imply. |
| Keep the port compile-time-only | It preserves a rebuild-heavy testbed mechanism and prevents cheap isolated server tests even though validating a runtime `u16` does not weaken the loopback-only boundary. |

## Consequences

Server tests and isolated harnesses can choose a free nondefault port without opening a non-loopback control socket. Default production behavior remains 8080, and `gv` fails before building or stopping anything if an ambient `GIT_VISTA_PORT` conflicts with its pinned port.

The browser suite keeps its network and mount namespaces: they isolate a stable test origin and candidate frontend bundle, guarantees that choosing another port alone does not replace. The MCP live handshake tests also remain ignored until the native client and token lookup can target the same private instance.
