//! GrokForge command-line entry point.
//!
//! Default invocation launches the interactive TUI (M3). `exec` runs headless (M2).
//! The other subcommands are scaffolded here and implemented at their milestones.

mod acp;
mod credentials;
mod debug;
mod doctor;
mod headless;
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

    /// Trust `.grokforge/mcp.json` to execute local MCP server commands.
    #[arg(long, global = true)]
    trust_project_mcp: bool,

    /// Trust `.grokforge/config.toml` to choose billable model and runtime settings.
    #[arg(long, global = true)]
    trust_project_config: bool,
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
        /// Pre-grant a boundary: `network`, `write:<path>`, or `cmd:<prefix>`. Repeatable.
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
    },
    /// Resume a previous session.
    Resume {
        /// Session id; omit for the most recent session in this project.
        id: Option<String>,
    },
    /// List and search past sessions.
    Sessions,
    /// Store credentials in the password-encrypted file: an API key (default), or sign in with
    /// your SuperGrok / X Premium+ subscription via `--subscription`.
    Login {
        /// Sign in with your Grok subscription (OAuth) instead of pasting an API key.
        #[arg(long)]
        subscription: bool,
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
}

#[tokio::main]
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
            })
            .await
        }
        None => {
            tui::launch(
                cli.trust_project_mcp,
                cli.trust_project_config,
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
            })
            .await
        }
        Some(Command::Doctor) => doctor::run(cli.trust_project_config),
        Some(Command::Acp) => {
            acp::run(
                cli.trust_project_mcp,
                cli.trust_project_config,
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
                cli.model,
                cli.effort,
            )
            .await
        }
        Some(Command::Sessions) => sessions::list().await,
        Some(Command::Login { subscription }) => {
            if subscription {
                credentials::login_subscription().await
            } else {
                credentials::login()
            }
        }
        Some(Command::Completions { shell }) => print_completions(shell),
        Some(Command::Debug {
            cmd: DebugCommand::Api { prompt },
        }) => {
            let model = cli.model.as_deref().unwrap_or("grok-build-0.1");
            debug::run_api(&prompt, model).await
        }
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

        for args in [
            vec!["grokforge", "--trust-project-mcp"],
            vec!["grokforge", "exec", "-p", "task", "--trust-project-mcp"],
            vec!["grokforge", "resume", "--trust-project-mcp"],
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

        for args in [
            vec!["grokforge", "--trust-project-config"],
            vec!["grokforge", "exec", "-p", "task", "--trust-project-config"],
            vec!["grokforge", "resume", "--trust-project-config"],
            vec!["grokforge", "doctor", "--trust-project-config"],
        ] {
            let parsed = Cli::try_parse_from(args).expect("trusted startup form");
            assert!(parsed.trust_project_config);
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
