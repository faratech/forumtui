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
        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        Ok(refreshed.access_token)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.api_gate.wait().await;
        let token = self.valid_token().await?;
        let url = format!("{}{path}", config::api_base());
        let resp = self.http.get(url).query(query).bearer_auth(&token).send().await?;
        decode(resp).await
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
        check_status(resp).await
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
    async fn me(&self) -> Result<User>;
    async fn user(&self, id: u32) -> Result<User>;
    async fn find_user(&self, username: &str) -> Result<Option<User>>;
}

#[async_trait]
impl WfApi for WfApiClient {
    async fn nodes(&self) -> Result<Vec<Node>> {
        let reply: NodesReply = self.get("/nodes", &[]).await?;
        Ok(reply.nodes)
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
        self.post_unit(
            "/conversation-messages",
            &[
                ("conversation_id", id.to_string()),
                ("message", message.to_string()),
            ],
        )
        .await
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
        let resp = self
            .http
            .post(url)
            .form(&[("keywords", keywords)])
            .bearer_auth(&token)
            .send()
            .await?;
        let created: SearchCreated = decode(resp).await?;
        let Some(search) = created.search else {
            return Ok(SearchResultsReply::default());
        };
        let search_id = search.search_id;
        self.get(&format!("/search/{search_id}"), &[("page", page.to_string())])
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
}
