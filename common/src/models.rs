//! Serde models for the XF REST API surface the TUI consumes.
//!
//! Lenient by design: every non-identity field has a default so an additive
//! API change can never brick the client. List replies follow XF's envelope
//! `<collection> + pagination` (AbstractController::getPaginationData).

use serde::Deserialize;

pub fn r<T: Default>() -> T {
    T::default()
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
    #[serde(default, rename = "discussion_open")]
    pub discussion_open: bool,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
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
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Conversation {
    #[serde(default)]
    pub conversation_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "start_user_id")]
    pub start_user_id: u32,
    #[serde(default, rename = "start_username")]
    pub start_username: String,
    #[serde(default, rename = "reply_count")]
    pub reply_count: u64,
    #[serde(default, rename = "last_message_date")]
    pub last_message_date: i64,
    #[serde(default, rename = "last_message_username")]
    pub last_message_username: String,
    #[serde(default, rename = "conversation_unread")]
    pub conversation_unread: bool,
    #[serde(default)]
    pub starred: bool,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
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
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserReply {
    #[serde(default)]
    pub user: User,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsersFindReply {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub users: Vec<User>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MeReply {
    #[serde(default)]
    pub me: User,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchHit {
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub date: i64,
    #[serde(default, rename = "content_type")]
    pub content_type: String,
    #[serde(default, rename = "content_id")]
    pub content_id: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResultsReply {
    #[serde(default)]
    pub results: Vec<SearchHit>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub attachment_id: u32,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub mime_type: String,
    /// Direct URL where guests/members can view the attachment.
    #[serde(default, rename = "view_url")]
    pub view_url: Option<String>,
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
}
