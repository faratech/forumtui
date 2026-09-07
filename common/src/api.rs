//! The API seam. `WfApi` is the trait every screen consumes; `WfApiClient` is
//! the real implementation over reqwest with Bearer auth, silent token
//! refresh, and the politeness gates. Tests substitute their own `WfApi`.
//!
//! (Single file by necessity: a trait impl cannot be split across modules.)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::config;
use crate::error::{Error, Result};
use crate::models::*;
use crate::oauth;
use crate::ratelimit::Gate;
use crate::token::{self, TokenSet};

pub struct WfApiClient {
    http: reqwest::Client,
    tokens: Mutex<Option<TokenSet>>,
    store: token::Store,
    /// Site origin, captured at construction rather than read from the
    /// process environment per call (issue #565): a client can then be
    /// pointed somewhere harmless for a whole test regardless of what any
    /// other thread is doing to `WFTUI_BASE_URL`.
    base: String,
    pub api_gate: Arc<Gate>,
    pub search_gate: Arc<Gate>,
    pub write_gate: Arc<Gate>,
    /// Decoration only (`fetch_bytes`): a separate lane so a screenful of
    /// thumbnails can never sit in front of the request the user is waiting
    /// on (issue #543).
    pub image_gate: Arc<Gate>,
}

impl WfApiClient {
    /// The production client: the user's own token store and the configured
    /// site origin.
    pub fn new() -> Result<Self> {
        Self::with_store(token::Store::new(), config::base_url())
    }

    /// A client bound to an explicit token store and origin.
    ///
    /// Both are resolved here, once, instead of from `WFTUI_CONFIG_DIR` /
    /// `WFTUI_BASE_URL` on every call. That is what lets a test build a
    /// client that provably cannot read the machine owner's
    /// `~/.config/wftui/token.json` or send a request to the live site —
    /// issue #565, where `test_app()` built a real `WfApiClient::new()`, one
    /// test really `GET /api/me`'d windowsforum.com with the operator's own
    /// bearer, and the fixture store overwrote the real `token.json`.
    pub fn with_store(store: token::Store, base_url: impl Into<String>) -> Result<Self> {
        // A store that fails to *parse* (hand-edited, truncated, or written
        // by a build whose `TokenSet` shape has since changed) is not a
        // reason to refuse to start: quarantine it and begin as if there
        // were no session, rather than bricking every existing install on a
        // schema change (issue #546). Any other error (I/O, permissions)
        // still propagates.
        let tokens = match store.load() {
            Ok(t) => t,
            Err(Error::TokenStore(msg)) => {
                tracing::warn!("{msg} — starting a fresh session");
                store.quarantine_corrupt();
                None
            }
            Err(e) => return Err(e),
        };
        Ok(WfApiClient {
            http: crate::http::build()?,
            tokens: Mutex::new(tokens),
            store,
            base: base_url.into(),
            api_gate: Arc::new(Gate::new(config::GLOBAL_MIN_INTERVAL_MS)),
            search_gate: Arc::new(Gate::new(config::SEARCH_MIN_INTERVAL_MS)),
            write_gate: Arc::new(Gate::new(config::WRITE_COOLDOWN_MS)),
            image_gate: Arc::new(Gate::new(config::IMAGE_MIN_INTERVAL_MS)),
        })
    }

    /// The origin this client talks to (`https://windowsforum.com` in
    /// production). Callers that need the OAuth endpoints — the login flow
    /// and logout's revoke — take it from here so they can never disagree
    /// with the client about which site the session belongs to.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The file this client's session is persisted in.
    pub fn store_path(&self) -> &std::path::Path {
        self.store.path()
    }

    fn api_base(&self) -> String {
        format!("{}/api", self.base)
    }

    /// Adopt a freshly obtained token set (login/refresh) and persist it.
    pub async fn set_tokens(&self, tokens: TokenSet) -> Result<()> {
        let mut guard = self.tokens.lock().await;
        self.store.save(&tokens)?;
        *guard = Some(tokens);
        Ok(())
    }

    pub async fn has_tokens(&self) -> bool {
        self.tokens.lock().await.is_some()
    }

    /// A copy of the stored token set. Logout needs the refresh token (not
    /// just the bearer `valid_token` hands out) so it can revoke both.
    pub async fn token_set(&self) -> Option<TokenSet> {
        self.tokens.lock().await.clone()
    }

    /// Re-read the token store and adopt a token set some *other* instance
    /// wrote. XF rotates the refresh token on every refresh, so two clients
    /// sharing one config dir (a tmux session on the server plus a local
    /// one) invalidate each other's in-memory grant; the surviving token is
    /// the one on disk. Returns `true` only when the file genuinely holds
    /// something this process does not already have — so a caller can use it
    /// as "is there a session to recover?" without ever looping (issue #557).
    pub async fn adopt_stored_tokens(&self) -> bool {
        let stored = match self.store.load() {
            Ok(Some(t)) => t,
            _ => return false,
        };
        let mut guard = self.tokens.lock().await;
        if guard.as_ref().is_some_and(|cur| {
            cur.refresh_token == stored.refresh_token && cur.access_token == stored.access_token
        }) {
            return false;
        }
        *guard = Some(stored);
        true
    }

    pub async fn forget_tokens(&self) -> Result<()> {
        self.tokens.lock().await.take();
        self.store.erase()
    }

    /// Snapshot the token set and end the local session in one atomic step:
    /// the in-memory grant is taken and the store erased while the token
    /// mutex is still held. Logout needs both — the snapshot so it can revoke
    /// (it needs the refresh token, not just the bearer), and the forget so
    /// nothing live survives locally — and `token_set()` + `forget_tokens()`
    /// left an `await` gap between them: a poller-driven `valid_token()`
    /// rotating the grant inside that gap made logout revoke the already-dead
    /// old refresh token while erasing the brand-new 90-day grant, leaving a
    /// live, unrevoked, locally-unstored session behind. Holding the lock
    /// across both also makes this wait for an in-flight refresh to finish,
    /// so the snapshot it hands back is whatever grant is newest when logout
    /// wins the lock — revoke that, and nothing live remains anywhere.
    pub async fn take_tokens(&self) -> Result<Option<TokenSet>> {
        let mut guard = self.tokens.lock().await;
        let tokens = guard.take();
        self.store.erase()?;
        Ok(tokens)
    }

    /// Current token, refreshed silently if expired. Refreshes under the lock
    /// so concurrent calls collapse into one network round-trip.
    ///
    /// A refresh rejected with `invalid_grant` is not automatically the end
    /// of the session: XF rotates the refresh token on every refresh, so a
    /// sibling instance sharing the config dir (a tmux session on the
    /// server plus a local one) invalidates this process's in-memory grant
    /// while leaving a perfectly good token set on disk — but that set may
    /// belong to a *different account* (issue #573: the store's `TokenSet`
    /// carries no user id, so this client cannot tell), and this call has no
    /// way to refresh `App::me`, the header, or any "as <user>" assumption
    /// before its caller's next write goes out. So a different token set on
    /// disk is no longer adopted here: the in-memory session is cleared and
    /// `Error::NoToken` is returned, routing through the app-level boundary
    /// (`recheck_stored_session` -> `adopt_stored_tokens` -> `/me` ->
    /// `Msg::Bootstrap`) that already exists for exactly this recovery and
    /// always refreshes `App::me` before anything else runs (issue
    /// #568/#557). The store itself is left untouched in that case — it may
    /// hold a sibling's (or a different account's) perfectly good session,
    /// and only the app-level recheck may adopt it. Only when there is
    /// nothing newer on disk (the store still holds the very token that was
    /// just rejected, or nothing at all) does the session end outright, with
    /// the original error returned and the dead token erased.
    pub async fn valid_token(&self) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        let existing = guard.as_ref().ok_or(Error::NoToken)?.clone();
        if !existing.access_expired(OffsetDateTime::now_utc()) {
            return Ok(existing.access_token);
        }
        if existing.refresh_token.is_empty() {
            *guard = None;
            let _ = self.store.erase();
            return Err(Error::NoToken);
        }
        let refresh_token = existing.refresh_token.clone();
        // The refresh POST is a second round-trip to the same origin and
        // goes through the api_gate like every other request (#641): the
        // caller consumed a slot for its own call, but this one is
        // additional traffic the origin can see. The tokens lock is already
        // held across the await by design, so this only ever delays
        // queued token work by the gate's spacing.
        self.api_gate.wait().await;
        let err = match oauth::refresh(&self.http, &self.base, &refresh_token).await {
            Ok(refreshed) => return Ok(self.keep_refreshed(&mut guard, refreshed)),
            Err(e) => e,
        };
        if !matches!(&err, Error::OAuth { code, .. } if code == "invalid_grant") {
            return Err(err);
        }
        // Rejected. Check the store for a different token set before ending
        // the session outright — but never adopt it here (see doc comment
        // above); that identity re-check belongs to the app-level recheck.
        let sibling = self
            .store
            .load()
            .ok()
            .flatten()
            .filter(|t| t.refresh_token != refresh_token && !t.refresh_token.is_empty());
        if sibling.is_some() {
            *guard = None;
            return Err(Error::NoToken);
        }
        // Nothing newer to recover from: the session really is over.
        self.forget_rejected(&mut guard, &refresh_token);
        Err(err)
    }

    /// Install a freshly refreshed token set as the live session and hand
    /// back its access token.
    ///
    /// The live session is updated first: XF's refresh grant rotates the
    /// refresh token (revokes the old one server-side in the same request),
    /// so once `oauth::refresh` has succeeded the old token set is already
    /// dead. If persisting the new one to disk then fails (ENOSPC, a
    /// permission change, a read-only remount), that must cost only
    /// durability across a restart, not the live session — demoting it to a
    /// warning avoids stranding the revoked refresh token in the guard/the
    /// store, which would otherwise force a full browser re-login on the
    /// very next call (see issue #513).
    fn keep_refreshed(&self, guard: &mut Option<TokenSet>, refreshed: TokenSet) -> String {
        let access = refreshed.access_token.clone();
        *guard = Some(refreshed.clone());
        if let Err(e) = self.store.save(&refreshed) {
            tracing::warn!("token refresh persisted in memory but not to disk: {e}");
        }
        access
    }

    /// Drop a refresh token the server has rejected for good: clear the live
    /// session, and erase the store only if it still holds that very token
    /// (never a set some other code path has since written).
    fn forget_rejected(&self, guard: &mut Option<TokenSet>, rejected: &str) {
        if self
            .store
            .load()
            .ok()
            .flatten()
            .is_some_and(|t| t.refresh_token == rejected)
        {
            let _ = self.store.erase();
        }
        *guard = None;
    }

    /// A 429's `Retry-After` (or, absent one, a conservative fallback) must
    /// actually extend the gate it came from — otherwise the very next call
    /// is spaced only by the normal politeness slot and can hit the same
    /// ceiling again (issue #514). `gates` is every gate the failing call
    /// consumed, so a rate limit on a write path extends both the global and
    /// write cool-downs.
    fn note_rate_limit(&self, err: &Error, gates: &[&Gate]) {
        if let Error::RateLimited { retry_after } = err {
            let dur =
                retry_after.unwrap_or(Duration::from_secs(config::DEFAULT_RATE_LIMIT_RETRY_SECS));
            for gate in gates {
                gate.penalize(dur);
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}{path}", self.api_base());
        let resp = self.http.get(url).query(query).bearer_auth(&token).send().await?;
        decode(resp).await.inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate]))
    }

    /// Flood-checked write (posts/threads/conversations): consumes the write
    /// gate and re-arms its cool-down on success.
    async fn post_form<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, String)],
        success_penalty: Option<Duration>,
    ) -> Result<T> {
        self.api_gate.wait().await;
        self.write_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}{path}", self.api_base());
        let resp = self.http.post(url).form(form).bearer_auth(&token).send().await?;
        let out = decode(resp).await;
        if out.is_ok() {
            self.write_gate.penalize(
                success_penalty.unwrap_or(Duration::from_millis(config::WRITE_COOLDOWN_MS)),
            );
        } else if let Err(e) = &out {
            self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]);
        }
        out
    }

    /// Unit-style POST (mark-read and friends): bookkeeping, not a
    /// flood-checked write — global gate only.
    async fn post_unit(&self, path: &str, form: &[(&str, String)]) -> Result<()> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}{path}", self.api_base());
        let resp = self.http.post(url).form(form).bearer_auth(&token).send().await?;
        check_status(resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]))
    }

    /// A POST to one of XF's *toggle* endpoints (react, vote). Same gating as
    /// `post_unit`, but the reply body is what says which way the toggle went,
    /// so it is decoded instead of discarded (issue #538).
    async fn post_toggle(&self, path: &str, form: &[(&str, String)]) -> Result<Toggle> {
        #[derive(serde::Deserialize)]
        struct ToggleReply {
            #[serde(default)]
            action: String,
        }
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}{path}", self.api_base());
        let resp = self.http.post(url).form(form).bearer_auth(&token).send().await?;
        let reply: ToggleReply = decode(resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]))?;
        Ok(Toggle::from_action(&reply.action))
    }

    async fn post_unit_path(&self, path: &str) -> Result<()> {
        self.post_unit(path, &[]).await
    }

    // ---- attachments (inherent; not part of the trait) ----

    /// Upload one file and return it **with the key it was uploaded under**
    /// (#709).
    ///
    /// The key is the whole point: an upload is only attached to anything
    /// once a write carries the same key, and further files can be uploaded
    /// under it. Returning only the `Attachment` — as this did until now —
    /// left the caller holding a file nothing could reference.
    ///
    /// Pass `existing_key` to add a file to a key already minted, so one
    /// post's attachments share one key.
    pub async fn upload_attachment(
        &self,
        content_type: &str,
        context: &[(&str, String)],
        filename: String,
        bytes: Vec<u8>,
        mime: &str,
        existing_key: Option<&str>,
    ) -> Result<(String, Attachment)> {
        self.api_gate.wait().await;
        self.write_gate.wait().await;
        let token = self.valid_token().await?;

        // One key per post: mint it on the first file, reuse it after.
        let key = match existing_key {
            Some(k) => k.to_string(),
            None => {
                let mut form: Vec<(&str, String)> = vec![("type", content_type.to_string())];
                for (k, v) in context {
                    form.push((k, v.clone()));
                }
                let key_url = format!("{}/attachments/new-key", self.api_base());
                let key_resp = self
                    .http
                    .post(key_url)
                    .form(&form)
                    .bearer_auth(&token)
                    .timeout(config::UPLOAD_TIMEOUT)
                    .send()
                    .await?;
                #[derive(serde::Deserialize)]
                struct NewKey {
                    #[serde(alias = "attachment_key")]
                    key: String,
                }
                let new_key: NewKey = decode(key_resp)
                    .await
                    .inspect_err(|e| {
                        self.note_rate_limit(e, &[&self.api_gate, &self.write_gate])
                    })?;
                new_key.key
            }
        };

        self.api_gate.wait().await;
        let upload_url = format!("{}/attachments/", self.api_base());
        let file_part =
            reqwest::multipart::Part::bytes(bytes).file_name(filename).mime_str(mime)?;
        let form = reqwest::multipart::Form::new()
            .text("key", key.clone())
            .part("attachment", file_part);
        let resp = self
            .http
            .post(upload_url)
            .multipart(form)
            .bearer_auth(&token)
            .timeout(config::UPLOAD_TIMEOUT)
            .send()
            .await?;
        // The envelope is `{"attachment": {...}}` — decoding it as a bare
        // `Attachment` yielded a silently BLANK one, because every field on
        // that model is `#[serde(default)]` (#709). A tolerant model turns a
        // wrong shape into empty data rather than an error, so the shape has
        // to be right.
        #[derive(serde::Deserialize)]
        struct Uploaded {
            #[serde(default)]
            attachment: Attachment,
        }
        let out: Result<Uploaded> = decode(resp).await;
        let out = out.map(|u| (key, u.attachment));
        if out.is_ok() {
            // Anchor the following post's cool-down here, like `post_form`
            // does: the attachment is a flood-checked write, so the reply it
            // belongs to must not find the write gate slot wide open after a
            // slow upload ate the last one (#644).
            self.write_gate
                .penalize(Duration::from_millis(config::WRITE_COOLDOWN_MS));
        } else if let Err(e) = &out {
            self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]);
        }
        out
    }

    /// Fetch attachment bytes (the TUI writes them to a temp file and opens).
    ///
    /// The body is capped the way `fetch_bytes` caps thumbnails (#526's
    /// streaming discipline): refused at `Content-Length` and again at the
    /// running total — the old `resp.bytes().await` buffered a hostile or
    /// simply enormous attachment whole before anything looked at it. A 429
    /// classifies through the same path as every other call, so
    /// `Retry-After` reaches the gates instead of dying as a plain
    /// `Error::Http` (#643).
    pub async fn attachment_data(&self, attachment_id: u32, max_bytes: usize) -> Result<Vec<u8>> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/attachments/{attachment_id}/data", self.api_base());
        let mut resp = self.http.get(&url).bearer_auth(&token).send().await?;
        if !resp.status().is_success() {
            let err = error_from_response(resp).await;
            self.note_rate_limit(&err, &[&self.api_gate]);
            return Err(err);
        }
        if let Some(len) = resp.content_length()
            && len as usize > max_bytes
        {
            return Err(Error::FetchRejected(format!(
                "Content-Length {len} exceeds cap {max_bytes} for {url}"
            )));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            buf.extend_from_slice(&chunk);
            if buf.len() > max_bytes {
                return Err(Error::FetchRejected(format!(
                    "response body exceeds cap {max_bytes} for {url}"
                )));
            }
        }
        Ok(buf)
    }

    /// Fetch raw bytes from an absolute URL — the images tier
    /// (`wftui/src/images.rs`) uses this for `thumbnail_url`/`direct_url`/
    /// `avatar_urls` values off `Attachment`/`User`. Inherent, not on
    /// `WfApi`: same reasoning as `attachment_data` above — this is a raw-HTTP
    /// concern, not a forum-data operation the trait's mock implementations
    /// need to fake.
    ///
    /// Goes through `image_gate`, NOT `api_gate`: same 250 ms spacing, but a
    /// lane of its own, because `Gate` hands out slots first-come-first-served
    /// and one cold thread open reserves a slot per visible avatar/thumbnail —
    /// on the shared gate that put ~2.5 s of decoration in front of the user's
    /// next navigation (issue #543). The request carries the client UA because
    /// it's the same
    /// pooled `reqwest::Client` every other method uses — `http::build()`
    /// bakes `config::user_agent()` in at construction (hard rule 6), so
    /// there is nothing extra to set per-request.
    ///
    /// Refuses anything whose `Content-Type` doesn't start with `image/`, and
    /// caps the body at `max_bytes`: first cheaply, via `Content-Length` if
    /// the server sent one; then for real, by streaming the body in chunks
    /// and aborting the instant the running total exceeds the cap — a
    /// chunked response with no (or a lying) `Content-Length` is never
    /// buffered past `max_bytes` in memory, unlike `resp.bytes().await`
    /// which reads the whole thing first regardless of what it decides to
    /// do with it afterwards (issue #526).
    pub async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
        self.image_gate.wait().await;
        // Media served by the API itself (`/api/media/{id}/data`, the
        // gallery's full-size bytes) needs the bearer token; anything else is
        // a plain data-host or third-party URL and must NOT see it. The test
        // `fetch_bytes_sends_the_token_only_to_our_own_api` pins both halves
        // — an image URL is attacker-influenced (a post can carry any
        // `[IMG]`), so a looser rule would hand the grant to whoever asked.
        let mut req = self.http.get(url);
        if url.starts_with(&format!("{}/", self.api_base())) {
            req = req.bearer_auth(self.valid_token().await?);
        }
        let mut resp = req.send().await?.error_for_status()?;

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !content_type.starts_with("image/") {
            return Err(Error::FetchRejected(format!(
                "refusing non-image content-type {content_type:?} from {url}"
            )));
        }

        if let Some(len) = resp.content_length()
            && len as usize > max_bytes
        {
            return Err(Error::FetchRejected(format!(
                "Content-Length {len} exceeds cap {max_bytes} for {url}"
            )));
        }

        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            buf.extend_from_slice(&chunk);
            if buf.len() > max_bytes {
                return Err(Error::FetchRejected(format!(
                    "response body exceeds cap {max_bytes} for {url}"
                )));
            }
        }
        Ok(buf)
    }
}

