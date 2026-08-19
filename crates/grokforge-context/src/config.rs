use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use directories::BaseDirs;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_LANGUAGES: usize = 64;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_COMMAND_BYTES: usize = 1024;
const MAX_INITIALIZATION_OPTIONS_BYTES: usize = 64 * 1024;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_DIAGNOSTIC_WAIT_MS: u64 = 2_000;
const MAX_DIAGNOSTIC_WAIT_MS: u64 = 10_000;

/// Code-intelligence failures are kept separate from process execution failures. The latter are
/// produced by `grokforge-sandbox` after a prepared command crosses the core approval boundary.
#[derive(Debug, Error)]
pub enum CodeIntelligenceError {
    #[error("cannot locate the home directory for ~/.grokforge/code-intelligence.toml")]
    NoHomeDirectory,
    #[error("unsafe code-intelligence config {path}: {reason}")]
    UnsafeConfig { path: PathBuf, reason: &'static str },
    #[error("cannot read code-intelligence config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid code-intelligence TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid code-intelligence config: {0}")]
    Validation(String),
    #[error("no configured language matches `{0}`")]
    UnsupportedLanguage(PathBuf),
    #[error("language `{language}` has no language server configured")]
    NoLanguageServer { language: String },
    #[error("language `{language}` has no formatter configured")]
    NoFormatter { language: String },
    #[error("cannot create file URI for `{0}`")]
    InvalidFileUri(PathBuf),
    #[error("invalid language-server response: {0}")]
    InvalidLspResponse(String),
}

/// Exact executable plus arguments. No shell is involved and placeholders are expanded before
/// the command is shown for approval.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// A language-server command speaking JSON-RPC over stdio.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspConfig {
    #[serde(flatten)]
    pub process: ProcessConfig,
    #[serde(default)]
    pub initialization_options: Option<Value>,
    /// Grace period after `didOpen` during which the server can publish asynchronous diagnostics
    /// before GrokForge closes stdin and reaps the one-shot server.
    #[serde(default = "default_diagnostic_wait_ms")]
    pub diagnostic_wait_ms: u64,
}

/// A formatter command that edits `{file}` in place. Core substitutes a private copy for that
/// placeholder; the formatter never receives write access to the real workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatterConfig {
    #[serde(flatten)]
    pub process: ProcessConfig,
}

/// Code-intelligence settings for one or more file extensions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageConfig {
    pub name: String,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub language_id: Option<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub lsp: Option<LspConfig>,
    #[serde(default)]
    pub formatter: Option<FormatterConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    extend_defaults: bool,
    language: Vec<LanguageConfig>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            extend_defaults: true,
            language: Vec::new(),
        }
    }
}

/// Fully resolved settings. Owner entries are searched before conservative built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeIntelligenceConfig {
    languages: Vec<ConfiguredLanguage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanguageSource {
    BuiltIn,
    Owner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredLanguage {
    language: LanguageConfig,
    source: LanguageSource,
}

/// A language-server process ready for the sandbox runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLanguageServer {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub language_id: String,
    pub initialization_options: Option<Value>,
    pub diagnostic_wait: Duration,
    pub(crate) canonical_program: PathBuf,
}

/// A formatter process ready for the sandbox runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFormatter {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub language: String,
    pub(crate) canonical_program: PathBuf,
}

impl PreparedLanguageServer {
    /// Re-check that the absolute invocation path still resolves to the executable verified while
    /// preparing this command. The invocation path itself is preserved for multicall proxies
    /// (for example rustup's `rust-analyzer` shim), while its target remains pinned.
    pub fn reverify_executable(&self) -> Result<(), CodeIntelligenceError> {
        reverify_executable(&self.program, &self.canonical_program)
    }
}

