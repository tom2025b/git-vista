# M1.13 (#66) — design trail

Working material from the #66 design passes, kept because the design changed
shape twice under adversarial review and the *reasons* are worth more than the
conclusions alone. Tracked deliberately: this box runs a mechanical hard drive,
and a design that cost three refutation rounds should not live only on it.

| File | What it is |
|---|---|
| `verified-facts.md` | Empirically tested git behaviour (git 2.43.0) — config-scope precedence, what `env_clear` breaks, how push really authenticates here. Several natural assumptions are wrong and this proves it. |
| `m1.13-findings.md` | Round-1 refutation: 6 fatal, 13 serious, three lenses. Each with a tested scenario. |
| `m1.13-round2-findings.md` | Round-2 refutation of the revision: 6 more fatal, 3 previously-fatal still open. |
| `m1.13-decision-test-sites.md` | Tom's binding decision that all ~42 test spawn sites migrate through the real chokepoint, with the sync/async sub-question it raises. |

These are raw agent findings, not polished documents — no PDF twins, by
intent. The synthesized, readable output lives beside this folder:

- `../2026-07-27-m1.13-cgroup-containment-limits.md` — what containment
  actually guarantees on this host, including a proven escape.
- `../../specs/2026-07-27-m1.13-git-process-policy-design.md` — the design
  itself (v3, containment-based).

## Why the design changed shape

v1 and v2 tried to guarantee *"git never executes an unexpected program"*, which
requires enumerating every git config key that can name an executable. Two
rounds of adversarial testing kept finding more — `core.hooksPath`,
`core.fsmonitor`, four diff/filter families, `core.sshCommand`, `credential.helper`,
`core.askpass`, `remote.<n>.receivepack`, `gpg.program`, `include.path`. The set
is version-dependent and open-ended; enumeration cannot converge.

v3 reframes around **containment**: anything git spawns runs inside one bounded,
killable envelope, so it stops mattering which config key named the program.
Enumeration demotes to a small closed set — secret leakage and commit semantics.

That reframe is the single most useful thing in this folder, and it only became
visible because two rounds were allowed to fail honestly rather than being
patched into looking settled.

---

**Signed:** thomas2025 · 2026-07-27T22:15:00-04:00
