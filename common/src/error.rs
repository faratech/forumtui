//! Error type. House style (services/mirror): a plain enum with hand-written
//! `Display`/`From`, no anyhow/thiserror.

use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Json(serde_json::Error),
    /// The API answered with `{"errors": [...]}`.
    Api {
        code: String,
        message: String,
        status: u16,
        /// `invalid_page` carries `{"max": N}` — callers clamp to it.
        max_page: Option<u32>,
    },
    /// OAuth token endpoint refused the grant/refresh.
    OAuth {
        code: String,
        message: String,
        status: u16,
    },
    NoToken,
    /// Our client-side throttle gate told the caller to back off.
    Throttled,
    RateLimited {
        retry_after: Option<Duration>,
    },
    Config(String),
    Io(std::io::Error),
    TokenStore(String),
    /// Loopback redirect listener failed or timed out.
    Handshake(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(e) => write!(f, "http error: {e}"),
            Error::Json(e) => write!(f, "bad json: {e}"),
            Error::Api {
                code,
                message,
                status,
                max_page: _,
            } => write!(f, "api error [{code}] (http {status}): {message}"),
            Error::OAuth {
                code,
                message,
                status,
            } => write!(f, "oauth error [{code}] (http {status}): {message}"),
            Error::NoToken => write!(f, "not logged in"),
            Error::Throttled => write!(f, "client-side throttle: slow down"),
            Error::RateLimited {
                retry_after: Some(d),
            } => write!(f, "rate limited, retry in {}s", d.as_secs().max(1)),
            Error::RateLimited { retry_after: None } => write!(f, "rate limited"),
            Error::Config(m) => write!(f, "configuration: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::TokenStore(m) => write!(f, "token store: {m}"),
            Error::Handshake(m) => write!(f, "login handshake: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_user_facing() {
        let e = Error::Api {
            code: "missing_scope".into(),
            message: "Scope required".into(),
            status: 403,
            max_page: None,
        };
        let s = e.to_string();
        assert!(s.contains("missing_scope") && s.contains("403"), "{s}");
    }
}
