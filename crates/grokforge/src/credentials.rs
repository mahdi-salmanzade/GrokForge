//! xAI credential storage — **on the host, never in the OS keychain**.
//!
//! The API key and/or subscription OAuth tokens are kept in a single encrypted file at
//! `~/.grokforge/credentials.enc`. The encryption key is derived from **your password** and a
//! **random salt** (stored in the file) via Argon2id, and the payload is sealed with
//! ChaCha20-Poly1305. GrokForge touches no system secret store.
//!
//! Flow: on first run you set a password, then log in (subscription or API key). On later runs
//! you enter the password to unlock. `XAI_API_KEY` in the environment still overrides everything
//! (for CI), needing no password.

use std::io::{IsTerminal, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use grokforge_xai::oauth::{self, OAuthTokens};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize as _, Zeroizing};

const CREDENTIAL_FILE_VERSION: u8 = 1;
const CREDENTIAL_FILE_MAX_BYTES: usize = 64 * 1024;
const ARGON2_MEMORY_KIB: u32 = 19_456;
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;
const MIN_NEW_PASSWORD_CHARS: usize = 12;

/// Credentials held in the encrypted file. New writes keep exactly one login method active, and
/// ambiguous legacy files containing both are rejected instead of guessing which one to bill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredCreds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthTokens>,
}

impl StoredCreds {
    fn use_api_key(&mut self, api_key: String) {
        self.api_key = Some(api_key);
        self.oauth = None;
    }

    fn use_oauth(&mut self, oauth: OAuthTokens) {
        self.api_key = None;
        self.oauth = Some(oauth);
    }

    fn validate(&self) -> Result<(), String> {
        if self.api_key.is_some() && self.oauth.is_some() {
            return Err(
                "credentials file contains both login methods; GrokForge cannot safely choose which one to bill. Move or delete the file, then sign in again"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// The on-disk envelope: salt + nonce + ciphertext, all base64.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedFile {
    version: u8,
    salt: String,
    nonce: String,
    ciphertext: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Derive a 32-byte key using the parameters fixed by credential-file version 1.
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(key.len()),
    )
    .map_err(|e| e.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

fn encrypt(password: &str, creds: &StoredCreds) -> Result<EncryptedFile, String> {
    creds.validate()?;
    let salt = random(16)?;
    let nonce = random(12)?;
    let mut key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    // The cipher has copied the key into its own state; scrub our derived copy immediately.
    key.zeroize();
    let plaintext = Zeroizing::new(serde_json::to_vec(creds).map_err(|e| e.to_string())?);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| "encryption failed".to_string())?;
    Ok(EncryptedFile {
        version: CREDENTIAL_FILE_VERSION,
        salt: b64(&salt),
        nonce: b64(&nonce),
        ciphertext: b64(&ciphertext),
    })
}

fn decrypt(password: &str, file: &EncryptedFile) -> Result<StoredCreds, String> {
    if file.version != CREDENTIAL_FILE_VERSION {
        return Err(format!(
            "unsupported credentials file version {}",
            file.version
        ));
    }
    let salt = unb64(&file.salt)?;
    let nonce = unb64(&file.nonce)?;
    let ciphertext = unb64(&file.ciphertext)?;
    if salt.len() != 16 || nonce.len() != 12 {
        return Err("credentials file is corrupt (invalid salt or nonce)".to_string());
    }
    let mut key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    key.zeroize();
    // Keep the decrypted bytes in a zeroizing owner so every return path, including a JSON/schema
    // parse error, scrubs the plaintext before releasing its allocation.
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| "incorrect password (or the credentials file is corrupt)".to_string())?,
    );
    let creds: StoredCreds =
        serde_json::from_slice(plaintext.as_slice()).map_err(|e| e.to_string())?;
    creds.validate()?;
    Ok(creds)
}

/// `~/.grokforge/credentials.enc` (override with `GROKFORGE_CREDENTIALS_PATH`, used by tests).
fn creds_path() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("GROKFORGE_CREDENTIALS_PATH") {
        if p.is_empty() {
            return Err("GROKFORGE_CREDENTIALS_PATH must not be empty".to_string());
        }
        return Ok(PathBuf::from(p));
    }
    directories::BaseDirs::new()
        .map(|base| base.home_dir().join(".grokforge").join("credentials.enc"))
        .ok_or_else(|| "could not determine the home directory for credential storage".to_string())
}

/// Whether an encrypted credentials file exists.
#[must_use]
pub fn has_stored_file() -> bool {
    creds_path().is_ok_and(|path| path.exists())
}

fn save_to(path: &Path, password: &str, creds: &StoredCreds) -> Result<(), String> {
    let file = encrypt(password, creds)?;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        create_private_dir(dir)?;
    }
    let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;

    #[cfg(unix)]
    {
        write_secret_file(path, &json)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, json).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Create the credentials directory (and any missing parents) restricted to the owner. On Unix
