//! Headless mode: `grokforge exec -p "..."`. Runs one turn without the TUI, streaming events
//! to stdout (plain text or `--json` NDJSON) and returning a CI-friendly exit code. Approvals
//! are resolved non-interactively — auto-denied with feedback unless `--allow`/`--yolo`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use grokforge_core::{
    Agent, AllowRule, AutoApprover, Session, SessionConfig, SessionMeta, ToolRegistry,
    TurnCancellation, sessions_dir,
};
use grokforge_protocol::{
    ApprovalPolicy, EventMsg, LedgerEntry, NetworkMode, SandboxMode, StopReason,
};
use grokforge_sandbox::default_runner;
use grokforge_xai::{Effort, ServerTool, XaiClient, model_supports_effort};
use tokio::sync::mpsc;

/// Parsed `exec` options.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI opt-ins; grouping them would obscure flag provenance.
pub struct ExecArgs {
    pub prompt: String,
    pub preset: String,
    pub model: Option<String>,
    pub json: bool,
    pub cd: Option<PathBuf>,
    pub allow: Vec<String>,
    pub effort: Option<String>,
    pub plan: bool,
    pub web_search: bool,
    pub x_search: bool,
    pub code_interpreter: bool,
    pub max_iterations: Option<u32>,
    pub trust_project_mcp: bool,
    pub trust_project_config: bool,
    pub trust_project_tools: bool,
    /// Owner-private JSON export of collected ledger entries (`--ledger <path>`).
    pub ledger_path: Option<PathBuf>,
}

pub(crate) fn preset_policy(preset: &str) -> Option<(ApprovalPolicy, SandboxMode)> {
    match preset {
        "readonly" => Some((ApprovalPolicy::OnRequest, SandboxMode::ReadOnly)),
        "auto" => Some((ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite)),
        "strict" => Some((ApprovalPolicy::Untrusted, SandboxMode::WorkspaceWrite)),
        "yolo" => Some((ApprovalPolicy::Never, SandboxMode::DangerFullAccess)),
        _ => None,
    }
}

