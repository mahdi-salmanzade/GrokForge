//! Race-resistant path handling for host-process file tools.
//!
//! Shell commands are confined by the sandbox backend, but `write_file` and `edit` execute in
//! the trusted host process.  A lexical `starts_with` check is not enough there: `..` and a
//! symlinked parent can redirect a write after policy evaluation.  On Unix we walk from an
//! already-open writable root with `openat`/`mkdirat` and `O_NOFOLLOW`, so every component is
//! resolved relative to a stable directory descriptor.  Other platforms use a conservative
//! no-symlink validation fallback.

use std::io::{Read, Write};
#[cfg(not(unix))]
use std::io::{Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use grokforge_protocol::{SandboxMode, SandboxPolicy};

/// Largest file a built-in write/edit accepts. This prevents a single tool call from exhausting
/// memory while remaining generous for source files.
pub(crate) const MAX_MUTATING_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PathSafetyError {
    #[error("path is outside the permitted boundary or is protected")]
    Denied,
    #[error("path must name a file below a writable root")]
    InvalidTarget,
    #[error("refusing to follow a symbolic link in a host-process write")]
    Symlink,
    #[error("refusing to access a multiply-linked file in the host process")]
    HardLink,
    #[error("target changed concurrently; refusing to overwrite a newer file")]
    ConcurrentModification,
    #[error("file is larger than the {MAX_MUTATING_FILE_BYTES}-byte tool limit")]
    TooLarge,
    #[error("target is not a regular UTF-8 text file")]
    NotText,
    #[error("`old_string` was not found")]
    OldStringNotFound,
    #[error("`old_string` occurs {0} times; make it unique or pass replace_all")]
    OldStringNotUnique(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Normalize `.` and `..` without touching the filesystem.
#[must_use]
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop a root/prefix. For a relative path, retain leading `..` so callers
                // cannot accidentally turn an escape into an in-root path.
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else if !out.has_root() {
                    out.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        std::env::current_dir().map_or_else(|_| normalize(path), |cwd| normalize(&cwd.join(path)))
    }
}

fn writable_root(policy: &SandboxPolicy, target: &Path) -> Option<PathBuf> {
    policy
        .writable_roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(absolute(root)).ok())
        .filter(|root| target.starts_with(root))
        .max_by_key(|root| root.components().count())
}

/// Resolve every existing path component, while retaining a missing suffix.  Approval boundary
/// checks use this as well as the file tools so a lexical in-bound path cannot gain a broader
/// grant through a symlinked parent.
pub(crate) fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut cursor = absolute(path);
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(normalize(&canonical));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name().map(std::ffi::OsStr::to_os_string) else {
                    return Err(error);
                };
                missing.push(name);
                if !cursor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn canonical_workspace_target(
    workspace: &Path,
    target: &Path,
) -> Result<(PathBuf, PathBuf), PathSafetyError> {
    let root = std::fs::canonicalize(absolute(workspace))?;
    let target = std::fs::canonicalize(absolute(target))?;
    if !target.starts_with(&root) {
        return Err(PathSafetyError::Denied);
    }
    Ok((root, target))
}

#[cfg(unix)]
fn open_workspace_target(
    workspace: &Path,
    target: &Path,
    directory: bool,
) -> Result<std::fs::File, PathSafetyError> {
    use rustix::fs::{Mode, OFlags, openat};
    use rustix::io::Errno;

    // `target` is the already-canonical path whose readable-root and secret-glob checks the
    // caller performed. Do not canonicalize it again: doing so would let a checked ordinary file
    // be replaced by a symlink to a secret between policy evaluation and this open.
    let root = std::fs::canonicalize(absolute(workspace))?;
    let target = absolute(target);
    if !target.starts_with(&root) {
        return Err(PathSafetyError::Denied);
    }
    let relative = target
        .strip_prefix(&root)
        .map_err(|_| PathSafetyError::Denied)?;
    if relative.as_os_str().is_empty() {
        if directory {
            return Ok(std::fs::File::open(root)?);
        }
        return Err(PathSafetyError::InvalidTarget);
    }
    let (parent, name) = open_parent(&root, relative, false)?;
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let fd = match openat(&parent, &name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::LOOP) => return Err(PathSafetyError::Symlink),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    Ok(fd.into())
}

#[cfg(not(unix))]
fn open_workspace_target(
    workspace: &Path,
    target: &Path,
    directory: bool,
) -> Result<std::fs::File, PathSafetyError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let mut cursor = absolute(workspace);
        let relative = absolute(target)
            .strip_prefix(&cursor)
            .map(Path::to_path_buf)
            .ok();
        if let Some(relative) = relative {
            for component in relative.components() {
                cursor.push(component.as_os_str());
                if std::fs::symlink_metadata(&cursor)
                    .is_ok_and(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                {
                    return Err(PathSafetyError::Symlink);
                }
            }
        }
    }
    let (_root, target) = canonical_workspace_target(workspace, target)?;
    let file = std::fs::File::open(target)?;
    if file.metadata()?.is_dir() != directory {
        return Err(PathSafetyError::InvalidTarget);
    }
    Ok(file)
}

