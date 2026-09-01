//! YAML manifests and resumable step execution for #590.
//!
//! A manifest stores the same argument arrays the application executors use.
//! The runner passes those arrays directly to its process boundary; it never
//! reparses a rendered shell command.

use std::fmt;

use git_vista_protocol::plan_export::{export_operation, operation_name, Export};
use git_vista_protocol::Plan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST_VERSION: u32 = 1;
const CHECKPOINT_VERSION: u32 = 1;

/// A closed, ordered collection of plans and their executable steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Schema version. Only version 1 is accepted.
    pub version: u32,
    /// Source plans, in the order supplied by the user.
    pub plans: Vec<ManifestPlan>,
    /// Globally numbered commands, in execution order.
    pub steps: Vec<ManifestStep>,
}

/// Identity carried from one source plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPlan {
    /// One-based source-plan number.
    pub number: u32,
    /// Plain operation name for review output.
    pub operation: String,
    /// Hash that bound the original approval to the operation.
    pub operation_hash: String,
    /// Repository generation against which the source plan was built.
    pub generation: String,
}

/// One exact process invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestStep {
    /// One-based, globally monotonic execution number.
    pub number: u32,
    /// One-based source-plan number.
    pub plan: u32,
    /// One line explaining why this command runs.
    pub why: String,
    /// Executable. Version 1 deliberately permits only `git`.
    pub program: String,
    /// Exact executor arguments, without a leading `git`.
    pub argv: Vec<String>,
}

/// Manifest construction, parsing, or validation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError(String);

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ManifestError {}

/// Build a manifest by flattening exact shared argv arrays in plan order.
pub fn manifest_from_plans(plans: &[Plan]) -> Result<Manifest, ManifestError> {
    if plans.is_empty() {
        return Err(ManifestError("a manifest needs at least one plan".into()));
    }

    let mut manifest_plans = Vec::with_capacity(plans.len());
    let mut steps = Vec::new();
    for (plan_index, plan) in plans.iter().enumerate() {
        let plan_number = checked_number(plan_index + 1, "plan")?;
        manifest_plans.push(ManifestPlan {
            number: plan_number,
            operation: operation_name(&plan.operation).to_string(),
            operation_hash: plan.operation_hash.as_str().to_string(),
            generation: plan.generation.as_str().to_string(),
        });

        let exported_steps = match export_operation(&plan.operation) {
            Export::Commands(exported_steps) => exported_steps,
            other => {
                return Err(ManifestError(format!(
                    "plan {plan_number} ({}) has no fixed argv sequence: {}",
                    operation_name(&plan.operation),
                    unavailable_reason(&other)
                )))
            }
        };
        for step in exported_steps {
            steps.push(ManifestStep {
                number: checked_number(steps.len() + 1, "step")?,
                plan: plan_number,
                why: one_line(&step.why),
                program: "git".to_string(),
                argv: step.argv,
            });
        }
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        plans: manifest_plans,
        steps,
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Serialize a validated manifest as YAML.
pub fn manifest_to_yaml(manifest: &Manifest) -> Result<String, ManifestError> {
    validate_manifest(manifest)?;
    serde_yaml_ng::to_string(manifest)
        .map_err(|error| ManifestError(format!("cannot encode manifest YAML: {error}")))
}

/// Parse YAML through a closed schema and then enforce semantic ordering.
pub fn manifest_from_yaml(yaml: &str) -> Result<Manifest, ManifestError> {
    let manifest: Manifest = serde_yaml_ng::from_str(yaml)
        .map_err(|error| ManifestError(format!("invalid manifest YAML: {error}")))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest) -> Result<(), ManifestError> {
    if manifest.version != MANIFEST_VERSION {
        return Err(ManifestError(format!(
            "unsupported manifest version {}; expected {MANIFEST_VERSION}",
            manifest.version
        )));
    }
    if manifest.plans.is_empty() {
        return Err(ManifestError("a manifest needs at least one plan".into()));
    }
    if manifest.steps.is_empty() {
        return Err(ManifestError("a manifest needs at least one step".into()));
    }

    for (index, plan) in manifest.plans.iter().enumerate() {
        let expected = checked_number(index + 1, "plan")?;
        if plan.number != expected {
            return Err(ManifestError(format!(
                "plan number {} is out of order; expected {expected}",
                plan.number
            )));
        }
        if plan.operation.trim().is_empty() || plan.generation.trim().is_empty() {
            return Err(ManifestError(format!(
                "plan {} has an empty operation or generation",
                plan.number
            )));
        }
        if !is_sha256(&plan.operation_hash) {
            return Err(ManifestError(format!(
                "plan {} has an invalid operation hash",
                plan.number
            )));
        }
    }

    for (index, step) in manifest.steps.iter().enumerate() {
        let expected = checked_number(index + 1, "step")?;
        if step.number != expected {
            return Err(ManifestError(format!(
                "step number {} is out of order; expected {expected}",
                step.number
            )));
        }
        if step.plan == 0 || step.plan as usize > manifest.plans.len() {
            return Err(ManifestError(format!(
                "step {} refers to missing plan {}",
                step.number, step.plan
            )));
        }
        if step.program != "git" {
            return Err(ManifestError(format!(
                "step {} has unsupported program {:?}; expected git",
                step.number, step.program
            )));
        }
        if step.argv.is_empty() || step.why.trim().is_empty() {
            return Err(ManifestError(format!(
                "step {} needs argv and a reason",
                step.number
            )));
        }
    }
    Ok(())
}

fn checked_number(number: usize, kind: &str) -> Result<u32, ManifestError> {
    u32::try_from(number)
        .map_err(|_| ManifestError(format!("too many {kind}s for the manifest format")))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unavailable_reason(export: &Export) -> &str {
    match export {
        Export::Commands(_) => "available",
        Export::ChosenAtRunTime { decided_by, .. } => decided_by,
        Export::Chained { why } | Export::NotACommandLine { why } => why,
    }
}

/// Durable progress for one exact manifest byte sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    /// Checkpoint schema version.
    pub version: u32,
    /// SHA-256 of the exact manifest file bytes.
    pub manifest_sha256: String,
    /// Last successfully executed and durably recorded step.
    pub last_completed_step: u32,
}

impl Checkpoint {
    /// Start with no completed steps for `manifest_sha256`.
    pub fn new(manifest_sha256: String) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            manifest_sha256,
            last_completed_step: 0,
        }
    }
}

