//! macOS Seatbelt backend: confines commands with a generated SBPL profile run via
//! `/usr/bin/sandbox-exec`. Enforced by the kernel — writes are confined to the workspace,
//! `.git` stays read-only, and network is denied in workspace-write mode.
//!
//! Two correctness details learned the hard way:
//! - Paths must be **canonicalized** (symlinks resolved), because Seatbelt matches on the
//!   physical path — `/var/folders/...` vs `/private/var/folders/...`.
//! - Paths are passed as `-D` **parameters**, never interpolated into the profile text, so a
//!   path containing `"` or `)` can't break out of the policy (injection guard).

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use grokforge_protocol::{NetworkMode, SandboxMode, SandboxPolicy};

use crate::classifier::classify;
use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::privacy::{
    PrivacyPath, prepare_privacy_candidates, privacy_path_candidates,
    validate_session_storage_aliases,
};
use crate::protected::validate_existing_protected_trees;
use crate::unreadable::discover_unreadable_paths;
use crate::writable::validate_confined_trees;
use crate::{SandboxCapability, SandboxRunner, is_truly_unconstrained, validate_backend_policy};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const MAX_FROZEN_PATHS: usize = 2_048;

/// The SBPL profile. Paths are referenced by parameter name only. Secret globs are translated
/// to escaped regular expressions because Seatbelt does not accept parameters inside regexes.
#[allow(clippy::format_push_string)]
fn build_profile(
    num_writable: usize,
    num_protected: usize,
    num_private: usize,
    num_frozen: usize,
    deny_network: bool,
    unreadable_globs: &[String],
) -> String {
    let mut p = String::from(
        "(version 1)\n(allow default)\n(deny mach-lookup)\n(deny appleevent-send)\n(deny signal)\n(allow signal (target same-sandbox))\n",
    );
    if deny_network {
        p.push_str("(deny network*)\n");
    }
    // Deny writes everywhere, then re-allow the writable roots, then re-deny protected paths
    // (last match wins in SBPL).
    p.push_str("(deny file-write* (subpath \"/\"))\n");
    p.push_str("(allow file-write-data (literal \"/dev/null\"))\n");
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
    for i in 0..num_private {
        p.push_str(&format!(
            "(deny file-read* (subpath (param \"SECRET{i}\")))\n"
        ));
        p.push_str(&format!(
            "(deny file-write* (subpath (param \"SECRET{i}\")))\n"
        ));
    }
    for i in 0..num_frozen {
        p.push_str(&format!(
            "(deny file-write* (literal (param \"FROZEN{i}\")))\n"
        ));
    }
    for glob in unreadable_globs {
        let regex = glob_regex(glob);
        p.push_str("(deny file-read* (regex #\"");
        p.push_str(&escape_sbpl_string(&regex));
        p.push_str("\"))\n");
        p.push_str("(deny file-write* (regex #\"");
        p.push_str(&escape_sbpl_string(&regex));
        p.push_str("\"))\n");
    }
    p
}

/// Canonicalize a path. For a path that does not exist yet, resolve its nearest existing
/// ancestor so a symlinked workspace cannot escape a physical-path Seatbelt rule.
fn canonical(path: &Path) -> String {
    canonical_path(path).to_string_lossy().into_owned()
}

fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }

    let mut missing = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return lexical_absolute(path);
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return lexical_absolute(path);
        };
        ancestor = parent;
    }
    let mut resolved = std::fs::canonicalize(ancestor).unwrap_or_else(|_| lexical_absolute(path));
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    resolved
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn glob_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                regex.push_str(".*");
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '\\' => {
                if let Some(literal) = chars.next() {
                    push_case_insensitive_regex_literal(&mut regex, literal);
                } else {
                    regex.push_str("\\\\");
                }
            }
            _ => push_case_insensitive_regex_literal(&mut regex, ch),
        }
    }
    regex.push('$');
    regex
}

