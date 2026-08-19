//! Conflict-detecting, single-file unified-diff application.
//!
//! Multi-file patches are intentionally split into separate tool calls. That keeps approval,
//! physical-path binding, stale-content checks, touched-path accounting, and failure semantics
//! atomic for every requested file.

use std::sync::Arc;

use async_trait::async_trait;
use grokforge_protocol::{ApprovalKind, DenialClass};
use serde_json::json;

use super::builtins::{canonical_read_path, is_blocked};
use super::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext, arg_str};
use crate::approvals::ApprovalNeed;
use crate::path_safety::{self, PathSafetyError};

const MAX_PATCH_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct ApplyPatch;

#[must_use]
pub(super) fn tool() -> Arc<dyn Tool> {
    Arc::new(ApplyPatch)
}

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_string(),
            description: "Apply one standard unified diff to one existing workspace text file. Include matching ---/+++ file headers and @@ hunks. The update uses descriptor-safe path binding and rejects stale content observed before replacement; use separate calls for separate files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Existing text file path relative to the workspace root."
                    },
                    "patch": {
                        "type": "string",
                        "description": "Unified diff for exactly this file, including ---/+++ headers."
                    }
                },
                "required": ["path", "patch"]
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let path = ctx.resolve(path);
        let kind = ApprovalKind::ApplyPatch {
            files: vec![path.clone()],
        };
        if ctx.policy.allows_write(&path) {
            ApprovalNeed::Gated(kind)
        } else {
            ApprovalNeed::OutsideSandbox(kind)
        }
    }

    #[allow(clippy::too_many_lines)] // Linear validation keeps the read/parse/compare/write audit trail explicit.
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let target_input = match arg_str(&inv.args, "path") {
            Ok(target_input) => target_input,
            Err(error) => return error,
        };
        let diff = match arg_str(&inv.args, "patch") {
            Ok(diff) => diff,
            Err(error) => return error,
        };
        if diff.is_empty() || diff.len() > MAX_PATCH_BYTES {
            return ToolOutput::failure(format!("patch must contain 1-{MAX_PATCH_BYTES} bytes"));
        }
        if diff.contains('\0')
            || diff
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return ToolOutput::failure("patch contains unsupported control characters");
        }
        if inv.ctx.cancellation.is_cancelled() {
            return ToolOutput::failure("[turn interrupted before patch application]");
        }

        let resolved = inv.ctx.resolve(target_input);
        if is_blocked(inv.ctx, &resolved) {
            return ToolOutput::failure(format!(
                "cannot patch `{target_input}`: path matches a secrets.deny rule"
            ));
        }
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(error) => {
                return ToolOutput::failure(format!("cannot patch `{target_input}`: {error}"));
            }
        };
        if is_blocked(inv.ctx, &canonical) {
            return ToolOutput::failure(format!(
                "cannot patch `{target_input}`: resolved path is secret"
            ));
        }
        let relative = match canonical.strip_prefix(&inv.ctx.workspace_root) {
            Ok(path) if !path.as_os_str().is_empty() => path.to_string_lossy().replace('\\', "/"),
            _ => {
                return ToolOutput::failure(
                    "patch target must be an existing file inside the workspace",
                );
            }
        };
        let workspace = inv.ctx.workspace_root.clone();
        let source_path = canonical.clone();
        let source = match tokio::task::spawn_blocking(move || {
            path_safety::read_workspace_text(
                &workspace,
                &source_path,
                path_safety::MAX_MUTATING_FILE_BYTES,
            )
        })
        .await
        {
            Ok(Ok((source, false))) => source,
            Ok(Ok((_, true))) => {
                return ToolOutput::failure("patch target exceeds the 8 MiB mutation limit");
            }
            Ok(Err(error)) => {
                return ToolOutput::failure(format!(
                    "cannot read `{target_input}` safely: {error}"
                ));
            }
            Err(error) => {
                return ToolOutput::failure(format!(
                    "cannot read `{target_input}` for patching: task failed: {error}"
                ));
            }
        };
        let updated = match apply_unified_patch(&source, diff, &relative) {
            Ok(updated) => updated,
            Err(error) => return ToolOutput::failure(format!("cannot apply patch: {error}")),
        };
        if updated == source {
            return ToolOutput::failure("patch produced no change");
        }
        let policy = inv.ctx.policy.clone();
        let target = canonical.clone();
        let approved_target = inv.ctx.bound_write_target(&canonical);
        let expected = source.into_bytes();
        let replacement = updated.into_bytes();
        match tokio::task::spawn_blocking(move || {
            path_safety::replace_file_if_unchanged_bound(
                &policy,
                &target,
                approved_target.as_deref(),
                &expected,
                &replacement,
            )
        })
        .await
        {
            Ok(Ok(())) => {
                inv.ctx.record_touched(canonical);
                ToolOutput::success(format!("applied unified diff to `{relative}`"))
            }
            Ok(Err(error)) => patch_write_failure(target_input, &error),
            Err(error) => ToolOutput::failure(format!(
                "cannot patch `{target_input}`: mutation task failed: {error}"
            )),
        }
    }
}