pub(crate) fn read_workspace_text(
    workspace: &Path,
    target: &Path,
    max_bytes: usize,
) -> Result<(String, bool), PathSafetyError> {
    if cfg!(not(unix)) {
        return Err(PathSafetyError::Denied);
    }
    let file = open_workspace_target(workspace, target, false)?;
    read_text_file(file, max_bytes)
}

/// Read automatic project context without following any symlink in the workspace-relative path.
/// This is stricter than an explicit `read_file`: silently discovered context must not be
/// redirectable to another in-workspace file between discovery and open.
pub(crate) fn read_workspace_context_text(
    workspace: &Path,
    target: &Path,
    max_bytes: usize,
) -> Result<(String, bool), PathSafetyError> {
    if cfg!(not(unix)) {
        return Err(PathSafetyError::Denied);
    }
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, openat};
        use rustix::io::Errno;

        let lexical_root = absolute(workspace);
        let lexical_target = absolute(target);
        let relative = lexical_target
            .strip_prefix(&lexical_root)
            .map_err(|_| PathSafetyError::Denied)?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(PathSafetyError::InvalidTarget);
        }
        let root = std::fs::canonicalize(&lexical_root)?;
        let (parent, name) = open_parent(&root, relative, false)?;
        match openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => std::fs::File::from(fd),
            Err(Errno::LOOP) => return Err(PathSafetyError::Symlink),
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    };
    #[cfg(not(unix))]
    let file = {
        if std::fs::symlink_metadata(target)?.file_type().is_symlink() {
            return Err(PathSafetyError::Symlink);
        }
        open_workspace_target(workspace, target, false)?
    };
    read_text_file(file, max_bytes)
}

fn read_text_file(
    mut file: std::fs::File,
    max_bytes: usize,
) -> Result<(String, bool), PathSafetyError> {
    if !file.metadata()?.is_file() {
        return Err(PathSafetyError::NotText);
    }
    reject_hard_link(&file)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    (&mut file)
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_string(),
        Err(error) if truncated && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
            String::from_utf8(bytes).map_err(|_| PathSafetyError::NotText)?
        }
        Err(_) => return Err(PathSafetyError::NotText),
    };
    Ok((text, truncated))
}

pub(crate) fn list_workspace_dir(
    workspace: &Path,
    target: &Path,
    max_entries: usize,
) -> Result<(Vec<(String, bool)>, bool), PathSafetyError> {
    if cfg!(not(unix)) {
        return Err(PathSafetyError::Denied);
    }
    let directory = open_workspace_target(workspace, target, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let mut reader = rustix::fs::Dir::read_from(&directory).map_err(std::io::Error::from)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in &mut reader {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
            if matches!(name.to_str(), Some("." | "..")) {
                continue;
            }
            entries.push((
                name.to_string_lossy().into_owned(),
                entry.file_type() == rustix::fs::FileType::Directory,
            ));
            if entries.len() >= max_entries {
                truncated = true;
                break;
            }
        }
        Ok((entries, truncated))
    }
    #[cfg(not(unix))]
    {
        let target = canonical_workspace_target(workspace, target)?.1;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in std::fs::read_dir(target)? {
            let entry = entry?;
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.file_type().is_ok_and(|kind| kind.is_dir()),
            ));
            if entries.len() >= max_entries {
                truncated = true;
                break;
            }
        }
        Ok((entries, truncated))
    }
}

