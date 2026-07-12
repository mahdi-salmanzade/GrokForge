//! Session persistence. The canonical record is an **append-only JSONL rollout** (ADR 0002):
//! one line per [`ResponseItem`], never rotated, so history is never lost. Size-capped rotation
//! applies only to debug logs (the anti-"640 TB/yr" guard) — its math lives here and is tested.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use grokforge_protocol::{ResponseItem, SessionId};
use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader,
};

const MAX_ROLLOUT_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_VISIBLE_ROLLOUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_VISIBLE_ROLLOUT_ITEMS: usize = 1_000_000;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_METADATA_ENTRIES_SCANNED: usize = 10_000;
const MAX_METADATA_RESULTS: usize = 1_000;
const MAX_METADATA_WORKSPACE_BYTES: usize = 4 * 1024;
const MAX_METADATA_MODEL_BYTES: usize = 256;
const MAX_METADATA_PROMPT_BYTES: usize = 1024;

/// The directory where session rollouts and metadata live (XDG data dir).
pub fn sessions_dir() -> std::io::Result<PathBuf> {
    let project =
        directories::ProjectDirs::from("dev", "grokforge", "grokforge").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no absolute per-user data directory is available for session storage",
            )
        })?;
    let dir = project.data_dir().join("sessions");
    if !dir.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "per-user session directory is not absolute",
        ));
    }
    Ok(dir)
}

pub(crate) fn prepare_sessions_dir_blocking() -> std::io::Result<PathBuf> {
    let dir = sessions_dir()?;
    ensure_private_dir_blocking(&dir)?;
    std::fs::canonicalize(dir)
}

/// Owner-private root for isolated subagent worktrees. This is deliberately outside every
/// project workspace so a parent or sibling workspace-write sandbox cannot mutate it.
pub(crate) fn worktrees_dir() -> std::io::Result<PathBuf> {
    let project =
        directories::ProjectDirs::from("dev", "grokforge", "grokforge").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no absolute per-user data directory is available for subagent worktrees",
            )
        })?;
    let dir = project.data_dir().join("worktrees");
    if !dir.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "per-user subagent worktree directory is not absolute",
        ));
    }
    Ok(dir)
}

pub(crate) async fn prepare_worktrees_dir() -> std::io::Result<PathBuf> {
    tokio::task::spawn_blocking(prepare_worktrees_dir_blocking)
        .await
        .map_err(|error| {
            std::io::Error::other(format!("worktree directory task failed: {error}"))
        })?
}

pub(crate) fn prepare_worktrees_dir_blocking() -> std::io::Result<PathBuf> {
    let dir = worktrees_dir()?;
    ensure_private_dir_blocking(&dir)?;
    Ok(dir)
}

