//! Session-local structured planning tools.
//!
//! A plan is agent state, not a workspace mutation: it is safe in read-only/plan mode and never
//! crosses the network except through the normal ledgered tool-result path.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::approvals::ApprovalNeed;
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec};

const MAX_PLAN_ITEMS: usize = 32;
const MAX_STEP_CHARS: usize = 1_000;
const MAX_EXPLANATION_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Completed,
}

impl Status {
    const fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PlanItem {
    step: String,
    status: Status,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    #[serde(default)]
    explanation: Option<String>,
    plan: Vec<PlanItem>,
}

#[derive(Debug, Default)]
struct PlanState {
    explanation: Option<String>,
    items: Vec<PlanItem>,
}

/// A parent agent and its isolated subagents deliberately share one tool registry. Key plan
/// state by the canonical session workspace so one lane cannot overwrite another lane's plan.
type PlanStates = BTreeMap<PathBuf, PlanState>;

#[derive(Debug, Clone)]
struct UpdatePlan {
    states: Arc<Mutex<PlanStates>>,
}

#[derive(Debug, Clone)]
struct ReadPlan {
    states: Arc<Mutex<PlanStates>>,
}

#[must_use]
pub(super) fn all() -> Vec<Arc<dyn Tool>> {
    let states = Arc::new(Mutex::new(PlanStates::new()));
    vec![
        Arc::new(UpdatePlan {
            states: Arc::clone(&states),
        }),
        Arc::new(ReadPlan { states }),
    ]
}

fn plan_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "explanation": {
                "type": "string",
                "description": "Optional short reason for this plan update."
            },
            "plan": {
                "type": "array",
                "maxItems": MAX_PLAN_ITEMS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "step": { "type": "string", "description": "Concrete task step." },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"]
                        }
                    },
                    "required": ["step", "status"]
                }
            }
        },
        "required": ["plan"]
    })
}

fn validate(args: UpdateArgs) -> Result<UpdateArgs, String> {
    if args.plan.len() > MAX_PLAN_ITEMS {
        return Err(format!("plan may contain at most {MAX_PLAN_ITEMS} steps"));
    }
    if args
        .explanation
        .as_ref()
        .is_some_and(|text| text.chars().count() > MAX_EXPLANATION_CHARS)
    {
        return Err(format!(
            "plan explanation may contain at most {MAX_EXPLANATION_CHARS} characters"
        ));
    }
    let mut active = 0usize;
    for (index, item) in args.plan.iter().enumerate() {
        let chars = item.step.chars().count();
        if item.step.trim().is_empty() {
            return Err(format!("plan step {} must not be empty", index + 1));
        }
        if chars > MAX_STEP_CHARS {
            return Err(format!(
                "plan step {} may contain at most {MAX_STEP_CHARS} characters",
                index + 1
            ));
        }
        if item.status == Status::InProgress {
            active += 1;
        }
    }
    if active > 1 {
        return Err("at most one plan step may be in_progress".to_string());
    }
    Ok(args)
}

fn render(state: &PlanState) -> String {
    if state.items.is_empty() {
        return "Plan is empty.".to_string();
    }
    let mut output = String::new();
    if let Some(explanation) = &state.explanation
        && !explanation.trim().is_empty()
    {
        output.push_str(explanation.trim());
        output.push_str("\n\n");
    }
    for (index, item) in state.items.iter().enumerate() {
        let _ = writeln!(
            output,
            "{}. {} {}",
            index + 1,
            item.status.marker(),
            item.step.trim()
        );
    }
    output.truncate(output.trim_end().len());
    output
}

#[async_trait]
impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".to_string(),
            description: "Replace the session's structured task plan. Keep steps concrete, mark exactly one in_progress while work is active, and update statuses as work completes.".to_string(),
            parameters: plan_schema(),
            mutating: false,
            parallel_safe: false,
        }
    }

    fn approval(
        &self,
        _args: &serde_json::Value,
        _ctx: &crate::tools::TurnContext,
    ) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let parsed = match serde_json::from_value::<UpdateArgs>(inv.args.clone()) {
            Ok(args) => args,
            Err(error) => return ToolOutput::failure(format!("invalid plan: {error}")),
        };
        let parsed = match validate(parsed) {
            Ok(args) => args,
            Err(error) => return ToolOutput::failure(error),
        };
        let Ok(mut states) = self.states.lock() else {
            return ToolOutput::failure("plan state lock was poisoned");
        };
        let state = states
            .entry(inv.ctx.workspace_root.clone())
            .or_insert_with(PlanState::default);
        state.explanation = parsed.explanation;
        state.items = parsed.plan;
        ToolOutput::success(render(state))
    }
}

#[async_trait]
impl Tool for ReadPlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_plan".to_string(),
            description: "Read the current session task plan.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(
        &self,
        _args: &serde_json::Value,
        _ctx: &crate::tools::TurnContext,
    ) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        match self.states.lock() {
            Ok(states) => ToolOutput::success(
                states
                    .get(&inv.ctx.workspace_root)
                    .map_or_else(|| "Plan is empty.".to_string(), render),
            ),
            Err(_) => ToolOutput::failure("plan state lock was poisoned"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_validation_rejects_multiple_active_steps() {
        let error = validate(UpdateArgs {
            explanation: None,
            plan: vec![
                PlanItem {
                    step: "one".to_string(),
                    status: Status::InProgress,
                },
                PlanItem {
                    step: "two".to_string(),
                    status: Status::InProgress,
                },
            ],
        })
        .expect_err("two active steps");
        assert!(error.contains("at most one"));
    }

    #[test]
    fn rendering_is_compact_and_stable() {
        let state = PlanState {
            explanation: Some("Closing gaps".to_string()),
            items: vec![
                PlanItem {
                    step: "Audit".to_string(),
                    status: Status::Completed,
                },
                PlanItem {
                    step: "Build".to_string(),
                    status: Status::InProgress,
                },
            ],
        };
        assert_eq!(render(&state), "Closing gaps\n\n1. [x] Audit\n2. [>] Build");
    }
}
