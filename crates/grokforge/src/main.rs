//! GrokForge command-line entry point.
//!
//! Default invocation launches the interactive TUI (M3). `exec` runs headless (M2).
//! The other subcommands are scaffolded here and implemented at their milestones.

mod acp;
mod credentials;
mod debug;
mod doctor;
mod headless;
mod serve;
mod sessions;
mod tui;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

/// Open-source terminal coding agent for Grok.
#[derive(Debug, Parser)]
#[command(name = "grokforge", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Headless: run a single prompt without the TUI (alias for `exec -p`).
    #[arg(short = 'p', long = "prompt", global = true)]
    prompt: Option<String>,

    /// Model slug for TUI, `exec`, `resume`, and ACP sessions.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Reasoning effort for TUI, `exec`, `resume`, and ACP sessions.
    #[arg(long, global = true, value_parser = ["auto", "low", "medium", "high", "xhigh"])]
    effort: Option<String>,

    /// Trust `.grokforge/mcp.json` to run local MCP commands or connect to remote MCP servers.
    #[arg(long, global = true)]
    trust_project_mcp: bool,

    /// Trust `.grokforge/config.toml` to choose billable model and runtime settings.
    #[arg(long, global = true)]
    trust_project_config: bool,

    /// Trust `.grokforge/tools.toml` to execute its hash-pinned custom tools.
    #[arg(long, global = true)]
    trust_project_tools: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a prompt headlessly and exit (for scripts and CI).
    Exec {
        /// The task for the agent to perform.
        #[arg(short = 'p', long)]
        prompt: Option<String>,
        /// Approval + sandbox preset.
        #[arg(long, default_value = "auto", value_parser = ["readonly", "auto", "strict", "yolo"])]
        preset: String,
        /// Emit NDJSON events instead of plain text.
        #[arg(long)]
        json: bool,
        /// Run in this directory instead of the current one.
        #[arg(long)]
        cd: Option<PathBuf>,
        /// Pre-grant a boundary: `network`, `write:<path>`, `cmd:<prefix>`, or `mcp:<server>`. Repeatable.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Plan mode: read-only tools + sandbox, produce a plan without changing anything.
        #[arg(long)]
        plan: bool,
        /// Enable xAI's separately metered web search server tool.
        #[arg(long)]
        web_search: bool,
        /// Enable xAI's separately metered live X search server tool.
        #[arg(long)]
        x_search: bool,
        /// Enable xAI's separately metered code interpreter server tool.
        #[arg(long)]
        code_interpreter: bool,
        /// Maximum tool-call iterations within the turn.
        #[arg(long, value_parser = bounded_iterations)]
        max_iterations: Option<u32>,
        /// Write an owner-private JSON ledger export. Refuses to overwrite an existing path.
        #[arg(long, value_name = "PATH")]
        ledger: Option<PathBuf>,
    },
    /// Run GrokForge as a bounded, authenticated local HTTP API.
    Serve {
        /// Loopback address to listen on. Put a TLS reverse proxy in front for remote access.
        #[arg(long, visible_alias = "listen", default_value = "127.0.0.1:4096", value_parser = loopback_server_address)]
        bind: std::net::SocketAddr,
        /// Bearer token. Prefer GROKFORGE_SERVER_TOKEN: command-line values can leak via history/process listings.
        #[arg(long, env = "GROKFORGE_SERVER_TOKEN", hide_env_values = true)]
        token: Option<serve::SecretToken>,
        /// Serve this directory instead of the current one.
        #[arg(long)]
        cd: Option<PathBuf>,
        /// Approval + sandbox preset. The persistent server deliberately has no yolo preset.
        #[arg(long, default_value = "auto", value_parser = ["readonly", "auto", "strict"])]
        preset: String,
        /// Pre-grant a boundary: `network`, `write:<path>`, `cmd:<prefix>`, or `mcp:<server>`. Repeatable.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Maximum simultaneous prompt streams.
        #[arg(long, default_value = "1", value_parser = bounded_server_concurrency)]
        max_concurrency: usize,
        /// Maximum lifetime of one prompt stream in seconds.
        #[arg(long, default_value = "1800", value_parser = bounded_server_timeout)]
        timeout_secs: u64,
    },
    /// Resume a previous session.
    Resume {
        /// Session id; omit for the most recent session in this project.
        id: Option<String>,
    },
    /// List, search, export, fork, rename, or delete saved sessions.
    Sessions {
        /// Search metadata and full transcripts instead of listing every session.
        #[arg(short, long, value_name = "TEXT")]
        query: Option<String>,
        #[command(subcommand)]
        action: Option<SessionsCommand>,
    },
    /// Store password-encrypted credentials: xAI API key/subscription, or remote MCP OAuth.
    Login {
        /// Sign in with your Grok subscription (OAuth) instead of pasting an API key.
        #[arg(long, conflicts_with = "mcp")]
        subscription: bool,
        /// Authorize a pre-registered OAuth client for a remote MCP server in this project.
        #[arg(long, value_name = "NAME", conflicts_with = "subscription")]
        mcp: Option<String>,
    },
    /// Report toolchain, sandbox capability, and configuration health.
    Doctor,
    /// Run as an ACP (Agent Client Protocol) agent over stdio, for editor embedding (Zed, etc.).
    /// Requires `XAI_API_KEY` in the environment (stdin is the protocol channel).
    Acp,
    /// Print the shell completion script.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Developer diagnostics (hidden).
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        cmd: DebugCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List saved sessions, optionally filtered by a full-text query.
    List {
        #[arg(short, long, value_name = "TEXT")]
        query: Option<String>,
    },
    /// Search metadata and physical transcripts, including pre-compaction turns.
    Search {
        /// One or more query terms; all terms must match.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
    },
    /// Export a session to Markdown or structured JSON (stdout by default).
    Export {
        /// Session id or unique prefix.
        id: String,
        #[arg(long, value_enum, default_value = "markdown")]
        format: sessions::ExportFormat,
        /// Write to this owner-private file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite an existing regular file (symlinks and hard links are refused).
        #[arg(long)]
        force: bool,
    },
    /// Fork a point-in-time copy into a new local session.
    Fork {
        /// Session id or unique prefix.
        id: String,
        /// Optional title for the fork.
        #[arg(long)]
        title: Option<String>,
    },
    /// Give a saved session a human-readable title.
    Rename {
        /// Session id or unique prefix.
        id: String,
        /// New title; unquoted words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        title: Vec<String>,
    },
    /// Permanently delete a saved session.
    Delete {
        /// Session id or unique prefix (at least eight characters).
        id: String,
        /// Skip typing the full session id; required outside an interactive terminal.
        #[arg(long)]
        force: bool,
    },
}

