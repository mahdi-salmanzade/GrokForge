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
        "○ NOT enforced (approval-only)"
    };
    println!("sandbox backend: {}  [{status}]", cap.backend);
    for note in &cap.notes {
        println!("  - {note}");
    }
    println!();

    // Git availability (needed for the git-native workflow).
    let git_ok = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    println!(
        "git: {}",
        if git_ok {
            "found"
        } else {
            "NOT FOUND (auto-commit/undo unavailable)"
        }
    );

    // API key presence (not the value).
    let key = std::env::var("XAI_API_KEY").is_ok();
    println!(
        "XAI_API_KEY: {}",
        if key {
            "set"
        } else {
            "not set (export it or use `grokforge login` once it lands)"
        }
    );
    let base = std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai".to_string());
    println!("endpoint: {base}");

    println!();
    println!("privacy: no network egress except the endpoint above and any MCP servers you");
    println!("connect; every request is accounted for in the context ledger. Telemetry: off.");

    ExitCode::SUCCESS
}
