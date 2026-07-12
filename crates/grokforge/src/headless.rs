//! Headless mode: `grokforge exec -p "..."`. Runs one turn without the TUI, streaming events
//! to stdout (plain text or `--json` NDJSON) and returning a CI-friendly exit code. Approvals
//! are resolved non-interactively — auto-denied with feedback unless `--allow`/`--yolo`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use grokforge_core::{
    Agent, AllowRule, AutoApprover, RolloutWriter, Session, SessionConfig, SessionMeta,
    ToolRegistry, TurnCancellation, sessions_dir,
};
use grokforge_protocol::{ApprovalPolicy, EventMsg, NetworkMode, SandboxMode, StopReason};
use grokforge_sandbox::default_runner;
use grokforge_xai::{Effort, XaiClient};
use tokio::sync::mpsc;

/// Parsed `exec` options.
#[derive(Debug)]
pub struct ExecArgs {
    pub prompt: String,
    pub preset: String,
    pub model: Option<String>,
    pub json: bool,
    pub cd: Option<PathBuf>,
    pub allow: Vec<String>,
    pub effort: Option<String>,
    pub plan: bool,
    pub max_iterations: u32,
}

fn preset_policy(preset: &str) -> Option<(ApprovalPolicy, SandboxMode)> {
    match preset {
        "readonly" => Some((ApprovalPolicy::OnRequest, SandboxMode::ReadOnly)),
        "auto" => Some((ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite)),
        "strict" => Some((ApprovalPolicy::Untrusted, SandboxMode::WorkspaceWrite)),
        "yolo" => Some((ApprovalPolicy::Never, SandboxMode::DangerFullAccess)),
        _ => None,
    }
}

fn parse_allow(specs: &[String], workspace: &std::path::Path) -> Result<Vec<AllowRule>, String> {
    specs
        .iter()
        .map(|s| {
            if s == "network" {
                Ok(AllowRule::Network)
            } else if let Some(p) = s.strip_prefix("write:") {
                if p.trim().is_empty() {
                    return Err("--allow write: requires a non-empty path".to_string());
                }
                normalize_path(workspace, &PathBuf::from(p))
                    .map(AllowRule::Write)
                    .map_err(|error| format!("invalid --allow `{s}`: {error}"))
            } else if let Some(p) = s.strip_prefix("cmd:") {
                if p.trim().is_empty() {
                    Err("--allow cmd: requires a non-empty command prefix".to_string())
                } else {
                    Ok(AllowRule::CmdPrefix(p.to_string()))
                }
            } else {
                Err(format!(
                    "unrecognized --allow `{s}` (expected network, write:<path>, or cmd:<prefix>)"
                ))
            }
        })
        .collect()
}

fn grants_network(rules: &[AllowRule]) -> bool {
    rules
        .iter()
        .any(|rule| matches!(rule, AllowRule::Network | AllowRule::All))
}

fn parse_effort(s: &str) -> Option<Effort> {
    match s {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)] // Linear validation/setup/event-drain flow is easier to audit.
