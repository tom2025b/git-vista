# PR 570 round-two decisions

- 2026-08-27 codex — Attack and test only `87a63385432a1a7f1bb707ae9ceb3d41f232ea6e` in `/tmp/git-vista-codex-570-round-two`; keep every Cargo artifact on `/dev/sde1`.
- 2026-08-27 codex — Continue hunting after every survivor; zero further survivors remains a valid result when the attempted mutations are recorded.
- 2026-08-27 codex — Treat `remaining` strictly as byte-accounting state: neither `Some(0)` nor `None` may determine `is_end_stream`; only the inner body owns lifecycle state.
- 2026-08-27 codex — Kill survivor S7 with a test-only regression because production already delegates `is_end_stream` correctly; prove red under the mutation and green after byte-identical restore.
- 2026-08-27 codex — Make trailer accounting discriminating at a nonzero remainder; a trailer observed only at `Some(0)` cannot distinguish no change from the wrong forced-zero implementation (survivor S8).
- 2026-08-27 codex — Decouple inner EOF from exact-byte exhaustion in the delegation fixture; using a deliberately false nine-byte claim kills survivor S9's `inner.is_end_stream() && remaining == Some(0)` gate.
- 2026-08-27 codex — Make the overrun fixture cross the boundary inside its first frame and assert every later frame/hint; landing on zero before overrunning let survivor S10 delay invalidation by one frame.
- 2026-08-27 codex — Test Pending liveness with a waker-capturing body, not `ScriptedBody`; the manual no-op fixture could not detect survivor S11 substituting its own inert context.
- 2026-08-27 codex — Treat empty DATA as a first-class forwarded frame that consumes zero bytes; survivor S12 laundered it into EOF and hid later data because no `KnownSizeBody` fixture emitted one.
- 2026-08-27 codex — Assert unknown rejoin hints before polling; survivor S13 defaulted absent `original_exact` to zero, a false framing claim that later frame reads happened to invalidate after the damage point.
- 2026-08-27 codex — Exercise `Pending` after exact DATA with trailers still queued; survivor S14 converted that readiness state to EOF because the earlier Pending fixture ran only at a positive remainder.
- 2026-08-27 codex — Exercise an error after exact DATA and assert its original text plus unchanged hint; survivor S15 laundered errors only at `Some(0)`/`None`, outside the earlier underrun fixture.
- 2026-08-27 codex — Exercise trailers after a direct overrun and pin `x-checksum`; survivor S16 dropped trailers only after `remaining` became unknown, a state no trailer fixture reached.
- 2026-08-27 codex — Cover `rejoin`'s empty-head/rest-present arm with exact-zero DATA plus trailers; survivor S17 returned the unknown stream early and skipped restoration of `original_exact`.
- 2026-08-27 codex — Pin data shape, error identity, and nonzero remaining count around the underrun error fixture; survivor S18 forwarded `Err` but silently rewrote the byte hint to zero.
