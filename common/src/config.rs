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
    let raw = std::env::var("WFTUI_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| BASE_URL.to_string());
    raw.trim().trim_end_matches('/').to_string()
}

/// Normalize and validate the configured site origin once at client
/// construction. Endpoint builders assume this is an origin, not a path or
/// query-bearing URL.
pub fn validate_base_url(raw: &str) -> Result<String, Error> {
    let normalized = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(normalized)
        .map_err(|e| Error::Config(format!("invalid site origin {raw:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.path() != "" && url.path() != "/")
    {
        return Err(Error::Config(format!(
            "site origin must be an http(s) origin without credentials, path, query, or fragment: {raw:?}"
        )));
    }
    Ok(normalized.to_string())
}

pub const OAUTH_AUTHORIZE_PATH: &str = "/oauth2/authorize";
pub const OAUTH_TOKEN_PATH: &str = "/api/oauth2/token";
pub const OAUTH_REVOKE_PATH: &str = "/api/oauth2/revoke";

/// Loopback port the client tries first for the OAuth redirect. XenForo
/// matches loopback-IP redirect URIs without regard to port (RFC 8252
/// §7.3), so a registered `http://127.0.0.1/callback` accepts this port and
/// the ephemeral one `oauth::bind_loopback` falls back to; an admin who
/// registered the exact `:9420` form on a stricter server still works.
pub const LOOPBACK_PORT: u16 = 9420;

/// Public OAuth client id (PKCE, no secret) of the built-in site.
/// Registered by `/web/ops/wftui_oauth_client.php` on 2026-09-05
/// ("WindowsForum TUI", client_type=public). Public clients carry no secret,
/// so this id is safe to embed. Any other site's id comes from
/// `config.json` (`site::SiteConfig::oauth_client_id`).
pub const DEFAULT_OAUTH_CLIENT_ID: &str = "6014883021104153";

/// The built-in site's client id, or the `WFTUI_OAUTH_CLIENT_ID` override.
/// `site::resolve` applies the same override on top of any site.
pub fn oauth_client_id() -> Result<String, Error> {
    let id = std::env::var("WFTUI_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_ID.to_string());
    if id.trim().is_empty() {
        return Err(Error::Config(
            "no OAuth client id: set WFTUI_OAUTH_CLIENT_ID (a public PKCE client \
             registered on the forum)"
                .into(),
        ));
    }
    Ok(id)
}

/// Minimum spacing between any two API calls (politeness budget; the zone's
/// flood ceiling is shared with every other visitor).
pub const GLOBAL_MIN_INTERVAL_MS: u64 = 250;
/// Thumbnails, avatars and the sign-in logo (`WfApiClient::fetch_bytes`) get
/// their OWN budget at the same spacing. `Gate` is FIFO with no priority, so
/// while decoration shared `api_gate` a single cold thread open reserved one
/// slot per visible image and every navigation the user made next queued
/// behind them — ~10 images ≈ 2.5 s before `/threads/{id}` was even sent
/// (issue #543). Same politeness per request; decoration just cannot spend
/// the interactive lane's slots any more.
pub const IMAGE_MIN_INTERVAL_MS: u64 = 250;
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
/// A release binary is ~8 MiB; a slow link needs longer than a page fetch.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Ceiling for one attachment download (`attachment_data`): XF caps uploads
/// well below this, so anything larger is a mistake or an attack. Enforced
/// at `Content-Length` and again on the streamed body, like `fetch_bytes`.
pub const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;

/// The UA for the built-in site: `wftui/<ver> (+https://windowsforum.com)`.
/// Hard rule 6 — Cloudflare's bot rule on windowsforum.com is keyed to it.
pub fn user_agent() -> String {
    format!("wftui/{} (+{})", env!("CARGO_PKG_VERSION"), BASE_URL)
}

/// The UA for any other site: same prefix, the client's own project URL.
/// Still distinctive, never another forum's address.
pub fn user_agent_for_project() -> String {
    format!("wftui/{} (+{})", env!("CARGO_PKG_VERSION"), crate::site::PROJECT_URL)
}

pub fn api_base() -> String {
    format!("{}/api", base_url())
}

/// Whether the client captures the mouse at all (`WFTUI_MOUSE`).
///
/// `WFTUI_MOUSE=0` (also `off`/`false`/`no`) skips `EnableMouseCapture`
/// entirely, so the terminal keeps its own selection and scrollback bindings
/// and every gesture — click, drag-select, wheel, tap, long-press — belongs to
/// the terminal rather than to us. The client then builds no hit map either:
/// with nothing capturing pointer events there is nothing to resolve. It is
/// the same escape hatch `Shift+drag` gives for one drag, made permanent.
pub fn mouse_enabled() -> bool {
    match std::env::var("WFTUI_MOUSE") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ),
        Err(_) => true,
    }
}

fn config_root() -> PathBuf {
    if let Ok(dir) = std::env::var("WFTUI_CONFIG_DIR")
        && !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    default_config_root()
}

/// The config dir itself (`WFTUI_CONFIG_DIR`, else the platform default):
/// where `config.json` lives and what a relative logo path resolves against.
pub fn config_dir() -> PathBuf {
    config_root()
}

/// Where one site's own files live. The built-in site keeps the flat layout
/// every existing install already has (`<config dir>/token.json`), so
/// nobody is signed out by an upgrade; any other site gets
/// `<config dir>/sites/<name>/`. `update/`, `cache/` and the log stay
/// shared — none of them is about a site.
pub fn site_root(name: &str) -> PathBuf {
    if name == crate::site::BUILTIN_NAME {
        config_root()
    } else {
        config_root().join("sites").join(name)
    }
}

pub fn token_path_for(name: &str) -> PathBuf {
    site_root(name).join("token.json")
}

pub fn drafts_path_for(name: &str) -> PathBuf {
    site_root(name).join("drafts.json")
}

/// The config dir used when `WFTUI_CONFIG_DIR` is unset — i.e. the machine
/// owner's real one. Exposed so both crates' test suites can assert that
/// nothing they build ever resolves to it (issue #565: the suite used to read
/// and overwrite the operator's own `token.json`).
pub fn default_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wftui")
}

pub fn token_path() -> PathBuf {
    config_root().join("token.json")
}

/// Where composer drafts are kept (#715). Beside the token store, in the
/// same config dir, so `WFTUI_CONFIG_DIR` moves both together and a test can
/// never reach the operator's own.
pub fn drafts_path() -> PathBuf {
    config_root().join("drafts.json")
}

pub fn log_path() -> PathBuf {
    config_root().join("wftui.log")
}

/// Where a downloaded update is staged until the next start applies it
/// (`update::apply_pending_update`). Under the config dir like everything
/// else the client writes, so `WFTUI_CONFIG_DIR` moves it and a test can
/// never reach the operator's own.
pub fn update_root() -> PathBuf {
    config_root().join("update")
}

/// The release feed (`WFTUI_UPDATE_URL`): a URL answering with GitHub's
/// `releases/latest` JSON shape. The default is the client's own repository.
pub fn update_feed_url() -> String {
    std::env::var("WFTUI_UPDATE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::update::DEFAULT_FEED_URL.to_string())
}

/// `WFTUI_NO_UPDATE=1` turns the self-updater off entirely: no feed check,
/// no download, and nothing applied at start. For a checkout's `bin/wftui`,
/// a distro package, or anyone who would rather update by hand.
pub fn updates_disabled() -> bool {
    match std::env::var("WFTUI_NO_UPDATE") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "yes"
        ),
        Err(_) => false,
    }
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
    fn base_url_validation_normalizes_only_an_origin() {
        assert_eq!(
            validate_base_url("  https://example.test/// ").unwrap(),
            "https://example.test"
        );
        for invalid in [
            "example.test",
            "ftp://example.test",
            "https://example.test/api",
            "https://user:pass@example.test",
            "https://example.test/?x=1",
            "https://example.test/#fragment",
        ] {
            assert!(validate_base_url(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn user_agent_is_distinctive_not_library_default() {
        let ua = user_agent();
        assert!(ua.starts_with("wftui/"));
        assert!(ua.contains("windowsforum.com"));
        let project = user_agent_for_project();
        assert!(project.starts_with("wftui/"));
        assert!(project.contains("github.com/faratech/wftui") && !project.contains("windowsforum"));
        // Cloudflare's bot rules block bare library UAs (reqwest/hyper/...);
        // ours must never degrade to one of those signatures.
        for ua in [ua, project] {
            for banned in ["reqwest", "hyper", "python", "okhttp", "axios", "go-http"] {
                assert!(!ua.to_ascii_lowercase().contains(banned), "{ua}");
            }
        }
    }

    /// #695: the Media Gallery and Resource Manager screens 403 with
    /// `missing_scope` unless the grant asks for these. An API key ignores
    /// scopes, so nothing but this test catches a screen shipped without
    /// its scope.
    #[test]
    fn scopes_cover_every_endpoint_family_the_client_calls() {
        let scopes = crate::site::SiteConfig::windowsforum().effective_scopes();
        for needed in [
            "node:read",
            "thread:read",
            "conversation:read",
            "alert:read",
            "search:read",
            "attachment:read",
            "media:read",
            "resource:read",
        ] {
            assert!(scopes.iter().any(|s| s == needed), "missing scope: {needed}");
        }
    }

    #[test]
    fn scopes_are_read_write_pairs() {
        for scope in crate::site::BASE_SCOPES {
            let (head, tail) = scope.split_once(':').expect("scope shape");
            assert!(matches!(tail, "read" | "write"), "{scope}");
            assert!(!head.is_empty());
        }
    }

    /// The built-in site's files stay where every install already has them;
    /// another site's live in their own directory, and both hang off the
    /// same config dir so `WFTUI_CONFIG_DIR` moves everything together.
    #[test]
    fn site_roots_keep_the_builtin_flat_and_nest_the_rest() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("WFTUI_CONFIG_DIR").ok();
        unsafe { std::env::set_var("WFTUI_CONFIG_DIR", "/tmp/wftui-t-roots") };
        assert_eq!(token_path_for("windowsforum"), token_path());
        assert_eq!(drafts_path_for("windowsforum"), drafts_path());
        assert_eq!(token_path_for("other"), PathBuf::from("/tmp/wftui-t-roots/sites/other/token.json"));
        assert_eq!(drafts_path_for("other"), PathBuf::from("/tmp/wftui-t-roots/sites/other/drafts.json"));
        assert_eq!(crate::site::config_path(), PathBuf::from("/tmp/wftui-t-roots/config.json"));
        match saved {
            Some(v) => unsafe { std::env::set_var("WFTUI_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("WFTUI_CONFIG_DIR") },
        }
    }

    /// `WFTUI_MOUSE=0` is the documented way to hand every gesture back to
    /// the terminal (CLAUDE.md, and the keys card's MOUSE group). Unset — the
    /// overwhelmingly common case — must stay on.
    #[test]
    fn mouse_is_on_unless_the_env_turns_it_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("WFTUI_MOUSE") };
        assert!(mouse_enabled(), "the default is a captured mouse");
        for off in ["0", "off", "OFF", "false", "no", " 0 "] {
            unsafe { std::env::set_var("WFTUI_MOUSE", off) };
            assert!(!mouse_enabled(), "WFTUI_MOUSE={off:?} must disable the mouse");
        }
        for on in ["1", "on", "yes", ""] {
            unsafe { std::env::set_var("WFTUI_MOUSE", on) };
            assert!(mouse_enabled(), "WFTUI_MOUSE={on:?} must leave it on");
        }
        unsafe { std::env::remove_var("WFTUI_MOUSE") };
    }

    /// The updater's two knobs: `WFTUI_UPDATE_URL` replaces the feed (a
    /// test or a staging build points it at a local file server) and
    /// `WFTUI_NO_UPDATE=1` switches the whole thing off. Unset is the
    /// GitHub feed, enabled.
    #[test]
    fn update_env_overrides_read_the_feed_url_and_the_opt_out() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("WFTUI_UPDATE_URL") };
        unsafe { std::env::remove_var("WFTUI_NO_UPDATE") };
        assert_eq!(update_feed_url(), crate::update::DEFAULT_FEED_URL);
        assert!(!updates_disabled());
        unsafe { std::env::set_var("WFTUI_UPDATE_URL", " http://127.0.0.1:1/latest.json ") };
        assert_eq!(update_feed_url(), "http://127.0.0.1:1/latest.json");
        unsafe { std::env::set_var("WFTUI_UPDATE_URL", "   ") };
        assert_eq!(update_feed_url(), crate::update::DEFAULT_FEED_URL, "blank means default");
        for on in ["1", "true", "YES", " on "] {
            unsafe { std::env::set_var("WFTUI_NO_UPDATE", on) };
            assert!(updates_disabled(), "WFTUI_NO_UPDATE={on:?} must disable updates");
        }
        for off in ["0", "", "no"] {
            unsafe { std::env::set_var("WFTUI_NO_UPDATE", off) };
            assert!(!updates_disabled(), "WFTUI_NO_UPDATE={off:?} must leave them on");
        }
        unsafe { std::env::remove_var("WFTUI_UPDATE_URL") };
        unsafe { std::env::remove_var("WFTUI_NO_UPDATE") };
        assert!(update_root().ends_with("update"));
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
