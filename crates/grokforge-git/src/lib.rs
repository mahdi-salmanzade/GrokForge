//! `grokforge-git` — git operations run from the **trusted host process** (never inside the
//! sandbox; `.git` is deny-write there). v0.1 shells out to the `git` CLI for both reads and
//! mutations for robustness; a `gix`-backed read path is a later optimization.
//!
//! The git-native workflow (docs/design/04-ux-spec.md §5): every agent-touched change becomes a
//! real commit carrying `Grokforge-Session`/`Grokforge-Turn` trailers (the marker `/undo` keys
//! off), with hooks neutralized so a repo hook can't be used as an injection vector.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use grokforge_protocol::{SessionId, TurnId};

/// The trailer key identifying which session produced a commit.
pub const SESSION_TRAILER: &str = "Grokforge-Session";
/// The trailer key identifying which turn produced a commit.
pub const TURN_TRAILER: &str = "Grokforge-Turn";

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_CAP: usize = 16 * 1024 * 1024;
const SAFE_GIT_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "XDG_CONFIG_HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "SystemRoot",
    "WINDIR",
];

/// Errors from git operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("path is outside repository: {0}")]
    OutsideRepo(PathBuf),
    #[error("git path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("git command timed out after {0:?}")]
    Timeout(Duration),
    #[error("git command output exceeded {0} bytes")]
    OutputLimit(usize),
    #[error("no trusted git executable is available: {0}")]
    UntrustedExecutable(String),
    #[error("io error running git: {0}")]
    Io(#[from] std::io::Error),
}

/// A handle to a git repository rooted at its top level.
#[derive(Debug, Clone)]
pub struct Git {
    root: PathBuf,
}

/// One agent commit found in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub session: Option<String>,
    pub turn: Option<String>,
}

