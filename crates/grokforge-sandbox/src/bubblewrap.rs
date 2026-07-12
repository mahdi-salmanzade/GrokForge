//! Linux backend via bubblewrap (`bwrap`): a read-only root with the workspace bind-mounted
//! writable, `.git` re-bound read-only, and the network namespace unshared in workspace-write
//! mode. Shelling out to `bwrap` avoids pinning an unstable Rust sandbox crate; in-process
//! Landlock + seccomp is the planned follow-up (docs/design/03-roadmap.md, Phase 2).

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grokforge_protocol::{NetworkMode, SandboxMode, SandboxPolicy};

use crate::classifier::classify;
use crate::exec::{CommandSpec, ExecError, ExecOutput, run_capture};
use crate::privacy::{
    privacy_path_candidates, validate_privacy_tree, validate_session_storage_aliases,
};
use crate::protected::validate_protected_tree;
use crate::unreadable::discover_unreadable_paths;
use crate::writable::validate_confined_trees;
use crate::{SandboxCapability, SandboxRunner, is_truly_unconstrained, validate_backend_policy};

const MAX_SECRET_MASKS: usize = 512;
const MIN_BWRAP_VERSION: (u32, u32, u32) = (0, 11, 2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_OUTPUT_CAP: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaskMount {
    path: PathBuf,
    is_dir: bool,
}

/// A runner that wraps commands in `bwrap`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BubblewrapRunner;

impl BubblewrapRunner {
    /// Whether `bwrap` is on PATH and runnable.
    #[must_use]
    pub fn available() -> bool {
        Self::probe().is_ok()
    }

    fn probe() -> Result<PathBuf, String> {
        let executable = resolve_bwrap()?;
        #[cfg(unix)]
        reject_setuid(&executable)?;

        let version = run_probe(&executable, &["--version"])?;
        if !version.status.success() {
            return Err(format!(
                "{} --version failed with {}",
                executable.display(),
                version.status
            ));
        }
        let text = String::from_utf8_lossy(&version.stdout);
        let parsed = parse_version(&text)
            .ok_or_else(|| format!("unrecognized bwrap version output: {}", text.trim()))?;
        if parsed < MIN_BWRAP_VERSION {
            return Err(format!(
                "bwrap {}.{}.{} is below required 0.11.2",
                parsed.0, parsed.1, parsed.2
            ));
        }

        let self_test = run_probe(
            &executable,
            &[
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--unshare-net",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--new-session",
                "--die-with-parent",
                "--cap-drop",
                "ALL",
                "--",
                "/bin/true",
            ],
        )?;
        if !self_test.status.success() {
            return Err(format!(
                "bubblewrap namespace self-test failed with {}: {}",
                self_test.status,
                String::from_utf8_lossy(&self_test.stderr).trim()
            ));
        }
        Ok(executable)
    }

    #[allow(clippy::too_many_lines)] // Policy compilation is clearest in mount-operation order.
    fn wrap(
        policy: &SandboxPolicy,
        command: &CommandSpec,
        unreadable: &[MaskMount],
        executable: &Path,
    ) -> CommandSpec {
        // An approved filesystem escalation is represented as WorkspaceWrite with `/` as its
        // writable root. Keep the mode (and therefore network isolation and secret masking)
        // intact while making the root bind writable for that one command.
        let root_is_writable = policy.mode != SandboxMode::ReadOnly
            && policy
                .writable_roots
                .iter()
                .map(|root| canonical(root))
                .any(|root| root == Path::new("/"));
        let root_mount = if policy.mode == SandboxMode::DangerFullAccess || root_is_writable {
            "--bind"
        } else {
            "--ro-bind"
        };
        let mut args: Vec<String> = vec![
            root_mount.into(),
            "/".into(),
            "/".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--unshare-uts".into(),
            "--new-session".into(),
            "--dev".into(),
            "/dev".into(),
            "--proc".into(),
            "/proc".into(),
            "--die-with-parent".into(),
            "--cap-drop".into(),
            "ALL".into(),
        ];

        let network_isolated = !matches!(policy.network, NetworkMode::Full);
        // Host control sockets cross the filesystem boundary (Docker can mount/write arbitrary
        // host paths), so hide them for every policy that still requires a wrapper. Only the
        // truly unrestricted danger-policy fast path in `run` may expose them.
        let hide_host_sockets = policy.mode != SandboxMode::DangerFullAccess
            || !policy.protected_paths.is_empty()
            || network_isolated;

        // Preserve workspaces below /tmp or /run through the private tmpfs mounts. Bubblewrap
        // resolves bind sources below its retained old root, so the original host path remains a
        // valid source after the destination-side runtime tree has been replaced.
        let mut writable: Vec<PathBuf> = match policy.mode {
            SandboxMode::ReadOnly => Vec::new(),
            SandboxMode::WorkspaceWrite | SandboxMode::DangerFullAccess => policy
                .writable_roots
                .iter()
                .map(|root| canonical(root))
                .filter(|root| root != Path::new("/"))
                .collect(),
        };
        let command_cwd = canonical(&command.cwd);
        if root_is_writable && is_below_hidden_runtime_root(&command_cwd) {
            // `/tmp` or `/run` will be replaced below. Keep an approved command's actual
            // workspace visible even when its abstract writable root is `/`.
            writable.push(command_cwd.clone());
        }
        writable.sort();
        writable.dedup();
        let read_only_tmp_roots: Vec<PathBuf> =
            if hide_host_sockets && policy.mode == SandboxMode::ReadOnly {
                let mut roots: Vec<PathBuf> = policy
                    .protected_paths
                    .iter()
                    .filter_map(|path| path.parent())
                    .map(canonical)
                    .filter(|path| command_cwd.starts_with(path))
                    .filter(|path| is_below_hidden_runtime_root(path))
                    .collect();
                if is_below_hidden_runtime_root(&command_cwd) {
                    roots.push(command_cwd.clone());
                }
                roots.sort();
                roots.dedup();
                roots
            } else {
                Vec::new()
            };
        if hide_host_sockets {
            // A separate network namespace does not block AF_UNIX. Hide the host's common
            // runtime socket directories as well, otherwise Docker/Podman/DBus sockets become
            // an egress and host-write escape hatch.
            args.push("--tmpfs".into());
            args.push("/run".into());

            // Host /tmp commonly contains agent, database, and application sockets. Give
            // workspace-write a private temp area; remount it read-only in read-only mode.
            args.push("--tmpfs".into());
            args.push("/tmp".into());

            // `run_capture` deliberately drops the host TMPDIR. Advertise only this private
            // namespace-local temp directory to the inner command.
            args.push("--setenv".into());
            args.push("TMPDIR".into());
            args.push("/tmp".into());
        }
        // Workspace roots writable.
        for root in &writable {
            let r = root.to_string_lossy().into_owned();
            args.push("--bind".into());
            args.push(r.clone());
            args.push(r);
        }
        if hide_host_sockets && policy.mode == SandboxMode::ReadOnly {
            // If the workspace lives below /tmp or /run, restore it read-only after the private
            // tmpfs hid the host path.
            for root in &read_only_tmp_roots {
                let root = root.to_string_lossy().into_owned();
                args.push("--ro-bind".into());
                args.push(root.clone());
                args.push(root);
            }
        }
        // Protected paths (.git) re-bound read-only, overriding the writable bind above.
        for prot in &policy.protected_paths {
            if prot.exists() {
                let protected = canonical(prot);
                let p = protected.to_string_lossy().into_owned();
                args.push("--ro-bind".into());
                args.push(p.clone());
                args.push(p);
            }
        }
        if hide_host_sockets {
            // Delay read-only remounts until any `/tmp/...` or `/run/...` workspace/protected
            // destination directories have been created and rebound.
            args.push("--remount-ro".into());
            args.push("/run".into());
            if policy.mode == SandboxMode::ReadOnly {
                args.push("--remount-ro".into());
                args.push("/tmp".into());
            }
        }
        // Mask currently existing secret-bearing files in the workspace. Bubblewrap cannot
        // express globs directly, so this is paired with context redaction and an explicit
        // partial-enforcement capability note. Symlinks are not followed while discovering
        // matches.
        for secret in unreadable {
            let path = secret.path.to_string_lossy().into_owned();
            if secret.is_dir {
                args.push("--tmpfs".into());
                args.push(path.clone());
                // An unreadable directory is an empty, read-only mount in every confined mode.
                // Leaving it writable would create an unexpected write surface outside the
                // policy's writable roots, even though the writes would be ephemeral.
                args.push("--remount-ro".into());
                args.push(path);
            } else {
                args.push("--ro-bind".into());
                args.push("/dev/null".into());
                args.push(path);
            }
        }
        // Proxy routing is not implemented yet. Keep it isolated rather than silently granting
        // unrestricted access.
        if !matches!(policy.network, NetworkMode::Full) {
            args.push("--unshare-net".into());
        }
        args.push("--chdir".into());
        args.push(command.cwd.to_string_lossy().into_owned());
        args.push("--".into());
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());

        CommandSpec {
            program: executable.to_string_lossy().into_owned(),
            args,
            cwd: command.cwd.clone(),
            timeout: command.timeout,
            cancellation: command.cancellation.clone(),
        }
    }
}