pub(crate) fn parse_allow(
    specs: &[String],
    workspace: &std::path::Path,
) -> Result<Vec<AllowRule>, String> {
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
            } else if let Some(server) = s.strip_prefix("mcp:") {
                if server.trim().is_empty()
                    || server.len() > 256
                    || server.chars().any(char::is_control)
                {
                    Err("--allow mcp: requires a non-empty MCP server name of at most 256 bytes"
                        .to_string())
                } else {
                    Ok(AllowRule::McpServer(server.to_string()))
                }
            } else {
                Err(format!(
                    "unrecognized --allow `{s}` (expected network, write:<path>, cmd:<prefix>, or mcp:<server>)"
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

pub(crate) fn parse_effort(s: &str) -> Result<Option<Effort>, ()> {
    match s {
        "auto" => Ok(None),
        "low" => Ok(Some(Effort::Low)),
        "medium" => Ok(Some(Effort::Medium)),
        "high" => Ok(Some(Effort::High)),
        "xhigh" => Ok(Some(Effort::Xhigh)),
        _ => Err(()),
    }
}

pub(crate) fn configured_effort(effort: grokforge_config::Effort) -> Effort {
    match effort {
        grokforge_config::Effort::Low => Effort::Low,
        grokforge_config::Effort::Medium => Effort::Medium,
        grokforge_config::Effort::High => Effort::High,
        grokforge_config::Effort::Xhigh => Effort::Xhigh,
    }
}

/// Register project-provided executable capabilities for an execute turn. Plan mode deliberately
/// returns before reading either trusted manifest: a project MCP process can perform side effects
/// during initialization, and a custom tool marked read-only is still an external executable.
async fn register_project_capabilities(
    workspace: &std::path::Path,
    plan: bool,
    trust_project_mcp: bool,
    trust_project_tools: bool,
    registry: &mut ToolRegistry,
    events: &mpsc::UnboundedSender<EventMsg>,
) -> Vec<String> {
    if plan {
        if trust_project_mcp || trust_project_tools {
            eprintln!(
                "plan mode: trusted project MCP servers and custom executables were not loaded"
            );
        }
        return Vec::new();
    }

    let custom_tools = grokforge_core::tools::custom::register_custom_tools(
        workspace,
        trust_project_tools,
        registry,
    );
    if !custom_tools.registered.is_empty() {
        eprintln!(
            "custom tools: loaded {}",
            crate::sanitize_terminal_line(&custom_tools.registered.join(", "))
        );
    }
    for warning in custom_tools.warnings {
        eprintln!("custom tools: {}", crate::sanitize_terminal_line(&warning));
    }

    if trust_project_mcp {
        eprintln!("{}", grokforge_core::mcp_config::PROJECT_MCP_TRUST_WARNING);
        let mcp_oauth_tokens = crate::credentials::mcp_access_tokens(workspace).await;
        grokforge_core::mcp_config::connect_and_register_trusted_with_events_and_oauth(
            workspace,
            registry,
            Some(events.clone()),
            &mcp_oauth_tokens,
        )
        .await
    } else {
        grokforge_core::mcp_config::connect_and_register(workspace, registry).await
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
    if args
        .max_iterations
        .is_some_and(|iterations| !(1..=256).contains(&iterations))
    {
        eprintln!("--max-iterations must be between 1 and 256");
        return ExitCode::from(2);
    }
    let workspace = match resolve_workspace(args.cd.as_deref()) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("invalid --cd: {error}");
            return ExitCode::from(2);
        }
    };
    let settings = match grokforge_config::Config::load_with_project_config(
        &workspace,
        args.trust_project_config,
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "configuration error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
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
            let Ok(effort) = parse_effort(value) else {
                eprintln!("invalid --effort `{value}` (auto|low|medium|high|xhigh)");
                return ExitCode::from(2);
            };
            effort
        }
        None if args.plan => Some(Effort::High),
        None => settings.agent.effort.map(configured_effort),
    };
    let model = args.model.unwrap_or_else(|| {
        if args.plan {
            settings.agent.plan_model.clone()
        } else {
            settings.agent.default_model.clone()
        }
    });
    // Headless: env override → unlock an existing encrypted file when attached to a terminal.
    // First-run onboarding stays disabled so scripts and CI never create credentials implicitly.
    let Some(api_key) = crate::credentials::resolve(false).await else {
        return ExitCode::from(3);
    };
    let base_url =
        std::env::var("XAI_BASE_URL").unwrap_or_else(|_| settings.provider.grok.base_url.clone());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return ExitCode::from(2);
        }
    };

    let model_catalog = match crate::model_catalog_startup(&client, &model).await {
        Ok(models) => models,
        Err(code) => return code,
    };
    let selected_model = model_catalog.iter().find(|candidate| {
        candidate.id == model || candidate.aliases.iter().any(|alias| alias == &model)
    });
    let active_model = selected_model.map_or(model, |candidate| candidate.id.clone());
    let context_window_tokens = selected_model.and_then(|candidate| candidate.context_window);
    if effort.is_some_and(|effort| !model_supports_effort(&active_model, effort)) {
        eprintln!("reasoning effort `xhigh` requires an xAI multi-agent model");
        return ExitCode::from(2);
    }
    let approver: Arc<AutoApprover> = if args.preset == "yolo" {
        Arc::new(AutoApprover::yolo())
    } else {
        Arc::new(AutoApprover::new(allow))
    };

    let mut config = SessionConfig::new(workspace.clone(), active_model).with_policy(policy, mode);
    if network_allowed {
        config.network = NetworkMode::Full;
    }
    config.max_iterations = args.max_iterations.unwrap_or(settings.agent.max_iterations);
    config.effort = effort;
    config.context_window_tokens = context_window_tokens;
    config.auto_compact = settings.agent.auto_compact;
    config.compaction_trigger_bytes = settings.agent.compaction_trigger_bytes;
    config.compaction_keep_tail = settings.agent.compaction_keep_tail;
    if args.web_search {
        config.enabled_server_tools.insert(ServerTool::WebSearch);
    }
    if args.x_search {
        config.enabled_server_tools.insert(ServerTool::XSearch);
    }
    if args.code_interpreter {
        config
            .enabled_server_tools
            .insert(ServerTool::CodeInterpreter);
    }
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
    let meta = SessionMeta::new(
        session.id,
        session.config.workspace_root.clone(),
        session.config.model.clone(),
        &args.prompt,
    )
    .with_effort(session.config.effort);
    let rollout = match meta.create_rollout(&dir, session.id).await {
        Ok(rollout) => rollout,
        Err(error) => {
            eprintln!("could not create durable session record: {error}");
            return ExitCode::from(2);
        }
    };

    // Create the event channel before MCP startup so remote transport egress is ledgered from the
    // initialize handshake onward.
    let (tx, mut rx) = mpsc::unbounded_channel();

    // Only start configured MCP subprocesses after the canonical recovery record is durable.
    let mut registry = ToolRegistry::with_builtins();
    let connected = register_project_capabilities(
        &workspace,
        args.plan,
        args.trust_project_mcp,
        args.trust_project_tools,
        &mut registry,
        &tx,
    )
    .await;
    if !connected.is_empty() {
        eprintln!(
            "mcp: connected {}",
            crate::sanitize_terminal_line(&connected.join(", "))
        );
    }

    let agent = Agent::new(client, registry, default_runner(), approver, tx);

    let prompt = args.prompt;
    let json = args.json;
    let plan = args.plan;
    let ledger_path = args.ledger_path;
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
    let mut ledger = LedgerLog::default();
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
                if is_error_event(&ev) {
                    had_error = true;
                }
                ledger.record_event(&ev);
                emit(&ev, json);
            }
            result = &mut handle => break result.unwrap_or(StopReason::Error),
        }
    };

    // The agent's sender is dropped before its task joins, so this drains every FIFO event that
    // preceded TurnComplete even if the join branch won the final select race.
    while let Some(ev) = rx.recv().await {
        if is_error_event(&ev) {
            had_error = true;
        }
        ledger.record_event(&ev);
        emit(&ev, json);
    }

    let export_failed = finish_ledger(json, ledger_path.as_deref(), &ledger);

    if interrupted {
        ExitCode::from(130)
    } else if export_failed && matches!(stop, StopReason::EndTurn) && !had_error {
        ExitCode::from(2)
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
        EventMsg::SubagentStarted {
            label,
            index,
            total,
            ..
        } => {
            eprintln!(
                "[agent {}/{}] {}",
                index + 1,
                total,
                crate::sanitize_terminal_line(label)
            );
        }
        EventMsg::SubagentUpdate { agent_id, inner } => emit_subagent_plain(agent_id, inner),
        EventMsg::SubagentFinished { ok, summary, .. } => {
            eprintln!(
                "[agent {}] {}",
                if *ok { "done" } else { "failed" },
                crate::sanitize_terminal(summary)
            );
        }
        _ => {}
    }
}

