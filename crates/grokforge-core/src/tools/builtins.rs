//! Built-in tools: file read/write/edit, sandboxed shell, and read-only search (list/glob/grep).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobBuilder, GlobSetBuilder};
use grokforge_protocol::ApprovalKind;
use grokforge_sandbox::CommandSpec;
use serde_json::json;

use crate::approvals::ApprovalNeed;
use crate::path_safety::{self, PathSafetyError};
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext, arg_str};

/// The name of the subagent-spawning tool, intercepted by the turn runner.
pub const SPAWN_TASK: &str = "spawn_task";

/// Whether a registered name belongs to the built-in safety/tooling surface.
#[must_use]
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "write_file" | "edit" | "shell" | "list" | "glob" | "grep" | SPAWN_TASK
    )
}

const MAX_READ_BYTES: usize = 1024 * 1024;
const MAX_LIST_ENTRIES: usize = 2_000;
const MAX_GREP_FILE_BYTES: usize = 1024 * 1024;
const MAX_GREP_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_GREP_LINE_CHARS: usize = 2_000;
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_GLOB_HITS: usize = 500;
const MAX_GREP_HITS: usize = 200;

/// Every built-in tool, ready to register.
#[must_use]
pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile),
        Arc::new(WriteFile),
        Arc::new(EditFile),
        Arc::new(Shell),
        Arc::new(ListDir),
        Arc::new(Glob_),
        Arc::new(Grep),
        Arc::new(SpawnTask),
    ]
}

/// True if `path` matches any of the policy's never-read globs (secrets).
fn is_blocked(ctx: &TurnContext, path: &Path) -> bool {
    let mut builder = GlobSetBuilder::new();
    for g in &ctx.policy.unreadable_globs {
        if let Ok(glob) = GlobBuilder::new(g).case_insensitive(true).build() {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return false;
    };
    set.is_match(path)
}

fn canonical_read_path(ctx: &TurnContext, path: &Path) -> Result<PathBuf, std::io::Error> {
    let (_workspace, canonical) =
        path_safety::canonical_workspace_target(&ctx.workspace_root, path)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error))?;
    let readable = ctx.policy.readable_roots.iter().any(|root| {
        std::fs::canonicalize(root).map_or_else(
            |_| canonical.starts_with(path_safety::normalize(root)),
            |canonical_root| canonical.starts_with(canonical_root),
        )
    });
    if readable {
        Ok(canonical)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path is outside the readable sandbox",
        ))
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max_chars).collect();
        truncated.push('…');
        truncated
    }
}

fn write_failure(path: &str, action: &str, error: &PathSafetyError) -> ToolOutput {
    // A genuine policy-boundary denial can be presented for an explicit sandbox retry. Invalid
    // targets, symlinks, and hard links are safety invariants, not retryable sandbox failures.
    let denial = matches!(error, PathSafetyError::Denied)
        .then_some(grokforge_protocol::DenialClass::FsWrite);
    ToolOutput::Failure {
        error: format!("cannot {action} `{path}`: {error}"),
        denial,
    }
}

// ---------- read_file ----------

#[derive(Debug)]
struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read the contents of a text file in the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "File path, relative to the workspace root." } },
                "required": ["path"]
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let path = match arg_str(&inv.args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let resolved = inv.ctx.resolve(path);
        if is_blocked(inv.ctx, &resolved) {
            return ToolOutput::success(format!(
                "[blocked: `{path}` matches a secrets.deny rule; contents withheld from the model]"
            ));
        }
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(e) => return ToolOutput::failure(format!("cannot read `{path}`: {e}")),
        };
        if is_blocked(inv.ctx, &canonical) {
            return ToolOutput::success(format!(
                "[blocked: `{path}` resolves to a secrets.deny path; contents withheld from the model]"
            ));
        }
        let workspace = inv.ctx.workspace_root.clone();
        let target = canonical;
        match tokio::task::spawn_blocking(move || {
            path_safety::read_workspace_text(&workspace, &target, MAX_READ_BYTES)
        })
        .await
        {
            Ok(Ok((mut content, truncated))) => {
                if truncated {
                    content.push_str("\n… [file truncated at 1 MiB] …");
                }
                ToolOutput::success(content)
            }
            Ok(Err(e)) => ToolOutput::failure(format!("cannot read `{path}`: {e}")),
            Err(e) => ToolOutput::failure(format!("cannot read `{path}`: task failed: {e}")),
        }
    }
}

// ---------- write_file ----------