fn prepare_bound(
    policy: &SandboxPolicy,
    target: &Path,
    approved_target: Option<&Path>,
) -> Result<(PathBuf, PathBuf), PathSafetyError> {
    let lexical_target = absolute(target);
    if policy.mode == SandboxMode::ReadOnly
        || policy
            .protected_paths
            .iter()
            .any(|protected| lexical_target.starts_with(absolute(protected)))
    {
        return Err(PathSafetyError::Denied);
    }
    let target = canonicalize_allow_missing(&lexical_target)?;
    if approved_target.is_some_and(|approved| target != approved) {
        return Err(PathSafetyError::ConcurrentModification);
    }
    if policy.protected_paths.iter().any(|protected| {
        canonicalize_allow_missing(protected).is_ok_and(|protected| target.starts_with(protected))
    }) {
        return Err(PathSafetyError::Denied);
    }
    let root = writable_root(policy, &target).ok_or(PathSafetyError::Denied)?;
    let relative = target
        .strip_prefix(&root)
        .map_err(|_| PathSafetyError::Denied)?
        .to_path_buf();
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(PathSafetyError::InvalidTarget);
    }
    Ok((root, relative))
}

#[cfg(not(unix))]
fn prepare(policy: &SandboxPolicy, target: &Path) -> Result<(PathBuf, PathBuf), PathSafetyError> {
    prepare_bound(policy, target, None)
}

#[cfg(unix)]
fn open_parent(
    root: &Path,
    relative: &Path,
    create: bool,
) -> Result<(std::os::fd::OwnedFd, std::ffi::OsString), PathSafetyError> {
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
    use rustix::io::Errno;

    let mut current = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut components = relative.components().peekable();
    let mut file_name = None;

    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(PathSafetyError::InvalidTarget);
        };
        if components.peek().is_none() {
            file_name = Some(name.to_os_string());
            break;
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&current, name, flags, Mode::empty()) {
            Ok(next) => current = next,
            Err(Errno::NOENT) if create => {
                match mkdirat(&current, name, Mode::from_bits_truncate(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(std::io::Error::from(e).into()),
                }
                current =
                    openat(&current, name, flags, Mode::empty()).map_err(std::io::Error::from)?;
            }
            Err(Errno::LOOP) => return Err(PathSafetyError::Symlink),
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
    }

    Ok((current, file_name.ok_or(PathSafetyError::InvalidTarget)?))
}

#[cfg(not(unix))]
fn open_confined(
    policy: &SandboxPolicy,
    target: &Path,
    create: bool,
    write: bool,
) -> Result<std::fs::File, PathSafetyError> {
    let (root, relative) = prepare(policy, target)?;
    let mut current = root;
    let mut components = relative.components().peekable();
    while let Some(Component::Normal(part)) = components.next() {
        current.push(part);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(PathSafetyError::Symlink);
            }
        } else if create && components.peek().is_some() {
            std::fs::create_dir(&current)?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(write).create(create);
    Ok(options.open(current)?)
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct FileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
}

#[cfg(unix)]
fn file_identity(file: &std::fs::File) -> Result<FileIdentity, PathSafetyError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(PathSafetyError::InvalidTarget);
    }
    if metadata.nlink() > 1 {
        return Err(PathSafetyError::HardLink);
    }
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    })
}

