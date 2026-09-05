//! OAuth 2.0 login against windowsforum.com's authorization server
//! (authorization_code + refresh_token, PKCE S256 — mandatory for public
//! clients, and the reason no client secret is embedded here).
//!
//! Flow: bind a loopback listener → open the authorize URL in the user's
//! browser → capture the redirect → exchange the code. Over SSH the redirect
//! page fails to load in the browser, so `code_from_pasted` accepts the URL
//! the user copies from the address bar instead.

use std::time::Duration;

use base64ct::{Base64UrlUnpadded, Encoding};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use time::OffsetDateTime;

use crate::config;
use crate::error::{Error, Result};
use crate::token::TokenSet;

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// RFC 7636: verifier is 43-128 chars of the unreserved set; we emit 86
/// (64 random bytes, base64url).
pub fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = Base64UrlUnpadded::encode_string(&bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = Base64UrlUnpadded::encode_string(&digest);
    Pkce {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    Base64UrlUnpadded::encode_string(&bytes)
}

fn authorize_endpoint() -> String {
    format!("{}{}", config::base_url(), config::OAUTH_AUTHORIZE_PATH)
}

/// Build the authorize URL the browser is pointed at.
pub fn authorize_url(pkce: &Pkce, state: &str, redirect_uri: &str, client_id: &str) -> String {
    let scopes = config::SCOPES.join(" ");
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorize_endpoint(),
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(&scopes),
        urlencode(state),
        urlencode(&pkce.challenge),
    )
}

/// Wait for the browser redirect on an already-bound listener and return the
/// authorization code. Rejects state mismatches (cross-request confusion) and
/// provider-reported errors.
pub async fn wait_for_redirect(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let fut = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|e| Error::Handshake(format!("listener: {e}")))?;
            let raw = read_request_head(&mut stream).await?;
            // Browser favicon/noise requests get a quiet 404 and another wait.
            let path = raw.split_whitespace().nth(1).unwrap_or("");
            if !path.starts_with("/callback") {
                respond(
                    &mut stream,
                    404,
                    "<html><body>Not the callback.</body></html>",
                )
                .await;
                continue;
            }
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let params = parse_query(query);
            if let Some(err) = params.get("error") {
                let desc = params.get("error_description").cloned().unwrap_or_default();
                respond(
                    &mut stream,
                    400,
                    "<html><body>Authorization failed; return to the terminal.</body></html>",
                )
                .await;
                return Err(Error::Handshake(format!("authorization denied: {err} {desc}")));
            }
            let state_ok = params.get("state").map(|s| s == expected_state) == Some(true);
            let code = params.get("code").cloned().filter(|c| !c.is_empty());
            match (state_ok, code) {
                (true, Some(code)) => {
                    respond(
                        &mut stream,
                        200,
                        "<html><body><h2>Login complete.</h2>Return to the terminal.</body></html>",
                    )
                    .await;
                    return Ok(code);
                }
                (false, _) => {
                    respond(
                        &mut stream,
                        400,
                        "<html><body>State mismatch; return to the terminal and retry.</body></html>",
                    )
                    .await;
                    return Err(Error::Handshake("redirect state mismatch".into()));
                }
                (true, None) => {
                    respond(
                        &mut stream,
                        400,
                        "<html><body>Missing code; return to the terminal and retry.</body></html>",
                    )
                    .await;
                    return Err(Error::Handshake("redirect carried no code".into()));
                }
            }
        }
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| Error::Handshake("timed out waiting for browser authorization".into()))?
}

/// Fallback for headless/SSH sessions: the user pastes the failed redirect
/// URL (or just `code=X&state=Y`) from their local browser's address bar.
pub fn code_from_pasted(input: &str, expected_state: &str) -> Result<String> {
    let raw = input.trim();
    let query = if let Some(pos) = raw.find('?') {
        &raw[pos + 1..]
    } else {
        raw
    };
    let params = parse_query(query);
    if let Some(err) = params.get("error") {
        return Err(Error::Handshake(format!(
            "authorization denied: {err} {}",
            params.get("error_description").cloned().unwrap_or_default()
        )));
    }
    let state_ok = params.get("state").map(|s| s == expected_state) == Some(true);
    if !state_ok {
        return Err(Error::Handshake("pasted state mismatch".into()));
    }
    params
        .get("code")
        .cloned()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Handshake("pasted value carries no code".into()))
}

pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    client_id: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    token_request(client, &form).await
}

pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &config::oauth_client_id()?),
    ];
    token_request(client, &form).await
}

pub async fn revoke(client: &reqwest::Client, token: &str) -> Result<()> {
    let form = [
        ("token", token),
        ("client_id", &config::oauth_client_id()?),
    ];
    let url = format!("{}{}", config::base_url(), config::OAUTH_REVOKE_PATH);
    client.post(url).form(&form).send().await?.error_for_status()?;
    Ok(())
}

// ---- TuiLink relay: short URL + code polling (no copy-paste) ----

#[derive(Debug, Clone)]
pub struct LoginLink {
    pub id: String,
    pub url: String,
}

