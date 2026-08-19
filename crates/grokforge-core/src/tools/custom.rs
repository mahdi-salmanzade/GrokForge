//! Owner-controlled executable custom tools.
//!
//! This is deliberately not an in-process plugin ABI. A manifest names one hashed executable,
//! exact arguments, and a bounded JSON Schema. Calls cross a JSON stdin/stdout boundary and run
//! through the same sandbox, approval, cancellation, and output-redaction path as built-ins.

use std::collections::{BTreeSet, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use directories::BaseDirs;
use grokforge_protocol::{ApprovalKind, NetworkMode, SandboxMode};
use grokforge_sandbox::{CommandSpec, ExecError, OUTPUT_CAP};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::approvals::ApprovalNeed;
use crate::redaction::Redactor;
use crate::tools::{
    MAX_TOOLS, Tool, ToolInvocation, ToolOutput, ToolRegistry, ToolSpec, TurnContext,
};

/// File read/parse limit. Tool schemas and declarations should stay reviewable.
pub const MAX_CUSTOM_MANIFEST_BYTES: u64 = 256 * 1024;
/// Maximum executable tools declared by one manifest.
pub const MAX_CUSTOM_TOOLS_PER_MANIFEST: usize = 32;
/// Maximum serialized JSON Schema size per tool.
pub const MAX_CUSTOM_SCHEMA_BYTES: usize = 32 * 1024;
/// Maximum tool input envelope sent over stdin.
pub const MAX_CUSTOM_INPUT_BYTES: usize = 256 * 1024;
/// Maximum executable size accepted by the integrity verifier.
pub const MAX_CUSTOM_EXECUTABLE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum static argv entries after the executable.
pub const MAX_CUSTOM_ARGS: usize = 32;
/// Maximum explicitly copied ambient environment names.
pub const MAX_CUSTOM_ENV: usize = 16;
/// Longest custom tool runtime.
pub const MAX_CUSTOM_TIMEOUT_MS: u64 = 120_000;

const MANIFEST_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 1_024;
const MAX_COMMAND_BYTES: usize = 4_096;
const MAX_ARG_BYTES: usize = 4_096;
const MAX_ARGS_BYTES: usize = 16 * 1024;
const MAX_ENV_NAME_BYTES: usize = 128;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_OUTPUT_BYTES: usize = OUTPUT_CAP;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 2_048;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CustomToolLoadReport {
    pub registered: Vec<String>,
    pub warnings: Vec<String>,
}

/// Load `~/.grokforge/tools.toml` and, when explicitly trusted, the workspace's
/// `.grokforge/tools.toml`, then register valid tools without replacing anything already present.
#[must_use]
pub fn register_custom_tools(
    workspace: &Path,
    trust_project_tools: bool,
    registry: &mut ToolRegistry,
) -> CustomToolLoadReport {
    let mut report = CustomToolLoadReport::default();
    let global = BaseDirs::new().map(|dirs| dirs.home_dir().join(".grokforge/tools.toml"));
    match global {
        Some(path) => register_manifest(&path, ManifestTrust::Owner, registry, &mut report),
        None => report
            .warnings
            .push("cannot locate ~/.grokforge/tools.toml: home directory unavailable".into()),
    }

    let project_directory = workspace.join(".grokforge");
    let project = project_directory.join("tools.toml");
    if trust_project_tools {
        register_manifest(
            &project,
            ManifestTrust::TrustedProject,
            registry,
            &mut report,
        );
    } else if project.exists() {
        report.warnings.push(format!(
            "ignored project custom-tool manifest {}; project tools require explicit trust",
            project.display()
        ));
    }
    report
}

#[derive(Debug, Clone, Copy)]
enum ManifestTrust {
    Owner,
    TrustedProject,
}

fn register_manifest(
    path: &Path,
    trust: ManifestTrust,
    registry: &mut ToolRegistry,
    report: &mut CustomToolLoadReport,
) {
    let declarations = match load_manifest(path, trust) {
        Ok(Some(declarations)) => declarations,
        Ok(None) => return,
        Err(error) => {
            report.warnings.push(format!(
                "custom tools from {} were not loaded: {error}",
                path.display()
            ));
            return;
        }
    };

    for tool in declarations {
        let name = tool.spec().name;
        if registry.get(&name).is_some() {
            report.warnings.push(format!(
                "custom tool `{name}` was ignored because that name is already registered"
            ));
            continue;
        }
        if registry.specs().len() >= MAX_TOOLS {
            report.warnings.push(format!(
                "custom tool `{name}` was ignored because the total tool limit is {MAX_TOOLS}"
            ));
            continue;
        }
        registry.register(Arc::new(tool));
        if registry.get(&name).is_some() {
            report.registered.push(name);
        } else {
            report
                .warnings
                .push(format!("custom tool `{name}` could not be registered"));
        }
    }
}

fn load_manifest(
    path: &Path,
    trust: ManifestTrust,
) -> Result<Option<Vec<CustomExecutableTool>>, CustomToolError> {
    let Some(text) = read_manifest(path, trust)? else {
        return Ok(None);
    };
    let manifest: CustomToolsManifest = toml::from_str(&text)
        .map_err(|error| CustomToolError::InvalidManifest(error.to_string()))?;
    if manifest.version != MANIFEST_VERSION {
        return Err(CustomToolError::InvalidManifest(format!(
            "unsupported manifest version {}; expected {MANIFEST_VERSION}",
            manifest.version
        )));
    }
    if manifest.tool.len() > MAX_CUSTOM_TOOLS_PER_MANIFEST {
        return Err(CustomToolError::InvalidManifest(format!(
            "manifest declares {} tools; limit is {MAX_CUSTOM_TOOLS_PER_MANIFEST}",
            manifest.tool.len()
        )));
    }
    let mut names = HashSet::new();
    manifest
        .tool
        .into_iter()
        .map(|declaration| {
            if !names.insert(declaration.name.clone()) {
                return Err(CustomToolError::InvalidTool {
                    name: declaration.name,
                    reason: "duplicate name in one manifest".into(),
                });
            }
            CustomExecutableTool::try_from(declaration)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomToolsManifest {
    version: u32,
    #[serde(default)]
    tool: Vec<CustomToolDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomToolDeclaration {
    name: String,
    description: String,
    command: String,
    sha256: String,
    #[serde(default)]
    args: Vec<String>,
    input_schema: serde_json::Value,
    #[serde(default = "default_mutating")]
    mutating: bool,
    #[serde(default)]
    parallel_safe: bool,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    env_allowlist: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_output_bytes")]
    max_output_bytes: usize,
}

const fn default_mutating() -> bool {
    true
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

const fn default_output_bytes() -> usize {
    DEFAULT_OUTPUT_BYTES
}

#[derive(Debug)]
struct CustomExecutableTool {
    spec: ToolSpec,
    command: PathBuf,
    executable_sha256: String,
    args: Vec<String>,
    env_allowlist: Vec<String>,
    network: bool,
    timeout: Duration,
    max_output_bytes: usize,
    metadata_redactions: usize,
}

impl CustomExecutableTool {
    fn try_from(declaration: CustomToolDeclaration) -> Result<Self, CustomToolError> {
        validate_name(&declaration.name)?;
        if declaration.description.is_empty()
            || declaration.description.len() > MAX_DESCRIPTION_BYTES
            || declaration.description.chars().any(char::is_control)
        {
            return Err(invalid_tool(
                declaration.name,
                format!("description must contain 1..={MAX_DESCRIPTION_BYTES} non-control bytes"),
            ));
        }
        if declaration.timeout_ms == 0 || declaration.timeout_ms > MAX_CUSTOM_TIMEOUT_MS {
            return Err(invalid_tool(
                declaration.name,
                format!("timeout_ms must be in 1..={MAX_CUSTOM_TIMEOUT_MS}"),
            ));
        }
        if declaration.max_output_bytes == 0 || declaration.max_output_bytes > OUTPUT_CAP {
            return Err(invalid_tool(
                declaration.name,
                format!("max_output_bytes must be in 1..={OUTPUT_CAP}"),
            ));
        }
        validate_args(&declaration.name, &declaration.args)?;
        validate_env_allowlist(&declaration.name, &declaration.env_allowlist)?;
        let command =
            validate_executable(&declaration.name, &declaration.command, &declaration.sha256)?;

        let schema_bytes = serde_json::to_vec(&declaration.input_schema).map_err(|error| {
            invalid_tool(
                declaration.name.clone(),
                format!("invalid input_schema: {error}"),
            )
        })?;
        if schema_bytes.len() > MAX_CUSTOM_SCHEMA_BYTES {
            return Err(invalid_tool(
                declaration.name,
                format!("input_schema exceeds {MAX_CUSTOM_SCHEMA_BYTES} bytes"),
            ));
        }
        validate_schema_shape(&declaration.name, &declaration.input_schema)?;

        let redacted_description = Redactor::apply(&declaration.description);
        let (redacted_schema, schema_redactions) = redact_json_strings(&declaration.input_schema);
        jsonschema::validator_for(&redacted_schema).map_err(|error| {
            invalid_tool(
                declaration.name.clone(),
                format!("input_schema is not a valid self-contained JSON Schema: {error}"),
            )
        })?;
        let metadata_redactions = redacted_description.count.saturating_add(schema_redactions);
        Ok(Self {
            spec: ToolSpec {
                name: declaration.name,
                description: redacted_description.text,
                parameters: redacted_schema,
                mutating: declaration.mutating,
                parallel_safe: declaration.parallel_safe,
            },
            command,
            executable_sha256: declaration.sha256.to_ascii_lowercase(),
            args: declaration.args,
            env_allowlist: declaration.env_allowlist,
            network: declaration.network,
            timeout: Duration::from_millis(declaration.timeout_ms),
            max_output_bytes: declaration.max_output_bytes,
            metadata_redactions,
        })
    }

    fn environment(&self) -> Result<Vec<(String, String)>, ToolOutput> {
        self.env_allowlist
            .iter()
            .filter_map(|name| match std::env::var(name) {
                Ok(value) => {
                    if Redactor::apply(&value).count == 0 {
                        Some(Ok((name.clone(), value)))
                    } else {
                        Some(Err(ToolOutput::failure(format!(
                            "custom tool environment `{name}` contains credential-like data"
                        ))))
                    }
                }
                Err(std::env::VarError::NotPresent) => None,
                Err(std::env::VarError::NotUnicode(_)) => Some(Err(ToolOutput::failure(format!(
                    "custom tool environment `{name}` is not valid UTF-8"
                )))),
            })
            .collect()
    }
}

#[async_trait]
impl Tool for CustomExecutableTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn metadata_redactions(&self) -> usize {
        self.metadata_redactions
    }

    fn approval(&self, _args: &serde_json::Value, ctx: &TurnContext) -> ApprovalNeed {
        // Always show the exact executable boundary first. If a network-enabled tool actually
        // reaches the network under an isolated policy, the classified denial drives a separate
        // capability-specific retry approval instead of hiding this command approval.
        let mut command = Vec::with_capacity(self.args.len().saturating_add(1));
        command.push(self.command.to_string_lossy().into_owned());
        command.extend(self.args.clone());
        ApprovalNeed::Always(ApprovalKind::ExecCommand {
            command,
            cwd: ctx.workspace_root.clone(),
            sandbox: ctx.policy.mode,
            escalation_of: None,
        })
    }

    #[allow(clippy::too_many_lines)] // Keep validation, staging, sandboxing, and protocol checks auditable in order.
    async fn invoke(&self, inv: ToolInvocation<'_>) -> ToolOutput {
        let validator = match jsonschema::validator_for(&self.spec.parameters) {
            Ok(validator) => validator,
            Err(error) => {
                return ToolOutput::failure(format!(
                    "custom tool schema became invalid after registration: {error}"
                ));
            }
        };
        if let Err(error) = validator.validate(&inv.args) {
            return ToolOutput::failure(format!(
                "arguments do not match `{}` input_schema: {error}",
                self.spec.name
            ));
        }

        let envelope = CustomToolInput {
            protocol_version: MANIFEST_VERSION,
            call_id: inv.call_id.to_string(),
            workspace_root: inv.ctx.workspace_root.to_string_lossy().into_owned(),
            arguments: &inv.args,
        };
        let stdin = match serde_json::to_vec(&envelope) {
            Ok(stdin) if stdin.len() <= MAX_CUSTOM_INPUT_BYTES => stdin,
            Ok(stdin) => {
                return ToolOutput::failure(format!(
                    "custom tool input is {} bytes; limit is {MAX_CUSTOM_INPUT_BYTES}",
                    stdin.len()
                ));
            }
            Err(error) => {
                return ToolOutput::failure(format!("cannot encode custom tool input: {error}"));
            }
        };

        let executable = self.command.clone();
        let expected_hash = self.executable_sha256.clone();
        let staged = match tokio::task::spawn_blocking(move || {
            stage_verified_executable(&executable, &expected_hash)
        })
        .await
        {
            Ok(Ok(staged)) => staged,
            Ok(Err(error)) => return ToolOutput::failure(error.to_string()),
            Err(error) => {
                return ToolOutput::failure(format!(
                    "custom tool integrity check task failed: {error}"
                ));
            }
        };
        let env = match self.environment() {
            Ok(env) => env,
            Err(error) => return error,
        };
        let mut policy = inv.ctx.policy.clone();
        if !self.network {
            policy.network = NetworkMode::Isolated;
        }
        if !self.spec.mutating {
            policy.mode = SandboxMode::ReadOnly;
            policy.writable_roots.clear();
        }
        let command = CommandSpec {
            program: staged.path.to_string_lossy().into_owned(),
            args: self.args.clone(),
            cwd: inv.ctx.workspace_root.clone(),
            timeout: self.timeout,
            stdin: Some(stdin),
            stdin_close_delay: Duration::ZERO,
            env,
            private_read_roots: vec![staged.private_read_root()],
            cancellation: Some(inv.ctx.cancellation.process_token()),
        };
        let output = match inv.ctx.sandbox.run(&policy, &command).await {
            Ok(output) => output,
            Err(ExecError::Cancelled) => {
                return ToolOutput::failure(
                    "[turn interrupted by user; custom tool killed and reaped]",
                );
            }
            Err(error) => {
                return ToolOutput::failure(format!("custom tool could not run: {error}"));
            }
        };
        // The staging directory is owner-private and remains alive until process teardown.
        drop(staged);
        if let Some(denial) = output.denial {
            // Manifest restrictions are an invariant, not an approval boundary. Advertising a
            // network/write denial for a capability this tool explicitly disabled would make the
            // generic turn runner offer an escalation, only for this method to reapply the same
            // restriction on retry. Preserve classifications only for capabilities the manifest
            // actually permits.
            let retryable_denial = match denial {
                grokforge_protocol::DenialClass::Network if !self.network => None,
                grokforge_protocol::DenialClass::FsWrite if !self.spec.mutating => None,
                other => Some(other),
            };
            return ToolOutput::Failure {
                error: format!("custom tool was blocked by the sandbox: {}", output.stderr),
                denial: retryable_denial,
            };
        }
        let output_bytes = output.stdout.len().saturating_add(output.stderr.len());
        if output.truncated || output_bytes > self.max_output_bytes {
            return ToolOutput::failure(format!(
                "custom tool output exceeded its {} byte limit",
                self.max_output_bytes
            ));
        }
        if !output.succeeded() {
            return ToolOutput::failure(format!(
                "custom tool exited with status {:?}: {}",
                output.exit_code,
                output.stderr.trim()
            ));
        }
        let response: CustomToolResponse = match serde_json::from_str(output.stdout.trim()) {
            Ok(response) => response,
            Err(error) => {
                return ToolOutput::failure(format!(
                    "custom tool returned invalid v1 JSON on stdout: {error}"
                ));
            }
        };
        if response.protocol_version != MANIFEST_VERSION {
            return ToolOutput::failure(format!(
                "custom tool returned protocol_version {}; expected {MANIFEST_VERSION}",
                response.protocol_version
            ));
        }
        match (response.ok, response.content, response.error) {
            (true, Some(content), None) => ToolOutput::success(content),
            (false, None, Some(error)) => ToolOutput::failure(error),
            _ => ToolOutput::failure(
                "custom tool response must contain exactly `content` when ok=true or `error` when ok=false",
            ),
        }
    }
}

#[derive(Serialize)]
struct CustomToolInput<'a> {
    protocol_version: u32,
    call_id: String,
    workspace_root: String,
    arguments: &'a serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomToolResponse {
    protocol_version: u32,
    ok: bool,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomToolError {
    #[error("manifest must be a regular, non-symlink file")]
    UnsafeManifest,
    #[error("owner manifest permissions are unsafe: {0}")]
    UnsafePermissions(String),
    #[error("could not read manifest: {0}")]
    Read(#[from] std::io::Error),
    #[error("manifest exceeds {MAX_CUSTOM_MANIFEST_BYTES} bytes")]
    ManifestTooLarge,
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid custom tool `{name}`: {reason}")]
    InvalidTool { name: String, reason: String },
}

fn invalid_tool(name: String, reason: impl Into<String>) -> CustomToolError {
    CustomToolError::InvalidTool {
        name,
        reason: reason.into(),
    }
}

fn read_manifest(path: &Path, trust: ManifestTrust) -> Result<Option<String>, CustomToolError> {
    let (mut file, metadata, parent_metadata) = match open_manifest_descriptor(path) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CustomToolError::Read(error)),
    };
    if !metadata.is_file() {
        return Err(CustomToolError::UnsafeManifest);
    }
    if metadata.len() > MAX_CUSTOM_MANIFEST_BYTES {
        return Err(CustomToolError::ManifestTooLarge);
    }
    if let Some(parent) = path.parent() {
        if !parent_metadata.is_dir() {
            return Err(CustomToolError::UnsafeManifest);
        }
        if matches!(trust, ManifestTrust::Owner) {
            validate_owner_permissions(parent, &parent_metadata, true)?;
        }
    }
    if matches!(trust, ManifestTrust::Owner) {
        validate_owner_permissions(path, &metadata, false)?;
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CUSTOM_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CUSTOM_MANIFEST_BYTES {
        return Err(CustomToolError::ManifestTooLarge);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| CustomToolError::InvalidManifest(error.to_string()))
}

#[cfg(unix)]
fn open_manifest_descriptor(
    path: &Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata, std::fs::Metadata)> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("manifest path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("manifest path has no file name"))?;
    // Hold the directory open while resolving the final component. O_NOFOLLOW closes both the
    // directory and file swap-to-symlink races; fstat via `metadata()` validates the objects the
    // descriptors actually refer to, rather than paths checked earlier.
    let directory = std::fs::File::from(
        open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let parent_metadata = directory.metadata()?;
    let file = std::fs::File::from(
        openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let metadata = file.metadata()?;
    Ok((file, metadata, parent_metadata))
}

#[cfg(not(unix))]
fn open_manifest_descriptor(
    path: &Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata, std::fs::Metadata)> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("manifest path has no parent"))?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if parent_metadata.file_type().is_symlink() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("manifest path is a symlink"));
    }
    let file = std::fs::File::open(path)?;
    Ok((file, metadata, parent_metadata))
}

#[cfg(unix)]
fn validate_owner_permissions(
    path: &Path,
    metadata: &std::fs::Metadata,
    directory: bool,
) -> Result<(), CustomToolError> {
    use std::os::unix::fs::MetadataExt as _;

    let current = rustix::process::geteuid().as_raw();
    if metadata.uid() != current {
        return Err(CustomToolError::UnsafePermissions(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    if !directory && metadata.nlink() != 1 {
        return Err(CustomToolError::UnsafePermissions(format!(
            "{} must have exactly one hard link",
            path.display()
        )));
    }
    let forbidden = 0o077;
    if metadata.mode() & forbidden != 0 {
        return Err(CustomToolError::UnsafePermissions(format!(
            "{} must not be accessible by group or other users",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner_permissions(
    path: &Path,
    _metadata: &std::fs::Metadata,
    _directory: bool,
) -> Result<(), CustomToolError> {
    Err(CustomToolError::UnsafePermissions(format!(
        "owner/ACL validation for {} is not implemented on this platform; refusing owner custom tools",
        path.display()
    )))
}

fn validate_name(name: &str) -> Result<(), CustomToolError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte == b'_' || byte.is_ascii_alphabetic()
            } else {
                byte == b'_' || byte == b'-' || byte.is_ascii_alphanumeric()
            }
        })
    {
        return Err(invalid_tool(
            name.to_string(),
            "name must be 1..=64 bytes, start with a letter or `_`, and contain only ASCII letters, digits, `_`, or `-`",
        ));
    }
    if Redactor::apply(name).count != 0 {
        return Err(invalid_tool(
            name.to_string(),
            "name appears to contain a secret",
        ));
    }
    Ok(())
}

fn validate_args(name: &str, args: &[String]) -> Result<(), CustomToolError> {
    if args.len() > MAX_CUSTOM_ARGS {
        return Err(invalid_tool(
            name.to_string(),
            format!("at most {MAX_CUSTOM_ARGS} arguments are allowed"),
        ));
    }
    let mut total = 0_usize;
    for argument in args {
        if argument.len() > MAX_ARG_BYTES || argument.contains('\0') {
            return Err(invalid_tool(
                name.to_string(),
                format!("each argument must be at most {MAX_ARG_BYTES} bytes and contain no NUL"),
            ));
        }
        if Redactor::apply(argument).count != 0 {
            return Err(invalid_tool(
                name.to_string(),
                "static arguments must not contain credentials",
            ));
        }
        total = total.saturating_add(argument.len());
    }
    if total > MAX_ARGS_BYTES {
        return Err(invalid_tool(
            name.to_string(),
            format!("argument data may total at most {MAX_ARGS_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn validate_env_allowlist(name: &str, environment: &[String]) -> Result<(), CustomToolError> {
    if environment.len() > MAX_CUSTOM_ENV {
        return Err(invalid_tool(
            name.to_string(),
            format!("at most {MAX_CUSTOM_ENV} environment names are allowed"),
        ));
    }
    let mut unique = BTreeSet::new();
    for variable in environment {
        if variable.is_empty()
            || variable.len() > MAX_ENV_NAME_BYTES
            || !variable.bytes().enumerate().all(|(index, byte)| {
                byte == b'_'
                    || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
            })
        {
            return Err(invalid_tool(
                name.to_string(),
                format!("invalid environment name `{variable}`"),
            ));
        }
        let upper = variable.to_ascii_uppercase();
        if is_sensitive_environment_name(&upper) || is_reserved_environment_name(&upper) {
            return Err(invalid_tool(
                name.to_string(),
                format!(
                    "environment name `{variable}` may carry credentials or affect the sandbox wrapper and is forbidden"
                ),
            ));
        }
        if !unique.insert(upper) {
            return Err(invalid_tool(
                name.to_string(),
                format!("duplicate environment name `{variable}`"),
            ));
        }
    }
    Ok(())
}

fn is_sensitive_environment_name(upper: &str) -> bool {
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "COOKIE",
        "AUTH",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
        || [
            "AWS_",
            "AZURE_",
            "GOOGLE_",
            "GITHUB_",
            "GITLAB_",
            "XAI_",
            "OPENAI_",
            "ANTHROPIC_",
            "SSH_",
            "KRB5",
            "DOCKER_CONFIG",
            "NPM_CONFIG",
        ]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
}

fn is_reserved_environment_name(upper: &str) -> bool {
    matches!(
        upper,
        "PATH"
            | "HOME"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
            | "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "CDPATH"
            | "GLOBIGNORE"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5LIB"
            | "RUBYLIB"
            | "RUBYOPT"
            | "NODE_OPTIONS"
            | "RUSTC_WRAPPER"
    ) || upper.starts_with("LC_")
        || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("GIT_")
}

fn validate_executable(
    name: &str,
    command: &str,
    expected_hash: &str,
) -> Result<PathBuf, CustomToolError> {
    if command.is_empty()
        || command.len() > MAX_COMMAND_BYTES
        || command.contains('\0')
        || !Path::new(command).is_absolute()
    {
        return Err(invalid_tool(
            name.to_string(),
            format!("command must be an absolute path of at most {MAX_COMMAND_BYTES} bytes"),
        ));
    }
    if expected_hash.len() != 64 || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_tool(
            name.to_string(),
            "sha256 must contain exactly 64 hexadecimal characters",
        ));
    }
    let command = std::fs::canonicalize(command).map_err(|error| {
        invalid_tool(name.to_string(), format!("cannot resolve command: {error}"))
    })?;
    verify_executable_hash(&command, &expected_hash.to_ascii_lowercase())
        .map_err(|error| invalid_tool(name.to_string(), error.to_string()))?;
    Ok(command)
}

fn verify_executable_hash(command: &Path, expected_hash: &str) -> Result<(), CustomToolError> {
    drop(stage_verified_executable(command, expected_hash)?);
    Ok(())
}

#[derive(Debug)]
struct StagedExecutable {
    directory: tempfile::TempDir,
    path: PathBuf,
}

impl StagedExecutable {
    fn private_read_root(&self) -> PathBuf {
        self.directory.path().to_path_buf()
    }
}

fn stage_verified_executable(
    command: &Path,
    expected_hash: &str,
) -> Result<StagedExecutable, CustomToolError> {
    use std::io::Write as _;

    let mut source = open_executable_descriptor(command)?;
    let metadata = source.metadata()?;
    if !metadata.is_file() {
        return Err(CustomToolError::InvalidManifest(
            "custom tool command is not a regular, non-symlink file".into(),
        ));
    }
    if metadata.len() > MAX_CUSTOM_EXECUTABLE_BYTES {
        return Err(CustomToolError::InvalidManifest(format!(
            "custom tool executable exceeds {MAX_CUSTOM_EXECUTABLE_BYTES} bytes"
        )));
    }
    validate_executable_permissions(command, &metadata)?;
    let directory = tempfile::Builder::new()
        .prefix(".grokforge-custom-")
        .tempdir()?;
    #[cfg(unix)]
    std::fs::set_permissions(
        directory.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let path = directory.path().join("tool");
    let mut target = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_CUSTOM_EXECUTABLE_BYTES {
            return Err(CustomToolError::InvalidManifest(format!(
                "custom tool executable exceeds {MAX_CUSTOM_EXECUTABLE_BYTES} bytes"
            )));
        }
        hasher.update(&buffer[..read]);
        target.write_all(&buffer[..read])?;
    }
    target.flush()?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_hash {
        return Err(CustomToolError::InvalidManifest(format!(
            "custom tool executable hash changed (expected {expected_hash}, got {actual})"
        )));
    }
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o500))?;
    Ok(StagedExecutable { directory, path })
}

#[cfg(unix)]
fn open_executable_descriptor(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, open};

    open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(std::fs::File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn open_executable_descriptor(path: &Path) -> std::io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("executable path is a symlink"));
    }
    std::fs::File::open(path)
}

#[cfg(unix)]
fn validate_executable_permissions(
    command: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), CustomToolError> {
    use std::os::unix::fs::MetadataExt as _;

    let current = rustix::process::geteuid().as_raw();
    if metadata.uid() != current && metadata.uid() != 0 {
        return Err(CustomToolError::InvalidManifest(format!(
            "custom tool executable {} must be owned by the current user or root",
            command.display()
        )));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(CustomToolError::InvalidManifest(format!(
            "custom tool executable {} must not be group/world writable",
            command.display()
        )));
    }
    if metadata.mode() & 0o111 == 0 {
        return Err(CustomToolError::InvalidManifest(format!(
            "custom tool executable {} is not executable",
            command.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_executable_permissions(
    _command: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), CustomToolError> {
    Err(CustomToolError::InvalidManifest(
        "custom tool executable ownership and permission validation is unavailable on this platform; use a supported Unix host or WSL2"
            .to_string(),
    ))
}

fn validate_schema_shape(name: &str, schema: &serde_json::Value) -> Result<(), CustomToolError> {
    if !schema.is_object()
        || schema.get("type").and_then(serde_json::Value::as_str) != Some("object")
    {
        return Err(invalid_tool(
            name.to_string(),
            "input_schema root must be a JSON Schema object with type=object",
        ));
    }
    let mut stack = vec![(schema, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH {
            return Err(invalid_tool(
                name.to_string(),
                format!(
                    "input_schema exceeds depth {MAX_SCHEMA_DEPTH} or {MAX_SCHEMA_NODES} nodes"
                ),
            ));
        }
        match value {
            serde_json::Value::Object(object) => {
                if object.contains_key("$ref") || object.contains_key("$dynamicRef") {
                    return Err(invalid_tool(
                        name.to_string(),
                        "input_schema references are disabled in custom-tool v1",
                    ));
                }
                stack.extend(
                    object
                        .values()
                        .map(|child| (child, depth.saturating_add(1))),
                );
            }
            serde_json::Value::Array(array) => {
                stack.extend(array.iter().map(|child| (child, depth.saturating_add(1))));
            }
            _ => {}
        }
    }
    Ok(())
}

fn redact_json_strings(value: &serde_json::Value) -> (serde_json::Value, usize) {
    match value {
        serde_json::Value::String(text) => {
            let redacted = Redactor::apply(text);
            (serde_json::Value::String(redacted.text), redacted.count)
        }
        serde_json::Value::Array(values) => {
            let mut count = 0_usize;
            let values = values
                .iter()
                .map(|value| {
                    let (value, redactions) = redact_json_strings(value);
                    count = count.saturating_add(redactions);
                    value
                })
                .collect();
            (serde_json::Value::Array(values), count)
        }
        serde_json::Value::Object(values) => {
            let mut count = 0_usize;
            let mut redacted = serde_json::Map::new();
            for (key, value) in values {
                let redacted_key = Redactor::apply(key);
                let (value, value_redactions) = redact_json_strings(value);
                count = count
                    .saturating_add(redacted_key.count)
                    .saturating_add(value_redactions);
                redacted.insert(redacted_key.text, value);
            }
            (serde_json::Value::Object(redacted), count)
        }
        scalar => (scalar.clone(), 0),
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use grokforge_protocol::{DenialClass, SandboxPolicy, ToolCallId};
    use grokforge_sandbox::{ExecOutput, SandboxCapability, SandboxRunner};
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn executable(dir: &Path) -> (PathBuf, String) {
        let path = dir.join("custom-tool");
        std::fs::write(
            &path,
            b"#!/bin/sh\nprintf '%s' '{\"protocol_version\":1,\"ok\":true,\"content\":\"ok\"}'\n",
        )
        .expect("write executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("executable permissions");
        let hash = {
            let bytes = std::fs::read(&path).expect("read executable");
            format!("{:x}", Sha256::digest(bytes))
        };
        (path, hash)
    }

    fn manifest(command: &Path, hash: &str) -> String {
        format!(
            r#"version = 1

[[tool]]
name = "owner_echo"
description = "Echo a value"
command = "{}"
sha256 = "{hash}"
args = []
input_schema = {{ type = "object", properties = {{ value = {{ type = "string" }} }}, required = ["value"] }}
mutating = false
parallel_safe = true
network = false
env_allowlist = ["GROKFORGE_CUSTOM_TEST_LABEL"]
timeout_ms = 1000
max_output_bytes = 4096
"#,
            command.display()
        )
    }

    #[test]
    fn secure_owner_manifest_registers_hashed_tool() {
        let root = tempfile::tempdir().expect("root");
        let owner = root.path().join("owner");
        std::fs::create_dir(&owner).expect("owner dir");
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o700))
            .expect("owner permissions");
        let (command, hash) = executable(root.path());
        let path = owner.join("tools.toml");
        std::fs::write(&path, manifest(&command, &hash)).expect("manifest");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("manifest permissions");

        let tools = load_manifest(&path, ManifestTrust::Owner)
            .expect("load")
            .expect("present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec.name, "owner_echo");
        assert!(!tools[0].spec.mutating);
        assert!(tools[0].spec.parallel_safe);
    }

    #[test]
    fn owner_manifest_rejects_loose_permissions_and_changed_executable() {
        let root = tempfile::tempdir().expect("root");
        let owner = root.path().join("owner");
        std::fs::create_dir(&owner).expect("owner dir");
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o700))
            .expect("owner permissions");
        let (command, hash) = executable(root.path());
        let path = owner.join("tools.toml");
        std::fs::write(&path, manifest(&command, &hash)).expect("manifest");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loose permissions");
        assert!(matches!(
            load_manifest(&path, ManifestTrust::Owner),
            Err(CustomToolError::UnsafePermissions(_))
        ));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secure permissions");
        std::fs::write(&command, b"#!/bin/sh\nexit 1\n").expect("replace executable");
        assert!(matches!(
            load_manifest(&path, ManifestTrust::Owner),
            Err(CustomToolError::InvalidTool { .. })
        ));
    }

    #[test]
    fn owner_manifest_rejects_hardlink_aliases() {
        let root = tempfile::tempdir().expect("root");
        let owner = root.path().join("owner");
        std::fs::create_dir(&owner).expect("owner dir");
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o700))
            .expect("owner permissions");
        let (command, hash) = executable(root.path());
        let path = owner.join("tools.toml");
        std::fs::write(&path, manifest(&command, &hash)).expect("manifest");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("manifest permissions");
        std::fs::hard_link(&path, owner.join("manifest-alias")).expect("hardlink");
        assert!(matches!(
            load_manifest(&path, ManifestTrust::Owner),
            Err(CustomToolError::UnsafePermissions(_))
        ));
    }

    #[test]
    fn manifest_rejects_secret_environment_and_external_schema_refs() {
        let root = tempfile::tempdir().expect("root");
        let (command, hash) = executable(root.path());
        let secret_env =
            manifest(&command, &hash).replace("GROKFORGE_CUSTOM_TEST_LABEL", "XAI_API_KEY");
        let path = root.path().join("tools.toml");
        std::fs::write(&path, secret_env).expect("manifest");
        assert!(matches!(
            load_manifest(&path, ManifestTrust::TrustedProject),
            Err(CustomToolError::InvalidTool { .. })
        ));

        let schema_ref = manifest(&command, &hash)
            .lines()
            .map(|line| {
                if line.starts_with("input_schema =") {
                    "input_schema = { type = \"object\", properties = { value = { \"$ref\" = \"https://example.invalid/schema\" } } }"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, schema_ref).expect("manifest");
        assert!(matches!(
            load_manifest(&path, ManifestTrust::TrustedProject),
            Err(CustomToolError::InvalidTool { .. })
        ));
    }

    #[test]
    fn duplicate_or_builtin_names_do_not_replace_registered_tools() {
        let root = tempfile::tempdir().expect("root");
        let (command, hash) = executable(root.path());
        let path = root.path().join("tools.toml");
        std::fs::write(&path, manifest(&command, &hash)).expect("manifest");
        let mut registry = ToolRegistry::with_builtins();
        let mut report = CustomToolLoadReport::default();
        register_manifest(
            &path,
            ManifestTrust::TrustedProject,
            &mut registry,
            &mut report,
        );
        assert_eq!(report.registered, ["owner_echo"]);

        let replacement = manifest(&command, &hash).replace("owner_echo", "read_file");
        std::fs::write(&path, replacement).expect("manifest");
        register_manifest(
            &path,
            ManifestTrust::TrustedProject,
            &mut registry,
            &mut report,
        );
        assert_eq!(
            registry.get("read_file").expect("builtin").spec().name,
            "read_file"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("already registered"))
        );
    }

    #[derive(Debug)]
    struct ReplacingRunner {
        source: PathBuf,
        replacement: Vec<u8>,
        observed: Mutex<Vec<u8>>,
    }

    #[async_trait]
    impl SandboxRunner for ReplacingRunner {
        fn capability(&self) -> SandboxCapability {
            SandboxCapability {
                backend: "test".into(),
                enforced: true,
                notes: Vec::new(),
            }
        }

        async fn run(
            &self,
            policy: &SandboxPolicy,
            command: &CommandSpec,
        ) -> Result<ExecOutput, ExecError> {
            assert_eq!(policy.mode, SandboxMode::ReadOnly);
            assert_eq!(policy.network, NetworkMode::Isolated);
            std::fs::write(&self.source, &self.replacement).expect("replace source after staging");
            let staged = std::fs::read(&command.program).expect("read staged executable");
            *self.observed.lock().expect("observed lock") = staged;
            Ok(ExecOutput {
                exit_code: Some(0),
                stdout: r#"{"protocol_version":1,"ok":true,"content":"staged"}"#.into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                denial: None,
            })
        }
    }

    #[tokio::test]
    async fn source_replacement_after_staging_cannot_change_executed_bytes() {
        let root = tempfile::tempdir().expect("root");
        let (source, hash) = executable(root.path());
        let original = std::fs::read(&source).expect("original bytes");
        let path = root.path().join("tools.toml");
        std::fs::write(&path, manifest(&source, &hash)).expect("manifest");
        let tool = load_manifest(&path, ManifestTrust::TrustedProject)
            .expect("load")
            .expect("present")
            .remove(0);
        let runner = Arc::new(ReplacingRunner {
            source: source.clone(),
            replacement: b"#!/bin/sh\nprintf malicious\n".to_vec(),
            observed: Mutex::new(Vec::new()),
        });
        let ctx = TurnContext {
            workspace_root: root.path().to_path_buf(),
            policy: SandboxPolicy::workspace_write(root.path()),
            sandbox: runner.clone(),
            touched: Arc::new(Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: crate::TurnCancellation::new(),
        };
        let output = tool
            .invoke(ToolInvocation {
                call_id: ToolCallId::new(),
                args: serde_json::json!({"value": "hello"}),
                ctx: &ctx,
            })
            .await;
        assert_eq!(output, ToolOutput::success("staged"));
        assert_eq!(*runner.observed.lock().expect("observed lock"), original);
        assert_ne!(std::fs::read(source).expect("replacement"), original);
    }

    #[derive(Debug)]
    struct DenialRunner(DenialClass);

    #[async_trait]
    impl SandboxRunner for DenialRunner {
        fn capability(&self) -> SandboxCapability {
            SandboxCapability {
                backend: "test".into(),
                enforced: true,
                notes: Vec::new(),
            }
        }

        async fn run(
            &self,
            _policy: &SandboxPolicy,
            _command: &CommandSpec,
        ) -> Result<ExecOutput, ExecError> {
            Ok(ExecOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "denied by test sandbox".into(),
                truncated: false,
                timed_out: false,
                denial: Some(self.0),
            })
        }
    }

    async fn invoke_with_denial(
        root: &Path,
        tool: &CustomExecutableTool,
        denial: DenialClass,
    ) -> ToolOutput {
        let ctx = TurnContext {
            workspace_root: root.to_path_buf(),
            policy: SandboxPolicy::workspace_write(root),
            sandbox: Arc::new(DenialRunner(denial)),
            touched: Arc::new(Mutex::new(Vec::new())),
            bound_write_targets: Vec::new(),
            cancellation: crate::TurnCancellation::new(),
        };
        tool.invoke(ToolInvocation {
            call_id: ToolCallId::new(),
            args: serde_json::json!({"value": "hello"}),
            ctx: &ctx,
        })
        .await
    }

    fn output_denial(output: &ToolOutput) -> Option<DenialClass> {
        match output {
            ToolOutput::Failure { denial, .. } => *denial,
            ToolOutput::Success { .. } => None,
        }
    }

    #[tokio::test]
    async fn manifest_forbidden_capabilities_do_not_offer_impossible_escalation() {
        let root = tempfile::tempdir().expect("root");
        let (source, hash) = executable(root.path());
        let path = root.path().join("tools.toml");
        std::fs::write(&path, manifest(&source, &hash)).expect("manifest");
        let tool = load_manifest(&path, ManifestTrust::TrustedProject)
            .expect("load")
            .expect("present")
            .remove(0);

        let network = invoke_with_denial(root.path(), &tool, DenialClass::Network).await;
        let write = invoke_with_denial(root.path(), &tool, DenialClass::FsWrite).await;

        assert!(network.is_error());
        assert!(write.is_error());
        assert_eq!(output_denial(&network), None);
        assert_eq!(output_denial(&write), None);
    }

    #[tokio::test]
    async fn manifest_allowed_capabilities_preserve_retryable_denials() {
        let root = tempfile::tempdir().expect("root");
        let (source, hash) = executable(root.path());
        let path = root.path().join("tools.toml");
        let declaration = manifest(&source, &hash)
            .replace("mutating = false", "mutating = true")
            .replace("network = false", "network = true");
        std::fs::write(&path, declaration).expect("manifest");
        let tool = load_manifest(&path, ManifestTrust::TrustedProject)
            .expect("load")
            .expect("present")
            .remove(0);

        let network = invoke_with_denial(root.path(), &tool, DenialClass::Network).await;
        let write = invoke_with_denial(root.path(), &tool, DenialClass::FsWrite).await;

        assert_eq!(output_denial(&network), Some(DenialClass::Network));
        assert_eq!(output_denial(&write), Some(DenialClass::FsWrite));
    }
}
