//! Shared HTTP client construction. One client per process: reqwest pools
//! connections, and a single pool keeps us inside Cloudflare's connection
//! expectations rather than opening a fresh TLS session per call.

use crate::config;
use crate::error::Result;

pub fn build() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(config::user_agent())
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        // Be a well-behaved API client: we speak for one user, not a scraper.
        .redirect(reqwest::redirect::Policy::limited(4))
        .build()?)
}