#[derive(Debug)]
struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a file with the given contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let path = ctx.resolve(path);
        let kind = ApprovalKind::WriteFile { path: path.clone() };
        if ctx.policy.allows_write(&path) {
            ApprovalNeed::Gated(kind)
        } else {
            ApprovalNeed::OutsideSandbox(kind)
        }
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let path = match arg_str(&inv.args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let content = match arg_str(&inv.args, "content") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let resolved = inv.ctx.resolve(path);
        // Host-process file tools use descriptor-relative writes so symlinked components cannot
        // redirect them after policy evaluation.
        let policy = inv.ctx.policy.clone();
        let target = resolved.clone();
        let approved_target = inv.ctx.bound_write_target(&resolved);
        let content_owned = content.as_bytes().to_vec();
        match tokio::task::spawn_blocking(move || {
            path_safety::write_file_bound(
                &policy,
                &target,
                approved_target.as_deref(),
                &content_owned,
            )
        })
        .await
        {
            Ok(Ok(())) => {
                inv.ctx.record_touched(resolved);
                ToolOutput::success(format!("wrote {} bytes to `{path}`", content.len()))
            }
            Ok(Err(e)) => write_failure(path, "write", &e),
            Err(e) => ToolOutput::failure(format!("cannot write `{path}`: task failed: {e}")),
        }
    }
}

// ---------- edit ----------

#[derive(Debug)]
struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace an exact string in a file with another string.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text to replace (must be unique unless replace_all)." },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean", "default": false }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let path = ctx.resolve(path);
        let kind = ApprovalKind::WriteFile { path: path.clone() };
        if ctx.policy.allows_write(&path) {
            ApprovalNeed::Gated(kind)
        } else {
            ApprovalNeed::OutsideSandbox(kind)
        }
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let (path, old, new) = match (
            arg_str(&inv.args, "path"),
            arg_str(&inv.args, "old_string"),
            arg_str(&inv.args, "new_string"),
        ) {
            (Ok(p), Ok(o), Ok(n)) => (p, o, n),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        let replace_all = inv
            .args
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if old.is_empty() {
            return ToolOutput::failure("`old_string` must not be empty");
        }
        let resolved = inv.ctx.resolve(path);
        let policy = inv.ctx.policy.clone();
        let target = resolved.clone();
        let approved_target = inv.ctx.bound_write_target(&resolved);
        let old = old.to_string();
        let new = new.to_string();
        match tokio::task::spawn_blocking(move || {
            path_safety::edit_file_bound(
                &policy,
                &target,
                approved_target.as_deref(),
                &old,
                &new,
                replace_all,
            )
        })
        .await
        {
            Ok(Ok(occurrences)) => {
                inv.ctx.record_touched(resolved);
                ToolOutput::success(format!("edited `{path}` ({occurrences} replacement(s))"))
            }
            Ok(Err(e)) => write_failure(path, "edit", &e),
            Err(e) => ToolOutput::failure(format!("cannot edit `{path}`: task failed: {e}")),
        }
    }
}

// ---------- shell ----------

#[derive(Debug)]
struct Shell;

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell".into(),
            description: "Run a shell command in the workspace, sandboxed per the active policy."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Preserve the exact shell source. CmdPrefix approval parses a deliberately tiny safe
        // grammar from this raw value; pre-splitting would erase newlines and other separators.
        let parts = vec![command.to_string()];
        let kind = ApprovalKind::ExecCommand {
            command: parts,
            cwd: ctx.workspace_root.clone(),
            sandbox: ctx.policy.mode,
            escalation_of: None,
        };
        if ctx.policy.mode.is_sandboxed() && !ctx.sandbox.capability().enforced {
            ApprovalNeed::Always(kind)
        } else {
            ApprovalNeed::Gated(kind)
        }
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let command = match arg_str(&inv.args, "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let spec = CommandSpec::shell(command, inv.ctx.workspace_root.clone())
            .with_cancellation(inv.ctx.cancellation.process_token());
        match inv.ctx.sandbox.run(&inv.ctx.policy, &spec).await {
            Ok(out) => {
                if let Some(denial) = out.denial {
                    return ToolOutput::Failure {
                        error: format!("blocked by sandbox: {out}", out = out.stderr),
                        denial: Some(denial),
                    };
                }
                let mut body = String::new();
                if !out.stdout.is_empty() {
                    body.push_str(&out.stdout);
                }
                if !out.stderr.is_empty() {
                    body.push_str("\n[stderr]\n");
                    body.push_str(&out.stderr);
                }
                let code = out
                    .exit_code
                    .map_or_else(|| "killed".to_string(), |c| c.to_string());
                let result = format!("exit: {code}\n{body}");
                if out.succeeded() {
                    ToolOutput::success(result)
                } else {
                    ToolOutput::failure(result)
                }
            }
            Err(grokforge_sandbox::ExecError::Cancelled) => {
                ToolOutput::failure("[turn interrupted by user; command killed and reaped]")
            }
            Err(e) => ToolOutput::failure(format!("failed to run command: {e}")),
        }
    }
}

