//! Read-only plan exports (#590).
//!
//! These tools are deliberately local: they accept the exact `Plan` object a
//! `plan_*` tool returned and render it with `git-vista-protocol`'s pure export
//! module. They do not authenticate, contact the server, re-plan an operation,
//! or construct git arguments. The argv came from the shared builder the
//! executor uses; this module is only a door from MCP into that renderer.

use git_vista_protocol::{plan_export, Plan};

use crate::tools::ToolError;

pub(crate) fn export_tool_catalog() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "export_plan_checklist",
            "description": "Render the exact Plan returned by a plan_* tool as a numbered, \
                            printable checklist. Each literal git command is followed by one \
                            line explaining why it is present; generation, expiry, \
                            preconditions, and recovery stay visible. This is local and \
                            read-only: it executes nothing and makes no network request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "object",
                        "description": "The exact `plan` object returned by a plan_* tool."
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "export_plan_fish_script",
            "description": "Render the exact literal argv steps in a plan_* result as an \
                            explicitly fish-targeted script. Every step has one explanatory \
                            comment and exits immediately on a non-zero status. Plans whose \
                            argv is selected at run time, depends on prior command output, or \
                            requires stdin/file bytes are refused instead of guessed. This is \
                            local and read-only: it executes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "object",
                        "description": "The exact `plan` object returned by a plan_* tool."
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "export_plans_yaml_manifest",
            "description": "Encode one or more exact plan_* results as an ordered YAML \
                            manifest for gv-run. Each step stores program plus argv data, \
                            never a shell command to reparse. gv-run stops at the first \
                            non-zero exit and checkpoints every successful step for resume. \
                            Plans without one fixed literal argv sequence are refused. This \
                            export is local and read-only: it executes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plans": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "object" },
                        "description": "Ordered exact `plan` objects returned by plan_* tools."
                    }
                },
                "required": ["plans"],
                "additionalProperties": false
            }
        }),
    ]
}

