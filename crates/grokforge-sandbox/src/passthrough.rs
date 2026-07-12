//! Fallback runner for platforms without a usable enforcing backend. It reports
//! `enforced = false` and fails closed whenever the requested policy has confinement
//! requirements. It only executes a deliberately unconstrained full-access policy with `/`
//! read/write roots, full network, and no protected paths or unreadable rules.

use async_trait::async_trait;
use grokforge_protocol::SandboxPolicy;

use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::{SandboxCapability, SandboxRunner, is_truly_unconstrained, validate_backend_policy};

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
                "no OS-level confinement; sandboxed command execution is disabled (fail closed)"
                    .to_string(),
            ],
        }
    }

    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        validate_backend_policy(policy, command)?;
        if !is_truly_unconstrained(policy) {
            return Err(ExecError::UnsupportedPolicy(
                "no enforcing sandbox backend is available".to_string(),
            ));
        }
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
        let mut policy = SandboxPolicy::danger_full_access(&PathBuf::from("/tmp"));
        // Passthrough is only valid when the policy truly has no confinement requirements.
        policy.protected_paths.clear();
        let spec = CommandSpec {
            program: "echo".to_string(),
            args: vec!["hello".to_string()],
            cwd: std::env::temp_dir(),
            timeout: Duration::from_secs(5),
            cancellation: None,
        };
        let out = runner.run(&policy, &spec).await.expect("run");
        assert!(out.succeeded());
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn reports_unenforced_capability() {
        assert!(!PassthroughRunner.capability().enforced);
    }

    #[tokio::test]
    async fn fails_closed_instead_of_ignoring_a_sandbox_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("must-not-exist");
        let policy = SandboxPolicy::workspace_write(dir.path());
        let spec = CommandSpec::shell(
            &format!("touch '{}'", marker.display()),
            dir.path().to_path_buf(),
        );
        let err = PassthroughRunner
            .run(&policy, &spec)
            .await
            .expect_err("sandboxed command must not run without a backend");
        assert!(matches!(err, ExecError::UnsupportedPolicy(_)));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn danger_mode_still_requires_git_protection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = SandboxPolicy::danger_full_access(dir.path());
        let spec = CommandSpec::shell("true", dir.path().to_path_buf());
        let err = PassthroughRunner
            .run(&policy, &spec)
            .await
            .expect_err("protected paths require an enforcing backend");
        assert!(matches!(err, ExecError::UnsupportedPolicy(_)));
    }

    #[tokio::test]
    async fn danger_mode_with_any_remaining_constraint_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = CommandSpec::shell("true", dir.path().to_path_buf());
        let mut base = SandboxPolicy::danger_full_access(dir.path());
        base.protected_paths.clear();

        let mut policies = Vec::new();
        let mut network = base.clone();
        network.network = grokforge_protocol::NetworkMode::Isolated;
        policies.push(network);
        let mut unreadable = base.clone();
        unreadable.unreadable_globs.push("**/.env".to_string());
        policies.push(unreadable);
        let mut writes = base;
        writes.writable_roots = vec![dir.path().to_path_buf()];
        policies.push(writes);

        for policy in policies {
            assert!(matches!(
                PassthroughRunner.run(&policy, &spec).await,
                Err(ExecError::UnsupportedPolicy(_))
            ));
        }
    }
}
