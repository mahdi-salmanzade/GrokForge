//! Explicit host credential/session paths that complement workspace-relative secret globs.

use std::path::{Path, PathBuf};

use crate::ExecError;

const MAX_PRIVACY_PATHS: usize = 512;
const MAX_PRIVACY_SCAN_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrivacyPath {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) lexical_paths: Vec<PathBuf>,
}

/// Session filenames are dynamic, so unlike the explicit cloud/SSH credential files below they
/// cannot be enumerated in advance. GrokForge owns this directory and keeps it searchable; scan
/// it without following symlinks and reject hard-link aliases before applying path rules.
pub(crate) fn validate_session_storage_aliases(cwd: &Path) -> Result<(), ExecError> {
    for session_dir in session_path_candidates(cwd) {
        match std::fs::symlink_metadata(&session_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                validate_privacy_tree(&session_dir)?;
            }
            Ok(_) => {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "GrokForge session path {} is not a physical directory",
                    session_dir.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(privacy_inspection_error(&session_dir, &error)),
        }
    }
    Ok(())
}

/// Recursively validate a physical path that will be hidden by a privacy rule. A directory mask
/// only hides names below that directory: a hard link or symlink can expose the same private
/// content elsewhere. Reject those aliases; a single-name special entry is safe because the
/// retained parent mask/deny hides its only path.
pub(crate) fn validate_privacy_tree(root: &Path) -> Result<(), ExecError> {
    let mut pending = vec![root.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(path) = pending.pop() {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_PRIVACY_SCAN_ENTRIES {
            return Err(ExecError::UnsupportedPolicy(format!(
                "privacy scan exceeded {MAX_PRIVACY_SCAN_ENTRIES} entries below {}",
                root.display()
            )));
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| privacy_inspection_error(&path, &error))?;
        if metadata.file_type().is_symlink() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "privacy tree contains symlink {} and cannot be masked safely",
                path.display()
            )));
        }
        if metadata.is_dir() {
            let entries = std::fs::read_dir(&path)
                .map_err(|error| privacy_inspection_error(&path, &error))?;
            for entry in entries {
                pending.push(
                    entry
                        .map_err(|error| privacy_inspection_error(&path, &error))?
                        .path(),
                );
            }
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() > 1 {
                return Err(ExecError::UnsupportedPolicy(format!(
                    "privacy entry {} has multiple hard links and cannot be denied by path",
                    path.display()
                )));
            }
        }
        // A single-name socket/FIFO/device below a retained parent directory is hidden by the
        // parent mask/deny just like a regular file. Unix special entries can be hard-linked, so
        // the nlink check above remains mandatory before accepting them.
        #[cfg(not(unix))]
        if !metadata.is_file() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "privacy tree contains non-regular entry {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Resolve and validate privacy or discovered unreadable paths. Missing candidates are skipped;
