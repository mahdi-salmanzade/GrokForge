//! Built-in tools: file read/write/edit, sandboxed shell, and read-only search (list/glob/grep).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use grokforge_protocol::ApprovalKind;
use grokforge_sandbox::CommandSpec;
use serde_json::json;

use crate::approvals::ApprovalNeed;
use crate::tools::{Tool, ToolInvocation, ToolOutput, ToolSpec, TurnContext, arg_str};

/// The name of the subagent-spawning tool, intercepted by the turn runner.
pub const SPAWN_TASK: &str = "spawn_task";

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
        if let Ok(glob) = Glob::new(g) {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return false;
    };
    set.is_match(path)
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
        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => ToolOutput::success(content),
            Err(e) => ToolOutput::failure(format!("cannot read `{path}`: {e}")),
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
        ApprovalNeed::Gated(ApprovalKind::WriteFile {
            path: ctx.resolve(path),
        })
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
        // File tools run in the host process, so they must honor the sandbox policy themselves
        // (the OS sandbox only confines shelled-out commands).
        if !inv.ctx.policy.allows_write(&resolved) {
            return ToolOutput::Failure {
                error: format!("`{path}` is outside the writable sandbox; refusing to write"),
                denial: Some(grokforge_protocol::DenialClass::FsWrite),
            };
        }
        if let Some(parent) = resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::failure(format!("cannot create parent of `{path}`: {e}"));
            }
        }
        match tokio::fs::write(&resolved, content).await {
            Ok(()) => {
                inv.ctx.record_touched(resolved);
                ToolOutput::success(format!("wrote {} bytes to `{path}`", content.len()))
            }
            Err(e) => ToolOutput::failure(format!("cannot write `{path}`: {e}")),
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
        ApprovalNeed::Gated(ApprovalKind::WriteFile {
            path: ctx.resolve(path),
        })
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
        let resolved = inv.ctx.resolve(path);
        if !inv.ctx.policy.allows_write(&resolved) {
            return ToolOutput::Failure {
                error: format!("`{path}` is outside the writable sandbox; refusing to edit"),
                denial: Some(grokforge_protocol::DenialClass::FsWrite),
            };
        }
        let original = match tokio::fs::read_to_string(&resolved).await {
            Ok(s) => s,
            Err(e) => return ToolOutput::failure(format!("cannot read `{path}`: {e}")),
        };
        let occurrences = original.matches(old).count();
        if occurrences == 0 {
            return ToolOutput::failure(format!("`old_string` not found in `{path}`"));
        }
        if occurrences > 1 && !replace_all {
            return ToolOutput::failure(format!(
                "`old_string` occurs {occurrences} times in `{path}`; pass replace_all or make it unique"
            ));
        }
        let updated = if replace_all {
            original.replace(old, new)
        } else {
            original.replacen(old, new, 1)
        };
        match tokio::fs::write(&resolved, updated).await {
            Ok(()) => {
                inv.ctx.record_touched(resolved);
                ToolOutput::success(format!("edited `{path}` ({occurrences} replacement(s))"))
            }
            Err(e) => ToolOutput::failure(format!("cannot write `{path}`: {e}")),
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
        let parts: Vec<String> = command.split_whitespace().map(String::from).collect();
        ApprovalNeed::Gated(ApprovalKind::ExecCommand {
            command: parts,
            cwd: ctx.workspace_root.clone(),
            sandbox: ctx.policy.mode,
            escalation_of: None,
        })
    }

    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let command = match arg_str(&inv.args, "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let spec = CommandSpec::shell(command, inv.ctx.workspace_root.clone());
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
                ToolOutput::success(format!("exit: {code}\n{body}"))
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
        let mut entries = match tokio::fs::read_dir(&resolved).await {
            Ok(rd) => rd,
            Err(e) => return ToolOutput::failure(format!("cannot list `{path}`: {e}")),
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = match entry.file_type().await {
                Ok(ft) if ft.is_dir() => "/",
                _ => "",
            };
            names.push(format!("{name}{suffix}"));
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
        let pattern = match arg_str(&inv.args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Ok(glob) = Glob::new(pattern) else {
            return ToolOutput::failure(format!("invalid glob `{pattern}`"));
        };
        let matcher = glob.compile_matcher();
        let root = inv.ctx.workspace_root.clone();
        let mut hits = Vec::new();
        for entry in ignore::WalkBuilder::new(&root).build().flatten() {
            if let Ok(rel) = entry.path().strip_prefix(&root) {
                if matcher.is_match(rel) {
                    hits.push(rel.to_string_lossy().into_owned());
                    if hits.len() >= 500 {
                        break;
                    }
                }
            }
        }
        hits.sort();
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
        ApprovalNeed::None
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
        let pattern = match arg_str(&inv.args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Ok(re) = regex::Regex::new(pattern) else {
            return ToolOutput::failure(format!("invalid regex `{pattern}`"));
        };
        let sub = inv.args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let root = inv.ctx.resolve(sub);
        let mut hits = Vec::new();
        for entry in ignore::WalkBuilder::new(&root).build().flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let rel = entry
                .path()
                .strip_prefix(&inv.ctx.workspace_root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            for (n, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                    if hits.len() >= 200 {
                        return ToolOutput::success(format!("{}\n… (truncated)", hits.join("\n")));
                    }
                }
            }
        }
        ToolOutput::success(if hits.is_empty() {
            "(no matches)".to_string()
        } else {
            hits.join("\n")
        })
    }
}