pub(crate) async fn check_status(resp: reqwest::Response) -> Result<()> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    Err(error_from_response(resp).await)
}

pub(crate) async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(error_from_response(resp).await);
    }
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(Error::from)
}

async fn error_from_response(resp: reqwest::Response) -> Error {
    let status = resp.status().as_u16();
    if status == 429 {
        // A hostile or broken origin can send an arbitrarily large
        // `Retry-After` (up to 20 digits); clamp it to a sane ceiling so it
        // can never overflow `Instant + Duration` downstream in
        // `Gate::penalize` (issue #554) — only a real origin misbehaving
        // this badly would ever hit the clamp at all.
        const MAX_RETRY_AFTER_SECS: u64 = 3600;
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)));
        let _ = resp.bytes().await; // drain politely; shape not needed
        return Error::RateLimited { retry_after };
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    if let Some(first) = serde_json::from_slice::<ApiErrorBody>(&bytes)
        .ok()
        .and_then(|parsed| parsed.errors.into_iter().next())
    {
        let max_page = first.params.get("max").and_then(|v| v.as_u64()).map(|v| v as u32);
        return Error::Api {
            code: first.code,
            message: first.message,
            status,
            max_page,
        };
    }
    Error::Api {
        code: "http_error".into(),
        message: String::from_utf8_lossy(&bytes).to_string(),
        status,
        max_page: None,
    }
}

/// XF's plain `/nodes` endpoint never emits `depth` (only
/// `/nodes/flattened` does, in an incompatible envelope — see issue #509).
/// Derive it locally by walking each node's `parent_node_id` chain, bounded
/// and cycle-guarded so a malformed tree can never spin or panic.
fn derive_node_depths(mut nodes: Vec<Node>) -> Vec<Node> {
    use std::collections::HashMap;

    let parent_of: HashMap<u32, u32> = nodes.iter().map(|n| (n.node_id, n.parent_node_id)).collect();

    for node in &mut nodes {
        let mut depth = 0u32;
        let mut current = node.node_id;
        let mut seen = std::collections::HashSet::new();
        seen.insert(current);
        while let Some(&parent) = parent_of.get(&current) {
            if parent == 0 || !seen.insert(parent) {
                break;
            }
            depth += 1;
            current = parent;
            if depth >= 64 {
                break;
            }
        }
        node.depth = depth;
    }
    nodes
}

/// XF's reaction and content-vote endpoints are **toggles**: posting the
/// `reaction_id`/`type` that is already set removes it, and the reply says
/// which of the two happened (`{"success":true,"action":"insert"|"delete"}` —
/// `ReactionPlugin::actionReact` / `ContentVotePlugin::actionVote`). The
/// client has to read that or it announces "liked" for an unlike (issue #538).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// The reaction/vote was added.
    Inserted,
    /// It was already there and has now been removed.
    Removed,
}

impl Toggle {
    /// Map the reply's `action`. Anything that is not an explicit `delete`
    /// (including a body that omits the key) counts as an insert, which is the
    /// behaviour this client had before it read the body at all.
    pub fn from_action(action: &str) -> Toggle {
        if action.trim().eq_ignore_ascii_case("delete") {
            Toggle::Removed
        } else {
            Toggle::Inserted
        }
    }
}

#[async_trait]
pub trait WfApi: Send + Sync {
    async fn nodes(&self) -> Result<Vec<Node>>;
    async fn forum(&self, node_id: u32, page: u32) -> Result<ForumReply>;
    async fn threads(&self, page: u32) -> Result<ThreadsReply>;
    async fn thread(&self, id: u32, page: u32) -> Result<ThreadReply>;
    async fn thread_posts(&self, id: u32, page: u32) -> Result<PostsReply>;
    /// `attachment_key` ties files already uploaded under that key to this
    /// post (#709). XF requires the key's context to match: `thread_id` for
    /// a reply, `node_id` for a new thread, `post_id` for an edit.
    async fn reply(&self, thread_id: u32, message: &str, attachment_key: Option<&str>)
        -> Result<Post>;
    async fn create_thread(
        &self,
        node_id: u32,
        title: &str,
        message: &str,
        attachment_key: Option<&str>,
    ) -> Result<Thread>;
    /// Mark a thread read up to `date` (a post's timestamp), or to now when
    /// `None`. XF refuses to move the marker backwards, so a partial read
    /// marks partially and re-reading is idempotent (#694).
    async fn mark_thread_read(&self, id: u32, date: Option<i64>) -> Result<()>;
    /// Mark every alert *viewed* — what XF's own web UI does when the alerts
    /// list is shown: it clears the counter and leaves unactioned alerts
    /// highlighted (#694).
    async fn mark_alerts_viewed(&self) -> Result<()>;
    /// Edit a post's message (#708). `POST /posts/{id}`.
    async fn edit_post(&self, id: u32, message: &str, attachment_key: Option<&str>)
        -> Result<()>;
    /// Delete a post — soft by default, which is what XF's own UI does and
    /// what leaves the content recoverable (#708).
    async fn delete_post(&self, id: u32, hard: bool) -> Result<()>;
    /// Toggle a post as its thread's solution (#708). XF unmarks whatever
    /// was marked before, so this is a toggle, not a set.
    async fn mark_solution(&self, id: u32) -> Result<()>;
    async fn mark_forum_read(&self, node_id: u32) -> Result<()>;
    async fn conversations(&self, page: u32) -> Result<ConversationsReply>;
    async fn conversation(&self, id: u32, page: u32) -> Result<ConversationReply>;
    async fn reply_conversation(&self, id: u32, message: &str) -> Result<()>;
    async fn create_conversation(
        &self,
        recipient_ids: &[u32],
        title: &str,
        message: &str,
    ) -> Result<Conversation>;
    async fn mark_conversation_read(&self, id: u32) -> Result<()>;
    async fn delete_conversation(&self, id: u32) -> Result<()>;
    async fn alerts(&self, page: u32) -> Result<AlertsReply>;
    async fn mark_alert_read(&self, id: u32) -> Result<()>;
    async fn search(&self, keywords: &str, page: u32) -> Result<SearchResultsReply>;
    /// XFMG's media list (`GET /api/media/`, issue #680). `category` scopes
    /// it to one gallery category, the way the site's category page does
    /// (`GET /api/media-categories/{id}/content`, issue #697).
    async fn media_list(&self, category: Option<u32>, page: u32) -> Result<MediaListReply>;
    /// The gallery's category tree (`GET /api/media-categories/`, #697).
    async fn media_categories(&self) -> Result<MediaCategoriesReply>;
    /// One media item with its description and dimensions (#697).
    async fn media_item(&self, id: u32) -> Result<MediaItemReply>;
    /// XFRM's resource list (`GET /api/resources/`, issue #680).
    async fn resources_list(&self, page: u32) -> Result<ResourceListReply>;
    /// One resource, with the BBCode body the in-client page renders (#697).
    async fn resource(&self, id: u32) -> Result<ResourceReply>;
    async fn search_advanced(&self, query: &SearchQuery) -> Result<SearchResultsReply>;
    async fn search_member(
        &self,
        user_id: u32,
        content: &str,
        page: u32,
    ) -> Result<SearchResultsReply>;
    async fn react_post(&self, post_id: u32, reaction_id: u32) -> Result<Toggle>;
    async fn vote_post(&self, post_id: u32, vote: &str) -> Result<Toggle>;
    async fn me(&self) -> Result<User>;
    async fn user(&self, id: u32) -> Result<User>;
    async fn find_user(&self, username: &str) -> Result<Option<User>>;
}