/// Path to a session's rollout file, keyed by its full UUID string.
#[must_use]
pub fn rollout_path(dir: &Path, session_uuid: &str) -> PathBuf {
    let safe: String = session_uuid
        .chars()
        .map(|c| {
            if c.is_ascii_hexdigit() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    dir.join(format!("rollout-{safe}.jsonl"))
}

/// Lightweight metadata written next to each rollout so sessions can be listed and resumed
/// without parsing the whole transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub workspace: PathBuf,
    /// Canonical repository root (or canonical workspace for a non-repository session).
    #[serde(default)]
    pub workspace_identity: Option<PathBuf>,
    /// Best-effort filesystem identity for the workspace and Git metadata at creation.
    #[serde(default)]
    pub workspace_fingerprint: Option<String>,
    pub model: String,
    pub created_unix: i64,
    /// Nanosecond tie-breaker for sessions created in the same second.
    #[serde(default)]
    pub created_unix_nanos: u32,
    pub first_prompt: String,
}

impl SessionMeta {
    #[must_use]
    pub fn new(session: SessionId, workspace: PathBuf, model: String, first_prompt: &str) -> Self {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).ok();
        let created_unix = created
            .as_ref()
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let created_unix_nanos = created.map_or(0, |d| d.subsec_nanos());
        let prompt = crate::redaction::Redactor::apply(first_prompt);
        let canonical_workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        let workspace_identity = grokforge_git::Git::discover(&canonical_workspace)
            .and_then(|git| std::fs::canonicalize(git.root()).ok())
            .or_else(|| Some(canonical_workspace.clone()));
        let workspace_fingerprint = workspace_fingerprint(&canonical_workspace);
        Self {
            session_id: session.as_uuid().to_string(),
            workspace: canonical_workspace,
            workspace_identity,
            workspace_fingerprint,
            model,
            created_unix,
            created_unix_nanos,
            first_prompt: prompt
                .text
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(120)
                .collect(),
        }
    }

    /// The rollout file for this session.
    #[must_use]
    pub fn rollout(&self, dir: &Path) -> PathBuf {
        rollout_path(dir, &self.session_id)
    }

    /// Verify the creation-time filesystem/Git identity when present. Legacy metadata without a
    /// fingerprint is handled by the caller's canonical path check.
    #[must_use]
    pub fn fingerprint_matches(&self, workspace: &Path) -> bool {
        self.workspace_fingerprint
            .as_ref()
            .is_none_or(|expected| workspace_fingerprint(workspace).as_ref() == Some(expected))
    }

    fn path(dir: &Path, session: SessionId) -> PathBuf {
        dir.join(format!("rollout-{}.meta.json", session.as_uuid()))
    }

    /// Write this metadata beside the rollout.
    pub async fn write(&self, dir: &Path, session: SessionId) -> std::io::Result<()> {
        let parsed = SessionId::parse_str(&self.session_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        if parsed != session {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "metadata session id does not match its file name",
            ));
        }
        if !metadata_fields_are_bounded(self) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "metadata field exceeds its safety cap",
            ));
        }
        ensure_private_dir(dir).await?;
        let json = serde_json::to_string_pretty(self)?;
        let path = Self::path(dir, session);
        let (tmp, mut file) = create_private_temp(dir, "metadata")?;
        let write_result = async {
            file.write_all(json.as_bytes()).await?;
            #[cfg(unix)]
            set_private_open_file_permissions(&file).await?;
            file.sync_all().await
        }
        .await;
        drop(file);
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        if let Err(error) = tokio::fs::rename(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        #[cfg(unix)]
        sync_directory(dir).await?;
        Ok(())
    }

    /// List all session metadata in `dir`, newest first.
    pub async fn list(dir: &Path) -> Vec<SessionMeta> {
        let mut metas = Vec::new();
        if let Err(error) = ensure_private_dir(dir).await {
            tracing::warn!(
                directory = %dir.display(),
                %error,
                "refusing unsafe session metadata directory"
            );
            return metas;
        }
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return metas;
        };
        let mut scanned = 0usize;
        while scanned < MAX_METADATA_ENTRIES_SCANNED {
            let Ok(Some(entry)) = rd.next_entry().await else {
                break;
            };
            scanned = scanned.saturating_add(1);
            let Some(file_session) = metadata_session_from_filename(&entry.file_name()) else {
                continue;
            };
            let path = entry.path();
            // `symlink_metadata` inspects the directory entry itself rather than its target.
            // Requiring a regular file therefore rejects symlinks on every supported platform.
            let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
                continue;
            };
            if !metadata.file_type().is_file() {
                continue;
            }
            let Ok(text) = read_text_capped(&path, MAX_METADATA_BYTES).await else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<SessionMeta>(&text) else {
                continue;
            };
            let Ok(metadata_session) = SessionId::parse_str(&meta.session_id) else {
                continue;
            };
            if metadata_session == file_session && metadata_fields_are_bounded(&meta) {
                if metas.len() < MAX_METADATA_RESULTS {
                    metas.push(meta);
                } else {
                    let replacement = metas
                        .iter()
                        .enumerate()
                        .min_by(|(_, left), (_, right)| compare_metadata(left, right))
                        .and_then(|(index, oldest)| {
                            compare_metadata(&meta, oldest).is_gt().then_some(index)
                        });
                    if let Some(oldest_index) = replacement {
                        metas[oldest_index] = meta;
                    }
                }
            }
        }
        if scanned == MAX_METADATA_ENTRIES_SCANNED {
            tracing::warn!(
                limit = MAX_METADATA_ENTRIES_SCANNED,
                directory = %dir.display(),
                "session metadata scan reached its safety cap"
            );
        }
        metas.sort_by(|left, right| compare_metadata(right, left));
        metas
    }
}