fn patch_write_failure(path: &str, error: &PathSafetyError) -> ToolOutput {
    ToolOutput::Failure {
        error: format!("cannot patch `{path}`: {error}"),
        denial: matches!(error, PathSafetyError::Denied).then_some(DenialClass::FsWrite),
    }
}

fn apply_unified_patch(source: &str, patch: &str, expected_path: &str) -> Result<String, String> {
    let patch_lines = patch.split_inclusive('\n').collect::<Vec<_>>();
    if patch_lines.len() < 3 {
        return Err("unified diff must contain file headers and at least one hunk".to_string());
    }
    validate_header(patch_lines[0], "--- ", expected_path)?;
    validate_header(patch_lines[1], "+++ ", expected_path)?;

    let source_lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let mut output = String::with_capacity(source.len().saturating_add(patch.len() / 2));
    let mut source_cursor = 0usize;
    let mut patch_cursor = 2usize;
    let mut hunks = 0usize;

    while patch_cursor < patch_lines.len() {
        if trim_line_end(patch_lines[patch_cursor]).is_empty() {
            patch_cursor += 1;
            continue;
        }
        let header = trim_line_end(patch_lines[patch_cursor]);
        let (old_start, old_count, new_count) = parse_hunk_header(header)?;
        hunks = hunks.saturating_add(1);
        let hunk_start = if old_start == 0 {
            if old_count == 0 {
                0
            } else {
                return Err("only an empty hunk may start at line 0".to_string());
            }
        } else {
            old_start.saturating_sub(1)
        };
        if hunk_start < source_cursor || hunk_start > source_lines.len() {
            return Err("hunks are out of order or outside the target file".to_string());
        }
        for line in &source_lines[source_cursor..hunk_start] {
            output.push_str(line);
        }
        source_cursor = hunk_start;
        patch_cursor += 1;
        let mut consumed_old = 0usize;
        let mut produced_new = 0usize;

        while patch_cursor < patch_lines.len()
            && !trim_line_end(patch_lines[patch_cursor]).starts_with("@@ ")
        {
            let line = patch_lines[patch_cursor];
            let Some(marker) = line.as_bytes().first().copied() else {
                return Err("empty hunk line (expected a leading space, +, or -)".to_string());
            };
            let content = line
                .get(1..)
                .ok_or_else(|| "patch line is not valid UTF-8".to_string())?;
            match marker {
                b' ' => {
                    require_source_line(&source_lines, source_cursor, content)?;
                    output.push_str(source_lines[source_cursor]);
                    source_cursor += 1;
                    consumed_old += 1;
                    produced_new += 1;
                }
                b'-' => {
                    require_source_line(&source_lines, source_cursor, content)?;
                    source_cursor += 1;
                    consumed_old += 1;
                }
                b'+' => {
                    output.push_str(content);
                    produced_new += 1;
                }
                b'\\' => {
                    return Err(
                        "`No newline at end of file` markers are not supported; use edit or write_file for this edge case"
                            .to_string(),
                    );
                }
                _ => return Err("hunk line must begin with a space, +, or -".to_string()),
            }
            patch_cursor += 1;
        }
        if consumed_old != old_count || produced_new != new_count {
            return Err(format!(
                "hunk counts do not match header (old {consumed_old}/{old_count}, new {produced_new}/{new_count})"
            ));
        }
    }
    if hunks == 0 {
        return Err("unified diff contains no @@ hunk".to_string());
    }
    for line in &source_lines[source_cursor..] {
        output.push_str(line);
    }
    if output.len() > path_safety::MAX_MUTATING_FILE_BYTES {
        return Err("patched file exceeds the 8 MiB mutation limit".to_string());
    }
    Ok(output)
}

