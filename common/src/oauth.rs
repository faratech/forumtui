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
use rand::TryRng;
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
    fill_from_os(&mut bytes);
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
    fill_from_os(&mut bytes);
    Base64UrlUnpadded::encode_string(&bytes)
}

/// Fill `dst` straight from the OS entropy source.
///
/// rand 0.10 replaced the infallible `rngs::OsRng` with `rngs::SysRng`, which
/// only implements the fallible `TryRng`. Panicking on failure is exactly what
/// rand 0.8's `OsRng::fill_bytes` did, and it is the right behaviour here:
/// these bytes are the PKCE verifier and the OAuth state parameter, so a
/// degraded fallback would be a security bug, not a graceful degradation.
fn fill_from_os(dst: &mut [u8]) {
    rand::rngs::SysRng
        .try_fill_bytes(dst)
        .expect("the OS entropy source must be available to start a login");
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

/// `base` is the site origin (`https://windowsforum.com`, or a mock server in
/// tests). Every network call in this module takes it explicitly instead of
/// reading `config::base_url()` per call, so a caller — notably a test — can
/// prove which host it is about to talk to (issue #565).
pub async fn exchange_code(
    client: &reqwest::Client,
    base: &str,
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
    token_request(client, base, &form).await
}

pub async fn refresh(
    client: &reqwest::Client,
    base: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &config::oauth_client_id()?),
    ];
    token_request(client, base, &form).await
}

/// Revoke one token at the OAuth server.
///
/// `hint` is RFC 7009's `token_type_hint`. It matters: the endpoint defaults
/// to `access_token`, so a logout that sent only the bearer left the 90-day
/// refresh token alive — anyone with a copy of `token.json` could keep using
/// the session (issue #527). Callers revoke the refresh token *and* the
/// access token, and must not swallow the result: with the public PKCE
/// client this call is expected to fail today (stock XF's revoke endpoint
/// requires the `client_secret` a public client by definition does not
/// hold), and that gap has to be visible rather than silent. Closing it for
/// real needs a relay in the `WindowsForum/TuiLink` add-on, which is
/// server-side work and out of this client's scope.
pub async fn revoke(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    hint: Option<&str>,
) -> Result<()> {
    let client_id = config::oauth_client_id()?;
    let mut form: Vec<(&str, &str)> = vec![("token", token), ("client_id", &client_id)];
    if let Some(hint) = hint {
        form.push(("token_type_hint", hint));
    }
    let url = format!("{base}{}", config::OAUTH_REVOKE_PATH);
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
    base: &str,
    state: &str,
    challenge: &str,
) -> Result<LoginLink> {
    let url = format!("{base}/api/wf-tuilink/register");
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

pub async fn poll_link(client: &reqwest::Client, base: &str, id: &str) -> Result<PollStatus> {
    let url = format!("{base}/api/wf-tuilink/poll");
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
        // An "authorized" verdict with an empty code (the field is
        // `#[serde(default)]`) must not reach `exchange_code` — the other
        // code paths (`wait_for_redirect`, `code_from_pasted`) filter empty
        // codes, and here the right move is to keep waiting: the next poll
        // may carry it.
        "authorized" if !poll.code.is_empty() => Ok(PollStatus::Authorized(poll.code)),
        "expired" => Ok(PollStatus::Expired),
        _ => Ok(PollStatus::Waiting),
    }
}