// ---------- list ----------

#[derive(Debug)]
struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list".into(),
            description: "List the entries of a directory in the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "default": "." } }
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let path = inv.args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let resolved = inv.ctx.resolve(path);
        let canonical = match canonical_read_path(inv.ctx, &resolved) {
            Ok(path) => path,
            Err(e) => return ToolOutput::failure(format!("cannot list `{path}`: {e}")),
        };
        let workspace = inv.ctx.workspace_root.clone();
        let target = canonical;
        let (listed, truncated) = match tokio::task::spawn_blocking(move || {
            path_safety::list_workspace_dir(&workspace, &target, MAX_LIST_ENTRIES)
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => return ToolOutput::failure(format!("cannot list `{path}`: {e}")),
            Err(e) => {
                return ToolOutput::failure(format!("cannot list `{path}`: task failed: {e}"));
            }
        };
        let mut names: Vec<String> = listed
            .into_iter()
            .map(|(name, directory)| format!("{name}{}", if directory { "/" } else { "" }))
            .collect();
        if truncated {
            names.push("… [listing truncated] …".to_string());
        }
        names.sort();
        ToolOutput::success(names.join("\n"))
    }
}

// ---------- glob ----------

#[derive(Debug)]
#[allow(clippy::doc_markdown)]
struct Glob_;

