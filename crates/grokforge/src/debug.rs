//! Hidden developer diagnostics.
//!
//! - `debug api` streams a single prompt directly from the xAI API — a manual, credentialed
//!   live smoke test for checking provider API drift.
//! - `debug sandbox -- <cmd>` runs a command under the default workspace-write sandbox.
//! - `debug repomap` prints a bounded local repository map (no network).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use futures::StreamExt;
use grokforge_context::{RepoMap, RepoMapLimits, RepoMapOptions, RepoMapStats, build_repo_map};
use grokforge_core::{Redactor, Session, SessionConfig};
use grokforge_protocol::{DenialClass, ResponseItem, SandboxPolicy};
use grokforge_sandbox::{CommandSpec, ExecOutput, SandboxCapability, default_runner};
use grokforge_xai::{StreamEvent, XaiClient};

/// Roadmap default for `debug repomap --budget`. `RepoMapLimits::bounded` still applies.
const DEFAULT_REPOMAP_BUDGET: usize = 2_000;
/// Matches `RepoMapLimits::bounded` for `max_output_bytes`.
const REPOMAP_MIN_OUTPUT_BYTES: usize = 256;
/// Matches `grokforge_context` `HARD_MAX_OUTPUT_BYTES` (not re-exported from that crate).
const REPOMAP_HARD_MAX_OUTPUT_BYTES: usize = 128 * 1024;

pub async fn run_api(prompt: &str, model: &str) -> ExitCode {
    let Some(api_key) = crate::credentials::resolve(false).await else {
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
    if let Err(code) = crate::validate_model_startup(&client, model).await {
        return code;
    }

    // Even this developer smoke path uses the same context assembler and reconciled request
    // ledger as normal turns; no raw network request may bypass the privacy choke point.
    let workspace = match std::env::current_dir() {
        Ok(workspace) if workspace.is_absolute() => workspace,
        Ok(_) => {
            eprintln!("debug API request refused: current workspace is not absolute");
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!(
                "debug API request refused: could not resolve current workspace: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let mut session = Session::new(SessionConfig::new(workspace, model));
    let prompt = Redactor::apply(prompt);
    session
        .history
        .push(ResponseItem::user_redacted(prompt.text, prompt.count));
    let assembled = match grokforge_core::context::assemble(&session, &[], &[], &[], Vec::new()) {
        Ok(assembled) => assembled,
        Err(error) => {
            eprintln!(
                "request assembly error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(4);
        }
    };
    for entry in &assembled.ledger.entries {
        eprintln!(
            "[ledger: {} bytes — {}]",
            entry.bytes,
            crate::sanitize_terminal_line(&entry.source)
        );
    }
    let stream = match client
        .stream_with_attempt_observer(&assembled.request, |attempt| {
            if let Some(line) = retry_ledger_line(attempt.number, attempt.request_bytes) {
                eprintln!("{line}");
            }
        })
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "request error: {}",
                crate::sanitize_terminal(&e.to_string())
            );
            return exit_for_error(&e);
        }
    };

    consume_stream(stream).await
}

pub async fn run_sandbox(command: Vec<String>) -> ExitCode {
    let command_line = join_command_line(&command);
    if command.is_empty() || command_line.is_empty() {
        eprintln!("debug sandbox refused: command is empty");
        return ExitCode::from(sandbox_exit_status(&SandboxExit::SetupFailure));
    }

    let runner = default_runner();
    let capability = runner.capability();
    print_capability(&capability);

    let cwd = match absolute_cwd() {
        Ok(cwd) => cwd,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(sandbox_exit_status(&SandboxExit::SetupFailure));
        }
    };

    let policy = SandboxPolicy::workspace_write(&cwd);
    print_policy(capability.enforced);
    println!("command: {}", crate::sanitize_terminal_line(&command_line));

    let spec = CommandSpec::shell(&command_line, cwd);
    match runner.run(&policy, &spec).await {
        Ok(output) => {
            print_exec_output(&output);
            ExitCode::from(sandbox_exit_status(&SandboxExit::from_output(&output)))
        }
        Err(error) => {
            eprintln!(
                "sandbox runner refused: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            ExitCode::from(sandbox_exit_status(&SandboxExit::SetupFailure))
        }
    }
}

pub fn run_repomap(budget: Option<usize>, query: Option<String>) -> ExitCode {
    let budget = clamp_repomap_budget(budget);
    let query = query.filter(|value| !value.is_empty());
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "debug repomap refused: could not resolve current workspace: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(2);
        }
    };
    let options = RepoMapOptions {
        limits: RepoMapLimits {
            max_output_bytes: budget,
            ..RepoMapLimits::default()
        },
        query,
        ..RepoMapOptions::default()
    };
    match build_repo_map(&root, &options, || false) {
        Ok(map) => {
            print_repo_map(&map);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "debug repomap error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            ExitCode::from(2)
        }
    }
}

