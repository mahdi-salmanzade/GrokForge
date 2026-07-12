//! GrokForge command-line entry point.
//!
//! Default invocation launches the interactive TUI (M3). `exec` runs headless (M2).
//! The other subcommands are scaffolded here and implemented at their milestones.

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
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None if cli.prompt.is_some() => {
            eprintln!("headless exec lands in M2 (see docs/design/03-roadmap.md)");
            std::process::ExitCode::from(2)
        }
        None => {
            eprintln!("the interactive TUI lands in M3 (see docs/design/03-roadmap.md)");
            std::process::ExitCode::from(2)
        }
        Some(Command::Exec { .. }) => {
            eprintln!("headless exec lands in M2");
            std::process::ExitCode::from(2)
        }
        Some(Command::Doctor) => {
            println!("grokforge {}", env!("CARGO_PKG_VERSION"));
            println!("toolchain: {}", env!("CARGO_PKG_RUST_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        Some(Command::Resume { .. } | Command::Sessions | Command::Login) => {
            eprintln!("session management lands in M8");
            std::process::ExitCode::from(2)
        }
        Some(Command::Completions { .. }) => {
            eprintln!("completions land with the release milestone (M11)");
            std::process::ExitCode::from(2)
        }
    }
}
