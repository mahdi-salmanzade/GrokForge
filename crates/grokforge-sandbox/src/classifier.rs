//! Denial classifier: decides whether a failed command was stopped *by the sandbox* (so the
//! UI can offer "retry unsandboxed") versus failing on its own. Heuristic by necessity — it
//! reads exit status and stderr patterns — but it never claims a denial for an unsandboxed run.

use grokforge_protocol::{DenialClass, NetworkMode, SandboxMode, SandboxPolicy};

use crate::exec::ExecOutput;

/// Inspect a finished command and classify a sandbox denial, if any.
#[must_use]
pub fn classify(policy: &SandboxPolicy, out: &ExecOutput) -> Option<DenialClass> {
    if out.succeeded() || policy.mode == SandboxMode::DangerFullAccess {
        return None;
    }
    let stderr = out.stderr.to_ascii_lowercase();

    // Network denials show up as resolver/connection failures when the policy blocks network.
    if policy.network != NetworkMode::Full
        && (stderr.contains("could not resolve")
            || stderr.contains("couldn't resolve")
            || stderr.contains("network is unreachable")
            || stderr.contains("network is down")
            || stderr.contains("temporary failure in name resolution")
            || (stderr.contains("socket") && stderr.contains("operation not permitted")))
    {
        return Some(DenialClass::Network);
    }

    // Seatbelt secret-glob denials are reads. Keep this narrow so an ordinary unreadable file
    // does not get mislabeled merely because the process itself lacked Unix permissions.
    if stderr.contains("operation not permitted")
        && (stderr.contains("cat:") || stderr.contains("cannot open") || stderr.contains("read "))
    {
        return Some(DenialClass::FsRead);
    }

    // Filesystem write denials surface as EPERM/EROFS under Seatbelt/bubblewrap. Bare
    // "permission denied" is intentionally insufficient: it also describes non-executable
    // files and normal discretionary-access failures unrelated to the sandbox.
    if stderr.contains("read-only file system")
        || stderr.contains("operation not permitted")
        || (stderr.contains("permission denied")
            && (stderr.contains("cannot create")
                || stderr.contains("mkdir:")
                || stderr.contains("touch:")))
    {
        return Some(DenialClass::FsWrite);
    }

    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn failed(stderr: &str) -> ExecOutput {
        ExecOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
            truncated: false,
            timed_out: false,
            denial: None,
        }
    }

    #[test]
    fn classifies_fs_denial() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/w"));
        assert_eq!(
            classify(&policy, &failed("/bin/sh: /etc/x: Operation not permitted")),
            Some(DenialClass::FsWrite)
        );
    }

    #[test]
    fn classifies_network_denial() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/w"));
        assert_eq!(
            classify(
                &policy,
                &failed("curl: (6) Could not resolve host: example.com")
            ),
            Some(DenialClass::Network)
        );
    }

    #[test]
    fn no_denial_for_full_access() {
        let policy = SandboxPolicy::danger_full_access(&PathBuf::from("/w"));
        assert_eq!(classify(&policy, &failed("Operation not permitted")), None);
    }

    #[test]
    fn no_denial_on_success() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/w"));
        let ok = ExecOutput {
            exit_code: Some(0),
            stdout: "fine".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            denial: None,
        };
        assert_eq!(classify(&policy, &ok), None);
    }

    #[test]
    fn ordinary_permission_and_connection_failures_are_not_escape_prompts() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/w"));
        assert_eq!(
            classify(&policy, &failed("./script.sh: Permission denied")),
            None
        );
        assert_eq!(
            classify(
                &policy,
                &failed("curl: (7) Failed to connect to 127.0.0.1:9: Connection refused")
            ),
            None
        );
        assert_eq!(
            classify(
                &policy,
                &failed("application sandbox configuration invalid")
            ),
            None
        );
    }

    #[test]
    fn secret_read_denial_is_classified_as_read() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/w"));
        assert_eq!(
            classify(&policy, &failed("cat: .env: Operation not permitted")),
            Some(DenialClass::FsRead)
        );
    }
}