fn push_case_insensitive_regex_literal(regex: &mut String, ch: char) {
    if ch.is_ascii_alphabetic() {
        regex.push('[');
        regex.push(ch.to_ascii_lowercase());
        regex.push(ch.to_ascii_uppercase());
        regex.push(']');
        return;
    }
    match ch {
        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
            regex.push('\\');
            regex.push(ch);
        }
        '"' => regex.push_str("\\x22"),
        ch if ch.is_control() => {
            let _ = write!(regex, "\\x{:02x}", u32::from(ch));
        }
        _ => regex.push(ch),
    }
}

fn escape_sbpl_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            // `#"..."` is an SBPL regex literal: regex backslashes must remain single rather
            // than receiving normal Scheme-string escaping.
            '\\' => escaped.push('\\'),
            '"' => escaped.push_str("\\x22"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            ch if ch.is_control() => {
                let _ = write!(escaped, "\\x{:02x}", u32::from(ch));
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn supports_read_policy(policy: &SandboxPolicy) -> bool {
    policy.readable_roots.len() == 1 && policy.readable_roots[0] == Path::new("/")
}

fn frozen_ancestors(
    policy: &SandboxPolicy,
    protected: &[String],
    private: &[String],
) -> Result<Vec<PathBuf>, ExecError> {
    if policy.mode == SandboxMode::ReadOnly {
        return Ok(Vec::new());
    }
    let mut writable_roots = Vec::new();
    for root in &policy.writable_roots {
        writable_roots.push(canonical_path(root));
        writable_roots.push(lexical_absolute(root));
    }
    writable_roots.sort();
    writable_roots.dedup();

    let mut frozen = Vec::new();
    for guarded in protected.iter().chain(private) {
        let guarded = Path::new(guarded);
        let Some(root) = writable_roots
            .iter()
            .filter(|root| guarded.starts_with(root))
            .min_by_key(|root| root.components().count())
        else {
            continue;
        };
        let mut ancestor = guarded.parent();
        while let Some(path) = ancestor {
            if path == root || !path.starts_with(root) {
                break;
            }
            frozen.push(path.to_path_buf());
            ancestor = path.parent();
        }
    }
    frozen.sort();
    frozen.dedup();
    if frozen.len() > MAX_FROZEN_PATHS {
        return Err(ExecError::UnsupportedPolicy(format!(
            "Seatbelt policy requires more than {MAX_FROZEN_PATHS} frozen ancestor paths"
        )));
    }
    Ok(frozen)
}

/// A runner that wraps commands in `sandbox-exec`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeatbeltRunner;

impl SeatbeltRunner {
    /// Whether Seatbelt is usable on this machine (runs `/usr/bin/true` under a valid profile).
    #[must_use]
    pub fn available() -> bool {
        let mut command = std::process::Command::new(SANDBOX_EXEC);
        command
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    fn wrap(
        policy: &SandboxPolicy,
        command: &CommandSpec,
        private_temp: Option<&Path>,
        private_paths: &[PrivacyPath],
    ) -> Result<CommandSpec, ExecError> {
        // Proxy routing is not implemented yet; isolate it rather than silently granting full
        // network access.
        let deny_network = !matches!(policy.network, NetworkMode::Full);
        let writable: &[PathBuf] = match policy.mode {
            SandboxMode::ReadOnly => &[],
            SandboxMode::WorkspaceWrite | SandboxMode::DangerFullAccess => &policy.writable_roots,
        };
        let allow_temp = policy.mode == SandboxMode::WorkspaceWrite && private_temp.is_some();
        let mut protected = Vec::new();
        for path in &policy.protected_paths {
            protected.push(canonical(path));
            protected.push(lexical_absolute(path).to_string_lossy().into_owned());
        }
        protected.sort();
        protected.dedup();
        let mut private = Vec::new();
        for path in private_paths {
            private.push(canonical(&path.path));
            private.extend(
                path.lexical_paths
                    .iter()
                    .map(|path| lexical_absolute(path).to_string_lossy().into_owned()),
            );
        }
        private.sort();
        private.dedup();
        let frozen = frozen_ancestors(policy, &protected, &private)?;
        let profile = build_profile(
            writable.len() + usize::from(allow_temp),
            protected.len(),
            private.len(),
            frozen.len(),
            deny_network,
            &policy.unreadable_globs,
        );

        let mut args = vec!["-p".to_string(), profile];
        // Writable roots as WS0..WSn. Only workspace-write receives a writable temp directory;
        // read-only must remain genuinely read-only.
        for (i, root) in writable.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("WS{i}={}", canonical(root)));
        }
        if allow_temp {
            let tmp_index = writable.len();
            args.push("-D".to_string());
            args.push(format!(
                "WS{tmp_index}={}",
                canonical(private_temp.unwrap_or_else(|| Path::new("/nonexistent")))
            ));
        }
        for (i, prot) in protected.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("GIT{i}={prot}"));
        }
        for (i, private) in private.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("SECRET{i}={private}"));
        }
        for (i, frozen) in frozen.iter().enumerate() {
            args.push("-D".to_string());
            args.push(format!("FROZEN{i}={}", frozen.to_string_lossy()));
        }
        // The command itself. Point TMPDIR at the per-command directory rather than granting
        // access to the process-wide macOS temp tree used by other applications.
        if let Some(temp) = private_temp {
            args.push("/usr/bin/env".to_string());
            args.push(format!("TMPDIR={}", canonical(temp)));
        }
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());

        Ok(CommandSpec {
            program: SANDBOX_EXEC.to_string(),
            args,
            cwd: command.cwd.clone(),
            timeout: command.timeout,
            cancellation: command.cancellation.clone(),
        })
    }

    async fn run_inner(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
        discover_ambient_privacy: bool,
    ) -> Result<ExecOutput, ExecError> {
        validate_backend_policy(policy, command)?;
        if !supports_read_policy(policy) {
            return Err(ExecError::UnsupportedPolicy(
                "Seatbelt backend only supports readable_roots = [\"/\"]".to_string(),
            ));
        }
        // Seatbelt rules are path based. Validate protected physical contents and explicit
        // credential/session paths off the async executor so aliases cannot leave a second
        // writable/readable name outside the rule.
        let protected_paths = policy.protected_paths.clone();
        let policy_for_scan = policy.clone();
        let cwd = command.cwd.clone();
        let workspace_roots = policy.writable_roots.clone();
        let include_privacy =
            discover_ambient_privacy && policy.mode != SandboxMode::DangerFullAccess;
        let private_paths = tokio::task::spawn_blocking(move || {
            validate_existing_protected_trees(&protected_paths)?;
            validate_confined_trees(&policy_for_scan, &cwd)?;
            let mut candidates = discover_unreadable_paths(&policy_for_scan, &cwd)?;
            if include_privacy {
                validate_session_storage_aliases(&cwd)?;
                candidates.extend(privacy_path_candidates(&cwd));
            }
            prepare_privacy_candidates(candidates, &cwd, &workspace_roots)
        })
        .await
        .map_err(|error| {
            ExecError::UnsupportedPolicy(format!(
                "Seatbelt path-validation task did not complete: {error}"
            ))
        })??;
        // A truly unrestricted policy needs no wrapper. The standard danger policy still has
        // `.git` protection, so it intentionally goes through Seatbelt.
        if is_truly_unconstrained(policy) {
            return run_capture(command).await;
        }
        let private_temp = if policy.mode == SandboxMode::WorkspaceWrite {
            Some(
                tempfile::Builder::new()
                    .prefix("grokforge-sandbox-")
                    .tempdir()
                    .map_err(ExecError::Io)?,
            )
        } else {
            None
        };
        let wrapped = Self::wrap(
            policy,
            command,
            private_temp.as_ref().map(tempfile::TempDir::path),
            &private_paths,
        )?;
        let mut out = run_capture(&wrapped).await?;
        out.denial = classify(policy, &out);
        Ok(out)
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
                    "configured secret globs plus explicit XDG/home credential and GrokForge session paths are denied"
                        .to_string(),
                    "Mach service lookup and Apple Events are denied; inherited descriptors and process visibility remain separate boundaries"
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
        self.run_inner(policy, command, true).await
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
            cancellation: None,
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
        let out = runner.run_inner(&policy, &inside, false).await.unwrap();
        assert!(out.succeeded(), "workspace write should succeed: {out:?}");

        // Writing outside the workspace is denied by the kernel.
        let outside = spec(
            "/bin/sh",
            &["-c", "echo hi > /tmp/grokforge_should_not_exist.txt"],
            ws.path().to_path_buf(),
        );
        let out = runner.run_inner(&policy, &outside, false).await.unwrap();
        assert!(!out.succeeded(), "outside write must be denied");
        assert_eq!(out.denial, Some(grokforge_protocol::DenialClass::FsWrite));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn danger_full_access_runs_unwrapped() {
        let ws = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::danger_full_access(ws.path());
        policy.protected_paths.clear();
        let out = SeatbeltRunner
            .run(
                &policy,
                &spec("/bin/echo", &["hi"], ws.path().to_path_buf()),
            )
            .await
            .unwrap();
        assert!(out.succeeded());
    }

    #[test]
    fn secret_globs_are_escaped_without_profile_injection() {
        let profile = build_profile(
            0,
            0,
            0,
            0,
            false,
            &["**/.env\")\n(allow file-read*)\n;\"".to_string()],
        );
        assert!(!profile.contains("\n(allow file-read*)\n"));
        assert!(profile.contains("\\x22"));
    }

    #[test]
    fn secret_glob_regex_is_ascii_case_insensitive() {
        let regex = glob_regex("**/.env");
        assert!(regex.contains("[eE][nN][vV]"), "{regex}");
        let profile = build_profile(1, 0, 0, 0, true, &["**/.env".to_string()]);
        assert!(profile.contains("(deny file-read* (regex"));
        assert!(profile.contains("(deny file-write* (regex"));
        assert!(profile.contains("(deny signal)"));
        assert!(profile.contains("(allow signal (target same-sandbox))"));
        assert!(profile.contains("(deny mach-lookup)"));
        assert!(profile.contains("(deny appleevent-send)"));
    }

    #[tokio::test]
    async fn confined_process_cannot_resolve_apps_through_launch_services() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::workspace_write(workspace.path());

        for application in ["Safari", "Finder"] {
            let command = spec(
                "/usr/bin/open",
                &["-Ra", application],
                workspace.path().to_path_buf(),
            );
            let output = SeatbeltRunner
                .run_inner(&policy, &command, false)
                .await
                .expect("run LaunchServices attempt");
            assert!(
                !output.succeeded(),
                "sandbox resolved {application} through LaunchServices: {output:?}"
            );
        }
    }

    #[test]
    fn private_target_freezes_only_ancestors_below_the_workspace_root() {
        let workspace = tempfile::tempdir().expect("workspace");
        let secret_dir = workspace.path().join("secret-dir");
        std::fs::create_dir(&secret_dir).expect("secret dir");
        let target = secret_dir.join("ordinary");
        std::fs::write(&target, "private").expect("target");
        let private = PrivacyPath {
            path: target.clone(),
            is_dir: false,
            lexical_paths: vec![workspace.path().join(".env")],
        };
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec("/usr/bin/true", &[], workspace.path().to_path_buf()),
            None,
            &[private],
        )
        .expect("wrap Seatbelt command");
        let frozen: Vec<&str> = wrapped
            .args
            .iter()
            .filter_map(|arg| arg.strip_prefix("FROZEN0="))
            .collect();
        assert_eq!(frozen, [canonical(&secret_dir)]);
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg == &format!("FROZEN0={}", canonical(workspace.path())))
        );
    }

    #[tokio::test]
    async fn uppercase_env_is_denied_when_seatbelt_is_available() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().expect("workspace");
        std::fs::write(ws.path().join(".ENV"), "UPPERCASE SECRET").expect("secret");
        let policy = SandboxPolicy::workspace_write(ws.path());
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec("/bin/cat", &[".ENV"], ws.path().to_path_buf()),
            None,
            &[],
        )
        .expect("wrap Seatbelt command");
        let output = run_capture(&wrapped).await.expect("run seatbelt");
        assert!(
            !output.succeeded(),
            "uppercase secret was readable: {output:?}"
        );
        assert!(!output.stdout.contains("UPPERCASE SECRET"));
    }

    #[tokio::test]
    async fn read_only_does_not_allow_temp_or_workspace_writes() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::read_only(ws.path());
        let out = SeatbeltRunner
            .run_inner(
                &policy,
                &spec(
                    "/bin/sh",
                    &[
                        "-c",
                        "touch workspace-write; touch \"$TMPDIR/readonly-write\"",
                    ],
                    ws.path().to_path_buf(),
                ),
                false,
            )
            .await
            .unwrap();
        assert!(
            !out.succeeded(),
            "read-only command wrote successfully: {out:?}"
        );
        assert!(!ws.path().join("workspace-write").exists());
        assert!(!std::env::temp_dir().join("readonly-write").exists());
    }

    #[tokio::test]
    async fn unreadable_globs_block_secret_reads() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(".env"), "SECRET=value\n").unwrap();
        let policy = SandboxPolicy::workspace_write(ws.path());
        let out = SeatbeltRunner
            .run_inner(
                &policy,
                &spec("/bin/cat", &[".env"], ws.path().to_path_buf()),
                false,
            )
            .await
            .unwrap();
        assert!(!out.succeeded(), "secret read escaped policy: {out:?}");
        assert!(!out.stdout.contains("SECRET=value"));
    }

    #[tokio::test]
    async fn unreadable_glob_name_cannot_be_renamed_before_reading() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let secret = workspace.path().join(".env");
        std::fs::write(&secret, "PRIVATE ENV").expect("secret");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec(
            "/bin/sh",
            &["-c", "mv .env ordinary && cat ordinary"],
            workspace.path().to_path_buf(),
        );

        let output = SeatbeltRunner
            .run_inner(&policy, &command, false)
            .await
            .expect("run seatbelt");
        assert!(!output.succeeded(), "secret rename succeeded: {output:?}");
        assert!(!output.stdout.contains("PRIVATE ENV"));
        assert!(secret.exists(), "matched secret name was renamed");
        assert!(!workspace.path().join("ordinary").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_glob_cannot_be_hardlinked_to_a_readable_name() {
        use std::os::unix::fs::MetadataExt as _;

        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let secret = workspace.path().join(".env");
        std::fs::write(&secret, "PRIVATE ENV").expect("secret");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec(
            "/bin/sh",
            &["-c", "ln .env ordinary && cat ordinary"],
            workspace.path().to_path_buf(),
        );

        let output = SeatbeltRunner
            .run_inner(&policy, &command, false)
            .await
            .expect("run seatbelt");
        assert!(!output.succeeded(), "secret hardlink succeeded: {output:?}");
        assert!(!output.stdout.contains("PRIVATE ENV"));
        assert!(!workspace.path().join("ordinary").exists());
        assert_eq!(
            std::fs::metadata(secret).expect("secret metadata").nlink(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_symlink_target_is_physically_denied() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("ordinary");
        std::fs::write(&target, "PRIVATE TARGET").expect("target");
        std::os::unix::fs::symlink("ordinary", workspace.path().join(".env"))
            .expect("secret symlink");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec("/bin/cat", &["ordinary"], workspace.path().to_path_buf());

        let output = SeatbeltRunner
            .run_inner(&policy, &command, false)
            .await
            .expect("run seatbelt");
        assert!(
            !output.succeeded(),
            "physical target was readable: {output:?}"
        );
        assert!(!output.stdout.contains("PRIVATE TARGET"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_target_ancestor_cannot_be_renamed_but_sibling_directory_can() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let secret_dir = workspace.path().join("secret-dir");
        std::fs::create_dir(&secret_dir).expect("secret dir");
        std::fs::write(secret_dir.join("ordinary"), "PRIVATE TARGET").expect("target");
        std::os::unix::fs::symlink("secret-dir/ordinary", workspace.path().join(".env"))
            .expect("secret symlink");
        let sibling = workspace.path().join("sibling-dir");
        std::fs::create_dir(&sibling).expect("sibling dir");
        std::fs::write(sibling.join("ordinary"), "public").expect("sibling file");
        let policy = SandboxPolicy::workspace_write(workspace.path());

        let relabel = spec(
            "/bin/sh",
            &["-c", "mv secret-dir renamed && cat renamed/ordinary"],
            workspace.path().to_path_buf(),
        );
        let output = SeatbeltRunner
            .run_inner(&policy, &relabel, false)
            .await
            .expect("run Seatbelt relabel attempt");
        assert!(
            !output.succeeded(),
            "private ancestor rename succeeded: {output:?}"
        );
        assert!(!output.stdout.contains("PRIVATE TARGET"));
        assert!(secret_dir.exists());
        assert!(!workspace.path().join("renamed").exists());

        let sibling_relabel = spec(
            "/bin/sh",
            &[
                "-c",
                "mv sibling-dir sibling-renamed && printf ok > sibling-renamed/new",
            ],
            workspace.path().to_path_buf(),
        );
        let output = SeatbeltRunner
            .run_inner(&policy, &sibling_relabel, false)
            .await
            .expect("run Seatbelt sibling rename");
        assert!(
            output.succeeded(),
            "ordinary sibling rename failed: {output:?}"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("sibling-renamed/new"))
                .expect("sibling output"),
            "ok"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_hardlink_alias_is_rejected_before_seatbelt_spawn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join(".env");
        std::fs::write(&target, "PRIVATE TARGET").expect("target");
        std::fs::hard_link(&target, workspace.path().join("ordinary")).expect("hardlink");
        let policy = SandboxPolicy::read_only(workspace.path());
        let command = spec("/usr/bin/true", &[], workspace.path().to_path_buf());

        assert!(matches!(
            SeatbeltRunner.run_inner(&policy, &command, false).await,
            Err(ExecError::UnsupportedPolicy(message))
                if message.contains("multiple hard links") || message.contains("hard-linked file")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_session_path_is_parameterized_as_read_denial() {
        let ws = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let private = PrivacyPath {
            path: sessions.path().to_path_buf(),
            is_dir: true,
            lexical_paths: vec![sessions.path().to_path_buf()],
        };
        let policy = SandboxPolicy::workspace_write(ws.path());
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec("/usr/bin/true", &[], ws.path().to_path_buf()),
            Some(ws.path()),
            std::slice::from_ref(&private),
        )
        .expect("wrap Seatbelt command");
        assert!(wrapped.args[1].contains("(deny file-read* (subpath (param \"SECRET0\")))"));
        assert!(wrapped.args[1].contains("(deny file-write* (subpath (param \"SECRET0\")))"));
        assert!(
            wrapped
                .args
                .iter()
                .any(|arg| { arg == &format!("SECRET0={}", canonical(&private.path)) })
        );
    }

    #[tokio::test]
    async fn explicit_session_path_cannot_be_read_when_seatbelt_is_available() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().expect("workspace");
        let sessions = ws.path().join("sessions");
        std::fs::create_dir(&sessions).expect("sessions");
        let rollout = sessions.join("rollout-private.jsonl");
        std::fs::write(&rollout, "PRIVATE SESSION").expect("rollout");
        let policy = SandboxPolicy::workspace_write(ws.path());
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec(
                "/bin/cat",
                &[rollout.to_string_lossy().as_ref()],
                ws.path().to_path_buf(),
            ),
            Some(ws.path()),
            &[PrivacyPath {
                path: sessions.clone(),
                is_dir: true,
                lexical_paths: vec![sessions.clone()],
            }],
        )
        .expect("wrap Seatbelt command");
        let output = run_capture(&wrapped).await.expect("run seatbelt");
        assert!(
            !output.succeeded(),
            "private rollout was readable: {output:?}"
        );
        assert!(!output.stdout.contains("PRIVATE SESSION"));

        let overwrite = format!("printf 'ESCAPED' > '{}'", rollout.display());
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec("/bin/sh", &["-c", &overwrite], ws.path().to_path_buf()),
            Some(ws.path()),
            &[PrivacyPath {
                path: sessions.clone(),
                is_dir: true,
                lexical_paths: vec![sessions],
            }],
        )
        .expect("wrap Seatbelt command");
        let output = run_capture(&wrapped).await.expect("run seatbelt");
        assert!(
            !output.succeeded(),
            "private rollout was writable: {output:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&rollout).expect("rollout remains"),
            "PRIVATE SESSION"
        );
    }

    #[tokio::test]
    async fn danger_mode_still_blocks_git_writes() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        let policy = SandboxPolicy::danger_full_access(ws.path());
        let out = SeatbeltRunner
            .run(
                &policy,
                &spec(
                    "/bin/sh",
                    &["-c", "echo escaped > .git/config"],
                    ws.path().to_path_buf(),
                ),
            )
            .await
            .unwrap();
        assert!(
            !out.succeeded(),
            "danger mode mutated protected .git: {out:?}"
        );
        assert!(!ws.path().join(".git/config").exists());
    }

    #[tokio::test]
    async fn danger_mode_cannot_relabel_git_or_private_store_ancestors() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let git = workspace.path().join(".git");
        std::fs::create_dir(&git).expect("git dir");
        let store_parent = tempfile::tempdir().expect("store parent");
        let store = store_parent.path().join("private-store");
        let sessions = store.join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let rollout = sessions.join("rollout.jsonl");
        std::fs::write(&rollout, "PRIVATE SESSION").expect("rollout");
        let mut policy = SandboxPolicy::danger_full_access(workspace.path());
        policy.protected_paths = vec![git.clone(), sessions.clone()];
        let session_literal = globset::escape(&sessions.to_string_lossy());
        policy.unreadable_globs = vec![session_literal.clone(), format!("{session_literal}/**")];

        let renamed_workspace_holder = tempfile::tempdir().expect("renamed workspace holder");
        let renamed_workspace = renamed_workspace_holder.path().to_path_buf();
        drop(renamed_workspace_holder);
        let relabel_workspace = format!(
            "mv '{}' '{}' && printf ESCAPED > '{}/.git/config'",
            workspace.path().display(),
            renamed_workspace.display(),
            renamed_workspace.display()
        );
        let output = SeatbeltRunner
            .run_inner(
                &policy,
                &spec(
                    "/bin/sh",
                    &["-c", &relabel_workspace],
                    workspace.path().to_path_buf(),
                ),
                false,
            )
            .await
            .expect("run workspace relabel attempt");
        assert!(
            !output.succeeded(),
            "workspace relabel succeeded: {output:?}"
        );
        assert!(workspace.path().exists());
        assert!(!renamed_workspace.exists());
        assert!(!git.join("config").exists());

        let renamed_store = store_parent.path().join("renamed-store");
        let relabel_store = format!(
            "mv '{}' '{}' && printf ESCAPED > '{}/sessions/rollout.jsonl'",
            store.display(),
            renamed_store.display(),
            renamed_store.display()
        );
        let output = SeatbeltRunner
            .run_inner(
                &policy,
                &spec(
                    "/bin/sh",
                    &["-c", &relabel_store],
                    workspace.path().to_path_buf(),
                ),
                false,
            )
            .await
            .expect("run store relabel attempt");
        assert!(
            !output.succeeded(),
            "private store relabel succeeded: {output:?}"
        );
        assert!(store.exists());
        assert!(!renamed_store.exists());
        assert_eq!(
            std::fs::read_to_string(rollout).expect("rollout remains"),
            "PRIVATE SESSION"
        );
    }

    #[tokio::test]
    async fn confined_process_cannot_signal_an_unsandboxed_process() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let mut helper = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn unsandboxed helper");
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let script = format!(
            "kill -0 {pid}; zero=$?; kill -TERM {pid}; term=$?; printf '%s %s' \"$zero\" \"$term\"",
            pid = helper.id()
        );
        let result = SeatbeltRunner
            .run_inner(
                &policy,
                &spec("/bin/sh", &["-c", &script], workspace.path().to_path_buf()),
                false,
            )
            .await;
        let helper_survived = helper.try_wait().expect("inspect helper").is_none();
        let _ = helper.kill();
        let _ = helper.wait();

        let output = result.expect("run Seatbelt signal attempt");
        let statuses: Vec<u32> = output
            .stdout
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        assert_eq!(
            statuses.len(),
            2,
            "unexpected signal status output: {output:?}"
        );
        assert!(statuses.iter().all(|status| *status != 0), "{output:?}");
        assert!(
            helper_survived,
            "sandbox command signaled unsandboxed helper"
        );
    }

    #[tokio::test]
    async fn confined_process_can_signal_and_wait_for_its_own_child() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec(
            "/bin/sh",
            &[
                "-c",
                "sleep 30 & child=$!; kill -TERM \"$child\"; wait \"$child\"; test $? -gt 0",
            ],
            workspace.path().to_path_buf(),
        );

        let output = SeatbeltRunner
            .run_inner(&policy, &command, false)
            .await
            .expect("run Seatbelt child signal");
        assert!(
            output.succeeded(),
            "same-sandbox signal was denied: {output:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_hardlink_alias_is_rejected_before_seatbelt_spawn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "original").expect("outside target");
        std::fs::hard_link(&target, workspace.path().join("alias")).expect("workspace hardlink");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec("/usr/bin/true", &[], workspace.path().to_path_buf());

        assert!(matches!(
            SeatbeltRunner.run_inner(&policy, &command, false).await,
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
        assert_eq!(
            std::fs::read_to_string(target).expect("outside target"),
            "original"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn narrow_danger_policy_hardlink_alias_is_rejected_before_seatbelt_spawn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "original").expect("outside target");
        std::fs::hard_link(&target, workspace.path().join("alias")).expect("workspace hardlink");
        let mut policy = SandboxPolicy::danger_full_access(workspace.path());
        policy.writable_roots = vec![workspace.path().to_path_buf()];
        policy.protected_paths.clear();
        let command = spec("/usr/bin/true", &[], workspace.path().to_path_buf());

        assert!(matches!(
            SeatbeltRunner.run_inner(&policy, &command, false).await,
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
        assert_eq!(
            std::fs::read_to_string(target).expect("outside target"),
            "original"
        );
    }

    #[test]
    fn ordinary_workspace_passes_seatbelt_writable_preflight() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("ordinary.txt"), "ordinary").expect("ordinary file");
        let policy = SandboxPolicy::workspace_write(workspace.path());

        validate_confined_trees(&policy, workspace.path()).expect("ordinary workspace");
    }

    #[cfg(unix)]
    #[test]
    fn protected_aliases_are_rejected_before_seatbelt_execution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let protected = tempfile::tempdir().expect("external protected tree");
        let target = workspace.path().join("target");
        std::fs::write(&target, "metadata").expect("target");
        std::os::unix::fs::symlink(&target, protected.path().join("config")).expect("symlink");
        assert!(matches!(
            validate_existing_protected_trees(&[protected.path().to_path_buf()]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
        ));

        std::fs::remove_file(protected.path().join("config")).expect("remove symlink");
        std::fs::hard_link(&target, protected.path().join("config")).expect("hard link");
        assert!(matches!(
            validate_existing_protected_trees(&[protected.path().to_path_buf()]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[tokio::test]
    async fn proxy_mode_is_isolated_until_proxy_routing_exists() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::workspace_write(ws.path());
        policy.network = NetworkMode::ProxyRouted;
        let wrapped = SeatbeltRunner::wrap(
            &policy,
            &spec("/usr/bin/true", &[], ws.path().to_path_buf()),
            Some(ws.path()),
            &[],
        )
        .expect("wrap Seatbelt command");
        assert!(wrapped.args[1].contains("(deny network*)"));
        assert!(!wrapped.args[1].contains("remote unix-socket"));
    }

    #[tokio::test]
    async fn workspace_write_uses_and_cleans_a_private_tmpdir() {
        if !SeatbeltRunner::available() {
            eprintln!("skipping: sandbox-exec unavailable");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::workspace_write(ws.path());
        let out = SeatbeltRunner
            .run_inner(
                &policy,
                &spec(
                    "/bin/sh",
                    &["-c", "printf '%s' \"$TMPDIR\"; touch \"$TMPDIR/ok\""],
                    ws.path().to_path_buf(),
                ),
                false,
            )
            .await
            .unwrap();
        assert!(out.succeeded(), "{out:?}");
        let private_temp = PathBuf::from(&out.stdout);
        assert!(
            private_temp
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("grokforge-sandbox-"))
        );
        assert!(
            !private_temp.exists(),
            "private temp directory was not cleaned"
        );
    }
}
