//! Session persistence. The canonical record is an **append-only JSONL rollout** (ADR 0002):
//! one line per [`ResponseItem`], never rotated, so history is never lost. Size-capped rotation
//! applies only to debug logs (the anti-"640 TB/yr" guard) — its math lives here and is tested.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use grokforge_protocol::{ResponseItem, SessionId};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

/// The directory where session rollouts and metadata live (XDG data dir).
#[must_use]
pub fn sessions_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "grokforge", "grokforge").map_or_else(
        || PathBuf::from(".grokforge/sessions"),
        |d| d.data_dir().join("sessions"),
    )
}

/// Path to a session's rollout file, keyed by its full UUID string.
#[must_use]
pub fn rollout_path(dir: &Path, session_uuid: &str) -> PathBuf {
    dir.join(format!("rollout-{session_uuid}.jsonl"))
}

/// Lightweight metadata written next to each rollout so sessions can be listed and resumed
/// without parsing the whole transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub workspace: PathBuf,
    pub model: String,
    pub created_unix: i64,
    pub first_prompt: String,
}

impl SessionMeta {
    #[must_use]
    pub fn new(session: SessionId, workspace: PathBuf, model: String, first_prompt: &str) -> Self {
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        Self {
            session_id: session.as_uuid().to_string(),
            workspace,
            model,
            created_unix,
            first_prompt: first_prompt.chars().take(120).collect(),
        }
    }

    /// The rollout file for this session.
    #[must_use]
    pub fn rollout(&self, dir: &Path) -> PathBuf {
        rollout_path(dir, &self.session_id)
    }

    fn path(dir: &Path, session: SessionId) -> PathBuf {
        dir.join(format!("rollout-{}.meta.json", session.as_uuid()))
    }

    /// Write this metadata beside the rollout.
    pub async fn write(&self, dir: &Path, session: SessionId) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(Self::path(dir, session), json).await
    }

    /// List all session metadata in `dir`, newest first.
    pub async fn list(dir: &Path) -> Vec<SessionMeta> {
        let mut metas = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return metas;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".meta.json") {
                if let Ok(text) = tokio::fs::read_to_string(&path).await {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&text) {
                        metas.push(meta);
                    }
                }
            }
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.created_unix));
        metas
    }
}

/// Appends conversation items to a session's JSONL rollout file.
#[derive(Debug)]
pub struct RolloutWriter {
    path: PathBuf,
    file: tokio::fs::File,
}

impl RolloutWriter {
    /// Open (creating if needed) the rollout for a session under `dir`.
    pub async fn create(dir: &Path, session: SessionId) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(dir).await?;
        let path = rollout_path(dir, &session.as_uuid().to_string());
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { path, file })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one item as a JSON line.
    pub async fn append(&mut self, item: &ResponseItem) -> std::io::Result<()> {
        let mut line = serde_json::to_string(item)?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).await?;
        self.file.flush().await
    }

    /// Read a rollout back into memory (for resume). Malformed lines are skipped.
    pub async fn read_all(path: &Path) -> std::io::Result<Vec<ResponseItem>> {
        let text = tokio::fs::read_to_string(path).await?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

/// Rotation policy for debug logs.
#[derive(Debug, Clone, Copy)]
pub struct LogRotation {
    pub max_file_bytes: u64,
    pub max_files: u32,
}

impl Default for LogRotation {
    fn default() -> Self {
        // 5 MB × 5 files = 25 MB hard ceiling for debug logs.
        Self {
            max_file_bytes: 5 * 1024 * 1024,
            max_files: 5,
        }
    }
}

impl LogRotation {
    /// Whether writing `incoming` bytes to a file currently at `current_bytes` should trigger a
    /// rotation first.
    #[must_use]
    pub fn should_rotate(&self, current_bytes: u64, incoming: u64) -> bool {
        current_bytes > 0 && current_bytes.saturating_add(incoming) > self.max_file_bytes
    }

    /// The absolute upper bound on total bytes this policy can retain.
    #[must_use]
    pub fn max_total_bytes(&self) -> u64 {
        self.max_file_bytes
            .saturating_mul(u64::from(self.max_files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_math_caps_total_disk_usage() {
        let policy = LogRotation {
            max_file_bytes: 1000,
            max_files: 3,
        };
        // Fits within the current file.
        assert!(!policy.should_rotate(500, 400));
        // Would exceed the per-file cap -> rotate first.
        assert!(policy.should_rotate(900, 200));
        // An empty file never rotates even for a large write (avoids infinite rotation).
        assert!(!policy.should_rotate(0, 5000));
        // Bounded total.
        assert_eq!(policy.max_total_bytes(), 3000);
    }

    #[tokio::test]
    async fn rollout_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let mut w = RolloutWriter::create(dir.path(), id).await.unwrap();
        w.append(&ResponseItem::user("hi")).await.unwrap();
        w.append(&ResponseItem::assistant("hello")).await.unwrap();
        let items = RolloutWriter::read_all(w.path()).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], ResponseItem::user("hi"));
    }
}