async fn consume_stream(mut stream: grokforge_xai::ResponseStream) -> ExitCode {
    let mut stdout = std::io::stdout();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(t)) => {
                print!("{}", crate::sanitize_terminal(&t));
                let _ = stdout.flush();
            }
            Ok(StreamEvent::ToolCall(c)) => {
                eprintln!(
                    "\n[tool call: {} {}]",
                    crate::sanitize_terminal_line(&c.name),
                    crate::sanitize_terminal(&c.arguments)
                );
            }
            Ok(StreamEvent::Usage(u)) => {
                eprintln!(
                    "\n[usage: in={} cached={} out={} reasoning={}]",
                    u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens
                );
            }
            Ok(StreamEvent::Completed { .. }) => {
                println!();
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "\nstream error: {}",
                    crate::sanitize_terminal(&e.to_string())
                );
                return exit_for_error(&e);
            }
        }
    }
    println!();
    eprintln!("stream error: response ended before a completed event");
    ExitCode::from(4)
}

fn retry_ledger_line(number: u32, request_bytes: usize) -> Option<String> {
    (number > 1).then(|| format!("[ledger: {request_bytes} bytes — request_retry_{number}]"))
}

fn exit_for_error(e: &grokforge_xai::XaiError) -> ExitCode {
    use grokforge_xai::XaiError;
    let code = match e {
        XaiError::Auth { .. } | XaiError::UnknownModel { .. } => 3,
        _ => 4,
    };
    ExitCode::from(code)
}

fn join_command_line(parts: &[String]) -> String {
    parts.join(" ")
}

fn clamp_repomap_budget(budget: Option<usize>) -> usize {
    let default_max = RepoMapLimits::default().max_output_bytes;
    let ceiling = default_max.max(REPOMAP_HARD_MAX_OUTPUT_BYTES);
    budget
        .unwrap_or(DEFAULT_REPOMAP_BUDGET)
        .clamp(REPOMAP_MIN_OUTPUT_BYTES, ceiling)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SandboxExit {
    SetupFailure,
    Timeout,
    Denial,
    Child(i32),
    Unknown,
}

impl SandboxExit {
    fn from_output(output: &ExecOutput) -> Self {
        if output.timed_out {
            Self::Timeout
        } else if output.denial.is_some() {
            Self::Denial
        } else if let Some(code) = output.exit_code {
            Self::Child(code)
        } else {
            Self::Unknown
        }
    }
}

fn sandbox_exit_status(result: &SandboxExit) -> u8 {
    match result {
        SandboxExit::SetupFailure => 2,
        SandboxExit::Timeout => 124,
        SandboxExit::Denial | SandboxExit::Unknown => 1,
        SandboxExit::Child(code) => u8::try_from(*code).unwrap_or(1),
    }
}

fn absolute_cwd() -> Result<PathBuf, String> {
    match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(cwd) if cwd.is_absolute() => Ok(cwd),
        Ok(_) => Err("debug sandbox refused: current workspace is not absolute".to_string()),
        Err(error) => Err(format!(
            "debug sandbox refused: could not resolve current workspace: {}",
            crate::sanitize_terminal(&error.to_string())
        )),
    }
}

fn print_capability(capability: &SandboxCapability) {
    println!(
        "sandbox backend: {}",
        crate::sanitize_terminal_line(&capability.backend)
    );
    if capability.enforced {
        println!("enforced: yes");
    } else {
        println!("enforced: no");
    }
    for note in &capability.notes {
        println!("note: {}", crate::sanitize_terminal_line(note));
    }
}

fn print_policy(enforced: bool) {
    // Honest wording: this is the policy we request. Do not claim OS enforcement unless the
    // selected backend actually confines the process.
    if enforced {
        println!("policy: workspace-write, network isolated, .git protected");
    } else {
        println!(
            "policy: workspace-write, network isolated, .git protected (requested; not enforced by this backend)"
        );
    }
}

fn print_exec_output(output: &ExecOutput) {
    match output.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: none"),
    }
    println!("timeout: {}", if output.timed_out { "yes" } else { "no" });
    println!("truncated: {}", if output.truncated { "yes" } else { "no" });
    if let Some(denial) = output.denial {
        println!("denial: {}", denial_label(denial));
    }
    println!("--- stdout ---");
    print_captured(&output.stdout);
    println!("--- stderr ---");
    print_captured(&output.stderr);
}

