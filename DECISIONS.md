# PR 570 round-two decisions

- 2026-08-27 codex — Attack and test only `87a63385432a1a7f1bb707ae9ceb3d41f232ea6e` in `/tmp/git-vista-codex-570-round-two`; keep every Cargo artifact on `/dev/sde1`.
- 2026-08-27 codex — Continue hunting after every survivor; zero further survivors remains a valid result when the attempted mutations are recorded.
- 2026-08-27 codex — Treat `remaining` strictly as byte-accounting state: neither `Some(0)` nor `None` may determine `is_end_stream`; only the inner body owns lifecycle state.
- 2026-08-27 codex — Kill survivor S7 with a test-only regression because production already delegates `is_end_stream` correctly; prove red under the mutation and green after byte-identical restore.
- 2026-08-27 codex — Make trailer accounting discriminating at a nonzero remainder; a trailer observed only at `Some(0)` cannot distinguish no change from the wrong forced-zero implementation (survivor S8).
- 2026-08-27 codex — Decouple inner EOF from exact-byte exhaustion in the delegation fixture; using a deliberately false nine-byte claim kills survivor S9's `inner.is_end_stream() && remaining == Some(0)` gate.
