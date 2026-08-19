//! `grokforge doctor` — reports toolchain, sandbox capability, git, config, local
//! code-intelligence, custom tools, MCP, and the loopback API so users can see exactly what is
//! (and isn't) enforced on their machine. Honest capability reporting is a project principle:
//! never claim protection that isn't active.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use grokforge_sandbox::default_runner;
use serde::Deserialize;

const RUSTC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CODE_INTEL_BYTES: usize = 256 * 1024;
const MAX_MCP_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_MCP_SERVERS: usize = 16;
const MAX_PROJECT_MCP_SERVER_NAME_BYTES: usize = 256;

#[allow(clippy::too_many_lines)] // Capability sections stay sequential so each boundary is visible.
pub fn run(trust_project_config: bool) -> ExitCode {
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

    // Credential presence (never the value): env var, or the encrypted file (locked).
    let from_env = std::env::var("XAI_API_KEY").is_ok_and(|k| !k.trim().is_empty());
    let has_file = crate::credentials::has_stored_file();
    let key_status = if from_env {
        "XAI_API_KEY env"
    } else if has_file {
        "encrypted file on host (unlock with your password)"
    } else {
        "none — run `grokforge` and set a password, or `grokforge login`"
    };
    println!("credential: {key_status}");
    let workspace = std::env::current_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    let settings = if let Some(workspace) = workspace.as_deref() {
        grokforge_config::Config::load_with_project_config(workspace, trust_project_config)
            .map(Some)
            .map_err(|error| error.to_string())
    } else {
        Ok(None)
    };
    let configured_base = match &settings {
        Ok(Some(config)) => {
            println!(
                "config: {}  [valid]",
                grokforge_config::global_config_path().map_or_else(
                    |_| "~/.grokforge/config.toml".to_string(),
                    |path| path.display().to_string()
                )
            );
            println!("default model: {}", config.agent.default_model);
            config.provider.grok.base_url.clone()
        }
        Ok(None) => "https://api.x.ai".to_string(),
        Err(error) => {
            println!("config: INVALID ({})", crate::sanitize_terminal_line(error));
            "https://api.x.ai".to_string()
        }
    };
    let base = std::env::var("XAI_BASE_URL").unwrap_or(configured_base);
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
    print_host();
    println!();
    print_code_intelligence(workspace.as_deref());
    println!();
    print_custom_tools(workspace.as_deref());
    println!();
    print_mcp(workspace.as_deref());
    println!();
    println!("local API: grokforge serve is loopback-only and bearer-authenticated");
    println!();
    for line in privacy_report_lines() {
        println!("{line}");
    }

    ExitCode::SUCCESS
}

fn print_host() {
    println!("host: {}", host_os_arch());
    match rustc_version(RUSTC_TIMEOUT) {
        Some(version) => println!("rustc: {}", crate::sanitize_terminal_line(&version)),
        None => println!("rustc: unavailable"),
    }
}

fn print_code_intelligence(workspace: Option<&Path>) {
    println!(
        "code intelligence: grokforge-context {}",
        grokforge_context::VERSION
    );
    match owner_grokforge_file("code-intelligence.toml") {
        Some(path) => println!(
            "  owner ~/.grokforge/code-intelligence.toml  {}",
            describe_code_intel_file(&path, true)
        ),
        None => println!(
            "  owner ~/.grokforge/code-intelligence.toml  [unavailable: home directory not found]"
        ),
    }

    match workspace {
        Some(workspace) => {
            let path = workspace.join(".grokforge/code-intelligence.toml");
            println!(
                "  project .grokforge/code-intelligence.toml  {}",
                describe_code_intel_file(&path, false)
            );
        }
        None => println!(
            "  project .grokforge/code-intelligence.toml  [unavailable: workspace could not be resolved]"
        ),
    }
    println!(
        "  project language-server/formatter config is not loaded (owner-only; no trust flag)"
    );
    println!("  doctor does not execute language servers");
}