pub async fn register_link(
    client: &reqwest::Client,
    state: &str,
    challenge: &str,
) -> Result<LoginLink> {
    let url = format!("{}/wf-tuilink/register", config::api_base());
    let resp = client
        .post(url)
        .json(&serde_json::json!({"state": state, "challenge": challenge}))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct Reg {
        id: String,
        url: String,
    }
    #[derive(Deserialize)]
    struct ErrBody {
        #[serde(default)]
        errors: Vec<ErrItem>,
    }
    #[derive(Deserialize)]
    struct ErrItem {
        #[serde(default)]
        message: String,
    }
    if !(200..300).contains(&status) {
        if let Ok(e) = serde_json::from_slice::<ErrBody>(&body) {
            let msg = e
                .errors
                .into_iter()
                .next()
                .map(|i| i.message)
                .unwrap_or_else(|| "register failed".into());
            return Err(Error::OAuth {
                code: "register_failed".into(),
                message: msg,
                status,
            });
        }
        return Err(Error::OAuth {
            code: "http_error".into(),
            message: String::from_utf8_lossy(&body).to_string(),
            status,
        });
    }
    let reg: Reg = serde_json::from_slice(&body)?;
    Ok(LoginLink {
        id: reg.id,
        url: reg.url,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollStatus {
    Waiting,
    Authorized(String),
    Expired,
}

pub async fn poll_link(client: &reqwest::Client, id: &str) -> Result<PollStatus> {
    let url = format!("{}/wf-tuilink/poll", config::api_base());
    let resp = client
        .post(url)
        .json(&serde_json::json!({"id": id}))
        .send()
        .await?
        .error_for_status()?;
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct Poll {
        status: String,
        #[serde(default)]
        code: String,
    }
    let poll: Poll = serde_json::from_slice(&body)?;
    match poll.status.as_str() {
        "authorized" => Ok(PollStatus::Authorized(poll.code)),
        "expired" => Ok(PollStatus::Expired),
        _ => Ok(PollStatus::Waiting),
    }
}

async fn token_request(client: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenSet> {
    let url = format!("{}{}", config::base_url(), config::OAUTH_TOKEN_PATH);
    let resp = client.post(url).form(form).send().await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?;
    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        expires_in: i64,
        #[serde(default)]
        scope: String,
    }
    #[derive(Deserialize)]
    struct OAuthErr {
        error: String,
        #[serde(default)]
        error_description: String,
    }
    if !(200..300).contains(&status) {
        if let Ok(e) = serde_json::from_slice::<OAuthErr>(&body) {
            return Err(Error::OAuth {
                code: e.error,
                message: e.error_description,
                status,
            });
        }
        return Err(Error::OAuth {
            code: "http_error".into(),
            message: String::from_utf8_lossy(&body).to_string(),
            status,
        });
    }
    let parsed: TokenResp = serde_json::from_slice(&body)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: now + parsed.expires_in,
        scope: parsed.scope,
    })
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, html: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<html><head><title>wftui</title></head><body>{html}</body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.entry(percent_decode(k))
            .or_insert_with(|| percent_decode(v));
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Open the authorize URL in the user's browser. Fire-and-forget: we do not
/// wait for the browser process.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // CRITICAL: xdg-open chains into browser-probe scripts that READ
        // stdin. Inheriting the terminal's stdin makes them eat the user's
        // keystrokes and split escape sequences (mouse events arrive
        // headless). Detach stdin fully.
        .stdin(std::process::Stdio::null());
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| Error::Handshake(format!("cannot open browser ({e}); paste the URL manually")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_matches_and_verifier_is_legal() {
        let pkce = generate_pkce();
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(
            pkce.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        );
        let expected = Base64UrlUnpadded::encode_string(&Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn authorize_url_carries_pkce_and_scopes() {
        let pkce = Pkce {
            verifier: "v".repeat(64),
            challenge: "chal".into(),
        };
        let url = authorize_url(&pkce, "state123", "http://127.0.0.1:9420/callback", "cid");
        assert!(url.starts_with(&authorize_endpoint()));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope="));
        assert!(url.contains("node%3Aread")); // colon encoded
    }

    #[test]
    fn pasted_url_yields_code_and_rejects_bad_state() {
        let url = "http://localhost:9420/callback?code=abc123&state=st1";
        assert_eq!(code_from_pasted(url, "st1").unwrap(), "abc123");
        assert!(code_from_pasted(url, "other").is_err());
        // Bare query fragment works too.
        assert_eq!(
            code_from_pasted("code=xyz&state=st1", "st1").unwrap(),
            "xyz"
        );
        // Denial surfaces as an error, not a code.
        let denied = "http://127.0.0.1:9420/callback?error=access_denied&state=st1";
        assert!(code_from_pasted(denied, "st1").is_err());
    }

    #[test]
    fn percent_decode_handles_hex_and_passthrough() {
        assert_eq!(percent_decode("node%3Aread"), "node:read");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[tokio::test]
    async fn loopback_listener_captures_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(wait_for_redirect(
            listener,
            "st9",
            Duration::from_secs(10),
        ));
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(b"GET /callback?code=thecode&state=st9 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let code = waiter.await.unwrap().unwrap();
        assert_eq!(code, "thecode");
    }

    #[tokio::test]
    async fn loopback_listener_rejects_state_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(wait_for_redirect(
            listener,
            "good",
            Duration::from_secs(10),
        ));
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(b"GET /callback?code=x&state=evil HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }
}