fn print_captured(text: &str) {
    let sanitized = crate::sanitize_terminal(text);
    if sanitized.is_empty() {
        return;
    }
    if sanitized.ends_with('\n') {
        print!("{sanitized}");
    } else {
        println!("{sanitized}");
    }
}

fn denial_label(denial: DenialClass) -> &'static str {
    match denial {
        DenialClass::FsWrite => "fs_write",
        DenialClass::FsRead => "fs_read",
        DenialClass::Network => "network",
        DenialClass::Signal => "signal",
    }
}

fn print_repo_map(map: &RepoMap) {
    let text = crate::sanitize_terminal(&map.text);
    if !text.is_empty() {
        if text.ends_with('\n') {
            print!("{text}");
        } else {
            println!("{text}");
        }
    }
    println!(
        "# files={} symbols={} bytes_read={} truncated={}",
        map.stats.mapped_files,
        map.stats.mapped_symbols,
        map.stats.bytes_read,
        truncation_flags(&map.stats)
    );
}

fn truncation_flags(stats: &RepoMapStats) -> String {
    let mut flags = Vec::new();
    if stats.walk_truncated {
        flags.push("walk");
    }
    if stats.files_truncated {
        flags.push("files");
    }
    if stats.read_truncated {
        flags.push("read");
    }
    if stats.output_truncated {
        flags.push("output");
    }
    if flags.is_empty() {
        "none".to_string()
    } else {
        flags.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_attempts_receive_explicit_ledger_lines() {
        assert_eq!(retry_ledger_line(1, 42), None);
        assert_eq!(
            retry_ledger_line(2, 42).as_deref(),
            Some("[ledger: 42 bytes — request_retry_2]")
        );
    }

    #[test]
    fn repomap_budget_is_clamped_to_hard_limits() {
        assert_eq!(clamp_repomap_budget(None), DEFAULT_REPOMAP_BUDGET);
        assert_eq!(clamp_repomap_budget(Some(0)), REPOMAP_MIN_OUTPUT_BYTES);
        assert_eq!(clamp_repomap_budget(Some(1)), REPOMAP_MIN_OUTPUT_BYTES);
        assert_eq!(clamp_repomap_budget(Some(2_000)), 2_000);
        assert_eq!(
            clamp_repomap_budget(Some(RepoMapLimits::default().max_output_bytes)),
            RepoMapLimits::default().max_output_bytes
        );
        assert_eq!(
            clamp_repomap_budget(Some(usize::MAX)),
            REPOMAP_HARD_MAX_OUTPUT_BYTES
        );
    }

    #[test]
    fn sandbox_command_line_joins_remaining_args() {
        assert_eq!(join_command_line(&[]), "");
        assert_eq!(join_command_line(&["true".to_string()]), "true");
        assert_eq!(
            join_command_line(&["echo".to_string(), "hello".to_string(), "world".to_string()]),
            "echo hello world"
        );
        assert_eq!(
            join_command_line(&["printf".to_string(), "%s".to_string()]),
            "printf %s"
        );
    }

    #[test]
    fn sandbox_exit_codes_map_timeout_denial_and_setup() {
        assert_eq!(sandbox_exit_status(&SandboxExit::SetupFailure), 2);
        assert_eq!(sandbox_exit_status(&SandboxExit::Timeout), 124);
        assert_eq!(sandbox_exit_status(&SandboxExit::Denial), 1);
        assert_eq!(sandbox_exit_status(&SandboxExit::Child(0)), 0);
        assert_eq!(sandbox_exit_status(&SandboxExit::Child(7)), 7);
        assert_eq!(sandbox_exit_status(&SandboxExit::Child(-1)), 1);
        assert_eq!(sandbox_exit_status(&SandboxExit::Unknown), 1);
        assert_eq!(
            SandboxExit::from_output(&ExecOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: true,
                denial: Some(DenialClass::Network),
            }),
            SandboxExit::Timeout
        );
        assert_eq!(
            SandboxExit::from_output(&ExecOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: Some(DenialClass::FsWrite),
            }),
            SandboxExit::Denial
        );
        assert_eq!(
            SandboxExit::from_output(&ExecOutput {
                exit_code: Some(7),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: None,
            }),
            SandboxExit::Child(7)
        );
        assert_eq!(
            SandboxExit::from_output(&ExecOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: None,
            }),
            SandboxExit::Unknown
        );
    }
}