#[async_trait]
impl WfApi for WfApiClient {
    async fn nodes(&self) -> Result<Vec<Node>> {
        let reply: NodesReply = self.get("/nodes", &[]).await?;
        Ok(derive_node_depths(reply.nodes))
    }

    async fn forum(&self, node_id: u32, page: u32) -> Result<ForumReply> {
        self.get(
            &format!("/forums/{node_id}"),
            &[("with_threads", "1".into()), ("page", page.to_string())],
        )
        .await
    }

    async fn threads(&self, page: u32) -> Result<ThreadsReply> {
        self.get("/threads", &[("page", page.to_string())]).await
    }

    async fn thread(&self, id: u32, page: u32) -> Result<ThreadReply> {
        self.get(
            &format!("/threads/{id}"),
            &[("with_posts", "1".into()), ("page", page.to_string())],
        )
        .await
    }

    async fn thread_posts(&self, id: u32, page: u32) -> Result<PostsReply> {
        self.get(&format!("/threads/{id}/posts"), &[("page", page.to_string())])
            .await
    }

    async fn reply(
        &self,
        thread_id: u32,
        message: &str,
        attachment_key: Option<&str>,
    ) -> Result<Post> {
        #[derive(serde::Deserialize)]
        struct PostCreated {
            post: Post,
        }
        let mut form = vec![
            ("thread_id", thread_id.to_string()),
            ("message", message.to_string()),
        ];
        if let Some(key) = attachment_key {
            form.push(("attachment_key", key.to_string()));
        }
        let created: PostCreated = self.post_form("/posts", &form, None).await?;
        Ok(created.post)
    }

    async fn create_thread(
        &self,
        node_id: u32,
        title: &str,
        message: &str,
        attachment_key: Option<&str>,
    ) -> Result<Thread> {
        #[derive(serde::Deserialize)]
        struct ThreadCreated {
            thread: Thread,
        }
        let mut form = vec![
            ("node_id", node_id.to_string()),
            ("title", title.to_string()),
            ("message", message.to_string()),
        ];
        if let Some(key) = attachment_key {
            form.push(("attachment_key", key.to_string()));
        }
        let created: ThreadCreated = self
            .post_form(
                "/threads",
                &form,
                Some(Duration::from_millis(config::NEW_THREAD_COOLDOWN_MS)),
            )
            .await?;
        Ok(created.thread)
    }

    async fn mark_thread_read(&self, id: u32, date: Option<i64>) -> Result<()> {
        let form: Vec<(&str, String)> = match date {
            Some(d) if d > 0 => vec![("date", d.to_string())],
            _ => Vec::new(),
        };
        self.post_unit(&format!("/threads/{id}/mark-read"), &form)
            .await
    }

    async fn mark_alerts_viewed(&self) -> Result<()> {
        self.post_unit("/alerts/mark-all", &[("viewed", "1".to_string())])
            .await
    }

    async fn edit_post(
        &self,
        id: u32,
        message: &str,
        attachment_key: Option<&str>,
    ) -> Result<()> {
        // An edit is a write like any other: it goes through the write gate,
        // which is what keeps this client inside the zone's flood budget.
        self.write_gate.wait().await;
        let mut form = vec![("message", message.to_string())];
        if let Some(key) = attachment_key {
            form.push(("attachment_key", key.to_string()));
        }
        self.post_unit(&format!("/posts/{id}"), &form).await
    }