/// Whether an event (including one wrapped inside a subagent lane) reports an error, so headless
/// exit-code accounting stays correct despite the per-lane attribution.
fn is_error_event(ev: &EventMsg) -> bool {
    match ev {
        EventMsg::Error { .. } => true,
        EventMsg::SubagentUpdate { inner, .. } => is_error_event(inner),
        _ => false,
    }
}

/// Plain-text rendering of a subagent's inner event: tool activity, commits, and errors are shown
/// on stderr tagged with a short lane id. Assistant/reasoning text is intentionally not echoed to
/// stdout so the primary output stays the top-level agent's answer.
fn emit_subagent_plain(agent_id: &str, inner: &EventMsg) {
    let tag: String = agent_id.chars().take(6).collect();
    match inner {
        EventMsg::ToolCallBegin {
            name, args_preview, ..
        } => {
            eprintln!(
                "[agent {tag} · tool] {} {}",
                crate::sanitize_terminal_line(name),
                crate::sanitize_terminal(args_preview)
            );
        }
        EventMsg::ToolCallEnd { ok, summary, .. } => {
            eprintln!(
                "[agent {tag} · tool] {} — {}",
                if *ok { "ok" } else { "failed/denied" },
                crate::sanitize_terminal(summary)
            );
        }
        EventMsg::Committed { sha, message } => {
            let short = &sha[..sha.len().min(8)];
            eprintln!(
                "[agent {tag} · commit {short}] {}",
                crate::sanitize_terminal(message)
            );
        }
        EventMsg::Error { message, .. } => {
            eprintln!(
                "[agent {tag} · error] {}",
                crate::sanitize_terminal(message)
            );
        }
        _ => {}
    }
}

/// Print a text-mode ledger summary (unless `--json`) and optionally write an owner-private JSON
/// export. Returns whether the export was requested and failed.
fn finish_ledger(json: bool, path: Option<&std::path::Path>, ledger: &LedgerLog) -> bool {
    if !json {
        let summary = format_ledger_summary(ledger);
        eprintln!("{summary}");
    }
    let Some(path) = path else {
        return false;
    };
    if ledger.entries.len() < ledger.sources {
        let retained = ledger.entries.len();
        let sources = ledger.sources;
        eprintln!("ledger: retained {retained} of {sources} sources for export");
    }
    match write_private_ledger_file(path, &ledger.entries) {
        Ok(()) => false,
        Err(error) => {
            let destination = crate::sanitize_terminal_line(&path.display().to_string());
            let error = crate::sanitize_terminal(&error.to_string());
            eprintln!("could not write ledger to {destination}: {error}");
            true
        }
    }
}