#[cfg(unix)]
fn open_at_if_exists(
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<Option<(std::fs::File, FileIdentity)>, PathSafetyError> {
    use rustix::fs::{Mode, OFlags, openat};
    use rustix::io::Errno;

    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => {
            let file: std::fs::File = fd.into();
            let identity = file_identity(&file)?;
            Ok(Some((file, identity)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(PathSafetyError::Symlink),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

#[cfg(unix)]
fn open_existing_for_edit(
    policy: &SandboxPolicy,
    target: &Path,
    approved_target: Option<&Path>,
) -> Result<(std::fs::File, FileIdentity), PathSafetyError> {
    let (root, relative) = prepare_bound(policy, target, approved_target)?;
    let (parent, name) = open_parent(&root, &relative, false)?;
    open_at_if_exists(&parent, &name)?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).into())
}

#[cfg(unix)]
fn atomic_replace(
    policy: &SandboxPolicy,
    target: &Path,
    approved_target: Option<&Path>,
    expected_identity: Option<FileIdentity>,
    content: &[u8],
) -> Result<(), PathSafetyError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    use rustix::io::Errno;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    let (root, relative) = prepare_bound(policy, target, approved_target)?;
    let (parent, name) = open_parent(&root, &relative, true)?;
    let observed = open_at_if_exists(&parent, &name)?;
    let expected_identity =
        expected_identity.or_else(|| observed.as_ref().map(|(_, identity)| *identity));
    if expected_identity.is_some() && observed.is_none() {
        return Err(PathSafetyError::ConcurrentModification);
    }

    let mode = expected_identity.map(|identity| identity.mode & 0o7777);
    let mut temporary = None;
    for _ in 0..16 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".grokforge-tmp-{}-{sequence}", std::process::id());
        match openat(
            &parent,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o666),
        ) {
            Ok(fd) => {
                temporary = Some((temp_name, std::fs::File::from(fd)));
                break;
            }
            Err(Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
    let (temp_name, mut temp_file) = temporary.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate temp file",
        )
    })?;
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        temp_file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    let result = (|| {
        temp_file.write_all(content)?;
        temp_file.flush()?;
        temp_file.sync_all()?;

        let current = open_at_if_exists(&parent, &name)?;
        match (
            expected_identity,
            current.as_ref().map(|(_, identity)| *identity),
        ) {
            (Some(before), Some(now))
                if before.device == now.device && before.inode == now.inode => {}
            (None, None) => {}
            _ => return Err(PathSafetyError::ConcurrentModification),
        }
        renameat(&parent, temp_name.as_str(), &parent, &name).map_err(std::io::Error::from)?;
        rustix::fs::fsync(&parent).map_err(std::io::Error::from)?;
        Ok::<_, PathSafetyError>(())
    })();
    if result.is_err() {
        let _ = unlinkat(&parent, temp_name.as_str(), AtFlags::empty());
    }
    result
}

pub(crate) fn write_file_bound(
    policy: &SandboxPolicy,
    target: &Path,
    approved_target: Option<&Path>,
    content: &[u8],
) -> Result<(), PathSafetyError> {
    if cfg!(not(unix)) {
        return Err(PathSafetyError::Denied);
    }
    if content.len() > MAX_MUTATING_FILE_BYTES {
        return Err(PathSafetyError::TooLarge);
    }
    #[cfg(unix)]
    {
        atomic_replace(policy, target, approved_target, None, content)
    }
    #[cfg(not(unix))]
    {
        if approved_target.is_some() {
            let _ = prepare_bound(policy, target, approved_target)?;
        }
        let mut file = open_confined(policy, target, true, true)?;
        if !file.metadata()?.is_file() {
            return Err(PathSafetyError::InvalidTarget);
        }
        reject_hard_link(&file)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(content)?;
        file.flush()?;
        Ok(())
    }
}