#[async_trait]
impl SandboxRunner for BubblewrapRunner {
    fn capability(&self) -> SandboxCapability {
        match Self::probe() {
            Ok(executable) => SandboxCapability {
                backend: "bubblewrap".to_string(),
                enforced: true,
                notes: vec![
                    format!(
                        "Linux bwrap {}: read-only root, workspace bind-mounted, network unshared",
                        executable.display()
                    ),
                    "workspace secret globs plus explicit XDG/home credential and GrokForge session paths are masked; arbitrary host-path discovery remains bounded"
                        .to_string(),
                    "in-process Landlock + seccomp is the planned upgrade".to_string(),
                ],
            },
            Err(reason) => SandboxCapability {
                backend: "bubblewrap (unavailable)".to_string(),
                enforced: false,
                notes: vec![format!("{reason}; sandboxed commands fail closed")],
            },
        }
    }

    async fn run(
        &self,
        policy: &SandboxPolicy,
        command: &CommandSpec,
    ) -> Result<ExecOutput, ExecError> {
        validate_backend_policy(policy, command)?;
        if !supports_read_policy(policy) {
            return Err(ExecError::UnsupportedPolicy(
                "bubblewrap backend only supports readable_roots = [\"/\"]".to_string(),
            ));
        }
        if policy
            .writable_roots
            .iter()
            .map(|path| canonical(path))
            .any(|path| path == Path::new("/tmp") || path == Path::new("/run"))
        {
            return Err(ExecError::UnsupportedPolicy(
                "a workspace rooted at /tmp or /run cannot be isolated from host runtime sockets"
                    .to_string(),
            ));
        }
        if is_truly_unconstrained(policy) {
            return run_capture(command).await;
        }
        let executable = tokio::task::spawn_blocking(Self::probe)
            .await
            .map_err(|error| {
                ExecError::UnsupportedPolicy(format!(
                    "bubblewrap probe task did not complete: {error}"
                ))
            })?
            .map_err(ExecError::UnsupportedPolicy)?;
        let policy_for_scan = policy.clone();
        let command_for_scan = command.clone();
        let unreadable = tokio::task::spawn_blocking(move || {
            validate_protected_mountpoints(&policy_for_scan)?;
            validate_confined_trees(&policy_for_scan, &command_for_scan.cwd)?;
            mask_mounts(&policy_for_scan, &command_for_scan)
        })
        .await
        .map_err(|error| {
            ExecError::UnsupportedPolicy(format!("secret-path scan task did not complete: {error}"))
        })??;
        let wrapped = Self::wrap(policy, command, &unreadable, &executable);
        let mut out = run_capture(&wrapped).await?;
        out.denial = classify(policy, &out);
        Ok(out)
    }
}

