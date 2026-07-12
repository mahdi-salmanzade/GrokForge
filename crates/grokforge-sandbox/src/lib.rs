//! `grokforge-sandbox` — compiles a platform-agnostic [`SandboxPolicy`] into an enforcement
//! plan and runs commands under it.
//!
//! v0.1 ships OS-native backends (Landlock+seccomp on Linux, Seatbelt on macOS) at M5. This
//! milestone (M2) establishes the **seam**: every command is spawned through a
//! [`SandboxRunner`], so real backends slot in later without the exec tool ever growing around
//! a raw `Command`. The M2 runner is [`PassthroughRunner`] — it applies no OS confinement and
//! honestly reports `enforced = false`, so the UI never claims protection that isn't active.

mod exec;
mod passthrough;

pub use exec::{CommandSpec, ExecError, ExecOutput, OUTPUT_CAP, run_capture};
pub use passthrough::PassthroughRunner;

use async_trait::async_trait;
use grokforge_protocol::SandboxPolicy;

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

    /// Run a command under the policy, returning captured output.
    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError>;
}
