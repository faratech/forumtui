//! Shared HTTP client construction. `build()` bakes in the UA and timeouts
//! every caller must use; the pool it hands back is what steady-state traffic
//! rides. In practice the long-lived client is the `WfApiClient`'s — one
//! pooled client keeps us inside Cloudflare's connection expectations rather
//! than opening a fresh TLS session per call — but the process is not limited
//! to exactly one: the login flow and the logout revokes build short-lived
//! clients of their own, deliberately outside the session's pool (#661).

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

/// The built-in site's client (its UA names windowsforum.com — hard rule 6).
/// The updater and the login/logout flows for that site use it.
pub fn build() -> Result<reqwest::Client> {
    build_with_ua(&config::user_agent())
}

/// A client wearing a site's own UA (`site::SiteConfig::user_agent`): the
/// same distinctive `wftui/<ver>` prefix, never a library default.
pub fn build_with_ua(ua: &str) -> Result<reqwest::Client> {
    install_crypto_provider();
    Ok(reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        // Be a well-behaved API client: we speak for one user, not a scraper.
        .redirect(redirect_policy())
        .build()?)
}


/// Follow up to four redirects, but only through a scheme we would have
/// requested in the first place: https, or http to an exact loopback host
/// (the wiremock tests, a local dev forum). The stock `Policy::limited`
/// follows every hop blindly, so an https URL that 302s to http:// would
/// silently downgrade the channel — for the updater that channel is asset
/// integrity: `fetch_capped` gates the initial URL (#775); the per-hop gate
/// is #818. The host here is the URL's *parsed* host, so userinfo and suffix
/// tricks (`127.0.0.1@evil.com`, `127.0.0.1.evil.com`) cannot pass — the
/// same exact-match rule as update.rs's `is_loopback_host` (#68).
fn redirect_policy() -> reqwest::redirect::Policy {
    const MAX_HOPS: usize = 4;
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_HOPS {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let loopback = url.host_str().is_some_and(|h| {
            matches!(
                h.to_ascii_lowercase().as_str(),
                "127.0.0.1" | "localhost" | "::1"
            )
        });
        if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
            attempt.follow()
        } else {
            attempt.error("refusing to follow a redirect to a non-https, non-loopback URL")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // #818: every redirect hop passes the same scheme gate as the initial
    // fetch — https, or http to an exact loopback host. The stock limited(4)
    // policy this replaced followed an https→http downgrade without a word.
    #[tokio::test]
    async fn redirect_to_plain_http_off_loopback_is_refused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", "http://example.invalid/downgrade"),
            )
            .mount(&server)
            .await;
        let client = build().unwrap();
        let err = client.get(server.uri()).send().await.unwrap_err();
        // reqwest wraps a policy rejection as "error following redirect for
        // url …"; the gate's own message sits further down the source chain.
        assert!(err.is_redirect(), "expected a redirect error, got: {err}");
        let mut saw_gate = false;
        let mut source = std::error::Error::source(&err);
        while let Some(e) = source {
            saw_gate |= e.to_string().contains("non-https");
            source = e.source();
        }
        assert!(saw_gate, "policy gate not in the error chain of: {err}");
    }

    #[tokio::test]
    async fn redirect_between_loopback_hosts_still_follows() {
        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&target)
            .await;
        let hop = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/final", target.uri())),
            )
            .mount(&hop)
            .await;
        let client = build().unwrap();
        let resp = client.get(hop.uri()).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }
}
