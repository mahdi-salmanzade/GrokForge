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

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use grokforge_mcp::oauth::{self as mcp_oauth, OAuthRecord};
use grokforge_xai::oauth::{self as xai_oauth, OAuthTokens};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize as _, Zeroizing};

const CREDENTIAL_FILE_VERSION: u8 = 1;
const CREDENTIAL_FILE_MAX_BYTES: usize = 8 * 1024 * 1024;
const ARGON2_MEMORY_KIB: u32 = 19_456;
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;
const SHORT_PASSWORD_WARNING_CHARS: usize = 12;
const MAX_MCP_OAUTH_RECORDS: usize = 16;

/// Credentials held in the encrypted file. New writes keep exactly one xAI login method active;
/// independently bound MCP OAuth records coexist in the same encrypted payload. Ambiguous legacy
/// xAI files containing both login methods are rejected instead of guessing which one to bill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredCreds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthTokens>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mcp_oauth: BTreeMap<String, OAuthRecord>,
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
        if self.mcp_oauth.len() > MAX_MCP_OAUTH_RECORDS {
            return Err(format!(
                "credentials file contains more than {MAX_MCP_OAUTH_RECORDS} MCP OAuth records"
            ));
        }
        for name in self.mcp_oauth.keys() {
            if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
                return Err("credentials file contains an invalid MCP OAuth server name".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct UnlockedCredentials {
    path: PathBuf,
    password: Zeroizing<String>,
    creds: StoredCreds,
}

static UNLOCKED_CREDENTIALS: OnceLock<Mutex<Option<UnlockedCredentials>>> = OnceLock::new();

fn remember_unlocked(path: &Path, password: &str, creds: &StoredCreds) {
    let unlocked = UnlockedCredentials {
        path: path.to_path_buf(),
        password: Zeroizing::new(password.to_string()),
        creds: creds.clone(),
    };
    if let Ok(mut cache) = UNLOCKED_CREDENTIALS.get_or_init(|| Mutex::new(None)).lock() {
        *cache = Some(unlocked);
    }
}

fn recalled_unlocked(path: &Path) -> Option<UnlockedCredentials> {
    UNLOCKED_CREDENTIALS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref().filter(|value| value.path == path).cloned())
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
    if json.len() > CREDENTIAL_FILE_MAX_BYTES {
        return Err(format!(
            "encrypted credentials exceed the {CREDENTIAL_FILE_MAX_BYTES}-byte safety limit"
        ));
    }

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

fn new_password_is_short(password: &str) -> bool {
    password.chars().count() < SHORT_PASSWORD_WARNING_CHARS
}

fn prompt_new_password() -> Option<Zeroizing<String>> {
    eprintln!("Create a password to encrypt your credentials on this machine.");
    eprintln!(
        "Any non-empty password is accepted. Longer passwords are harder to guess if the encrypted file is copied."
    );
    let first = prompt_password("New password: ")?;
    if new_password_is_short(&first) {
        eprintln!(
            "warning: passwords shorter than {SHORT_PASSWORD_WARNING_CHARS} characters are easier to guess; continuing with your choice."
        );
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
        let mut creds = match load_from(&path, &password) {
            Ok(creds) => creds,
            Err(e) => {
                eprintln!("{e}");
                return None;
            }
        };
        let bearer = bearer_from(&path, &mut creds, &password).await;
        remember_unlocked(&path, &password, &creds);
        return bearer;
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
        match xai_oauth::login().await {
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
        Ok(()) => {
            remember_unlocked(path, &password, &creds);
            eprintln!(
                "✓ credentials encrypted and saved to {}",
                terminal_path(path)
            );
        }
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
    creds: &mut StoredCreds,
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
            && let Ok(mut fresh) = xai_oauth::refresh(&refresh).await
        {
            if fresh.refresh_token.is_none() {
                fresh.refresh_token = Some(refresh);
            }
            let access = fresh.access_token.clone();
            creds.oauth = Some(fresh);
            if let Err(error) = save_to(path, password, creds) {
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
    match xai_oauth::login().await {
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

/// Resolve OAuth Bearer tokens for trusted remote MCP servers. The already-unlocked credential
/// payload is reused when xAI credentials came from the same file, so startup asks for the
/// password at most once. Expired tokens are rediscovered, binding-checked, refreshed, and sealed
/// back into the same file before use.
pub async fn mcp_access_tokens(workspace: &Path) -> BTreeMap<String, String> {
    let clients = match grokforge_core::mcp_config::oauth_client_configs(workspace).await {
        Ok(clients) => clients,
        Err(error) => {
            eprintln!(
                "MCP OAuth configuration error: {}",
                crate::sanitize_terminal(&error)
            );
            return BTreeMap::new();
        }
    };
    if clients.is_empty() {
        return BTreeMap::new();
    }
    let path = match creds_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate MCP OAuth credential storage: {error}");
            return BTreeMap::new();
        }
    };
    let mut unlocked = if let Some(unlocked) = recalled_unlocked(&path) {
        unlocked
    } else {
        if !path.exists() {
            eprintln!("MCP OAuth is not signed in; run `grokforge login --mcp <name>`.");
            return BTreeMap::new();
        }
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "MCP OAuth credentials are password-encrypted and cannot be unlocked on protocol stdin."
            );
            return BTreeMap::new();
        }
        let Some(password) = prompt_password("Enter your GrokForge password for MCP OAuth: ")
        else {
            return BTreeMap::new();
        };
        let creds = match load_from(&path, &password) {
            Ok(creds) => creds,
            Err(error) => {
                eprintln!("{error}");
                return BTreeMap::new();
            }
        };
        UnlockedCredentials {
            path: path.clone(),
            password,
            creds,
        }
    };

    let mut access_tokens = BTreeMap::new();
    let mut changed = false;
    for (name, client) in clients {
        let Some(record) = unlocked.creds.mcp_oauth.get(&name).cloned() else {
            eprintln!("MCP OAuth `{name}` is not signed in; run `grokforge login --mcp {name}`.");
            continue;
        };
        if !mcp_oauth::stored_binding_matches(&client, &record) {
            eprintln!(
                "MCP OAuth `{name}` configuration changed; run `grokforge login --mcp {name}` again."
            );
            continue;
        }
        let current = if record.tokens.is_valid() {
            record
        } else {
            match mcp_oauth::refresh(&client, &record).await {
                Ok(fresh) => {
                    changed = true;
                    fresh
                }
                Err(error) => {
                    eprintln!(
                        "MCP OAuth `{name}` refresh failed: {}",
                        crate::sanitize_terminal(&error.to_string())
                    );
                    continue;
                }
            }
        };
        access_tokens.insert(name.clone(), current.tokens.access_token.clone());
        unlocked.creds.mcp_oauth.insert(name, current);
    }
    if changed && let Err(error) = save_to(&path, &unlocked.password, &unlocked.creds) {
        eprintln!(
            "warning: refreshed MCP OAuth but could not update {} ({error})",
            terminal_path(&path)
        );
    }
    remember_unlocked(&path, &unlocked.password, &unlocked.creds);
    access_tokens
}

/// `grokforge login --mcp <name>` — authorize one pre-registered project MCP client and store its
/// bound tokens inside the existing password-encrypted credential payload.
pub async fn login_mcp(workspace: &Path, name: &str) -> ExitCode {
    let mut clients = match grokforge_core::mcp_config::oauth_client_configs(workspace).await {
        Ok(clients) => clients,
        Err(error) => {
            eprintln!(
                "MCP OAuth configuration error: {}",
                crate::sanitize_terminal(&error)
            );
            return ExitCode::from(2);
        }
    };
    let Some(client) = clients.remove(name) else {
        let available = clients.keys().cloned().collect::<Vec<_>>().join(", ");
        if available.is_empty() {
            eprintln!("no OAuth-enabled remote MCP servers are configured in .grokforge/mcp.json");
        } else {
            eprintln!("unknown MCP OAuth server `{name}`; configured: {available}");
        }
        return ExitCode::from(2);
    };
    let path = match creds_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot locate credential storage: {error}");
            return ExitCode::from(1);
        }
    };
    let (password, mut creds) = match unlock_or_create(&path) {
        Ok(value) => value,
        Err(code) => return code,
    };
    eprintln!(
        "Authorizing trusted project MCP `{}`. Tokens will only be stored in {}.",
        crate::sanitize_terminal_line(name),
        terminal_path(&path)
    );
    let record = match mcp_oauth::login(&client).await {
        Ok(record) => record,
        Err(error) => {
            eprintln!(
                "MCP sign-in failed: {}",
                crate::sanitize_terminal(&error.to_string())
            );
            return ExitCode::from(1);
        }
    };
    creds.mcp_oauth.insert(name.to_string(), record);
    match save_to(&path, &password, &creds) {
        Ok(()) => {
            remember_unlocked(&path, &password, &creds);
            println!(
                "✓ MCP `{}` tokens encrypted and saved.",
                crate::sanitize_terminal_line(name)
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("authorized, but could not save credentials: {error}");
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
            ..StoredCreds::default()
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
            ..StoredCreds::default()
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
            ..StoredCreds::default()
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
    fn mcp_tokens_are_encrypted_persisted_and_wrong_password_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.enc");
        let mut creds = StoredCreds::default();
        creds.mcp_oauth.insert(
            "remote".into(),
            OAuthRecord {
                binding: grokforge_mcp::oauth::OAuthBinding {
                    endpoint: "https://mcp.example/rpc".into(),
                    resource: "https://mcp.example/".into(),
                    issuer: "https://auth.example/".into(),
                    client_id: "grokforge-client".into(),
                    redirect_uri: "http://127.0.0.1:49152/callback".into(),
                },
                tokens: grokforge_mcp::oauth::OAuthTokens {
                    access_token: "mcp-secret-access".into(),
                    refresh_token: Some("mcp-secret-refresh".into()),
                    expires_at: i64::MAX,
                    scope: Some("files:read".into()),
                },
            },
        );

        save_to(&path, "correct-password", &creds).unwrap();
        let envelope = std::fs::read_to_string(&path).unwrap();
        assert!(!envelope.contains("mcp-secret-access"));
        assert!(!envelope.contains("mcp-secret-refresh"));
        let loaded = load_from(&path, "correct-password").unwrap();
        assert_eq!(
            loaded.mcp_oauth["remote"].tokens.access_token,
            "mcp-secret-access"
        );
        assert!(load_from(&path, "wrong-password").is_err());
    }

    #[test]
    fn legacy_credential_payloads_default_to_no_mcp_tokens() {
        let legacy: StoredCreds = serde_json::from_str(r#"{"api_key":"xai-legacy"}"#).unwrap();
        assert_eq!(legacy.api_key.as_deref(), Some("xai-legacy"));
        assert!(legacy.mcp_oauth.is_empty());
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
    fn short_passwords_warn_but_are_not_a_validation_boundary() {
        assert!(new_password_is_short("1234"));
        assert!(!new_password_is_short("correct horse"));
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
            ..StoredCreds::default()
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
