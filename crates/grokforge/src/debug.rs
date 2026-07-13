//! Hidden developer diagnostics. `debug api` streams a single prompt directly from the
//! xAI API — a manual, credentialed live smoke test for checking provider API drift.

use std::io::Write;

use futures::StreamExt;
use grokforge_core::{Redactor, Session, SessionConfig};
use grokforge_protocol::ResponseItem;
use grokforge_xai::{StreamEvent, XaiClient};

pub async fn run_api(prompt: &str, model: &str) -> std::process::ExitCode {
    let Some(api_key) = crate::credentials::resolve(false).await else {
        return std::process::ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {}", crate::sanitize_terminal(&e.to_string()));
            return std::process::ExitCode::from(2);
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
            return std::process::ExitCode::from(2);
        }
        Err(error) => {
            eprintln!(
                "debug API request refused: could not resolve current workspace: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return std::process::ExitCode::from(2);
        }
    };
    let mut session = Session::new(SessionConfig::new(workspace, model));
    let prompt = Redactor::apply(prompt);
    session
        .history
        .push(ResponseItem::user_redacted(prompt.text, prompt.count));
    let assembled = match grokforge_core::context::assemble(&session, &[], Vec::new()) {
        Ok(assembled) => assembled,
        Err(error) => {
            eprintln!(
                "request assembly error: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return std::process::ExitCode::from(4);
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

async fn consume_stream(mut stream: grokforge_xai::ResponseStream) -> std::process::ExitCode {
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
                return std::process::ExitCode::SUCCESS;
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
    std::process::ExitCode::from(4)
}

fn retry_ledger_line(number: u32, request_bytes: usize) -> Option<String> {
    (number > 1).then(|| format!("[ledger: {request_bytes} bytes — request_retry_{number}]"))
}

fn exit_for_error(e: &grokforge_xai::XaiError) -> std::process::ExitCode {
    use grokforge_xai::XaiError;
    let code = match e {
        XaiError::Auth { .. } | XaiError::UnknownModel { .. } => 3,
        _ => 4,
    };
    std::process::ExitCode::from(code)
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
}
