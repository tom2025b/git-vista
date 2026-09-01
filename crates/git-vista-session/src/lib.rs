//! git-vista-session — the client half of git-vista's session boundary
//! (M10.01, #456): how a non-browser process on this box becomes an
//! authenticated client of the running `git-vista-server`.
//!
//! Two modules, moved here **verbatim** from `git-vista-mcp` (where M2.23a,
//! #245 built and proved them) when `gv-tui` became the second standalone
//! binary to need them:
//!
//! - [`http`] — a deliberately minimal HTTP/1.1 client over `TcpStream`,
//!   loopback-only, small `Content-Length` JSON bodies. Its module doc
//!   records why it is hand-rolled rather than reqwest/hyper; that reasoning
//!   predates the extraction and survives it unchanged.
//! - [`auth`] — the bootstrap-token exchange: read the `0600` one-time token,
//!   `POST /api/session`, hold the `HttpOnly` session cookie and the CSRF
//!   token in memory only. The token is single-use and **self-replacing** —
//!   the server mints a fresh one into the same file the moment one is
//!   spent — so a client consuming a token never locks a human out of the
//!   browser flow; it only rotates the file.
//!
//! # Why a shared crate, not a dependency on `git-vista-mcp` (ADR 0101)
//!
//! The #456 issue put two options on the table: extract, or have `gv-tui`
//! depend on the MCP crate. The MCP crate is a **binary** with no library
//! target, so the dependency would first have meant carving one out — and the
//! library surface it would export is the MCP tool catalogue, the plan
//! builders and the JSON-RPC dispatch, none of which a terminal UI has any
//! business linking. The session boundary is the genuinely client-generic
//! part — the SPA's `gv` flow, the MCP bridge and the TUI authenticate
//! identically — so the session boundary is what moves, and nothing else.
//!
//! What deliberately did **not** move (yet): `git-vista-mcp`'s `authed_fetch`
//! / `authed_post` helpers (lazy first auth + one 401 retry). They serve a
//! long-lived bridge that must survive a server restart mid-session; the
//! first long-lived TUI slice that needs the same loop (#457) is the right
//! moment to lift them, with a consumer on each side to keep the seam honest.
//!
//! # What this crate must never do
//!
//! - **Never link `git-vista-server`.** `tests/no_write_dependency.rs` proves
//!   the whole transitive dependency graph never reaches it — the same
//!   compile-time mechanism as `git-vista-mcp`'s #246 proof, applied to the
//!   crate every non-browser client now links.
//! - **Never let a secret out of memory.** No token, cookie or CSRF value may
//!   reach argv, an environment variable, or any file this crate writes
//!   (#245's criterion — load-bearing because argv and env are world-visible
//!   on this box in ways a `0600` file is not). [`auth`]'s census tests hold
//!   that structurally for every source file in this crate, and both `Debug`
//!   impls redact rather than print.

pub mod auth;
pub mod http;