/// A bind mount can pin an existing protected file or directory against rename/replacement, but
/// it cannot express a negative rule for a path that does not exist below a writable bind. Fail
/// closed instead of letting a command create fresh `.git` metadata after sandbox setup. Symlink
/// mountpoints are rejected because binding only their canonical target leaves the directory
/// entry itself replaceable in the writable workspace.
fn validate_protected_mountpoints(policy: &SandboxPolicy) -> Result<(), ExecError> {
    for protected in &policy.protected_paths {
        let under_writable_root = policy
            .writable_roots
            .iter()
            .map(|root| canonical(root))
            .any(|root| canonical_path_allow_missing(protected).starts_with(root));
        match std::fs::symlink_metadata(protected) {
            Ok(_) => validate_protected_tree(protected)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && under_writable_root => {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "protected path {} does not exist below a writable root; refusing a sandbox that could create it",
                    protected.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ExecError::Io(error)),
        }
    }
    Ok(())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve the nearest existing ancestor and retain a missing suffix. This is only used for
/// containment checks; the final protected path is still inspected with `symlink_metadata`.
fn canonical_path_allow_missing(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let mut suffix = Vec::new();
    let mut ancestor = path;
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(name) = ancestor.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
}

fn resolve_bwrap() -> Result<PathBuf, String> {
    let executable_name = if cfg!(windows) { "bwrap.exe" } else { "bwrap" };
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    directories.extend(
        ["/usr/bin", "/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from),
    );
    directories.sort();
    directories.dedup();

    let mut rejected = Vec::new();
    for directory in directories {
        let candidate = directory.join(executable_name);
        let Ok(candidate) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if trusted_system_executable(&candidate) {
            return Ok(candidate);
        }
        if rejected.len() < 4 {
            rejected.push(candidate.display().to_string());
        }
    }
    if rejected.is_empty() {
        Err("bwrap was not found in an absolute PATH entry or standard system directory".into())
    } else {
        Err(format!(
            "refusing untrusted bwrap candidate(s): {}",
            rejected.join(", ")
        ))
    }
}

#[cfg(unix)]
fn trusted_system_executable(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let in_standard_bin = matches!(
        path.parent(),
        Some(parent)
            if parent == Path::new("/usr/bin") || parent == Path::new("/usr/local/bin")
    );
    let in_nix_store = path.starts_with("/nix/store/");
    if !in_standard_bin && !in_nix_store {
        return false;
    }
    for (index, component) in path.ancestors().enumerate() {
        let Ok(metadata) = std::fs::metadata(component) else {
            return false;
        };
        let mode = metadata.permissions().mode();
        if metadata.uid() != 0 || mode & 0o022 != 0 {
            return false;
        }
        if index == 0 && (!metadata.is_file() || mode & 0o111 == 0 || mode & 0o6000 != 0) {
            return false;
        }
    }
    true
}

#[cfg(not(unix))]
fn trusted_system_executable(_path: &Path) -> bool {
    // Bubblewrap is only selected on Linux. Other platforms fail closed rather than attempting
    // to infer executable trust from filesystem APIs with different ACL semantics.
    false
}

#[derive(Debug)]
struct ProbeOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_probe(executable: &Path, args: &[&str]) -> Result<ProbeOutput, String> {
    run_probe_with_limits(executable, args, PROBE_TIMEOUT, PROBE_OUTPUT_CAP)
}

fn run_probe_with_limits(
    executable: &Path,
    args: &[&str],
    timeout: Duration,
    output_cap: usize,
) -> Result<ProbeOutput, String> {
    let mut command = std::process::Command::new(executable);
    command
        .args(args)
        // Capability probing never needs user credentials or project configuration.
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", executable.display()))?;
    let child_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "bubblewrap probe stdout pipe was unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "bubblewrap probe stderr pipe was unavailable".to_string())?;
    let stdout_reader = std::thread::spawn(move || read_probe_output(stdout, output_cap));
    let stderr_reader = std::thread::spawn(move || read_probe_output(stderr, output_cap));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                kill_probe_process(child_id);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!(
                    "{} probe timed out after {timeout:?}",
                    executable.display()
                ));
            }
            Err(error) => {
                kill_probe_process(child_id);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!(
                    "could not wait for {} probe: {error}",
                    executable.display()
                ));
            }
        }
    };
    let (stdout, stdout_truncated) = join_probe_reader(stdout_reader)?;
    let (stderr, stderr_truncated) = join_probe_reader(stderr_reader)?;
    if stdout_truncated || stderr_truncated {
        return Err(format!(
            "{} probe output exceeded {output_cap} bytes",
            executable.display()
        ));
    }
    Ok(ProbeOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_probe_output<R: std::io::Read>(
    mut reader: R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(cap.min(4096));
    let mut truncated = false;
    let mut chunk = [0u8; 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok((retained, truncated));
        }
        let keep = cap.saturating_sub(retained.len()).min(read);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
}

fn join_probe_reader(
    reader: std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), String> {
    reader
        .join()
        .map_err(|_| "bubblewrap probe output reader panicked".to_string())?
        .map_err(|error| format!("could not read bubblewrap probe output: {error}"))
}

fn kill_probe_process(id: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{id}")])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = id;
}

fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let version = output
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))?;
    let mut fields = version.split('.');
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next()?.parse().ok()?;
    let patch = fields
        .next()
        .and_then(|field| field.split(|ch: char| !ch.is_ascii_digit()).next())?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

