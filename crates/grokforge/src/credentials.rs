//! xAI credential resolution + secure storage, for two auth methods:
//! - **API key** (`XAI_API_KEY` / `grokforge login`) — pay-per-token developer billing.
//! - **Subscription OAuth** (`grokforge login --subscription`) — signs in with SuperGrok /
//!   X Premium+ so usage bills against the subscription. The bearer token is used identically.
//!
//! Resolution order: `XAI_API_KEY` env → stored API key → stored OAuth token (refreshed if
//! expired) → (interactive) hidden API-key prompt. Nothing is written to disk in plaintext;
//! secrets live in the OS keychain.

use std::io::IsTerminal;
use std::process::ExitCode;

use grokforge_xai::oauth::{self, OAuthTokens};

const SERVICE: &str = "grokforge";
const ACCOUNT: &str = "xai-api-key";
const OAUTH_ACCOUNT: &str = "xai-oauth";

// ---------- API key storage ----------

/// Load a stored API key from the OS keychain, if present.
#[must_use]
pub fn load_stored() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => None,
    }
}

/// Store (or replace) the API key in the OS keychain.
pub fn store(key: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
    entry.set_password(key).map_err(|e| e.to_string())
}

// ---------- OAuth (subscription) token storage ----------

fn load_oauth() -> Option<OAuthTokens> {
    let entry = keyring::Entry::new(SERVICE, OAUTH_ACCOUNT).ok()?;
    let json = entry.get_password().ok()?;
    serde_json::from_str(&json).ok()
}

/// Whether a subscription (OAuth) token is stored. Used by `doctor` (no network).
#[must_use]
pub fn has_oauth() -> bool {
    load_oauth().is_some()
}

fn store_oauth(tokens: &OAuthTokens) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, OAUTH_ACCOUNT).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
    entry.set_password(&json).map_err(|e| e.to_string())
}

/// Return a usable OAuth access token, refreshing (and re-storing) if it has expired.
async fn oauth_access_token() -> Option<String> {
    let tokens = load_oauth()?;
    if tokens.is_valid() {
        return Some(tokens.access_token);
    }
    // Expired — try to refresh.
    let refresh = tokens.refresh_token?;
    match oauth::refresh(&refresh).await {
        Ok(mut fresh) => {
            if fresh.refresh_token.is_none() {
                fresh.refresh_token = Some(refresh);
            }
            let token = fresh.access_token.clone();
            let _ = store_oauth(&fresh);
            Some(token)
        }
        Err(e) => {
            eprintln!(
                "subscription token refresh failed ({e}); run `grokforge login --subscription` again."
            );
            None
        }
    }
}

// ---------- resolution ----------

fn prompt_hidden() -> Option<String> {
    rpassword::prompt_password("xAI API key (input hidden): ")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve a bearer token for the API (an API key or an OAuth access token — the client sends
/// either as `Authorization: Bearer`). When `allow_prompt` is set and stdin is a TTY, prompt for
/// an API key as a last resort. Returns `None` (after printing guidance) when nothing is available.
pub async fn resolve(allow_prompt: bool) -> Option<String> {
    if let Ok(key) = std::env::var("XAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    if let Some(key) = load_stored() {
        return Some(key);
    }
    if let Some(token) = oauth_access_token().await {
        return Some(token);
    }
    if allow_prompt && std::io::stdin().is_terminal() {
        return onboard_interactive().await;
    }
    eprintln!(
        "No xAI credentials. Set XAI_API_KEY, run `grokforge login` (API key), or `grokforge login --subscription` (SuperGrok)."
    );
    None
}

/// First-run onboarding shown inside `grokforge` when no credential exists: let the user sign in
/// with their subscription or paste an API key, right here — no separate command needed.
async fn onboard_interactive() -> Option<String> {
    use std::io::Write as _;
    eprintln!("\nWelcome to GrokForge 👋  You're not signed in yet. Choose how to connect:");
    eprintln!("  [1] Sign in with your Grok subscription (SuperGrok / X Premium+) — no API key");
    eprintln!("  [2] Paste an xAI API key (console.x.ai)");
    eprint!("Choice [1/2] (default 1): ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    if line.trim() == "2" {
        // Paste an API key.
        let key = prompt_hidden()?;
        match store(&key) {
            Ok(()) => eprintln!("✓ saved to keychain"),
            Err(e) => eprintln!("warning: couldn't save to keychain ({e}); using for this session"),
        }
        Some(key)
    } else {
        // Default: subscription OAuth.
        eprintln!("Note: subscription API access currently requires the SuperGrok Heavy tier.\n");
        match oauth::login().await {
            Ok(tokens) => {
                let token = tokens.access_token.clone();
                if let Err(e) = store_oauth(&tokens) {
                    eprintln!("signed in, but couldn't store tokens ({e}); using for this session");
                } else {
                    eprintln!("✓ signed in — starting GrokForge…");
                }
                Some(token)
            }
            Err(e) => {
                eprintln!("sign-in failed: {e}");
                None
            }
        }
    }
}

// ---------- login subcommands ----------

/// `grokforge login` — prompt for an API key and store it in the OS keychain.
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

/// `grokforge login --subscription` — sign in with SuperGrok / X Premium+ via OAuth and store
/// the resulting tokens in the OS keychain. Usage then bills against the subscription.
pub async fn login_subscription() -> ExitCode {
    eprintln!(
        "Note: xAI currently limits subscription (OAuth) API access to the SuperGrok Heavy tier."
    );
    eprintln!("Standard SuperGrok / X Premium+ may be refused with a 403 until xAI lifts that.\n");
    match oauth::login().await {
        Ok(tokens) => match store_oauth(&tokens) {
            Ok(()) => {
                println!("✓ signed in — subscription tokens stored in your OS keychain.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("signed in, but could not store tokens: {e}");
                ExitCode::from(1)
            }
        },
        Err(e) => {
            eprintln!("sign-in failed: {e}");
            ExitCode::from(1)
        }
    }
}