fn bounded_iterations(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if (1..=256).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("value must be between 1 and 256".to_string())
    }
}

fn bounded_server_concurrency(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if (1..=64).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("value must be between 1 and 64".to_string())
    }
}

fn bounded_server_timeout(value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if (1..=86_400).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("value must be between 1 and 86400 seconds".to_string())
    }
}

fn loopback_server_address(value: &str) -> Result<std::net::SocketAddr, String> {
    let address = value
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("`{value}` is not a valid socket address: {error}"))?;
    if address.ip().is_loopback() {
        Ok(address)
    } else {
        Err(
            "grokforge serve is loopback-only; use a TLS reverse proxy for remote access"
                .to_string(),
        )
    }
}

/// Remove terminal control characters from untrusted human-readable output. JSON mode keeps
/// the original data encoded by serde, so machine consumers lose no information.
pub(crate) fn sanitize_terminal(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            matches!(ch, '\n' | '\t')
                || (!ch.is_control()
                    && !matches!(*ch as u32, 0x7f..=0x9f)
                    && !matches!(
                        *ch as u32,
                        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
                    ))
        })
        .collect()
}

pub(crate) fn sanitize_terminal_line(value: &str) -> String {
    sanitize_terminal(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) async fn validate_model_startup(
    client: &grokforge_xai::XaiClient,
    model: &str,
) -> Result<(), std::process::ExitCode> {
    model_catalog_startup(client, model).await.map(|_| ())
}

/// Fetch and validate the startup model in one request. Frontends keep the returned catalog for
/// model switching and context-window lookup instead of hitting `/v1/models` two or three times.
pub(crate) async fn model_catalog_startup(
    client: &grokforge_xai::XaiClient,
    model: &str,
) -> Result<Vec<grokforge_xai::ModelInfo>, std::process::ExitCode> {
    if model.is_empty()
        || model.len() > 160
        || model.trim() != model
        || model.chars().any(char::is_whitespace)
        || model.chars().any(char::is_control)
    {
        eprintln!("invalid model slug; expected 1-160 non-whitespace characters");
        return Err(std::process::ExitCode::from(2));
    }
    eprintln!("[model validation: GET /v1/models; no project context sent]");
    match tokio::time::timeout(std::time::Duration::from_secs(10), client.list_models()).await {
        Ok(Ok(models))
            if models.iter().any(|candidate| {
                candidate.id == model || candidate.aliases.iter().any(|alias| alias == model)
            }) =>
        {
            Ok(models)
        }
        Ok(Ok(models)) => {
            let available = models
                .iter()
                .map(|candidate| candidate.id.as_str())
                .take(32)
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if models.len() > 32 { ", …" } else { "" };
            eprintln!(
                "model validation failed: model `{}` is not advertised; available: {}{}",
                sanitize_terminal_line(model),
                sanitize_terminal_line(&available),
                suffix
            );
            Err(std::process::ExitCode::from(3))
        }
        Ok(Err(error @ grokforge_xai::XaiError::Auth { .. })) => {
            // The credential itself was rejected — do not proceed to a full prompt request.
            eprintln!(
                "authentication failed — check your API key: {}",
                sanitize_terminal(&error.to_string())
            );
            Err(std::process::ExitCode::from(3))
        }
        Ok(Err(error @ grokforge_xai::XaiError::AccessDenied { .. })) => {
            // The key is valid, but the account/team can't run the request (no credits/license).
            // This is a billing/permissions problem, not an auth failure.
            if error.is_billing() {
                eprintln!(
                    "xAI denied the request — your account/team has no credits or license yet."
                );
                match error.console_url() {
                    Some(url) => eprintln!(
                        "Add credits or a license, then re-run:\n  {}",
                        sanitize_terminal(&url)
                    ),
                    None => eprintln!("Add credits/billing at https://console.x.ai, then re-run."),
                }
            } else {
                eprintln!(
                    "access denied (check model/endpoint permissions): {}",
                    sanitize_terminal(&error.to_string())
                );
            }
            Err(std::process::ExitCode::from(3))
        }
        Ok(Err(error)) => {
            eprintln!(
                "warning: model validation was unavailable; continuing: {}",
                sanitize_terminal(&error.to_string())
            );
            Ok(Vec::new())
        }
        Err(_) => {
            eprintln!("warning: model validation timed out after 10 seconds; continuing");
            Ok(Vec::new())
        }
    }
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Stream a one-shot prompt straight from the xAI API (live smoke test).
    ///
    /// Uses `XAI_API_KEY` and `XAI_BASE_URL` (default `https://api.x.ai`).
    Api {
        /// Prompt to send.
        prompt: String,
    },
    /// Run a command under the default workspace-write sandbox.
    ///
    /// The command must follow `--` so flags in the payload are not parsed by clap.
    Sandbox {
        /// Command line to run (`/bin/sh -c` on Unix, `cmd /C` on Windows).
        #[arg(last = true, required = true, num_args = 1.., value_name = "CMD")]
        command: Vec<String>,
    },
    /// Print a bounded local repository map (no network).
    Repomap {
        /// Maximum rendered map bytes. Clamped to repository-map limits.
        #[arg(long, value_name = "BYTES")]
        budget: Option<usize>,
        /// Optional free-text query used only for local ranking.
        #[arg(value_name = "QUERY")]
        query: Option<String>,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // Top-level CLI routing stays explicit so trust flags are visible at each frontend boundary.
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None if cli.prompt.is_some() => {
            headless::run(headless::ExecArgs {
                prompt: cli.prompt.unwrap_or_default(),
                preset: "auto".to_string(),
                model: cli.model,
                json: false,
                cd: None,
                allow: Vec::new(),
                effort: cli.effort,
                plan: false,
                web_search: false,
                x_search: false,
                code_interpreter: false,
                max_iterations: None,
                trust_project_mcp: cli.trust_project_mcp,
                trust_project_config: cli.trust_project_config,
                trust_project_tools: cli.trust_project_tools,
                ledger_path: None,
            })
            .await
        }
        None => {
            tui::launch(
                cli.trust_project_mcp,
                cli.trust_project_config,
                cli.trust_project_tools,
                cli.model,
                cli.effort,
            )
            .await
        }
        Some(Command::Exec {
            prompt,
            preset,
            json,
            cd,
            allow,
            plan,
            web_search,
            x_search,
            code_interpreter,
            max_iterations,
            ledger,
        }) => {
            let Some(prompt) = prompt.or(cli.prompt) else {
                eprintln!("provide a prompt with -p/--prompt");
                return std::process::ExitCode::from(2);
            };
            headless::run(headless::ExecArgs {
                prompt,
                preset,
                model: cli.model,
                json,
                cd,
                allow,
                effort: cli.effort,
                plan,
                web_search,
                x_search,
                code_interpreter,
                max_iterations,
                trust_project_mcp: cli.trust_project_mcp,
                trust_project_config: cli.trust_project_config,
                trust_project_tools: cli.trust_project_tools,
                ledger_path: ledger,
            })
            .await
        }
        Some(Command::Serve {
            bind,
            token,
            cd,
            preset,
            allow,
            max_concurrency,
            timeout_secs,
        }) => {
            serve::run(serve::ServeArgs {
                bind,
                token,
                cd,
                preset,
                allow,
                max_concurrency,
                timeout_secs,
                model: cli.model,
                effort: cli.effort,
                trust_project_mcp: cli.trust_project_mcp,
                trust_project_config: cli.trust_project_config,
                trust_project_tools: cli.trust_project_tools,
            })
            .await
        }
        Some(Command::Doctor) => doctor::run(cli.trust_project_config),
        Some(Command::Acp) => {
            acp::run(
                cli.trust_project_mcp,
                cli.trust_project_config,
                cli.trust_project_tools,
                cli.model,
                cli.effort,
            )
            .await
        }
        Some(Command::Resume { id }) => {
            sessions::resume(
                id,
                cli.trust_project_mcp,
                cli.trust_project_config,
                cli.trust_project_tools,
                cli.model,
                cli.effort,
            )
            .await
        }
        Some(Command::Sessions { query, action }) => run_sessions(query, action).await,
        Some(Command::Login { subscription, mcp }) => run_login(subscription, mcp).await,
        Some(Command::Completions { shell }) => print_completions(shell),
        Some(Command::Debug { cmd }) => match cmd {
            DebugCommand::Api { prompt } => {
                let model = cli.model.as_deref().unwrap_or("grok-build-0.1");
                debug::run_api(&prompt, model).await
            }
            DebugCommand::Sandbox { command } => debug::run_sandbox(command).await,
            DebugCommand::Repomap { budget, query } => debug::run_repomap(budget, query),
        },
    }
}

async fn run_login(subscription: bool, mcp: Option<String>) -> std::process::ExitCode {
    if let Some(name) = mcp {
        let workspace = match std::env::current_dir().and_then(std::fs::canonicalize) {
            Ok(workspace) => workspace,
            Err(error) => {
                eprintln!("cannot resolve project workspace: {error}");
                return std::process::ExitCode::from(2);
            }
        };
        credentials::login_mcp(&workspace, &name).await
    } else if subscription {
        credentials::login_subscription().await
    } else {
        credentials::login()
    }
}

async fn run_sessions(
    query: Option<String>,
    action: Option<SessionsCommand>,
) -> std::process::ExitCode {
    match action {
        None => sessions::list(query).await,
        Some(SessionsCommand::List { query: list_query }) => {
            sessions::list(list_query.or(query)).await
        }
        Some(SessionsCommand::Search { query }) => sessions::search(query.join(" ")).await,
        Some(SessionsCommand::Export {
            id,
            format,
            output,
            force,
        }) => sessions::export(id, format, output, force).await,
        Some(SessionsCommand::Fork { id, title }) => sessions::fork(id, title).await,
        Some(SessionsCommand::Rename { id, title }) => sessions::rename(id, title.join(" ")).await,
        Some(SessionsCommand::Delete { id, force }) => sessions::delete(id, force).await,
    }
}

fn print_completions(shell: Shell) -> std::process::ExitCode {
    let mut command = Cli::command();
    clap_complete::generate(shell, &mut command, "grokforge", &mut std::io::stdout());
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn global_and_exec_prompt_forms_parse() {
        assert!(Cli::try_parse_from(["grokforge", "-p", "task"]).is_ok());
        assert!(Cli::try_parse_from(["grokforge", "exec", "-p", "task"]).is_ok());
        assert!(Cli::try_parse_from(["grokforge", "-p", "global", "exec", "-p", "local"]).is_ok());
    }

    #[test]
    fn session_lifecycle_commands_parse() {
        assert!(Cli::try_parse_from(["grokforge", "sessions"]).is_ok());
        assert!(Cli::try_parse_from(["grokforge", "sessions", "--query", "needle"]).is_ok());
        assert!(Cli::try_parse_from(["grokforge", "sessions", "search", "alpha", "beta"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "grokforge",
                "sessions",
                "export",
                "abcd1234",
                "--format",
                "json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "grokforge",
                "sessions",
                "rename",
                "abcd1234",
                "release",
                "review"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "sessions", "delete", "abcd1234", "--force"]).is_ok()
        );
    }

    #[test]
    fn parser_rejects_out_of_range_iterations_and_invalid_effort() {
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--max-iterations", "0"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--max-iterations", "257"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--max-iterations", "256"])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--effort", "extreme"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--effort", "auto"]).is_ok()
        );
    }

    #[test]
    fn server_defaults_are_loopback_bounded_and_have_no_embedded_token() {
        let parsed = Cli::try_parse_from(["grokforge", "serve"]).expect("server defaults");
        assert!(matches!(
            parsed.command,
            Some(Command::Serve {
                bind,
                token: None,
                preset,
                max_concurrency: 1,
                timeout_secs: 1_800,
                ..
            }) if bind.ip().is_loopback() && preset == "auto"
        ));

        assert!(Cli::try_parse_from(["grokforge", "serve", "--preset", "yolo"]).is_err());
        assert!(Cli::try_parse_from(["grokforge", "serve", "--max-concurrency", "0"]).is_err());
        assert!(Cli::try_parse_from(["grokforge", "serve", "--max-concurrency", "65"]).is_err());
        assert!(Cli::try_parse_from(["grokforge", "serve", "--timeout-secs", "0"]).is_err());
    }

    #[test]
    fn server_accepts_explicit_bind_token_and_safety_grants() {
        let parsed = Cli::try_parse_from([
            "grokforge",
            "serve",
            "--listen",
            "127.0.0.1:4321",
            "--token",
            "63hNf7kPq4Ws8Ty2Za5Vc9Bm1Dx6Lu0R",
            "--preset",
            "strict",
            "--allow",
            "write:generated",
            "--max-concurrency",
            "4",
        ])
        .expect("explicit server options");
        assert!(matches!(
            parsed.command,
            Some(Command::Serve {
                bind,
                token: Some(_),
                preset,
                max_concurrency: 4,
                ..
            }) if bind == "127.0.0.1:4321".parse().expect("address") && preset == "strict"
        ));
    }

    #[test]
    fn shipped_server_rejects_every_non_loopback_bind() {
        for address in ["0.0.0.0:4096", "192.0.2.10:4096", "[::]:4096"] {
            assert!(
                Cli::try_parse_from(["grokforge", "serve", "--bind", address]).is_err(),
                "accepted non-loopback address {address}"
            );
        }
        assert!(Cli::try_parse_from(["grokforge", "serve", "--bind", "[::1]:4096"]).is_ok());
    }

    #[test]
    fn completions_accept_supported_shells_and_reject_unknown_ones() {
        for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
            assert!(Cli::try_parse_from(["grokforge", "completions", shell]).is_ok());
        }
        assert!(Cli::try_parse_from(["grokforge", "completions", "nushell"]).is_err());
    }

    #[test]
    fn headless_server_tool_flags_are_explicit_opt_ins() {
        let defaults = Cli::try_parse_from(["grokforge", "exec", "-p", "task"])
            .expect("default exec arguments");
        assert!(matches!(
            defaults.command,
            Some(Command::Exec {
                web_search: false,
                x_search: false,
                code_interpreter: false,
                ..
            })
        ));

        let enabled = Cli::try_parse_from([
            "grokforge",
            "exec",
            "-p",
            "task",
            "--web-search",
            "--x-search",
            "--code-interpreter",
        ])
        .expect("server-tool flags");
        assert!(matches!(
            enabled.command,
            Some(Command::Exec {
                web_search: true,
                x_search: true,
                code_interpreter: true,
                ..
            })
        ));
    }

    #[test]
    fn project_mcp_trust_is_an_explicit_opt_in_for_every_startup_form() {
        let interactive = Cli::try_parse_from(["grokforge"]).expect("interactive defaults");
        assert!(!interactive.trust_project_mcp);

        let exec = Cli::try_parse_from(["grokforge", "exec", "-p", "task"]).expect("exec defaults");
        assert!(!exec.trust_project_mcp);

        let resume = Cli::try_parse_from(["grokforge", "resume"]).expect("resume defaults");
        assert!(!resume.trust_project_mcp);

        let server = Cli::try_parse_from(["grokforge", "serve"]).expect("server defaults");
        assert!(!server.trust_project_mcp);

        for args in [
            vec!["grokforge", "--trust-project-mcp"],
            vec!["grokforge", "exec", "-p", "task", "--trust-project-mcp"],
            vec!["grokforge", "resume", "--trust-project-mcp"],
            vec!["grokforge", "serve", "--trust-project-mcp"],
        ] {
            let parsed = Cli::try_parse_from(args).expect("trusted startup form");
            assert!(parsed.trust_project_mcp);
        }
    }

    #[test]
    fn project_config_trust_is_an_explicit_opt_in_for_runtime_startup_forms() {
        let interactive = Cli::try_parse_from(["grokforge"]).expect("interactive defaults");
        assert!(!interactive.trust_project_config);

        let exec = Cli::try_parse_from(["grokforge", "exec", "-p", "task"]).expect("exec defaults");
        assert!(!exec.trust_project_config);

        let resume = Cli::try_parse_from(["grokforge", "resume"]).expect("resume defaults");
        assert!(!resume.trust_project_config);

        let doctor = Cli::try_parse_from(["grokforge", "doctor"]).expect("doctor defaults");
        assert!(!doctor.trust_project_config);

        let server = Cli::try_parse_from(["grokforge", "serve"]).expect("server defaults");
        assert!(!server.trust_project_config);

        for args in [
            vec!["grokforge", "--trust-project-config"],
            vec!["grokforge", "exec", "-p", "task", "--trust-project-config"],
            vec!["grokforge", "resume", "--trust-project-config"],
            vec!["grokforge", "doctor", "--trust-project-config"],
            vec!["grokforge", "serve", "--trust-project-config"],
        ] {
            let parsed = Cli::try_parse_from(args).expect("trusted startup form");
            assert!(parsed.trust_project_config);
        }
    }

    #[test]
    fn project_tools_trust_is_an_explicit_opt_in_for_every_agent_frontend() {
        let interactive = Cli::try_parse_from(["grokforge"]).expect("interactive defaults");
        assert!(!interactive.trust_project_tools);

        let exec = Cli::try_parse_from(["grokforge", "exec", "-p", "task"]).expect("exec defaults");
        assert!(!exec.trust_project_tools);

        let resume = Cli::try_parse_from(["grokforge", "resume"]).expect("resume defaults");
        assert!(!resume.trust_project_tools);

        let acp = Cli::try_parse_from(["grokforge", "acp"]).expect("acp defaults");
        assert!(!acp.trust_project_tools);

        let server = Cli::try_parse_from(["grokforge", "serve"]).expect("server defaults");
        assert!(!server.trust_project_tools);

        for args in [
            vec!["grokforge", "--trust-project-tools"],
            vec!["grokforge", "exec", "-p", "task", "--trust-project-tools"],
            vec!["grokforge", "resume", "--trust-project-tools"],
            vec!["grokforge", "acp", "--trust-project-tools"],
            vec!["grokforge", "serve", "--trust-project-tools"],
        ] {
            let parsed = Cli::try_parse_from(args).expect("trusted startup form");
            assert!(parsed.trust_project_tools);
        }
    }

    #[test]
    fn resume_accepts_global_model_and_effort_overrides() {
        let parsed = Cli::try_parse_from([
            "grokforge",
            "resume",
            "session-prefix",
            "--model",
            "grok-4.5",
            "--effort",
            "high",
        ])
        .expect("resume overrides");
        assert_eq!(parsed.model.as_deref(), Some("grok-4.5"));
        assert_eq!(parsed.effort.as_deref(), Some("high"));
    }

    #[test]
    fn terminal_sanitizer_blocks_escape_and_c1_sequences() {
        let sanitized = sanitize_terminal("safe\u{1b}]52;c;payload\u{7} text\u{009d}bad\u{202e}");
        assert_eq!(sanitized, "safe]52;c;payload textbad");
        assert_eq!(sanitize_terminal_line("one\n two\tthree"), "one two three");
    }

    #[tokio::test]
    async fn startup_model_validation_accepts_advertised_and_rejects_unknown_slugs() {
        let server = grokforge_test_support::MockXai::builder()
            .route(
                "/v1/models",
                grokforge_test_support::Reply::json(
                    200,
                    &serde_json::json!({
                        "data": [{"id": "grok-build-0.1", "aliases": ["grok-build-latest"]}]
                    }),
                ),
            )
            .start()
            .await;
        let client =
            grokforge_xai::XaiClient::new(&server.base_url(), "test-key").expect("test client");
        assert!(
            validate_model_startup(&client, "grok-build-latest")
                .await
                .is_ok()
        );
        assert_eq!(
            validate_model_startup(&client, "retired-model")
                .await
                .expect_err("unknown model"),
            std::process::ExitCode::from(3)
        );

        let unauthorized = grokforge_test_support::MockXai::builder()
            .route(
                "/v1/models",
                grokforge_test_support::Reply::json(
                    401,
                    &serde_json::json!({"error": {"message": "invalid key"}}),
                ),
            )
            .start()
            .await;
        let client = grokforge_xai::XaiClient::new(&unauthorized.base_url(), "bad-key")
            .expect("test client");
        assert_eq!(
            validate_model_startup(&client, "grok-build-0.1")
                .await
                .expect_err("authentication failure"),
            std::process::ExitCode::from(3)
        );
    }
}