fn compare_metadata(left: &SessionMeta, right: &SessionMeta) -> std::cmp::Ordering {
    (
        left.created_unix,
        left.created_unix_nanos,
        left.session_id.as_str(),
    )
        .cmp(&(
            right.created_unix,
            right.created_unix_nanos,
            right.session_id.as_str(),
        ))
}

fn metadata_fields_are_bounded(meta: &SessionMeta) -> bool {
    meta.session_id.len() <= 36
        && meta.workspace.to_string_lossy().len() <= MAX_METADATA_WORKSPACE_BYTES
        && meta
            .workspace_identity
            .as_ref()
            .is_none_or(|identity| identity.to_string_lossy().len() <= MAX_METADATA_WORKSPACE_BYTES)
        && meta
            .workspace_fingerprint
            .as_ref()
            .is_none_or(|fingerprint| fingerprint.len() <= MAX_METADATA_WORKSPACE_BYTES)
        && meta.model.len() <= MAX_METADATA_MODEL_BYTES
        && meta.first_prompt.len() <= MAX_METADATA_PROMPT_BYTES
}

fn workspace_fingerprint(workspace: &Path) -> Option<String> {
    fn append_identity(output: &mut String, label: &str, path: &Path) -> Option<()> {
        use std::fmt::Write as _;

        let canonical = std::fs::canonicalize(path).ok()?;
        let metadata = std::fs::metadata(&canonical).ok()?;
        write!(output, "|{label}={}", canonical.display()).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            write!(output, ":{}:{}", metadata.dev(), metadata.ino()).ok()?;
        }
        #[cfg(not(unix))]
        {
            let created = metadata.created().ok()?.duration_since(UNIX_EPOCH).ok()?;
            write!(output, ":{}:{}", created.as_secs(), created.subsec_nanos()).ok()?;
        }
        Some(())
    }

    let mut fingerprint = "v1".to_string();
    append_identity(&mut fingerprint, "workspace", workspace)?;
    if let Some(git) = grokforge_git::Git::discover(workspace) {
        let mut paths = git.metadata_paths().ok()?;
        paths.sort();
        paths.dedup();
        for (index, path) in paths.iter().enumerate() {
            append_identity(&mut fingerprint, &format!("git{index}"), path)?;
        }
    }
    (fingerprint.len() <= MAX_METADATA_WORKSPACE_BYTES).then_some(fingerprint)
}

/// Appends conversation items to a session's JSONL rollout file.
#[derive(Debug)]
pub struct RolloutWriter {
    path: PathBuf,
    file: tokio::fs::File,
    _lock: SessionLock,
}

struct CappedRolloutLine {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl CappedRolloutLine {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            exceeded: false,
        }
    }
}