impl Git {
    /// Discover the repository containing `start`, if any.
    #[must_use]
    pub fn discover(start: &Path) -> Option<Self> {
        let canonical_start = std::fs::canonicalize(start).ok()?;
        let marker_root = repository_marker_root(&canonical_start)?;
        let mut command = hardened_git_command().ok()?;
        command
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(start);
        let out = execute_git_command(&mut command).ok()?;
        if !out.status.success() {
            return None;
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return None;
        }
        let root = std::fs::canonicalize(root).ok()?;
        // `core.worktree` can make rev-parse report an arbitrary path. Anchor discovery to the
        // nearest actual `.git` file/directory above the supplied start path before trusting it
        // as the worktree for host-side mutations.
        if root != marker_root || !canonical_start.starts_with(&root) {
            return None;
        }
        Some(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical repository metadata locations that sandbox policies must protect. In a linked
    /// worktree these point through the workspace's `.git` file to the real per-worktree git-dir
    /// and shared common-dir.
    pub fn metadata_paths(&self) -> Result<Vec<PathBuf>, GitError> {
        let mut paths = Vec::new();
        for query in ["--git-dir", "--git-common-dir"] {
            let raw = self.run(&["rev-parse", query])?;
            let path = PathBuf::from(raw.trim());
            let path = if path.is_absolute() {
                path
            } else {
                self.root.join(path)
            };
            paths.push(std::fs::canonicalize(&path).map_err(GitError::Io)?);
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Resolve the trusted Git executable GrokForge will use for host-process operations.
    /// Candidates are never executed during validation.
    pub fn trusted_executable() -> Result<PathBuf, GitError> {
        trusted_git_executable()
    }

    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        self.run_inner(args)
    }

    fn run_mutation(&self, args: &[&str]) -> Result<String, GitError> {
        self.run_inner(args)
    }

    fn run_inner(&self, args: &[&str]) -> Result<String, GitError> {
        let mut command = self.repo_command()?;
        command.args(args).current_dir(&self.root);
        let out = execute_git_command(&mut command)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("git exited with {}", out.status)
            } else {
                stderr
            };
            Err(GitError::Command(detail))
        }
    }

    /// Construct a Git command with every repository-configured executable hook neutralized.
    /// Reads such as `status` can invoke clean/process filters while hashing worktree content,
    /// so this applies to all repository commands, not only mutations.
    #[allow(clippy::case_sensitive_file_extension_comparisons)] // Git config keys, not paths.
    fn repo_command(&self) -> Result<Command, GitError> {
        let mut command = hardened_git_command()?;
        for key in self.configured_command_keys()? {
            let lowercase = key.to_ascii_lowercase();
            // An empty custom merge driver exits successfully without actually merging the
            // file. Use a command that fails instead: repository-defined code still cannot run,
            // and Git cannot silently accept an unmerged result during revert/merge plumbing.
            if lowercase.starts_with("merge.") && lowercase.ends_with(".driver") {
                command.arg("-c").arg(format!("{key}=false"));
                continue;
            }
            if lowercase.ends_with(".required") {
                command.arg("-c").arg(format!("{key}=false"));
                continue;
            }
            command.arg("-c").arg(format!("{key}="));
            if lowercase.ends_with(".clean") {
                let driver = &key[..key.len() - ".clean".len()];
                command.arg("-c").arg(format!("{driver}.required=false"));
            }
            if lowercase.ends_with(".process") {
                let driver = &key[..key.len() - ".process".len()];
                command.arg("-c").arg(format!("{driver}.required=false"));
            }
        }
        Ok(command)
    }

    /// Repository-defined clean/process filters are shell commands and must never execute in
    /// the trusted host process. Return their config keys so mutations can override them with
    /// empty, non-required filters for that invocation.
    #[allow(clippy::case_sensitive_file_extension_comparisons)] // Git config keys, not paths.
    fn configured_command_keys(&self) -> Result<Vec<String>, GitError> {
        let mut command = hardened_git_command()?;
        command
            .args([
                "config",
                "--name-only",
                "--get-regexp",
                "^(filter|merge|diff)\\.",
            ])
            .current_dir(&self.root);
        let out = execute_git_command(&mut command)?;
        // `git config --get-regexp` exits 1 when there are no matches.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(GitError::Command(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|key| {
                let lowercase = key.to_ascii_lowercase();
                (lowercase.starts_with("filter.")
                    && (lowercase.ends_with(".clean")
                        || lowercase.ends_with(".smudge")
                        || lowercase.ends_with(".process")
                        || lowercase.ends_with(".required")))
                    || (lowercase.starts_with("merge.") && lowercase.ends_with(".driver"))
                    || (lowercase.starts_with("diff.")
                        && (lowercase.ends_with(".textconv") || lowercase.ends_with(".command")))
            })
            // Git subsection names (the driver identifier between the dots) are case-sensitive.
            // Preserve the exact spelling for the command-line override.
            .map(str::to_string)
            .collect())
    }

    /// The current branch name (or a short SHA when detached).
    pub fn current_branch(&self) -> Result<String, GitError> {
        let branch = self.run(&["branch", "--show-current"])?;
        if branch.trim().is_empty() {
            Ok(self
                .run(&["rev-parse", "--short", "HEAD"])?
                .trim()
                .to_string())
        } else {
            Ok(branch.trim().to_string())
        }
    }

    /// The HEAD commit SHA.
    pub fn head_sha(&self) -> Result<String, GitError> {
        Ok(self.run(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// Porcelain status entries (`XY path`).
    pub fn status_porcelain(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .run(&["status", "--porcelain"])?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Repository-relative paths present in a tracked-file diff.
    ///
    /// Rename detection is disabled deliberately: callers can apply path-level disclosure rules
    /// to both the old and new names independently before asking for the corresponding content.
    pub fn diff_changed_paths(&self, staged: bool) -> Result<Vec<PathBuf>, GitError> {
        let mut command = self.repo_command()?;
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
        ]);
        if staged {
            command.arg("--cached");
        }
        command.current_dir(&self.root);
        let out = execute_git_command(&mut command)?;
        if !out.status.success() {
            return Err(GitError::Command(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        let raw = std::str::from_utf8(&out.stdout)
            .map_err(|_| GitError::Command("git diff returned a non-UTF-8 path".to_string()))?;
        let mut paths = raw
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Diff exact repository paths after validating that every path stays within this worktree.
    /// An empty path list returns an empty diff rather than accidentally widening to the whole
    /// repository.
    pub fn diff_paths(&self, paths: &[PathBuf], staged: bool) -> Result<String, GitError> {
        if paths.is_empty() {
            return Ok(String::new());
        }
        let relative = paths
            .iter()
            .map(|path| self.repo_relative(path))
            .collect::<Result<Vec<_>, _>>()?;
        let path_strs = relative
            .iter()
            .map(|path| {
                path.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| GitError::NonUtf8Path(path.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-renames"];
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.extend(path_strs.iter().map(String::as_str));
        self.run(&args)
    }

    /// Absolute paths changed in the worktree/index, including both sides of renames/copies.
    /// Intended for a clean-at-turn-start auto-commit flow so shell-created files and deletions
    /// are not missed by tool-local touched-path tracking.
    pub fn changed_paths(&self) -> Result<Vec<PathBuf>, GitError> {
        let mut command = self.repo_command()?;
        command
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .current_dir(&self.root);
        let out = execute_git_command(&mut command)?;
        if !out.status.success() {
            return Err(GitError::Command(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        let raw = std::str::from_utf8(&out.stdout)
            .map_err(|_| GitError::Command("git status returned a non-UTF-8 path".to_string()))?;
        let mut records = raw.split('\0');
        let mut paths = Vec::new();
        while let Some(record) = records.next() {
            if record.is_empty() {
                break;
            }
            let bytes = record.as_bytes();
            if bytes.len() < 4 || bytes[2] != b' ' {
                return Err(GitError::Command(format!(
                    "malformed porcelain status record: {record:?}"
                )));
            }
            paths.push(self.root.join(&record[3..]));
            if matches!(bytes[0], b'R' | b'C') || matches!(bytes[1], b'R' | b'C') {
                let original = records.next().ok_or_else(|| {
                    GitError::Command("rename status omitted original path".to_string())
                })?;
                if original.is_empty() {
                    return Err(GitError::Command(
                        "rename status contained an empty original path".to_string(),
                    ));
                }
                paths.push(self.root.join(original));
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Whether the worktree has any uncommitted changes.
    pub fn is_dirty(&self) -> Result<bool, GitError> {
        Ok(!self.status_porcelain()?.is_empty())
    }

    /// Whether HEAD has a parent (i.e. there is at least one prior commit).
    #[must_use]
    pub fn head_has_parent(&self) -> bool {
        self.run(&["rev-parse", "--verify", "HEAD~1"]).is_ok()
    }

    /// Commit the given paths as an agent commit: stages only those paths, neutralizes hooks,
    /// and attaches the session/turn trailers. Returns the new SHA, or `None` if the paths held
    /// no changes to commit.
    pub fn agent_commit(
        &self,
        paths: &[PathBuf],
        message: &str,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<String>, GitError> {
        if paths.is_empty() {
            return Ok(None);
        }
        // Stage only the agent-touched paths (the user's other dirty files are untouched).
        let relative_paths = paths
            .iter()
            .map(|path| self.repo_relative(path))
            .collect::<Result<Vec<_>, _>>()?;
        let path_strs = relative_paths
            .iter()
            .map(|path| {
                path.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| GitError::NonUtf8Path(path.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut add_args = vec!["add", "--"];
        add_args.extend(path_strs.iter().map(String::as_str));
        self.run_mutation(&add_args)?;

        // Anything actually staged among these paths?
        let mut staged_args = vec!["diff", "--cached", "--name-only", "--"];
        staged_args.extend(path_strs.iter().map(String::as_str));
        let staged = self.run(&staged_args)?;
        if staged.trim().is_empty() {
            return Ok(None);
        }

        let full_message = format!(
            "{message}\n\n{SESSION_TRAILER}: {}\n{TURN_TRAILER}: {}",
            session.as_uuid(),
            turn.as_uuid()
        );
        // Hooks, signing, fsmonitor, and repository-defined filters are neutralized by the
        // hardened mutation command; `--no-verify` is defense in depth.
        let mut commit_args = vec!["commit", "--no-verify", "-m", &full_message, "--"];
        commit_args.extend(path_strs.iter().map(String::as_str));
        self.run_mutation(&commit_args)?;
        Ok(Some(self.head_sha()?))
    }

    fn repo_relative(&self, path: &Path) -> Result<PathBuf, GitError> {
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(GitError::OutsideRepo(path.to_path_buf()));
        }
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let resolved = resolve_existing_ancestor(&candidate)
            .ok_or_else(|| GitError::OutsideRepo(path.to_path_buf()))?;
        let relative = resolved
            .strip_prefix(&self.root)
            .map_err(|_| GitError::OutsideRepo(path.to_path_buf()))?;
        if relative.as_os_str().is_empty() {
            return Err(GitError::OutsideRepo(path.to_path_buf()));
        }
        Ok(relative.to_path_buf())
    }

    /// Agent commits belonging to `session`, newest first, stopping at the first foreign commit.
    pub fn session_commits(&self, session: SessionId) -> Result<Vec<CommitInfo>, GitError> {
        // NUL is forbidden in commit messages, unlike the printable/control separators used by
        // many ad-hoc log parsers. `-z` terminates each three-field record with another NUL.
        let fmt = "--format=%H%x00%s%x00%b";
        let raw = self.run(&["log", "-z", "-n", "100", fmt])?;
        let session_str = session.as_uuid().to_string();
        let mut commits = Vec::new();
        let mut fields = raw.split('\0');
        while let (Some(sha), Some(subject), Some(body)) =
            (fields.next(), fields.next(), fields.next())
        {
            if sha.is_empty() {
                break;
            }
            let this_session = trailer_value(body, SESSION_TRAILER);
            // Stop at the first commit that isn't from this session (contiguous run only).
            if this_session.as_deref() != Some(session_str.as_str()) {
                break;
            }
            commits.push(CommitInfo {
                sha: sha.trim().to_string(),
                subject: subject.to_string(),
                session: this_session,
                turn: trailer_value(body, TURN_TRAILER),
            });
        }
        Ok(commits)
    }

    /// Undo the most recent agent commit for `session`. If it is at HEAD, has a parent, and no
    /// local remote-tracking ref contains it, tidy history with `reset --keep`; otherwise
    /// preserve published/non-tip history with `revert`.
    pub fn undo_last(&self, session: SessionId) -> Result<Option<String>, GitError> {
        let commits = self.session_commits(session)?;
        let Some(top) = commits.first() else {
            return Ok(None);
        };
        let head = self.head_sha()?;
        if top.sha == head && self.head_has_parent() && !self.remote_ref_contains(&top.sha)? {
            self.run_mutation(&["reset", "--keep", "HEAD~1"])?;
            Ok(Some(format!("reset past {}", short(&top.sha))))
        } else {
            self.run_mutation(&["revert", "--no-edit", &top.sha])?;
            Ok(Some(format!("reverted {}", short(&top.sha))))
        }
    }

    /// Whether any locally known remote-tracking ref contains `sha`. Git updates the relevant
    /// tracking ref after an ordinary successful push, so this prevents `/undo` from rewriting
    /// published history without requiring network access at undo time.
    fn remote_ref_contains(&self, sha: &str) -> Result<bool, GitError> {
        let contains = format!("--contains={sha}");
        Ok(!self
            .run(&[
                "for-each-ref",
                "--format=%(refname)",
                &contains,
                "refs/remotes",
            ])?
            .trim()
            .is_empty())
    }

    /// A `--stat` summary of a diff (e.g. `base..HEAD`).
    pub fn diff_stat(&self, range: &str) -> Result<String, GitError> {
        self.run(&["diff", "--no-ext-diff", "--no-textconv", "--stat", range])
    }

    /// Create a worktree at `path` on a new `branch` from `base` (for subagents, M10).
    pub fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        let p = path
            .to_str()
            .ok_or_else(|| GitError::NonUtf8Path(path.to_path_buf()))?;
        self.run_mutation(&["worktree", "add", "-b", branch, "--", p, base])?;
        Ok(())
    }

    /// Remove a worktree.
    pub fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        let p = path
            .to_str()
            .ok_or_else(|| GitError::NonUtf8Path(path.to_path_buf()))?;
        self.run_mutation(&["worktree", "remove", "--", p])?;
        Ok(())
    }
}

fn repository_marker_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return std::fs::canonicalize(current).ok();
        }
        current = current.parent()?;
    }
}

fn trailer_value(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    body.lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(&prefix).map(str::to_string))
}

fn hardened_git_command() -> Result<Command, GitError> {
    let executable = trusted_git_executable()?;
    let inherited: Vec<_> = SAFE_GIT_ENV
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (*key, value)))
        .collect();
    let mut command = Command::new(executable);
    // Start from a minimal environment: API keys and ambient GIT_* controls must never reach
    // even the trusted host Git process. A small allowlist retains user identity/config lookup,
    // locale handling, and platform temp/system paths.
    command.env_clear();
    for (key, value) in inherited {
        command.env(key, value);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--literal-pathspecs")
        .args([
            "-c",
            "core.hooksPath=",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "diff.external=",
        ]);
    Ok(command)
}

/// Resolve Git without ever executing an ambient-PATH candidate. Only conventional system
/// locations (or an immutable Nix store path) whose file and ancestor directories are owned by
/// root and not group/world-writable are accepted on Unix. This prevents an untrusted checkout
/// from supplying the executable used by the trusted host mutation boundary.
fn trusted_git_executable() -> Result<PathBuf, GitError> {
    let executable_name = if cfg!(windows) { "git.exe" } else { "git" };
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    directories.extend(
        ["/usr/bin", "/bin", "/usr/local/bin", "/opt/local/bin"]
            .into_iter()
            .map(PathBuf::from),
    );
    directories.sort();
    directories.dedup();

    let mut rejected = Vec::new();
    for directory in directories {
        let candidate = directory.join(executable_name);
        let Ok(candidate) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if trusted_system_executable(&candidate) {
            return Ok(candidate);
        }
        if rejected.len() < 4 {
            rejected.push(candidate.display().to_string());
        }
    }
    let detail = if rejected.is_empty() {
        "no Git binary was found in an absolute PATH entry or standard system directory".to_string()
    } else {
        format!("rejected untrusted candidate(s): {}", rejected.join(", "))
    };
    Err(GitError::UntrustedExecutable(detail))
}

#[cfg(unix)]
fn trusted_system_executable(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let in_standard_bin = matches!(
        path.parent(),
        Some(parent)
            if parent == Path::new("/usr/bin")
                || parent == Path::new("/usr/local/bin")
                || parent == Path::new("/opt/local/bin")
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
    // Windows ACL/owner verification is not yet implemented. Host Git mutations are a security
    // boundary, so non-Unix platforms fail closed instead of trusting a PATH or install prefix.
    false
}

#[derive(Debug)]
struct GitCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn execute_git_command(command: &mut Command) -> Result<GitCommandOutput, GitError> {
    execute_command(command, GIT_TIMEOUT, GIT_OUTPUT_CAP)
}

fn execute_command(
    command: &mut Command,
    timeout: Duration,
    output_cap: usize,
) -> Result<GitCommandOutput, GitError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let child_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Command("git stdout pipe was unavailable".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitError::Command("git stderr pipe was unavailable".to_string()))?;
    let stdout_reader = std::thread::spawn(move || read_capped(stdout, output_cap));
    let stderr_reader = std::thread::spawn(move || read_capped(stderr, output_cap));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            // A repository-controlled helper must not outlive the trusted Git operation or keep
            // an inherited output pipe open forever after the Git leader exits.
            #[cfg(unix)]
            kill_git_process_group(child_id);
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            kill_git_process_group(child_id);
            let _ = child.kill();
            let _ = child.wait();
            // Reader threads may still be blocked on pipes inherited by a descendant that
            // escaped the process group. Detach them rather than turning a bounded timeout into
            // an unbounded host-process hang.
            drop(stdout_reader);
            drop(stderr_reader);
            return Err(GitError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let (stdout, stdout_truncated) = join_reader_until(stdout_reader, deadline, timeout)?;
    let (stderr, stderr_truncated) = join_reader_until(stderr_reader, deadline, timeout)?;
    if stdout_truncated || stderr_truncated {
        return Err(GitError::OutputLimit(output_cap));
    }
    Ok(GitCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_capped<R: Read>(mut reader: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(cap.min(64 * 1024));
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok((retained, truncated));
        }
        let remaining = cap.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
}

fn join_reader_until(
    reader: std::thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
    deadline: Instant,
    timeout: Duration,
) -> Result<(Vec<u8>, bool), GitError> {
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            // Dropping a JoinHandle detaches the bounded-memory reader. This can happen only if
            // a descendant retained the pipe after the trusted Git leader was reaped.
            drop(reader);
            return Err(GitError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    reader
        .join()
        .map_err(|_| GitError::Command("git output reader panicked".to_string()))?
        .map_err(GitError::Io)
}

#[cfg(unix)]
fn kill_git_process_group(id: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{id}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn resolve_existing_ancestor(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Some(path);
    }
    let mut missing = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).ok()?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Git) {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@grokforge.dev"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README"), "start\n").unwrap();
        run(&["add", "README"]);
        run(&["commit", "-q", "-m", "init"]);
        let git = Git::discover(dir.path()).expect("repo");
        (dir, git)
    }

    #[test]
    fn discovers_repo_and_branch() {
        let (_d, git) = init_repo();
        assert!(git.current_branch().is_ok());
        assert!(!git.is_dirty().unwrap());
    }

    #[test]
    fn diff_reads_are_scoped_to_validated_paths_and_stage() {
        let (dir, git) = init_repo();
        std::fs::write(dir.path().join("README"), "unstaged\n").unwrap();
        std::fs::write(dir.path().join("staged.txt"), "staged\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "staged.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );

        assert_eq!(
            git.diff_changed_paths(false).unwrap(),
            vec![PathBuf::from("README")]
        );
        assert_eq!(
            git.diff_changed_paths(true).unwrap(),
            vec![PathBuf::from("staged.txt")]
        );

        let unstaged = git.diff_paths(&[PathBuf::from("README")], false).unwrap();
        assert!(unstaged.contains("+unstaged"));
        assert!(!unstaged.contains("staged.txt"));
        let staged = git
            .diff_paths(&[dir.path().join("staged.txt")], true)
            .unwrap();
        assert!(staged.contains("+staged"));
        assert!(!staged.contains("README"));
        assert_eq!(git.diff_paths(&[], false).unwrap(), "");
    }

    #[test]
    fn diff_paths_rejects_paths_outside_the_repository() {
        let (dir, git) = init_repo();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let error = git
            .diff_paths(&[outside.path().to_path_buf()], false)
            .unwrap_err();
        assert!(matches!(error, GitError::OutsideRepo(_)));
        let escape = PathBuf::from("missing/../../outside.txt");
        let error = git.diff_paths(&[escape], false).unwrap_err();
        assert!(matches!(error, GitError::OutsideRepo(_)));
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn discovery_rejects_core_worktree_redirection() {
        let (dir, _git) = init_repo();
        let outside = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args([
                "config",
                "core.worktree",
                outside.path().to_string_lossy().as_ref(),
            ])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert!(Git::discover(dir.path()).is_none());
    }

    #[test]
    fn agent_commit_stages_only_named_paths_and_adds_trailers() {
        let (dir, git) = init_repo();
        std::fs::write(dir.path().join("a.txt"), "agent edit\n").unwrap();
        std::fs::write(dir.path().join("user.txt"), "user's own change\n").unwrap();

        let session = SessionId::new();
        let sha = git
            .agent_commit(&[PathBuf::from("a.txt")], "add a", session, TurnId::new())
            .unwrap()
            .expect("committed");
        assert!(!sha.is_empty());

        // user.txt was NOT swept into the commit.
        let status = git.status_porcelain().unwrap();
        assert!(status.iter().any(|s| s.contains("user.txt")));

        let commits = git.session_commits(session).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].session.as_deref(),
            Some(session.as_uuid().to_string().as_str())
        );
    }

    #[test]
    fn session_commits_stop_at_foreign_commit() {
        let (dir, git) = init_repo();
        let s1 = SessionId::new();
        std::fs::write(dir.path().join("a.txt"), "1\n").unwrap();
        git.agent_commit(&[PathBuf::from("a.txt")], "a", s1, TurnId::new())
            .unwrap();

        std::fs::write(dir.path().join("b.txt"), "2\n").unwrap();
        Command::new("git")
            .args(["add", "b.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "human commit"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        assert!(git.session_commits(s1).unwrap().is_empty());
    }

    #[test]
    fn undo_last_removes_the_agent_commit() {
        let (dir, git) = init_repo();
        let session = SessionId::new();
        std::fs::write(dir.path().join("a.txt"), "1\n").unwrap();
        git.agent_commit(&[PathBuf::from("a.txt")], "a", session, TurnId::new())
            .unwrap();
        let before = git.head_sha().unwrap();

        let msg = git.undo_last(session).unwrap().expect("undone");
        assert!(msg.contains("reset") || msg.contains("revert"));
        assert_ne!(git.head_sha().unwrap(), before);
        assert!(git.session_commits(session).unwrap().is_empty());
    }

    #[test]
    fn undo_reverts_instead_of_rewriting_a_published_agent_commit() {
        let (dir, git) = init_repo();
        let remote = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .current_dir(remote.path())
                .status()
                .unwrap()
                .success()
        );
        let remote_path = remote.path().to_str().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        run(&["remote", "add", "origin", remote_path]);
        run(&["push", "-q", "-u", "origin", "HEAD"]);

        std::fs::write(dir.path().join("published.txt"), "agent\n").unwrap();
        let session = SessionId::new();
        let published = git
            .agent_commit(
                &[PathBuf::from("published.txt")],
                "published agent edit",
                session,
                TurnId::new(),
            )
            .unwrap()
            .expect("commit");
        run(&["push", "-q"]);
        assert!(git.remote_ref_contains(&published).unwrap());

        let message = git.undo_last(session).unwrap().expect("undo");
        assert!(message.contains("reverted"));
        assert_ne!(git.head_sha().unwrap(), published);
        assert!(!dir.path().join("published.txt").exists());
    }

    #[test]
    fn worktree_add_and_remove_round_trip() {
        let (dir, git) = init_repo();
        let hook_marker = dir.path().join("hook-ran");
        let hook = dir.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let wt = dir.path().join("wt");
        git.worktree_add(&wt, "gf/agent/1", "HEAD").unwrap();
        assert!(wt.join("README").exists());
        assert!(
            !hook_marker.exists(),
            "worktree mutation executed repository hook"
        );
        let worktree_git = Git::discover(&wt).expect("linked worktree repo");
        let metadata = worktree_git.metadata_paths().expect("metadata paths");
        let common = std::fs::canonicalize(dir.path().join(".git")).unwrap();
        assert!(metadata.iter().any(|path| path == &common));
        assert!(
            metadata
                .iter()
                .any(|path| path.starts_with(common.join("worktrees")))
        );
        git.worktree_remove(&wt).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn detached_head_reports_a_short_sha_instead_of_head() {
        let (dir, git) = init_repo();
        let ok = Command::new("git")
            .args(["checkout", "--detach", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let branch = git.current_branch().unwrap();
        assert_ne!(branch, "HEAD");
        assert!(git.head_sha().unwrap().starts_with(&branch));
    }

    #[test]
    fn staged_detection_is_scoped_to_agent_paths() {
        let (dir, git) = init_repo();
        std::fs::write(dir.path().join("user.txt"), "user staged\n").unwrap();
        let ok = Command::new("git")
            .args(["add", "user.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let result = git
            .agent_commit(
                &[PathBuf::from("README")],
                "no README change",
                SessionId::new(),
                TurnId::new(),
            )
            .unwrap();
        assert!(result.is_none());
        let staged = git.run(&["diff", "--cached", "--name-only"]).unwrap();
        assert_eq!(staged.trim(), "user.txt");
    }

    #[test]
    fn agent_paths_are_literal_not_pathspec_patterns() {
        let (dir, git) = init_repo();
        std::fs::write(dir.path().join("star*.txt"), "agent\n").unwrap();
        std::fs::write(dir.path().join("starA.txt"), "user\n").unwrap();
        git.agent_commit(
            &[PathBuf::from("star*.txt")],
            "literal path",
            SessionId::new(),
            TurnId::new(),
        )
        .unwrap();
        let status = git.status_porcelain().unwrap();
        assert!(status.iter().any(|entry| entry.contains("starA.txt")));
    }

    #[test]
    fn repository_clean_filter_never_executes_in_host_process() {
        let (dir, git) = init_repo();
        let marker = dir.path().join("filter-ran");
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=EVIL\n").unwrap();
        std::fs::write(dir.path().join("safe.txt"), "initial\n").unwrap();
        let ok = Command::new("git")
            .args(["add", ".gitattributes", "safe.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let ok = Command::new("git")
            .args(["commit", "-q", "-m", "attributes"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let filter = format!("sh -c 'touch \"{}\"; cat'", marker.display());
        let ok = Command::new("git")
            .args(["config", "filter.EVIL.clean", &filter])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let ok = Command::new("git")
            .args(["config", "filter.EVIL.required", "true"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);

        std::fs::write(dir.path().join("safe.txt"), "content\n").unwrap();
        assert!(git.is_dirty().unwrap());
        assert!(
            git.changed_paths()
                .unwrap()
                .iter()
                .any(|path| path.ends_with("safe.txt"))
        );
        assert!(!marker.exists(), "status executed repository clean filter");
        git.agent_commit(
            &[PathBuf::from("safe.txt")],
            "safe filter",
            SessionId::new(),
            TurnId::new(),
        )
        .unwrap();
        assert!(!marker.exists(), "repository clean filter executed");
    }

    #[test]
    fn repository_external_diff_command_never_executes_in_host_process() {
        let (dir, git) = init_repo();
        let marker = dir.path().join("diff-command-ran");
        std::fs::write(dir.path().join(".gitattributes"), "README diff=EVIL\n").unwrap();
        let command = format!("sh -c 'touch \"{}\"'", marker.display());
        let ok = Command::new("git")
            .args(["config", "diff.EVIL.command", &command])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::write(dir.path().join("README"), "changed\n").unwrap();

        // The override may make Git decline the custom diff, but repository-controlled code
        // must never execute in this trusted host process.
        let _ = git.run(&["diff"]);
        assert!(
            !marker.exists(),
            "repository external diff command executed"
        );
    }

    #[test]
    fn repository_merge_driver_is_replaced_with_a_failing_command() {
        let (dir, git) = init_repo();
        let ok = Command::new("git")
            .args(["config", "merge.EVIL.driver", "touch should-never-run"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let command = git.repo_command().unwrap();
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "merge.EVIL.driver=false")
        );
        assert!(!arguments.iter().any(|argument| argument.contains("touch")));
    }

    #[cfg(unix)]
    #[test]
    fn repo_local_executable_is_not_a_trusted_git_binary() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("git");
        std::fs::write(&candidate, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!trusted_system_executable(&candidate));
    }

    #[test]
    fn appended_trailer_wins_over_spoofed_message_trailer() {
        let (dir, git) = init_repo();
        let session = SessionId::new();
        std::fs::write(dir.path().join("a.txt"), "agent\n").unwrap();
        git.agent_commit(
            &[PathBuf::from("a.txt")],
            "message\n\nGrokforge-Session: attacker-controlled",
            session,
            TurnId::new(),
        )
        .unwrap();
        let commits = git.session_commits(session).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].session.as_deref(),
            Some(session.as_uuid().to_string().as_str())
        );
    }

    #[test]
    fn absolute_in_repo_paths_commit_and_outside_paths_are_rejected() {
        let (dir, git) = init_repo();
        let inside = dir.path().join("absolute.txt");
        std::fs::write(&inside, "inside\n").unwrap();
        let committed = git
            .agent_commit(
                std::slice::from_ref(&inside),
                "absolute",
                SessionId::new(),
                TurnId::new(),
            )
            .unwrap();
        assert!(committed.is_some());

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside.txt");
        std::fs::write(&outside, "outside\n").unwrap();
        let error = git
            .agent_commit(
                std::slice::from_ref(&outside),
                "outside",
                SessionId::new(),
                TurnId::new(),
            )
            .expect_err("outside path must be rejected");
        assert!(matches!(error, GitError::OutsideRepo(path) if path == outside));
    }

    #[test]
    fn worktree_remove_preserves_dirty_worktree() {
        let (dir, git) = init_repo();
        let wt = dir.path().join("dirty-wt");
        git.worktree_add(&wt, "gf/dirty", "HEAD").unwrap();
        std::fs::write(wt.join("untracked.txt"), "keep me\n").unwrap();
        assert!(git.worktree_remove(&wt).is_err());
        assert!(wt.join("untracked.txt").exists());
        let ok = Command::new("git")
            .args(["worktree", "remove", "--force", "--", &wt.to_string_lossy()])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    #[test]
    fn repository_merge_driver_never_executes_in_host_process() {
        let (dir, git) = init_repo();
        let marker = dir.path().join("merge-driver-ran");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "conflict.txt merge=evil\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("conflict.txt"), "base\n").unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(run(&["add", ".gitattributes", "conflict.txt"]));
        assert!(run(&["commit", "-q", "-m", "merge base"]));

        std::fs::write(dir.path().join("conflict.txt"), "target\n").unwrap();
        assert!(run(&["add", "conflict.txt"]));
        assert!(run(&["commit", "-q", "-m", "target"]));
        let target = git.head_sha().unwrap();
        std::fs::write(dir.path().join("conflict.txt"), "later\n").unwrap();
        assert!(run(&["add", "conflict.txt"]));
        assert!(run(&["commit", "-q", "-m", "later"]));

        let driver = format!("sh -c 'touch \"{}\"; exit 0'", marker.display());
        assert!(
            Command::new("git")
                .args(["config", "merge.evil.driver", &driver])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let _ = git.run_mutation(&["revert", "--no-edit", &target]);
        assert!(!marker.exists(), "repository merge driver executed");
        let _ = run(&["revert", "--abort"]);
    }

    #[test]
    fn changed_paths_includes_untracked_deletions_and_both_rename_sides() {
        let (dir, git) = init_repo();
        std::fs::write(dir.path().join("delete-me"), "tracked\n").unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(run(&["add", "delete-me"]));
        assert!(run(&["commit", "-q", "-m", "fixture"]));
        assert!(run(&["mv", "README", "moved-readme"]));
        std::fs::remove_file(dir.path().join("delete-me")).unwrap();
        std::fs::write(dir.path().join("untracked"), "new\n").unwrap();

        let changed = git.changed_paths().unwrap();
        for expected in ["README", "moved-readme", "delete-me", "untracked"] {
            assert!(
                changed.contains(&std::fs::canonicalize(dir.path()).unwrap().join(expected)),
                "missing {expected}: {changed:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn command_runner_caps_output_and_times_out() {
        let mut noisy = Command::new("/bin/sh");
        noisy.args(["-c", "awk 'BEGIN { for(i=0;i<10000;i++) printf \"x\" }'"]);
        let error = execute_command(&mut noisy, Duration::from_secs(2), 128)
            .expect_err("large output must be rejected");
        assert!(matches!(error, GitError::OutputLimit(128)));

        let mut slow = Command::new("/bin/sh");
        slow.args(["-c", "sleep 5"]);
        let error = execute_command(&mut slow, Duration::from_millis(50), 128)
            .expect_err("slow command must time out");
        assert!(matches!(error, GitError::Timeout(_)));
    }

    #[cfg(unix)]
    #[test]
    fn command_runner_cleans_up_background_descendants_after_success() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let started = Instant::now();
        let output = execute_command(&mut command, Duration::from_secs(2), 128)
            .expect("leader should exit successfully");
        assert!(output.status.success());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "background child retained Git output pipes"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_runner_deadline_includes_descendant_pipe_drain() {
        let fixture = tempfile::tempdir().expect("fixture");
        let ready_file = fixture.path().join("escaped-pid");
        let mut command = Command::new("/bin/sh");
        command.env("READY_FILE", &ready_file).args([
            "-c",
            "setsid /bin/sh -c 'printf \"%s\" \"$$\" > \"$READY_FILE\"; exec sleep 5' & while [ ! -s \"$READY_FILE\" ]; do sleep 0.01; done; exit 0",
        ]);
        let started = Instant::now();
        let result = execute_command(&mut command, Duration::from_millis(100), 128);
        if let Ok(pid) = std::fs::read_to_string(&ready_file) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
        let error = result.expect_err("escaped descendant pipe must remain deadline-bounded");
        assert!(matches!(error, GitError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