#[derive(Debug, Default)]
struct LedgerLog {
    entries: Vec<LedgerEntry>,
    sources: usize,
    bytes: usize,
    redactions: usize,
}

impl LedgerLog {
    const COLLECT_LIMIT: usize = 4096;
    const SUMMARY_PREVIEW: usize = 20;

    fn record(&mut self, entry: &LedgerEntry) {
        self.sources = self.sources.saturating_add(1);
        self.bytes = self.bytes.saturating_add(entry.bytes);
        self.redactions = self.redactions.saturating_add(entry.redactions);
        if self.entries.len() < Self::COLLECT_LIMIT {
            self.entries.push(entry.clone());
        }
    }

    fn record_event(&mut self, ev: &EventMsg) {
        match ev {
            EventMsg::LedgerAppended(entry) => self.record(entry),
            EventMsg::SubagentUpdate { inner, .. } => self.record_event(inner),
            _ => {}
        }
    }
}

fn format_ledger_summary(ledger: &LedgerLog) -> String {
    use std::fmt::Write as _;

    let sources = ledger.sources;
    let bytes = ledger.bytes;
    let redactions = ledger.redactions;
    let mut out = format!("ledger: {sources} sources, {bytes} bytes, {redactions} redactions");
    if sources == 0 {
        return out;
    }
    for entry in ledger.entries.iter().take(LedgerLog::SUMMARY_PREVIEW) {
        let source = crate::sanitize_terminal_line(&entry.source);
        let reason = crate::sanitize_terminal_line(&entry.reason);
        let bytes = entry.bytes;
        let redactions = entry.redactions;
        let _ = write!(out, "\n  {source}  {bytes}  {reason}  {redactions}");
    }
    out
}

fn write_private_ledger_file(
    path: &std::path::Path,
    entries: &[LedgerEntry],
) -> std::io::Result<()> {
    let mut rendered = serde_json::to_string_pretty(entries).map_err(std::io::Error::other)?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    write_private_ledger_blocking(path, rendered.as_bytes())
}

fn write_private_ledger_blocking(path: &std::path::Path, rendered: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to overwrite a symlink",
        ));
    }

    let file = options.create_new(true).open(path)?;
    validate_private_ledger_target(&file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut file = file;
    file.write_all(rendered)?;
    file.sync_all()
}

fn validate_private_ledger_target(file: &std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ledger export destination is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to overwrite a file with multiple hard links",
            ));
        }
    }
    Ok(())
}

fn exit_code(stop: &StopReason, had_error: bool) -> ExitCode {
    match stop {
        StopReason::EndTurn if !had_error => ExitCode::SUCCESS,
        StopReason::Interrupted => ExitCode::from(130),
        _ => ExitCode::from(1),
    }
}