#[cfg(unix)]
fn reject_setuid(executable: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::metadata(executable)
        .map_err(|error| format!("cannot inspect {}: {error}", executable.display()))?;
    if metadata.permissions().mode() & 0o4000 != 0 {
        Err(format!(
            "refusing setuid bubblewrap executable {}",
            executable.display()
        ))
    } else {
        Ok(())
    }
}

fn supports_read_policy(policy: &SandboxPolicy) -> bool {
    policy.readable_roots.len() == 1 && policy.readable_roots[0] == Path::new("/")
}

/// `/tmp` and `/run` are replaced with private tmpfs mounts to hide host Unix sockets. Preserve
/// workspaces nested below either tree through an intermediate bind, but never preserve the root
/// itself because doing so would re-expose every socket in that host runtime directory.
fn is_below_hidden_runtime_root(path: &Path) -> bool {
    [Path::new("/tmp"), Path::new("/run")]
        .iter()
        .any(|root| path.starts_with(root) && path != *root)
}

fn mask_mounts(policy: &SandboxPolicy, command: &CommandSpec) -> Result<Vec<MaskMount>, ExecError> {
    let mut candidates = discover_unreadable_paths(policy, &command.cwd)?;
    if policy.mode != SandboxMode::DangerFullAccess {
        validate_session_storage_aliases(&command.cwd)?;
        candidates.extend(privacy_path_candidates(&command.cwd));
    }
    prepare_mask_mounts(policy, command, candidates)
}

