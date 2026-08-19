//! Model-callable, host-side repository map.
//!
//! The map is a read-only local operation. It has no network path and delegates discovery and
//! descriptor-safe bounded reads to `grokforge-context`; the active secrets denylist is copied
//! into that builder before any candidate file is opened.

use std::sync::Arc;

use async_trait::async_trait;
use grokforge_context::{RepoMapError, RepoMapLimits, RepoMapOptions, build_repo_map};
use serde_json::json;

use super::builtins::{canonical_read_path, is_blocked};
use super::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext};
use crate::approvals::ApprovalNeed;

const TOOL_MIN_OUTPUT_BYTES: usize = 1_024;
const TOOL_MAX_OUTPUT_BYTES: usize = 64 * 1_024;
const MAX_QUERY_BYTES: usize = 512;

#[derive(Debug)]
struct RepoMapTool;

pub(crate) fn tool() -> Arc<dyn Tool> {
    Arc::new(RepoMapTool)
}

#[async_trait]
impl Tool for RepoMapTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "repo_map".to_string(),
            description: "Build a deterministic, read-only lexical map of the local repository: source-oriented file inventory plus declaration names for common languages. It respects project ignore files and secrets.deny, omits hidden/binary/unsupported files, never follows symlinks, rejects hard links on Unix, and enforces file/depth/read/output caps. Optional query text changes local ranking. The scan itself makes no network request; its bounded result enters the normal redacted, ledgered model context."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Optional names or concepts to rank matching paths and symbols first."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": TOOL_MIN_OUTPUT_BYTES,
                        "maximum": TOOL_MAX_OUTPUT_BYTES,
                        "default": 49152,
                        "description": "Maximum UTF-8 bytes returned to the model. Other safety limits remain fixed."
                    }
                },
                "additionalProperties": false
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let query = match inv.args.get("query") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(query)) if query.len() <= MAX_QUERY_BYTES => {
                Some(query.clone())
            }
            Some(serde_json::Value::String(_)) => {
                return ToolOutput::failure(format!(
                    "repo_map query exceeds the {MAX_QUERY_BYTES}-byte limit"
                ));
            }
            Some(_) => return ToolOutput::failure("repo_map `query` must be a string"),
        };
        let max_output_bytes = match inv.args.get("max_bytes") {
            None | Some(serde_json::Value::Null) => RepoMapLimits::default().max_output_bytes,
            Some(value) => {
                let Some(value) = value.as_u64() else {
                    return ToolOutput::failure("repo_map `max_bytes` must be an integer");
                };
                let Ok(value) = usize::try_from(value) else {
                    return ToolOutput::failure("repo_map `max_bytes` is too large");
                };
                if !(TOOL_MIN_OUTPUT_BYTES..=TOOL_MAX_OUTPUT_BYTES).contains(&value) {
                    return ToolOutput::failure(format!(
                        "repo_map `max_bytes` must be between {TOOL_MIN_OUTPUT_BYTES} and {TOOL_MAX_OUTPUT_BYTES}"
                    ));
                }
                value
            }
        };
        if is_blocked(inv.ctx, &inv.ctx.workspace_root) {
            return ToolOutput::failure("cannot map workspace: root matches a secrets.deny rule");
        }
        let root = match canonical_read_path(inv.ctx, &inv.ctx.workspace_root) {
            Ok(root) => root,
            Err(error) => {
                return ToolOutput::failure(format!("cannot map workspace safely: {error}"));
            }
        };
        if !root.is_dir() {
            return ToolOutput::failure("cannot map workspace: root is not a directory");
        }
        let limits = RepoMapLimits {
            max_output_bytes,
            ..RepoMapLimits::default()
        };
        let options = RepoMapOptions {
            limits,
            query,
            excluded_globs: inv.ctx.policy.unreadable_globs.clone(),
        };
        let cancellation = inv.ctx.cancellation.clone();
        match tokio::task::spawn_blocking(move || {
            build_repo_map(&root, &options, || cancellation.is_cancelled())
        })
        .await
        {
            Ok(Ok(map)) => ToolOutput::success(map.text),
            Ok(Err(RepoMapError::Cancelled)) => {
                ToolOutput::failure("[turn interrupted by user; repository map cancelled]")
            }
            Ok(Err(error)) => ToolOutput::failure(format!("cannot build repository map: {error}")),
            Err(error) => {
                ToolOutput::failure(format!("repository map task failed unexpectedly: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use grokforge_protocol::{SandboxPolicy, ToolCallId};
    use grokforge_sandbox::PassthroughRunner;

    use super::*;
    use crate::TurnCancellation;

    fn context(root: &Path) -> TurnContext {
        TurnContext {
            workspace_root: std::fs::canonicalize(root).unwrap(),
            policy: SandboxPolicy::workspace_write(root),
            sandbox: Arc::new(PassthroughRunner),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: TurnCancellation::new(),
        }
    }

    #[tokio::test]
    async fn tool_maps_symbols_and_respects_secrets_policy() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            workspace.path().join("credentials.rs"),
            "fn should_not_leak() {}\n",
        )
        .unwrap();
        let mut ctx = context(workspace.path());
        ctx.policy
            .unreadable_globs
            .push("**/credentials.rs".to_string());
        ctx.policy
            .unreadable_globs
            .push("credentials.rs".to_string());
        let output = RepoMapTool
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"query": "main", "max_bytes": 4096}),
                ctx: &ctx,
            })
            .await;
        assert!(!output.is_error(), "{output:?}");
        assert!(output.content().contains("fn main"));
        assert!(!output.content().contains("credentials.rs"));
        assert!(!output.content().contains("should_not_leak"));
    }

    #[tokio::test]
    async fn tool_observes_turn_cancellation() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
        let ctx = context(workspace.path());
        ctx.cancellation.cancel();
        let output = RepoMapTool
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert!(output.content().contains("interrupted"));
    }

    #[test]
    fn registry_exposes_repo_map_as_a_read_only_builtin() {
        let registry = crate::tools::ToolRegistry::with_builtins();
        assert!(registry.get("repo_map").is_some());
        assert!(
            registry
                .readonly_tool_defs()
                .iter()
                .filter_map(grokforge_xai::ToolDef::function_name)
                .any(|name| name == "repo_map")
        );
    }

    #[tokio::test]
    async fn malformed_limits_fail_without_work() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = context(workspace.path());
        for args in [json!({"max_bytes": 1}), json!({"max_bytes": "many"})] {
            let output = RepoMapTool
                .invoke(ToolInvocation {
                    call_id: ToolCallId::new(),
                    args,
                    ctx: &ctx,
                })
                .await;
            assert!(output.is_error());
        }
    }
}