/// this builds every missing component with mode `0o700` atomically, so a permissive umask never
/// opens a disclosure window on the directory that holds the encrypted credentials.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())
    }
}

#[cfg(unix)]
fn write_secret_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err("credentials path must be a regular file".to_string());
            }
            if metadata.nlink() != 1 {
                return Err("credentials path must not be hard-linked".to_string());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| "credentials path has no file name".to_string())?
        .to_string_lossy();
    let suffix = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random(9)?);
    let temporary = directory.join(format!(".{file_name}.{suffix}.tmp"));

    let result = (|| -> Result<(), String> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        output
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        output.write_all(contents).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn load_from(path: &Path, password: &str) -> Result<StoredCreds, String> {
    let input = open_secret_file(path)?;
    let mut data = Vec::with_capacity(CREDENTIAL_FILE_MAX_BYTES + 1);
    input
        .take(u64::try_from(CREDENTIAL_FILE_MAX_BYTES + 1).map_err(|e| e.to_string())?)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > CREDENTIAL_FILE_MAX_BYTES {
        return Err(format!(
            "credentials file exceeds the {CREDENTIAL_FILE_MAX_BYTES}-byte safety limit"
        ));
    }
    let file: EncryptedFile = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    decrypt(password, &file)
}

#[cfg(unix)]
fn open_secret_file(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let metadata = input.metadata().map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("credentials path must be a regular file".to_string());
    }
    if metadata.nlink() != 1 {
        return Err("credentials path must not be hard-linked".to_string());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(
            "credentials file permissions are too broad; run `chmod 600` on it".to_string(),
        );
    }
    Ok(input)
}

#[cfg(not(unix))]
fn open_secret_file(path: &Path) -> Result<std::fs::File, String> {
    std::fs::File::open(path).map_err(|e| e.to_string())
}

// ---------- prompts ----------

fn prompt_password(prompt: &str) -> Option<Zeroizing<String>> {
    rpassword::prompt_password(prompt)
        .ok()
        .filter(|p| !p.is_empty())
        .map(Zeroizing::new)
}

fn new_password_is_long_enough(password: &str) -> bool {
    password.chars().count() >= MIN_NEW_PASSWORD_CHARS
}

fn prompt_new_password() -> Option<Zeroizing<String>> {
    eprintln!("Create a password to encrypt your credentials on this machine.");
    eprintln!(
        "Use at least {MIN_NEW_PASSWORD_CHARS} characters. No recovery — if you forget it, you'll sign in again."
    );
    let first = prompt_password("New password: ")?;
    if !new_password_is_long_enough(&first) {
        eprintln!("password must be at least {MIN_NEW_PASSWORD_CHARS} characters.");
        return None;
    }
    let confirm = prompt_password("Confirm password: ")?;
    if first.as_str() != confirm.as_str() {
        eprintln!("passwords did not match.");
        return None;
    }
    Some(first)
}

fn prompt_api_key() -> Option<String> {
    rpassword::prompt_password("xAI API key (input hidden): ")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn terminal_path(path: &Path) -> String {
    crate::sanitize_terminal_line(&path.to_string_lossy())
}

// ---------- resolution ----------

/// Resolve a bearer token: `XAI_API_KEY` env → password-unlock the encrypted file → (first run,
/// interactive) onboarding. Returns `None` (after printing guidance) when nothing is available.
pub async fn resolve(allow_prompt: bool) -> Option<String> {
    if let Ok(key) = std::env::var("XAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }

    let path = match creds_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate credential storage: {error}");
            return None;
        }
    };
    if path.exists() {
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "credentials are password-encrypted; run in a terminal to unlock, or set XAI_API_KEY."
            );
            return None;
        }
        let password = prompt_password("Enter your GrokForge password: ")?;
        let creds = match load_from(&path, &password) {
            Ok(creds) => creds,
            Err(e) => {
                eprintln!("{e}");
                return None;
            }
        };
        return bearer_from(&path, creds, &password).await;
    }

    if allow_prompt && std::io::stdin().is_terminal() {
        return onboard(&path).await;
    }
    eprintln!(
        "No credentials yet. Run `grokforge` (or `grokforge login`) to set up, or set XAI_API_KEY."
    );
    None
}

