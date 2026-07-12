//! Process execution primitives shared by every backend: the command spec, the captured
//! output (byte-capped, with head+tail truncation), timeout/kill, and a fail-closed child
//! environment so ambient credentials never reach subprocesses.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use grokforge_protocol::DenialClass;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

/// Locale variables with defined libc/POSIX meanings. Accepting arbitrary `LC_*` names would
/// turn the allowlist back into an attacker-controlled credential channel.
const SAFE_LOCALE_ENV: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_ADDRESS",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NAME",
    "LC_NUMERIC",
    "LC_PAPER",
    "LC_TELEPHONE",
    "LC_TIME",
];

const SAFE_TERMINAL_ENV: &[&str] = &["TERM", "COLORTERM"];
const MAX_SAFE_ENV_VALUE_BYTES: usize = 128;
const MAX_SAFE_PATH_BYTES: usize = 16 * 1024;

/// Maximum captured bytes per stream before head+tail truncation kicks in.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// A command to run.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Cooperative cancellation for an active command. Sandbox backends preserve this token
    /// when wrapping the command so cancellation reaches the process-group owner.
    pub cancellation: Option<CancellationToken>,
}

impl CommandSpec {
    /// Run a command line through the system shell (`/bin/sh -c` on Unix, `cmd /C` on Windows),
    /// so quotes, pipes, redirects, and globs work as written. The shell itself runs under the
    /// sandbox, so its child processes are confined too.
    #[must_use]
    pub fn shell(command_line: &str, cwd: PathBuf) -> Self {
        #[cfg(windows)]
        let (program, args) = (
            "cmd".to_string(),
            vec!["/C".to_string(), command_line.to_string()],
        );
        #[cfg(not(windows))]
        let (program, args) = (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), command_line.to_string()],
        );
        Self {
            program,
            args,
            cwd,
            timeout: Duration::from_secs(120),
            cancellation: None,
        }
    }

    /// Attach cooperative cancellation to this command.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

/// The result of running a command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    /// Set when the denial classifier attributes the failure to the sandbox.
    pub denial: Option<DenialClass>,
}

impl ExecOutput {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// Errors from spawning a process (distinct from the process failing).
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("sandbox policy cannot be enforced: {0}")]
    UnsupportedPolicy(String),
    #[error("command interrupted by user")]
    Cancelled,
    #[error("io error while running command: {0}")]
    Io(#[from] std::io::Error),
}

enum CommandCompletion {
    Child(Result<Result<std::process::ExitStatus, std::io::Error>, tokio::time::error::Elapsed>),
    Cancelled,
}

