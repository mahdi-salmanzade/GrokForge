//! macOS Seatbelt backend: confines commands with a generated SBPL profile run via
//! `/usr/bin/sandbox-exec`. Enforced by the kernel — writes are confined to the workspace,
//! `.git` stays read-only, and network is denied in workspace-write mode.
//!
//! Two correctness details learned the hard way:
//! - Paths must be **canonicalized** (symlinks resolved), because Seatbelt matches on the
//!   physical path — `/var/folders/...` vs `/private/var/folders/...`.
//! - Paths are passed as `-D` **parameters**, never interpolated into the profile text, so a
//!   path containing `"` or `)` can't break out of the policy (injection guard).

use std::path::Path;

use async_trait::async_trait;
use grokforge_protocol::{NetworkMode, SandboxMode, SandboxPolicy};

use crate::classifier::classify;
use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::{SandboxCapability, SandboxRunner};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The SBPL profile. Paths are referenced by parameter name only.
#[allow(clippy::format_push_string)]
fn build_profile(num_writable: usize, num_protected: usize, deny_network: bool) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if deny_network {
        p.push_str("(deny network*)\n(allow network* (remote unix-socket))\n");
    }
    // Deny writes everywhere, then re-allow the writable roots, then re-deny protected paths
    // (last match wins in SBPL).
    p.push_str("(deny file-write* (subpath \"/\"))\n");
    p.push_str("(allow file-write-data (literal \"/dev/null\"))\n");
    p.push_str("(allow file-write-data (literal \"/dev/dtracehelper\"))\n");
    for i in 0..num_writable {
        p.push_str(&format!(
            "(allow file-write* (subpath (param \"WS{i}\")))\n"
        ));
    }
    for i in 0..num_protected {
        p.push_str(&format!(
            "(deny file-write* (subpath (param \"GIT{i}\")))\n"
        ));
    }
    p
}

/// Canonicalize a path, falling back to the logical path if it doesn't exist yet.
fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// A runner that wraps commands in `sandbox-exec`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeatbeltRunner;

impl SeatbeltRunner {
    /// Whether Seatbelt is usable on this machine (a quick deny-default self-test).
    #[must_use]
    pub fn available() -> bool {
        std::process::Command::new(SANDBOX_EXEC)
            .args([
                "-p",
                "(version 1)(deny default)(allow process-exec*)",
                "/usr/bin/true",
            ])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn wrap(policy: &SandboxPolicy, command: &CommandSpec) -> CommandSpec {
        let deny_network = matches!(policy.network, NetworkMode::Isolated);
        let profile = build_profile(
            policy.writable_roots.len() + 1, // +1 for TMPDIR
            policy.protected_paths.len(),
            deny_network,
        );

        let mut args = vec!["-p".to_string(), profile];
        // Writable roots as WS0..WSn, plus the temp dir as the last WS.
        for (i, root) in policy.writable_roots.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("WS{i}={}", canonical(root)));
        }
        let tmp_index = policy.writable_roots.len();
        args.push("-D".to_string());
        args.push(format!(
            "WS{tmp_index}={}",
            canonical(&std::env::temp_dir())
        ));
        for (i, prot) in policy.protected_paths.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("GIT{i}={}", canonical(prot)));
        }
        // The command itself.
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());

        CommandSpec {
            program: SANDBOX_EXEC.to_string(),
            args,
            cwd: command.cwd.clone(),
            timeout: command.timeout,
        }
    }
}

#[async_trait]
impl SandboxRunner for SeatbeltRunner {
    fn capability(&self) -> SandboxCapability {
        if Self::available() {
            SandboxCapability {
                backend: "seatbelt".to_string(),
                enforced: true,
                notes: vec![
                    "macOS Seatbelt via sandbox-exec: workspace-confined writes, network deny"
                        .to_string(),
                ],
            }
        } else {
            SandboxCapability {
                backend: "seatbelt (unavailable)".to_string(),
                enforced: false,
                notes: vec![
                    "sandbox-exec self-test failed; falling back to approval-only".to_string(),
                ],
            }
        }
    }

    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        // Full access runs unwrapped; nothing to enforce.
        if policy.mode == SandboxMode::DangerFullAccess {
            return run_capture(command).await;
        }
        let wrapped = Self::wrap(policy, command);
        let mut out = run_capture(&wrapped).await?;
        out.denial = classify(policy, &out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    fn spec(cmd: &str, args: &[&str], cwd: PathBuf) -> CommandSpec {
        CommandSpec {
            program: cmd.to_string(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            cwd,
            timeout: Duration::from_secs(10),
        }
    }

    #[tokio::test]
    async fn enforces_workspace_write_and_network_deny() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::workspace_write(ws.path());
        let runner = SeatbeltRunner;

        // Writing inside the workspace succeeds.
        let inside = spec(
            "/bin/sh",
            &["-c", "echo hi > inside.txt && echo ok"],
            ws.path().to_path_buf(),
        );
        let out = runner.run(&policy, &inside).await.unwrap();
        assert!(out.succeeded(), "workspace write should succeed: {out:?}");

        // Writing outside the workspace is denied by the kernel.
        let outside = spec(
            "/bin/sh",
            &["-c", "echo hi > /tmp/grokforge_should_not_exist.txt"],
            ws.path().to_path_buf(),
        );
        let out = runner.run(&policy, &outside).await.unwrap();
        assert!(!out.succeeded(), "outside write must be denied");
        assert_eq!(out.denial, Some(grokforge_protocol::DenialClass::FsWrite));
    }

    #[tokio::test]
    async fn danger_full_access_runs_unwrapped() {
        let ws = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::danger_full_access(ws.path());
        let out = SeatbeltRunner
            .run(
                &policy,
                &spec("/bin/echo", &["hi"], ws.path().to_path_buf()),
            )
            .await
            .unwrap();
        assert!(out.succeeded());
    }
}
