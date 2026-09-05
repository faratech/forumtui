//! Serde models for the XF REST API surface the TUI consumes.
//!
//! Lenient by design: every non-identity field has a default so an additive
//! API change can never brick the client. List replies follow XF's envelope
//! `<collection> + pagination` (AbstractController::getPaginationData).

use serde::{Deserialize, Serialize};

pub fn r<T: Default>() -> T {
    T::default()
}

/// Serde default for booleans whose *absent* meaning is `true`.
///
/// `#[serde(default)]` on a `bool` yields `false`, which for
/// `Thread::discussion_open` reads as "closed" and would stamp the locked
/// glyph on every row of a partial payload. Absent means open.
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pagination {
    #[serde(default, rename = "current_page")]
    pub current_page: u32,
    #[serde(default, rename = "last_page")]
    pub last_page: u32,
    #[serde(default)]
    pub total: u64,
}

/// `{"errors":[{"code":..,"message":..,"params":..}]}`
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub errors: Vec<ApiErrorItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorItem {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Node {
    #[serde(default)]
    pub node_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "node_type_id")]
    pub node_type: String,
    #[serde(default, rename = "parent_node_id")]
    pub parent_node_id: u32,
    #[serde(default)]
    pub depth: u32,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodesReply {
    #[serde(default)]
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Forum {
    #[serde(default)]
    pub node_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "thread_count")]
    pub thread_count: u64,
    #[serde(default, rename = "message_count")]
    pub message_count: u64,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Thread {
    #[serde(default)]
    pub thread_id: u32,
    #[serde(default)]
    pub node_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub user_id: u32,
    #[serde(default, rename = "post_date")]
    pub post_date: i64,
    #[serde(default, rename = "reply_count")]
    pub reply_count: u64,
    #[serde(default, rename = "view_count")]
    pub view_count: u64,
    #[serde(default, rename = "last_post_date")]
    pub last_post_date: i64,
    #[serde(default, rename = "last_post_username")]
    pub last_post_username: String,
    #[serde(default)]
    pub sticky: bool,
    #[serde(default = "default_true", rename = "discussion_open")]
    pub discussion_open: bool,
    #[serde(default, rename = "discussion_type")]
    pub discussion_type: String,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "vote_score")]
    pub vote_score: i64,
    #[serde(default, rename = "highlighted_post_ids")]
    pub highlighted_post_ids: Vec<u32>,
    #[serde(default, rename = "type_data")]
    pub type_data: serde_json::Value,
    #[serde(default, rename = "is_unread")]
    pub is_unread: bool,
    #[serde(default, rename = "is_watching")]
    pub is_watching: bool,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

impl Thread {
    pub fn is_question(&self) -> bool {
        self.discussion_type == "question"
    }

    pub fn is_article(&self) -> bool {
        self.discussion_type == "article"
    }

    pub fn is_suggestion(&self) -> bool {
        self.discussion_type == "suggestion"
    }

    pub fn is_poll(&self) -> bool {
        self.discussion_type == "poll"
    }

    pub fn has_solution(&self) -> bool {
        !self.highlighted_post_ids.is_empty()
            || self
                .type_data
                .get("solution_post_id")
                .and_then(|v| v.as_u64())
                .is_some_and(|id| id > 0)
    }

