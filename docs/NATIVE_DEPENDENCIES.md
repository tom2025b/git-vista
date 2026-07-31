# Native / kernel-ABI dependency register

`docs/DEPENDENCY_EXCEPTIONS.md` is a register of RustSec **advisory
suppressions**. It does not, and was never designed to, gate the *addition* of a
crate. Round 4's independent audit recorded that as finding **F10**. This file is
the missing gate, and it is deliberately narrow: it covers only crates that talk
to the kernel ABI directly, because those are the ones whose bugs are memory
safety or a silently weaker security policy rather than a wrong answer.

Enforced twice: `sandbox::deps` (a unit test, so it fails in the `core` job) and
a step in CI's `audit` job.

| Crate | Version | Why it is unavoidable | Owner | Reviewed alternative, and why not | Review date |
|---|---|---|---|---|---|
| `libc` | 0.2 | Landlock has no stable syscall wrapper in std; `landlock_create_ruleset`/`add_rule`/`restrict_self` (444/445/446) and `prctl(PR_SET_NO_NEW_PRIVS)` are raw syscalls. Already present transitively via `rustix`/`gix-index`. | Tom | The `landlock` crate: rejected, because the ABI-6 `landlock_ruleset_attr` layout and the exact `landlock_create_ruleset(NULL,0,VERSION)` floor check (C5) are the whole security-relevant surface, and a crate that negotiates "best effort" ABI downgrades is the specific failure C5 forbids. | 2026-07-28 |
| `seccompiler` | 0.5 | cBPF must be assembled with masked argument comparisons (C2). Hand-written BPF for the same filter is ~200 lines of jump arithmetic with no type checking. | Tom | `libseccomp-sys`: rejected, C-library FFI plus a build-time system dependency, for a filter of ~20 rules. Hand-rolled cBPF: rejected, C2's `Dword` masking is exactly the class of detail hand-rolled jump tables get wrong. | 2026-07-28 |

## Adding a row

1. Confirm the crate genuinely needs kernel ABI access — a crate that merely
   *depends* on `libc` does not belong here.
2. Add it to `KERNEL_API_CRATES` in `crates/git-vista-server/src/sandbox/deps.rs`.
3. Add a row here, in the same commit, with a real alternative considered.