fn print_custom_tools(workspace: Option<&Path>) {
    match owner_grokforge_file("tools.toml") {
        Some(path) => println!(
            "custom tools: owner ~/.grokforge/tools.toml  {}",
            describe_path_presence(path_presence(&path))
        ),
        None => println!(
            "custom tools: owner ~/.grokforge/tools.toml  [unavailable: home directory not found]"
        ),
    }

    match workspace {
        Some(workspace) => {
            let path = workspace.join(".grokforge/tools.toml");
            println!(
                "  project .grokforge/tools.toml  {}",
                describe_path_presence(path_presence(&path))
            );
        }
        None => {
            println!(
                "  project .grokforge/tools.toml  [unavailable: workspace could not be resolved]"
            );
        }
    }
    println!("  project tools are not loaded unless --trust-project-tools is passed");
    println!("  doctor does not load or execute custom tools");
}

fn print_mcp(workspace: Option<&Path>) {
    let Some(workspace) = workspace else {
        println!(
            "mcp: project .grokforge/mcp.json  [unavailable: workspace could not be resolved]"
        );
        println!("  doctor does not start MCP processes");
        return;
    };
    let path = workspace.join(".grokforge/mcp.json");
    match inspect_mcp_path(&path) {
        McpInspection::Absent => println!("mcp: no project .grokforge/mcp.json"),
        McpInspection::Invalid(error) => println!(
            "mcp: .grokforge/mcp.json  [INVALID: {}]",
            crate::sanitize_terminal_line(&error)
        ),
        McpInspection::Valid(servers) if servers.is_empty() => {
            println!("mcp: .grokforge/mcp.json  [valid; 0 servers]");
        }
        McpInspection::Valid(servers) => {
            println!(
                "mcp: .grokforge/mcp.json  [valid; {} server{}]",
                servers.len(),
                if servers.len() == 1 { "" } else { "s" }
            );
            for server in servers {
                println!(
                    "  {} ({})",
                    crate::sanitize_terminal_line(&server.name),
                    server.transport
                );
            }
        }
    }
    println!("  not started (requires --trust-project-mcp; doctor never starts MCP)");
}