/// First-run onboarding: set a password, then choose a login method, then save.
async fn onboard(path: &std::path::Path) -> Option<String> {
    eprintln!("\nWelcome to GrokForge 👋  Let's get you set up.");
    let password = prompt_new_password()?;

    eprintln!("\nHow do you want to connect?");
    eprintln!("  [1] Sign in with your Grok subscription (SuperGrok / X Premium+) — no API key");
    eprintln!("  [2] Paste an xAI API key (console.x.ai)");
    eprint!("Choice [1/2] (default 1): ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }

    let mut creds = StoredCreds::default();
    let token = if line.trim() == "2" {
        let key = prompt_api_key()?;
        creds.use_api_key(key.clone());
        key
    } else {
        eprintln!("Note: subscription API access currently requires the SuperGrok Heavy tier.\n");
        match oauth::login().await {
            Ok(tokens) => {
                let access = tokens.access_token.clone();
                creds.use_oauth(tokens);
                access
            }
            Err(e) => {
                eprintln!(
                    "sign-in failed: {}",
                    crate::sanitize_terminal_line(&e.to_string())
                );
                return None;
            }
        }
    };

    match save_to(path, &password, &creds) {
        Ok(()) => eprintln!(
            "✓ credentials encrypted and saved to {}",
            terminal_path(path)
        ),
        Err(e) => {
            eprintln!("warning: couldn't save credentials ({e}); using for this session only");
        }
    }
    Some(token)
}

/// Turn stored credentials into a usable bearer token, refreshing an expired OAuth token (and
/// re-saving with the same password) when needed.
async fn bearer_from(
    path: &std::path::Path,
    mut creds: StoredCreds,
    password: &str,
) -> Option<String> {
    if let Some(key) = &creds.api_key
        && !key.trim().is_empty()
    {
        return Some(key.clone());
    }
    if let Some(tokens) = creds.oauth.clone() {
        if tokens.is_valid() {
            return Some(tokens.access_token);
        }
        if let Some(refresh) = tokens.refresh_token.clone()
            && let Ok(mut fresh) = oauth::refresh(&refresh).await
        {
            if fresh.refresh_token.is_none() {
                fresh.refresh_token = Some(refresh);
            }
            let access = fresh.access_token.clone();
            creds.oauth = Some(fresh);
            if let Err(error) = save_to(path, password, &creds) {
                eprintln!(
                    "warning: refreshed the subscription session but could not update {} ({error})",
                    terminal_path(path)
                );
            }
            return Some(access);
        }
        eprintln!("your subscription session expired; run `grokforge login --subscription` again.");
    }
    None
}

// ---------- login subcommands ----------

/// Unlock an existing credentials file, or create a new one — returns `(password, current creds)`.
fn unlock_or_create(path: &std::path::Path) -> Result<(Zeroizing<String>, StoredCreds), ExitCode> {
    if !std::io::stdin().is_terminal() {
        eprintln!("`grokforge login` needs an interactive terminal.");
        return Err(ExitCode::from(2));
    }
    if path.exists() {
        let password =
            prompt_password("Enter your GrokForge password: ").ok_or(ExitCode::from(1))?;
        let creds = load_from(path, &password).map_err(|e| {
            eprintln!("{e}");
            ExitCode::from(1)
        })?;
        Ok((password, creds))
    } else {
        let password = prompt_new_password().ok_or(ExitCode::from(1))?;
        Ok((password, StoredCreds::default()))
    }
}

/// `grokforge login` — store an API key in the encrypted file.
#[must_use]
pub fn login() -> ExitCode {
    let path = match creds_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate credential storage: {error}");
            return ExitCode::from(1);
        }
    };
    let (password, mut creds) = match unlock_or_create(&path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!("Paste your xAI API key (input hidden). Get one at https://console.x.ai.");
    let Some(key) = prompt_api_key() else {
        eprintln!("no key entered.");
        return ExitCode::from(1);
    };
    creds.use_api_key(key);
    match save_to(&path, &password, &creds) {
        Ok(()) => {
            println!("✓ API key encrypted and saved to {}", terminal_path(&path));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not save credentials: {e}");
            ExitCode::from(1)
        }
    }
}