/// Run a command with the given extra environment removals, capturing capped output and
/// enforcing the timeout by killing the process group on expiry.
#[allow(clippy::too_many_lines)] // Keep every kill/reap branch together for auditability.
pub async fn run_capture(spec: &CommandSpec) -> Result<ExecOutput, ExecError> {
    if spec
        .cancellation
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(ExecError::Cancelled);
    }
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    configure_child_environment(&mut cmd);

    let mut child = cmd.spawn().map_err(|source| ExecError::Spawn {
        program: spec.program.clone(),
        source,
    })?;

    let child_id = child.id();
    let mut process_group = ProcessGroupGuard::new(child_id);
    let stdout = Arc::new(Mutex::new(CappedBytes::new(OUTPUT_CAP)));
    let stderr = Arc::new(Mutex::new(CappedBytes::new(OUTPUT_CAP)));
    let mut stdout_reader = tokio::spawn(drain_pipe(child.stdout.take(), Arc::clone(&stdout)));
    let mut stderr_reader = tokio::spawn(drain_pipe(child.stderr.take(), Arc::clone(&stderr)));

    let completion = {
        // Scope the wait future: it owns the temporary `&mut child` borrow and must be dropped
        // before the teardown branches borrow the child again to kill and reap it.
        let cancellation = spec.cancellation.clone();
        let wait_for_cancellation = async move {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let wait_for_child = tokio::time::timeout(spec.timeout, child.wait());
        tokio::pin!(wait_for_child);
        tokio::pin!(wait_for_cancellation);
        tokio::select! {
            result = &mut wait_for_child => CommandCompletion::Child(result),
            () = &mut wait_for_cancellation => CommandCompletion::Cancelled,
        }
    };

    match completion {
        CommandCompletion::Child(Ok(Ok(status))) => {
            // A shell can report success while a background child remains in its process group.
            // Kill that remainder before disarming the cancellation guard, then bound pipe
            // draining so a child that deliberately escaped the group cannot hold us open.
            process_group.kill_remaining();
            process_group.disarm();
            finish_capture_readers(&mut stdout_reader, &mut stderr_reader).await?;
            let (stdout, t1) = snapshot(&stdout);
            let (stderr, t2) = snapshot(&stderr);
            Ok(ExecOutput {
                exit_code: status.code(),
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: false,
                denial: None,
            })
        }
        CommandCompletion::Child(Ok(Err(error))) => {
            process_group.kill_remaining();
            let _ = child.start_kill();
            let _ = child.wait().await;
            process_group.disarm();
            abort_capture_readers(&mut stdout_reader, &mut stderr_reader).await;
            Err(ExecError::Io(error))
        }
        CommandCompletion::Child(Err(_elapsed)) => {
            // Timed out: terminate the whole process group before the leader, then reap the
            // leader. Killing only `/bin/sh` would leave its grandchildren running with our
            // stdout/stderr pipes held open.
            process_group.kill_remaining();
            let _ = child.start_kill();
            let _ = child.wait().await;
            process_group.disarm();

            // Drain bytes already buffered in the pipes, but never let a surviving/misbehaving
            // descendant hold timeout reporting hostage.
            finish_capture_readers(&mut stdout_reader, &mut stderr_reader).await?;

            let (stdout, t1) = snapshot(&stdout);
            let (captured_stderr, t2) = snapshot(&stderr);
            let timeout_message = format!("command timed out after {:?}", spec.timeout);
            let stderr = if captured_stderr.is_empty() {
                timeout_message
            } else {
                format!("{captured_stderr}\n{timeout_message}")
            };
            Ok(ExecOutput {
                exit_code: None,
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: true,
                denial: None,
            })
        }
        CommandCompletion::Cancelled => {
            // Complete process-group teardown and reap the leader before returning. The caller
            // can then safely release the durable session lock without a background command
            // continuing to mutate the workspace.
            process_group.kill_remaining();
            let _ = child.start_kill();
            let _ = child.wait().await;
            process_group.disarm();
            finish_capture_readers(&mut stdout_reader, &mut stderr_reader).await?;
            Err(ExecError::Cancelled)
        }
    }
}

async fn drain_pipe<R>(pipe: Option<R>, capture: Arc<Mutex<CappedBytes>>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buf = vec![0u8; 8192];
    loop {
        let read = pipe.read(&mut buf).await?;
        if read == 0 {
            return Ok(());
        }
        lock_capture(&capture).push(&buf[..read]);
    }
}

async fn finish_capture_readers(
    stdout: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    stderr: &mut tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<(), ExecError> {
    let joined = async {
        let (stdout_result, stderr_result) = tokio::join!(&mut *stdout, &mut *stderr);
        stdout_result
            .map_err(|error| std::io::Error::other(format!("stdout reader failed: {error}")))??;
        stderr_result
            .map_err(|error| std::io::Error::other(format!("stderr reader failed: {error}")))??;
        Ok::<(), std::io::Error>(())
    };
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), joined).await {
        result.map_err(ExecError::Io)
    } else {
        abort_capture_readers(stdout, stderr).await;
        Ok(())
    }
}

async fn abort_capture_readers(
    stdout: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    stderr: &mut tokio::task::JoinHandle<std::io::Result<()>>,
) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

fn snapshot(capture: &Arc<Mutex<CappedBytes>>) -> (String, bool) {
    lock_capture(capture).render()
}

fn lock_capture(capture: &Arc<Mutex<CappedBytes>>) -> MutexGuard<'_, CappedBytes> {
    capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
fn kill_process_group(id: u32) {
    // `Command::process_group(0)` makes the child PID its process-group ID. A negative operand
    // addresses the entire group. This avoids unsafe signal calls while retaining MSRV support.
    let _ = std::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{id}")])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Debug)]
struct ProcessGroupGuard {
    #[cfg(unix)]
    id: Option<u32>,
    #[cfg(unix)]
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(id: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self { id, armed: true }
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Self {}
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.armed = false;
        }
        #[cfg(not(unix))]
        let _ = self;
    }

    fn kill_remaining(&self) {
        #[cfg(unix)]
        if let Some(id) = self.id {
            kill_process_group(id);
        }
        #[cfg(not(unix))]
        let _ = self;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed
            && let Some(id) = self.id
        {
            kill_process_group(id);
        }
    }
}