#[async_trait]
impl Tool for Glob_ {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files matching a glob pattern (respects .gitignore).".into(),
            parameters: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string", "description": "e.g. **/*.rs" } },
                "required": ["pattern"]
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        if cfg!(not(unix)) {
            return ToolOutput::failure(
                "host filesystem tools require Unix descriptor-relative path safety; use WSL2 on Windows",
            );
        }
        let pattern = match arg_str(&inv.args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Ok(glob) = Glob::new(pattern) else {
            return ToolOutput::failure(format!("invalid glob `{pattern}`"));
        };
        let matcher = glob.compile_matcher();
        let root = inv.ctx.workspace_root.clone();
        let (mut hits, truncated) = match tokio::task::spawn_blocking(move || {
            glob_walk(&root, &matcher, MAX_WALK_ENTRIES, MAX_GLOB_HITS)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => return ToolOutput::failure(format!("glob task failed: {error}")),
        };
        hits.sort();
        if truncated {
            hits.push("… (scan truncated)".to_string());
        }
        ToolOutput::success(if hits.is_empty() {
            "(no matches)".to_string()
        } else {
            hits.join("\n")
        })
    }
}

// ---------- spawn_task (subagent) ----------
//
// The runtime intercepts this by name and runs a sub-agent in an isolated git worktree; the
// `invoke` below is only reached if subagents are disabled (e.g. inside a subagent).

#[derive(Debug)]
struct SpawnTask;

#[async_trait]
impl Tool for SpawnTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SPAWN_TASK.to_string(),
            description: "Run a self-contained subtask in an isolated git worktree and return its \
                          result. Use for parallelizable or exploratory work."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "prompt": { "type": "string", "description": "The subtask to perform." } },
                "required": ["prompt"]
            }),
            mutating: true,
            parallel_safe: false,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::Always(ApprovalKind::GitMutation {
            description: "create an isolated subagent worktree and branch".to_string(),
            command: vec!["git".to_string(), "worktree".to_string(), "add".to_string()],
        })
    }

    async fn invoke(&self, _inv: ToolInvocation<'_>) -> ToolOutput {
        ToolOutput::failure("subagents cannot spawn further subagents")
    }
}

// ---------- grep ----------

#[derive(Debug)]
struct Grep;

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search workspace files for a regular expression (respects .gitignore)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "default": "." }
                },
                "required": ["pattern"]
            }),
            mutating: false,
            parallel_safe: true,
        }
    }

    fn approval(&self, _args: &serde_json::Value, _ctx: &TurnContext) -> ApprovalNeed {
        ApprovalNeed::None
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        if cfg!(not(unix)) {
            return ToolOutput::failure(
                "host filesystem tools require Unix descriptor-relative path safety; use WSL2 on Windows",
            );
        }
        let pattern = match arg_str(&inv.args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Ok(re) = regex::Regex::new(pattern) else {
            return ToolOutput::failure(format!("invalid regex `{pattern}`"));
        };
        let sub = inv.args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let requested_root = inv.ctx.resolve(sub);
        if is_blocked(inv.ctx, &requested_root) {
            return ToolOutput::success(
                "[blocked: search path matches a secrets.deny rule]".to_string(),
            );
        }
        let root = match canonical_read_path(inv.ctx, &requested_root) {
            Ok(path) => path,
            Err(e) => return ToolOutput::failure(format!("cannot search `{sub}`: {e}")),
        };
        if is_blocked(inv.ctx, &root) {
            return ToolOutput::success(
                "[blocked: search path resolves to a secrets.deny path]".to_string(),
            );
        }
        let ctx = inv.ctx.clone();
        match tokio::task::spawn_blocking(move || grep_walk(&ctx, &root, &re, MAX_WALK_ENTRIES))
            .await
        {
            Ok(result) => ToolOutput::success(result),
            Err(error) => ToolOutput::failure(format!("grep task failed: {error}")),
        }
    }
}

fn glob_walk(
    root: &Path,
    matcher: &globset::GlobMatcher,
    max_entries: usize,
    max_hits: usize,
) -> (Vec<String>, bool) {
    let mut hits = Vec::new();
    let mut visited = 0usize;
    let mut truncated = false;
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        visited = visited.saturating_add(1);
        if visited > max_entries {
            truncated = true;
            break;
        }
        if let Ok(relative) = entry.path().strip_prefix(root)
            && matcher.is_match(relative)
        {
            hits.push(relative.to_string_lossy().into_owned());
            if hits.len() >= max_hits {
                truncated = true;
                break;
            }
        }
    }
    (hits, truncated)
}

fn grep_walk(
    ctx: &TurnContext,
    root: &Path,
    expression: &regex::Regex,
    max_entries: usize,
) -> String {
    let mut hits = Vec::new();
    let mut output_bytes = 0usize;
    let mut visited = 0usize;
    let mut truncated = false;
    'walk: for entry in ignore::WalkBuilder::new(root).build().flatten() {
        visited = visited.saturating_add(1);
        if visited > max_entries {
            truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if is_blocked(ctx, entry.path()) || is_blocked(ctx, &canonical) {
            continue;
        }
        let Ok((content, file_truncated)) =
            path_safety::read_workspace_text(&ctx.workspace_root, &canonical, MAX_GREP_FILE_BYTES)
        else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        for (line_number, line) in content.lines().enumerate() {
            if expression.is_match(line) {
                let hit = format!(
                    "{relative}:{}: {}",
                    line_number + 1,
                    truncate_chars(line.trim(), MAX_GREP_LINE_CHARS)
                );
                output_bytes = output_bytes.saturating_add(hit.len() + 1);
                hits.push(hit);
                if hits.len() >= MAX_GREP_HITS || output_bytes >= MAX_GREP_OUTPUT_BYTES {
                    truncated = true;
                    break 'walk;
                }
            }
        }
        truncated |= file_truncated;
    }
    if truncated {
        hits.push("… (scan truncated)".to_string());
    }
    if hits.is_empty() {
        "(no matches)".to_string()
    } else {
        hits.join("\n")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use grokforge_protocol::{SandboxMode, SandboxPolicy, ToolCallId};
    use grokforge_sandbox::{
        CommandSpec, ExecError, ExecOutput, PassthroughRunner, SandboxCapability, SandboxRunner,
    };

    use super::*;

    fn context(root: &Path, mode: SandboxMode) -> TurnContext {
        let policy = match mode {
            SandboxMode::ReadOnly => SandboxPolicy::read_only(root),
            SandboxMode::WorkspaceWrite => SandboxPolicy::workspace_write(root),
            SandboxMode::DangerFullAccess => SandboxPolicy::danger_full_access(root),
        };
        TurnContext {
            workspace_root: root.to_path_buf(),
            policy,
            sandbox: Arc::new(PassthroughRunner),
            touched: Arc::new(std::sync::Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: crate::TurnCancellation::new(),
        }
    }

    #[tokio::test]
    async fn parent_traversal_write_is_denied() {
        let workspace = tempfile::tempdir().unwrap();
        let parent = workspace.path().parent().unwrap();
        let outside = parent.join("grokforge-parent-escape-test.txt");
        let _ = std::fs::remove_file(&outside);
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = WriteFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"../grokforge-parent-escape-test.txt","content":"escape"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert!(!outside.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_parent_cannot_redirect_write_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = WriteFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"link/escaped.txt","content":"escape"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert!(!outside.path().join("escaped.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_write_cannot_modify_outside_inode() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside original").unwrap();
        std::fs::hard_link(outside.path(), workspace.path().join("alias.txt")).unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = WriteFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"alias.txt","content":"changed"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "outside original"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_read_cannot_disclose_outside_inode() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret").unwrap();
        std::fs::hard_link(outside.path(), workspace.path().join("innocent.txt")).unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = ReadFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"innocent.txt"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert!(!output.content().contains("outside secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_blocks_secret_reached_through_symlink() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join(".env"),
            "UNUSUAL_CREDENTIAL=do-not-leak",
        )
        .unwrap();
        std::os::unix::fs::symlink(".env", workspace.path().join("innocent.txt")).unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = ReadFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"innocent.txt"}),
                ctx: &ctx,
            })
            .await;
        assert!(!output.content().contains("do-not-leak"));
        assert!(output.content().contains("blocked"));
    }

    #[tokio::test]
    async fn read_truncates_on_a_utf8_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'a'; MAX_READ_BYTES - 1];
        bytes.extend_from_slice("é".as_bytes());
        bytes.push(b'b');
        std::fs::write(workspace.path().join("large.txt"), bytes).unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = ReadFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"large.txt"}),
                ctx: &ctx,
            })
            .await;
        assert!(!output.is_error());
        assert!(output.content().contains("file truncated"));
    }

    #[tokio::test]
    async fn direct_read_list_and_grep_cannot_escape_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("ordinary-name.txt");
        std::fs::write(&secret, "UNMATCHED_PRIVATE_VALUE").unwrap();
        let ctx = context(workspace.path(), SandboxMode::DangerFullAccess);

        let read = ReadFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":secret.to_string_lossy()}),
                ctx: &ctx,
            })
            .await;
        assert!(read.is_error());
        assert!(!read.content().contains("UNMATCHED_PRIVATE_VALUE"));

        let list = ListDir
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":outside.path().to_string_lossy()}),
                ctx: &ctx,
            })
            .await;
        assert!(list.is_error());

        let grep = Grep
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":outside.path().to_string_lossy(),"pattern":"PRIVATE"}),
                ctx: &ctx,
            })
            .await;
        assert!(grep.is_error());
        assert!(!grep.content().contains("UNMATCHED_PRIVATE_VALUE"));
    }

    #[test]
    fn glob_scan_cap_applies_even_when_nothing_matches() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..10 {
            std::fs::write(workspace.path().join(format!("file-{index}.txt")), "x").unwrap();
        }
        let matcher = Glob::new("**/*.never").unwrap().compile_matcher();
        let (hits, truncated) = glob_walk(workspace.path(), &matcher, 3, 500);
        assert!(hits.is_empty());
        assert!(truncated);
    }

    #[tokio::test]
    async fn empty_edit_needle_is_rejected_without_modifying_file() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("a.txt");
        std::fs::write(&file, "abc").unwrap();
        let ctx = context(workspace.path(), SandboxMode::WorkspaceWrite);
        let output = EditFile
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"path":"a.txt","old_string":"","new_string":"x"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "abc");
    }

    #[tokio::test]
    async fn nonzero_shell_exit_is_a_tool_failure() {
        #[derive(Debug)]
        struct ExitRunner;

        #[async_trait]
        impl SandboxRunner for ExitRunner {
            fn capability(&self) -> SandboxCapability {
                SandboxCapability {
                    backend: "test".into(),
                    enforced: true,
                    notes: vec![],
                }
            }

            async fn run(
                &self,
                _policy: &SandboxPolicy,
                _command: &CommandSpec,
            ) -> Result<ExecOutput, ExecError> {
                Ok(ExecOutput {
                    exit_code: Some(7),
                    stdout: String::new(),
                    stderr: "failed".into(),
                    truncated: false,
                    timed_out: false,
                    denial: None,
                })
            }
        }

        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = context(workspace.path(), SandboxMode::DangerFullAccess);
        ctx.sandbox = Arc::new(ExitRunner);
        let output = Shell
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: json!({"command":"exit 7"}),
                ctx: &ctx,
            })
            .await;
        assert!(output.is_error());
        assert!(output.content().contains("exit: 7"));
    }
}