#[must_use]
fn host_os_arch() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn rustc_version(timeout: Duration) -> Option<String> {
    let mut child = Command::new("rustc")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut stdout = child.stdout.take()?;
                let mut buf = String::new();
                stdout.read_to_string(&mut buf).ok()?;
                let line = buf.lines().next()?.trim();
                if line.is_empty() {
                    return None;
                }
                return Some(line.to_string());
            }
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn owner_grokforge_file(name: &str) -> Option<PathBuf> {
    directories::BaseDirs::new().map(|base| base.home_dir().join(".grokforge").join(name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPresence {
    Absent,
    File,
    Other,
}

fn path_presence(path: &Path) -> Result<PathPresence, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(PathPresence::File),
        Ok(_) => Ok(PathPresence::Other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PathPresence::Absent),
        Err(error) => Err(error.to_string()),
    }
}

fn describe_path_presence(presence: Result<PathPresence, String>) -> String {
    match presence {
        Ok(PathPresence::File) => "[present]".to_string(),
        Ok(PathPresence::Absent) => "[absent]".to_string(),
        Ok(PathPresence::Other) => "[INVALID: not a regular file]".to_string(),
        Err(error) => format!("[INVALID: {}]", crate::sanitize_terminal_line(&error)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CodeIntelCounts {
    languages: usize,
    language_servers: usize,
    formatters: usize,
}

fn describe_code_intel_file(path: &Path, owner: bool) -> String {
    match read_optional_regular_text(path, MAX_CODE_INTEL_BYTES) {
        Ok(None) => {
            if owner {
                "[absent; built-in defaults]".to_string()
            } else {
                "[absent]".to_string()
            }
        }
        Ok(Some(text)) => match inspect_code_intel_text(&text) {
            Ok(counts) => format!("[valid; {}]", format_code_intel_counts(counts)),
            Err(error) => format!("[INVALID: {}]", crate::sanitize_terminal_line(&error)),
        },
        Err(error) => format!("[INVALID: {}]", crate::sanitize_terminal_line(&error)),
    }
}

fn read_optional_regular_text(path: &Path, max_bytes: usize) -> Result<Option<String>, String> {
    match path_presence(path)? {
        PathPresence::Absent => Ok(None),
        PathPresence::Other => Err("not a regular file".to_string()),
        PathPresence::File => read_regular_text(path, max_bytes).map(Some),
    }
}

fn read_regular_text(path: &Path, max_bytes: usize) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(
        u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|error| error.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!("exceeds the {max_bytes}-byte limit"));
    }
    String::from_utf8(bytes).map_err(|error| format!("not valid UTF-8: {error}"))
}

fn inspect_code_intel_text(text: &str) -> Result<CodeIntelCounts, String> {
    grokforge_context::CodeIntelligenceConfig::from_toml(text)
        .map_err(|error| error.to_string())?;
    Ok(count_code_intelligence(text))
}

fn format_code_intel_counts(counts: CodeIntelCounts) -> String {
    format!(
        "{} language{}, {} language server{}, {} formatter{}",
        counts.languages,
        if counts.languages == 1 { "" } else { "s" },
        counts.language_servers,
        if counts.language_servers == 1 {
            ""
        } else {
            "s"
        },
        counts.formatters,
        if counts.formatters == 1 { "" } else { "s" }
    )
}

fn count_code_intelligence(text: &str) -> CodeIntelCounts {
    let mut scan = LanguageScan::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[language]]" {
            scan.flush();
            scan.in_language = true;
            continue;
        }
        if !scan.in_language {
            continue;
        }
        if toml_line_has_table_or_key(line, "lsp") {
            scan.current_lsp = true;
        }
        if toml_line_has_table_or_key(line, "formatter") {
            scan.current_formatter = true;
        }
    }
    scan.flush();
    scan.counts
}

#[derive(Debug, Default)]
struct LanguageScan {
    counts: CodeIntelCounts,
    in_language: bool,
    current_lsp: bool,
    current_formatter: bool,
}

impl LanguageScan {
    fn flush(&mut self) {
        if !self.in_language {
            return;
        }
        self.counts.languages = self.counts.languages.saturating_add(1);
        if self.current_lsp {
            self.counts.language_servers = self.counts.language_servers.saturating_add(1);
        }
        if self.current_formatter {
            self.counts.formatters = self.counts.formatters.saturating_add(1);
        }
        self.in_language = false;
        self.current_lsp = false;
        self.current_formatter = false;
    }
}