/// inspection failures and alias-bearing physical targets fail closed.
pub(crate) fn prepare_privacy_candidates(
    candidates: Vec<PathBuf>,
    cwd: &Path,
    workspace_roots: &[PathBuf],
) -> Result<Vec<PrivacyPath>, ExecError> {
    let mut physical_roots: Vec<PathBuf> = workspace_roots
        .iter()
        .map(|path| canonical_path_allow_missing(path))
        .collect();
    physical_roots.push(canonical_path_allow_missing(cwd));
    physical_roots.sort();
    physical_roots.dedup();

    let mut prepared = Vec::new();
    for candidate in candidates {
        let lexical_path = candidate.clone();
        let metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(privacy_inspection_error(&candidate, &error)),
        };
        let path = std::fs::canonicalize(&candidate)
            .map_err(|error| privacy_inspection_error(&candidate, &error))?;
        if path == Path::new("/") {
            return Err(ExecError::UnsupportedPolicy(
                "refusing to hide the filesystem root as a privacy path".to_string(),
            ));
        }
        let target_metadata = if metadata.file_type().is_symlink() {
            std::fs::metadata(&path).map_err(|error| privacy_inspection_error(&path, &error))?
        } else {
            metadata
        };
        if !target_metadata.is_file() && !target_metadata.is_dir() {
            return Err(ExecError::UnsupportedPolicy(format!(
                "privacy path {} is not a regular file or directory",
                candidate.display()
            )));
        }
        if target_metadata.is_dir()
            && physical_roots
                .iter()
                .any(|root| root == &path || root.starts_with(&path))
        {
            return Err(ExecError::UnsupportedPolicy(format!(
                "workspace path is nested below privacy directory {}; refusing conflicting rules",
                candidate.display()
            )));
        }
        prepared.push(PrivacyPath {
            path,
            is_dir: target_metadata.is_dir(),
            lexical_paths: vec![lexical_path],
        });
    }
    prepared.sort_by(|left, right| {
        left.path
            .components()
            .count()
            .cmp(&right.path.components().count())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut merged: Vec<PrivacyPath> = Vec::new();
    for mut path in prepared {
        if let Some(existing) = merged.last_mut()
            && existing.path == path.path
        {
            existing.lexical_paths.append(&mut path.lexical_paths);
            existing.lexical_paths.sort();
            existing.lexical_paths.dedup();
            continue;
        }
        merged.push(path);
    }
    let mut reduced: Vec<PrivacyPath> = Vec::new();
    for path in merged {
        if reduced
            .iter()
            .any(|parent| parent.is_dir && path.path.starts_with(&parent.path))
        {
            continue;
        }
        reduced.push(path);
    }
    if reduced.len() > MAX_PRIVACY_PATHS {
        return Err(ExecError::UnsupportedPolicy(format!(
            "privacy policy requires more than {MAX_PRIVACY_PATHS} paths"
        )));
    }
    // Validate only the reduced physical roots. A retained parent directory covers and scans
    // every descendant candidate, avoiding repeated walks of credential trees.
    for path in &reduced {
        validate_privacy_tree(&path.path)?;
    }
    Ok(reduced)
}

fn privacy_inspection_error(path: &Path, error: &std::io::Error) -> ExecError {
    ExecError::UnsupportedPolicy(format!(
        "could not inspect privacy path {}: {error}",
        path.display()
    ))
}

/// Bounded, explicit host paths that commonly contain reusable credentials. Whole credential
/// directories are preferred where their contents have variable names.
pub(crate) fn privacy_path_candidates(cwd: &Path) -> Vec<PathBuf> {
    let base = directories::BaseDirs::new();
    let home = absolute_env_path("HOME")
        .or_else(|| base.as_ref().map(|dirs| dirs.home_dir().to_path_buf()));
    let config_home = absolute_env_path("XDG_CONFIG_HOME")
        .or_else(|| base.as_ref().map(|dirs| dirs.config_dir().to_path_buf()));
    let data_home = absolute_env_path("XDG_DATA_HOME")
        .or_else(|| base.as_ref().map(|dirs| dirs.data_dir().to_path_buf()));
    let mut paths = known_privacy_paths(
        home.as_deref(),
        config_home.as_deref(),
        data_home.as_deref(),
        None,
    );
    paths.extend(session_path_candidates(cwd));
    // Some tools ignore XDG_CONFIG_HOME. Cover the conventional location as well when XDG was
    // redirected, without broadening the mask to all of `~/.config`.
    if let Some(default_config) = home.as_ref().map(|path| path.join(".config"))
        && config_home.as_ref() != Some(&default_config)
    {
        paths.extend(config_privacy_paths(&default_config));
    }

    for variable in [
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_CONFIG_FILE",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "NETRC",
        "NPM_CONFIG_USERCONFIG",
        "PIP_CONFIG_FILE",
        "GIT_CONFIG_GLOBAL",
        "GNUPGHOME",
        "DOCKER_CONFIG",
        "AZURE_CONFIG_DIR",
    ] {
        if let Some(path) = env_path(variable, cwd) {
            paths.push(path);
        }
    }
    if let Some(paths_var) = std::env::var_os("KUBECONFIG") {
        paths.extend(std::env::split_paths(&paths_var).map(|path| absolute_path(&path, cwd)));
    }
    if let Some(cargo_home) = env_path("CARGO_HOME", cwd) {
        paths.push(cargo_home.join("credentials"));
        paths.push(cargo_home.join("credentials.toml"));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn known_privacy_paths(
    home: Option<&Path>,
    config_home: Option<&Path>,
    data_home: Option<&Path>,
    session_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home {
        for relative in [
            ".ssh",
            ".ssh/id_rsa",
            ".ssh/id_ed25519",
            ".ssh/id_ecdsa",
            ".ssh/id_dsa",
            ".ssh/config",
            ".aws",
            ".aws/credentials",
            ".aws/config",
            ".gnupg",
            ".gnupg/secring.gpg",
            ".docker",
            ".docker/config.json",
            ".kube",
            ".kube/config",
            ".azure",
            ".azure/accessTokens.json",
            ".azure/azureProfile.json",
            ".azure/msal_token_cache.json",
            ".password-store",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".git-credentials",
            ".cargo/credentials",
            ".cargo/credentials.toml",
            ".gem/credentials",
            ".terraform.d/credentials.tfrc.json",
            ".composer/auth.json",
        ] {
            paths.push(home.join(relative));
        }
    }
    if let Some(config) = config_home {
        paths.extend(config_privacy_paths(config));
    }
    if let Some(data) = data_home {
        paths.push(data.join("keyrings"));
        paths.push(data.join("grokforge/sessions"));
    }
    if let Some(session_dir) = session_dir {
        paths.push(session_dir.to_path_buf());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn config_privacy_paths(config: &Path) -> Vec<PathBuf> {
    [
        "gcloud",
        "gcloud/application_default_credentials.json",
        "gcloud/credentials.db",
        "gcloud/access_tokens.db",
        "gh",
        "gh/hosts.yml",
        "glab-cli",
        "glab-cli/config.yml",
        "docker",
        "docker/config.json",
        "op",
        "containers/auth.json",
        "pip/pip.conf",
        "pypoetry/auth.toml",
        "composer/auth.json",
        "sops/age/keys.txt",
        "rclone/rclone.conf",
        "npm/npmrc",
        "git/credentials",
    ]
    .into_iter()
    .map(|relative| config.join(relative))
    .collect()
}

fn session_path_candidates(cwd: &Path) -> Vec<PathBuf> {
    directories::ProjectDirs::from("dev", "grokforge", "grokforge").map_or_else(
        || vec![cwd.join(".grokforge/sessions")],
        |dirs| vec![dirs.data_dir().join("sessions")],
    )
}

fn absolute_env_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn env_path(variable: &str, cwd: &Path) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .map(|path| absolute_path(&path, cwd))
}

fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn known_paths_cover_sessions_and_common_credentials() {
        let home = Path::new("/home/alice");
        let config = Path::new("/xdg/config");
        let data = Path::new("/xdg/data");
        let session = Path::new("/custom/grokforge/sessions");
        let paths = known_privacy_paths(Some(home), Some(config), Some(data), Some(session));
        for expected in [
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".netrc"),
            config.join("gcloud"),
            config.join("containers/auth.json"),
            data.join("grokforge/sessions"),
            session.to_path_buf(),
        ] {
            assert!(paths.contains(&expected), "missing {}", expected.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn explicit_privacy_hardlink_fails_closed() {
        let dir = tempfile::tempdir().expect("home");
        let target = dir.path().join("target");
        let credential = dir.path().join("credential");
        std::fs::write(&target, "secret").expect("target");
        std::fs::hard_link(&target, &credential).expect("hard link");
        let workspace = tempfile::tempdir().expect("workspace");
        assert!(matches!(
            prepare_privacy_candidates(
                vec![credential],
                workspace.path(),
                &[workspace.path().to_path_buf()]
            ),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn aws_credential_hardlinked_into_workspace_fails_closed() {
        let home = tempfile::tempdir().expect("home");
        let aws = home.path().join(".aws");
        std::fs::create_dir(&aws).expect("aws dir");
        let credential = aws.join("credentials");
        std::fs::write(&credential, "secret").expect("credential");
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::hard_link(&credential, workspace.path().join("aws-alias"))
            .expect("workspace alias");
        let candidates = known_privacy_paths(Some(home.path()), None, None, None);
        assert!(matches!(
            prepare_privacy_candidates(candidates, workspace.path(), &[]),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("hard links")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn session_file_hardlink_fails_closed() {
        let sessions = tempfile::tempdir().expect("sessions");
        let rollout = sessions.path().join("rollout.jsonl");
        std::fs::write(&rollout, "private").expect("rollout");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::hard_link(&rollout, outside.path().join("rollout-alias")).expect("rollout alias");
        assert!(matches!(
            validate_privacy_tree(sessions.path()),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_enumerated_nested_privacy_hardlink_fails_closed() {
        let home = tempfile::tempdir().expect("home");
        let private = home.path().join(".ssh");
        let nested = private.join("keys/custom-provider.key");
        std::fs::create_dir_all(nested.parent().expect("key parent")).expect("private tree");
        std::fs::write(&nested, "private").expect("private key");
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::hard_link(&nested, workspace.path().join("key-alias")).expect("workspace alias");

        assert!(matches!(
            prepare_privacy_candidates(
                vec![private],
                workspace.path(),
                &[workspace.path().to_path_buf()]
            ),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("multiple hard links")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn nested_privacy_symlink_target_fails_closed() {
        let home = tempfile::tempdir().expect("home");
        let private = home.path().join(".ssh");
        std::fs::create_dir(&private).expect("private tree");
        let target = home.path().join("physical-private-key");
        std::fs::write(&target, "private").expect("target");
        std::os::unix::fs::symlink(&target, private.join("custom-provider.key"))
            .expect("private symlink");
        let workspace = tempfile::tempdir().expect("workspace");

        assert!(matches!(
            prepare_privacy_candidates(
                vec![private],
                workspace.path(),
                &[workspace.path().to_path_buf()]
            ),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("symlink")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn single_name_agent_socket_below_a_masked_directory_is_allowed() {
        use std::os::unix::net::UnixListener;

        let home = tempfile::tempdir().expect("home");
        let private = home.path().join(".gnupg");
        std::fs::create_dir(&private).expect("private tree");
        let _listener = UnixListener::bind(private.join("S.gpg-agent")).expect("agent socket");
        let workspace = tempfile::tempdir().expect("workspace");

        let paths = prepare_privacy_candidates(
            vec![private],
            workspace.path(),
            &[workspace.path().to_path_buf()],
        )
        .expect("parent mask hides a single-name agent socket");
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn workspace_nested_under_private_directory_fails_closed() {
        let home = tempfile::tempdir().expect("home");
        let private = home.path().join(".ssh");
        let workspace = private.join("project");
        std::fs::create_dir_all(&workspace).expect("workspace");
        assert!(matches!(
            prepare_privacy_candidates(
                vec![private],
                &workspace,
                std::slice::from_ref(&workspace)
            ),
            Err(ExecError::UnsupportedPolicy(message)) if message.contains("nested below privacy")
        ));
    }
}