pub(crate) fn call_export_tool(
    name: &str,
    args: &serde_json::Value,
) -> Option<Result<serde_json::Value, ToolError>> {
    if name == "export_plans_yaml_manifest" {
        let Some(value) = args.get("plans") else {
            return Some(Err(ToolError::Execution(
                "missing required argument `plans`".to_string(),
            )));
        };
        let plans: Vec<Plan> = match serde_json::from_value(value.clone()) {
            Ok(plans) => plans,
            Err(error) => {
                return Some(Err(ToolError::Execution(format!(
                    "`plans` is not a valid list of Plans: {error}"
                ))))
            }
        };
        return Some(
            git_vista_plan_runner::manifest_from_plans(&plans)
                .and_then(|manifest| git_vista_plan_runner::manifest_to_yaml(&manifest))
                .map(serde_json::Value::String)
                .map_err(|error| ToolError::Execution(error.to_string())),
        );
    }
    if !matches!(name, "export_plan_checklist" | "export_plan_fish_script") {
        return None;
    }
    let Some(value) = args.get("plan") else {
        return Some(Err(ToolError::Execution(
            "missing required argument `plan`".to_string(),
        )));
    };
    let plan: Plan = match serde_json::from_value(value.clone()) {
        Ok(plan) => plan,
        Err(error) => {
            return Some(Err(ToolError::Execution(format!(
                "`plan` is not a valid Plan: {error}"
            ))))
        }
    };
    Some(match name {
        "export_plan_checklist" => Ok(serde_json::Value::String(plan_export::checklist(&plan))),
        "export_plan_fish_script" => plan_export::fish_script(&plan)
            .map(serde_json::Value::String)
            .map_err(|unavailable| {
                ToolError::Execution(format!(
                    "this plan cannot be exported as a literal fish script: {}",
                    unavailable.why
                ))
            }),
        _ => unreachable!("the name gate above is exhaustive"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{
        GenerationToken, GitOperation, OperationHash, RecoveryStrategy, RepositoryToken, RiskLevel,
        UnixSeconds, WorktreeToken,
    };

    fn a_plan() -> Plan {
        Plan {
            repository: RepositoryToken::new("repo-1").unwrap(),
            worktree: WorktreeToken::new("wt-1").unwrap(),
            generation: GenerationToken::new("generation-7").unwrap(),
            operation: GitOperation::StageAll,
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            issued_at: UnixSeconds(1_788_280_000),
            expires_at: UnixSeconds(1_788_280_300),
            risk: RiskLevel::Safe,
            preconditions: Vec::new(),
            expected_ref_changes: Vec::new(),
            advisories: Vec::new(),
            recovery: RecoveryStrategy::NotNeeded,
        }
    }

    /// INVARIANT: the MCP export is the protocol renderer's answer verbatim.
    ///
    /// MUTATION 1 (remove): return an empty string — the exact equality fails.
    /// MUTATION 2 (weaken): re-render only `operation_name` — equality fails
    /// while the known-tool and valid-plan baselines remain green.
    #[test]
    fn checklist_tool_returns_the_shared_renderer_verbatim() {
        let plan = a_plan();
        let args = serde_json::json!({ "plan": plan });
        let rendered = call_export_tool("export_plan_checklist", &args)
            .expect("the tool is known")
            .expect("a valid plan renders");
        assert_eq!(
            rendered,
            serde_json::Value::String(plan_export::checklist(&a_plan()))
        );
    }

    /// INVARIANT: MCP returns the protocol crate's fish script verbatim.
    ///
    /// MUTATION 1 (remove): return an empty string — exact equality is red.
    /// MUTATION 2 (weaken): return the checklist under the script tool's name
    /// — equality is red even though both renderers contain the same argv.
    #[test]
    fn fish_script_tool_returns_the_shared_renderer_verbatim() {
        let plan = a_plan();
        let args = serde_json::json!({ "plan": plan });
        let rendered = call_export_tool("export_plan_fish_script", &args)
            .expect("the tool is known")
            .expect("a valid scriptable plan renders");
        assert_eq!(
            rendered,
            serde_json::Value::String(plan_export::fish_script(&a_plan()).unwrap())
        );
    }

    /// INVARIANT: the many-plan MCP export delegates the whole ordered plan
    /// list to the native manifest encoder.
    ///
    /// MUTATION 1 (remove): return an empty string — exact equality is red.
    /// MUTATION 2 (weaken): pass only the first plan — the second plan's argv
    /// disappears and exact equality is red.
    #[test]
    fn yaml_manifest_tool_returns_the_shared_many_plan_encoder_verbatim() {
        let first = a_plan();
        let mut second = a_plan();
        second.operation = GitOperation::UnstageAll;
        second.operation_hash = OperationHash::new("b".repeat(64)).unwrap();
        let plans = vec![first, second];
        let args = serde_json::json!({ "plans": plans });
        let rendered = call_export_tool("export_plans_yaml_manifest", &args)
            .expect("the tool is known")
            .expect("valid fixed-argv plans render");
        let expected = git_vista_plan_runner::manifest_to_yaml(
            &git_vista_plan_runner::manifest_from_plans(&plans).unwrap(),
        )
        .unwrap();
        assert_eq!(rendered, serde_json::Value::String(expected));
    }

    #[test]
    fn checklist_tool_refuses_a_malformed_plan_and_ignores_other_names() {
        assert!(call_export_tool("not-this-tool", &serde_json::json!({})).is_none());
        let error = call_export_tool(
            "export_plan_checklist",
            &serde_json::json!({ "plan": { "operation": { "op": "stage_all" } } }),
        )
        .expect("the tool is known")
        .expect_err("a partial plan is invalid");
        assert!(
            matches!(error, ToolError::Execution(message) if message.contains("not a valid Plan"))
        );
    }
}
