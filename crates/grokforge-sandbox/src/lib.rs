//! `grokforge-sandbox` — compiles a platform-agnostic [`SandboxPolicy`] into an enforcement
//! plan and runs commands under it.
//!
//! Linux uses a validated bubblewrap backend and macOS uses Seatbelt. Every command is spawned
//! through [`SandboxRunner`]; when neither enforcing backend is usable, [`PassthroughRunner`]
//! reports `enforced = false` and refuses any policy with confinement requirements. In
//! particular, it cannot silently bypass the protected Git-metadata paths present in normal and
//! full-access policies.

mod bubblewrap;
mod classifier;
mod exec;
mod passthrough;
mod privacy;
mod protected;
mod seatbelt;
mod unreadable;
mod writable;

pub use bubblewrap::BubblewrapRunner;
pub use classifier::classify;
pub use exec::{CommandSpec, ExecError, ExecOutput, OUTPUT_CAP, run_capture};
pub use passthrough::PassthroughRunner;
pub use seatbelt::SeatbeltRunner;

use std::sync::Arc;

use async_trait::async_trait;
use grokforge_protocol::{NetworkMode, SandboxMode, SandboxPolicy};

/// Validate the public policy boundary before a backend canonicalizes paths or takes an
/// unwrapped fast path. Roots and the command cwd must already be absolute, existing physical
/// directories; protected paths may be absent so a backend can install a future deny rule, but
/// they must still be absolute.
pub(crate) fn validate_backend_policy(
    policy: &SandboxPolicy,
    command: &CommandSpec,
) -> Result<(), ExecError> {
    validate_existing_absolute_directory("command cwd", &command.cwd)?;
    for root in &policy.readable_roots {
        validate_existing_absolute_directory("readable root", root)?;
    }
    for root in &policy.writable_roots {
        validate_existing_absolute_directory("writable root", root)?;
    }
    for protected in &policy.protected_paths {
        if !protected.is_absolute() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "protected path {} is not absolute",
                protected.display()
            )));
        }
    }
    for pattern in &policy.unreadable_globs {
        globset::GlobBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "invalid unreadable glob `{pattern}`: {error}"
                ))
            })?;
    }
    Ok(())
}

fn validate_existing_absolute_directory(
    label: &str,
    path: &std::path::Path,
) -> Result<(), ExecError> {
    if !path.is_absolute() {
        return Err(ExecError::UnsupportedPolicy(format!(
            "{label} {} is not absolute",
            path.display()
        )));
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        ExecError::UnsupportedPolicy(format!(
            "could not resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ExecError::UnsupportedPolicy(format!(
            "{label} {} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

/// Whether a policy has no confinement requirement at all and may therefore bypass a wrapper.
pub(crate) fn is_truly_unconstrained(policy: &SandboxPolicy) -> bool {
    policy.mode == SandboxMode::DangerFullAccess
        && policy.network == NetworkMode::Full
        && policy.unreadable_globs.is_empty()
        && policy.protected_paths.is_empty()
        && policy.readable_roots == [std::path::PathBuf::from("/")]
        && policy.writable_roots == [std::path::PathBuf::from("/")]
}

/// What a backend can actually enforce on this machine, surfaced to the UI so degradation is
/// never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapability {
    /// Human-readable backend name.
    pub backend: String,
    /// Whether OS-level confinement is actually active.
    pub enforced: bool,
    /// Notes about partial enforcement or fallback.
    pub notes: Vec<String>,
}

/// Runs commands under a [`SandboxPolicy`]. Implemented by each platform backend.
#[async_trait]
pub trait SandboxRunner: Send + Sync + std::fmt::Debug {
    /// What this runner can enforce here.
    fn capability(&self) -> SandboxCapability;

    /// Run a command under the policy, returning captured output. Wrapping backends must preserve
    /// [`CommandSpec`]'s cancellation token so process teardown remains cooperative.
    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError>;
}

/// Select the best available OS sandbox runner for this platform, falling back to a fail-closed
/// passthrough runner when no enforcing backend is usable.
#[must_use]
pub fn default_runner() -> Arc<dyn SandboxRunner> {
    #[cfg(target_os = "macos")]
    {
        if SeatbeltRunner::available() {
            return Arc::new(SeatbeltRunner);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if BubblewrapRunner::available() {
            return Arc::new(BubblewrapRunner);
        }
    }
    Arc::new(PassthroughRunner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dedicated CI job sets this flag so unavailable kernel enforcement is a hard failure,
    /// while ordinary developer test runs can still exercise fail-closed fallback behavior.
    #[test]
    fn required_ci_sandbox_backend_is_really_enforced() {
        if std::env::var_os("GROKFORGE_REQUIRE_SANDBOX").is_none() {
            return;
        }
        let capability = default_runner().capability();
        assert!(
            capability.enforced,
            "required sandbox backend unavailable: {capability:?}"
        );
    }

    #[test]
    fn backend_boundary_rejects_relative_and_unresolvable_paths() {
        let dir = tempfile::tempdir().expect("workspace");
        let command = CommandSpec::shell("true", dir.path().to_path_buf());
        let mut policy = SandboxPolicy::workspace_write(dir.path());
        policy.writable_roots = vec![std::path::PathBuf::from("relative")];
        assert!(matches!(
            validate_backend_policy(&policy, &command),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("not absolute")
        ));

        policy.writable_roots = vec![dir.path().join("missing")];
        assert!(matches!(
            validate_backend_policy(&policy, &command),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("could not resolve")
        ));

        let relative_command = CommandSpec::shell("true", std::path::PathBuf::from("relative"));
        assert!(matches!(
            validate_backend_policy(&SandboxPolicy::workspace_write(dir.path()), &relative_command),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("not absolute")
        ));
    }

    #[test]
    fn only_exactly_unconstrained_policy_can_bypass_a_backend() {
        let dir = tempfile::tempdir().expect("workspace");
        let mut policy = SandboxPolicy::danger_full_access(dir.path());
        policy.protected_paths.clear();
        assert!(is_truly_unconstrained(&policy));

        policy.network = NetworkMode::Isolated;
        assert!(!is_truly_unconstrained(&policy));
        policy.network = NetworkMode::Full;
        policy.unreadable_globs.push("**/.env".to_string());
        assert!(!is_truly_unconstrained(&policy));
        policy.unreadable_globs.clear();
        policy.writable_roots = vec![dir.path().to_path_buf()];
        assert!(!is_truly_unconstrained(&policy));
    }
}
