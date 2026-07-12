//! Linux backend via bubblewrap (`bwrap`): a read-only root with the workspace bind-mounted
//! writable, `.git` re-bound read-only, and the network namespace unshared in workspace-write
//! mode. Shelling out to `bwrap` avoids pinning an unstable Rust sandbox crate; in-process
//! Landlock + seccomp is the planned follow-up (docs/design/03-roadmap.md, Phase 2).

use async_trait::async_trait;
use grokforge_protocol::{NetworkMode, SandboxMode, SandboxPolicy};

use crate::classifier::classify;
use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::{SandboxCapability, SandboxRunner};

/// A runner that wraps commands in `bwrap`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BubblewrapRunner;

impl BubblewrapRunner {
    /// Whether `bwrap` is on PATH and runnable.
    #[must_use]
    pub fn available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn wrap(policy: &SandboxPolicy, command: &CommandSpec) -> CommandSpec {
        let mut args: Vec<String> = vec![
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--die-with-parent".into(),
        ];
        // Workspace roots writable.
        for root in &policy.writable_roots {
            let r = root.to_string_lossy().into_owned();
            args.push("--bind".into());
            args.push(r.clone());
            args.push(r);
        }
        // Protected paths (.git) re-bound read-only, overriding the writable bind above.
        for prot in &policy.protected_paths {
            if prot.exists() {
                let p = prot.to_string_lossy().into_owned();
                args.push("--ro-bind".into());
                args.push(p.clone());
                args.push(p);
            }
        }
        if matches!(policy.network, NetworkMode::Isolated) {
            args.push("--unshare-net".into());
        }
        args.push("--chdir".into());
        args.push(command.cwd.to_string_lossy().into_owned());
        args.push("--".into());
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());

        CommandSpec {
            program: "bwrap".into(),
            args,
            cwd: command.cwd.clone(),
            timeout: command.timeout,
        }
    }
}

#[async_trait]
impl SandboxRunner for BubblewrapRunner {
    fn capability(&self) -> SandboxCapability {
        if Self::available() {
            SandboxCapability {
                backend: "bubblewrap".to_string(),
                enforced: true,
                notes: vec![
                    "Linux bwrap: read-only root, workspace bind-mounted, network unshared"
                        .to_string(),
                    "in-process Landlock + seccomp is the planned upgrade".to_string(),
                ],
            }
        } else {
            SandboxCapability {
                backend: "bubblewrap (unavailable)".to_string(),
                enforced: false,
                notes: vec!["bwrap not found on PATH; falling back to approval-only".to_string()],
            }
        }
    }

    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        if policy.mode == SandboxMode::DangerFullAccess {
            return run_capture(command).await;
        }
        let wrapped = Self::wrap(policy, command);
        let mut out = run_capture(&wrapped).await?;
        out.denial = classify(policy, &out);
        Ok(out)
    }
}
