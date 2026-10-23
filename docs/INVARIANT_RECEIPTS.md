# Invariant Receipts for Git-Vista

Git-Vista uses [Invariant Receipt](https://github.com/tom2025b/invariant-receipt)
to ask a narrow question: **does one named test detect one explicitly declared,
realistic violation of a rule we care about?**

The configuration lives at [`/invariants.yaml`](../invariants.yaml). It is an
independent evidence layer, not a replacement for ordinary tests, Clippy,
integration tests, the sandbox escape battery, or human review.

## Evidence contract

Invariant Receipt does not search the codebase for possible mutations. Every
mutation is written by a human as either:

- an exact find/replace that must match once; or
- a patch whose application is checked.

For a mutation to earn `CAUGHT`, the configured test must first pass on the
unmodified baseline and then be observed failing after that mutation. An
optional text matcher can require a stable, test-local failure signature.

The following are not successful evidence:

- the project does not compile;
- the test cannot be collected or resolved;
- no test runs;
- a different test fails;
- execution times out;
- the mutation cannot be applied exactly;
- failure output does not satisfy a configured oracle;
- any baseline repeat fails.

Those outcomes are `INVALID`, not `CAUGHT`. A mutation that builds and leaves
the named test passing is `SURVIVED` and identifies a real gap between the rule
and its test.

Invariant Receipt performs every mutation in a disposable copy and verifies
the source working tree before and after the run. JSON is the contract; human
reports are views of that evidence.

## Current invariants

| Invariant | Rule under test | Named test |
| --- | --- | --- |
| `porcelain-status-keeps-index-and-worktree-sides-distinct` | Porcelain-v2 X and Y columns must not swap staged and unstaged meaning. | `status::tests::changed_records_split_by_side` |
| `push-never-emits-an-unguarded-force` | Ordinary push must not gain a force flag, and force push must retain an explicit lease. | `planner::push::tests::no_push_argv_can_carry_a_bare_force` |
| `discard-tracked-paths-reverts-index-and-worktree-to-head` | Discarding a tracked path must restore both index and worktree to `HEAD`. | `planner::contract_suite::discard_tracked_paths_actually_reverts_a_staged_only_change` |
| `commit-graph-order-is-topological-and-tie-deterministic` | Equal-time input order must not reshuffle the graph, and nonempty geometry must report valid lanes. | `layout::tests::topology::layout_is_deterministic_whatever_order_ties_arrive_in` |
| `staging-selection-preserves-file-hunk-and-line-coordinates` | File, hunk, and line selections must remain actionable and preserve their exact coordinates. | `panes::staging::tests::file_hunk_and_changed_lines_build_the_three_shared_plan_shapes` |

Each invariant currently declares two different breaks. Two declarations alone
do not make an invariant two-way verified: both mutations must be caught, and
their observed failure signatures must be independently meaningful.

## Run on Linux

Run the tool from a clean Git-Vista checkout on the Linux VM. First establish
the ordinary baseline:

```sh
git status --short
cargo test --workspace
```

Then, with `invariant-receipt` installed and available on `PATH`:

```sh
invariant-receipt run invariants.yaml --repeat 3
```

`--repeat 3` repeats only the unmodified baseline. If any baseline run fails,
the invariant is reported as `INVALID` with `baseline_not_stable`; no mutation
result from that run is promoted to `CAUGHT`.

Afterward, verify the real checkout stayed unchanged:

```sh
git status --short
```

Do not run by manually applying the mutations to the working tree. The
disposable-copy boundary is part of the evidence contract, not an optional
convenience.

## Reading results

Invariant Receipt has four mutation verdicts:

| Verdict | Meaning |
| --- | --- |
| `CAUGHT` | The named test was observed failing under the declared mutation and satisfied its oracle. |
| `SURVIVED` | The mutation applied and the named test still passed. |
| `INVALID` | The run could not establish valid test evidence. |
| `UNKNOWN` | The run completed without enough information for one of the other verdicts. |

A single honest `CAUGHT` receipt stands on its own. The invariant—not an
individual receipt—may later earn `TWO-WAY VERIFIED` after two distinct breaks
are caught with genuinely different evidence. The project does not call that
result “proven.”

Treat results as follows:

- **`CAUGHT`:** retain the receipt and review whether the matcher is neither too
  broad nor coupled to incidental formatting.
- **`SURVIVED`:** inspect the named test. Usually the test needs one minimal
  assertion or the mutation exposed a rule the test does not actually own.
- **`mutation_not_applied`:** update the exact fingerprint only after confirming
  the production code changed intentionally.
- **`oracle_mismatch`:** compare the named test's real failure with the matcher.
  Relax only unstable formatting; do not broaden the oracle until an unrelated
  failure can satisfy it.
- **`baseline_not_stable`:** fix the baseline or its environment before reading
  any mutation result as evidence.

## Configuration conventions

Every Git-Vista entry should follow these rules:

1. Name one rule in kebab case.
2. Point to one existing test using its real Rust test path.
3. Keep each mutation small, realistic, and compiling.
4. Require the mutation fingerprint to match exactly once in the current tree.
5. Prefer two mutations that break the rule through different code paths or
   produce different test-local signatures.
6. Match stable semantic failure text when it exists. Omit a text matcher when
   the test has no stable message; do not manufacture a broad matcher merely to
   fill the field.
7. Never accept compile, collection, wrong-test, zero-test, or timeout failures
   as evidence that the test detected the violation.

## Updating an invariant

When production code or a test changes:

1. Confirm the rule itself still matters.
2. Re-read the named test and verify it still owns the rule.
3. Confirm each `find` string appears exactly once.
4. Apply each mutation only through Invariant Receipt and confirm the project
   still compiles far enough to run the named test.
5. Inspect the actual failure signature before editing `matches`.
6. Run with repeated baselines and retain the JSON receipts.
7. Review the real working tree before committing.

The configuration is intentionally small. Add an invariant when a silent
regression would be costly and an existing test can give honest, local evidence;
do not add entries to inflate a coverage count.
