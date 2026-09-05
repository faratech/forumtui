//! Shared HTTP client construction. One client per process: reqwest pools
//! connections, and a single pool keeps us inside Cloudflare's connection
//! expectations rather than opening a fresh TLS session per call.

use crate::config;
use crate::error::Result;

/// Install *ring* as the process-wide rustls crypto provider, once.
///
/// reqwest 0.13's `rustls-no-provider` feature deliberately ships no provider:
/// `ClientBuilder::build` calls `CryptoProvider::get_default()` and panics with
/// "No provider set" if nothing installed one. We take that feature (rather
/// than plain `rustls`) to avoid aws-lc-rs's vendored C library — see the note
/// in Cargo.toml — so installing the provider is our job. `install_default`
/// returns `Err` when a provider is already set, which is exactly the no-op we
/// want on the second and later calls.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn build() -> Result<reqwest::Client> {
    install_crypto_provider();
    Ok(reqwest::Client::builder()
        .user_agent(config::user_agent())
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        // Be a well-behaved API client: we speak for one user, not a scraper.
        .redirect(reqwest::redirect::Policy::limited(4))
        .build()?)
}
