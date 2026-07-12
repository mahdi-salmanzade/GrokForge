//! Headless mode: `grokforge exec -p "..."`. Runs one turn without the TUI, streaming events
//! to stdout (plain text or `--json` NDJSON) and returning a CI-friendly exit code. Approvals
//! are resolved non-interactively — auto-denied with feedback unless `--allow`/`--yolo`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use grokforge_core::{
    Agent, AllowRule, AutoApprover, RolloutWriter, Session, SessionConfig, SessionMeta,
    ToolRegistry, sessions_dir,
};
use grokforge_protocol::{ApprovalPolicy, EventMsg, SandboxMode, StopReason};
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

fn parse_allow(specs: &[String]) -> Vec<AllowRule> {
    specs
        .iter()
        .filter_map(|s| {
            if s == "network" {
                Some(AllowRule::Network)
            } else if let Some(p) = s.strip_prefix("write:") {
                Some(AllowRule::Write(PathBuf::from(p)))
            } else if let Some(p) = s.strip_prefix("cmd:") {
                Some(AllowRule::CmdPrefix(p.to_string()))
            } else {
                eprintln!("warning: ignoring unrecognized --allow `{s}`");
                None
            }
        })
        .collect()
}

fn parse_effort(s: &str) -> Option<Effort> {
    match s {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        _ => None,
    }
}

pub async fn run(args: ExecArgs) -> ExitCode {
    let Ok(api_key) = std::env::var("XAI_API_KEY") else {
        eprintln!("XAI_API_KEY is not set (run `grokforge login` once it lands, or export it)");
        return ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let Some((policy, mode)) = preset_policy(&args.preset) else {
        eprintln!(
            "unknown --preset `{}` (readonly|auto|strict|yolo)",
            args.preset
        );
        return ExitCode::from(2);
    };

    let workspace = match args.cd {
        Some(dir) => dir,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {e}");
            return ExitCode::from(2);
        }
    };

    let model = args.model.unwrap_or_else(|| "grok-build-0.1".to_string());
    let approver: Arc<AutoApprover> = if args.preset == "yolo" {
        Arc::new(AutoApprover::yolo())
    } else {
        Arc::new(AutoApprover::new(parse_allow(&args.allow)))
    };

    // Build the tool registry and register any MCP servers declared in .grokforge/mcp.json.
    let mut registry = ToolRegistry::with_builtins();
    let connected =
        grokforge_core::mcp_config::connect_and_register(&workspace, &mut registry).await;
    if !connected.is_empty() {
        eprintln!("mcp: connected {}", connected.join(", "));
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = Agent::new(client, registry, default_runner(), approver, tx);

    let mut config = SessionConfig::new(workspace, model).with_policy(policy, mode);
    config.max_iterations = args.max_iterations;
    config.effort = args.effort.as_deref().and_then(parse_effort);
    let mut session = Session::new(config);

    // Persist this run so it is listable/resumable via `grokforge sessions`/`resume`.
    let dir = sessions_dir();
    let rollout = RolloutWriter::create(&dir, session.id).await.ok();
    let meta = SessionMeta::new(
        session.id,
        session.config.workspace_root.clone(),
        session.config.model.clone(),
        &args.prompt,
    );
    let _ = meta.write(&dir, session.id).await;

    let prompt = args.prompt;
    let json = args.json;
    let plan = args.plan;
    let handle = tokio::spawn(async move {
        let mut rollout = rollout;
        if plan {
            agent
                .run_plan_turn(&mut session, &prompt, &mut rollout)
                .await
        } else {
            agent.run_turn(&mut session, &prompt, &mut rollout).await
        }
    });

    let mut had_error = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, EventMsg::Error { .. }) {
            had_error = true;
        }
        emit(&ev, json);
    }

    let stop = handle.await.unwrap_or(StopReason::Error);
    exit_code(&stop, had_error)
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
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
        EventMsg::ToolCallBegin {
            name, args_preview, ..
        } => {
            eprintln!("[tool] {name} {args_preview}");
        }
        EventMsg::ToolCallEnd { ok, summary, .. } => {
            eprintln!(
                "[tool] {} — {summary}",
                if *ok { "ok" } else { "failed/denied" }
            );
        }
        EventMsg::Committed { sha, message } => {
            let short = &sha[..sha.len().min(8)];
            eprintln!("[commit {short}] {message}");
        }
        EventMsg::TurnComplete { stop, .. } => {
            println!();
            eprintln!("[done: {stop:?}]");
        }
        EventMsg::Error { message, .. } => eprintln!("[error] {message}"),
        _ => {}
    }
}

fn exit_code(stop: &StopReason, had_error: bool) -> ExitCode {
    match stop {
        StopReason::EndTurn | StopReason::MaxIterations if !had_error => ExitCode::SUCCESS,
        StopReason::Interrupted => ExitCode::from(130),
        _ => ExitCode::from(1),
    }
}
