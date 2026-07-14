//! Typed GrokForge configuration.
//!
//! Values are layered as defaults → `~/.grokforge/config.toml` → optionally the project's
//! `.grokforge/config.toml` → `GROKFORGE_CONFIG_*` environment variables. Project configuration
//! is ignored unless the caller explicitly trusts it and is deliberately limited to model/runtime
//! preferences: a checked-out repository cannot redirect the API endpoint that receives the
//! user's credential or weaken sandbox/approval policy.

// `ConfigError` wraps `figment::Error`, which is large by design; config loading is not a hot path
// and boxing every error would only obscure the source, so the large-Err results stay as-is.
#![allow(clippy::result_large_err)]

use std::io::Read as _;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// Crate version, surfaced in `grokforge doctor`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_BASE_URL: &str = "https://api.x.ai";
const DEFAULT_MODEL: &str = "grok-build-0.1";
const DEFAULT_PLAN_MODEL: &str = "grok-4.5";
const MAX_MODEL_ID_BYTES: usize = 160;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Fully resolved application settings. Credentials never belong in this structure.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
}

/// Provider settings are accepted only from the owner-controlled global file or environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub grok: GrokProviderConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrokProviderConfig {
    pub base_url: String,
}

impl Default for GrokProviderConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

/// Agent defaults shared by the TUI and headless frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub default_model: String,
    pub plan_model: String,
    #[serde(default, deserialize_with = "deserialize_effort")]
    pub effort: Option<Effort>,
    pub max_iterations: u32,
    pub auto_compact: bool,
    pub compaction_trigger_bytes: usize,
    pub compaction_keep_tail: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_model: DEFAULT_MODEL.to_string(),
            plan_model: DEFAULT_PLAN_MODEL.to_string(),
            effort: None,
            max_iterations: 32,
            auto_compact: true,
            compaction_trigger_bytes: 400_000,
            compaction_keep_tail: 8,
        }
    }
}

/// Provider-neutral reasoning-effort preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl Effort {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// Layer-local effort choice. `Auto` is kept distinct until that layer is applied so a trusted
/// project or environment override can deliberately clear a lower-precedence explicit effort.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EffortChoice {
    Auto,
    Low,
    Medium,
    High,
    Xhigh,
}

impl EffortChoice {
    const fn effective(self) -> Option<Effort> {
        match self {
            Self::Auto => None,
            Self::Low => Some(Effort::Low),
            Self::Medium => Some(Effort::Medium),
            Self::High => Some(Effort::High),
            Self::Xhigh => Some(Effort::Xhigh),
        }
    }
}

fn deserialize_effort<'de, D>(deserializer: D) -> Result<Option<Effort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<EffortChoice>::deserialize(deserializer)?.and_then(EffortChoice::effective))
}

/// Safe subset that a repository is allowed to suggest.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectConfig {
    agent: ProjectAgentConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectAgentConfig {
    default_model: Option<String>,
    plan_model: Option<String>,
    effort: Option<EffortChoice>,
    max_iterations: Option<u32>,
    auto_compact: Option<bool>,
    compaction_trigger_bytes: Option<usize>,
    compaction_keep_tail: Option<usize>,
}