pub async fn run(args: ExecArgs) -> ExitCode {
    let Some((policy, mode)) = preset_policy(&args.preset) else {
        eprintln!(
            "unknown --preset `{}` (readonly|auto|strict|yolo)",
            args.preset
        );
        return ExitCode::from(2);
    };

    if args.prompt.trim().is_empty() {
        eprintln!("prompt must not be empty");
        return ExitCode::from(2);
    }
    if args.max_iterations == 0 {
        eprintln!("--max-iterations must be at least 1");
        return ExitCode::from(2);
    }

    let workspace = match resolve_workspace(args.cd.as_deref()) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("invalid --cd: {error}");
            return ExitCode::from(2);
        }
    };
    let allow = match parse_allow(&args.allow, &workspace) {
        Ok(allow) => allow,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let network_allowed = grants_network(&allow);
    let effort = match args.effort.as_deref() {
        Some(value) => {
            if let Some(effort) = parse_effort(value) {
                Some(effort)
            } else {
                eprintln!("invalid --effort `{value}` (low|medium|high)");
                return ExitCode::from(2);
            }
        }
        None => None,
    };

    let Ok(api_key) = std::env::var("XAI_API_KEY") else {
        eprintln!("XAI_API_KEY is not set (run `grokforge login` once it lands, or export it)");
        return ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return ExitCode::from(2);
        }
    };

    let model = args.model.unwrap_or_else(|| "grok-build-0.1".to_string());
    if let Err(code) = crate::validate_model_startup(&client, &model).await {
        return code;
    }
    let approver: Arc<AutoApprover> = if args.preset == "yolo" {
        Arc::new(AutoApprover::yolo())
    } else {
        Arc::new(AutoApprover::new(allow))
    };

    let mut config = SessionConfig::new(workspace.clone(), model).with_policy(policy, mode);
    if network_allowed {
        config.network = NetworkMode::Full;
    }
    config.max_iterations = args.max_iterations;
    config.effort = effort;
    protect_user_changes(&mut config);
    let mut session = Session::new(config);

    // Persist this run so it is listable/resumable via `grokforge sessions`/`resume`.
    let dir = match sessions_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("could not locate secure session storage: {error}");
            return ExitCode::from(2);
        }
    };
    let rollout = match RolloutWriter::create(&dir, session.id).await {
        Ok(rollout) => rollout,
        Err(error) => {
            eprintln!("could not open durable session transcript: {error}");
            return ExitCode::from(2);
        }
    };
    let meta = SessionMeta::new(
        session.id,
        session.config.workspace_root.clone(),
        session.config.model.clone(),
        &args.prompt,
    );
    if let Err(error) = meta.write(&dir, session.id).await {
        eprintln!("could not persist session metadata: {error}");
        return ExitCode::from(2);
    }

    // Only start configured MCP subprocesses after the canonical recovery record is durable.
    let mut registry = ToolRegistry::with_builtins();
    let connected =
        grokforge_core::mcp_config::connect_and_register(&workspace, &mut registry).await;
    if !connected.is_empty() {
        eprintln!(
            "mcp: connected {}",
            crate::sanitize_terminal_line(&connected.join(", "))
        );
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = Agent::new(client, registry, default_runner(), approver, tx);

    let prompt = args.prompt;
    let json = args.json;
    let plan = args.plan;
    let cancellation = TurnCancellation::new();
    let task_cancellation = cancellation.clone();
    let mut handle = tokio::spawn(async move {
        let mut rollout = Some(rollout);
        if plan {
            agent
                .run_plan_turn_cancellable(&mut session, &prompt, &mut rollout, &task_cancellation)
                .await
        } else {
            agent
                .run_turn_cancellable(&mut session, &prompt, &mut rollout, &task_cancellation)
                .await
        }
    });

    let mut had_error = false;
    let mut interrupted = false;
    let mut listen_for_interrupt = true;
    let stop = loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c(), if listen_for_interrupt => {
                match signal {
                    Ok(()) => {
                        interrupted = true;
                        listen_for_interrupt = false;
                        cancellation.cancel();
                        eprintln!("[interrupt] stopping safely; waiting for active host operation to finish");
                    }
                    Err(error) => {
                        listen_for_interrupt = false;
                        eprintln!("[warning] could not install Ctrl+C handler: {}", crate::sanitize_terminal(&error.to_string()));
                    }
                }
            }
            Some(ev) = rx.recv() => {
                if matches!(ev, EventMsg::Error { .. }) {
                    had_error = true;
                }
                emit(&ev, json);
            }
            result = &mut handle => break result.unwrap_or(StopReason::Error),
        }
    };

    // The agent's sender is dropped before its task joins, so this drains every FIFO event that
    // preceded TurnComplete even if the join branch won the final select race.
    while let Some(ev) = rx.recv().await {
        if matches!(ev, EventMsg::Error { .. }) {
            had_error = true;
        }
        emit(&ev, json);
    }

    if interrupted {
        ExitCode::from(130)
    } else {
        exit_code(&stop, had_error)
    }
}

fn emit(ev: &EventMsg, json: bool) {
    if json {
        if let Ok(line) = serde_json::to_string(ev) {
            println!("{line}");
        }
        return;
    }
    // Plain-text mode: assistant text to stdout, progress to stderr.
    match ev {
        EventMsg::AgentMessageDelta { delta } => {
            use std::io::Write as _;
            print!("{}", crate::sanitize_terminal(delta));
            let _ = std::io::stdout().flush();
        }
        EventMsg::ToolCallBegin {
            name, args_preview, ..
        } => {
            eprintln!(
                "[tool] {} {}",
                crate::sanitize_terminal_line(name),
                crate::sanitize_terminal(args_preview)
            );
        }
        EventMsg::ToolCallEnd { ok, summary, .. } => {
            eprintln!(
                "[tool] {} — {}",
                if *ok { "ok" } else { "failed/denied" },
                crate::sanitize_terminal(summary)
            );
        }
        EventMsg::Committed { sha, message } => {
            let short = &sha[..sha.len().min(8)];
            eprintln!("[commit {short}] {}", crate::sanitize_terminal(message));
        }
        EventMsg::TurnComplete { stop, .. } => {
            println!();
            eprintln!("[done: {stop:?}]");
        }
        EventMsg::Error { message, .. } => {
            eprintln!("[error] {}", crate::sanitize_terminal(message));
        }
        _ => {}
    }
}