fn toml_line_has_table_or_key(line: &str, key: &str) -> bool {
    let table = format!("[language.{key}]");
    let nested = format!("[language.{key}.");
    let dotted = format!("{key}.");
    line == table
        || line.starts_with(&nested)
        || line == key
        || line.starts_with(&format!("{key}="))
        || line.starts_with(&format!("{key} ="))
        || line.starts_with(&dotted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpServerSummary {
    name: String,
    transport: &'static str,
}

#[derive(Debug)]
enum McpInspection {
    Absent,
    Invalid(String),
    Valid(Vec<McpServerSummary>),
}

#[derive(Debug, Deserialize)]
struct DoctorMcpConfig {
    #[serde(default)]
    servers: BTreeMap<String, serde_json::Value>,
}

fn inspect_mcp_path(path: &Path) -> McpInspection {
    match read_optional_regular_text(path, MAX_MCP_CONFIG_BYTES) {
        Ok(None) => McpInspection::Absent,
        Ok(Some(text)) => match inspect_project_mcp(&text) {
            Ok(servers) => McpInspection::Valid(servers),
            Err(error) => McpInspection::Invalid(error),
        },
        Err(error) => McpInspection::Invalid(error),
    }
}

fn inspect_project_mcp(text: &str) -> Result<Vec<McpServerSummary>, String> {
    let config: DoctorMcpConfig = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if config.servers.len() > MAX_PROJECT_MCP_SERVERS {
        return Err(format!(
            "project MCP configuration exceeds the {MAX_PROJECT_MCP_SERVERS}-server limit"
        ));
    }

    let mut summaries = Vec::with_capacity(config.servers.len());
    for (name, spec) in config.servers {
        if !valid_mcp_server_name(&name) {
            return Err("MCP server name is invalid".to_string());
        }
        let Some(transport) = mcp_transport_label(&spec) else {
            return Err(format!(
                "server `{}` has an unknown transport",
                crate::sanitize_terminal_line(&name)
            ));
        };
        summaries.push(McpServerSummary { name, transport });
    }
    Ok(summaries)
}

fn valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROJECT_MCP_SERVER_NAME_BYTES
        && !name.chars().any(char::is_control)
}

fn mcp_transport_label(spec: &serde_json::Value) -> Option<&'static str> {
    let object = spec.as_object()?;
    let command = object.get("command");
    let url = object.get("url");
    match (command, url) {
        (Some(command), None) if command.is_string() => Some("stdio"),
        (None, Some(url)) if url.is_string() => Some("http"),
        _ => None,
    }
}

