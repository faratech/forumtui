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

    pub async fn upload_attachment(
        &self,
        content_type: &str,
        context: &[(&str, String)],
        filename: String,
        bytes: Vec<u8>,
        mime: &str,
    ) -> Result<Attachment> {
        self.api_gate.wait().await;
        self.write_gate.wait().await;
        let token = self.valid_token().await?;

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
            attachment_key: String,
        }
        let new_key: NewKey = decode(key_resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]))?;

        self.api_gate.wait().await;
        let upload_url = format!("{}/attachments/", self.api_base());
        let file_part =
            reqwest::multipart::Part::bytes(bytes).file_name(filename).mime_str(mime)?;
        let form = reqwest::multipart::Form::new()
            .text("key", new_key.attachment_key)
            .part("attachment", file_part);
        let resp = self
            .http
            .post(upload_url)
            .multipart(form)
            .bearer_auth(&token)
            .timeout(config::UPLOAD_TIMEOUT)
            .send()
            .await?;
        let out: Result<Attachment> = decode(resp).await;
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
        let mut resp = self.http.get(url).send().await?.error_for_status()?;

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
    async fn reply(&self, thread_id: u32, message: &str) -> Result<Post>;
    async fn create_thread(&self, node_id: u32, title: &str, message: &str) -> Result<Thread>;
    async fn mark_thread_read(&self, id: u32) -> Result<()>;
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

    async fn reply(&self, thread_id: u32, message: &str) -> Result<Post> {
        #[derive(serde::Deserialize)]
        struct PostCreated {
            post: Post,
        }
        let created: PostCreated = self
            .post_form(
                "/posts",
                &[
                    ("thread_id", thread_id.to_string()),
                    ("message", message.to_string()),
                ],
                None,
            )
            .await?;
        Ok(created.post)
    }

    async fn create_thread(&self, node_id: u32, title: &str, message: &str) -> Result<Thread> {
        #[derive(serde::Deserialize)]
        struct ThreadCreated {
            thread: Thread,
        }
        let created: ThreadCreated = self
            .post_form(
                "/threads",
                &[
                    ("node_id", node_id.to_string()),
                    ("title", title.to_string()),
                    ("message", message.to_string()),
                ],
                Some(Duration::from_millis(config::NEW_THREAD_COOLDOWN_MS)),
            )
            .await?;
        Ok(created.thread)
    }

    async fn mark_thread_read(&self, id: u32) -> Result<()> {
        self.post_unit_path(&format!("/threads/{id}/mark-read")).await
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

    /// Issue #557: two instances sharing one config dir rotate each other's
    /// refresh token out from under themselves. `adopt_stored_tokens` must
    /// pick up what the sibling wrote — and must report `false` (nothing to
    /// recover) when the file is missing or already the token set in memory,
    /// so the caller can end the session instead of looping.
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "attachment_key": "key-1"
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
        c.upload_attachment("post", &[], "shot.png".into(), vec![1, 2, 3], "image/png")
            .await
            .unwrap();
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
