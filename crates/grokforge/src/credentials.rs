//! xAI API key resolution + secure storage.
//!
//! Resolution order: `XAI_API_KEY` env (best for CI) → OS keychain → (interactive) hidden
//! prompt that saves into the keychain. The key is never written to disk in plaintext.

use std::io::IsTerminal;
use std::process::ExitCode;

const SERVICE: &str = "grokforge";
const ACCOUNT: &str = "xai-api-key";

/// Load a stored key from the OS keychain, if present.
#[must_use]
pub fn load_stored() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => None,
    }
}

/// Store (or replace) the key in the OS keychain.
pub fn store(key: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
    entry.set_password(key).map_err(|e| e.to_string())
}

fn prompt_hidden() -> Option<String> {
    rpassword::prompt_password("xAI API key (input hidden): ")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the API key. When `allow_prompt` is set, stdin is a TTY, and nothing is configured,
/// interactively prompt (hidden) and save it to the keychain. Returns `None` (after printing
/// guidance) when no key can be obtained.
#[must_use]
pub fn resolve(allow_prompt: bool) -> Option<String> {
    if let Ok(key) = std::env::var("XAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    if let Some(key) = load_stored() {
        return Some(key);
    }
    if allow_prompt && std::io::stdin().is_terminal() {
        eprintln!("No xAI API key found. Paste it below — it will be saved to your OS keychain.");
        eprintln!("(Get a key at https://console.x.ai; you'll need API credits.)");
        if let Some(key) = prompt_hidden() {
            match store(&key) {
                Ok(()) => {
                    eprintln!("✓ saved to keychain (change it later with `grokforge login`)");
                }
                Err(e) => {
                    eprintln!(
                        "warning: couldn't save to keychain ({e}); using it for this session only"
                    );
                }
            }
            return Some(key);
        }
        eprintln!("no key entered.");
    } else {
        eprintln!(
            "No xAI API key. Set XAI_API_KEY, or run `grokforge login` to store one securely."
        );
    }
    None
}

/// `grokforge login` — prompt for a key and store it in the OS keychain.
#[must_use]
pub fn login() -> ExitCode {
    if !std::io::stdin().is_terminal() {
        eprintln!("`grokforge login` needs an interactive terminal.");
        return ExitCode::from(2);
    }
    eprintln!("Paste your xAI API key (input hidden). Get one at https://console.x.ai.");
    let Some(key) = prompt_hidden() else {
        eprintln!("no key entered.");
        return ExitCode::from(1);
    };
    match store(&key) {
        Ok(()) => {
            println!("✓ API key stored in your OS keychain.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not store key: {e}");
            ExitCode::from(1)
        }
    }
}
