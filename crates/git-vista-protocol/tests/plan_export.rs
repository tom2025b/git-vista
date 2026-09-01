//! #590's pure export contract.
//!
//! The fixture already contains one `Plan` per `GitOperation`, so these tests
//! exercise the export against the same closed vocabulary the wire contract
//! pins instead of maintaining a second catalogue of hand-built operations.

use git_vista_protocol::plan_export::{
    checklist, export_operation, render, Export, Rendered, Step,
};
use git_vista_protocol::Plan;

const PLANS: &str = include_str!("fixtures/plan_v1.json");

fn plans() -> Vec<Plan> {
    serde_json::from_str(PLANS).expect("the plan golden fixture is valid")
}

fn plan(op: &str) -> Plan {
    plans()
        .into_iter()
        .find(|plan| {
            serde_json::to_value(&plan.operation)
                .expect("an operation serializes")
                .get("op")
                .and_then(serde_json::Value::as_str)
                == Some(op)
        })
        .unwrap_or_else(|| panic!("the golden fixture has no {op} plan"))
}

/// INVARIANT: the checklist prints the shared argv as a numbered command and
/// follows it with one line explaining why it exists.
///
/// MUTATION 1 (remove): make `checklist` omit `format_step` — the command and
/// reason assertions both fail while the fixture baseline remains green.
/// MUTATION 2 (weaken): make `format_step` render only `git` plus argv[0] —
/// the exact `git add -A` assertion fails while numbering still passes.
#[test]
fn a_printable_step_is_numbered_copyable_and_explained() {
    let plan = plan("stage_all");
    let Export::Commands(steps) = export_operation(&plan.operation) else {
        panic!("stage_all has one literal command")
    };
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].argv, ["add", "-A"]);
    assert_eq!(
        steps[0].why,
        "Stage every change in the working tree, including new files."
    );

    let printed = checklist(&plan);
    assert!(printed.contains("  1. git add -A\n"), "{printed}");
    assert!(printed.contains(&steps[0].why), "{printed}");
    assert!(printed.contains("      [ ] done\n"), "{printed}");
}

/// INVARIANT: quote handling never silently emits one shell's spelling as if
/// it were portable to Tom's fish shell and to POSIX shells.
///
/// MUTATION 1 (remove): always return `Rendered::Portable(posix)` — this test
/// fails because the shell-specific arm disappears.
/// MUTATION 2 (weaken): use the POSIX spelling for `fish` too — the exact fish
/// assertion fails while the POSIX baseline remains green.
#[test]
fn a_single_quote_names_both_real_shell_spellings() {
    let step = Step {
        argv: vec!["commit".into(), "-m".into(), "Tom's plan".into()],
        why: "Record the plan.".into(),
    };
    assert_eq!(
        render(&step),
        Rendered::ShellSpecific {
            posix: "git commit -m 'Tom'\\''s plan'".into(),
            fish: "git commit -m 'Tom\\'s plan'".into(),
        }
    );
}

/// INVARIANT: a durable printout carries the plan's staleness facts and warns
/// that a terminal will not enforce them.
///
/// MUTATION 1 (remove): delete the generation line — the token assertion is
/// red while the plan fixture still parses.
/// MUTATION 2 (weaken): claim the terminal enforces expiry — the warning's
/// exact safety sentence disappears and this test is red.
#[test]
fn the_printout_carries_generation_expiry_and_preconditions() {
    let plan = plan("create_branch");
    let printed = checklist(&plan);
    assert!(
        printed.contains(&format!("Generation:  {}", plan.generation.as_str())),
        "{printed}"
    );
    assert!(
        printed.contains(&format!("App expiry:  {}", plan.expires_at.0)),
        "{printed}"
    );
    assert!(
        printed.contains("they cannot stop a command you\ntype by hand"),
        "{printed}"
    );
    assert!(printed.contains("CHECK FIRST"), "{printed}");
}

/// INVARIANT: operations whose actual argv depends on live state are labelled
/// as conditional; no candidate is promoted to an unconditional command.
///
/// MUTATION 1 (remove): classify ResetBranch as only `reset_hard_argv` — the
/// enum-shape assertion fails.
/// MUTATION 2 (weaken): drop the move-branch candidate — the cardinality and
/// `branch -f` assertions fail while the reset candidate remains green.
#[test]
fn a_runtime_choice_keeps_both_commands_conditional() {
    let plan = plan("reset_branch");
    let Export::ChosenAtRunTime { candidates, .. } = export_operation(&plan.operation) else {
        panic!("reset_branch must not guess which command the executor chooses")
    };
    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .flat_map(|candidate| &candidate.steps)
        .any(|step| step.argv.starts_with(&["reset".into(), "--hard".into()])));
    assert!(candidates
        .iter()
        .flat_map(|candidate| &candidate.steps)
        .any(|step| step.argv.starts_with(&["branch".into(), "-f".into()])));
    assert!(checklist(&plan).contains("THIS ONE DEPENDS ON THE REPOSITORY"));
}

/// Census tripwire: the export accepts every operation in the golden closed
/// vocabulary. The exhaustive production match is the compiler-level half;
/// this is the data-level half that makes the test non-vacuous.
#[test]
fn every_golden_operation_has_an_explicit_export_answer() {
    let plans = plans();
    assert_eq!(
        plans.len(),
        37,
        "the fixture census changed; inspect the new operation"
    );
    for plan in plans {
        let _ = export_operation(&plan.operation);
        assert!(!checklist(&plan).is_empty());
    }
}