impl std::io::Write for CappedRolloutLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length >= MAX_ROLLOUT_LINE_BYTES)
        {
            self.exceeded = true;
            return Err(std::io::Error::other("rollout line cap exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RolloutWriter {
    /// Open (creating if needed) the rollout for a session under `dir`.
    pub async fn create(dir: &Path, session: SessionId) -> std::io::Result<Self> {
        Self::open_and_read(dir, session)
            .await
            .map(|(writer, _)| writer)
    }

    /// Exclusively open a session and read/repair its history while holding the same lifetime
    /// lock the returned writer owns. Resume callers must use this instead of a separate
    /// `read_all` + `create` sequence.
    pub async fn open_and_read(
        dir: &Path,
        session: SessionId,
    ) -> std::io::Result<(Self, Vec<ResponseItem>)> {
        ensure_private_dir(dir).await?;
        let path = rollout_path(dir, &session.as_uuid().to_string());
        let lock_path = dir.join(format!("rollout-{}.lock", session.as_uuid()));
        let session_lock = SessionLock::acquire(lock_path).await?;
        repair_truncated_tail(&path).await?;
        let mut history = match Self::read_all_raw(&path).await {
            Ok(history) => history,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let repairs = interrupted_tool_results(&history);
        let file = open_rollout_append(&path)?;
        #[cfg(unix)]
        set_private_open_file_permissions(&file).await?;
        // Syncing unconditionally also covers a concurrently-created-but-safe entry without a
        // path-based existence probe that could itself race.
        #[cfg(unix)]
        sync_directory(dir).await?;
        let mut writer = Self {
            path,
            file,
            _lock: session_lock,
        };
        for repair in repairs {
            writer.append(&repair).await?;
            history.push(repair);
        }
        Ok((writer, history))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one item as a JSON line.
    pub async fn append(&mut self, item: &ResponseItem) -> std::io::Result<()> {
        let mut line = CappedRolloutLine::new();
        if let Err(error) = serde_json::to_writer(&mut line, item) {
            if line.exceeded {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("rollout item exceeds {MAX_ROLLOUT_LINE_BYTES}-byte line cap"),
                ));
            }
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
        line.bytes.push(b'\n');
        self.file.write_all(&line.bytes).await?;
        self.file.sync_data().await
    }

    /// Read a rollout back into memory (for resume). A crash-truncated final line is ignored;
    /// corruption anywhere else is reported rather than silently deleting canonical history.
    pub async fn read_all(path: &Path) -> std::io::Result<Vec<ResponseItem>> {
        let mut items = Self::read_all_raw(path).await?;
        let repairs = interrupted_tool_results(&items);
        items.extend(repairs);
        Ok(items)
    }

    async fn read_all_raw(path: &Path) -> std::io::Result<Vec<ResponseItem>> {
        let file = open_regular_read(path)?;
        let mut reader = BufReader::new(file);
        let mut items = Vec::new();
        let mut visible_bytes = 0usize;
        let mut line_number = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            let complete = read_line_capped(&mut reader, &mut line, MAX_ROLLOUT_LINE_BYTES)
                .await
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::InvalidData {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "rollout line {} exceeds {MAX_ROLLOUT_LINE_BYTES} bytes",
                                line_number.saturating_add(1)
                            ),
                        )
                    } else {
                        error
                    }
                })?;
            let read = line.len();
            if read == 0 {
                break;
            }
            line_number = line_number.saturating_add(1);
            if complete {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice(&line) {
                Ok(ResponseItem::CompactionCheckpoint { history }) => {
                    if history
                        .iter()
                        .any(|item| matches!(item, ResponseItem::CompactionCheckpoint { .. }))
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "nested compaction checkpoint in rollout",
                        ));
                    }
                    items = history;
                    visible_bytes = items.iter().fold(0usize, |total, item| {
                        serde_json::to_vec(item)
                            .map_or(total, |bytes| total.saturating_add(bytes.len()))
                    });
                }
                Ok(item) => {
                    visible_bytes = visible_bytes.saturating_add(line.len());
                    items.push(item);
                }
                Err(_) if !complete => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("malformed rollout line {line_number}: {e}"),
                    ));
                }
            }
            if items.len() > MAX_VISIBLE_ROLLOUT_ITEMS || visible_bytes > MAX_VISIBLE_ROLLOUT_BYTES
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "visible rollout history exceeds in-memory safety cap",
                ));
            }
        }
        Ok(items)
    }
}

#[derive(Debug)]
struct SessionLock {
    _file: std::fs::File,
}

impl SessionLock {
    async fn acquire(path: PathBuf) -> std::io::Result<Self> {
        tokio::task::spawn_blocking(move || Self::acquire_blocking(&path))
            .await
            .map_err(|error| std::io::Error::other(format!("session lock task failed: {error}")))?
    }

    fn acquire_blocking(path: &Path) -> std::io::Result<Self> {
        let file = open_lock_file(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs4::TryLockError::WouldBlock) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "session is already open in another process",
            )),
            Err(fs4::TryLockError::Error(error)) => Err(error),
        }
    }
}