/// `grokforge login --subscription` — sign in with SuperGrok / X Premium+ and store the tokens
/// in the encrypted file.
pub async fn login_subscription() -> ExitCode {
    let path = match creds_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate credential storage: {error}");
            return ExitCode::from(1);
        }
    };
    let (password, mut creds) = match unlock_or_create(&path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!(
        "\nNote: xAI currently limits subscription (OAuth) API access to the SuperGrok Heavy tier."
    );
    eprintln!("Standard SuperGrok / X Premium+ may be refused with a 403 until xAI lifts that.\n");
    match oauth::login().await {
        Ok(tokens) => {
            creds.use_oauth(tokens);
            match save_to(&path, &password, &creds) {
                Ok(()) => {
                    println!("✓ signed in — subscription tokens encrypted and saved.");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("signed in, but could not save credentials: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!(
                "sign-in failed: {}",
                crate::sanitize_terminal_line(&e.to_string())
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let creds = StoredCreds {
            api_key: Some("xai-secret".to_string()),
            oauth: None,
        };
        let file = encrypt("hunter2", &creds).unwrap();
        let back = decrypt("hunter2", &file).unwrap();
        assert_eq!(back.api_key.as_deref(), Some("xai-secret"));
    }

    #[test]
    fn wrong_password_fails_to_decrypt() {
        let creds = StoredCreds {
            api_key: Some("xai-secret".to_string()),
            oauth: None,
        };
        let file = encrypt("correct-horse", &creds).unwrap();
        assert!(decrypt("wrong-password", &file).is_err());
    }

    #[test]
    fn salt_is_random_per_encryption() {
        let creds = StoredCreds::default();
        let a = encrypt("pw", &creds).unwrap();
        let b = encrypt("pw", &creds).unwrap();
        assert_ne!(a.salt, b.salt, "each encryption uses a fresh random salt");
        assert_ne!(a.nonce, b.nonce);
    }

    #[test]
    fn save_and_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.enc");
        let creds = StoredCreds {
            api_key: Some("xai-file-key".to_string()),
            oauth: None,
        };
        save_to(&path, "pass", &creds).unwrap();
        assert!(path.exists());
        let loaded = load_from(&path, "pass").unwrap();
        assert_eq!(loaded.api_key.as_deref(), Some("xai-file-key"));
        assert!(load_from(&path, "nope").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn malformed_envelopes_are_rejected_without_panicking() {
        let creds = StoredCreds::default();
        let mut file = encrypt("pw", &creds).unwrap();
        file.version = 2;
        assert!(decrypt("pw", &file).is_err());

        file.version = CREDENTIAL_FILE_VERSION;
        file.nonce = b64(b"short");
        assert!(decrypt("pw", &file).is_err());
    }

    #[test]
    fn authenticated_but_invalid_plaintext_is_rejected() {
        let salt = random(16).unwrap();
        let nonce = random(12).unwrap();
        let key = derive_key("pw", &salt).unwrap();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                b"not valid credential json".as_ref(),
            )
            .unwrap();
        let file = EncryptedFile {
            version: CREDENTIAL_FILE_VERSION,
            salt: b64(&salt),
            nonce: b64(&nonce),
            ciphertext: b64(&ciphertext),
        };

        assert!(decrypt("pw", &file).is_err());
    }

    #[test]
    fn choosing_a_login_method_clears_the_previous_one() {
        let oauth = OAuthTokens {
            access_token: "oauth-access".to_string(),
            refresh_token: Some("oauth-refresh".to_string()),
            expires_at: i64::MAX,
        };
        let mut creds = StoredCreds::default();
        creds.use_api_key("xai-key".to_string());
        creds.use_oauth(oauth);
        assert!(creds.api_key.is_none());
        assert!(creds.oauth.is_some());

        creds.use_api_key("replacement-key".to_string());
        assert_eq!(creds.api_key.as_deref(), Some("replacement-key"));
        assert!(creds.oauth.is_none());
    }

    #[test]
    fn new_password_minimum_is_long_enough_for_offline_storage() {
        assert!(!new_password_is_long_enough("short"));
        assert!(new_password_is_long_enough("correct horse"));
    }

    #[test]
    fn ambiguous_login_methods_are_rejected() {
        let creds = StoredCreds {
            api_key: Some("xai-key".to_string()),
            oauth: Some(OAuthTokens {
                access_token: "oauth-access".to_string(),
                refresh_token: None,
                expires_at: i64::MAX,
            }),
        };
        assert!(encrypt("pw", &creds).is_err());
    }

    #[test]
    fn oversized_credential_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.enc");
        std::fs::write(&path, vec![b'x'; CREDENTIAL_FILE_MAX_BYTES + 1]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_from(&path, "pw").unwrap_err().contains("safety limit"));
    }

    #[cfg(unix)]
    #[test]
    fn broad_credential_permissions_are_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.enc");
        let creds = StoredCreds::default();
        save_to(&path, "pw", &creds).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_from(&path, "pw").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credential_paths_reject_links() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.enc");
        let creds = StoredCreds::default();
        save_to(&original, "pw", &creds).unwrap();

        let symlink_path = dir.path().join("symlink.enc");
        symlink(&original, &symlink_path).unwrap();
        assert!(load_from(&symlink_path, "pw").is_err());
        assert!(save_to(&symlink_path, "pw", &creds).is_err());

        let hard_link_path = dir.path().join("hard-link.enc");
        std::fs::hard_link(&original, &hard_link_path).unwrap();
        assert!(load_from(&hard_link_path, "pw").is_err());
        assert!(save_to(&hard_link_path, "pw", &creds).is_err());
    }
}
