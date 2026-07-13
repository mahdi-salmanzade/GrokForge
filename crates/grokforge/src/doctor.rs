//! `grokforge doctor` — reports toolchain, sandbox capability, git, and config health so users
//! can see exactly what is (and isn't) enforced on their machine. Honest capability reporting is
//! a project principle: never claim protection that isn't active.

use std::process::ExitCode;

use grokforge_sandbox::default_runner;

pub fn run() -> ExitCode {
    println!("grokforge {}", env!("CARGO_PKG_VERSION"));
    println!("minimum toolchain: {}", env!("CARGO_PKG_RUST_VERSION"));
    println!();

    // Sandbox capability (the load-bearing security claim).
    let runner = default_runner();
    let cap = runner.capability();
    let status = if cap.enforced {
        "● enforced"
    } else {
        "○ NOT enforced (confined commands fail closed)"
    };
    println!("sandbox backend: {}  [{status}]", cap.backend);
    for note in &cap.notes {
        println!("  - {note}");
    }
    println!();

    // Git availability (needed for the git-native workflow).
    match grokforge_git::Git::trusted_executable() {
        Ok(path) => println!("git: trusted executable at {}", path.display()),
        Err(error) => println!("git: unavailable ({error}; auto-commit/undo disabled)"),
    }

    // API key presence (never the value): from the env var or the OS keychain.
    let from_env = std::env::var("XAI_API_KEY").is_ok_and(|k| !k.trim().is_empty());
    let from_keychain = crate::credentials::load_stored().is_some();
    let key_status = if from_env {
        "set (XAI_API_KEY env)"
    } else if from_keychain {
        "set (OS keychain)"
    } else {
        "not set — run `grokforge login`, or export XAI_API_KEY"
    };
    println!("API key: {key_status}");
    let base = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());
    let endpoint = crate::sanitize_terminal_line(&base);
    // This only parses and validates the URL; no request is made and the placeholder is never
    // logged or retained after this branch.
    match grokforge_xai::XaiClient::new(&base, "doctor-validation-only") {
        Ok(_) => println!("endpoint: {endpoint}"),
        Err(error) => println!(
            "endpoint: {endpoint}  [INVALID: {}]",
            crate::sanitize_terminal_line(&error.to_string())
        ),
    }

    println!();
    println!("privacy: serialized model-request body bytes (including retries) are accounted for");
    println!(
        "in the context ledger. HTTP headers, MCP, and approved/full-access tools are separate"
    );
    println!("egress boundaries. Telemetry: off.");

    ExitCode::SUCCESS
}