pub(crate) fn interrupted_tool_results(history: &[ResponseItem]) -> Vec<ResponseItem> {
    let mut outstanding = Vec::<grokforge_protocol::ToolCallId>::new();
    for item in history {
        let call_id = match item {
            ResponseItem::ToolCall { id, .. } => Some(id.clone()),
            ResponseItem::ProviderOutput { item }
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call") =>
            {
                item.get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(grokforge_protocol::ToolCallId::from_raw)
            }
            ResponseItem::ToolResult { id, .. } => {
                outstanding.retain(|call| call.as_str() != id.as_str());
                None
            }
            _ => None,
        };
        if let Some(call_id) = call_id
            && !outstanding
                .iter()
                .any(|existing| existing.as_str() == call_id.as_str())
        {
            outstanding.push(call_id);
        }
    }
    outstanding
        .into_iter()
        .map(|id| ResponseItem::ToolResult {
            id,
            content: "interrupted before result recorded; inspect workspace".to_string(),
            is_error: true,
            redactions: 0,
        })
        .collect()
}

/// Read one newline-delimited record without ever growing `line` beyond `cap` bytes.
///
/// The returned boolean is true when the record ended in a newline and false only for a final
/// unterminated record at EOF.
async fn read_line_capped<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(false);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            if line.len().saturating_add(consumed) > cap {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds size cap",
                ));
            }
            line.extend_from_slice(&available[..consumed]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if complete {
            return Ok(true);
        }
    }
}

fn metadata_session_from_filename(name: &std::ffi::OsStr) -> Option<SessionId> {
    let name = name.to_str()?;
    let raw = name.strip_prefix("rollout-")?.strip_suffix(".meta.json")?;
    let session = SessionId::parse_str(raw).ok()?;
    (session.as_uuid().to_string() == raw).then_some(session)
}

async fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || ensure_private_dir_blocking(&dir))
        .await
        .map_err(|error| std::io::Error::other(format!("private directory task failed: {error}")))?
}

fn ensure_private_dir_blocking(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

        // `create_dir_all` uses 0777 before a follow-up chmod. Build every missing component with
        // 0700 atomically so a permissive umask never creates a disclosure window.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(dir)?;
        let descriptor = open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let directory = std::fs::File::from(descriptor);
        let metadata = directory.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session storage directory is not owned by the current user",
            ));
        }
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
        if !std::fs::symlink_metadata(dir)?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session storage path is not a directory",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn set_private_open_file_permissions(file: &tokio::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
}

async fn repair_truncated_tail(path: &Path) -> std::io::Result<()> {
    let mut file = match open_regular_read_write(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let length = file.metadata().await?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::End(-1)).await?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let tail_len = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(MAX_ROLLOUT_LINE_BYTES + 1);
    let tail_offset = length.saturating_sub(tail_len as u64);
    file.seek(std::io::SeekFrom::Start(tail_offset)).await?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail).await?;
    let relative_start = tail
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |position| position + 1);
    if relative_start == 0 && (tail_offset > 0 || length > MAX_ROLLOUT_LINE_BYTES as u64) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unterminated rollout line exceeds safety cap",
        ));
    }
    let tail_start = tail_offset.saturating_add(relative_start as u64);
    if tail.len().saturating_sub(relative_start) >= MAX_ROLLOUT_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unterminated rollout line exceeds safety cap",
        ));
    }
    let valid = serde_json::from_slice::<ResponseItem>(&tail[relative_start..]).is_ok();
    if valid {
        file.seek(std::io::SeekFrom::End(0)).await?;
        file.write_all(b"\n").await?;
    } else {
        file.set_len(tail_start as u64).await?;
    }
    file.sync_data().await
}

#[cfg(unix)]
async fn sync_directory(dir: &Path) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all())
        .await
        .map_err(|error| std::io::Error::other(format!("directory sync task failed: {error}")))?
}

async fn read_text_capped(path: &Path, cap: usize) -> std::io::Result<String> {
    let file = open_regular_read(path)?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes).await?;
    if bytes.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds size cap",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(unix)]