fn exit_code(stop: &StopReason, had_error: bool) -> ExitCode {
    match stop {
        StopReason::EndTurn if !had_error => ExitCode::SUCCESS,
        StopReason::Interrupted => ExitCode::from(130),
        _ => ExitCode::from(1),
    }
}

fn resolve_workspace(cd: Option<&std::path::Path>) -> Result<PathBuf, String> {
    let requested = match cd {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => std::env::current_dir()
            .map_err(|error| format!("cannot read current directory: {error}"))?
            .join(path),
        None => std::env::current_dir()
            .map_err(|error| format!("cannot read current directory: {error}"))?,
    };
    let workspace = std::fs::canonicalize(&requested)
        .map_err(|error| format!("{}: {error}", requested.display()))?;
    if !workspace.is_dir() {
        return Err(format!("{} is not a directory", workspace.display()));
    }
    Ok(workspace)
}

fn normalize_path(workspace: &std::path::Path, path: &std::path::Path) -> Result<PathBuf, String> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    if let Ok(canonical) = std::fs::canonicalize(&joined) {
        return Ok(canonical);
    }

    let mut missing = Vec::new();
    let mut ancestor = joined.as_path();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return Err(format!("cannot resolve {}", joined.display()));
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return Err(format!("cannot resolve {}", joined.display()));
        };
        ancestor = parent;
    }
    let mut normalized = std::fs::canonicalize(ancestor)
        .map_err(|error| format!("{}: {error}", ancestor.display()))?;
    for component in missing.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

fn protect_user_changes(config: &mut SessionConfig) {
    if !config.auto_commit {
        return;
    }
    let Some(git) = grokforge_git::Git::discover(&config.workspace_root) else {
        return;
    };
    match git.is_dirty() {
        Ok(false) => {}
        Ok(true) => {
            config.auto_commit = false;
            eprintln!(
                "warning: auto-commit disabled because the workspace has pre-existing changes"
            );
        }
        Err(error) => {
            config.auto_commit = false;
            eprintln!(
                "warning: auto-commit disabled because workspace cleanliness could not be verified: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn allow_rules_reject_empty_or_unknown_boundaries() {
        let workspace = std::env::temp_dir();
        assert!(parse_allow(&["cmd:".to_string()], &workspace).is_err());
        assert!(parse_allow(&["write:".to_string()], &workspace).is_err());
        assert!(parse_allow(&["everything".to_string()], &workspace).is_err());
    }

    #[test]
    fn network_allow_is_carried_into_the_base_sandbox_grant() {
        let workspace = std::env::temp_dir();
        let rules = parse_allow(&["network".to_string()], &workspace).expect("network rule");
        assert!(grants_network(&rules));
        let mut config = SessionConfig::new(workspace, "model")
            .with_policy(ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite);
        if grants_network(&rules) {
            config.network = NetworkMode::Full;
        }
        assert_eq!(config.network, NetworkMode::Full);
        assert_eq!(config.sandbox_mode, SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn relative_write_allow_is_anchored_to_canonical_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let rules =
            parse_allow(&["write:generated".to_string()], workspace.path()).expect("allow rule");
        let expected = std::fs::canonicalize(workspace.path())
            .expect("canonical workspace")
            .join("generated");
        assert!(matches!(
            &rules[0],
            AllowRule::Write(path) if path == &expected
        ));
    }

    #[test]
    fn resolve_workspace_rejects_files_and_missing_paths() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("file");
        std::fs::write(&file, "x").expect("file");
        assert!(resolve_workspace(Some(&file)).is_err());
        assert!(resolve_workspace(Some(&dir.path().join("missing"))).is_err());
    }

    #[test]
    fn max_iterations_is_not_a_successful_stop() {
        assert_eq!(
            exit_code(&StopReason::MaxIterations, false),
            ExitCode::from(1)
        );
    }

    #[test]
    fn dirty_workspace_disables_auto_commit() {
        use std::process::Command;

        let dir = tempfile::tempdir().expect("workspace");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(dir.path().join("user.txt"), "user change\n").expect("user change");
        let mut config = SessionConfig::new(dir.path().to_path_buf(), "model");
        protect_user_changes(&mut config);
        assert!(!config.auto_commit);
    }
}