    pub fn solution_post_id(&self) -> Option<u32> {
        self.highlighted_post_ids
            .first()
            .copied()
            .or_else(|| {
                self.type_data
                    .get("solution_post_id")
                    .and_then(|v| v.as_u64())
                    .map(|id| id as u32)
            })
            .filter(|&id| id > 0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Post {
    #[serde(default)]
    pub post_id: u32,
    #[serde(default)]
    pub thread_id: u32,
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub username: String,
    #[serde(default, rename = "post_date")]
    pub post_date: i64,
    /// BBCode source, as returned by the API.
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "attach_count")]
    pub attach_count: u32,
    #[serde(default, rename = "reaction_score")]
    pub reaction_score: i64,
    #[serde(default, rename = "vote_score")]
    pub vote_score: i64,
    #[serde(default, rename = "is_first_post")]
    pub is_first_post: bool,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
    /// `XF\Entity\Post::setupApiResultData()` only calls
    /// `$result->includeRelation('Attachments')` when `attach_count` is
    /// non-zero, so the key is simply absent (not `[]`) on a post with none —
    /// `#[serde(default)]` covers that.
    #[serde(default, rename = "Attachments")]
    pub attachments: Vec<Attachment>,
    /// The post author, when the payload carries one. `Post`'s `api`
    /// with-alias always includes `User`/`User.api` (the relation is flagged
    /// `'api' => true` in `XF\Entity\Post::getStructure()`), so this is
    /// populated on every real API response; kept `Option` for lenient
    /// parsing of hand-built test fixtures and the (unlikely) case of a
    /// missing/deleted author.
    #[serde(default, rename = "User")]
    pub user: Option<User>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadsReply {
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ForumReply {
    #[serde(default)]
    pub forum: Forum,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadReply {
    #[serde(default)]
    pub thread: Thread,
    #[serde(default)]
    pub posts: Vec<Post>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PostsReply {
    #[serde(default)]
    pub posts: Vec<Post>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConversationRecipient {
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub username: String,
}

pub fn deserialize_recipients<'de, D>(deserializer: D) -> Result<Vec<ConversationRecipient>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    let mut recipients = Vec::new();
    match val {
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                let user_id = key.parse::<u32>().unwrap_or(0);
                match v {
                    serde_json::Value::String(username) => {
                        if !username.is_empty() {
                            recipients.push(ConversationRecipient { user_id, username });
                        }
                    }
                    serde_json::Value::Object(obj) => {
                        let uid = obj
                            .get("user_id")
                            .and_then(|u| u.as_u64())
                            .map(|u| u as u32)
                            .unwrap_or(user_id);
                        let uname = obj
                            .get("username")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !uname.is_empty() {
                            recipients.push(ConversationRecipient {
                                user_id: uid,
                                username: uname,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                match item {
                    serde_json::Value::String(s) => {
                        if !s.is_empty() {
                            recipients.push(ConversationRecipient {
                                user_id: 0,
                                username: s,
                            });
                        }
                    }
                    serde_json::Value::Object(obj) => {
                        let uid = obj
                            .get("user_id")
                            .and_then(|u| u.as_u64())
                            .map(|u| u as u32)
                            .unwrap_or(0);
                        let uname = obj
                            .get("username")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !uname.is_empty() {
                            recipients.push(ConversationRecipient {
                                user_id: uid,
                                username: uname,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    recipients.sort_by_key(|a| a.username.to_lowercase());
    Ok(recipients)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Conversation {
    #[serde(default)]
    pub conversation_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "user_id")]
    pub user_id: u32,
    #[serde(default, rename = "username")]
    pub username: String,
    #[serde(default, rename = "start_user_id")]
    pub start_user_id: u32,
    #[serde(default, rename = "start_username")]
    pub start_username: String,
    #[serde(default, rename = "reply_count")]
    pub reply_count: u64,
    #[serde(default, rename = "recipient_count")]
    pub recipient_count: u32,
    #[serde(default, rename = "last_message_date")]
    pub last_message_date: i64,
    #[serde(default, rename = "last_message_username")]
    pub last_message_username: String,
    #[serde(default, rename = "conversation_unread")]
    pub conversation_unread: bool,
    #[serde(default, rename = "is_unread")]
    pub is_unread: bool,
    #[serde(default, rename = "is_starred")]
    pub is_starred: bool,
    #[serde(default)]
    pub starred: bool,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_recipients")]
    pub recipients: Vec<ConversationRecipient>,
}

impl Conversation {
    pub fn is_unread_conv(&self) -> bool {
        self.conversation_unread || self.is_unread
    }

    pub fn is_starred_conv(&self) -> bool {
        self.starred || self.is_starred
    }

    pub fn starter(&self) -> &str {
        if !self.start_username.is_empty() {
            &self.start_username
        } else if !self.username.is_empty() {
            &self.username
        } else {
            ""
        }
    }

    pub fn starter_id(&self) -> u32 {
        if self.start_user_id > 0 {
            self.start_user_id
        } else {
            self.user_id
        }
    }

    /// Returns all participant usernames (starter first, then recipients), deduplicated.
    pub fn participant_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let starter = self.starter();
        if !starter.is_empty() {
            names.push(starter.to_string());
        }
        for r in &self.recipients {
            if !r.username.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&r.username)) {
                names.push(r.username.clone());
            }
        }
        names
    }

    /// Formats all participants as a human-readable list:
    /// "Alice (starter), Bob, Charlie"
    pub fn participants_display(&self) -> String {
        let starter = self.starter();
        let mut parts = Vec::new();
        if !starter.is_empty() {
            parts.push(format!("{starter} (starter)"));
        }
        for r in &self.recipients {
            if !r.username.is_empty() && !starter.eq_ignore_ascii_case(&r.username) {
                parts.push(r.username.clone());
            }
        }
        if parts.is_empty() {
            "No participants listed".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConversationsReply {
    #[serde(default)]
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConversationMessage {
    #[serde(default)]
    pub message_id: u32,
    #[serde(default)]
    pub conversation_id: u32,
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub username: String,
    #[serde(default, rename = "message_date")]
    pub message_date: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConversationReply {
    #[serde(default)]
    pub conversation: Conversation,
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessagesReply {
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Alert {
    #[serde(default)]
    pub alert_id: u32,
    #[serde(default)]
    pub alert_date: i64,
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub viewed: bool,
    #[serde(default, rename = "content_type")]
    pub content_type: String,
    #[serde(default, rename = "content_id")]
    pub content_id: u64,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AlertsReply {
    #[serde(default)]
    pub alerts: Vec<Alert>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub username: String,
    #[serde(default, rename = "message_count")]
    pub message_count: u64,
    #[serde(default, rename = "register_date")]
    pub register_date: i64,
    #[serde(default, rename = "custom_title")]
    pub custom_title: String,
    #[serde(default, rename = "reaction_score")]
    pub reaction_score: i64,
    #[serde(default, rename = "trophy_points")]
    pub trophy_points: u32,
    #[serde(default, rename = "is_staff")]
    pub is_staff: bool,
    #[serde(default, rename = "is_admin")]
    pub is_admin: bool,
    #[serde(default, rename = "is_moderator")]
    pub is_moderator: bool,
    #[serde(default, rename = "last_activity")]
    pub last_activity: i64,
    #[serde(default)]
    pub about: String,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub website: String,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
    /// `XF\Entity\User::setupApiResultData()` builds this from
    /// `avatarSizeMap` (`App.php`: `o`/`h`/`l`/`m`/`s`, largest to smallest);
    /// each value is `getAvatarUrl($size)`, which is `string|null` (`null`
    /// when the user has neither a gravatar nor an uploaded avatar) — never
    /// absent as a whole object for a real user, but `Option` here so a
    /// guest stub or hand-built fixture without it still parses.
    #[serde(default, rename = "avatar_urls")]
    pub avatar_urls: Option<AvatarUrls>,
}

/// Keys match `avatarSizeMap` in `XF/App.php` exactly: `o` (384px) → `h`
/// (384px, "huge" cropped) → `l` (192px) → `m` (96px) → `s` (48px, DESIGN.md's
/// 2-row × 5-cell avatar tier).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AvatarUrls {
    #[serde(default)]
    pub s: Option<String>,
    #[serde(default)]
    pub m: Option<String>,
    #[serde(default)]
    pub l: Option<String>,
    #[serde(default)]
    pub h: Option<String>,
    #[serde(default)]
    pub o: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserReply {
    #[serde(default)]
    pub user: User,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsersFindNameReply {
    #[serde(default)]
    pub exact: Option<User>,
    #[serde(default)]
    pub recommendations: Vec<User>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MeReply {
    #[serde(default)]
    pub me: User,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub view_url: Option<String>,
    pub message: String,
    pub username: String,
    pub date: i64,
    pub content_type: String,
    pub content_id: u64,
    pub thread_id: Option<u32>,
}

impl<'de> serde::Deserialize<'de> for SearchHit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        // Case 1: XenForo native search result format:
        // { "type": "...", "id": ..., "result": { ... } }
        if let Some(result_obj) = val.get("result").and_then(|r| r.as_object()) {
            let content_type = val
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("thread")
                .to_string();
            let content_id = val
                .get("id")
                .and_then(|id| id.as_u64())
                .unwrap_or_default();

            let username = result_obj
                .get("username")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string();

            let date = result_obj
                .get("post_date")
                .or_else(|| result_obj.get("date"))
                .and_then(|d| d.as_i64())
                .unwrap_or_default();

            let view_url = result_obj
                .get("view_url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string());

            let message = result_obj
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();

            let (title, thread_id) = if content_type == "post" {
                let tid = result_obj
                    .get("thread_id")
                    .and_then(|t| t.as_u64())
                    .map(|t| t as u32);
                let thread_title = result_obj
                    .get("Thread")
                    .and_then(|t| t.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or_default();
                let post_title = if !thread_title.is_empty() {
                    thread_title.to_string()
                } else {
                    result_obj
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                (post_title, tid)
            } else {
                let tid = if content_id > 0 {
                    Some(content_id as u32)
                } else {
                    None
                };
                let t_title = result_obj
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                (t_title, tid)
            };

            return Ok(SearchHit {
                title,
                view_url,
                message,
                username,
                date,
                content_type,
                content_id,
                thread_id,
            });
        }

        // Case 2: Flat JSON format (mock tests or simple endpoints)
        let title = val
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();
        let view_url = val
            .get("view_url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let message = val
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let username = val
            .get("username")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string();
        let date = val
            .get("date")
            .or_else(|| val.get("post_date"))
            .and_then(|d| d.as_i64())
            .unwrap_or_default();
        let content_type = val
            .get("content_type")
            .or_else(|| val.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("thread")
            .to_string();
        let content_id = val
            .get("content_id")
            .or_else(|| val.get("id"))
            .and_then(|id| id.as_u64())
            .unwrap_or_default();
        let thread_id = val
            .get("thread_id")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
            .or_else(|| {
                if content_type == "thread" && content_id > 0 {
                    Some(content_id as u32)
                } else {
                    None
                }
            });

        Ok(SearchHit {
            title,
            view_url,
            message,
            username,
            date,
            content_type,
            content_id,
            thread_id,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub keywords: String,
    pub user: Option<String>,
    pub content_type: Option<String>,
    pub order: Option<String>,
    pub page: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResultsReply {
    #[serde(default)]
    pub results: Vec<SearchHit>,
    #[serde(default)]
    pub pagination: Pagination,
}

/// Mirrors the wire shape from `XF\Entity\Attachment::setupApiResultData()` +
/// `getStructure()` exactly — nothing guessed. Real fields present on every
/// attachment: `attachment_id` (`autoIncrement`, always emitted regardless of
/// `api` flags), `content_type`/`content_id`/`attach_date`/`view_count`
/// (columns flagged `'api' => true`), `filename`/`file_size`/`height`/`width`/
/// `is_video`/`is_audio`/`direct_url` (always set as `extra` in
/// `setupApiResultData`). `thumbnail_url`/`retina_thumbnail_url` are set only
/// `if ($this->has_thumbnail)` / `has_retina_thumbnail` — genuinely absent,
/// not just empty, on a non-image attachment.
///
/// Two things XF does **not** emit that it would be easy to assume it does:
/// there is no MIME-type field (`content_type` is the entity this attachment
/// is *attached to*, e.g. `"post"` — not a media type), and there is no
/// `view_url`. Use `extension()` below to classify the file for the images
/// tier; `view_url` is kept as a lenient `Option` purely so a future XF
/// field/hand-built fixture with that key still parses — real responses will
/// leave it `None` and callers should fall back to `direct_url`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub attachment_id: u32,
    #[serde(default)]
    pub filename: String,
    /// XF's own `content_type`: the type of content this attachment is
    /// attached to (`"post"`, `"conversation_message"`, ...), NOT a MIME
    /// type. See `extension()`.
    #[serde(default, rename = "content_type")]
    pub content_type: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default, rename = "file_size")]
    pub file_size: u64,
    #[serde(default, rename = "thumbnail_url")]
    pub thumbnail_url: Option<String>,
    #[serde(default, rename = "direct_url")]
    pub direct_url: Option<String>,
    /// Not part of XF's attachment wire format (see struct docs above); kept
    /// for lenient forward-compat only.
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

impl Attachment {
    /// Lower-cased file extension parsed from `filename` — XF's attachment
    /// API exposes no MIME type or extension field directly, so this is the
    /// only way to classify the file client-side.
    pub fn extension(&self) -> String {
        match self.filename.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
            _ => String::new(),
        }
    }

    /// True for extensions the images tier (`wftui/src/images.rs`) can decode
    /// and render inline; anything else stays a text placeholder / browser
    /// open target.
    pub fn is_image(&self) -> bool {
        matches!(
            self.extension().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif"
        )
    }

    /// Best URL to open in a browser (`u` key / digit keys 1-9): prefer the
    /// real `direct_url` XF emits, fall back to the non-standard `view_url`
    /// if some future response carries one.
    pub fn open_url(&self) -> Option<&str> {
        self.direct_url.as_deref().or(self.view_url.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_block_parses() {
        let body = serde_json::json!({
            "current_page": 2, "last_page": 7, "per_page": 20,
            "shown": 20, "total": 132
        });
        let p: Pagination = serde_json::from_value(body).unwrap();
        assert_eq!((p.current_page, p.last_page), (2, 7));
        assert_eq!(p.total, 132);
    }

    #[test]
    fn threads_reply_is_lenient_about_additions() {
        let body = serde_json::json!({
            "threads": [{"thread_id": 1, "title": "T", "brand_new_field": true}],
            "pagination": {"current_page": 1, "last_page": 1, "total": 1}
        });
        let r: ThreadsReply = serde_json::from_value(body).unwrap();
        assert_eq!(r.threads[0].title, "T");
        assert!(r.threads[0].view_url.is_none());
    }

    #[test]
    fn error_body_parses() {
        let body = serde_json::json!({
            "errors": [{"code": "invalid_page", "message": "Invalid page.", "params": {"max": 4}}]
        });
        let e: ApiErrorBody = serde_json::from_value(body).unwrap();
        assert_eq!(e.errors[0].code, "invalid_page");
        assert_eq!(e.errors[0].params["max"], 4);
    }

    #[test]
    fn question_thread_solution_and_article_helpers() {
        let q_json = serde_json::json!({
            "thread_id": 42,
            "title": "How do I fix this BSOD?",
            "discussion_type": "question",
            "vote_score": 12,
            "highlighted_post_ids": [101],
            "type_data": {"solution_post_id": 101}
        });
        let thread: Thread = serde_json::from_value(q_json).unwrap();
        assert!(thread.is_question());
        assert!(!thread.is_article());
        assert!(thread.has_solution());
        assert_eq!(thread.solution_post_id(), Some(101));

        let art_json = serde_json::json!({
            "thread_id": 99,
            "title": "Windows 11 24H2 Deep Dive",
            "discussion_type": "article",
            "prefix": "Guide"
        });
        let art: Thread = serde_json::from_value(art_json).unwrap();
        assert!(art.is_article());
        assert!(!art.is_question());
        assert!(!art.has_solution());
        assert_eq!(art.prefix.as_deref(), Some("Guide"));
    }

    #[test]
    fn native_xenforo_search_hit_deserialization() {
        let post_hit_json = serde_json::json!({
            "type": "post",
            "id": 555,
            "result": {
                "post_id": 555,
                "thread_id": 789,
                "username": "SysAdmin",
                "post_date": 1720000000,
                "message": "Run sfc /scannow in cmd",
                "view_url": "https://windowsforum.com/posts/555/",
                "Thread": {
                    "thread_id": 789,
                    "title": "Corrupt system files"
                }
            }
        });
        let hit: SearchHit = serde_json::from_value(post_hit_json).unwrap();
        assert_eq!(hit.content_type, "post");
        assert_eq!(hit.content_id, 555);
        assert_eq!(hit.thread_id, Some(789));
        assert_eq!(hit.title, "Corrupt system files");
        assert_eq!(hit.username, "SysAdmin");
        assert!(hit.message.contains("sfc /scannow"));

        let flat_hit_json = serde_json::json!({
            "title": "Flat Thread",
            "content_type": "thread",
            "content_id": 123,
            "username": "User1"
        });
        let flat_hit: SearchHit = serde_json::from_value(flat_hit_json).unwrap();
        assert_eq!(flat_hit.title, "Flat Thread");
        assert_eq!(flat_hit.thread_id, Some(123));
    }

    #[test]
    fn user_full_profile_parses() {
        let u_json = serde_json::json!({
            "user_id": 10,
            "username": "AdminMike",
            "message_count": 5000,
            "reaction_score": 12500,
            "trophy_points": 450,
            "is_staff": true,
            "is_admin": true,
            "custom_title": "Administrator",
            "about": "Windows enthusiast",
            "location": "Redmond, WA",
            "website": "https://windowsforum.com"
        });
        let user: User = serde_json::from_value(u_json).unwrap();
        assert_eq!(user.username, "AdminMike");
        assert_eq!(user.reaction_score, 12500);
        assert_eq!(user.trophy_points, 450);
        assert!(user.is_staff && user.is_admin);
        assert_eq!(user.location, "Redmond, WA");
    }

    #[test]
    fn conversation_participants_deserialization_and_helpers() {
        // Native XenForo map format
        let conv_json = serde_json::json!({
            "conversation_id": 100,
            "title": "Group Discussion",
            "start_user_id": 1,
            "start_username": "StarterAlice",
            "reply_count": 4,
            "recipients": {
                "2": "Bob",
                "3": "Charlie"
            }
        });
        let conv: Conversation = serde_json::from_value(conv_json).unwrap();
        assert_eq!(conv.starter(), "StarterAlice");
        assert_eq!(conv.recipients.len(), 2);
        let names = conv.participant_names();
        assert_eq!(names, vec!["StarterAlice", "Bob", "Charlie"]);
        assert_eq!(
            conv.participants_display(),
            "StarterAlice (starter), Bob, Charlie"
        );

        // Array format
        let conv_arr_json = serde_json::json!({
            "conversation_id": 101,
            "title": "1-on-1",
            "username": "Dave",
            "recipients": [{"user_id": 5, "username": "Eve"}]
        });
        let conv_arr: Conversation = serde_json::from_value(conv_arr_json).unwrap();
        assert_eq!(conv_arr.starter(), "Dave");
        assert_eq!(conv_arr.participant_names(), vec!["Dave", "Eve"]);
        assert_eq!(conv_arr.participants_display(), "Dave (starter), Eve");

        // Empty array format (standard PHP empty json_encode)
        let conv_empty_json = serde_json::json!({
            "conversation_id": 102,
            "title": "Solo Note",
            "username": "SelfUser",
            "recipients": []
        });
        let conv_empty: Conversation = serde_json::from_value(conv_empty_json).unwrap();
        assert_eq!(conv_empty.participant_names(), vec!["SelfUser"]);
        assert_eq!(conv_empty.participants_display(), "SelfUser (starter)");
    }

    #[test]
    fn absent_discussion_open_deserializes_as_open() {
        // A partial payload (search hits, embedded thread stubs) omits the
        // field; absent must mean OPEN, or every row gets the locked glyph.
        let t: Thread = serde_json::from_value(serde_json::json!({
            "thread_id": 1,
            "title": "A thread"
        }))
        .unwrap();
        assert!(t.discussion_open);

        // An explicit false still closes it.
        let closed: Thread = serde_json::from_value(serde_json::json!({
            "thread_id": 1,
            "discussion_open": false
        }))
        .unwrap();
        assert!(!closed.discussion_open);
    }
}
