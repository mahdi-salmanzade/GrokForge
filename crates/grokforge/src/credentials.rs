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

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use argon2::Argon2;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use grokforge_xai::oauth::{self, OAuthTokens};
use serde::{Deserialize, Serialize};

/// The credentials held in the encrypted file (either or both may be set).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredCreds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<OAuthTokens>,
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

/// Derive a 32-byte key from the password and salt (Argon2id, default params).
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

fn encrypt(password: &str, creds: &StoredCreds) -> Result<EncryptedFile, String> {
    let salt = random(16)?;
    let nonce = random(12)?;
    let key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = serde_json::to_vec(creds).map_err(|e| e.to_string())?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
        .map_err(|_| "encryption failed".to_string())?;
    Ok(EncryptedFile {
        version: 1,
        salt: b64(&salt),
        nonce: b64(&nonce),
        ciphertext: b64(&ciphertext),
    })
}

fn decrypt(password: &str, file: &EncryptedFile) -> Result<StoredCreds, String> {
    let salt = unb64(&file.salt)?;
    let nonce = unb64(&file.nonce)?;
    let ciphertext = unb64(&file.ciphertext)?;
    let key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| "incorrect password (or the credentials file is corrupt)".to_string())?;
    serde_json::from_slice(&plaintext).map_err(|e| e.to_string())
}

/// `~/.grokforge/credentials.enc` (override with `GROKFORGE_CREDENTIALS_PATH`, used by tests).
fn creds_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GROKFORGE_CREDENTIALS_PATH") {
        return PathBuf::from(p);
    }
    let home = directories::BaseDirs::new()
        .map_or_else(|| PathBuf::from("."), |b| b.home_dir().to_path_buf());
    home.join(".grokforge").join("credentials.enc")
}

/// Whether an encrypted credentials file exists.
#[must_use]
pub fn has_stored_file() -> bool {
    creds_path().exists()
}

fn save_to(path: &std::path::Path, password: &str, creds: &StoredCreds) -> Result<(), String> {
    let file = encrypt(password, creds)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    // Owner-only permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn load_from(path: &std::path::Path, password: &str) -> Result<StoredCreds, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let file: EncryptedFile = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    decrypt(password, &file)
}

// ---------- prompts ----------

fn prompt_password(prompt: &str) -> Option<String> {
    rpassword::prompt_password(prompt)
        .ok()
        .filter(|p| !p.is_empty())
}

fn prompt_new_password() -> Option<String> {
    eprintln!("Create a password to encrypt your credentials on this machine.");
    eprintln!("(No recovery — if you forget it, you'll just sign in again.)");
    let first = prompt_password("New password: ")?;
    let confirm = prompt_password("Confirm password: ")?;
    if first != confirm {
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

// ---------- resolution ----------

/// Resolve a bearer token: `XAI_API_KEY` env → password-unlock the encrypted file → (first run,
/// interactive) onboarding. Returns `None` (after printing guidance) when nothing is available.
pub async fn resolve(allow_prompt: bool) -> Option<String> {
    if let Ok(key) = std::env::var("XAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }

    let path = creds_path();
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
    use std::io::Write as _;
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
        creds.api_key = Some(key.clone());
        key
    } else {
        eprintln!("Note: subscription API access currently requires the SuperGrok Heavy tier.\n");
        match oauth::login().await {
            Ok(tokens) => {
                let access = tokens.access_token.clone();
                creds.oauth = Some(tokens);
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
        Ok(()) => eprintln!("✓ credentials encrypted and saved to {}", path.display()),
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
            let _ = save_to(path, password, &creds);
            return Some(access);
        }
        eprintln!("your subscription session expired; run `grokforge login --subscription` again.");
    }
    None
}

// ---------- login subcommands ----------

/// Unlock an existing credentials file, or create a new one — returns `(password, current creds)`.
fn unlock_or_create(path: &std::path::Path) -> Result<(String, StoredCreds), ExitCode> {
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
    let path = creds_path();
    let (password, mut creds) = match unlock_or_create(&path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    eprintln!("Paste your xAI API key (input hidden). Get one at https://console.x.ai.");
    let Some(key) = prompt_api_key() else {
        eprintln!("no key entered.");
        return ExitCode::from(1);
    };
    creds.api_key = Some(key);
    match save_to(&path, &password, &creds) {
        Ok(()) => {
            println!("✓ API key encrypted and saved to {}", path.display());
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
    let path = creds_path();
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
            creds.oauth = Some(tokens);
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
    }
}