fn validate_single_link_regular(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session store entry is not a regular file",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session store entry has multiple hard links",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session store entry is not owned by the current user",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_single_link_regular(file: &std::fs::File) -> std::io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session store entry is not a regular file",
        ))
    }
}

#[cfg(unix)]
fn open_regular_unix(
    path: &Path,
    flags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> std::io::Result<std::fs::File> {
    let descriptor = rustix::fs::open(
        path,
        flags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        mode,
    )
    .map_err(std::io::Error::from)?;
    let file = std::fs::File::from(descriptor);
    validate_single_link_regular(&file)?;
    Ok(file)
}

fn open_regular_read(path: &Path) -> std::io::Result<tokio::fs::File> {
    #[cfg(unix)]
    let file = open_regular_unix(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    #[cfg(not(unix))]
    let file = {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing symlinked session store entry",
            ));
        }
        let file = std::fs::File::open(path)?;
        validate_single_link_regular(&file)?;
        file
    };
    Ok(tokio::fs::File::from_std(file))
}

fn open_regular_read_write(path: &Path) -> std::io::Result<tokio::fs::File> {
    #[cfg(unix)]
    let file = open_regular_unix(
        path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    #[cfg(not(unix))]
    let file = {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing symlinked session store entry",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        validate_single_link_regular(&file)?;
        file
    };
    Ok(tokio::fs::File::from_std(file))
}

fn open_rollout_append(path: &Path) -> std::io::Result<tokio::fs::File> {
    #[cfg(unix)]
    let file = open_regular_unix(
        path,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::APPEND
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::from_bits_truncate(0o600),
    )?;
    #[cfg(not(unix))]
    let file = {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing symlinked session store entry",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        validate_single_link_regular(&file)?;
        file
    };
    Ok(tokio::fs::File::from_std(file))
}

fn open_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        open_regular_unix(
            path,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::from_bits_truncate(0o600),
        )
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing symlinked session lock",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        validate_single_link_regular(&file)?;
        Ok(file)
    }
}