/// SHA-256 of the exact bytes the runner was asked to execute.
pub fn manifest_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

/// Serialize a validated checkpoint as YAML.
pub fn checkpoint_to_yaml(checkpoint: &Checkpoint) -> Result<String, ManifestError> {
    validate_checkpoint(checkpoint, &checkpoint.manifest_sha256, u32::MAX)?;
    serde_yaml_ng::to_string(checkpoint)
        .map_err(|error| ManifestError(format!("cannot encode checkpoint YAML: {error}")))
}

/// Parse a checkpoint and bind it to the exact manifest and step count.
pub fn checkpoint_from_yaml(
    yaml: &str,
    expected_manifest_sha256: &str,
    step_count: u32,
) -> Result<Checkpoint, ManifestError> {
    let checkpoint: Checkpoint = serde_yaml_ng::from_str(yaml)
        .map_err(|error| ManifestError(format!("invalid checkpoint YAML: {error}")))?;
    validate_checkpoint(&checkpoint, expected_manifest_sha256, step_count)?;
    Ok(checkpoint)
}

fn validate_checkpoint(
    checkpoint: &Checkpoint,
    expected_manifest_sha256: &str,
    step_count: u32,
) -> Result<(), ManifestError> {
    if checkpoint.version != CHECKPOINT_VERSION {
        return Err(ManifestError(format!(
            "unsupported checkpoint version {}; expected {CHECKPOINT_VERSION}",
            checkpoint.version
        )));
    }
    if !is_sha256(&checkpoint.manifest_sha256) {
        return Err(ManifestError("checkpoint manifest hash is invalid".into()));
    }
    if checkpoint.manifest_sha256 != expected_manifest_sha256 {
        return Err(ManifestError(
            "checkpoint belongs to different manifest bytes".into(),
        ));
    }
    if checkpoint.last_completed_step > step_count {
        return Err(ManifestError(format!(
            "checkpoint completed step {} but manifest has only {step_count} steps",
            checkpoint.last_completed_step
        )));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Counts from one execution or resume attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    /// Durable steps skipped on resume.
    pub skipped: u32,
    /// Steps completed in this attempt.
    pub completed: u32,
}

/// The precise boundary at which execution stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    /// Starting state did not describe a valid durable prefix.
    InvalidCheckpoint(String),
    /// The process could not be started.
    StepSpawn { step: u32, message: String },
    /// The process ran and returned non-zero.
    StepFailed { step: u32, code: i32 },
    /// A successful command could not be durably recorded.
    Checkpoint { step: u32, message: String },
}

impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCheckpoint(message) => write!(f, "invalid checkpoint: {message}"),
            Self::StepSpawn { step, message } => {
                write!(f, "step {step} could not start: {message}")
            }
            Self::StepFailed { step, code } => {
                write!(f, "step {step} stopped with exit code {code}")
            }
            Self::Checkpoint { step, message } => {
                write!(
                    f,
                    "step {step} succeeded but its checkpoint failed: {message}"
                )
            }
        }
    }
}

impl std::error::Error for RunFailure {}

/// Execute only the steps after the durable prefix, stopping at first error.
///
/// `execute` receives the exact manifest step. `persist` is called after each
/// zero exit and before the next command is attempted.
pub fn run_remaining<E, P>(
    manifest: &Manifest,
    checkpoint: &mut Checkpoint,
    mut execute: E,
    mut persist: P,
) -> Result<RunSummary, RunFailure>
where
    E: FnMut(&ManifestStep) -> Result<i32, String>,
    P: FnMut(&Checkpoint) -> Result<(), String>,
{
    validate_manifest(manifest)
        .map_err(|error| RunFailure::InvalidCheckpoint(error.to_string()))?;
    let step_count = checked_number(manifest.steps.len(), "step")
        .map_err(|error| RunFailure::InvalidCheckpoint(error.to_string()))?;
    validate_checkpoint(checkpoint, &checkpoint.manifest_sha256, step_count)
        .map_err(|error| RunFailure::InvalidCheckpoint(error.to_string()))?;

    let skipped = checkpoint.last_completed_step;
    let mut completed = 0;
    for step in manifest
        .steps
        .iter()
        .skip(checkpoint.last_completed_step as usize)
    {
        let code = execute(step).map_err(|message| RunFailure::StepSpawn {
            step: step.number,
            message,
        })?;
        if code != 0 {
            return Err(RunFailure::StepFailed {
                step: step.number,
                code,
            });
        }

        checkpoint.last_completed_step = step.number;
        persist(checkpoint).map_err(|message| RunFailure::Checkpoint {
            step: step.number,
            message,
        })?;
        completed += 1;
    }

    Ok(RunSummary { skipped, completed })
}
