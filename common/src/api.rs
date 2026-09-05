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
    pub api_gate: Arc<Gate>,
    pub search_gate: Arc<Gate>,
    pub write_gate: Arc<Gate>,
}

impl WfApiClient {
    pub fn new() -> Result<Self> {
        Ok(WfApiClient {
            http: crate::http::build()?,
            tokens: Mutex::new(token::Store::new().load()?),
            store: token::Store::new(),
            api_gate: Arc::new(Gate::new(config::GLOBAL_MIN_INTERVAL_MS)),
            search_gate: Arc::new(Gate::new(config::SEARCH_MIN_INTERVAL_MS)),
            write_gate: Arc::new(Gate::new(config::WRITE_COOLDOWN_MS)),
        })
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

    pub async fn forget_tokens(&self) -> Result<()> {
        self.tokens.lock().await.take();
        self.store.erase()
    }

    /// Current token, refreshed silently if expired. Refreshes under the lock
    /// so concurrent calls collapse into one network round-trip.
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
        let refreshed = oauth::refresh(&self.http, &refresh_token).await.inspect_err(|e| {
            if matches!(e, Error::OAuth { code, .. } if code == "invalid_grant") {
                // Refresh token expired/revoked: force a clean re-login. Only
                // wipe the store if it still holds this refresh token (the
                // user may have re-logged in from another code path).
                if self
                    .store
                    .load()
                    .ok()
                    .flatten()
                    .is_some_and(|t| t.refresh_token == refresh_token)
                {
                    let _ = self.store.erase();
                }
                *guard = None;
            }
        })?;
        // Update the live session first: XF's refresh grant rotates the
        // refresh token (revokes the old one server-side in the same
        // request), so once `oauth::refresh` above has succeeded the old
        // token set is already dead. If persisting the new one to disk then
        // fails (ENOSPC, a permission change, a read-only remount), that
        // must cost only durability across a restart, not the live session —
        // demoting it to a warning avoids stranding the revoked refresh
        // token in `*guard`/the store, which would otherwise force a full
        // browser re-login on the very next call (see issue #513).
        *guard = Some(refreshed.clone());
        if let Err(e) = self.store.save(&refreshed) {
            tracing::warn!("token refresh persisted in memory but not to disk: {e}");
        }
        Ok(refreshed.access_token)
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
        let url = format!("{}{path}", config::api_base());
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
        let url = format!("{}{path}", config::api_base());
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
        let url = format!("{}{path}", config::api_base());
        let resp = self.http.post(url).form(form).bearer_auth(&token).send().await?;
        check_status(resp)
            .await
            .inspect_err(|e| self.note_rate_limit(e, &[&self.api_gate, &self.write_gate]))
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
        let key_url = format!("{}/attachments/new-key", config::api_base());
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
        let new_key: NewKey = decode(key_resp).await?;

        self.api_gate.wait().await;
        let upload_url = format!("{}/attachments/", config::api_base());
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
        decode(resp).await
    }

    /// Fetch attachment bytes (the TUI writes them to a temp file and opens).
    pub async fn attachment_data(&self, attachment_id: u32) -> Result<Vec<u8>> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}/attachments/{attachment_id}/data", config::api_base());
        let resp = self.http.get(url).bearer_auth(&token).send().await?.error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Fetch raw bytes from an absolute URL — the images tier
    /// (`wftui/src/images.rs`) uses this for `thumbnail_url`/`direct_url`/
    /// `avatar_urls` values off `Attachment`/`User`. Inherent, not on
    /// `WfApi`: same reasoning as `attachment_data` above — this is a raw-HTTP
    /// concern, not a forum-data operation the trait's mock implementations
    /// need to fake.
    ///
    /// Goes through `api_gate` like every other call (one shared politeness
    /// budget), and the request carries the client UA because it's the same
    /// pooled `reqwest::Client` every other method uses — `http::build()`
    /// bakes `config::user_agent()` in at construction (hard rule 6), so
    /// there is nothing extra to set per-request.
    ///
    /// Refuses anything whose `Content-Type` doesn't start with `image/`, and
    /// caps the body at `max_bytes`: first cheaply, via `Content-Length` if
    /// the server sent one; then for real, by checking the length of the
    /// bytes actually received (a lying or missing `Content-Length` must not
    /// bypass the cap — the buffer is still fully read either way, same as
    /// `attachment_data` above, but thumbnails are small enough that this is
    /// not worth a streaming dependency).
    pub async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>> {
        self.api_gate.wait().await;
        let resp = self.http.get(url).send().await?.error_for_status()?;

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

        let bytes = resp.bytes().await?;
        if bytes.len() > max_bytes {
            return Err(Error::FetchRejected(format!(
                "response body {} bytes exceeds cap {max_bytes} for {url}",
                bytes.len()
            )));
        }
        Ok(bytes.to_vec())
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
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
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
    async fn react_post(&self, post_id: u32, reaction_id: u32) -> Result<()>;
    async fn vote_post(&self, post_id: u32, vote: &str) -> Result<()>;
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
        let url = format!("{}/conversations/{id}", config::api_base());
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
        let url = format!("{}/search", config::api_base());
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
        let url = format!("{}/search/member", config::api_base());
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

    async fn react_post(&self, post_id: u32, reaction_id: u32) -> Result<()> {
        self.post_unit(
            &format!("/posts/{post_id}/react"),
            &[("reaction_id", reaction_id.to_string())],
        )
        .await
    }

    async fn vote_post(&self, post_id: u32, vote: &str) -> Result<()> {
        self.post_unit(
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
    }

    impl EnvGuard {
        fn hold(base: &str, dir: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var("WFTUI_BASE_URL", base) };
            unsafe { std::env::set_var("WFTUI_OAUTH_CLIENT_ID", "test-client") };
            unsafe { std::env::set_var("WFTUI_CONFIG_DIR", dir) };
            let _ = std::fs::remove_dir_all(dir);
            EnvGuard {
                _lock: lock,
                dir: std::path::PathBuf::from(dir),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("WFTUI_BASE_URL") };
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Caller must already hold `EnvGuard` (the env is process-global).
    async fn logged_in_client(token: &str) -> WfApiClient {
        let c = WfApiClient::new().unwrap();
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

    #[tokio::test]
    async fn expired_access_token_triggers_refresh() {
        let server = MockServer::start().await;
        let dir = "/tmp/wftui-t-refresh";
        let _env = EnvGuard::hold(&server.uri(), dir);
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
        let _env = EnvGuard::hold(&server.uri(), dir);
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
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::write(dir, b"blocker").unwrap();

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
        let res = c.react_post(101, 1).await;
        assert!(res.is_ok());
    }
}