impl PreparedFormatter {
    /// Re-check the prepared executable immediately before it crosses the sandbox boundary.
    pub fn reverify_executable(&self) -> Result<(), CodeIntelligenceError> {
        reverify_executable(&self.program, &self.canonical_program)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedExecutable {
    invocation_path: String,
    canonical_target: PathBuf,
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

const fn default_diagnostic_wait_ms() -> u64 {
    DEFAULT_DIAGNOSTIC_WAIT_MS
}

impl CodeIntelligenceConfig {
    /// Load the owner-controlled `~/.grokforge/code-intelligence.toml`, or built-in defaults when
    /// it does not exist. The owner directory and file receive the same no-symlink, ownership,
    /// hard-link, and permission checks as credential-adjacent GrokForge configuration.
    pub fn load_global() -> Result<Self, CodeIntelligenceError> {
        let base = BaseDirs::new().ok_or(CodeIntelligenceError::NoHomeDirectory)?;
        let path = base.home_dir().join(".grokforge/code-intelligence.toml");
        match read_secure_config(&path)? {
            Some(text) => Self::from_toml(&text),
            None => Ok(Self::default()),
        }
    }

    /// Parse an explicit configuration string. This is also useful to frontends that keep their
    /// own trusted configuration store.
    pub fn from_toml(text: &str) -> Result<Self, CodeIntelligenceError> {
        let file: ConfigFile = toml::from_str(text)?;
        validate_languages(&file.language, true)?;
        let mut languages: Vec<ConfiguredLanguage> = file
            .language
            .into_iter()
            .map(|language| ConfiguredLanguage {
                language,
                source: LanguageSource::Owner,
            })
            .collect();
        if file.extend_defaults {
            languages.extend(
                default_languages()
                    .into_iter()
                    .map(|language| ConfiguredLanguage {
                        language,
                        source: LanguageSource::BuiltIn,
                    }),
            );
        }
        Ok(Self { languages })
    }

    /// Built-in settings for common local toolchains. Missing executables fail clearly at spawn;
    /// GrokForge never downloads or runs an installer implicitly.
    #[must_use]
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Resolve a language server for `file`. Both paths must already be canonical and `file` must
    /// be inside `workspace`; the core path-safety layer establishes that invariant.
    pub fn prepare_language_server(
        &self,
        workspace: &Path,
        file: &Path,
    ) -> Result<PreparedLanguageServer, CodeIntelligenceError> {
        let language = self
            .languages
            .iter()
            .find(|entry| entry.language.matches(file) && entry.language.lsp.is_some())
            .ok_or_else(|| self.missing_capability(file, true))?;
        let source = language.source;
        let language = &language.language;
        let lsp = language
            .lsp
            .as_ref()
            .ok_or_else(|| CodeIntelligenceError::NoLanguageServer {
                language: language.name.clone(),
            })?;
        let root = language.project_root(workspace, file);
        let (program, args) = lsp.process.expand(workspace, &root, file, file)?;
        let executable = resolve_executable(source, workspace, &program)?;
        Ok(PreparedLanguageServer {
            program: executable.invocation_path,
            args,
            cwd: root,
            timeout: Duration::from_millis(lsp.process.timeout_ms),
            language_id: language
                .language_id
                .clone()
                .unwrap_or_else(|| language.name.clone()),
            initialization_options: lsp.initialization_options.clone(),
            diagnostic_wait: Duration::from_millis(lsp.diagnostic_wait_ms),
            canonical_program: executable.canonical_target,
        })
    }

    /// Resolve the configured formatter for `file`. This form is intended for approval display;
    /// execution should use [`Self::prepare_formatter_for_copy`] so `{file}` is private scratch.
    pub fn prepare_formatter(
        &self,
        workspace: &Path,
        file: &Path,
    ) -> Result<PreparedFormatter, CodeIntelligenceError> {
        let language = self
            .languages
            .iter()
            .find(|entry| entry.language.matches(file) && entry.language.formatter.is_some())
            .ok_or_else(|| self.missing_capability(file, false))?;
        let source = language.source;
        let language = &language.language;
        let formatter =
            language
                .formatter
                .as_ref()
                .ok_or_else(|| CodeIntelligenceError::NoFormatter {
                    language: language.name.clone(),
                })?;
        let root = language.project_root(workspace, file);
        let (program, args) = formatter.process.expand(workspace, &root, file, file)?;
        let executable = resolve_executable(source, workspace, &program)?;
        Ok(PreparedFormatter {
            program: executable.invocation_path,
            args,
            cwd: root,
            timeout: Duration::from_millis(formatter.process.timeout_ms),
            language: language.name.clone(),
            canonical_program: executable.canonical_target,
        })
    }

    /// Resolve the formatter selected by `source_file`, while substituting a private copy for
    /// `{file}`. This lets the core give the formatter a writable scratch directory and keep the
    /// real workspace read-only until a descriptor-safe host replacement.
    pub fn prepare_formatter_for_copy(
        &self,
        workspace: &Path,
        source_file: &Path,
        private_copy: &Path,
    ) -> Result<PreparedFormatter, CodeIntelligenceError> {
        let language = self
            .languages
            .iter()
            .find(|entry| entry.language.matches(source_file) && entry.language.formatter.is_some())
            .ok_or_else(|| self.missing_capability(source_file, false))?;
        let source = language.source;
        let language = &language.language;
        let formatter =
            language
                .formatter
                .as_ref()
                .ok_or_else(|| CodeIntelligenceError::NoFormatter {
                    language: language.name.clone(),
                })?;
        let root = language.project_root(workspace, source_file);
        let (program, args) =
            formatter
                .process
                .expand(workspace, &root, private_copy, source_file)?;
        let executable = resolve_executable(source, workspace, &program)?;
        Ok(PreparedFormatter {
            program: executable.invocation_path,
            args,
            cwd: root,
            timeout: Duration::from_millis(formatter.process.timeout_ms),
            language: language.name.clone(),
            canonical_program: executable.canonical_target,
        })
    }

    fn missing_capability(&self, file: &Path, lsp: bool) -> CodeIntelligenceError {
        self.languages
            .iter()
            .find(|entry| entry.language.matches(file))
            .map_or_else(
                || CodeIntelligenceError::UnsupportedLanguage(file.to_path_buf()),
                |entry| {
                    let language = &entry.language;
                    if lsp {
                        CodeIntelligenceError::NoLanguageServer {
                            language: language.name.clone(),
                        }
                    } else {
                        CodeIntelligenceError::NoFormatter {
                            language: language.name.clone(),
                        }
                    }
                },
            )
    }
}

impl Default for CodeIntelligenceConfig {
    fn default() -> Self {
        Self {
            languages: default_languages()
                .into_iter()
                .map(|language| ConfiguredLanguage {
                    language,
                    source: LanguageSource::BuiltIn,
                })
                .collect(),
        }
    }
}

impl LanguageConfig {
    fn matches(&self, file: &Path) -> bool {
        let Some(extension) = file.extension().and_then(std::ffi::OsStr::to_str) else {
            return false;
        };
        self.extensions
            .iter()
            .any(|candidate| normalize_extension(candidate) == extension.to_ascii_lowercase())
    }

    fn project_root(&self, workspace: &Path, file: &Path) -> PathBuf {
        let start = file.parent().unwrap_or(workspace);
        for directory in start.ancestors() {
            if !directory.starts_with(workspace) {
                break;
            }
            if self.root_markers.iter().any(|marker| {
                std::fs::symlink_metadata(directory.join(marker))
                    .is_ok_and(|metadata| !metadata.file_type().is_symlink())
            }) {
                return directory.to_path_buf();
            }
            if directory == workspace {
                break;
            }
        }
        workspace.to_path_buf()
    }
}

impl ProcessConfig {
    fn expand(
        &self,
        workspace: &Path,
        root: &Path,
        file_argument: &Path,
        relative_source: &Path,
    ) -> Result<(String, Vec<String>), CodeIntelligenceError> {
        validate_process(self)?;
        let relative = relative_source.strip_prefix(workspace).map_err(|_| {
            CodeIntelligenceError::Validation(format!(
                "file {} is outside workspace {}",
                relative_source.display(),
                workspace.display()
            ))
        })?;
        let expand = |value: &str| {
            value
                .replace("{file_relative}", &relative.to_string_lossy())
                .replace("{file}", &file_argument.to_string_lossy())
                .replace("{workspace}", &workspace.to_string_lossy())
                .replace("{root}", &root.to_string_lossy())
        };
        let program = expand(&self.command);
        let args: Vec<String> = self.args.iter().map(|arg| expand(arg)).collect();
        validate_text("expanded process.command", &program, MAX_COMMAND_BYTES)?;
        if args.iter().any(|argument| {
            argument.len() > MAX_ARGUMENT_BYTES || argument.chars().any(char::is_control)
        }) {
            return Err(CodeIntelligenceError::Validation(format!(
                "expanded process arguments must be at most {MAX_ARGUMENT_BYTES} bytes and contain no control characters"
            )));
        }
        Ok((program, args))
    }
}

fn validate_languages(
    languages: &[LanguageConfig],
    require_absolute_commands: bool,
) -> Result<(), CodeIntelligenceError> {
    if languages.len() > MAX_LANGUAGES {
        return Err(CodeIntelligenceError::Validation(format!(
            "at most {MAX_LANGUAGES} language entries are allowed"
        )));
    }
    let mut names = BTreeSet::new();
    for language in languages {
        validate_text("language.name", &language.name, 128)?;
        if !names.insert(language.name.clone()) {
            return Err(CodeIntelligenceError::Validation(format!(
                "duplicate language name `{}`",
                language.name
            )));
        }
        if language.extensions.is_empty() || language.extensions.len() > 32 {
            return Err(CodeIntelligenceError::Validation(format!(
                "language `{}` must have between 1 and 32 extensions",
                language.name
            )));
        }
        for extension in &language.extensions {
            let normalized = normalize_extension(extension);
            if normalized.is_empty()
                || normalized.len() > 32
                || !normalized
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(CodeIntelligenceError::Validation(format!(
                    "invalid extension `{extension}` for language `{}`",
                    language.name
                )));
            }
        }
        if let Some(language_id) = &language.language_id {
            validate_text("language.language_id", language_id, 128)?;
        }
        for marker in &language.root_markers {
            validate_relative_marker(&language.name, marker)?;
        }
        if language.lsp.is_none() && language.formatter.is_none() {
            return Err(CodeIntelligenceError::Validation(format!(
                "language `{}` must configure lsp, formatter, or both",
                language.name
            )));
        }
        if let Some(lsp) = &language.lsp {
            validate_process(&lsp.process)?;
            validate_owner_command(&language.name, &lsp.process, require_absolute_commands)?;
            if !(MIN_TIMEOUT_MS..=MAX_DIAGNOSTIC_WAIT_MS).contains(&lsp.diagnostic_wait_ms) {
                return Err(CodeIntelligenceError::Validation(format!(
                    "language `{}` lsp.diagnostic_wait_ms must be between {MIN_TIMEOUT_MS} and {MAX_DIAGNOSTIC_WAIT_MS}",
                    language.name
                )));
            }
            if lsp.initialization_options.as_ref().is_some_and(|options| {
                serde_json::to_vec(options).map_or(true, |encoded| {
                    encoded.len() > MAX_INITIALIZATION_OPTIONS_BYTES
                })
            }) {
                return Err(CodeIntelligenceError::Validation(format!(
                    "language `{}` initialization_options exceed {MAX_INITIALIZATION_OPTIONS_BYTES} bytes",
                    language.name
                )));
            }
        }
        if let Some(formatter) = &language.formatter {
            validate_process(&formatter.process)?;
            validate_owner_command(
                &language.name,
                &formatter.process,
                require_absolute_commands,
            )?;
            let has_file = formatter
                .process
                .args
                .iter()
                .any(|argument| argument.contains("{file}"));
            if !has_file {
                return Err(CodeIntelligenceError::Validation(format!(
                    "language `{}` formatter args must contain {{file}} so GrokForge can format a private copy safely",
                    language.name
                )));
            }
        }
    }
    Ok(())
}

fn validate_owner_command(
    language: &str,
    process: &ProcessConfig,
    required: bool,
) -> Result<(), CodeIntelligenceError> {
    if required && (!Path::new(&process.command).is_absolute() || process.command.contains('{')) {
        return Err(CodeIntelligenceError::Validation(format!(
            "language `{language}` owner-configured commands must use an explicit absolute executable path"
        )));
    }
    Ok(())
}

fn resolve_executable(
    source: LanguageSource,
    workspace: &Path,
    program: &str,
) -> Result<ResolvedExecutable, CodeIntelligenceError> {
    match source {
        LanguageSource::Owner => resolve_owner_executable(program),
        LanguageSource::BuiltIn => {
            let path = std::env::var_os("PATH").ok_or_else(|| {
                CodeIntelligenceError::Validation(format!(
                    "cannot resolve built-in executable `{program}` because PATH is unset"
                ))
            })?;
            resolve_builtin_executable_in_paths(program, workspace, std::env::split_paths(&path))
        }
    }
}

fn resolve_owner_executable(program: &str) -> Result<ResolvedExecutable, CodeIntelligenceError> {
    let path = Path::new(program);
    if !path.is_absolute() {
        return Err(CodeIntelligenceError::Validation(format!(
            "owner-configured executable `{program}` must be an absolute path"
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        CodeIntelligenceError::Validation(format!(
            "cannot resolve owner-configured executable `{program}`: {error}"
        ))
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        CodeIntelligenceError::Validation(format!(
            "cannot inspect owner-configured executable {}: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(CodeIntelligenceError::Validation(format!(
            "owner-configured executable {} is not a regular executable file",
            canonical.display()
        )));
    }
    Ok(ResolvedExecutable {
        invocation_path: path.to_string_lossy().into_owned(),
        canonical_target: canonical,
    })
}

fn resolve_builtin_executable_in_paths(
    program: &str,
    workspace: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<ResolvedExecutable, CodeIntelligenceError> {
    let program_path = Path::new(program);
    if program_path.components().count() != 1
        || !matches!(program_path.components().next(), Some(Component::Normal(_)))
    {
        return Err(CodeIntelligenceError::Validation(format!(
            "built-in executable name `{program}` must be a single bare file name"
        )));
    }
    let workspace = std::fs::canonicalize(workspace).map_err(|error| {
        CodeIntelligenceError::Validation(format!(
            "cannot resolve workspace {} while locating `{program}`: {error}",
            workspace.display()
        ))
    })?;
    // Key by canonical target to collapse harmless aliases, but retain the absolute invocation
    // candidate so multicall proxies keep their argv[0] basename.
    let mut candidates = BTreeMap::new();
    for directory in paths {
        if !directory.is_absolute() {
            return Err(CodeIntelligenceError::Validation(format!(
                "refusing relative PATH entry `{}` while resolving built-in executable `{program}`; use absolute PATH entries or configure an owner-controlled absolute command",
                directory.display()
            )));
        }
        let original_candidate = directory.join(program);
        let Ok(canonical_directory) = std::fs::canonicalize(&directory) else {
            continue;
        };
        let candidate = canonical_directory.join(program);
        let Ok(canonical) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        if !metadata.is_file() || !is_executable(&metadata) {
            continue;
        }
        if original_candidate.starts_with(&workspace)
            || candidate.starts_with(&workspace)
            || canonical.starts_with(&workspace)
        {
            return Err(CodeIntelligenceError::Validation(format!(
                "refusing workspace-shadowed built-in executable `{}`; remove the workspace directory from PATH or configure an owner-controlled absolute command",
                candidate.display()
            )));
        }
        candidates.entry(canonical).or_insert(candidate);
    }
    match candidates.len() {
        0 => Err(CodeIntelligenceError::Validation(format!(
            "built-in executable `{program}` was not found as one canonical executable on absolute PATH entries; install it or configure an owner-controlled absolute command"
        ))),
        1 => candidates
            .into_iter()
            .next()
            .map(|(canonical_target, invocation_path)| ResolvedExecutable {
                invocation_path: invocation_path.to_string_lossy().into_owned(),
                canonical_target,
            })
            .ok_or_else(|| {
                CodeIntelligenceError::Validation(format!(
                    "built-in executable `{program}` disappeared during resolution"
                ))
            }),
        count => Err(CodeIntelligenceError::Validation(format!(
            "built-in executable `{program}` is ambiguous across {count} distinct PATH targets; configure one owner-controlled absolute command"
        ))),
    }
}

fn reverify_executable(
    invocation_path: &str,
    expected_target: &Path,
) -> Result<(), CodeIntelligenceError> {
    let observed = std::fs::canonicalize(invocation_path).map_err(|error| {
        CodeIntelligenceError::Validation(format!(
            "prepared executable `{invocation_path}` can no longer be resolved: {error}"
        ))
    })?;
    if observed != expected_target {
        return Err(CodeIntelligenceError::Validation(format!(
            "prepared executable `{invocation_path}` changed target from {} to {}; refusing to run it",
            expected_target.display(),
            observed.display()
        )));
    }
    let metadata = std::fs::metadata(&observed).map_err(|error| {
        CodeIntelligenceError::Validation(format!(
            "cannot re-inspect prepared executable {}: {error}",
            observed.display()
        ))
    })?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(CodeIntelligenceError::Validation(format!(
            "prepared executable {} is no longer a regular executable file",
            observed.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn validate_process(process: &ProcessConfig) -> Result<(), CodeIntelligenceError> {
    validate_text("process.command", &process.command, MAX_COMMAND_BYTES)?;
    if process.args.len() > MAX_ARGUMENTS {
        return Err(CodeIntelligenceError::Validation(format!(
            "a process may have at most {MAX_ARGUMENTS} arguments"
        )));
    }
    for argument in &process.args {
        if argument.len() > MAX_ARGUMENT_BYTES || argument.chars().any(char::is_control) {
            return Err(CodeIntelligenceError::Validation(format!(
                "process arguments must be at most {MAX_ARGUMENT_BYTES} bytes and contain no control characters"
            )));
        }
    }
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&process.timeout_ms) {
        return Err(CodeIntelligenceError::Validation(format!(
            "process.timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_text(field: &str, value: &str, max_bytes: usize) -> Result<(), CodeIntelligenceError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(CodeIntelligenceError::Validation(format!(
            "{field} must be non-empty, trimmed, at most {max_bytes} bytes, and contain no control characters"
        )));
    }
    Ok(())
}

fn validate_relative_marker(language: &str, marker: &str) -> Result<(), CodeIntelligenceError> {
    let path = Path::new(marker);
    if marker.is_empty()
        || marker.len() > 256
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CodeIntelligenceError::Validation(format!(
            "invalid root marker `{marker}` for language `{language}`"
        )));
    }
    Ok(())
}

fn normalize_extension(extension: &str) -> String {
    extension.trim_start_matches('.').to_ascii_lowercase()
}

fn process(command: &str, args: &[&str]) -> ProcessConfig {
    ProcessConfig {
        command: command.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
    }
}

#[allow(clippy::too_many_lines)]
fn default_languages() -> Vec<LanguageConfig> {
    let language = |name: &str,
                    extensions: &[&str],
                    language_id: &str,
                    markers: &[&str],
                    lsp: (&str, &[&str]),
                    formatter: (&str, &[&str])| LanguageConfig {
        name: name.to_string(),
        extensions: extensions
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        language_id: Some(language_id.to_string()),
        root_markers: markers.iter().map(|value| (*value).to_string()).collect(),
        lsp: Some(LspConfig {
            process: process(lsp.0, lsp.1),
            initialization_options: None,
            diagnostic_wait_ms: DEFAULT_DIAGNOSTIC_WAIT_MS,
        }),
        formatter: Some(FormatterConfig {
            process: process(formatter.0, formatter.1),
        }),
    };
    vec![
        language(
            "rust",
            &["rs"],
            "rust",
            &["Cargo.toml", "rust-project.json"],
            ("rust-analyzer", &[]),
            (
                "rustfmt",
                &["--edition", "2024", "--config-path", "{root}", "{file}"],
            ),
        ),
        language(
            "typescript",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            "typescript",
            &["tsconfig.json", "jsconfig.json", "package.json"],
            ("typescript-language-server", &["--stdio"]),
            ("prettier", &["--write", "{file}"]),
        ),
        language(
            "python",
            &["py", "pyi"],
            "python",
            &["pyproject.toml", "setup.py", "requirements.txt"],
            ("pyright-langserver", &["--stdio"]),
            ("ruff", &["format", "{file}"]),
        ),
        language(
            "go",
            &["go"],
            "go",
            &["go.work", "go.mod"],
            ("gopls", &[]),
            ("gofmt", &["-w", "{file}"]),
        ),
        language(
            "c-cpp",
            &["c", "h", "cc", "cpp", "cxx", "hpp", "hxx"],
            "cpp",
            &["compile_commands.json", ".clangd"],
            ("clangd", &[]),
            ("clang-format", &["-i", "{file}"]),
        ),
    ]
}

fn read_open_config(file: std::fs::File, path: &Path) -> Result<String, CodeIntelligenceError> {
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| CodeIntelligenceError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(CodeIntelligenceError::Validation(format!(
            "{} exceeds the {MAX_CONFIG_BYTES}-byte limit",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        CodeIntelligenceError::Validation(format!("{} is not valid UTF-8: {error}", path.display()))
    })
}

#[cfg(unix)]
fn read_secure_config(path: &Path) -> Result<Option<String>, CodeIntelligenceError> {
    use std::os::unix::fs::MetadataExt as _;

    use rustix::fs::{Mode, OFlags, open, openat};
    use rustix::io::Errno;

    let parent = path
        .parent()
        .ok_or_else(|| CodeIntelligenceError::UnsafeConfig {
            path: path.to_path_buf(),
            reason: "path has no parent directory",
        })?;
    let name = path
        .file_name()
        .ok_or_else(|| CodeIntelligenceError::UnsafeConfig {
            path: path.to_path_buf(),
            reason: "path has no file name",
        })?;
    let directory = match open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(CodeIntelligenceError::UnsafeConfig {
                path: parent.to_path_buf(),
                reason: "parent must be a real, non-symlink directory",
            });
        }
        Err(error) => {
            return Err(CodeIntelligenceError::Read {
                path: parent.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    let directory: std::fs::File = directory.into();
    let descriptor = match openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => {
            return Err(CodeIntelligenceError::UnsafeConfig {
                path: path.to_path_buf(),
                reason: "file must not be a symlink",
            });
        }
        Err(error) => {
            return Err(CodeIntelligenceError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    let metadata = directory
        .metadata()
        .map_err(|source| CodeIntelligenceError::Read {
            path: parent.to_path_buf(),
            source,
        })?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 & !0o700 != 0
    {
        return Err(CodeIntelligenceError::UnsafeConfig {
            path: parent.to_path_buf(),
            reason: "parent must be owned by the current user with permissions 0700 or stricter",
        });
    }
    let file: std::fs::File = descriptor.into();
    let metadata = file
        .metadata()
        .map_err(|source| CodeIntelligenceError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 & !0o600 != 0
    {
        return Err(CodeIntelligenceError::UnsafeConfig {
            path: path.to_path_buf(),
            reason: "file must be regular, singly linked, owner-only (0600), and owned by the current user",
        });
    }
    read_open_config(file, path).map(Some)
}

#[cfg(not(unix))]
fn read_secure_config(path: &Path) -> Result<Option<String>, CodeIntelligenceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CodeIntelligenceError::UnsafeConfig {
                path: path.to_path_buf(),
                reason: "file must be a regular, non-symlink file",
            })
        }
        Ok(_) => {
            let file = std::fs::File::open(path).map_err(|source| CodeIntelligenceError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            read_open_config(file, path).map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CodeIntelligenceError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_formatter_can_extend_default_lsp_for_same_extension() {
        let config = CodeIntelligenceConfig::from_toml(
            r#"
                [[language]]
                name = "my-rust"
                extensions = ["rs"]

                [language.formatter]
                command = "/bin/echo"
                args = ["--emit", "files", "{file}"]
                timeout_ms = 1000
            "#,
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("src/lib.rs");
        let formatter = config.prepare_formatter(workspace.path(), &file).unwrap();
        assert!(formatter.program.ends_with("/echo"));
        let default_server = config
            .languages
            .iter()
            .find(|entry| entry.language.matches(&file) && entry.language.lsp.is_some())
            .unwrap();
        assert_eq!(default_server.source, LanguageSource::BuiltIn);
        assert_eq!(
            default_server
                .language
                .lsp
                .as_ref()
                .unwrap()
                .process
                .command,
            "rust-analyzer"
        );
    }

    #[test]
    fn placeholders_are_expanded_as_exact_arguments() {
        let config = CodeIntelligenceConfig::from_toml(
            r#"
                extend_defaults = false
                [[language]]
                name = "demo"
                extensions = ["demo"]
                root_markers = ["project.demo"]

                [language.formatter]
                command = "/bin/echo"
                args = ["--root", "{root}", "--relative", "{file_relative}", "--file", "{file}"]
            "#,
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let nested = workspace.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("project.demo"), "").unwrap();
        let file = nested.join("has spaces.demo");
        let prepared = config.prepare_formatter(workspace.path(), &file).unwrap();
        assert_eq!(prepared.cwd, nested);
        assert!(prepared.program.ends_with("/echo"));
        assert_eq!(prepared.args[0], "--root");
        assert_eq!(prepared.args[1], prepared.cwd.to_string_lossy());
        assert_eq!(prepared.args[3], "nested/has spaces.demo");
        assert_eq!(prepared.args[5], file.to_string_lossy());

        let scratch = tempfile::tempdir().unwrap();
        let copy = scratch.path().join("source.demo");
        let prepared_copy = config
            .prepare_formatter_for_copy(workspace.path(), &file, &copy)
            .unwrap();
        assert_eq!(prepared_copy.args[3], "nested/has spaces.demo");
        assert_eq!(prepared_copy.args[5], copy.to_string_lossy());
    }

    #[test]
    fn unsafe_or_ambiguous_config_is_rejected() {
        let duplicate = CodeIntelligenceConfig::from_toml(
            r#"
                extend_defaults = false
                [[language]]
                name = "demo"
                extensions = ["x"]
                [language.lsp]
                command = "/bin/echo"
                diagnostic_wait_ms = 1250

                [[language]]
                name = "demo"
                extensions = ["y"]
                [language.lsp]
                command = "/bin/echo"
                diagnostic_wait_ms = 1250
            "#,
        );
        assert!(duplicate.unwrap_err().to_string().contains("duplicate"));

        let shellish = CodeIntelligenceConfig::from_toml(
            r#"
                extend_defaults = false
                [[language]]
                name = "demo"
                extensions = ["x"]
                [language.formatter]
                command = "/bin/formatter\nrm"
                args = ["{file}"]
            "#,
        );
        assert!(shellish.is_err());
    }

    #[test]
    fn default_toolchains_cover_common_languages() {
        let cases = [
            ("main.rs", "rust-analyzer", "rustfmt"),
            ("main.ts", "typescript-language-server", "prettier"),
            ("main.py", "pyright-langserver", "ruff"),
            ("main.go", "gopls", "gofmt"),
            ("main.cpp", "clangd", "clang-format"),
        ];
        for (path, server, formatter) in cases {
            let path = Path::new(path);
            let config = CodeIntelligenceConfig::default();
            let server_entry = config
                .languages
                .iter()
                .find(|entry| entry.language.matches(path) && entry.language.lsp.is_some())
                .unwrap();
            assert_eq!(
                server_entry.language.lsp.as_ref().unwrap().process.command,
                server
            );
            let formatter_entry = config
                .languages
                .iter()
                .find(|entry| entry.language.matches(path) && entry.language.formatter.is_some())
                .unwrap();
            assert_eq!(
                formatter_entry
                    .language
                    .formatter
                    .as_ref()
                    .unwrap()
                    .process
                    .command,
                formatter
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn built_in_resolution_rejects_workspace_path_shadowing_and_ambiguity() {
        use std::os::unix::fs::PermissionsExt as _;

        fn executable(path: &Path) {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let workspace = tempfile::tempdir().unwrap();
        let safe_one = tempfile::tempdir().unwrap();
        let safe_two = tempfile::tempdir().unwrap();
        executable(&workspace.path().join("demo-lsp"));
        executable(&safe_one.path().join("demo-lsp"));
        executable(&safe_two.path().join("demo-lsp"));

        let shadowed = resolve_builtin_executable_in_paths(
            "demo-lsp",
            workspace.path(),
            [
                workspace.path().to_path_buf(),
                safe_one.path().to_path_buf(),
            ],
        )
        .unwrap_err();
        assert!(shadowed.to_string().contains("workspace-shadowed"));

        let ambiguous = resolve_builtin_executable_in_paths(
            "demo-lsp",
            workspace.path(),
            [safe_one.path().to_path_buf(), safe_two.path().to_path_buf()],
        )
        .unwrap_err();
        assert!(ambiguous.to_string().contains("ambiguous"));

        let relative = resolve_builtin_executable_in_paths(
            "demo-lsp",
            workspace.path(),
            [PathBuf::from("relative"), safe_one.path().to_path_buf()],
        )
        .unwrap_err();
        assert!(relative.to_string().contains("relative PATH entry"));

        let resolved = resolve_builtin_executable_in_paths(
            "demo-lsp",
            workspace.path(),
            [safe_one.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(
            PathBuf::from(&resolved.invocation_path),
            std::fs::canonicalize(safe_one.path().join("demo-lsp")).unwrap()
        );
        assert_eq!(
            resolved.canonical_target,
            std::fs::canonicalize(safe_one.path().join("demo-lsp")).unwrap()
        );

        // Standard Rust installations expose tools as rustup-style multicall symlinks. The
        // target is verified, but the proxy path must remain the invocation path so argv[0]
        // continues to select the intended applet.
        let proxy_dir = tempfile::tempdir().unwrap();
        let dispatcher = proxy_dir.path().join("dispatcher");
        executable(&dispatcher);
        std::os::unix::fs::symlink("dispatcher", proxy_dir.path().join("demo-lsp")).unwrap();
        let proxy = resolve_builtin_executable_in_paths(
            "demo-lsp",
            workspace.path(),
            [proxy_dir.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(
            PathBuf::from(&proxy.invocation_path),
            std::fs::canonicalize(proxy_dir.path())
                .unwrap()
                .join("demo-lsp")
        );
        assert_eq!(
            proxy.canonical_target,
            std::fs::canonicalize(dispatcher).unwrap()
        );
        reverify_executable(&proxy.invocation_path, &proxy.canonical_target).unwrap();

        let replacement = proxy_dir.path().join("replacement");
        executable(&replacement);
        std::fs::remove_file(&proxy.invocation_path).unwrap();
        std::os::unix::fs::symlink("replacement", &proxy.invocation_path).unwrap();
        let retargeted =
            reverify_executable(&proxy.invocation_path, &proxy.canonical_target).unwrap_err();
        assert!(retargeted.to_string().contains("changed target"));
    }

    #[cfg(unix)]
    #[test]
    fn owner_config_loader_rejects_loose_permissions_and_aliases() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("code-intelligence.toml");
        std::fs::write(&path, "extend_defaults = true\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_secure_config(&path).unwrap().is_some());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_secure_config(&path),
            Err(CodeIntelligenceError::UnsafeConfig { .. })
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let alias = directory.path().join("alias.toml");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            read_secure_config(&path),
            Err(CodeIntelligenceError::UnsafeConfig { .. })
        ));
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_file(&path).unwrap();

        let target = directory.path().join("target.toml");
        std::fs::write(&target, "extend_defaults = true\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(matches!(
            read_secure_config(&path),
            Err(CodeIntelligenceError::UnsafeConfig { .. })
        ));
    }
}