#[must_use]
fn privacy_report_lines() -> &'static [&'static str] {
    &[
        "privacy: model request bodies (including retries) go through the context ledger.",
        "Configured Streamable HTTP MCP JSON-RPC request bodies are byte-accounted.",
        "Local stdio MCP process egress is unaudited.",
        "HTTP headers, API responses, and shell-command traffic are outside the ledger.",
        "Telemetry: off.",
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn path_presence_classifies_missing_file_directory_and_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("tools.toml");
        assert_eq!(path_presence(&missing), Ok(PathPresence::Absent));
        assert_eq!(path_presence(directory.path()), Ok(PathPresence::Other));
        std::fs::write(&missing, "version = 1\n").unwrap();
        assert_eq!(path_presence(&missing), Ok(PathPresence::File));
        assert_eq!(describe_path_presence(Ok(PathPresence::File)), "[present]");
        assert_eq!(describe_path_presence(Ok(PathPresence::Absent)), "[absent]");
        assert_eq!(
            describe_path_presence(Ok(PathPresence::Other)),
            "[INVALID: not a regular file]"
        );
        assert_eq!(
            describe_path_presence(Err("permission denied\nfor file".into())),
            "[INVALID: permission denied for file]"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_presence_treats_symlinks_as_not_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.toml");
        std::fs::write(&target, "version = 1\n").unwrap();
        let link = directory.path().join("tools.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(path_presence(&link), Ok(PathPresence::Other));
    }

    #[test]
    fn mcp_transport_label_distinguishes_stdio_and_http_without_leaking_headers() {
        let stdio = serde_json::json!({"command": "my-mcp-server", "args": ["--stdio"]});
        let http = serde_json::json!({
            "url": "https://mcp.example.test/rpc",
            "headers": {"Authorization": "Bearer secret-token"}
        });
        let both = serde_json::json!({"command": "my-mcp-server", "url": "https://example.test"});
        let empty = serde_json::json!({});
        assert_eq!(mcp_transport_label(&stdio), Some("stdio"));
        assert_eq!(mcp_transport_label(&http), Some("http"));
        assert_eq!(mcp_transport_label(&both), None);
        assert_eq!(mcp_transport_label(&empty), None);
        assert_eq!(mcp_transport_label(&serde_json::json!("stdio")), None);
    }

    #[test]
    fn inspect_project_mcp_lists_sorted_names_and_kinds_without_secrets() {
        let text = r#"{
            "servers": {
                "remote": {
                    "url": "https://mcp.example.test/rpc",
                    "headers": {"Authorization": "Bearer secret-token"}
                },
                "local": {"command": "my-mcp-server", "args": ["--flag"]}
            }
        }"#;
        let servers = inspect_project_mcp(text).unwrap();
        assert_eq!(
            servers,
            vec![
                McpServerSummary {
                    name: "local".into(),
                    transport: "stdio",
                },
                McpServerSummary {
                    name: "remote".into(),
                    transport: "http",
                },
            ]
        );
        let rendered = format!("{servers:?}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("Bearer"));
        assert!(!rendered.contains("https://"));
        assert!(inspect_project_mcp("{").is_err());
        assert!(inspect_project_mcp(r#"{"servers":[]}"#).is_err());
        assert!(
            inspect_project_mcp(r#"{"servers":{"bad":{"command":1}}}"#)
                .unwrap_err()
                .contains("unknown transport")
        );
    }

    #[test]
    fn count_code_intelligence_counts_language_servers_and_formatters() {
        let text = r#"
            extend_defaults = false
            # [[language]]
            [[language]]
            name = "demo"
            lsp = { command = "/bin/true" }

            [[language]]
            name = "fmt"
            [language.formatter]
            command = "/bin/echo"
            args = ["{file}"]
        "#;
        assert_eq!(
            count_code_intelligence(text),
            CodeIntelCounts {
                languages: 2,
                language_servers: 1,
                formatters: 1,
            }
        );
        assert_eq!(
            count_code_intelligence("extend_defaults = true\n"),
            CodeIntelCounts {
                languages: 0,
                language_servers: 0,
                formatters: 0,
            }
        );
        assert_eq!(
            format_code_intel_counts(CodeIntelCounts {
                languages: 1,
                language_servers: 1,
                formatters: 0,
            }),
            "1 language, 1 language server, 0 formatters"
        );
        assert!(inspect_code_intel_text("language = 1\n").is_err());
        let valid = inspect_code_intel_text(
            r#"
                extend_defaults = false
                [[language]]
                name = "demo"
                extensions = ["demo"]
                [language.formatter]
                command = "/bin/echo"
                args = ["{file}"]
            "#,
        )
        .unwrap();
        assert_eq!(
            valid,
            CodeIntelCounts {
                languages: 1,
                language_servers: 0,
                formatters: 1,
            }
        );
    }

    #[test]
    fn privacy_lines_match_the_ledger_scope() {
        let lines = privacy_report_lines();
        let text = lines.join("\n").to_ascii_lowercase();
        assert!(text.contains("retries"));
        assert!(text.contains("context ledger"));
        assert!(text.contains("streamable http"));
        assert!(text.contains("byte-accounted"));
        assert!(text.contains("stdio"));
        assert!(text.contains("unaudited"));
        assert!(text.contains("headers"));
        assert!(text.contains("api responses"));
        assert!(text.contains("shell-command"));
        assert!(text.contains("telemetry: off"));
        assert!(!text.contains("every byte"));
        assert!(lines[0].starts_with("privacy:"));
    }

    #[test]
    fn host_os_arch_uses_std_consts() {
        assert_eq!(
            host_os_arch(),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
        );
    }

    #[test]
    fn inspect_mcp_path_reads_files_and_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        assert!(matches!(inspect_mcp_path(&path), McpInspection::Absent));
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(inspect_mcp_path(&path), McpInspection::Invalid(_)));
        std::fs::write(
            &path,
            r#"{"servers":{"docs":{"url":"https://mcp.example.test"}}}"#,
        )
        .unwrap();
        match inspect_mcp_path(&path) {
            McpInspection::Valid(servers) => {
                assert_eq!(
                    servers,
                    vec![McpServerSummary {
                        name: "docs".into(),
                        transport: "http",
                    }]
                );
            }
            other => panic!("expected valid MCP inspection, got {other:?}"),
        }
    }
}
