//! GrokForge command-line entry point.
//!
//! Default invocation launches the interactive TUI (M3). `exec` runs headless (M2).
//! The other subcommands are scaffolded here and implemented at their milestones.

mod credentials;
mod debug;
mod doctor;
mod headless;
mod sessions;
mod tui;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Open-source terminal coding agent for Grok.
#[derive(Debug, Parser)]
#[command(name = "grokforge", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Headless: run a single prompt without the TUI (alias for `exec -p`).
    #[arg(short = 'p', long = "prompt", global = true)]
    prompt: Option<String>,
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
        /// Model slug (defaults to grok-build-0.1).
        #[arg(long)]
        model: Option<String>,
        /// Emit NDJSON events instead of plain text.
        #[arg(long)]
        json: bool,
        /// Run in this directory instead of the current one.
        #[arg(long)]
        cd: Option<PathBuf>,
        /// Pre-grant a boundary: `network`, `write:<path>`, or `cmd:<prefix>`. Repeatable.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Reasoning effort: low, medium, or high.
        #[arg(long, value_parser = ["low", "medium", "high"])]
        effort: Option<String>,
        /// Plan mode: read-only tools + sandbox, produce a plan without changing anything.
        #[arg(long)]
        plan: bool,
        /// Maximum tool-call iterations within the turn.
        #[arg(long, default_value_t = 32, value_parser = positive_u32)]
        max_iterations: u32,
    },
    /// Resume a previous session.
    Resume {
        /// Session id; omit for the most recent session in this project.
        id: Option<String>,
    },
    /// List and search past sessions.
    Sessions,
    /// Store credentials in the OS keychain: an API key (default), or sign in with your
    /// SuperGrok / X Premium+ subscription via `--subscription`.
    Login {
        /// Sign in with your Grok subscription (OAuth) instead of pasting an API key.
        #[arg(long)]
        subscription: bool,
    },
    /// Report toolchain, sandbox capability, and configuration health.
    Doctor,
    /// Print the shell completion script.
    Completions {
        /// Target shell (bash, zsh, fish, powershell).
        shell: String,
    },
    /// Developer diagnostics (hidden).
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        cmd: DebugCommand,
    },
}

fn positive_u32(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if parsed == 0 {
        Err("value must be at least 1".to_string())
    } else {
        Ok(parsed)
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
    eprintln!("[model validation: GET /v1/models; no project context sent]");
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.validate_model(model),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error @ grokforge_xai::XaiError::UnknownModel { .. })) => {
            eprintln!(
                "model validation failed: {}",
                sanitize_terminal(&error.to_string())
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
            Ok(())
        }
        Err(_) => {
            eprintln!("warning: model validation timed out after 10 seconds; continuing");
            Ok(())
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
        /// Model slug.
        #[arg(long, default_value = "grok-build-0.1")]
        model: String,
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
                model: None,
                json: false,
                cd: None,
                allow: Vec::new(),
                effort: None,
                plan: false,
                max_iterations: 32,
            })
            .await
        }
        None => tui::launch().await,
        Some(Command::Exec {
            prompt,
            preset,
            model,
            json,
            cd,
            allow,
            effort,
            plan,
            max_iterations,
        }) => {
            let Some(prompt) = prompt.or(cli.prompt) else {
                eprintln!("provide a prompt with -p/--prompt");
                return std::process::ExitCode::from(2);
            };
            headless::run(headless::ExecArgs {
                prompt,
                preset,
                model,
                json,
                cd,
                allow,
                effort,
                plan,
                max_iterations,
            })
            .await
        }
        Some(Command::Doctor) => doctor::run(),
        Some(Command::Resume { id }) => sessions::resume(id).await,
        Some(Command::Sessions) => sessions::list().await,
        Some(Command::Login { subscription }) => {
            if subscription {
                credentials::login_subscription().await
            } else {
                credentials::login()
            }
        }
        Some(Command::Completions { .. }) => milestone("completions", "M11"),
        Some(Command::Debug {
            cmd: DebugCommand::Api { prompt, model },
        }) => debug::run_api(&prompt, &model).await,
    }
}

fn milestone(feature: &str, ms: &str) -> std::process::ExitCode {
    eprintln!("{feature} lands in {ms} (see docs/design/03-roadmap.md)");
    std::process::ExitCode::from(2)
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
    fn parser_rejects_zero_iterations_and_invalid_effort() {
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--max-iterations", "0"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["grokforge", "exec", "-p", "task", "--effort", "extreme"])
                .is_err()
        );
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
