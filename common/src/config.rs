//! Static configuration: URLs, OAuth client wiring, throttle constants, paths.
//!
//! Anything that an operator might legitimately want to tune is an env var
//! (`WFTUI_*`); everything else is a constant on purpose — the values here are
//! politeness budgets agreed with the site operator, not preferences.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::Error;

/// Site origin default. The REST API lives at `{base_url()}/api`.
pub const BASE_URL: &str = "https://windowsforum.com";

/// Origin override for tests/staging (`WFTUI_BASE_URL`).
pub fn base_url() -> String {
    std::env::var("WFTUI_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| BASE_URL.to_string())
}

pub const OAUTH_AUTHORIZE_PATH: &str = "/oauth2/authorize";
pub const OAUTH_TOKEN_PATH: &str = "/api/oauth2/token";
pub const OAUTH_REVOKE_PATH: &str = "/api/oauth2/revoke";

/// Loopback redirect the TUI listens on during the OAuth handshake. The
/// matching redirect URIs must be registered on the OAuth client row.
pub const LOOPBACK_PORT: u16 = 9420;
pub const REDIRECT_URIS: [&str; 2] = [
    "http://127.0.0.1:9420/callback",
    "http://localhost:9420/callback",
];

/// Public OAuth client id (PKCE, no secret). Registered by
/// `/web/ops/wftui_oauth_client.php` on 2026-09-05 ("WindowsForum TUI",
/// client_type=public, loopback redirect 127.0.0.1:9420). Public clients
/// carry no secret, so this id is safe to embed.
pub const DEFAULT_OAUTH_CLIENT_ID: &str = "6014883021104153";

pub fn oauth_client_id() -> Result<String, Error> {
    let id = std::env::var("WFTUI_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_ID.to_string());
    if id.trim().is_empty() {
        return Err(Error::Config(
            "no OAuth client id: set WFTUI_OAUTH_CLIENT_ID (public PKCE client \
             registered on windowsforum.com)"
                .into(),
        ));
    }
    Ok(id)
}

/// Scopes requested during authorization. Keep in sync with the client row's
/// allowed scope list; requesting a scope the client lacks fails the handshake.
pub const SCOPES: [&str; 12] = [
    "node:read",
    "thread:read",
    "thread:write",
    "user:read",
    "conversation:read",
    "conversation:write",
    "alert:read",
    "search:read",
    "search:write",
    "attachment:read",
    "attachment:write",
    "profile_post:read",
];

/// Minimum spacing between any two API calls (politeness budget; the zone's
/// flood ceiling is shared with every other visitor).
pub const GLOBAL_MIN_INTERVAL_MS: u64 = 250;
/// Search is the expensive server-side path (ES/hybrid backend): stricter.
pub const SEARCH_MIN_INTERVAL_MS: u64 = 3_000;
/// XF enforces 30s between posts / 180s between new threads per user; mirror
/// that client-side so the compose screen can show the wait instead of 422ing.
pub const WRITE_COOLDOWN_MS: u64 = 30_000;
pub const NEW_THREAD_COOLDOWN_MS: u64 = 180_000;

/// Floor applied to the relevant gate when a 429 carries no `Retry-After`
/// header at all — a conservative fallback, not a measured value.
pub const DEFAULT_RATE_LIMIT_RETRY_SECS: u64 = 30;

pub const ALERT_POLL_SECS: u64 = 45;
pub const CONVERSATION_POLL_SECS: u64 = 90;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Attachment uploads bypass the default total timeout.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

pub fn user_agent() -> String {
    format!("wftui/{} (+{})", env!("CARGO_PKG_VERSION"), BASE_URL)
}

pub fn api_base() -> String {
    format!("{}/api", base_url())
}

/// Site-relayed OAuth redirect (WindowsForum\TuiLink addon). The authorize
/// request uses this as redirect_uri; /tui-done captures the code server-side
/// and the TUI polls for it — no copy-paste, works over SSH and on phones.
pub fn tui_done_url() -> String {
    format!("{}/tui-done", base_url())
}

fn config_root() -> PathBuf {
    if let Ok(dir) = std::env::var("WFTUI_CONFIG_DIR")
        && !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wftui")
}

pub fn token_path() -> PathBuf {
    config_root().join("token.json")
}

pub fn log_path() -> PathBuf {
    config_root().join("wftui.log")
}

/// Serialized env access for tests (base URL override is process-global).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_is_origin_scoped() {
        // Another test may be pointing WFTUI_BASE_URL at a mock server.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("WFTUI_BASE_URL") };
        assert_eq!(api_base(), "https://windowsforum.com/api");
    }

    #[test]
    fn user_agent_is_distinctive_not_library_default() {
        let ua = user_agent();
        assert!(ua.starts_with("wftui/"));
        assert!(ua.contains("windowsforum.com"));
        // Cloudflare's bot rules block bare library UAs (reqwest/hyper/...);
        // ours must never degrade to one of those signatures.
        for banned in ["reqwest", "hyper", "python", "okhttp", "axios", "go-http"] {
            assert!(!ua.to_ascii_lowercase().contains(banned), "{ua}");
        }
    }

    #[test]
    fn scopes_are_read_write_pairs() {
        for scope in SCOPES {
            let (head, tail) = scope.split_once(':').expect("scope shape");
            assert!(matches!(tail, "read" | "write"), "{scope}");
            assert!(!head.is_empty());
        }
    }

    #[test]
    fn oauth_client_id_env_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("WFTUI_OAUTH_CLIENT_ID", "12345") };
        assert_eq!(oauth_client_id().unwrap(), "12345");
        unsafe { std::env::set_var("WFTUI_OAUTH_CLIENT_ID", "") };
        // Empty override falls back to the baked-in public client id.
        assert_eq!(oauth_client_id().unwrap(), DEFAULT_OAUTH_CLIENT_ID);
        unsafe { std::env::remove_var("WFTUI_OAUTH_CLIENT_ID") };
        assert_eq!(oauth_client_id().unwrap(), DEFAULT_OAUTH_CLIENT_ID);
    }
}