/// Resolve, validate, de-duplicate, and order masks before constructing mount arguments. A
/// directory mask that contains the command's cwd or a writable root would hide that workspace
/// and make later bind operations ambiguous, so such a policy fails before bubblewrap starts.
#[allow(clippy::too_many_lines)] // Validation and mount-order reduction form one security check.
fn prepare_mask_mounts(
    policy: &SandboxPolicy,
    command: &CommandSpec,
    candidates: Vec<PathBuf>,
) -> Result<Vec<MaskMount>, ExecError> {
    let mut exposed_roots: Vec<PathBuf> = policy
        .writable_roots
        .iter()
        .map(|path| canonical_path_allow_missing(path))
        .collect();
    exposed_roots.push(canonical_path_allow_missing(&command.cwd));
    exposed_roots.sort();
    exposed_roots.dedup();
    let root_is_writable = policy.mode != SandboxMode::ReadOnly
        && policy
            .writable_roots
            .iter()
            .map(|path| canonical_path_allow_missing(path))
            .any(|path| path == Path::new("/"));
    let mut restored_runtime_roots: Vec<PathBuf> = exposed_roots
        .iter()
        .filter(|path| path.as_path() != Path::new("/") && is_below_hidden_runtime_root(path))
        .cloned()
        .collect();
    if root_is_writable && is_below_hidden_runtime_root(&command.cwd) {
        restored_runtime_roots.push(canonical_path_allow_missing(&command.cwd));
    }
    restored_runtime_roots.extend(
        policy
            .protected_paths
            .iter()
            .filter(|path| path.exists())
            .map(|path| canonical(path))
            .filter(|path| is_below_hidden_runtime_root(path)),
    );
    restored_runtime_roots.sort();
    restored_runtime_roots.dedup();

    let mut mounts = Vec::new();
    for candidate in candidates {
        let metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ExecError::Io(error)),
        };
        // Resolve a symlink at the configured name so every access through that name reaches the
        // masked physical target. A broken link is an unsafe, unmaskable policy target.
        let path = std::fs::canonicalize(&candidate)?;
        if path == Path::new("/") {
            return Err(ExecError::UnsupportedPolicy(
                "refusing to mask the filesystem root".to_string(),
            ));
        }
        let target_metadata = if metadata.file_type().is_symlink() {
            std::fs::metadata(&path)?
        } else {
            metadata
        };
        if !target_metadata.is_file() && !target_metadata.is_dir() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "unreadable path {} is not a regular file or directory",
                candidate.display()
            )));
        }
        validate_privacy_tree(&path)?;

        if target_metadata.is_dir()
            && exposed_roots
                .iter()
                .any(|root| root == &path || root.starts_with(&path))
        {
            return Err(ExecError::UnsupportedPolicy(format!(
                "workspace path is nested below unreadable directory {}; refusing conflicting mounts",
                candidate.display()
            )));
        }
        // `/tmp` and `/run` are already hidden. Do not try to mount a now-invisible child unless
        // a staged workspace bind will deliberately expose that child again.
        if is_below_hidden_runtime_root(&path)
            && !restored_runtime_roots
                .iter()
                .any(|root| path.starts_with(root))
        {
            continue;
        }
        mounts.push(MaskMount {
            path,
            is_dir: target_metadata.is_dir(),
        });
    }

    mounts.sort_by(|left, right| {
        left.path
            .components()
            .count()
            .cmp(&right.path.components().count())
            .then_with(|| left.path.cmp(&right.path))
    });
    mounts.dedup_by(|left, right| left.path == right.path);

    // If an entire directory is hidden, mounting its descendants afterward is both redundant
    // and invalid because those host paths no longer exist in the mount namespace.
    let mut reduced: Vec<MaskMount> = Vec::new();
    for mount in mounts {
        if reduced
            .iter()
            .any(|parent| parent.is_dir && mount.path.starts_with(&parent.path))
        {
            continue;
        }
        reduced.push(mount);
    }
    if reduced.len() > MAX_SECRET_MASKS {
        return Err(ExecError::UnsupportedPolicy(format!(
            "secret-path policy requires more than {MAX_SECRET_MASKS} masks"
        )));
    }
    Ok(reduced)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::time::Duration;

    use super::*;

    fn spec(cwd: PathBuf) -> CommandSpec {
        CommandSpec {
            program: "/bin/true".to_string(),
            args: Vec::new(),
            cwd,
            timeout: Duration::from_secs(1),
            cancellation: None,
        }
    }

    fn wrap(policy: &SandboxPolicy, command: &CommandSpec) -> CommandSpec {
        let unreadable = discover_unreadable_paths(policy, &command.cwd).expect("scan secrets");
        let unreadable = prepare_mask_mounts(policy, command, unreadable).expect("prepare masks");
        BubblewrapRunner::wrap(policy, command, &unreadable, Path::new("/usr/bin/bwrap"))
    }

    #[test]
    fn read_only_remounts_private_tmp_read_only_and_has_no_workspace_write_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = SandboxPolicy::read_only(dir.path());
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        assert!(wrapped.args.windows(2).any(|w| w == ["--tmpfs", "/tmp"]));
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w == ["--remount-ro", "/tmp"])
        );
        assert!(
            wrapped
                .args
                .windows(3)
                .any(|w| w == ["--setenv", "TMPDIR", "/tmp"])
        );
        assert!(!wrapped.args.iter().any(|a| a == "--bind"));
    }

    #[test]
    fn proxy_mode_is_network_isolated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut policy = SandboxPolicy::workspace_write(dir.path());
        policy.network = NetworkMode::ProxyRouted;
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        assert!(wrapped.args.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn existing_workspace_secrets_are_masked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join(".env");
        std::fs::write(&secret, "TOKEN=secret").expect("secret fixture");
        let policy = SandboxPolicy::workspace_write(dir.path());
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        let secret = canonical(&secret).to_string_lossy().into_owned();
        assert!(
            wrapped.args.windows(3).any(|args| {
                args[0] == "--ro-bind" && args[1] == "/dev/null" && args[2] == secret
            })
        );
    }

    #[test]
    fn uppercase_workspace_secrets_are_masked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join(".ENV");
        std::fs::write(&secret, "TOKEN=secret").expect("secret fixture");
        let policy = SandboxPolicy::workspace_write(dir.path());
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        let secret = canonical(&secret).to_string_lossy().into_owned();
        assert!(
            wrapped.args.windows(3).any(|args| {
                args[0] == "--ro-bind" && args[1] == "/dev/null" && args[2] == secret
            })
        );
    }

    #[test]
    fn danger_policy_masks_an_external_private_store() {
        let workspace = tempfile::tempdir().expect("workspace");
        let private = tempfile::tempdir().expect("private store");
        let mut policy = SandboxPolicy::danger_full_access(workspace.path());
        policy.protected_paths.push(private.path().to_path_buf());
        let literal = globset::escape(&private.path().to_string_lossy());
        policy.unreadable_globs = vec![literal.clone(), format!("{literal}/**")];
        let wrapped = wrap(&policy, &spec(workspace.path().to_path_buf()));
        let private = canonical(private.path()).to_string_lossy().into_owned();
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| args == ["--tmpfs", private.as_str()])
        );
    }

    #[test]
    fn masked_secret_directory_is_read_only_in_read_only_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join("secret-dir");
        std::fs::create_dir(&secret).expect("secret directory");
        let mut policy = SandboxPolicy::read_only(dir.path());
        policy.unreadable_globs = vec!["**/secret-dir".to_string()];
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        let secret = canonical(&secret).to_string_lossy().into_owned();
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| args == ["--tmpfs", secret.as_str()])
        );
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| args == ["--remount-ro", secret.as_str()])
        );
    }

    #[test]
    fn danger_policy_is_wrapped_to_protect_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("git dir");
        let policy = SandboxPolicy::danger_full_access(dir.path());
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        assert_eq!(&wrapped.args[..3], ["--bind", "/", "/"]);
        assert_eq!(
            wrapped
                .args
                .windows(3)
                .filter(|args| *args == ["--bind", "/", "/"])
                .count(),
            1
        );
        assert!(wrapped.args.iter().any(|a| a == "--ro-bind"));
        assert!(
            wrapped
                .args
                .iter()
                .any(|arg| Path::new(arg).file_name() == Some(std::ffi::OsStr::new(".git")))
        );
    }

    #[test]
    fn missing_or_symlinked_protected_mountpoints_fail_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = SandboxPolicy::workspace_write(dir.path());
        assert!(matches!(
            validate_protected_mountpoints(&policy),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("does not exist")
        ));

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().expect("outside");
            std::os::unix::fs::symlink(outside.path(), dir.path().join(".git")).expect("symlink");
            assert!(matches!(
                validate_protected_mountpoints(&policy),
                Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
            ));
        }
    }

    #[test]
    fn workspace_write_root_escalation_keeps_other_confinement() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("git dir");
        let mut policy = SandboxPolicy::workspace_write(dir.path());
        policy.writable_roots = vec![PathBuf::from("/")];
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        assert_eq!(&wrapped.args[..3], ["--bind", "/", "/"]);
        assert!(wrapped.args.iter().any(|arg| arg == "--unshare-net"));
        assert!(wrapped.args.windows(3).any(|args| {
            args[0] == "--ro-bind"
                && Path::new(&args[1]).file_name() == Some(std::ffi::OsStr::new(".git"))
                && args[1] == args[2]
        }));
    }

    #[test]
    fn root_write_escalation_restores_a_tmp_cwd_after_hiding_tmp() {
        let root = PathBuf::from("/tmp/grokforge-approved-workspace");
        let mut policy = SandboxPolicy::workspace_write(&root);
        policy.writable_roots = vec![PathBuf::from("/")];
        policy.protected_paths.clear();
        let wrapped = wrap(&policy, &spec(root));
        let tmpfs = wrapped
            .args
            .windows(2)
            .position(|args| args == ["--tmpfs", "/tmp"])
            .expect("private tmp");
        let restore = wrapped
            .args
            .windows(3)
            .position(|args| {
                args == [
                    "--bind",
                    "/tmp/grokforge-approved-workspace",
                    "/tmp/grokforge-approved-workspace",
                ]
            })
            .expect("cwd restore bind");
        assert!(tmpfs < restore);
    }

    #[cfg(unix)]
    #[test]
    fn writable_tree_preflight_rejects_hardlinks_and_special_entries() {
        use std::os::unix::net::UnixListener;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        let alias = workspace.path().join("alias");
        std::fs::write(&target, "outside").expect("target");
        std::fs::hard_link(&target, &alias).expect("alias");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));

        std::fs::remove_file(alias).expect("remove alias");
        let _listener = UnixListener::bind(workspace.path().join("host.sock")).expect("socket");
        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("non-regular entry")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn writable_tree_preflight_keeps_ordinary_symlinks() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "outside").expect("target");
        std::os::unix::fs::symlink(&target, workspace.path().join("link")).expect("symlink");
        let policy = SandboxPolicy::workspace_write(workspace.path());

        validate_confined_trees(&policy, workspace.path()).expect("ordinary symlink is safe");
    }

    #[cfg(unix)]
    #[test]
    fn writable_tree_preflight_rejects_an_outside_directory_symlink_containing_a_socket() {
        use std::os::unix::net::UnixListener;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let socket = outside.path().join("host.sock");
        let _listener = UnixListener::bind(&socket).expect("socket");
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("control-dir"))
            .expect("directory symlink");
        let policy = SandboxPolicy::workspace_write(workspace.path());

        assert!(matches!(
            validate_confined_trees(&policy, workspace.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("outside the scanned confined roots")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn protected_tree_aliases_fail_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).expect("git dir");
        let target = dir.path().join("config-target");
        std::fs::write(&target, "safe").expect("target");
        std::os::unix::fs::symlink("../config-target", git.join("config"))
            .expect("protected symlink");
        let policy = SandboxPolicy::workspace_write(dir.path());
        assert!(matches!(
            validate_protected_mountpoints(&policy),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
        ));

        std::fs::remove_file(git.join("config")).expect("remove symlink");
        std::fs::hard_link(&target, git.join("config")).expect("hard link");
        assert!(matches!(
            validate_protected_mountpoints(&policy),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn linked_worktree_external_metadata_is_always_validated() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join(".git"), "gitdir: external")
            .expect("linked-worktree marker");
        let external = tempfile::tempdir().expect("external git dir");
        let target = tempfile::NamedTempFile::new().expect("target");
        std::os::unix::fs::symlink(target.path(), external.path().join("config")).expect("symlink");
        let mut policy = SandboxPolicy::workspace_write(workspace.path());
        policy.protected_paths.push(external.path().to_path_buf());
        assert!(matches!(
            validate_protected_mountpoints(&policy),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
        ));

        std::fs::remove_file(external.path().join("config")).expect("remove symlink");
        std::fs::hard_link(target.path(), external.path().join("config")).expect("hard link");
        assert!(matches!(
            validate_protected_mountpoints(&policy),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_runtime_protected_tree_is_restored_read_only() {
        let workspace = tempfile::tempdir_in("/var/tmp").expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("workspace git");
        let external = tempfile::tempdir_in("/tmp").expect("external metadata");
        std::fs::write(external.path().join("config"), "safe").expect("config");
        let mut policy = SandboxPolicy::workspace_write(workspace.path());
        policy.protected_paths.push(external.path().to_path_buf());
        let wrapped = wrap(&policy, &spec(workspace.path().to_path_buf()));

        let external = canonical(external.path());
        let tmpfs = wrapped
            .args
            .windows(2)
            .position(|args| args == ["--tmpfs", "/tmp"])
            .expect("private tmp");
        let restore = wrapped
            .args
            .windows(3)
            .position(|args| {
                args[0] == "--ro-bind"
                    && args[1] == external.to_string_lossy()
                    && args[2] == external.to_string_lossy()
            })
            .expect("protected restore bind");
        assert!(tmpfs < restore);
    }

    #[test]
    fn workspace_nested_under_privacy_mask_fails_before_mounting() {
        let home = tempfile::tempdir().expect("home");
        let ssh = home.path().join(".ssh");
        let workspace = ssh.join("project");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let policy = SandboxPolicy::workspace_write(&workspace);
        let command = spec(workspace);
        assert!(matches!(
            prepare_mask_mounts(&policy, &command, vec![ssh]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("nested below unreadable")
        ));
    }

    #[test]
    fn parent_privacy_mask_removes_redundant_descendant_mounts() {
        // Keep the candidates inside the restored workspace. On Linux, unrelated paths below
        // `/tmp` are already hidden by the private tmpfs and are intentionally omitted.
        let workspace = tempfile::tempdir().expect("workspace");
        let secret_dir = workspace.path().join(".aws");
        let secret_file = secret_dir.join("credentials");
        std::fs::create_dir(&secret_dir).expect("secret dir");
        std::fs::write(&secret_file, "secret").expect("secret file");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec(workspace.path().to_path_buf());
        let mounts = prepare_mask_mounts(&policy, &command, vec![secret_file, secret_dir.clone()])
            .expect("prepare masks");
        assert_eq!(
            mounts,
            vec![MaskMount {
                path: canonical(&secret_dir),
                is_dir: true,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn nested_credential_hardlink_is_rejected_before_parent_mask_reduction() {
        let home = tempfile::tempdir().expect("home");
        let aws = home.path().join(".aws");
        let credential = aws.join("credentials");
        std::fs::create_dir(&aws).expect("aws dir");
        std::fs::write(&credential, "secret").expect("credential");
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::hard_link(&credential, workspace.path().join("aws-alias"))
            .expect("workspace alias");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = spec(workspace.path().to_path_buf());
        assert!(matches!(
            prepare_mask_mounts(&policy, &command, vec![aws, credential]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[test]
    fn isolates_process_namespaces_and_host_socket_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = SandboxPolicy::workspace_write(dir.path());
        let wrapped = wrap(&policy, &spec(dir.path().to_path_buf()));
        for flag in [
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--new-session",
            "--unshare-net",
        ] {
            assert!(wrapped.args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| args == ["--tmpfs", "/run"])
        );
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| args == ["--cap-drop", "ALL"])
        );
    }

    #[test]
    fn danger_masks_exact_external_private_store_globs() {
        let workspace = tempfile::tempdir().expect("workspace");
        let private = tempfile::tempdir().expect("private store");
        std::fs::write(private.path().join("rollout.jsonl"), "private").expect("rollout");
        let private = canonical(private.path());
        let mut policy = SandboxPolicy::danger_full_access(workspace.path());
        policy.protected_paths = vec![private.clone()];
        let literal = globset::escape(&private.to_string_lossy());
        policy.unreadable_globs = vec![literal.clone(), format!("{literal}/**")];
        let command = spec(workspace.path().to_path_buf());
        let paths =
            discover_unreadable_paths(&policy, &command.cwd).expect("find exact private store");
        assert!(paths.contains(&private));
        let masks = prepare_mask_mounts(&policy, &command, paths).expect("prepare mask");
        let wrapped =
            BubblewrapRunner::wrap(&policy, &command, &masks, Path::new("/usr/bin/bwrap"));
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|args| { args[0] == "--tmpfs" && Path::new(&args[1]) == private.as_path() })
        );
    }

    #[test]
    fn tmp_workspace_is_restored_after_private_tmp_mount() {
        let root = PathBuf::from("/tmp/grokforge-workspace");
        let policy = SandboxPolicy::workspace_write(&root);
        let wrapped = wrap(&policy, &spec(root.clone()));
        let tmpfs = wrapped
            .args
            .windows(2)
            .position(|args| args == ["--tmpfs", "/tmp"])
            .expect("private tmpfs");
        let restore = wrapped
            .args
            .windows(3)
            .position(|args| {
                args == [
                    "--bind",
                    "/tmp/grokforge-workspace",
                    "/tmp/grokforge-workspace",
                ]
            })
            .expect("workspace restore bind");
        assert!(tmpfs < restore);
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg.starts_with("/.grokforge-bind-sources"))
        );
    }

    #[test]
    fn run_workspace_is_restored_after_private_run_mount() {
        let root = PathBuf::from("/run/user/1000/grokforge-workspace");
        let policy = SandboxPolicy::workspace_write(&root);
        let wrapped = wrap(&policy, &spec(root));
        let tmpfs = wrapped
            .args
            .windows(2)
            .position(|args| args == ["--tmpfs", "/run"])
            .expect("private run tmpfs");
        let restore = wrapped
            .args
            .windows(3)
            .position(|args| {
                args == [
                    "--bind",
                    "/run/user/1000/grokforge-workspace",
                    "/run/user/1000/grokforge-workspace",
                ]
            })
            .expect("workspace restore bind");
        assert!(tmpfs < restore);
    }

    #[cfg(target_os = "linux")]
    fn live_bwrap_available() -> bool {
        let capability = BubblewrapRunner.capability();
        if capability.enforced {
            return true;
        }
        assert!(
            std::env::var_os("GROKFORGE_REQUIRE_SANDBOX").is_none(),
            "required live bubblewrap backend unavailable: {capability:?}"
        );
        eprintln!("skipping live bubblewrap test: {capability:?}");
        false
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_keeps_a_tmp_workspace_writable_when_available() {
        if !live_bwrap_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(dir.path().join(".git")).expect("git dir");
        let policy = SandboxPolicy::workspace_write(dir.path());
        let command = CommandSpec::shell("printf ok > result.txt", dir.path().to_path_buf());
        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result.txt")).expect("result"),
            "ok"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_keeps_a_tmp_workspace_read_only_when_available() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir_in("/tmp").expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        std::fs::write(workspace.path().join("input.txt"), "visible").expect("input");
        let policy = SandboxPolicy::read_only(workspace.path());
        let command = CommandSpec::shell(
            "cat input.txt; if printf escaped > result.txt 2>/dev/null; then exit 21; else printf READ_ONLY_OK; fi",
            workspace.path().to_path_buf(),
        );
        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.contains("visibleREAD_ONLY_OK"), "{output:?}");
        assert!(!workspace.path().join("result.txt").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_restores_an_external_tmp_protected_tree_read_only() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir_in("/var/tmp").expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        let protected = tempfile::tempdir_in("/tmp").expect("protected");
        let config = protected.path().join("config");
        std::fs::write(&config, "visible").expect("config");
        let mut policy = SandboxPolicy::workspace_write(workspace.path());
        policy.protected_paths.push(protected.path().to_path_buf());
        let command = CommandSpec::shell(
            &format!(
                "cat '{}'; if printf escaped > '{}' 2>/dev/null; then exit 21; else printf PROTECTED_OK; fi",
                config.display(),
                config.display()
            ),
            workspace.path().to_path_buf(),
        );
        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.contains("visiblePROTECTED_OK"), "{output:?}");
        assert_eq!(std::fs::read_to_string(config).expect("config"), "visible");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_drops_effective_inheritable_and_ambient_capabilities() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = CommandSpec::shell(
            "awk 'BEGIN { seen=0 } /^Cap(Inh|Eff|Amb):/ { seen++; if ($2 !~ /^0+$/) exit 1 } END { if (seen != 3) exit 1 }' /proc/self/status",
            workspace.path().to_path_buf(),
        );

        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "capabilities were retained: {output:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_masks_uppercase_env_when_available() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        std::fs::write(workspace.path().join(".ENV"), "UPPERCASE SECRET").expect("secret");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = CommandSpec::shell(
            "cat .ENV; printf SANDBOX_OK",
            workspace.path().to_path_buf(),
        );

        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.contains("SANDBOX_OK"), "{output:?}");
        assert!(!output.stdout.contains("UPPERCASE SECRET"), "{output:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_danger_masks_an_external_private_store() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let private = tempfile::tempdir().expect("private store");
        let rollout = private.path().join("rollout.jsonl");
        std::fs::write(&rollout, "PRIVATE ROLLOUT").expect("rollout");
        let private = canonical(private.path());
        let mut policy = SandboxPolicy::danger_full_access(workspace.path());
        policy.protected_paths = vec![private.clone()];
        let literal = globset::escape(&private.to_string_lossy());
        policy.unreadable_globs = vec![literal.clone(), format!("{literal}/**")];
        let command = CommandSpec::shell(
            &format!("cat '{}' 2>/dev/null; printf SANDBOX_OK", rollout.display()),
            workspace.path().to_path_buf(),
        );

        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.contains("SANDBOX_OK"), "{output:?}");
        assert!(!output.stdout.contains("PRIVATE ROLLOUT"), "{output:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_rejects_a_workspace_hardlink_to_an_outside_file() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "original").expect("outside target");
        std::fs::hard_link(&target, workspace.path().join("alias")).expect("workspace alias");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = CommandSpec::shell("printf escaped > alias", workspace.path().to_path_buf());

        assert!(matches!(
            BubblewrapRunner.run(&policy, &command).await,
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
        assert_eq!(
            std::fs::read_to_string(target).expect("outside target"),
            "original"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_keeps_a_workspace_symlink_target_read_only() {
        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, "original").expect("outside target");
        std::os::unix::fs::symlink(&target, workspace.path().join("link")).expect("workspace link");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = CommandSpec::shell(
            "if printf escaped > link 2>/dev/null; then exit 21; else printf BLOCKED_OK; fi",
            workspace.path().to_path_buf(),
        );

        let output = BubblewrapRunner
            .run(&policy, &command)
            .await
            .expect("run bwrap");
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.contains("BLOCKED_OK"), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(target).expect("outside target"),
            "original"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_bwrap_rejects_an_outside_directory_symlink_containing_a_unix_socket() {
        use std::os::unix::net::UnixListener;

        if !live_bwrap_available() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join(".git")).expect("git dir");
        let outside = tempfile::tempdir().expect("outside");
        let socket = outside.path().join("host.sock");
        let _listener = UnixListener::bind(&socket).expect("socket");
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("control-dir"))
            .expect("directory symlink");
        let policy = SandboxPolicy::workspace_write(workspace.path());
        let command = CommandSpec::shell("true", workspace.path().to_path_buf());

        assert!(matches!(
            BubblewrapRunner.run(&policy, &command).await,
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("outside the scanned confined roots")
        ));
    }

    #[test]
    fn parses_and_enforces_minimum_bwrap_version() {
        assert_eq!(parse_version("bubblewrap 0.11.2\n"), Some((0, 11, 2)));
        assert_eq!(parse_version("bwrap 1.2.3-rc1"), Some((1, 2, 3)));
        assert!((0, 11, 1) < MIN_BWRAP_VERSION);
        assert!((0, 11, 2) >= MIN_BWRAP_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_setuid_bwrap_binary() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let executable = dir.path().join("bwrap");
        std::fs::write(&executable, "fixture").expect("fixture");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o4755))
            .expect("permissions");
        assert!(reject_setuid(&executable).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn repo_local_bwrap_candidate_is_rejected_without_execution() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("executed");
        let executable = dir.path().join("bwrap");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .expect("fixture");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("permissions");

        assert!(!trusted_system_executable(&executable));
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn probe_output_and_runtime_are_bounded() {
        let noisy = run_probe_with_limits(
            Path::new("/bin/sh"),
            &["-c", "yes x | head -c 10000"],
            Duration::from_secs(1),
            128,
        )
        .expect_err("oversized probe output must fail");
        assert!(noisy.contains("output exceeded"));

        let started = Instant::now();
        let stalled = run_probe_with_limits(
            Path::new("/bin/sh"),
            &["-c", "sleep 10"],
            Duration::from_millis(100),
            128,
        )
        .expect_err("stalled probe must time out");
        assert!(stalled.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
