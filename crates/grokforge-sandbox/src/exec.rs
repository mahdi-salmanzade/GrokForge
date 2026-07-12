//! Process execution primitives shared by every backend: the command spec, the captured
//! output (byte-capped, with head+tail truncation), timeout/kill, and child-env scrubbing so
//! secrets like `XAI_API_KEY` never reach subprocesses.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use grokforge_protocol::DenialClass;
use tokio::io::AsyncReadExt;

/// Environment variables never passed to a child process.
const SCRUBBED_ENV: &[&str] = &["XAI_API_KEY", "XAI_BASE_URL"];

/// Maximum captured bytes per stream before head+tail truncation kicks in.
pub const OUTPUT_CAP: usize = 64 * 1024;

/// A command to run.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
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
        }
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
    #[error("io error while running command: {0}")]
    Io(#[from] std::io::Error),
}

/// Run a command with the given extra environment removals, capturing capped output and
/// enforcing the timeout by killing the process group on expiry.
pub async fn run_capture(spec: &CommandSpec) -> Result<ExecOutput, ExecError> {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in SCRUBBED_ENV {
        cmd.env_remove(key);
    }

    let mut child = cmd.spawn().map_err(|source| ExecError::Spawn {
        program: spec.program.clone(),
        source,
    })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let capture = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            p.read_to_end(&mut out).await?;
        }
        if let Some(p) = stderr_pipe.as_mut() {
            p.read_to_end(&mut err).await?;
        }
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };

    match tokio::time::timeout(spec.timeout, capture).await {
        Ok(Ok((status, out, err))) => {
            let (stdout, t1) = cap_bytes(&out);
            let (stderr, t2) = cap_bytes(&err);
            Ok(ExecOutput {
                exit_code: status.code(),
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: false,
                denial: None,
            })
        }
        Ok(Err(e)) => Err(ExecError::Io(e)),
        Err(_elapsed) => {
            // Timed out: kill the child and report it.
            let _ = child.start_kill();
            Ok(ExecOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("command timed out after {:?}", spec.timeout),
                truncated: false,
                timed_out: true,
                denial: None,
            })
        }
    }
}

/// Cap a byte buffer to [`OUTPUT_CAP`], keeping the head and tail with an elision marker.
fn cap_bytes(bytes: &[u8]) -> (String, bool) {
    if bytes.len() <= OUTPUT_CAP {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let head = OUTPUT_CAP / 2;
    let tail = OUTPUT_CAP - head;
    let mut s = String::from_utf8_lossy(&bytes[..head]).into_owned();
    s.push_str("\n… [output truncated] …\n");
    s.push_str(&String::from_utf8_lossy(&bytes[bytes.len() - tail..]));
    (s, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bytes_passes_short_output_through() {
        let (s, trunc) = cap_bytes(b"hello");
        assert_eq!(s, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_bytes_truncates_long_output() {
        let big = vec![b'a'; OUTPUT_CAP * 2];
        let (s, trunc) = cap_bytes(&big);
        assert!(trunc);
        assert!(s.contains("output truncated"));
        assert!(s.len() < big.len());
    }
}