fn create_private_temp(dir: &Path, label: &str) -> std::io::Result<(PathBuf, tokio::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            ".grokforge-{label}-{}-{sequence}.tmp",
            std::process::id()
        ));
        #[cfg(unix)]
        let opened = open_regular_unix(
            &path,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::from_bits_truncate(0o600),
        );
        #[cfg(not(unix))]
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|file| {
                validate_single_link_regular(&file)?;
                Ok(file)
            });
        match opened {
            Ok(file) => return Ok((path, tokio::fs::File::from_std(file))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique session metadata temp file",
    ))
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

    #[test]
    fn configured_session_directory_is_absolute() {
        assert!(sessions_dir().unwrap().is_absolute());
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

    #[tokio::test]
    async fn second_writer_for_a_live_session_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let first = RolloutWriter::create(dir.path(), id).await.unwrap();
        let error = RolloutWriter::create(dir.path(), id).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        RolloutWriter::create(dir.path(), id).await.unwrap();
    }

    #[tokio::test]
    async fn append_rejects_an_oversized_rollout_line_without_writing_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let mut writer = RolloutWriter::create(dir.path(), id).await.unwrap();
        let error = writer
            .append(&ResponseItem::user("x".repeat(MAX_ROLLOUT_LINE_BYTES)))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(tokio::fs::metadata(writer.path()).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn read_rejects_an_oversized_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut bytes = vec![b'x'; MAX_ROLLOUT_LINE_BYTES];
        bytes.push(b'\n');
        tokio::fs::write(&path, bytes).await.unwrap();
        let error = RolloutWriter::read_all(&path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("rollout line 1 exceeds"));
    }

    #[tokio::test]
    async fn malformed_middle_line_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let first = serde_json::to_string(&ResponseItem::user("ok")).unwrap();
        let last = serde_json::to_string(&ResponseItem::assistant("lost")).unwrap();
        tokio::fs::write(&path, format!("{first}\nnot-json\n{last}\n"))
            .await
            .unwrap();
        let error = RolloutWriter::read_all(&path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn create_repairs_crash_truncated_tail_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = rollout_path(dir.path(), &id.as_uuid().to_string());
        let first = serde_json::to_string(&ResponseItem::user("ok")).unwrap();
        tokio::fs::write(&path, format!("{first}\n{{\"kind\":\"assistant"))
            .await
            .unwrap();
        let mut writer = RolloutWriter::create(dir.path(), id).await.unwrap();
        writer
            .append(&ResponseItem::assistant("continued"))
            .await
            .unwrap();
        let items = RolloutWriter::read_all(&path).await.unwrap();
        assert_eq!(
            items,
            vec![
                ResponseItem::user("ok"),
                ResponseItem::assistant("continued")
            ]
        );
    }

    #[tokio::test]
    async fn orphaned_tool_call_is_repaired_in_memory_and_persisted_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = rollout_path(dir.path(), &id.as_uuid().to_string());
        let call_id = grokforge_protocol::ToolCallId::from_raw("provider-call");
        let call = ResponseItem::ProviderOutput {
            item: serde_json::json!({
                "type": "function_call",
                "call_id": call_id.as_str(),
                "name": "write_file",
                "arguments": "{}"
            }),
        };
        tokio::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&call).unwrap()),
        )
        .await
        .unwrap();

        let in_memory = RolloutWriter::read_all(&path).await.unwrap();
        assert_eq!(in_memory.len(), 2);
        assert!(matches!(
            &in_memory[1],
            ResponseItem::ToolResult { id, content, is_error: true, .. }
                if id.as_str() == call_id.as_str()
                    && content.contains("interrupted before result recorded")
        ));

        let writer = RolloutWriter::create(dir.path(), id).await.unwrap();
        drop(writer);
        let persisted = RolloutWriter::read_all_raw(&path).await.unwrap();
        assert_eq!(persisted, in_memory);
    }

    #[test]
    fn orphan_repair_handles_parallel_calls_in_call_order() {
        let first = grokforge_protocol::ToolCallId::from_raw("first");
        let second = grokforge_protocol::ToolCallId::from_raw("second");
        let history = vec![
            ResponseItem::ToolCall {
                id: first.clone(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            ResponseItem::ToolCall {
                id: second.clone(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            ResponseItem::ToolResult {
                id: first,
                content: "ok".into(),
                is_error: false,
                redactions: 0,
            },
        ];
        let repairs = interrupted_tool_results(&history);
        assert_eq!(repairs.len(), 1);
        assert!(matches!(
            &repairs[0],
            ResponseItem::ToolResult { id, .. } if id.as_str() == second.as_str()
        ));
    }

    #[tokio::test]
    async fn compaction_checkpoint_replaces_replayed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let mut writer = RolloutWriter::create(dir.path(), id).await.unwrap();
        writer
            .append(&ResponseItem::user("old prefix"))
            .await
            .unwrap();
        let visible = vec![
            ResponseItem::CompactionSummary {
                text: "summary".into(),
                redactions: 0,
            },
            ResponseItem::user("tail"),
        ];
        writer
            .append(&ResponseItem::CompactionCheckpoint {
                history: visible.clone(),
            })
            .await
            .unwrap();
        writer
            .append(&ResponseItem::assistant("new"))
            .await
            .unwrap();
        let mut expected = visible;
        expected.push(ResponseItem::assistant("new"));
        assert_eq!(
            RolloutWriter::read_all(writer.path()).await.unwrap(),
            expected
        );
    }

    #[test]
    fn metadata_redacts_and_sanitizes_first_prompt() {
        let meta = SessionMeta::new(
            SessionId::new(),
            PathBuf::from("/tmp"),
            "m".into(),
            "PASSWORD=very-secret-password-value\nnext line\u{1b}[31m",
        );
        assert!(!meta.first_prompt.contains("very-secret-password-value"));
        assert!(!meta.first_prompt.contains('\n'));
        assert!(!meta.first_prompt.contains('\u{1b}'));
    }

    #[cfg(unix)]
    #[test]
    fn metadata_fingerprint_detects_workspace_replacement_at_same_path() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let meta = SessionMeta::new(SessionId::new(), workspace.clone(), "m".into(), "");
        assert!(meta.fingerprint_matches(&workspace));
        std::fs::rename(&workspace, parent.path().join("old-workspace")).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        assert!(!meta.fingerprint_matches(&workspace));
    }

    #[tokio::test]
    async fn metadata_list_rejects_a_filename_for_a_different_session() {
        let dir = tempfile::tempdir().unwrap();
        let metadata_session = SessionId::new();
        let filename_session = SessionId::new();
        let meta = SessionMeta::new(
            metadata_session,
            PathBuf::from("/tmp"),
            "m".into(),
            "prompt",
        );
        let mismatched_path = SessionMeta::path(dir.path(), filename_session);
        tokio::fs::write(&mismatched_path, serde_json::to_vec(&meta).unwrap())
            .await
            .unwrap();

        assert!(SessionMeta::list(dir.path()).await.is_empty());

        meta.write(dir.path(), metadata_session).await.unwrap();
        let listed = SessionMeta::list(dir.path()).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, meta.session_id);
    }

    #[tokio::test]
    async fn metadata_rejects_oversized_fields() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let mut meta = SessionMeta::new(session, PathBuf::from("/tmp"), "m".into(), "prompt");
        meta.model = "m".repeat(MAX_METADATA_MODEL_BYTES + 1);
        let path = SessionMeta::path(dir.path(), session);
        tokio::fs::write(&path, serde_json::to_vec(&meta).unwrap())
            .await
            .unwrap();
        assert!(SessionMeta::list(dir.path()).await.is_empty());
        assert_eq!(
            meta.write(dir.path(), session).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_list_ignores_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let meta = SessionMeta::new(session, PathBuf::from("/tmp"), "m".into(), "prompt");
        let target = target_dir.path().join("meta.json");
        tokio::fs::write(&target, serde_json::to_vec(&meta).unwrap())
            .await
            .unwrap();
        symlink(&target, SessionMeta::path(dir.path(), session)).unwrap();

        assert!(SessionMeta::list(dir.path()).await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_list_refuses_a_symlinked_session_directory() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions = parent.path().join("sessions");
        symlink(outside.path(), &sessions).unwrap();
        assert!(SessionMeta::list(&sessions).await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preplanted_rollout_symlink_and_hardlink_are_rejected() {
        use std::os::unix::fs::symlink;

        for hard_link in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(outside.path(), "outside\n").unwrap();
            let session = SessionId::new();
            let path = rollout_path(dir.path(), &session.as_uuid().to_string());
            if hard_link {
                std::fs::hard_link(outside.path(), &path).unwrap();
            } else {
                symlink(outside.path(), &path).unwrap();
            }

            assert!(RolloutWriter::create(dir.path(), session).await.is_err());
            assert_eq!(
                std::fs::read_to_string(outside.path()).unwrap(),
                "outside\n"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preplanted_session_lock_aliases_are_rejected() {
        use std::os::unix::fs::symlink;

        for hard_link in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            let session = SessionId::new();
            let lock = dir
                .path()
                .join(format!("rollout-{}.lock", session.as_uuid()));
            if hard_link {
                std::fs::hard_link(outside.path(), &lock).unwrap();
            } else {
                symlink(outside.path(), &lock).unwrap();
            }
            assert!(RolloutWriter::create(dir.path(), session).await.is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_hardlink_is_not_read_or_followed_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let session = SessionId::new();
        let meta = SessionMeta::new(session, PathBuf::from("/tmp"), "m".into(), "prompt");
        std::fs::write(outside.path(), serde_json::to_vec(&meta).unwrap()).unwrap();
        std::fs::hard_link(outside.path(), SessionMeta::path(dir.path(), session)).unwrap();

        assert!(SessionMeta::list(dir.path()).await.is_empty());
        meta.write(dir.path(), session).await.unwrap();
        assert_eq!(
            std::fs::read(outside.path()).unwrap(),
            serde_json::to_vec(&meta).unwrap()
        );
        assert_eq!(SessionMeta::list(dir.path()).await.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_directory_and_rollout_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("sessions");
        let writer = RolloutWriter::create(&dir, SessionId::new()).await.unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(writer.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
