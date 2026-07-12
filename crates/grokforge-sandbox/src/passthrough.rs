//! The M2 passthrough runner: runs commands with cwd + env scrubbing but **no OS
//! confinement**. It exists so the exec path is built against the [`SandboxRunner`] seam from
//! day one; the Linux/macOS enforcing backends replace it at M5. It reports `enforced = false`
//! so nothing claims protection that isn't there.

use async_trait::async_trait;
use grokforge_protocol::SandboxPolicy;

use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::{SandboxCapability, SandboxRunner};

/// A runner that applies no kernel-level confinement.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughRunner;

#[async_trait]
impl SandboxRunner for PassthroughRunner {
    fn capability(&self) -> SandboxCapability {
        SandboxCapability {
            backend: "passthrough".to_string(),
            enforced: false,
            notes: vec![
                "no OS-level confinement (M2); enforced Landlock/Seatbelt backends land at M5"
                    .to_string(),
            ],
        }
    }

    async fn run(
        &self,
        _policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        run_capture(command).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn runs_a_command_and_captures_stdout() {
        let runner = PassthroughRunner;
        let policy = SandboxPolicy::danger_full_access(&PathBuf::from("/tmp"));
        let spec = CommandSpec {
            program: "echo".to_string(),
            args: vec!["hello".to_string()],
            cwd: std::env::temp_dir(),
            timeout: Duration::from_secs(5),
        };
        let out = runner.run(&policy, &spec).await.expect("run");
        assert!(out.succeeded());
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn reports_unenforced_capability() {
        assert!(!PassthroughRunner.capability().enforced);
    }
}
