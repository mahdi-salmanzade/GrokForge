//! GrokForge command-line entry point.
//!
//! Default invocation launches the interactive TUI (M3). `exec` runs headless (M2).
//! The other subcommands are scaffolded here and implemented at their milestones.

mod debug;
mod headless;
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
        #[arg(long)]
        effort: Option<String>,
        /// Maximum tool-call iterations within the turn.
        #[arg(long, default_value_t = 32)]
        max_iterations: u32,
    },
    /// Resume a previous session.
    Resume {
        /// Session id; omit for the most recent session in this project.
        id: Option<String>,
    },
    /// List and search past sessions.
    Sessions,
    /// Store or replace the xAI API key in the OS keyring.
    Login,
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
                max_iterations,
            })
            .await
        }
        Some(Command::Doctor) => {
            println!("grokforge {}", env!("CARGO_PKG_VERSION"));
            println!("minimum toolchain: {}", env!("CARGO_PKG_RUST_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        Some(Command::Resume { .. } | Command::Sessions | Command::Login) => {
            milestone("session management", "M8")
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
