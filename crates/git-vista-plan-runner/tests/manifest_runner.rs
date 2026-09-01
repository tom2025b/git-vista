use git_vista_plan_runner::{
    checkpoint_from_yaml, checkpoint_to_yaml, manifest_from_plans, manifest_from_yaml,
    manifest_sha256, manifest_to_yaml, run_remaining, Checkpoint, Manifest, RunFailure,
};
use git_vista_protocol::plan_export::{export_operation, Export};
use git_vista_protocol::Plan;

const PLANS: &str = include_str!("../../git-vista-protocol/tests/fixtures/plan_v1.json");

fn plans() -> Vec<Plan> {
    serde_json::from_str(PLANS).unwrap()
}

fn plan(op: &str) -> Plan {
    plans()
        .into_iter()
        .find(|plan| serde_json::to_value(&plan.operation).unwrap()["op"].as_str() == Some(op))
        .unwrap()
}

fn a_manifest() -> Manifest {
    manifest_from_plans(&[plan("stage_all"), plan("pull_branch")]).unwrap()
}

/// INVARIANT: a many-plan manifest contains the exact shared argv arrays in
/// plan order, with one globally monotonic step number.
///
/// MUTATION 1 (remove): omit the first plan while flattening — argv equality
/// and the plan count are red.
/// MUTATION 2 (weaken): sort steps by their why text — exact order is red even
/// though every command is still present.
#[test]
fn manifest_flattens_many_plans_without_rebuilding_their_argv() {
    let source = [plan("stage_all"), plan("pull_branch")];
    let manifest = manifest_from_plans(&source).unwrap();
    assert_eq!(manifest.plans.len(), 2);

    let expected: Vec<Vec<String>> = source
        .iter()
        .flat_map(|plan| match export_operation(&plan.operation) {
            Export::Commands(steps) => steps.into_iter().map(|step| step.argv).collect::<Vec<_>>(),
            other => panic!("fixture unexpectedly unavailable: {other:?}"),
        })
        .collect();
    assert_eq!(
        manifest
            .steps
            .iter()
            .map(|step| step.argv.clone())
            .collect::<Vec<_>>(),
        expected
    );
    for (index, step) in manifest.steps.iter().enumerate() {
        assert_eq!(step.number as usize, index + 1);
        assert_eq!(step.program, "git");
        assert!(!step.why.is_empty());
    }
}

/// INVARIANT: a manifest refuses plans whose executor does not have one fixed
/// argv sequence available before execution.
///
/// MUTATION 1 (remove): choose the first runtime candidate for reset_branch —
/// that dangerous guess makes the reset negative leg red.
/// MUTATION 2 (weaken): invent a bare `git apply` for stage_selection — the
/// stdin-dependent negative leg is red.
#[test]
fn manifest_refuses_runtime_selected_and_non_argv_operations() {
    for operation in ["reset_branch", "stage_selection"] {
        assert!(
            manifest_from_plans(&[plan(operation)]).is_err(),
            "{operation} must not acquire guessed argv"
        );
    }
}

/// INVARIANT: the generated YAML round-trips through a closed, validated
/// schema; a visually plausible extra field is never ignored.
///
/// MUTATION 1 (remove): drop `deny_unknown_fields` from Manifest — the extra
/// top-level key parses and the negative leg is red.
/// MUTATION 2 (weaken): skip sequential-number validation — the changed step
/// number parses and the negative leg is red.
#[test]
fn yaml_round_trips_and_refuses_unknown_or_disordered_data() {
    let manifest = a_manifest();
    let yaml = manifest_to_yaml(&manifest).unwrap();
    assert_eq!(manifest_from_yaml(&yaml).unwrap(), manifest);

    let with_extra = format!("{yaml}surprise: ignored\n");
    assert!(manifest_from_yaml(&with_extra).is_err());

    let steps_start = yaml
        .find("steps:\n")
        .expect("encoder writes a steps section");
    let (header, steps) = yaml.split_at(steps_start);
    let disordered = format!("{header}{}", steps.replacen("number: 2", "number: 9", 1));
    assert!(manifest_from_yaml(&disordered).is_err());
}

/// INVARIANT: execution stops on the first non-zero result and checkpoints
/// only the successful prefix.
///
/// MUTATION 1 (remove): continue after a non-zero result — step 3 appears in
/// `seen` and the exact call list is red.
/// MUTATION 2 (weaken): mark a non-zero step completed before returning —
/// last_completed_step is 2 and the state assertion is red.
#[test]
fn runner_stops_on_first_error_and_records_only_completed_steps() {
    let manifest = a_manifest();
    assert!(manifest.steps.len() >= 3);
    let mut checkpoint = Checkpoint::new("a".repeat(64));
    let mut seen = Vec::new();
    let mut saved = Vec::new();

    let failure = run_remaining(
        &manifest,
        &mut checkpoint,
        |step| {
            seen.push(step.number);
            Ok(if step.number == 2 { 17 } else { 0 })
        },
        |state| {
            saved.push(state.last_completed_step);
            Ok(())
        },
    )
    .unwrap_err();

    assert!(matches!(
        failure,
        RunFailure::StepFailed { step: 2, code: 17 }
    ));
    assert_eq!(seen, [1, 2]);
    assert_eq!(saved, [1]);
    assert_eq!(checkpoint.last_completed_step, 1);
}

/// INVARIANT: resume skips the durable prefix and persists each newly
/// completed step before moving on.
///
/// MUTATION 1 (remove): always start at step 1 — `seen` contains 1.
/// MUTATION 2 (weaken): save only at the end — `saved` lacks the intermediate
/// checkpoint for step 2.
#[test]
fn runner_resumes_after_the_last_completed_step() {
    let manifest = a_manifest();
    let mut checkpoint = Checkpoint::new("b".repeat(64));
    checkpoint.last_completed_step = 1;
    let mut seen = Vec::new();
    let mut saved = Vec::new();

    let summary = run_remaining(
        &manifest,
        &mut checkpoint,
        |step| {
            seen.push(step.number);
            Ok(0)
        },
        |state| {
            saved.push(state.last_completed_step);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(seen, [2, 3]);
    assert_eq!(saved, [2, 3]);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.completed, 2);
    assert_eq!(checkpoint.last_completed_step, 3);
}

/// INVARIANT: a checkpoint is bound both to the exact manifest bytes and to a
/// completed prefix that exists in that manifest.
///
/// MUTATION 1 (remove): skip manifest-digest equality — the different-digest
/// negative leg is red.
/// MUTATION 2 (weaken): accept a completed step beyond the manifest length —
/// the out-of-range negative leg is red.
#[test]
fn checkpoint_round_trip_is_bound_to_the_exact_manifest_bytes() {
    let yaml = manifest_to_yaml(&a_manifest()).unwrap();
    let digest = manifest_sha256(yaml.as_bytes());
    assert_eq!(digest.len(), 64);
    let mut checkpoint = Checkpoint::new(digest.clone());
    checkpoint.last_completed_step = 2;
    let encoded = checkpoint_to_yaml(&checkpoint).unwrap();
    assert_eq!(
        checkpoint_from_yaml(&encoded, &digest, 3).unwrap(),
        checkpoint
    );
    assert!(checkpoint_from_yaml(&encoded, &"f".repeat(64), 3).is_err());
    assert!(checkpoint_from_yaml(&encoded, &digest, 1).is_err());
}