impl ProjectAgentConfig {
    fn apply(self, target: &mut AgentConfig) {
        if let Some(value) = self.default_model {
            target.default_model = value;
        }
        if let Some(value) = self.plan_model {
            target.plan_model = value;
        }
        if let Some(value) = self.effort {
            target.effort = value.effective();
        }
        if let Some(value) = self.max_iterations {
            target.max_iterations = value;
        }
        if let Some(value) = self.auto_compact {
            target.auto_compact = value;
        }
        if let Some(value) = self.compaction_trigger_bytes {
            target.compaction_trigger_bytes = value;
        }
        if let Some(value) = self.compaction_keep_tail {
            target.compaction_keep_tail = value;
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot locate the home directory for ~/.grokforge/config.toml")]
    NoHomeDirectory,
    #[error("could not read configuration: {0}")]
    Invalid(#[from] figment::Error),
    #[error("project config must be a regular, non-symlink file: {0}")]
    UnsafeProjectConfig(PathBuf),
    #[error("unsafe global config directory {path}: {reason}")]
    UnsafeGlobalDirectory { path: PathBuf, reason: &'static str },
    #[error("unsafe global config file {path}: {reason}")]
    UnsafeGlobalConfig { path: PathBuf, reason: &'static str },
    #[error("could not read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Validation(String),
}

impl Config {
    /// Load owner-controlled configuration, ignore project configuration, then apply
    /// `GROKFORGE_CONFIG_*` environment overrides.
    pub fn load(workspace: &Path) -> Result<Self, ConfigError> {
        Self::load_with_project_config(workspace, false)
    }

    /// Load owner-controlled configuration and optionally trusted project configuration, then
    /// apply `GROKFORGE_CONFIG_*` environment overrides. Nested keys use double underscores, for
    /// example
    /// `GROKFORGE_CONFIG_PROVIDER__GROK__BASE_URL`. The dedicated prefix intentionally avoids
    /// treating UI switches such as `GROKFORGE_ASCII` as configuration keys.
    pub fn load_with_project_config(
        workspace: &Path,
        trust_project_config: bool,
    ) -> Result<Self, ConfigError> {
        let global = global_config_path()?;
        Self::load_from_paths(workspace, &global, true, trust_project_config)
    }

    fn load_from_paths(
        workspace: &Path,
        global_path: &Path,
        include_environment: bool,
        trust_project_config: bool,
    ) -> Result<Self, ConfigError> {
        let mut global = Figment::from(Serialized::defaults(Self::default()));
        if let Some(text) = read_secure_global_config(global_path)? {
            global = global.merge(Toml::string(&text));
        }
        let mut config: Self = global.extract()?;

        let project_dir = workspace.join(".grokforge");
        let project_path = project_dir.join("config.toml");
        if trust_project_config
            && let Some(text) = read_trusted_project_config(&project_dir, &project_path)?
        {
            let project: ProjectConfig = Figment::new().merge(Toml::string(&text)).extract()?;
            project.agent.apply(&mut config.agent);
        }

        if include_environment {
            config = Figment::from(Serialized::defaults(config))
                .merge(Env::prefixed("GROKFORGE_CONFIG_").split("__"))
                .extract()?;
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_base_url(&self.provider.grok.base_url)?;
        validate_model_id("agent.default_model", &self.agent.default_model)?;
        validate_model_id("agent.plan_model", &self.agent.plan_model)?;
        if !(1..=256).contains(&self.agent.max_iterations) {
            return Err(ConfigError::Validation(
                "agent.max_iterations must be between 1 and 256".to_string(),
            ));
        }
        if self.agent.compaction_trigger_bytes < 16 * 1024 {
            return Err(ConfigError::Validation(
                "agent.compaction_trigger_bytes must be at least 16384".to_string(),
            ));
        }
        if self.agent.compaction_keep_tail == 0 || self.agent.compaction_keep_tail > 1_024 {
            return Err(ConfigError::Validation(
                "agent.compaction_keep_tail must be between 1 and 1024".to_string(),
            ));
        }
        Ok(())
    }
}

/// Owner-controlled configuration location. Credential storage uses the same private directory,
/// but credentials are a separate encrypted file and are never deserialized here.
pub fn global_config_path() -> Result<PathBuf, ConfigError> {
    let base = BaseDirs::new().ok_or(ConfigError::NoHomeDirectory)?;
    Ok(base.home_dir().join(".grokforge/config.toml"))
}

fn validate_base_url(value: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(value)
        .map_err(|error| ConfigError::Validation(format!("provider.grok.base_url: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.cannot_be_a_base()
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigError::Validation(
            "provider.grok.base_url must be an http(s) origin without credentials, query, or fragment"
                .to_string(),
        ));
    }
    if parsed.scheme() == "http" && !is_loopback_url(&parsed) {
        return Err(ConfigError::Validation(
            "provider.grok.base_url must use HTTPS unless it targets a loopback address"
                .to_string(),
        ));
    }
    Ok(())
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn validate_model_id(field: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > MAX_MODEL_ID_BYTES
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(ConfigError::Validation(format!(
            "{field} must be a non-empty model id of at most {MAX_MODEL_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_config_file(path: &Path) -> Result<String, ConfigError> {
    let file = std::fs::File::open(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    read_open_config_file(file, path)
}

/// Read a trusted repository config through a no-follow directory descriptor. Explicit trust lets
/// the repository suggest runtime preferences; it does not make a symlink/hardlink race into an
/// acceptable way to read an unrelated host file.
#[cfg(unix)]
fn read_trusted_project_config(
    directory: &Path,
    path: &Path,
) -> Result<Option<String>, ConfigError> {
    use std::os::unix::fs::MetadataExt as _;

    use rustix::fs::{Mode, OFlags, open, openat};
    use rustix::io::Errno;

    let directory_descriptor = match open(
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(ConfigError::UnsafeProjectConfig(directory.to_path_buf()));
        }
        Err(error) => {
            return Err(ConfigError::Read {
                path: directory.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    let directory_file: std::fs::File = directory_descriptor.into();
    let descriptor = match openat(
        &directory_file,
        "config.toml",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err(ConfigError::UnsafeProjectConfig(path.to_path_buf())),
        Err(error) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    let file: std::fs::File = descriptor.into();
    let metadata = file.metadata().map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(ConfigError::UnsafeProjectConfig(path.to_path_buf()));
    }
    read_open_config_file(file, path).map(Some)
}

#[cfg(not(unix))]
fn read_trusted_project_config(
    directory: &Path,
    path: &Path,
) -> Result<Option<String>, ConfigError> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ConfigError::UnsafeProjectConfig(directory.to_path_buf()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: directory.to_path_buf(),
                source,
            });
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ConfigError::UnsafeProjectConfig(path.to_path_buf()))
        }
        Ok(_) => read_config_file(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn read_open_config_file(file: std::fs::File, path: &Path) -> Result<String, ConfigError> {
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(ConfigError::Validation(format!(
            "{} exceeds the {MAX_CONFIG_BYTES}-byte config limit",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        ConfigError::Validation(format!("{} is not valid UTF-8: {error}", path.display()))
    })
}

/// Open the global file relative to an already-validated directory descriptor. This prevents a
/// checked path from being exchanged for a symlink between validation and read.
#[cfg(unix)]
fn read_secure_global_config(path: &Path) -> Result<Option<String>, ConfigError> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use rustix::io::Errno;

    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::UnsafeGlobalConfig {
            path: path.to_path_buf(),
            reason: "path has no parent directory",
        })?;
    let name = path
        .file_name()
        .ok_or_else(|| ConfigError::UnsafeGlobalConfig {
            path: path.to_path_buf(),
            reason: "path has no file name",
        })?;

    let directory_descriptor = match open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(ConfigError::UnsafeGlobalDirectory {
                path: parent.to_path_buf(),
                reason: "must be a real, non-symlink directory",
            });
        }
        Err(error) => {
            return Err(ConfigError::Read {
                path: parent.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    let directory: std::fs::File = directory_descriptor.into();
    let descriptor = match openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => {
            return Err(ConfigError::UnsafeGlobalConfig {
                path: path.to_path_buf(),
                reason: "must not be a symlink",
            });
        }
        Err(error) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::from(error),
            });
        }
    };
    validate_global_directory(&directory, parent)?;
    let file: std::fs::File = descriptor.into();
    validate_global_file(&file, path)?;

    read_open_config_file(file, path).map(Some)
}

#[cfg(unix)]
fn validate_global_directory(directory: &std::fs::File, path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = directory.metadata().map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let reason = if !metadata.is_dir() {
        Some("must be a directory")
    } else if metadata.uid() != rustix::process::geteuid().as_raw() {
        Some("must be owned by the current user")
    } else if metadata.mode() & 0o7777 & !0o700 != 0 {
        Some("permissions must be 0700 or stricter; run `chmod 700 ~/.grokforge`")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(ConfigError::UnsafeGlobalDirectory {
            path: path.to_path_buf(),
            reason,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_global_file(file: &std::fs::File, path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata().map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let reason = if !metadata.is_file() {
        Some("must be a regular file")
    } else if metadata.nlink() != 1 {
        Some("must have exactly one hard link")
    } else if metadata.uid() != rustix::process::geteuid().as_raw() {
        Some("must be owned by the current user")
    } else if metadata.mode() & 0o7777 & !0o600 != 0 {
        Some("permissions must be 0600 or stricter; run `chmod 600 ~/.grokforge/config.toml`")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(ConfigError::UnsafeGlobalConfig {
            path: path.to_path_buf(),
            reason,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_secure_global_config(path: &Path) -> Result<Option<String>, ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::UnsafeGlobalConfig {
            path: path.to_path_buf(),
            reason: "path has no parent directory",
        })?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ConfigError::UnsafeGlobalDirectory {
                path: parent.to_path_buf(),
                reason: "must be a real, non-symlink directory",
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: parent.to_path_buf(),
                source,
            });
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ConfigError::UnsafeGlobalConfig {
                path: path.to_path_buf(),
                reason: "must be a regular, non-symlink file",
            })
        }
        Ok(_) => read_config_file(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn write_global(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(
                path.parent().expect("test global config parent"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
    }

    #[test]
    fn defaults_are_real_runtime_values() {
        let config = Config::default();
        assert_eq!(config.provider.grok.base_url, "https://api.x.ai");
        assert_eq!(config.agent.default_model, "grok-build-0.1");
        assert_eq!(config.agent.plan_model, "grok-4.5");
        assert_eq!(config.agent.max_iterations, 32);
    }

    #[test]
    fn global_then_project_layers_only_safe_agent_preferences() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("repo");
        fs::create_dir_all(workspace.join(".grokforge")).unwrap();
        let global = dir.path().join("config.toml");
        write_global(
            &global,
            "[provider.grok]\nbase_url = 'https://example.test/v1'\n[agent]\ndefault_model = 'global-model'\nmax_iterations = 20\n",
        );
        fs::write(
            workspace.join(".grokforge/config.toml"),
            "[agent]\ndefault_model = 'project-model'\neffort = 'high'\n",
        )
        .unwrap();

        let config = Config::load_from_paths(&workspace, &global, false, true).unwrap();
        assert_eq!(config.provider.grok.base_url, "https://example.test/v1");
        assert_eq!(config.agent.default_model, "project-model");
        assert_eq!(config.agent.effort, Some(Effort::High));
        assert_eq!(config.agent.max_iterations, 20);
    }

    #[test]
    fn project_cannot_redirect_provider_or_add_unknown_policy() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join(".grokforge");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "[provider.grok]\nbase_url = 'https://attacker.invalid'\n",
        )
        .unwrap();

        assert!(
            Config::load_from_paths(dir.path(), &dir.path().join("none"), false, true).is_err()
        );
    }

    #[test]
    fn project_config_is_ignored_without_explicit_trust() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join(".grokforge");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "[agent]\ndefault_model = 'billable-project-model'\neffort = 'xhigh'\nmax_iterations = 256\n",
        )
        .unwrap();

        let config =
            Config::load_from_paths(dir.path(), &dir.path().join("none"), false, false).unwrap();
        assert_eq!(config.agent, AgentConfig::default());
    }

    #[test]
    fn trusted_project_auto_clears_global_effort_but_omission_preserves_it() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("repo");
        let project_dir = workspace.join(".grokforge");
        fs::create_dir_all(&project_dir).unwrap();
        let global = dir.path().join("config.toml");
        write_global(&global, "[agent]\neffort = 'high'\n");

        fs::write(
            project_dir.join("config.toml"),
            "[agent]\nmax_iterations = 8\n",
        )
        .unwrap();
        let omitted = Config::load_from_paths(&workspace, &global, false, true).unwrap();
        assert_eq!(omitted.agent.effort, Some(Effort::High));

        fs::write(
            project_dir.join("config.toml"),
            "[agent]\neffort = 'auto'\n",
        )
        .unwrap();
        let cleared = Config::load_from_paths(&workspace, &global, false, true).unwrap();
        assert_eq!(cleared.agent.effort, None);
    }

    #[test]
    fn a_higher_precedence_auto_value_deserializes_as_provider_default() {
        let mut lower = Config::default();
        lower.agent.effort = Some(Effort::Xhigh);
        let resolved: Config = Figment::from(Serialized::defaults(lower))
            .merge(Toml::string("[agent]\neffort = 'auto'\n"))
            .extract()
            .unwrap();
        assert_eq!(resolved.agent.effort, None);
    }

    #[cfg(unix)]
    #[test]
    fn project_config_links_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let config_dir = dir.path().join(".grokforge");
        fs::create_dir(&config_dir).unwrap();
        let target = dir.path().join("elsewhere.toml");
        fs::write(&target, "[agent]\ndefault_model = 'redirected'\n").unwrap();
        symlink(&target, config_dir.join("config.toml")).unwrap();

        assert!(matches!(
            Config::load_from_paths(dir.path(), &dir.path().join("none"), false, true),
            Err(ConfigError::UnsafeProjectConfig(_))
        ));

        fs::remove_file(config_dir.join("config.toml")).unwrap();
        fs::hard_link(&target, config_dir.join("config.toml")).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &dir.path().join("none"), false, true),
            Err(ConfigError::UnsafeProjectConfig(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn project_config_parent_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(
            outside.join("config.toml"),
            "[agent]\ndefault_model = 'redirected'\n",
        )
        .unwrap();
        symlink(&outside, workspace.join(".grokforge")).unwrap();

        assert!(matches!(
            Config::load_from_paths(&workspace, &parent.path().join("none"), false, true),
            Err(ConfigError::UnsafeProjectConfig(_))
        ));
    }

    #[test]
    fn rejects_ambiguous_endpoint_and_unbounded_iterations() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("config.toml");
        write_global(
            &global,
            "[provider.grok]\nbase_url = 'https://user@example.test/?q=1'\n[agent]\nmax_iterations = 999\n",
        );

        assert!(Config::load_from_paths(dir.path(), &global, false, false).is_err());
    }

    #[test]
    fn plaintext_http_is_limited_to_loopback_endpoints() {
        for allowed in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            assert!(validate_base_url(allowed).is_ok(), "{allowed}");
        }
        for rejected in ["http://api.x.ai", "http://192.0.2.1:8080"] {
            assert!(validate_base_url(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn rejects_oversized_config_before_parsing() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("config.toml");
        let oversized = usize::try_from(MAX_CONFIG_BYTES).unwrap().saturating_add(1);
        write_global(&global, vec![b' '; oversized]);

        assert!(Config::load_from_paths(dir.path(), &global, false, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn global_config_rejects_symlinks_hardlinks_and_broad_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("private");
        fs::create_dir(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let target = dir.path().join("target.toml");
        write_global(&target, "[agent]\nmax_iterations = 4\n");
        let config = config_dir.join("config.toml");
        symlink(&target, &config).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &config, false, false),
            Err(ConfigError::UnsafeGlobalConfig { .. })
        ));

        fs::remove_file(&config).unwrap();
        fs::hard_link(&target, &config).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &config, false, false),
            Err(ConfigError::UnsafeGlobalConfig { .. })
        ));

        fs::remove_file(&config).unwrap();
        write_global(&config, "[agent]\nmax_iterations = 4\n");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &config, false, false),
            Err(ConfigError::UnsafeGlobalConfig { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn global_config_rejects_unsafe_or_symlinked_parent() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("private");
        fs::create_dir(&config_dir).unwrap();
        let config = config_dir.join("config.toml");
        write_global(&config, "[agent]\nmax_iterations = 4\n");
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &config, false, false),
            Err(ConfigError::UnsafeGlobalDirectory { .. })
        ));

        fs::remove_file(&config).unwrap();
        fs::remove_dir(&config_dir).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&outside, &config_dir).unwrap();
        assert!(matches!(
            Config::load_from_paths(dir.path(), &config, false, false),
            Err(ConfigError::UnsafeGlobalDirectory { .. })
        ));
    }
}