/// Start every sandbox subprocess from an empty environment. The wrapper process and, by
/// inheritance, the command inside bubblewrap/Seatbelt receive only this narrow usability set.
/// Backend-specific private values such as Seatbelt's `TMPDIR` are injected by the backend after
/// this boundary, never copied from the host.
fn configure_child_environment(command: &mut tokio::process::Command) {
    command.env_clear();
    for (key, value) in safe_child_environment(std::env::vars_os()) {
        command.env(key, value);
    }
}

fn safe_child_environment(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut path = None;
    let mut locale = Vec::new();
    let mut terminal = Vec::new();
    let mut no_color = false;

    for (key, value) in environment {
        if env_key_eq(&key, "PATH") {
            path = safe_path(&value);
        } else if SAFE_LOCALE_ENV
            .iter()
            .any(|candidate| env_key_eq(&key, candidate))
        {
            if is_safe_env_value(&value) {
                locale.push((key, value));
            }
        } else if SAFE_TERMINAL_ENV
            .iter()
            .any(|candidate| env_key_eq(&key, candidate))
        {
            if is_safe_env_value(&value) {
                terminal.push((key, value));
            }
        } else if env_key_eq(&key, "NO_COLOR") {
            // The convention is presence-based. Do not copy an arbitrary ambient value.
            no_color = true;
        }
    }

    let mut safe = Vec::new();
    if let Some(path) = path.or_else(default_safe_path) {
        safe.push((OsString::from("PATH"), path));
    }
    safe.extend(locale);
    safe.extend(terminal);
    if no_color {
        safe.push((OsString::from("NO_COLOR"), OsString::from("1")));
    }
    safe
}

fn env_key_eq(key: &OsStr, expected: &str) -> bool {
    #[cfg(windows)]
    {
        key.to_string_lossy().eq_ignore_ascii_case(expected)
    }
    #[cfg(not(windows))]
    {
        key == OsStr::new(expected)
    }
}

fn safe_path(value: &OsStr) -> Option<OsString> {
    if value.as_encoded_bytes().len() > MAX_SAFE_PATH_BYTES {
        return None;
    }
    let mut directories = Vec::new();
    for directory in std::env::split_paths(value) {
        if !directory.is_absolute()
            || !directory.is_dir()
            || contains_control_bytes(directory.as_os_str())
            || directories.contains(&directory)
        {
            continue;
        }
        directories.push(directory);
    }
    if directories.is_empty() {
        return None;
    }
    std::env::join_paths(directories).ok()
}

fn default_safe_path() -> Option<OsString> {
    let directories = ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_dir());
    std::env::join_paths(directories).ok()
}

fn is_safe_env_value(value: &OsStr) -> bool {
    let bytes = value.as_encoded_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SAFE_ENV_VALUE_BYTES
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.@+-".contains(byte))
}

fn contains_control_bytes(value: &OsStr) -> bool {
    value.as_encoded_bytes().iter().any(u8::is_ascii_control)
}

#[derive(Debug)]
struct CappedBytes {
    cap: usize,
    seen: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
}