fn validate_header(line: &str, prefix: &str, expected_path: &str) -> Result<(), String> {
    let line = trim_line_end(line);
    let value = line
        .strip_prefix(prefix)
        .ok_or_else(|| format!("missing `{prefix}` file header"))?;
    let value = value.split('\t').next().unwrap_or(value).trim();
    if value == "/dev/null" {
        return Err("file creation and deletion are not supported by apply_patch v1".to_string());
    }
    let value = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(value);
    if value != expected_path {
        return Err(format!(
            "patch header names `{value}`, expected `{expected_path}`"
        ));
    }
    Ok(())
}

fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize), String> {
    let body = line
        .strip_prefix("@@ -")
        .and_then(|value| value.split_once(" @@").map(|(ranges, _)| ranges))
        .ok_or_else(|| "invalid @@ hunk header".to_string())?;
    let (old, new) = body
        .split_once(" +")
        .ok_or_else(|| "hunk header must include old and new ranges".to_string())?;
    let (old_start, old_count) = parse_range(old)?;
    let (_, new_count) = parse_range(new)?;
    Ok((old_start, old_count, new_count))
}

fn parse_range(value: &str) -> Result<(usize, usize), String> {
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start = start
        .parse::<usize>()
        .map_err(|_| "hunk range start is not a number".to_string())?;
    let count = count
        .parse::<usize>()
        .map_err(|_| "hunk range count is not a number".to_string())?;
    Ok((start, count))
}

fn require_source_line(lines: &[&str], index: usize, expected: &str) -> Result<(), String> {
    match lines.get(index) {
        Some(actual) if *actual == expected => Ok(()),
        Some(_) => Err(format!(
            "patch context does not match target at line {}",
            index + 1
        )),
        None => Err("patch reads beyond the end of the target file".to_string()),
    }
}

fn trim_line_end(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn tool_applies_and_records_only_the_requested_file() {
        use grokforge_protocol::{SandboxPolicy, ToolCallId};
        use grokforge_sandbox::PassthroughRunner;

        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join("src")).expect("source directory");
        let target = workspace.path().join("src/lib.rs");
        std::fs::write(&target, "fn old() {}\n").expect("source file");
        let root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
        let ctx = TurnContext {
            workspace_root: root.clone(),
            policy: SandboxPolicy::workspace_write(&root),
            sandbox: Arc::new(PassthroughRunner),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: crate::TurnCancellation::new(),
        };
        let output = ApplyPatch
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({
                    "path": "src/lib.rs",
                    "patch": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n"
                }),
                ctx: &ctx,
            })
            .await;
        assert!(!output.is_error(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("patched source"),
            "fn new() {}\n"
        );
        assert_eq!(
            ctx.touched_paths(),
            vec![std::fs::canonicalize(target).expect("target")]
        );
    }

    #[test]
    fn applies_multiple_exact_hunks() {
        let source = "alpha\nbeta\ngamma\ndelta\n";
        let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+BETA\n@@ -4,1 +4,2 @@\n delta\n+epsilon\n";
        assert_eq!(
            apply_unified_patch(source, patch, "src/lib.rs").expect("valid patch"),
            "alpha\nBETA\ngamma\ndelta\nepsilon\n"
        );
    }

    #[test]
    fn rejects_wrong_file_or_stale_context() {
        let wrong_path = "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(apply_unified_patch("old\n", wrong_path, "src/lib.rs").is_err());

        let stale = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-stale\n+new\n";
        assert!(apply_unified_patch("current\n", stale, "src/lib.rs").is_err());
    }

    #[test]
    fn rejects_multi_file_and_no_newline_extensions() {
        let multi = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n--- a/src/two.rs\n+++ b/src/two.rs\n@@ -1 +1 @@\n-x\n+y\n";
        assert!(apply_unified_patch("old\n", multi, "src/lib.rs").is_err());

        let marker = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        assert!(apply_unified_patch("old\n", marker, "src/lib.rs").is_err());
    }
}
