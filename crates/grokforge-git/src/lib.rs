//! `grokforge-git` — git operations run from the **trusted host process** (never inside the
//! sandbox; `.git` is deny-write there). v0.1 shells out to the `git` CLI for both reads and
//! mutations for robustness; a `gix`-backed read path is a later optimization.
//!
//! The git-native workflow (docs/design/04-ux-spec.md §5): every agent-touched change becomes a
//! real commit carrying `Grokforge-Session`/`Grokforge-Turn` trailers (the marker `/undo` keys
//! off), with hooks neutralized so a repo hook can't be used as an injection vector.

use std::path::{Path, PathBuf};
use std::process::Command;

use grokforge_protocol::{SessionId, TurnId};

/// The trailer key identifying which session produced a commit.
pub const SESSION_TRAILER: &str = "Grokforge-Session";
/// The trailer key identifying which turn produced a commit.
pub const TURN_TRAILER: &str = "Grokforge-Turn";

/// Errors from git operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("git command failed: {0}")]
    Command(String),
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
        let out = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(start)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return None;
        }
        Some(Self {
            root: PathBuf::from(root),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(GitError::Command(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }

    /// The current branch name (or a short SHA when detached).
    pub fn current_branch(&self) -> Result<String, GitError> {
        Ok(self
            .run(&["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string())
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
        let path_strs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let mut add_args = vec!["add", "--"];
        add_args.extend(path_strs.iter().map(String::as_str));
        self.run(&add_args)?;

        // Anything actually staged among these paths?
        let staged = self.run(&["diff", "--cached", "--name-only", "--"])?;
        if staged.trim().is_empty() {
            return Ok(None);
        }

        let full_message =
            format!("{message}\n\n{SESSION_TRAILER}: {session}\n{TURN_TRAILER}: {turn}");
        // Hooks neutralized (`--no-verify` + empty hooksPath) so a repo hook can't run.
        let mut commit_args = vec![
            "-c",
            "core.hooksPath=",
            "commit",
            "--no-verify",
            "-m",
            &full_message,
            "--",
        ];
        commit_args.extend(path_strs.iter().map(String::as_str));
        self.run(&commit_args)?;
        Ok(Some(self.head_sha()?))
    }

    /// Agent commits belonging to `session`, newest first, stopping at the first foreign commit.
    pub fn session_commits(&self, session: SessionId) -> Result<Vec<CommitInfo>, GitError> {
        let fmt = "--format=%H\x1e%s\x1e%b\x1f";
        let raw = self.run(&["log", "-n", "100", fmt])?;
        let session_str = session.to_string();
        let mut commits = Vec::new();
        for record in raw.split('\x1f') {
            let record = record.trim_start_matches('\n');
            if record.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = record.split('\x1e').collect();
            if parts.len() < 3 {
                continue;
            }
            let body = parts[2];
            let this_session = trailer_value(body, SESSION_TRAILER);
            // Stop at the first commit that isn't from this session (contiguous run only).
            if this_session.as_deref() != Some(session_str.as_str()) {
                break;
            }
            commits.push(CommitInfo {
                sha: parts[0].trim().to_string(),
                subject: parts[1].to_string(),
                session: this_session,
                turn: trailer_value(body, TURN_TRAILER),
            });
        }
        Ok(commits)
    }

    /// Undo the most recent agent commit for `session`. If it is at HEAD and the repo has a
    /// parent, tidy history with `reset --keep`; otherwise preserve history with `revert`.
    pub fn undo_last(&self, session: SessionId) -> Result<Option<String>, GitError> {
        let commits = self.session_commits(session)?;
        let Some(top) = commits.first() else {
            return Ok(None);
        };
        let head = self.head_sha()?;
        if top.sha == head && self.head_has_parent() {
            self.run(&["reset", "--keep", "HEAD~1"])?;
            Ok(Some(format!("reset past {}", short(&top.sha))))
        } else {
            self.run(&["revert", "--no-edit", &top.sha])?;
            Ok(Some(format!("reverted {}", short(&top.sha))))
        }
    }

    /// A `--stat` summary of a diff (e.g. `base..HEAD`).
    pub fn diff_stat(&self, range: &str) -> Result<String, GitError> {
        self.run(&["diff", "--stat", range])
    }

    /// Create a worktree at `path` on a new `branch` from `base` (for subagents, M10).
    pub fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        let p = path.to_string_lossy();
        self.run(&["worktree", "add", "-b", branch, &p, base])?;
        Ok(())
    }

    /// Remove a worktree.
    pub fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        let p = path.to_string_lossy();
        self.run(&["worktree", "remove", "--force", &p])?;
        Ok(())
    }
}

fn trailer_value(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    body.lines()
        .find_map(|l| l.trim().strip_prefix(&prefix).map(str::to_string))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(test)]
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
            Some(session.to_string().as_str())
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
    fn worktree_add_and_remove_round_trip() {
        let (dir, git) = init_repo();
        let wt = dir.path().join("wt");
        git.worktree_add(&wt, "gf/agent/1", "HEAD").unwrap();
        assert!(wt.join("README").exists());
        git.worktree_remove(&wt).unwrap();
        assert!(!wt.exists());
    }
}