    async fn delete_post(&self, id: u32, hard: bool) -> Result<()> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/posts/{id}", self.api_base());
        let form: Vec<(&str, String)> = if hard {
            vec![("hard_delete", "1".to_string())]
        } else {
            Vec::new()
        };
        let resp = self
            .http
            .delete(url)
            .form(&form)
            .bearer_auth(&token)
            .send()
            .await?;
        check_status(resp).await
    }

    async fn mark_solution(&self, id: u32) -> Result<()> {
        self.post_unit_path(&format!("/posts/{id}/mark-solution")).await
    }

    async fn mark_forum_read(&self, node_id: u32) -> Result<()> {
        self.post_unit_path(&format!("/forums/{node_id}/mark-read")).await
    }

    async fn conversations(&self, page: u32) -> Result<ConversationsReply> {
        self.get("/conversations", &[("page", page.to_string())]).await
    }

    async fn conversation(&self, id: u32, page: u32) -> Result<ConversationReply> {
        self.get(
            &format!("/conversations/{id}"),
            &[("with_messages", "1".into()), ("page", page.to_string())],
        )
        .await
    }

    async fn reply_conversation(&self, id: u32, message: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_form(
                "/conversation-messages",
                &[
                    ("conversation_id", id.to_string()),
                    ("message", message.to_string()),
                ],
                None,
            )
            .await?;
        Ok(())
    }

    async fn create_conversation(
        &self,
        recipient_ids: &[u32],
        title: &str,
        message: &str,
    ) -> Result<Conversation> {
        // recipient_ids is int[] — XF form-decodes "recipient_ids[]=1&recipient_ids[]=2".
        let mut form: Vec<(&str, String)> =
            vec![("title", title.to_string()), ("message", message.to_string())];
        for id in recipient_ids {
            form.push(("recipient_ids[]", id.to_string()));
        }
        #[derive(serde::Deserialize)]
        struct ConversationCreated {
            conversation: Conversation,
        }
        let created: ConversationCreated = self.post_form("/conversations", &form, None).await?;
        Ok(created.conversation)
    }

    async fn mark_conversation_read(&self, id: u32) -> Result<()> {
        self.post_unit_path(&format!("/conversations/{id}/mark-read")).await
    }

    async fn delete_conversation(&self, id: u32) -> Result<()> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/conversations/{id}", self.api_base());
        let resp = self.http.delete(url).bearer_auth(&token).send().await?;
        check_status(resp).await
    }

    async fn alerts(&self, page: u32) -> Result<AlertsReply> {
        self.get("/alerts", &[("page", page.to_string())]).await
    }

    async fn mark_alert_read(&self, id: u32) -> Result<()> {
        self.post_unit(&format!("/alerts/{id}/mark"), &[("read", "1".into())])
            .await
    }

    async fn search(&self, keywords: &str, page: u32) -> Result<SearchResultsReply> {
        self.search_advanced(&SearchQuery {
            keywords: keywords.to_string(),
            page,
            ..Default::default()
        })
        .await
    }

    async fn search_advanced(&self, query: &SearchQuery) -> Result<SearchResultsReply> {
        self.search_gate.wait().await;
        #[derive(serde::Deserialize)]
        struct SearchInfo {
            #[serde(default)]
            search_id: u32,
        }
        #[derive(serde::Deserialize)]
        struct SearchCreated {
            #[serde(default)]
            search: Option<SearchInfo>,
        }
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/search", self.api_base());
        let mut form: Vec<(&str, String)> = Vec::new();
        if !query.keywords.trim().is_empty() {
            form.push(("keywords", query.keywords.trim().to_string()));
        }
        if let Some(user) = &query.user
            && !user.trim().is_empty()
        {
            form.push(("c[users]", user.trim().to_string()));
        }
        if let Some(ct) = &query.content_type
            && !ct.trim().is_empty()
            && ct != "all"
        {
            form.push(("search_type", ct.trim().to_string()));
        }
        if let Some(order) = &query.order
            && !order.trim().is_empty()
        {
            form.push(("order", order.trim().to_string()));
        }
        let form_slices: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let resp = self
            .http
            .post(url)
            .form(&form_slices)
            .bearer_auth(&token)
            .send()
            .await?;
        let created: SearchCreated = decode(resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.search_gate]))?;
        let Some(search) = created.search else {
            return Ok(SearchResultsReply::default());
        };
        let search_id = search.search_id;
        self.get(
            &format!("/search/{search_id}"),
            &[("page", query.page.to_string())],
        )
        .await
    }

    /// XFMG media list (issue #680): `GET /media/?page=N` — the addon's
    /// REST list controller returns `{media: [...], pagination}`. With a
    /// category the same envelope comes from that category's content route
    /// (#697), so one screen shape covers both.
    async fn media_list(&self, category: Option<u32>, page: u32) -> Result<MediaListReply> {
        let path = match category {
            Some(id) => format!("/media-categories/{id}/content"),
            None => "/media".to_string(),
        };
        self.get(&path, &[("page", page.to_string())]).await
    }

    async fn media_categories(&self) -> Result<MediaCategoriesReply> {
        self.get("/media-categories", &[]).await
    }

    async fn media_item(&self, id: u32) -> Result<MediaItemReply> {
        self.get(&format!("/media/{id}"), &[]).await
    }

    async fn resource(&self, id: u32) -> Result<ResourceReply> {
        self.get(&format!("/resources/{id}"), &[]).await
    }

    /// XFRM resource list (issue #680): `GET /resources/?page=N` —
    /// `{resources: [...], pagination}`.
    async fn resources_list(&self, page: u32) -> Result<ResourceListReply> {
        self.get("/resources", &[("page", page.to_string())]).await
    }

    async fn search_member(
        &self,
        user_id: u32,
        content: &str,
        page: u32,
    ) -> Result<SearchResultsReply> {
        self.search_gate.wait().await;
        #[derive(serde::Deserialize)]
        struct SearchInfo {
            #[serde(default)]
            search_id: u32,
        }
        #[derive(serde::Deserialize)]
        struct SearchCreated {
            #[serde(default)]
            search: Option<SearchInfo>,
        }
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/search/member", self.api_base());
        let mut form: Vec<(&str, String)> = vec![("user_id", user_id.to_string())];
        if !content.is_empty() && content != "all" {
            form.push(("content", content.to_string()));
        }
        let form_slices: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let resp = self
            .http
            .post(url)
            .form(&form_slices)
            .bearer_auth(&token)
            .send()
            .await?;
        let created: SearchCreated = decode(resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.search_gate]))?;
        let Some(search) = created.search else {
            return Ok(SearchResultsReply::default());
        };
        let search_id = search.search_id;
        self.get(
            &format!("/search/{search_id}"),
            &[("page", page.to_string())],
        )
        .await
    }

    async fn react_post(&self, post_id: u32, reaction_id: u32) -> Result<Toggle> {
        self.post_toggle(
            &format!("/posts/{post_id}/react"),
            &[("reaction_id", reaction_id.to_string())],
        )
        .await
    }

    async fn vote_post(&self, post_id: u32, vote: &str) -> Result<Toggle> {
        self.post_toggle(
            &format!("/posts/{post_id}/vote"),
            &[("type", vote.to_string())],
        )
        .await
    }

    async fn me(&self) -> Result<User> {
        let reply: MeReply = self.get("/me", &[]).await?;
        Ok(reply.me)
    }

    async fn user(&self, id: u32) -> Result<User> {
        let reply: UserReply = self.get(&format!("/users/{id}"), &[]).await?;
        Ok(reply.user)
    }

    async fn find_user(&self, username: &str) -> Result<Option<User>> {
        let reply: UsersFindNameReply = self
            .get("/users/find-name", &[("username", username.to_string())])
            .await?;
        Ok(reply.exact.filter(|u| u.user_id > 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Env overrides (base URL, client id, config dir) are process-global:
    /// hold one lock for the whole test and give each test its own config dir.
    use config::ENV_LOCK;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
        /// The process's prior env, restored on drop — set back or removed,
        /// never left pointing at the deleted fixture dir (logging.rs's
        /// documented "restored, never removed" policy; #660).
        prior: [(String, Option<String>); 3],
    }

    impl EnvGuard {
        fn hold(base: &str, dir: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // Pid-suffix the fixture dir, like the app crate's tests do:
            // two test binaries running `cargo test --workspace` must not
            // share — or delete each other's — fixtures (#660).
            let dir = format!("{dir}-{}", std::process::id());
            let prior = [
                (
                    "WFTUI_BASE_URL".to_string(),
                    std::env::var("WFTUI_BASE_URL").ok(),
                ),
                (
                    "WFTUI_OAUTH_CLIENT_ID".to_string(),
                    std::env::var("WFTUI_OAUTH_CLIENT_ID").ok(),
                ),
                (
                    "WFTUI_CONFIG_DIR".to_string(),
                    std::env::var("WFTUI_CONFIG_DIR").ok(),
                ),
            ];
            unsafe { std::env::set_var("WFTUI_BASE_URL", base) };
            unsafe { std::env::set_var("WFTUI_OAUTH_CLIENT_ID", "test-client") };
            unsafe { std::env::set_var("WFTUI_CONFIG_DIR", &dir) };
            // The whole point of the guard: from here on, every path this
            // process resolves must be inside the fixture dir. Asserting it
            // *before* any client is built (and so before any `save()`) is
            // what turns a lost race on the process-global variable into a
            // loud test failure instead of a silent write to the machine
            // owner's real `~/.config/wftui/token.json` — issue #565, where
            // exactly that happened and destroyed the operator's session.
            let path = config::token_path();
            assert!(
                path.starts_with(&dir),
                "token store escaped the fixture dir: {path:?} is not under {dir}"
            );
            assert!(
                !path.starts_with(config::default_config_root()),
                "a test resolved the real config dir: {path:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
            EnvGuard {
                _lock: lock,
                dir: std::path::PathBuf::from(&dir),
                prior,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore (set back or remove) — the variable is never just
            // dropped on the floor pointing at a deleted fixture dir (#660).
            for (name, value) in &self.prior {
                match value {
                    Some(v) => unsafe { std::env::set_var(name, v) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Caller must already hold `EnvGuard` (the env is process-global).
    async fn logged_in_client(token: &str) -> WfApiClient {
        let c = WfApiClient::new().unwrap();
        // Re-checked here because the very next line SAVES a token set: if
        // the store were the real one, this fixture would overwrite the
        // machine owner's session (issue #565).
        assert!(
            !c.store_path().starts_with(config::default_config_root()),
            "refusing to write a fixture token set into the real config dir: {:?}",
            c.store_path()
        );
        c.set_tokens(TokenSet {
            access_token: token.into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() + 3600,
            scope: "test".into(),
        })
        .await
        .unwrap();
        c
    }

    /// Issue #712: the politeness gates run on real time in tests too. A
    /// second write on one client therefore sleeps the full 30 s
    /// `WRITE_COOLDOWN_MS` — 30.6 s of the `common` suite's 36.8 s was one
    /// test doing exactly that, and the suite burned 36.8 s of wall clock
    /// for 1.9 s of CPU.
    ///
    /// A test that is about *what* goes on the wire, rather than about the
    /// spacing between requests, opens the gates first. The gate behaviour
    /// itself stays covered, and deliberately still waits, in
    /// `upload_attachment_re_anchors_the_write_gate_on_success` and the
    /// `ratelimit` module's own tests; the guard against a new multi-write
    /// test re-introducing the wait is
    /// `write_gate_is_opened_in_every_test_that_writes_twice`.
    ///
    /// Call it before *each* write, not once up front: a successful write
    /// `penalize`s the gate back to the full cool-down, which a fresh
    /// zero-`min` gate is the simplest way to clear.
    fn open_gates(c: &mut WfApiClient) {
        c.write_gate = Arc::new(Gate::new(0));
        c.image_gate = Arc::new(Gate::new(0));
    }

    /// GUARD (issue #565). The suite must never read or write the machine
    /// owner's real config dir, and must never send a request to the live
    /// site. It once did both: `WfApiClient::new()` resolved
    /// `~/.config/wftui/token.json`, a `wftui` test really `GET /api/me`'d
    /// windowsforum.com with the operator's bearer, and this crate's own
    /// fixture (`tok-1`/`refresh-1`/`test`) was found written into the real
    /// store. This test pins the two properties that make that impossible:
    /// the fixture env resolves every path inside the scratch dir, and a
    /// *poisoned* config dir (one that cannot hold a store at all) yields no
    /// session and fails a save loudly instead of falling back to anything
    /// real.
    #[tokio::test]
    async fn guard_no_test_can_reach_the_real_config_dir_or_the_live_site() {
        let real = config::default_config_root();
        let dir = "/tmp/wftui-t-guard";
        {
            let env = EnvGuard::hold("http://127.0.0.1:1", dir);
            // `hold` pid-suffixes the fixture dir (#660); compare against
            // the dir it actually created.
            let dir = env.dir.to_string_lossy().to_string();
            for path in [config::token_path(), config::log_path()] {
                assert!(path.starts_with(&dir), "escaped the scratch dir: {path:?}");
                assert!(!path.starts_with(&real), "resolved the real config dir: {path:?}");
            }
            let c = WfApiClient::new().unwrap();
            assert!(c.store_path().starts_with(&dir), "{:?}", c.store_path());
            assert_eq!(c.base_url(), "http://127.0.0.1:1");
            assert!(
                !c.base_url().contains("windowsforum.com"),
                "a test client must never be pointed at the live site"
            );
            assert!(!c.has_tokens().await, "a scratch config dir holds no session");
        }

        // Poisoned: the config dir is a plain FILE, so a store under it can
        // neither be read nor created. Nothing may quietly fall back to a
        // usable (i.e. real) location. (`hold` pid-suffixes the fixture
        // dir, so the poison is planted at the path it actually created.)
        let poison_base = "/tmp/wftui-t-guard-poison";
        let _ = std::fs::remove_dir_all(poison_base);
        let _ = std::fs::remove_file(poison_base);
        {
            let env = EnvGuard::hold("http://127.0.0.1:1", poison_base);
            let _ = std::fs::remove_dir_all(&env.dir);
            std::fs::write(&env.dir, b"poison").unwrap();
            let store = token::Store::new();
            assert!(store.path().starts_with(&env.dir), "{:?}", store.path());
            assert!(
                !matches!(store.load(), Ok(Some(_))),
                "a poisoned config dir must never yield a session"
            );
            assert!(
                store
                    .save(&TokenSet {
                        access_token: "guard".into(),
                        refresh_token: "guard".into(),
                        expires_at: 0,
                        scope: "guard".into(),
                    })
                    .is_err(),
                "a stray save must fail loudly, not land in the real store"
            );
        }
        let _ = std::fs::remove_file(poison_base);
    }

    /// Issue #712: the `common` suite once took 36.8 s of wall clock for
    /// 1.9 s of CPU, because one test did two `upload_attachment` calls on
    /// one client and slept the real 30 s `WRITE_COOLDOWN_MS` between them.
    ///
    /// The gates are deliberately real everywhere else, so nothing stops the
    /// next multi-write test from doing it again — and a 30 s test reads as
    /// "the suite is just slow", not as a mistake. This scan is the guard: a
    /// test that calls a write-gated method more than once must also call
    /// `open_gates`, or be named here as one that waits on purpose.
    ///
    /// It checks that `open_gates` is *mentioned*, not that it is called
    /// often enough — a successful write re-`penalize`s the gate, so a test
    /// with three writes needs three calls. That part is self-evident to
    /// whoever writes it, because the test is slow until they get it right.
    #[test]
    fn write_gate_is_opened_in_every_test_that_writes_twice() {
        // The methods that `wait()` on `write_gate`, directly or through
        // `post_form`. `delete_post` and `mark_solution` are not here: they
        // take the api gate only.
        const WRITE_CALLS: [&str; 6] = [
            ".reply(",
            ".create_thread(",
            ".reply_conversation(",
            ".create_conversation(",
            ".edit_post(",
            ".upload_attachment(",
        ];
        // Tests whose subject *is* the spacing, so they must keep waiting.
        const WAITS_ON_PURPOSE: [&str; 1] =
            ["upload_attachment_re_anchors_the_write_gate_on_success"];

        let src = include_str!("api.rs");
        // Split on the test-function boundary; the first chunk is everything
        // before the first test and is not one.
        let mut offenders = Vec::new();
        for chunk in src.split("    async fn ").skip(1) {
            let name = chunk
                .split(['(', '<'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if WAITS_ON_PURPOSE.contains(&name.as_str()) {
                continue;
            }
            let writes: usize = WRITE_CALLS.iter().map(|m| chunk.matches(m).count()).sum();
            if writes >= 2 && !chunk.contains("open_gates") {
                offenders.push(format!("{name} ({writes} write calls)"));
            }
        }
        assert!(
            offenders.is_empty(),
            "these tests write more than once without opening the write gate, so \
             each extra write sleeps the real {} ms cool-down (#712): {:?}",
            config::WRITE_COOLDOWN_MS,
            offenders
        );
    }

    /// Issue #557: two instances sharing one config dir rotate each other's
    /// refresh token out from under themselves. `adopt_stored_tokens` must
    /// pick up what the sibling wrote — and must report `false` (nothing to
    /// recover) when the file is missing or already the token set in memory,
    /// so the caller can end the session instead of looping.
    /// #709: a second file goes up under the key the first one minted, so
    /// one post's attachments share one key — and the key request is not
    /// repeated.
    #[tokio::test]
    async fn a_second_upload_reuses_the_first_keys() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-upload-reuse");
        Mock::given(method("POST"))
            .and(path("/api/attachments/new-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "key-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/attachments/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attachment": {"attachment_id": 55, "filename": "shot.png"}
            })))
            .mount(&server)
            .await;

        // Two writes on one client: without this the second `upload_attachment`
        // sleeps the full 30 s write cool-down for a test that is about key
        // reuse, not spacing (#712).
        let mut c = logged_in_client("tok-1").await;
        open_gates(&mut c);
        let (key, _) = c
            .upload_attachment("post", &[], "a.png".into(), vec![1], "image/png", None)
            .await
            .unwrap();
        // A *successful* upload re-anchors the cool-down by `penalize`-ing the
        // gate (#644), so opening it once up front is not enough — the second
        // call would still sleep the full 30 s.
        open_gates(&mut c);
        let (key2, _) = c
            .upload_attachment("post", &[], "b.png".into(), vec![2], "image/png", Some(&key))
            .await
            .unwrap();
        assert_eq!(key2, key, "the second file joins the first one's key");
        // `expect(1)` on the key mock is the other half: it must not be
        // minted twice.
    }

    /// #697: the gallery's full-size bytes come from the API itself
    /// (`/api/media/{id}/data`) and need the bearer token — but an image URL
    /// is attacker-influenced (any post can carry an `[IMG]` pointing
    /// anywhere), so the token must never leave our own API. Both halves are
    /// the contract.
    #[tokio::test]
    async fn fetch_bytes_sends_the_token_only_to_our_own_api() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-fetchauth");
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
        ];
        // Our own API path: the request must carry the grant.
        Mock::given(method("GET"))
            .and(path("/api/media/7/data"))
            .and(wiremock::matchers::header("authorization", "Bearer tok-1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(png.clone(), "image/png"),
            )
            .mount(&server)
            .await;
        // Anywhere else on the same host: it must NOT.
        Mock::given(method("GET"))
            .and(path("/data/attachments/9.jpg"))
            .respond_with(move |req: &wiremock::Request| {
                if req.headers.get("authorization").is_some() {
                    // A leaked grant is a failure, not a fallback.
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_raw(
                        vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                        "image/png",
                    )
                }
            })
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let api_url = format!("{}/api/media/7/data", server.uri());
        assert!(
            c.fetch_bytes(&api_url, 1024).await.is_ok(),
            "the API's own media needs the token"
        );
        let other = format!("{}/data/attachments/9.jpg", server.uri());
        assert!(
            c.fetch_bytes(&other, 1024).await.is_ok(),
            "a non-API image URL must be fetched WITHOUT the token"
        );
    }

    /// #680/#696/#697: both catalog envelopes, against **captured live
    /// responses** rather than hand-written JSON.
    ///
    /// This is the lesson of #696: the first version of these tests invented
    /// their own fixture, so `rating_average` — a field that does not exist
    /// on the wire — passed happily while the real rating never rendered. A
    /// captured body also carries the shapes nobody would think to invent,
    /// like `review_count: null`, which fails a plain `u64` outright
    /// (`#[serde(default)]` covers a missing field, not an explicit null).
    /// **Whole captured bodies**, not field-filtered ones (#698). The first
    /// attempt at this captured the response but selected only the fields
    /// the model claimed to want, which dropped `tags` — and `tags` is
    /// exactly the field whose shape (`[]`, not a map) failed the entire
    /// Resources list in production. A fixture is only as good as the part
    /// of the body it kept.
    const MEDIA_PAGE_JSON: &str = r#"{"media":[{"album_id":0,"album_state":null,"can_edit":false,"can_edit_tags":false,"can_hard_delete":false,"can_react":false,"can_soft_delete":false,"category_id":11,"comment_count":0,"custom_fields":{},"description":"","file_size":177058,"height":1024,"last_comment_date":0,"last_comment_id":0,"last_comment_user_id":0,"last_comment_username":"","last_edit_date":0,"media_date":1788748191,"media_id":35691,"media_state":"visible","media_type":"image","media_url":"https://windowsforum.com/api/media/35691/data","rating_avg":0,"rating_count":0,"rating_weighted":3,"reaction_score":0,"tags":[],"thumbnail_url":"https://data.windowsforum.com/xfmg/thumbnail/35/35691-ce8b7d219b1999476972c0ae567a5ca1.jpg?1788748208","title":"windowsforum-windows-11-10-create-local-groups-for-folder-and-smb-access.webp","user_id":125694,"username":"WindowsForum AI","view_count":7,"view_url":"https://windowsforum.com/media/windowsforum-windows-11-10-create-local-groups-for-folder-and-smb-access-webp.35691/","warning_message":"","width":1536,"User":{"avatar_urls":{"o":"https://data.windowsforum.com/avatars/o/125/125694.jpg?1769482146","h":"https://data.windowsforum.com/avatars/h/125/125694.jpg?1769482146","l":"https://data.windowsforum.com/avatars/l/125/125694.jpg?1769482146","m":"https://data.windowsforum.com/avatars/m/125/125694.jpg?1769482146","s":"https://data.windowsforum.com/avatars/s/125/125694.jpg?1769482146"},"can_ban":false,"can_converse":false,"can_edit":false,"can_follow":false,"can_ignore":false,"can_post_profile":false,"can_view_profile":true,"can_view_profile_posts":true,"can_warn":false,"custom_fields":{"real_name":"","operating_system":"","computer_type":"","cpu_type_and_speed":"","occupation":"","motherboard_chipset":"","system_memory_type":"","video_card_type_and_speed":"NVIDIA A100 Tensor Core GPU, 40GB HBM2e VRAM (x1000)","power_supply_unit_psu":"","computer_monitor":"","sound_card":"","hard_drive":"","network_adapter":"","network_speed":"","gaming_console":"","computer_skill_level":"","favorite_game":"","skype":"","facebook":"","twitter":""},"is_staff":true,"last_activity":1788750302,"location":"","message_count":114251,"profile_banner_urls":{"l":null,"m":null},"question_solution_count":8724,"reaction_score":419,"register_date":1678821147,"signature":"I am an artificially intelligent bot. [B][I]Use the Reply button to quote me for additional responses or use @ChatGPT[/I][/B][I].[/I] You can use my custom GPT helper at: [URL='https://chatgpt.com/g/g-uEYHkDTB2-windowsforum-helper']OpenAI[/URL]. I could be error-prone or occasionally hallucinate!","trophy_points":361,"user_id":125694,"user_title":"AI","username":"WindowsForum AI","view_url":"https://windowsforum.com/members/windowsforum-ai.125694/","vote_score":8734,"website":""}}],"pagination":{"current_page":1,"last_page":2092,"per_page":12,"shown":12,"total":25101}}"#;
    const RESOURCES_PAGE_JSON: &str = r#"{"resources":[{"alt_support_url":"","can_download":true,"can_edit":false,"can_edit_icon":false,"can_edit_tags":false,"can_hard_delete":false,"can_soft_delete":false,"can_view_description_attachments":true,"currency":"","custom_fields":{},"download_count":5678,"external_url":"https://windowsforum.com/resources/windowsforum-com-diagnostic-tool.1/","icon_url":"https://data.windowsforum.com/resource_icons/0/1.jpg?1756443640","last_update":1787444096,"prefix_id":0,"price":"0.00","rating_avg":5,"rating_count":2,"rating_weighted":3.33333,"resource_category_id":2,"resource_date":1375003071,"resource_id":1,"resource_state":"visible","resource_type":"download","tag_line":"Windows diagnostic collector for support and troubleshooting","tags":[],"title":"WindowsForum.com Diagnostic Tool","user_id":1,"username":"Mike","version":"2.5.8","view_count":19017,"view_url":"https://windowsforum.com/resources/windowsforum-com-diagnostic-tool.1/","Category":{"allow_commercial_external":true,"allow_external":true,"allow_fileless":true,"allow_local":true,"can_add":false,"can_upload_images":false,"description":"Diagnostic tools are designed to help you diagnose problems on your system.","display_order":1,"enable_support_url":true,"enable_versioning":true,"last_resource_id":1,"last_resource_title":"WindowsForum.com Diagnostic Tool","last_update":1787444096,"min_tags":0,"parent_category_id":0,"resource_category_id":2,"resource_count":18,"title":"Diagnostic Tools","view_url":"https://windowsforum.com/resources/categories/diagnostic-tools.2/"},"User":{"age":43,"avatar_urls":{"o":"https://data.windowsforum.com/avatars/o/0/1.jpg?1756625372","h":"https://data.windowsforum.com/avatars/h/0/1.jpg?1756625372","l":"https://data.windowsforum.com/avatars/l/0/1.jpg?1756625372","m":"https://data.windowsforum.com/avatars/m/0/1.jpg?1756625372","s":"https://data.windowsforum.com/avatars/s/0/1.jpg?1756625372"},"can_ban":false,"can_converse":false,"can_edit":false,"can_follow":false,"can_ignore":false,"can_post_profile":false,"can_view_profile":true,"can_view_profile_posts":true,"can_warn":false,"custom_fields":{"real_name":"Mike Fara","operating_system":"Windows 11","computer_type":"Dell XPS 15 9510","cpu_type_and_speed":"","occupation":"IT","motherboard_chipset":"","system_memory_type":"","video_card_type_and_speed":"","power_supply_unit_psu":"","computer_monitor":"","sound_card":"","hard_drive":"","network_adapter":"","network_speed":"","gaming_console":"","computer_skill_level":"certified_professional","favorite_game":"","skype":"mikeawib","facebook":"601645028","twitter":"windowsforum"},"dob":{"year":1982,"month":12,"day":11},"is_staff":true,"location":"","message_count":9267,"profile_banner_urls":{"l":"https://data.windowsforum.com/profile_banners/l/0/1.jpg?1724851275","m":"https://data.windowsforum.com/profile_banners/m/0/1.jpg?1724851275"},"question_solution_count":613,"reaction_score":989,"register_date":1122004800,"signature":"","trophy_points":1744,"user_id":1,"user_title":"Windows Forum Admin","username":"Mike","view_url":"https://windowsforum.com/members/mike.1/","vote_score":615,"website":""}}],"pagination":{"current_page":1,"last_page":3,"per_page":20,"shown":20,"total":44}}"#;
    const RESOURCE_JSON: &str = r#"{"resource":{"alt_support_url":"","can_download":true,"can_edit":false,"can_edit_icon":false,"can_edit_tags":false,"can_hard_delete":false,"can_soft_delete":false,"can_view_description_attachments":true,"currency":"","current_download_url":"https://github.com/faratech/wfdiag/releases/tag/v2.5.7","custom_fields":{},"description":"[B]About this resource\n\nWhat it is[/B]\nWindowsForum.com Diagnostic Tool is a portable Windows support utility that collects a broad troubleshooting snapshot into a ZIP archive.\n\n[B]What it collects[/B]\nIt gathers system, hardware, driver, event-log, network, update, service, process, and crash-dump information that can help diagnose blue screens, device problems, startup issues, and general instability.\n\n[B]Before sharing[/B]\nRun it only on a PC you are troubleshooting. Review the archive before posting it: diagnostic reports can contain computer names, installed-program lists, network details, event data, and file paths. Remove or redact anything you do not want to share.\n\n[B]Availability[/B]\nThis project provides portable Windows builds. Use its release page for current build notes and instructions.\n\n[B]Project resources[/B]\n[URL='https://github.com/faratech/wfdiag']WindowsForum Diagnostic Tool releases[/URL]\n\n[B]Original resource notes[/B]\nThe WindowsForum.com Diagnostic Tool v2.0 is an advanced utility designed to gather a broad spectrum of system and diagnostic information to aid in troubleshooting issues on your Windows system. Here's a detailed breakdown of its functions and the types of logs it collects:\n\nHow to Run:\n\nDownload, extract, and run the WF Diagnostic Tool by right-clicking on the WF Diagnostic Tool executable and choosing 'Run as administrator'.\n\nThe WF Diagnostic Tool will automatically begin collecting a variety of diagnostic information, saving it to a folder named 'WindowsForum' on your Desktop. This process may take some time.\n\nUpon completion, the tool will compress the collected files into a .zip file named 'WF-Diagnostics.zip', also located on your Desktop. Please review these files before sharing them online.\n\n- System Summary: Gathers detailed information about the system hardware, including the computer system, operating system, BIOS, processor, and physical memory.\n- Hardware Resources: Collects data about the system's hardware resources, such as device memory addresses, DMA channels, IRQ resources, disk drives, and disk partitions.\n- Components: Provides information about system devices, network adapters, and printers.\n- Software Environment: Retrieves information about the system's software environment, including environment variables, startup commands, and system drivers.\n- DXDiag: Collects information about the system's DirectX sound and video configurations.\n- SystemInfo: Gathers information about the computer and operating system.\n- Device Drivers: Lists all signed drivers on the system.\n- Event Logs: Exports the System and Application event logs.\n- Network Configuration: Gathers detailed information about the system's network configuration.\n- Installed Programs: Lists all installed programs on the system.\n- Windows Store Apps: Lists all Windows Store apps installed on the system.\n- System Services: Lists all services on the system.\n- Running Processes: Lists all processes currently running on the system.\n- Performance Data: Gathers performance data from the system.\n- HOSTS File: Copies the HOSTS file from the system.\n- Scheduled Tasks: Lists all scheduled tasks on the system.\n- Windows Update Log: Exports the Windows Update log.\n- Battery Report: Generates a detailed battery report.\n- Driver Verifier Settings: Gathers information about the system's driver verifier settings.\n- BSOD Minidump: Copies minidump files generated by BSOD crashes from the system.\n\nAfter all diagnostic tasks have completed, the tool compresses the results into a .zip file and opens the location of the .zip file. It also opens the WindowsForum.com website in the default web browser.\n\nPLEASE NOTE: This is currently a work in progress! Additional improved as time is allowed.","description_attach_count":2,"description_parsed":"<b>About this resource<br />\n<br />\nWhat it is</b><br />\nWindowsForum.com Diagnostic Tool is a portable Windows support utility that collects a broad troubleshooting snapshot into a ZIP archive.<br />\n<br />\n<b>What it collects</b><br />\nIt gathers system, hardware, driver, event-log, network, update, service, process, and crash-dump information that can help diagnose blue screens, device problems, startup issues, and general instability.<br />\n<br />\n<b>Before sharing</b><br />\nRun it only on a PC you are troubleshooting. Review the archive before posting it: diagnostic reports can contain computer names, installed-program lists, network details, event data, and file paths. Remove or redact anything you do not want to share.<br />\n<br />\n<b>Availability</b><br />\nThis project provides portable Windows builds. Use its release page for current build notes and instructions.<br />\n<br />\n<b>Project resources</b><br />\n<a href=\"https://github.com/faratech/wfdiag\" target=\"_blank\" class=\"link link--external\" rel=\"noopener\">WindowsForum Diagnostic Tool releases</a><br />\n<br />\n<b>Original resource notes</b><br />\nThe WindowsForum.com Diagnostic Tool v2.0 is an advanced utility designed to gather a broad spectrum of system and diagnostic information to aid in troubleshooting issues on your Windows system. Here&#039;s a detailed breakdown of its functions and the types of logs it collects:<br />\n<br />\nHow to Run:<br />\n<br />\nDownload, extract, and run the WF Diagnostic Tool by right-clicking on the WF Diagnostic Tool executable and choosing &#039;Run as administrator&#039;.<br />\n<br />\nThe WF Diagnostic Tool will automatically begin collecting a variety of diagnostic information, saving it to a folder named &#039;WindowsForum&#039; on your Desktop. This process may take some time.<br />\n<br />\nUpon completion, the tool will compress the collected files into a .zip file named &#039;WF-Diagnostics.zip&#039;, also located on your Desktop. Please review these files before sharing them online.<br />\n<br />\n- System Summary: Gathers detailed information about the system hardware, including the computer system, operating system, BIOS, processor, and physical memory.<br />\n- Hardware Resources: Collects data about the system&#039;s hardware resources, such as device memory addresses, DMA channels, IRQ resources, disk drives, and disk partitions.<br />\n- Components: Provides information about system devices, network adapters, and printers.<br />\n- Software Environment: Retrieves information about the system&#039;s software environment, including environment variables, startup commands, and system drivers.<br />\n- DXDiag: Collects information about the system&#039;s DirectX sound and video configurations.<br />\n- SystemInfo: Gathers information about the computer and operating system.<br />\n- Device Drivers: Lists all signed drivers on the system.<br />\n- Event Logs: Exports the System and Application event logs.<br />\n- Network Configuration: Gathers detailed information about the system&#039;s network configuration.<br />\n- Installed Programs: Lists all installed programs on the system.<br />\n- Windows Store Apps: Lists all Windows Store apps installed on the system.<br />\n- System Services: Lists all services on the system.<br />\n- Running Processes: Lists all processes currently running on the system.<br />\n- Performance Data: Gathers performance data from the system.<br />\n- HOSTS File: Copies the HOSTS file from the system.<br />\n- Scheduled Tasks: Lists all scheduled tasks on the system.<br />\n- Windows Update Log: Exports the Windows Update log.<br />\n- Battery Report: Generates a detailed battery report.<br />\n- Driver Verifier Settings: Gathers information about the system&#039;s driver verifier settings.<br />\n- BSOD Minidump: Copies minidump files generated by BSOD crashes from the system.<br />\n<br />\nAfter all diagnostic tasks have completed, the tool compresses the results into a .zip file and opens the location of the .zip file. It also opens the WindowsForum.com website in the default web browser.<br />\n<br />\nPLEASE NOTE: This is currently a work in progress! Additional improved as time is allowed.","download_count":5678,"external_url":"https://windowsforum.com/resources/windowsforum-com-diagnostic-tool.1/","icon_url":"https://data.windowsforum.com/resource_icons/0/1.jpg?1756443640","last_update":1787444096,"prefix_id":0,"price":"0.00","rating_avg":5,"rating_count":2,"rating_weighted":3.33333,"reaction_score":0,"resource_category_id":2,"resource_date":1375003071,"resource_id":1,"resource_state":"visible","resource_type":"download","review_count":2,"tag_line":"Windows diagnostic collector for support and troubleshooting","tags":[],"title":"WindowsForum.com Diagnostic Tool","update_count":8,"user_id":1,"username":"Mike","version":"2.5.8","view_count":19017,"view_url":"https://windowsforum.com/resources/windowsforum-com-diagnostic-tool.1/","Category":{"allow_commercial_external":true,"allow_external":true,"allow_fileless":true,"allow_local":true,"can_add":false,"can_upload_images":false,"description":"Diagnostic tools are designed to help you diagnose problems on your system.","display_order":1,"enable_support_url":true,"enable_versioning":true,"last_resource_id":1,"last_resource_title":"WindowsForum.com Diagnostic Tool","last_update":1787444096,"min_tags":0,"parent_category_id":0,"resource_category_id":2,"resource_count":18,"title":"Diagnostic Tools","view_url":"https://windowsforum.com/resources/categories/diagnostic-tools.2/"},"DescriptionAttachments":[{"attach_date":1375003026,"attachment_id":25028,"content_id":1,"content_type":"resource_update","direct_url":"https://windowsforum.com/attachments/w7f_diagnostic_tool-webp.25028/","file_size":19254,"filename":"w7f_diagnostic_tool.webp","height":632,"is_audio":false,"is_video":false,"retina_thumbnail_url":"https://data.windowsforum.com/attachments/15/2x/15644-7acfb6a6cb00b50b30146dee570be7e1.jpg?hash=AGJvzKoQUF","thumbnail_url":"https://data.windowsforum.com/attachments/15/15644-7acfb6a6cb00b50b30146dee570be7e1.jpg?hash=AGJvzKoQUF","view_count":1314,"width":354},{"attach_date":1711366174,"attachment_id":43769,"content_id":1,"content_type":"resource_update","direct_url":"https://windowsforum.com/attachments/screenshot-2024-03-25-072825-webp.43769/","file_size":40172,"filename":"Screenshot 2024-03-25 072825.webp","height":352,"is_audio":false,"is_video":false,"retina_thumbnail_url":"https://data.windowsforum.com/attachments/34/2x/34387-bb5a96424690f4070bde5f971a7ab126.jpg?hash=-VR4-eXpWE","thumbnail_url":"https://data.windowsforum.com/attachments/34/34387-bb5a96424690f4070bde5f971a7ab126.jpg?hash=-VR4-eXpWE","view_count":0,"width":1215}],"User":{"age":43,"avatar_urls":{"o":"https://data.windowsforum.com/avatars/o/0/1.jpg?1756625372","h":"https://data.windowsforum.com/avatars/h/0/1.jpg?1756625372","l":"https://data.windowsforum.com/avatars/l/0/1.jpg?1756625372","m":"https://data.windowsforum.com/avatars/m/0/1.jpg?1756625372","s":"https://data.windowsforum.com/avatars/s/0/1.jpg?1756625372"},"can_ban":false,"can_converse":false,"can_edit":false,"can_follow":false,"can_ignore":false,"can_post_profile":false,"can_view_profile":true,"can_view_profile_posts":true,"can_warn":false,"custom_fields":{"real_name":"Mike Fara","operating_system":"Windows 11","computer_type":"Dell XPS 15 9510","cpu_type_and_speed":"","occupation":"IT","motherboard_chipset":"","system_memory_type":"","video_card_type_and_speed":"","power_supply_unit_psu":"","computer_monitor":"","sound_card":"","hard_drive":"","network_adapter":"","network_speed":"","gaming_console":"","computer_skill_level":"certified_professional","favorite_game":"","skype":"mikeawib","facebook":"601645028","twitter":"windowsforum"},"dob":{"year":1982,"month":12,"day":11},"is_staff":true,"location":"","message_count":9267,"profile_banner_urls":{"l":"https://data.windowsforum.com/profile_banners/l/0/1.jpg?1724851275","m":"https://data.windowsforum.com/profile_banners/m/0/1.jpg?1724851275"},"question_solution_count":613,"reaction_score":989,"register_date":1122004800,"signature":"","trophy_points":1744,"user_id":1,"user_title":"Windows Forum Admin","username":"Mike","view_url":"https://windowsforum.com/members/mike.1/","vote_score":615,"website":""}}}"#;

    #[tokio::test]
    async fn media_list_maps_the_gallery_envelope() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-medialist");
        Mock::given(method("GET"))
            .and(path("/api/media"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(MEDIA_PAGE_JSON, "application/json"),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let reply = c.media_list(None, 1).await.unwrap();
        assert_eq!(reply.media.len(), 1);
        let m = &reply.media[0];
        assert_eq!(m.media_id, 35691);
        assert!(m.is_image(), "media_type: {:?}", m.media_type);
        assert_eq!(m.px(), Some((1536, 1024)));
        assert!(m.thumbnail_url.as_deref().is_some_and(|u| u.contains("/xfmg/thumbnail/")));
        assert!(m.media_url.as_deref().is_some_and(|u| u.ends_with("/data")));
        assert_eq!(m.username, "WindowsForum AI");
        assert_eq!(m.category_id, 11);
        assert_eq!(reply.pagination.last_page, 2092);
    }

    /// A category scopes the same envelope to that category's content route.
    #[tokio::test]
    async fn media_list_with_a_category_uses_the_category_content_route() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-mediacat");
        Mock::given(method("GET"))
            .and(path("/api/media-categories/11/content"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(MEDIA_PAGE_JSON, "application/json"),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let reply = c.media_list(Some(11), 1).await.unwrap();
        assert_eq!(reply.media.len(), 1, "the category route must be the one called");
    }

    #[tokio::test]
    async fn resources_list_maps_the_resource_envelope_and_rating() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-reslist");
        Mock::given(method("GET"))
            .and(path("/api/resources"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(RESOURCES_PAGE_JSON, "application/json"),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let reply = c.resources_list(1).await.unwrap();
        assert_eq!(reply.resources.len(), 1);
        let r = &reply.resources[0];
        assert_eq!(r.resource_id, 1);
        assert_eq!(r.tag_line, "Windows diagnostic collector for support and troubleshooting");
        assert_eq!(r.download_count, 5678);
        assert_eq!(r.version, "2.5.8");
        // The whole point of #696: this is `rating_avg` on the wire.
        assert_eq!(r.rating_avg, Some(5.0));
        assert_eq!(r.rating_count, 2);
        // And the whole point of the null hardening: XF sends this as null.
        assert_eq!(r.review_count, 0);
        // #698: `tags` is an ARRAY here. Demanding a map failed the whole
        // reply — "invalid type: sequence, expected a map" — and took the
        // Resources screen with it.
        assert!(r.tags.is_empty(), "an untagged resource: {:?}", r.tags);
    }

    /// Tags decode from either spelling, and never fail the reply.
    #[test]
    fn tags_decode_from_an_array_a_keyed_map_or_nothing() {
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "crate::models::deserialize_tags_for_test")]
            tags: Vec<String>,
        }
        for (body, want) in [
            (r#"{"tags": ["bg3", "windows"]}"#, vec!["bg3", "windows"]),
            (r#"{"tags": []}"#, vec![]),
            (r#"{"tags": null}"#, vec![]),
            (r#"{}"#, vec![]),
            (r#"{"tags": {"12": {"tag": "bg3"}}}"#, vec!["bg3"]),
            (r#"{"tags": [17, "ok"]}"#, vec!["ok"]),
        ] {
            let got: Holder = serde_json::from_str(body).expect(body);
            let mut got = got.tags;
            got.sort();
            let mut want: Vec<String> = want.into_iter().map(String::from).collect();
            want.sort();
            assert_eq!(got, want, "{body}");
        }
    }

    /// The single-resource call is what the in-client page renders: the
    /// BBCode body, the nested category, the icon.
    #[tokio::test]
    async fn resource_maps_the_page_the_client_renders() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-resource");
        Mock::given(method("GET"))
            .and(path("/api/resources/1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(RESOURCE_JSON, "application/json"),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let r = c.resource(1).await.unwrap().resource;
        assert_eq!(r.resource_id, 1);
        assert_eq!(r.version, "2.5.8");
        assert!(r.description.contains("[B]"), "the BBCode body, unparsed");
        assert_eq!(
            r.category.as_ref().map(|c| c.title.as_str()),
            Some("Diagnostic Tools")
        );
        assert!(r.icon_url.is_some());
        assert_eq!(r.rating_avg, Some(5.0));
    }

    /// `take_tokens` must hand back the live grant *and* leave nothing
    /// behind in one step — logout revokes from the snapshot, so if anything
    /// (memory or disk) survived, a grant the user believes revoked would
    /// still be live. Holding the token lock across snapshot + erase is what
    /// makes the two indivisible; the behaviour pin is that after the call
    /// neither place holds a session and `valid_token` reports `NoToken`.
    #[tokio::test]
    async fn take_tokens_snapshots_the_grant_and_ends_the_local_session_at_once() {
        let _env = EnvGuard::hold("http://127.0.0.1:1", "/tmp/wftui-test-take");
        let client = logged_in_client("access-1").await;

        let snapshot = client
            .take_tokens()
            .await
            .unwrap()
            .expect("logout revokes from the snapshot, so it must get one");
        assert_eq!(snapshot.access_token, "access-1");
        assert_eq!(snapshot.refresh_token, "refresh-1");

        assert!(!client.has_tokens().await, "memory must be empty");
        assert!(client.token_set().await.is_none());
        assert!(
            token::Store::new().load().unwrap().is_none(),
            "the store must be erased"
        );
        assert!(matches!(client.valid_token().await, Err(Error::NoToken)));

        // Idempotent: a second take is empty and still erases cleanly.
        assert!(client.take_tokens().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn adopt_stored_tokens_takes_a_siblings_rotated_token_only_once() {        let _env = EnvGuard::hold("http://127.0.0.1:1", "/tmp/wftui-test-adopt");
        let client = logged_in_client("access-1").await;

        // Same token set on disk as in memory: nothing to adopt.
        assert!(!client.adopt_stored_tokens().await);

        // A sibling instance refreshes and rewrites the store.
        let store = token::Store::new();
        store
            .save(&TokenSet {
                access_token: "access-2".into(),
                refresh_token: "refresh-2".into(),
                expires_at: OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();
        assert!(client.adopt_stored_tokens().await, "the sibling's token must be adopted");
        assert_eq!(client.valid_token().await.unwrap(), "access-2");
        // Idempotent: the same file is no longer news.
        assert!(!client.adopt_stored_tokens().await);

        // No store at all is not something to recover from either.
        client.forget_tokens().await.unwrap();
        assert!(!client.adopt_stored_tokens().await);
    }

    #[test]
    fn derive_node_depths_walks_the_parent_chain() {
        // Category 0 / Forum 1 / sub-Forum 2, XF's real "/nodes" shape (no
        // `depth` key at all — see issue #509).
        let nodes = vec![
            Node {
                node_id: 10,
                parent_node_id: 0,
                title: "Category".into(),
                ..Default::default()
            },
            Node {
                node_id: 11,
                parent_node_id: 10,
                title: "Forum".into(),
                ..Default::default()
            },
            Node {
                node_id: 12,
                parent_node_id: 11,
                title: "Sub-forum".into(),
                ..Default::default()
            },
        ];
        let derived = derive_node_depths(nodes);
        assert_eq!(derived[0].depth, 0);
        assert_eq!(derived[1].depth, 1);
        assert_eq!(derived[2].depth, 2);
    }

    #[test]
    fn derive_node_depths_is_cycle_safe() {
        // A malformed/circular parent chain must not spin or panic.
        let nodes = vec![
            Node {
                node_id: 1,
                parent_node_id: 2,
                ..Default::default()
            },
            Node {
                node_id: 2,
                parent_node_id: 1,
                ..Default::default()
            },
        ];
        let derived = derive_node_depths(nodes);
        assert!(derived[0].depth < 64);
        assert!(derived[1].depth < 64);
    }

    #[tokio::test]
    async fn thread_read_maps_posts_and_pagination() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-threadread");
        Mock::given(method("GET"))
            .and(path("/api/threads/440365"))
            .and(header("Authorization", "Bearer tok-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "thread": {"thread_id": 440365, "title": "Hello", "view_url": "/windows-news.4/hello.440365/"},
                "posts": [{"post_id": 1, "username": "op", "message": "[B]hi[/B]"}],
                "pagination": {"current_page": 1, "last_page": 3, "per_page": 20, "shown": 20, "total": 55}
            })))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let reply = c.thread(440365, 1).await.unwrap();
        assert_eq!(reply.thread.title, "Hello");
        assert_eq!(reply.posts[0].message, "[B]hi[/B]");
        assert_eq!(reply.pagination.last_page, 3);
        assert_eq!(
            reply.thread.view_url.as_deref(),
            Some("/windows-news.4/hello.440365/")
        );
    }

    #[tokio::test]
    async fn invalid_page_error_carries_max() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-invalidpage");
        Mock::given(method("GET"))
            .and(path("/api/threads/1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errors": [{"code": "invalid_page", "message": "Invalid page.", "params": {"max": 4}}]
            })))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let err = c.thread(1, 99).await.unwrap_err();
        match err {
            Error::Api { code, max_page, .. } => {
                assert_eq!(code, "invalid_page");
                assert_eq!(max_page, Some(4));
            }
            other => panic!("wrong error: {other}"),
        }
    }

    #[tokio::test]
    async fn rate_limit_surfaces_retry_after() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-429");
        Mock::given(method("GET"))
            .and(path("/api/nodes"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        match c.nodes().await.unwrap_err() {
            Error::RateLimited { retry_after: Some(d) } => assert_eq!(d.as_secs(), 7),
            other => panic!("wrong error: {other}"),
        }
    }

    /// Issue #554: a hostile or broken origin's `Retry-After` can be a
    /// 19-20 digit number that overflows `Instant + Duration` once it
    /// reaches `Gate::penalize`. `error_from_response` must clamp it to a
    /// sane ceiling (an hour) before it ever gets that far — this pins the
    /// clamp itself, independent of `Gate::penalize`'s own `checked_add`
    /// defense in depth.
    #[tokio::test]
    async fn rate_limit_retry_after_is_clamped_to_an_hour() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-429-huge");
        Mock::given(method("GET"))
            .and(path("/api/nodes"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "18446744073709551615"),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        match c.nodes().await.unwrap_err() {
            Error::RateLimited { retry_after: Some(d) } => {
                assert_eq!(d, Duration::from_secs(3600), "must clamp, not pass the raw value through")
            }
            other => panic!("wrong error: {other}"),
        }
    }

    /// A 429's `Retry-After` must actually extend the gate it came from, not
    /// just decorate the returned error: the very next caller must see the
    /// server's requested backoff, not just the normal ~250ms politeness
    /// slot (issue #514).
    #[tokio::test]
    async fn rate_limit_extends_the_gate_the_call_used() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-429-gate");
        Mock::given(method("GET"))
            .and(path("/api/nodes"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        assert!(c.api_gate.pending_wait() < Duration::from_secs(1), "no penalty yet");
        c.nodes().await.unwrap_err();
        let waited = c.api_gate.pending_wait();
        assert!(
            waited >= Duration::from_secs(28),
            "429's Retry-After: 30 must extend api_gate, got {waited:?}"
        );
    }

    #[tokio::test]
    async fn search_runs_two_legs_and_returns_results() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-search");
        Mock::given(method("POST"))
            .and(path("/api/search"))
            .and(wiremock::matchers::body_string_contains("keywords=rust"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"success": true, "search": {"search_id": 77}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/search/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"title": "Rust hits", "view_url": "/x.1/y.2/"}],
                "pagination": {"current_page": 1, "last_page": 1, "total": 1}
            })))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let out = c.search("rust", 1).await.unwrap();
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "Rust hits");
    }

    #[tokio::test]
    async fn search_zero_hits_returns_empty_results() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-searchzero");
        Mock::given(method("POST"))
            .and(path("/api/search"))
            .and(wiremock::matchers::body_string_contains("keywords=nomatch"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": "No results found."})),
            )
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let out = c.search("nomatch", 1).await.unwrap();
        assert!(out.results.is_empty());
        assert_eq!(out.pagination.total, 0);
    }

    #[tokio::test]
    async fn conversation_create_encodes_recipient_array() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-convcreate");
        Mock::given(method("POST"))
            .and(path("/api/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "conversation": {"conversation_id": 9, "title": "Hi"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        let conv = c.create_conversation(&[5, 6], "Hi", "body").await.unwrap();
        assert_eq!(conv.conversation_id, 9);
    }

    /// Issue #527: logout used to POST the bearer alone, with no
    /// `token_type_hint`. RFC 7009 lets the server default that to
    /// `access_token`, so the 90-day refresh token survived a "Logged out."
    /// and anyone holding a copy of `token.json` kept the session. Both
    /// tokens are now sent, each with its own hint.
    #[tokio::test]
    async fn revoke_sends_a_token_type_hint_for_both_tokens() {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-revoke");
        Mock::given(method("POST"))
            .and(path("/api/oauth2/revoke"))
            .and(body_string_contains("token=refresh-1"))
            .and(body_string_contains("token_type_hint=refresh_token"))
            .and(body_string_contains("client_id=test-client"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oauth2/revoke"))
            .and(body_string_contains("token=tok-1"))
            .and(body_string_contains("token_type_hint=access_token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let http = crate::http::build().unwrap();
        crate::oauth::revoke(&http, &server.uri(), "refresh-1", Some("refresh_token"))
            .await
            .expect("refresh-token revoke");
        crate::oauth::revoke(&http, &server.uri(), "tok-1", Some("access_token"))
            .await
            .expect("access-token revoke");
    }

    /// Logout needs the refresh token, which `valid_token` never hands out.
    #[tokio::test]
    async fn token_set_exposes_both_tokens_for_logout() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-tokenset");
        let c = logged_in_client("tok-1").await;
        let set = c.token_set().await.expect("a stored token set");
        assert_eq!(set.access_token, "tok-1");
        assert!(!set.refresh_token.is_empty(), "the refresh token must be reachable");
        c.forget_tokens().await.unwrap();
        assert!(c.token_set().await.is_none());
    }

    #[tokio::test]
    async fn expired_access_token_triggers_refresh() {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-refresh";
        let env = EnvGuard::hold(&server.uri(), dir);
        let dir = env.dir.to_string_lossy().to_string();
        Mock::given(method("POST"))
            .and(path("/api/oauth2/token"))
            .and(wiremock::matchers::body_string_contains(
                "grant_type=refresh_token",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok-2", "refresh_token": "refresh-2",
                "expires_in": 7200, "token_type": "bearer", "scope": "test"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .and(header("Authorization", "Bearer tok-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"me": {"user_id": 1, "username": "me"}}),
            ))
            .mount(&server)
            .await;

        let c = WfApiClient::new().unwrap();
        c.set_tokens(TokenSet {
            access_token: "expired".into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() - 10,
            scope: "test".into(),
        })
        .await
        .unwrap();
        let me = c.me().await.unwrap();
        assert_eq!(me.username, "me");
        // New token set persisted for the next run.
        let stored = token::Store::with_path(std::path::PathBuf::from(dir).join("token.json"))
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(stored.access_token, "tok-2");
        assert_eq!(stored.refresh_token, "refresh-2");
    }

    /// A `store.save()` failure after a successful (rotating) refresh must
    /// not strand the now-revoked refresh token in the live session: the
    /// in-memory guard has to hold the new token set even when persisting it
    /// to disk fails, so the very next call reuses the new access token
    /// instead of retrying the refresh endpoint with a token the server has
    /// already revoked (issue #513).
    #[tokio::test]
    async fn refresh_survives_a_store_save_failure_without_losing_the_new_token() {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-refresh-savefail";
        // A prior run's blocker file survives `EnvGuard`'s `remove_dir_all`
        // (that call no-ops on a plain file), so clear it explicitly first.
        let _ = std::fs::remove_file(dir);
        let env = EnvGuard::hold(&server.uri(), dir);
        let dir = env.dir.to_string_lossy().to_string();
        // Exactly one refresh call: if the stranded-old-token bug returns,
        // the second `valid_token()` below retries refresh with the
        // already-consumed `refresh-1` and this expectation fails.
        Mock::given(method("POST"))
            .and(path("/api/oauth2/token"))
            .and(wiremock::matchers::body_string_contains(
                "grant_type=refresh_token",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok-2", "refresh_token": "refresh-2",
                "expires_in": 7200, "token_type": "bearer", "scope": "test"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = WfApiClient::new().unwrap();
        c.set_tokens(TokenSet {
            access_token: "expired".into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() - 10,
            scope: "test".into(),
        })
        .await
        .unwrap();

        // Sabotage the store *after* the initial save succeeded: replace the
        // config dir with a plain file, so the next `save()`'s
        // `create_dir_all` fails deterministically (no reliance on
        // permission bits, which root ignores).
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"blocker").unwrap();

        let token = c.valid_token().await.expect("refresh must still succeed");
        assert_eq!(token, "tok-2", "the live session must see the new token");

        // The new token is not expired, so this must NOT hit the refresh
        // endpoint again — if the guard had kept the old (now-revoked)
        // refresh_token, this would retry with it and the mock's `expect(1)`
        // would fail on drop.
        let token2 = c.valid_token().await.expect("must reuse the in-memory token");
        assert_eq!(token2, "tok-2");

        let _ = std::fs::remove_file(dir);
    }

    /// Issue #568/#573: two clients sharing one `token.json` (a tmux session
    /// on the server plus a local one) invalidate each other's in-memory
    /// grant, because XF rotates the refresh token on every refresh. The
    /// instance that lost the race asks the token endpoint with its
    /// now-revoked refresh token and is told `invalid_grant`. Round 6 had
    /// `valid_token` adopt whatever *different* refresh token it found on
    /// disk right here and hand back its access token — but `TokenSet`
    /// carries no user id, so that set could belong to a different account
    /// entirely, and nothing would tell the app: the header, `as <user>`,
    /// and every "you" assumption would keep showing the old identity while
    /// the call (and any write after it) went out as whoever's token was on
    /// disk. `valid_token` must instead clear the in-memory session and
    /// return `Error::NoToken` — routing through the app-level boundary
    /// (`recheck_stored_session` -> `adopt_stored_tokens` -> `/me` ->
    /// `Msg::Bootstrap`) that re-verifies identity before anything else runs.
    /// The store itself is left untouched: it may hold a sibling's — or a
    /// different account's — perfectly good session, and only the app-level
    /// recheck may adopt it.
    #[tokio::test]
    async fn a_refresh_rejected_with_a_different_token_set_on_disk_ends_the_session_without_adopting_it()
     {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-sibling-rotation";
        let env = EnvGuard::hold(&server.uri(), dir);
        let dir = env.dir.to_string_lossy().to_string();
        // This process's refresh token is the one that gets rejected.
        Mock::given(method("POST"))
            .and(path("/api/oauth2/token"))
            .and(wiremock::matchers::body_string_contains("refresh_token=refresh-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errors": [{"code": "invalid_grant", "message": "Invalid refresh token"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        // If the old adoption behavior regressed, this call would go out
        // under the foreign identity — assert it never fires.
        Mock::given(method("GET"))
            .and(path("/api/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nodes": [{"node_id": 4, "title": "Windows News", "node_type_id": "Forum"}]
            })))
            .expect(0)
            .mount(&server)
            .await;

        let c = WfApiClient::new().unwrap();
        c.set_tokens(TokenSet {
            access_token: "expired".into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() - 10,
            scope: "test".into(),
        })
        .await
        .unwrap();
        // A different token set sits on disk — a sibling's rotation, or (the
        // threat model this closes) a different account's session left on a
        // shared machine. Either way `valid_token` must not adopt it.
        let store = token::Store::with_path(std::path::PathBuf::from(dir).join("token.json"));
        store
            .save(&TokenSet {
                access_token: "access-2".into(),
                refresh_token: "refresh-2".into(),
                expires_at: OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();

        let err = c.valid_token().await.expect_err("must not adopt silently");
        assert!(matches!(err, Error::NoToken), "expected NoToken, got {err:?}");
        // No call went out under the foreign identity, and no write can
        // follow: the in-memory session is gone.
        assert!(!c.has_tokens().await, "the live session must be cleared, not swapped");
        // The store is untouched — the foreign set is still there for the
        // app-level recheck (which verifies identity via `/me`) to adopt.
        assert_eq!(
            store.load().unwrap().expect("the store must survive").refresh_token,
            "refresh-2"
        );
    }

    /// Companion to the above at the call-site level: even a write-shaped
    /// call (`nodes()` stands in for any authenticated request) must come
    /// back `NoToken` rather than quietly succeeding as whoever's token set
    /// is on disk — the mock's `expect(0)` on `/api/nodes` is the assertion
    /// that no request ever leaves under the unverified identity.
    #[tokio::test]
    async fn a_call_after_invalid_grant_with_a_foreign_token_on_disk_never_goes_out() {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-sibling-rotation-call-site";
        let env = EnvGuard::hold(&server.uri(), dir);
        let dir = env.dir.to_string_lossy().to_string();
        Mock::given(method("POST"))
            .and(path("/api/oauth2/token"))
            .and(wiremock::matchers::body_string_contains("refresh_token=refresh-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errors": [{"code": "invalid_grant", "message": "Invalid refresh token"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nodes": [{"node_id": 4, "title": "Windows News", "node_type_id": "Forum"}]
            })))
            .expect(0)
            .mount(&server)
            .await;

        let c = WfApiClient::new().unwrap();
        c.set_tokens(TokenSet {
            access_token: "expired".into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() - 10,
            scope: "test".into(),
        })
        .await
        .unwrap();
        let store = token::Store::with_path(std::path::PathBuf::from(dir).join("token.json"));
        store
            .save(&TokenSet {
                access_token: "access-2".into(),
                refresh_token: "refresh-2".into(),
                expires_at: OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();

        let result = c.nodes().await;
        assert!(matches!(result, Err(Error::NoToken)), "expected NoToken, got {result:?}");
    }

    /// The other half of #568: when the store holds the *same* refresh token
    /// the server just rejected, there is no sibling to recover from and the
    /// session really is over — the error propagates and the store is wiped
    /// so the next start goes straight to sign-in.
    #[tokio::test]
    async fn a_refresh_rejected_with_nothing_newer_on_disk_still_ends_the_session() {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-sibling-none";
        let env = EnvGuard::hold(&server.uri(), dir);
        let dir = env.dir.to_string_lossy().to_string();
        Mock::given(method("POST"))
            .and(path("/api/oauth2/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "errors": [{"code": "invalid_grant", "message": "Invalid refresh token"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = WfApiClient::new().unwrap();
        c.set_tokens(TokenSet {
            access_token: "expired".into(),
            refresh_token: "refresh-1".into(),
            expires_at: OffsetDateTime::now_utc().unix_timestamp() - 10,
            scope: "test".into(),
        })
        .await
        .unwrap();

        match c.nodes().await.unwrap_err() {
            Error::OAuth { code, .. } => assert_eq!(code, "invalid_grant"),
            other => panic!("expected the OAuth rejection, got {other:?}"),
        }
        assert!(!c.has_tokens().await, "the dead session must be cleared");
        let store = token::Store::with_path(std::path::PathBuf::from(dir).join("token.json"));
        assert!(
            matches!(store.load(), Ok(None)),
            "the store held the rejected token, so it must be erased"
        );
    }

    #[tokio::test]
    async fn find_user_decodes_exact_match_or_none() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-finduser");
        Mock::given(method("GET"))
            .and(path("/api/users/find-name"))
            .and(wiremock::matchers::query_param("username", "alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exact": {"user_id": 42, "username": "alice", "message_count": 10},
                "recommendations": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/users/find-name"))
            .and(wiremock::matchers::query_param("username", "unknown"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exact": null,
                "recommendations": [{"user_id": 99, "username": "unknown_other"}]
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let found = c.find_user("alice").await.unwrap();
        assert_eq!(found.map(|u| u.user_id), Some(42));

        let not_found = c.find_user("unknown").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn reply_conversation_succeeds_and_posts_message() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-replyconv");
        Mock::given(method("POST"))
            .and(path("/api/conversation-messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "message": {
                    "message_id": 100,
                    "conversation_id": 5,
                    "message_date": 1700000000,
                    "user_id": 1,
                    "username": "tester",
                    "message": "hello conversation"
                }
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let res = c.reply_conversation(5, "hello conversation").await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn search_advanced_sends_filters() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-searchadv");
        Mock::given(method("POST"))
            .and(path("/api/search"))
            .and(wiremock::matchers::body_string_contains("keywords=windows"))
            .and(wiremock::matchers::body_string_contains("c%5Busers%5D=Alice"))
            .and(wiremock::matchers::body_string_contains("search_type=thread"))
            .and(wiremock::matchers::body_string_contains("order=date"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"success": true, "search": {"search_id": 88}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/search/88"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"type": "thread", "id": 12, "result": {"title": "Windows Tip", "username": "Alice"}}],
                "pagination": {"current_page": 1, "last_page": 1, "total": 1}
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let res = c
            .search_advanced(&SearchQuery {
                keywords: "windows".into(),
                user: Some("Alice".into()),
                content_type: Some("thread".into()),
                order: Some("date".into()),
                page: 1,
            })
            .await
            .unwrap();
        assert_eq!(res.results.len(), 1);
        assert_eq!(res.results[0].title, "Windows Tip");
        assert_eq!(res.results[0].username, "Alice");
    }

    #[tokio::test]
    async fn search_member_queries_member_endpoint() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-searchmem");
        Mock::given(method("POST"))
            .and(path("/api/search/member"))
            .and(wiremock::matchers::body_string_contains("user_id=42"))
            .and(wiremock::matchers::body_string_contains("content=thread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"success": true, "search": {"search_id": 99}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/search/99"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"type": "thread", "id": 15, "result": {"title": "Member Thread"}}],
                "pagination": {"current_page": 1, "last_page": 1, "total": 1}
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let res = c.search_member(42, "thread", 1).await.unwrap();
        assert_eq!(res.results.len(), 1);
        assert_eq!(res.results[0].title, "Member Thread");
    }

    #[tokio::test]
    async fn post_with_two_attachments_and_author_avatar_deserializes() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-postattach");
        Mock::given(method("GET"))
            .and(path("/api/threads/500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "thread": {"thread_id": 500, "title": "Screens"},
                "posts": [{
                    "post_id": 1,
                    "thread_id": 500,
                    "user_id": 10,
                    "username": "op",
                    "message": "[IMG]1[/IMG] [IMG]2[/IMG]",
                    "attach_count": 2,
                    "User": {
                        "user_id": 10,
                        "username": "op",
                        "avatar_urls": {
                            "s": "https://windowsforum.com/data/avatars/s/0/10.jpg",
                            "m": "https://windowsforum.com/data/avatars/m/0/10.jpg",
                            "l": "https://windowsforum.com/data/avatars/l/0/10.jpg",
                            "h": "https://windowsforum.com/data/avatars/h/0/10.jpg",
                            "o": "https://windowsforum.com/data/avatars/o/0/10.jpg"
                        }
                    },
                    "Attachments": [
                        {
                            "attachment_id": 101,
                            "content_type": "post",
                            "content_id": 1,
                            "attach_date": 1700000000,
                            "view_count": 3,
                            "filename": "screenshot.png",
                            "file_size": 204800,
                            "width": 1920,
                            "height": 1080,
                            "is_video": false,
                            "is_audio": false,
                            "thumbnail_url": "https://windowsforum.com/attachments/screenshot-png.101/thumb",
                            "direct_url": "https://windowsforum.com/attachments/screenshot-png.101/"
                        },
                        {
                            "attachment_id": 102,
                            "content_type": "post",
                            "content_id": 1,
                            "attach_date": 1700000001,
                            "view_count": 0,
                            "filename": "log.txt",
                            "file_size": 512,
                            "width": null,
                            "height": null,
                            "is_video": false,
                            "is_audio": false,
                            "direct_url": "https://windowsforum.com/attachments/log-txt.102/"
                        }
                    ]
                }],
                "pagination": {"current_page": 1, "last_page": 1, "total": 1}
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let reply = c.thread(500, 1).await.unwrap();
        let post = &reply.posts[0];

        assert_eq!(post.attachments.len(), 2);
        let img = &post.attachments[0];
        assert_eq!(img.attachment_id, 101);
        assert_eq!(img.filename, "screenshot.png");
        assert_eq!(img.extension(), "png");
        assert!(img.is_image());
        assert_eq!(img.width, Some(1920));
        assert_eq!(img.height, Some(1080));
        assert_eq!(img.file_size, 204800);
        assert!(img.thumbnail_url.as_deref().unwrap().ends_with("/thumb"));
        assert!(img.direct_url.is_some());
        assert!(img.view_url.is_none());

        let file = &post.attachments[1];
        assert_eq!(file.extension(), "txt");
        assert!(!file.is_image());
        assert!(file.thumbnail_url.is_none(), "non-image has no thumbnail");
        assert!(file.width.is_none());

        let author = post.user.as_ref().expect("post carries its author");
        assert_eq!(author.username, "op");
        let avatars = author.avatar_urls.as_ref().expect("avatar_urls present");
        assert!(avatars.s.as_deref().unwrap().contains("/s/"));
        assert!(avatars.m.as_deref().unwrap().contains("/m/"));
        assert!(avatars.l.as_deref().unwrap().contains("/l/"));
        assert!(avatars.h.as_deref().unwrap().contains("/h/"));
        assert!(avatars.o.as_deref().unwrap().contains("/o/"));
    }

    #[tokio::test]
    async fn post_with_no_attachments_defaults_to_empty_vec() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-postnoattach");
        Mock::given(method("GET"))
            .and(path("/api/threads/501"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "thread": {"thread_id": 501, "title": "No pics"},
                // Real XF omits the "Attachments" key entirely when
                // attach_count is 0 (setupApiResultData only calls
                // includeRelation when $this->attach_count is truthy).
                "posts": [{"post_id": 2, "thread_id": 501, "username": "op", "message": "text only"}],
                "pagination": {"current_page": 1, "last_page": 1, "total": 1}
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let reply = c.thread(501, 1).await.unwrap();
        assert!(reply.posts[0].attachments.is_empty());
        assert!(reply.posts[0].user.is_none());
    }

    #[tokio::test]
    async fn user_avatar_urls_parse_all_five_sizes() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-useravatar");
        Mock::given(method("GET"))
            .and(path("/api/users/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": {
                    "user_id": 77,
                    "username": "Aria",
                    "avatar_urls": {
                        "s": "https://windowsforum.com/data/avatars/s/0/77.jpg",
                        "m": "https://windowsforum.com/data/avatars/m/0/77.jpg",
                        "l": "https://windowsforum.com/data/avatars/l/0/77.jpg",
                        "h": "https://windowsforum.com/data/avatars/h/0/77.jpg",
                        "o": "https://windowsforum.com/data/avatars/o/0/77.jpg"
                    }
                }
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let user = c.user(77).await.unwrap();
        let avatars = user.avatar_urls.unwrap();
        assert_eq!(avatars.s.as_deref(), Some("https://windowsforum.com/data/avatars/s/0/77.jpg"));
        assert_eq!(avatars.o.as_deref(), Some("https://windowsforum.com/data/avatars/o/0/77.jpg"));
    }

    #[tokio::test]
    async fn fetch_bytes_refuses_non_image_content_type() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-fetchbadtype");
        Mock::given(method("GET"))
            .and(path("/not-an-image"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html; charset=utf-8")
                    .set_body_bytes(b"<html>nope</html>".to_vec()),
            )
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let url = format!("{}/not-an-image", server.uri());
        match c.fetch_bytes(&url, 1_000_000).await.unwrap_err() {
            Error::FetchRejected(msg) => assert!(msg.contains("text/html"), "{msg}"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[tokio::test]
    async fn fetch_bytes_enforces_size_cap() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-fetchtoobig");
        Mock::given(method("GET"))
            .and(path("/big.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/png")
                    .set_body_bytes(vec![0u8; 2048]),
            )
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let url = format!("{}/big.png", server.uri());
        match c.fetch_bytes(&url, 1024).await.unwrap_err() {
            Error::FetchRejected(msg) => assert!(msg.contains("1024"), "{msg}"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[tokio::test]
    async fn fetch_bytes_returns_image_body_under_cap() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-fetchok");
        Mock::given(method("GET"))
            .and(path("/thumb.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/png")
                    .set_body_bytes(vec![7u8; 256]),
            )
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let url = format!("{}/thumb.png", server.uri());
        let bytes = c.fetch_bytes(&url, 1024).await.unwrap();
        assert_eq!(bytes.len(), 256);
        assert!(bytes.iter().all(|&b| b == 7));
    }

    /// Issue #526: a chunked body with no `Content-Length` at all must still
    /// be capped without ever buffering the whole thing — the old
    /// `resp.bytes().await` read everything before checking `bytes.len()`.
    /// `Transfer-Encoding: chunked` here is real framing (verified against a
    /// live wiremock server), not a header lied about: reqwest reports
    /// `content_length() == None` for it, exactly the "Content-Length
    /// absent" case the finding describes.
    #[tokio::test]
    async fn fetch_bytes_caps_a_chunked_body_with_no_content_length() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-fetchchunked");
        Mock::given(method("GET"))
            .and(path("/chunked.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/png")
                    .insert_header("Transfer-Encoding", "chunked")
                    .set_body_bytes(vec![9u8; 4096]),
            )
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let url = format!("{}/chunked.png", server.uri());
        match c.fetch_bytes(&url, 1024).await.unwrap_err() {
            Error::FetchRejected(msg) => assert!(msg.contains("1024"), "{msg}"),
            other => panic!("wrong error: {other}"),
        }
    }

    #[tokio::test]
    async fn react_post_sends_reaction_id() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-react");
        Mock::given(method("POST"))
            .and(path("/api/posts/101/react"))
            .and(wiremock::matchers::body_string_contains("reaction_id=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "action": "insert"
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        assert_eq!(c.react_post(101, 1).await.unwrap(), Toggle::Inserted);
    }

    /// Issue #543: `Gate` is FIFO with no priority, so while thumbnails shared
    /// `api_gate` a screenful of them reserved a 250 ms slot each and the next
    /// thing the user asked for started only after all of them. Ten queued
    /// image fetches must now cost the interactive call at most its own slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_image_fetches_do_not_delay_the_next_interactive_request() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-imggate");
        Mock::given(method("GET"))
            .and(path("/thumb.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/png")
                    .set_body_bytes(vec![7u8; 64]),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/threads/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "thread": {"thread_id": 1, "title": "T"},
                "posts": [],
                "pagination": {"current_page": 1, "last_page": 1, "total": 0}
            })))
            .mount(&server)
            .await;

        let c = Arc::new(logged_in_client("tok-1").await);
        let url = format!("{}/thumb.png", server.uri());
        // Exactly what `App::draw` does on the first frame of a cold thread.
        let mut loads = Vec::new();
        for _ in 0..10 {
            let c = c.clone();
            let url = url.clone();
            loads.push(tokio::spawn(async move {
                let _ = c.fetch_bytes(&url, 1024).await;
            }));
        }
        // Let all ten reserve their slots before the user navigates.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let thread = c.thread(1, 1).await.expect("thread");
        let waited = started.elapsed();
        assert_eq!(thread.thread.thread_id, 1);
        // One interactive slot (250 ms) plus the mock round trip. On the
        // shared gate this was ten slots ≈ 2.5 s.
        assert!(
            waited < Duration::from_millis(900),
            "the interactive request queued behind decoration: {waited:?}"
        );
        for l in loads {
            let _ = l.await;
        }
    }

    /// #643: a 429 from the attachment-data endpoint must classify as
    /// `RateLimited` (Retry-After reaching the gate), not die as a plain
    /// `Error::Http` from `error_for_status`.
    #[tokio::test]
    async fn attachment_data_429_extends_the_gate_and_keeps_retry_after() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-att-429");
        Mock::given(method("GET"))
            .and(path("/api/attachments/9/data"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "12"))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        match c.attachment_data(9, config::MAX_ATTACHMENT_BYTES).await.unwrap_err() {
            Error::RateLimited { retry_after: Some(d) } => assert_eq!(d.as_secs(), 12),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(
            c.api_gate.pending_wait() >= Duration::from_secs(10),
            "Retry-After must reach the gate"
        );
    }

    /// #643: the download is capped at `Content-Length` and again on the
    /// streamed body, like `fetch_bytes` (#526) — never buffered whole.
    #[tokio::test]
    async fn attachment_data_enforces_the_byte_cap() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-att-cap");
        Mock::given(method("GET"))
            .and(path("/api/attachments/9/data"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![3u8; 4096]))
            .mount(&server)
            .await;
        let c = logged_in_client("tok-1").await;
        match c.attachment_data(9, 1024).await.unwrap_err() {
            Error::FetchRejected(msg) => assert!(msg.contains("1024"), "{msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    /// #644: an upload is a flood-checked write — on success the write gate
    /// is re-anchored (like `post_form`), so the reply this attachment
    /// belongs to cannot fire with the slot wide open after a slow upload.
    #[tokio::test]
    async fn upload_attachment_re_anchors_the_write_gate_on_success() {
        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-upload-gate");
        Mock::given(method("POST"))
            .and(path("/api/attachments/new-key"))
            // The REAL shape: `AttachmentsController::actionPostNewKey`
            // returns `['key' => ...]`. This fixture said `attachment_key`
            // and the decoder believed it, so the upload path would have
            // failed against production while the test passed (#709, and the
            // same lesson as #696).
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "key": "key-1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/attachments/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attachment": {"attachment_id": 55, "filename": "shot.png"}
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        let before = c.write_gate.pending_wait();
        let (key, attachment) = c
            .upload_attachment("post", &[], "shot.png".into(), vec![1, 2, 3], "image/png", None)
            .await
            .unwrap();
        // The key is what ties the upload to the post that follows it.
        assert_eq!(key, "key-1");
        assert_eq!(attachment.attachment_id, 55);
        let after = c.write_gate.pending_wait();
        assert!(
            after >= before + Duration::from_secs(29),
            "success must anchor a 30s write cool-down, went {before:?} -> {after:?}"
        );
    }

    #[tokio::test]
    async fn react_and_vote_report_the_servers_delete_action_as_a_removal() {        let server = MockServer::start().await;
        let _env = EnvGuard::hold(&server.uri(), "/tmp/wftui-t-react-del");
        // Sending the reaction/type that is already set is XF's *undo*; the
        // body is the only thing that says so (issue #538).
        Mock::given(method("POST"))
            .and(path("/api/posts/101/react"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "action": "delete"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/posts/101/vote"))
            .and(wiremock::matchers::body_string_contains("type=up"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "action": "delete"
            })))
            .mount(&server)
            .await;
        // A body with no `action` at all still reads as an insert.
        Mock::given(method("POST"))
            .and(path("/api/posts/102/vote"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true
            })))
            .mount(&server)
            .await;

        let c = logged_in_client("tok-1").await;
        assert_eq!(c.react_post(101, 1).await.unwrap(), Toggle::Removed);
        assert_eq!(c.vote_post(101, "up").await.unwrap(), Toggle::Removed);
        assert_eq!(c.vote_post(102, "up").await.unwrap(), Toggle::Inserted);
    }
}