pub(crate) fn resolve_workspace(cd: Option<&std::path::Path>) -> Result<PathBuf, String> {
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
        assert!(parse_allow(&["mcp:".to_string()], &workspace).is_err());
        assert!(parse_allow(&["everything".to_string()], &workspace).is_err());
    }

    #[test]
    fn mcp_allow_names_one_exact_server_without_implying_network() {
        let workspace = std::env::temp_dir();
        let rules = parse_allow(&["mcp:docs".to_string()], &workspace).expect("MCP rule");
        assert_eq!(rules, vec![AllowRule::McpServer("docs".to_string())]);
        assert!(!grants_network(&rules));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn plan_skips_trusted_project_executable_capabilities() {
        let workspace = tempfile::tempdir().expect("workspace");
        let project = workspace.path().join(".grokforge");
        std::fs::create_dir(&project).expect("project config directory");
        let marker = workspace.path().join("mcp-started");
        let config = serde_json::json!({
            "servers": {
                "side-effect": {
                    "command": "/usr/bin/touch",
                    "args": [marker.to_string_lossy()]
                }
            }
        });
        std::fs::write(
            project.join("mcp.json"),
            serde_json::to_vec(&config).expect("MCP config"),
        )
        .expect("write MCP config");

        let (events, _events_rx) = mpsc::unbounded_channel();
        let mut registry = ToolRegistry::with_builtins();
        let before = registry.specs().len();
        let connected = register_project_capabilities(
            workspace.path(),
            true,
            true,
            true,
            &mut registry,
            &events,
        )
        .await;

        assert!(connected.is_empty());
        assert_eq!(registry.specs().len(), before);
        assert!(!marker.exists(), "plan mode started a trusted MCP process");
    }

    #[cfg(unix)]
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

    fn ledger_from(entries: &[LedgerEntry]) -> LedgerLog {
        let mut ledger = LedgerLog::default();
        for entry in entries {
            ledger.record(entry);
        }
        ledger
    }

    #[test]
    fn ledger_summary_is_compact_and_includes_detail_lines() {
        let ledger = ledger_from(&[
            LedgerEntry::new("src/main.rs", 1024, "tool read").with_redactions(2),
            LedgerEntry::new("system_prompt", 80, "configuration"),
        ]);
        assert_eq!(
            format_ledger_summary(&ledger),
            "ledger: 2 sources, 1104 bytes, 2 redactions\n  src/main.rs  1024  tool read  2\n  system_prompt  80  configuration  0"
        );
    }

    #[test]
    fn ledger_summary_zero_has_no_detail_lines() {
        assert_eq!(
            format_ledger_summary(&LedgerLog::default()),
            "ledger: 0 sources, 0 bytes, 0 redactions"
        );
    }

    #[test]
    fn ledger_summary_sanitizes_source_and_reason() {
        let ledger = ledger_from(&[LedgerEntry::new("src/\nmain.rs\u{1b}[31m", 8, "tool\tread")]);
        let text = format_ledger_summary(&ledger);
        assert_eq!(
            text,
            "ledger: 1 sources, 8 bytes, 0 redactions\n  src/ main.rs[31m  8  tool read  0"
        );
        assert!(!text.contains('\u{1b}'));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn ledger_summary_caps_preview_at_twenty_lines() {
        let entries: Vec<_> = (0..25)
            .map(|i| LedgerEntry::new(format!("src/{i}.rs"), i, "tool read"))
            .collect();
        let ledger = ledger_from(&entries);
        let text = format_ledger_summary(&ledger);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "ledger: 25 sources, 300 bytes, 0 redactions");
        assert_eq!(lines.len(), 21);
        assert!(lines[20].contains("src/19.rs"));
        assert!(!text.contains("src/20.rs"));
    }

    #[test]
    fn ledger_log_folds_subagent_entries_and_bounds_retention() {
        let mut ledger = LedgerLog::default();
        ledger.record_event(&EventMsg::LedgerAppended(LedgerEntry::new(
            "top", 10, "request",
        )));
        ledger.record_event(&EventMsg::SubagentUpdate {
            agent_id: "lane".into(),
            inner: Box::new(EventMsg::LedgerAppended(
                LedgerEntry::new("nested", 5, "request").with_redactions(1),
            )),
        });
        assert_eq!(ledger.sources, 2);
        assert_eq!(ledger.bytes, 15);
        assert_eq!(ledger.redactions, 1);
        assert_eq!(ledger.entries.len(), 2);

        for _ in 0..LedgerLog::COLLECT_LIMIT {
            ledger.record(&LedgerEntry::new("overflow", 1, "request"));
        }
        assert_eq!(
            ledger.entries.len(),
            LedgerLog::COLLECT_LIMIT,
            "retention is capped"
        );
        assert_eq!(
            ledger.sources,
            LedgerLog::COLLECT_LIMIT.saturating_add(2),
            "totals stay honest after the cap"
        );
        assert_eq!(ledger.bytes, LedgerLog::COLLECT_LIMIT.saturating_add(15));
    }

    #[test]
    fn private_ledger_write_creates_owner_private_json_and_refuses_existing() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("ledger.json");
        let entries = vec![LedgerEntry::new("src/lib.rs", 12, "tool read").with_redactions(1)];
        write_private_ledger_file(&path, &entries).expect("write");
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: Vec<LedgerEntry> = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed, entries);
        assert!(raw.starts_with('['));
        assert!(raw.ends_with("]\n"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
                0o600
            );
        }

        assert!(write_private_ledger_file(&path, &entries).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), raw);
    }

    #[cfg(unix)]
    #[test]
    fn private_ledger_write_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("target");
        std::fs::write(&target, "keep").expect("target");
        let link = dir.path().join("ledger.json");
        symlink(&target, &link).expect("symlink");
        let entries = vec![LedgerEntry::new("src/lib.rs", 1, "tool read")];
        assert!(write_private_ledger_file(&link, &entries).is_err());
        assert_eq!(std::fs::read_to_string(target).expect("kept"), "keep");
    }
}
