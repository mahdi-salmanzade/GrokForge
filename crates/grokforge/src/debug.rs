//! Hidden developer diagnostics. `debug api` streams a single prompt directly from the
//! xAI API — a manual live smoke test for the client (nightly CI runs this against the
//! real endpoint to catch API drift).

use std::io::Write;

use futures::StreamExt;
use grokforge_xai::{InputItem, ResponsesRequest, Role, StreamEvent, XaiClient};

pub async fn run_api(prompt: &str, model: &str) -> std::process::ExitCode {
    let Ok(api_key) = std::env::var("XAI_API_KEY") else {
        eprintln!("XAI_API_KEY is not set");
        return std::process::ExitCode::from(3);
    };
    let base_url = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());

    let client = match XaiClient::new(&base_url, api_key) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client error: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let req = ResponsesRequest::new(model, vec![InputItem::text(Role::User, prompt)]);
    let mut stream = match client.stream(&req).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("request error: {e}");
            return exit_for_error(&e);
        }
    };

    let mut stdout = std::io::stdout();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(t)) => {
                print!("{t}");
                let _ = stdout.flush();
            }
            Ok(StreamEvent::ToolCall(c)) => {
                eprintln!("\n[tool call: {} {}]", c.name, c.arguments);
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
                eprintln!("\nstream error: {e}");
                return exit_for_error(&e);
            }
        }
    }
    println!();
    std::process::ExitCode::SUCCESS
}

fn exit_for_error(e: &grokforge_xai::XaiError) -> std::process::ExitCode {
    use grokforge_xai::XaiError;
    let code = match e {
        XaiError::Auth { .. } | XaiError::UnknownModel { .. } => 3,
        _ => 4,
    };
    std::process::ExitCode::from(code)
}