async fn token_request(
    client: &reqwest::Client,
    base: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet> {
    let url = format!("{base}{}", config::OAUTH_TOKEN_PATH);
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
        // XenForo's API answers a rejected refresh with its own envelope,
        // not RFC 6749's `{"error": "..."}` — `{"errors":[{"code":
        // "invalid_grant","message":"…"}]}` (see
        // XF\Api\Controller\OAuth2Controller and `api::error_from_response`'s
        // identical parse for every other XF endpoint). Without this, every
        // XF-shaped rejection — including a stale/rotated refresh token,
        // the exact case that ends a session — fell through to the
        // `http_error` fallback below with the whole raw JSON body as the
        // message, and `TaskError::ends_session()` treats `http_error` as
        // non-session-ending, so the client never reached the Login screen.
        if let Some(first) = serde_json::from_slice::<crate::models::ApiErrorBody>(&body)
            .ok()
            .and_then(|b| b.errors.into_iter().next())
            .filter(|i| !i.code.is_empty())
        {
            return Err(Error::OAuth {
                code: first.code,
                message: first.message,
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
    // `expires_in` is untrusted input (#642, the #554 class): a hostile or
    // corrupt value must neither overflow `now + expires_in` (panic in
    // debug, wrap in release) nor make a nonsense-negative expiry. 90 days
    // is already the client's ceiling for any access token.
    const MAX_EXPIRES_IN_SECS: i64 = 90 * 24 * 3600;
    let expires_in = parsed.expires_in.clamp(0, MAX_EXPIRES_IN_SECS);
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_at: now.saturating_add(expires_in),
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

/// Build the (program, argv) pair used to open `url` in the user's default
/// browser. The URL always travels as a single argv element — never through
/// a shell — so `&`, `%`, `|`, `^`, `<`, `>` inside it reach the child intact
/// instead of being parsed. On Windows this is `rundll32.exe
/// url.dll,FileProtocolHandler <url>` (no `cmd /C start`, which handed the
/// whole string to cmd.exe for parsing and let it split/execute on `&`).
///
/// Deliberately not `#[cfg(target_os = ...)]`-gated so it compiles (and is
/// unit-testable) on every host, not just the one it will run on.
fn browser_command(url: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "windows") {
        (
            "rundll32.exe",
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        )
    } else if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else {
        ("xdg-open", vec![url.to_string()])
    }
}

/// Open the authorize URL in the user's browser. Fire-and-forget: we do not
/// wait for the browser process.
pub fn open_browser(url: &str) -> Result<()> {
    let (program, args) = browser_command(url);
    let mut cmd = std::process::Command::new(program);
    cmd.args(&args);
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
    fn browser_command_passes_special_chars_through_argv_intact() {
        // A URL containing `&` and `%` must reach the child process as one
        // untouched argv element on every platform — never handed to a shell
        // (cmd.exe, /bin/sh, ...) that could parse `&`/`|`/`^` inside it.
        let url = "https://example.com/?a=1&calc.exe%20oops";
        let (program, args) = browser_command(url);
        assert!(!program.is_empty());
        assert_eq!(args.len(), if cfg!(target_os = "windows") { 2 } else { 1 });
        assert_eq!(
            args.last().map(String::as_str),
            Some(url),
            "URL must survive as a single argv element, unmodified"
        );
        // Windows path specifically: rundll32 via url.dll, never cmd.exe/start.
        if cfg!(target_os = "windows") {
            assert_eq!(program, "rundll32.exe");
            assert_eq!(args[0], "url.dll,FileProtocolHandler");
        }
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

    // These token-endpoint tests take the mock server's origin as an
    // argument (issue #565), so they neither read nor write the
    // process-global `WFTUI_BASE_URL` and need no `ENV_LOCK`.

    /// A stale/rotated refresh token is rejected by XenForo's own API
    /// envelope (`{"errors":[{"code":"invalid_grant",...}]}`), not RFC
    /// 6749's `{"error": "..."}`. Before this fix that shape fell through to
    /// the `http_error` fallback with the whole JSON body as the message —
    /// and `TaskError::ends_session()` treats `code == "http_error"` as
    /// never session-ending, so a genuinely dead refresh token never sent
    /// the user back to the Login screen (the bug this test pins).
    #[tokio::test]
    async fn token_request_parses_the_xf_api_error_envelope() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(config::OAUTH_TOKEN_PATH))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errors": [{
                    "code": "invalid_grant",
                    "message": "The provided authorization code or refresh token is invalid, expired, revoked, does not match the redirection URI used in the authorization request, or was issued to another client."
                }]
            })))
            .mount(&server)
            .await;

        let client = crate::http::build().unwrap();
        let err = refresh(&client, &server.uri(), "stale-refresh-token").await.unwrap_err();
        match err {
            Error::OAuth { code, message, status } => {
                assert_eq!(code, "invalid_grant");
                assert!(message.contains("invalid"), "{message}");
                assert_eq!(status, 400);
            }
            other => panic!("expected Error::OAuth, got {other:?}"),
        }
    }

    /// The RFC 6749 shape (a real OAuth server, or a future non-XF one) must
    /// keep working — the XF-envelope parse is tried only after this one
    /// fails to match.
    #[tokio::test]
    async fn token_request_still_parses_the_rfc_error_shape() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(config::OAUTH_TOKEN_PATH))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token expired"
            })))
            .mount(&server)
            .await;

        let client = crate::http::build().unwrap();
        let err = refresh(&client, &server.uri(), "stale-refresh-token").await.unwrap_err();
        match err {
            Error::OAuth { code, message, status } => {
                assert_eq!(code, "invalid_grant");
                assert_eq!(message, "Token expired");
                assert_eq!(status, 400);
            }
            other => panic!("expected Error::OAuth, got {other:?}"),
        }
    }

    /// A non-JSON rejection (Cloudflare 5xx/WAF HTML page) must still fall
    /// back to the synthetic `"http_error"` code — that is the signal
    /// `TaskError::ends_session()` uses to keep a merely-transient failure
    /// from being mistaken for a dead grant.
    /// `expires_in` is untrusted input (#642): a hostile i64::MAX must be
    /// clamped to the client's 90-day ceiling instead of overflowing
    /// `now + expires_in`, and the result must be a sane future instant.
    #[tokio::test]
    async fn token_request_clamps_a_hostile_expires_in() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(config::OAUTH_TOKEN_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_in": 9_223_372_036_854_775_807i64,
                "scope": "node:read"
            })))
            .mount(&server)
            .await;

        let client = crate::http::build().unwrap();
        let tokens = refresh(&client, &server.uri(), "rt").await.unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(
            tokens.expires_at > now,
            "a clamped expiry is still in the future"
        );
        assert!(
            tokens.expires_at <= now + 90 * 24 * 3600 + 5,
            "the clamp caps the expiry at 90 days: {}",
            tokens.expires_at
        );
    }

    /// An "authorized" poll verdict with no code (the field defaults to
    /// empty) must keep the flow waiting, not hand an empty code to
    /// `exchange_code` (#645) — the other code paths filter empties too.
    #[tokio::test]
    async fn poll_link_keeps_waiting_when_authorized_carries_no_code() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/wf-tuilink/poll"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "status": "authorized" }),
            ))
            .mount(&server)
            .await;

        let client = crate::http::build().unwrap();
        let status = poll_link(&client, &server.uri(), "abc123").await.unwrap();
        assert_eq!(status, PollStatus::Waiting);
    }

    #[tokio::test]
    async fn token_request_falls_back_to_http_error_for_non_json_bodies() {        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(config::OAUTH_TOKEN_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(502)
                    .set_body_string("<html>bad gateway</html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&server)
            .await;

        let client = crate::http::build().unwrap();
        let err = refresh(&client, &server.uri(), "whatever").await.unwrap_err();
        match err {
            Error::OAuth { code, status, .. } => {
                assert_eq!(code, "http_error");
                assert_eq!(status, 502);
            }
            other => panic!("expected Error::OAuth, got {other:?}"),
        }
    }
}