pub(crate) fn edit_file_bound(
    policy: &SandboxPolicy,
    target: &Path,
    approved_target: Option<&Path>,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<usize, PathSafetyError> {
    if cfg!(not(unix)) {
        return Err(PathSafetyError::Denied);
    }
    if old.is_empty() || new.len() > MAX_MUTATING_FILE_BYTES {
        return Err(PathSafetyError::TooLarge);
    }
    #[cfg(unix)]
    let (mut file, identity) = open_existing_for_edit(policy, target, approved_target)?;
    #[cfg(not(unix))]
    let mut file = {
        if approved_target.is_some() {
            let _ = prepare_bound(policy, target, approved_target)?;
        }
        open_confined(policy, target, false, true)?
    };
    reject_hard_link(&file)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(PathSafetyError::InvalidTarget);
    }
    let size = metadata.len();
    if size > MAX_MUTATING_FILE_BYTES as u64 {
        return Err(PathSafetyError::TooLarge);
    }
    let mut original = String::new();
    (&mut file)
        .take((MAX_MUTATING_FILE_BYTES + 1) as u64)
        .read_to_string(&mut original)?;
    if original.len() > MAX_MUTATING_FILE_BYTES {
        return Err(PathSafetyError::TooLarge);
    }
    let occurrences = original.matches(old).count();
    if occurrences == 0 {
        return Err(PathSafetyError::OldStringNotFound);
    }
    if occurrences > 1 && !replace_all {
        return Err(PathSafetyError::OldStringNotUnique(occurrences));
    }
    let replacements = if replace_all { occurrences } else { 1 };
    let removed = old
        .len()
        .checked_mul(replacements)
        .ok_or(PathSafetyError::TooLarge)?;
    let added = new
        .len()
        .checked_mul(replacements)
        .ok_or(PathSafetyError::TooLarge)?;
    let expected = original
        .len()
        .checked_sub(removed)
        .and_then(|size| size.checked_add(added))
        .ok_or(PathSafetyError::TooLarge)?;
    if expected > MAX_MUTATING_FILE_BYTES {
        return Err(PathSafetyError::TooLarge);
    }
    let updated = if replace_all {
        original.replace(old, new)
    } else {
        original.replacen(old, new, 1)
    };
    if updated.len() > MAX_MUTATING_FILE_BYTES {
        return Err(PathSafetyError::TooLarge);
    }
    #[cfg(unix)]
    atomic_replace(
        policy,
        target,
        approved_target,
        Some(identity),
        updated.as_bytes(),
    )?;
    #[cfg(not(unix))]
    {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(updated.as_bytes())?;
        file.flush()?;
    }
    Ok(occurrences)
}

fn reject_hard_link(file: &std::fs::File) -> Result<(), PathSafetyError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() > 1 {
            return Err(PathSafetyError::HardLink);
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_does_not_hide_parent_escape() {
        assert_eq!(
            normalize(Path::new("/workspace/src/../../outside")),
            PathBuf::from("/outside")
        );
        assert_eq!(
            normalize(Path::new("../../outside")),
            PathBuf::from("../../outside")
        );
    }

    #[cfg(unix)]
    #[test]
    fn approved_physical_write_rejects_parent_retarget() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = workspace.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("file.txt");
        let approved = canonicalize_allow_missing(&target).unwrap();

        std::fs::rename(&parent, workspace.path().join("old-parent")).unwrap();
        symlink(outside.path(), &parent).unwrap();
        let policy = SandboxPolicy::danger_full_access(workspace.path());
        let error = write_file_bound(&policy, &target, Some(&approved), b"escape").unwrap_err();
        assert!(matches!(error, PathSafetyError::ConcurrentModification));
        assert!(!outside.path().join("file.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn checked_read_rejects_retarget_to_secret_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let ordinary = workspace.path().join("ordinary.txt");
        let secret = workspace.path().join(".env");
        std::fs::write(&ordinary, "ordinary").unwrap();
        std::fs::write(&secret, "DO_NOT_DISCLOSE").unwrap();
        let checked = canonical_workspace_target(workspace.path(), &ordinary)
            .unwrap()
            .1;

        std::fs::remove_file(&ordinary).unwrap();
        symlink(&secret, &ordinary).unwrap();

        let error = read_workspace_text(workspace.path(), &checked, 1024).unwrap_err();
        assert!(matches!(
            error,
            PathSafetyError::Symlink | PathSafetyError::Io(_)
        ));
    }

    #[cfg(not(unix))]
    #[test]
    fn descriptor_sensitive_host_operations_fail_closed() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("file.txt");
        std::fs::write(&file, "contents").unwrap();
        let policy = SandboxPolicy::workspace_write(workspace.path());

        assert!(matches!(
            read_workspace_text(workspace.path(), &file, 1024),
            Err(PathSafetyError::Denied)
        ));
        assert!(matches!(
            read_workspace_context_text(workspace.path(), &file, 1024),
            Err(PathSafetyError::Denied)
        ));
        assert!(matches!(
            list_workspace_dir(workspace.path(), workspace.path(), 10),
            Err(PathSafetyError::Denied)
        ));
        assert!(matches!(
            write_file_bound(&policy, &file, None, b"changed"),
            Err(PathSafetyError::Denied)
        ));
    }
}