impl CappedBytes {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: 0,
            head: Vec::with_capacity(cap / 2),
            tail: VecDeque::with_capacity(cap - cap / 2),
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len());
        let head_cap = self.cap / 2;
        let head_remaining = head_cap.saturating_sub(self.head.len());
        let into_head = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..into_head]);
        bytes = &bytes[into_head..];

        let tail_cap = self.cap - head_cap;
        if bytes.len() >= tail_cap {
            self.tail.clear();
            self.tail.extend(
                bytes[bytes.len().saturating_sub(tail_cap)..]
                    .iter()
                    .copied(),
            );
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(tail_cap);
        self.tail.drain(..overflow);
        self.tail.extend(bytes.iter().copied());
    }

    fn render(&self) -> (String, bool) {
        let truncated = self.seen > self.cap;
        let mut bytes = Vec::with_capacity(self.head.len() + self.tail.len());
        bytes.extend_from_slice(&self.head);
        if truncated {
            bytes.extend_from_slice(b"\n\xe2\x80\xa6 [output truncated] \xe2\x80\xa6\n");
        }
        bytes.extend(self.tail.iter().copied());
        (String::from_utf8_lossy(&bytes).into_owned(), truncated)
    }

    #[cfg(test)]
    fn retained_len(&self) -> usize {
        self.head.len() + self.tail.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bytes_passes_short_output_through() {
        let mut capture = CappedBytes::new(OUTPUT_CAP);
        capture.push(b"hello");
        let (s, trunc) = capture.render();
        assert_eq!(s, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_bytes_truncates_long_output() {
        let big = vec![b'a'; OUTPUT_CAP * 2];
        let mut capture = CappedBytes::new(OUTPUT_CAP);
        capture.push(&big);
        let (s, trunc) = capture.render();
        assert!(trunc);
        assert!(s.contains("output truncated"));
        assert!(s.len() < big.len());
        assert_eq!(capture.retained_len(), OUTPUT_CAP);
    }

    #[test]
    fn explicit_environment_allowlist_rejects_unknown_and_malformed_values() {
        let safe = safe_child_environment([
            (
                OsString::from("PATH"),
                std::env::join_paths(["/usr/bin", "/bin", "relative"]).expect("test path"),
            ),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (
                OsString::from("LC_TIME"),
                OsString::from("../../credential"),
            ),
            (OsString::from("LC_CREDENTIAL"), OsString::from("jwt")),
            (OsString::from("TERM"), OsString::from("xterm-256color")),
            (
                OsString::from("NO_COLOR"),
                OsString::from("do-not-copy-this"),
            ),
            (
                OsString::from("HTTPS_PROXY"),
                OsString::from("https://user:pass@example.invalid"),
            ),
            (OsString::from("HOME"), OsString::from("/private/home")),
        ]);

        let lookup = |name: &str| {
            safe.iter()
                .find(|(key, _)| key == OsStr::new(name))
                .map(|(_, value)| value)
        };
        let path = lookup("PATH").expect("safe PATH");
        let path_entries: Vec<_> = std::env::split_paths(path).collect();
        assert!(path_entries.contains(&PathBuf::from("/usr/bin")));
        assert!(path_entries.contains(&PathBuf::from("/bin")));
        assert!(!path_entries.contains(&PathBuf::from("relative")));
        assert_eq!(lookup("LANG"), Some(&OsString::from("C.UTF-8")));
        assert_eq!(lookup("TERM"), Some(&OsString::from("xterm-256color")));
        assert_eq!(lookup("NO_COLOR"), Some(&OsString::from("1")));
        assert!(lookup("LC_TIME").is_none());
        assert!(lookup("LC_CREDENTIAL").is_none());
        assert!(lookup("HTTPS_PROXY").is_none());
        assert!(lookup("HOME").is_none());
    }

    /// Re-exec this one test with hostile ambient variables. This proves `run_capture` clears
    /// the real OS environment without mutating the multi-threaded test process via `set_var`.
    #[cfg(unix)]
    #[test]
    fn sandbox_child_environment_is_an_explicit_allowlist() {
        const CHILD_MARKER: &str = "GROKFORGE_ENV_ALLOWLIST_TEST_CHILD";
        const TEST_NAME: &str = "exec::tests::sandbox_child_environment_is_an_explicit_allowlist";
        const SECRET_NAMES: &[&str] = &[
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "KRB5CCNAME",
            "KRB5_CLIENT_KTNAME",
            "PGPASSFILE",
            "MYSQL_PWD",
            "CI_JOB_JWT",
            "CI_CREDENTIAL_HANDLE",
            "HOME",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "LC_CREDENTIAL",
        ];

        if std::env::var_os(CHILD_MARKER).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let output = runtime
                .block_on(run_capture(&CommandSpec {
                    program: "/usr/bin/env".to_string(),
                    args: Vec::new(),
                    cwd: PathBuf::from("/tmp"),
                    timeout: Duration::from_secs(5),
                    cancellation: None,
                }))
                .expect("capture child environment");
            assert!(output.succeeded(), "{output:?}");
            let variables: std::collections::HashMap<_, _> = output
                .stdout
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            for name in SECRET_NAMES {
                assert!(!variables.contains_key(name), "{name} leaked to child");
            }
            assert_eq!(variables.get("LANG"), Some(&"C.UTF-8"));
            assert_eq!(variables.get("LC_ALL"), Some(&"C"));
            assert_eq!(variables.get("TERM"), Some(&"xterm-256color"));
            assert_eq!(variables.get("COLORTERM"), Some(&"truecolor"));
            assert_eq!(variables.get("NO_COLOR"), Some(&"1"));
            let child_path = variables.get("PATH").expect("validated PATH");
            assert_eq!(*child_path, "/usr/bin:/bin");
            eprintln!("GROKFORGE_ENV_ALLOWLIST_CHILD_OK");
            return;
        }

        let current_executable = std::env::current_exe().expect("current test executable");
        let mut child = std::process::Command::new(current_executable);
        child
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_MARKER, "1")
            .env("PATH", "/usr/bin:/bin:relative:/does/not/exist")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C")
            .env("LC_CREDENTIAL", "ambient-ci-jwt")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("NO_COLOR", "ambient-secret-value")
            .env("HTTPS_PROXY", "https://user:pass@proxy.example.invalid")
            .env("HTTP_PROXY", "http://user:pass@proxy.example.invalid")
            .env("ALL_PROXY", "socks5://user:pass@proxy.example.invalid")
            .env("GIT_ASKPASS", "/tmp/credential-helper")
            .env("SSH_ASKPASS", "/tmp/credential-helper")
            .env("KRB5CCNAME", "FILE:/tmp/krb5cc-secret")
            .env("KRB5_CLIENT_KTNAME", "FILE:/tmp/client-secret.keytab")
            .env("PGPASSFILE", "/tmp/pgpass-secret")
            .env("MYSQL_PWD", "database-password")
            .env("CI_JOB_JWT", "header.payload.signature")
            .env("CI_CREDENTIAL_HANDLE", "opaque-credential-handle")
            .env("HOME", "/tmp/host-home")
            .env("TMPDIR", "/tmp/host-private-temp")
            .env("XDG_CONFIG_HOME", "/tmp/host-config")
            .stdin(Stdio::null());
        let output = child.output().expect("run isolated test child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed: {stderr}");
        assert!(
            stderr.contains("GROKFORGE_ENV_ALLOWLIST_CHILD_OK"),
            "child test did not execute: {stderr}"
        );
    }

    #[tokio::test]
    async fn drains_stdout_and_stderr_concurrently_without_deadlock() {
        let mut spec = CommandSpec::shell(
            "awk 'BEGIN { for(i=0;i<200000;i++) printf \"x\" }'; awk 'BEGIN { for(i=0;i<200000;i++) printf \"y\" }' >&2",
            std::env::temp_dir(),
        );
        spec.timeout = Duration::from_secs(5);
        let out = run_capture(&spec).await.expect("run");
        assert!(out.succeeded(), "{out:?}");
        assert!(out.stdout.contains('x'));
        assert!(out.stderr.contains('y'));
        assert!(out.truncated);
        assert!(out.stdout.len() < OUTPUT_CAP + 64);
        assert!(out.stderr.len() < OUTPUT_CAP + 64);
    }

    #[tokio::test]
    async fn timeout_preserves_partial_output() {
        let mut spec = CommandSpec::shell(
            "printf before; printf warning >&2; sleep 10",
            std::env::temp_dir(),
        );
        spec.timeout = Duration::from_millis(100);
        let out = run_capture(&spec).await.expect("run");
        assert!(out.timed_out);
        assert!(out.stdout.contains("before"));
        assert!(out.stderr.contains("warning"));
        assert!(out.stderr.contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_descendant_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived");
        let command = format!("(sleep 1; touch '{}') & wait", marker.display());
        let mut spec = CommandSpec::shell(&command, dir.path().to_path_buf());
        spec.timeout = Duration::from_millis(100);
        let out = run_capture(&spec).await.expect("run");
        assert!(out.timed_out);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists(), "timed-out grandchild survived");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_leader_exit_kills_background_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived-success");
        let command = format!(
            "(sleep 1; touch '{}') >/dev/null 2>&1 & exit 0",
            marker.display()
        );
        let mut spec = CommandSpec::shell(&command, dir.path().to_path_buf());
        spec.timeout = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let out = run_capture(&spec).await.expect("run");
        assert!(out.succeeded(), "{out:?}");
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "background child delayed successful completion"
        );
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "successful command left a grandchild alive"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_capture_kills_the_descendant_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived-cancel");
        let command = format!("(sleep 1; touch '{}') & wait", marker.display());
        let spec = CommandSpec::shell(&command, dir.path().to_path_buf());
        let task = tokio::spawn(async move { run_capture(&spec).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists(), "cancelled command grandchild survived");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cooperative_cancellation_kills_and_reaps_before_returning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived-cooperative-cancel");
        let command = format!("(sleep 1; touch '{}') & wait", marker.display());
        let cancellation = CancellationToken::new();
        let spec = CommandSpec::shell(&command, dir.path().to_path_buf())
            .with_cancellation(cancellation.clone());
        let task = tokio::spawn(async move { run_capture(&spec).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
        let error = task
            .await
            .expect("capture task")
            .expect_err("command should report cancellation");
        assert!(matches!(error, ExecError::Cancelled));
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!marker.exists(), "cancelled command grandchild survived");
    }
}
