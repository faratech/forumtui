//! Screen states and the Action outbox.
//!
//! Screens own their local state and return `Action`s from key handling; the
//! app executes them (spawning tasks, pushing screens). Screens never hold an
//! `App` reference — that keeps borrows trivial and screens testable.

mod browse;
mod misc;
mod social;

use ratatui::Frame;
use ratatui::layout::Rect;

use common::models::*;

use crate::theme::Theme;

// ---------- state structs ----------

#[derive(Debug, Clone)]
pub enum LoginStage {
    Idle,
    /// Short link issued; the TUI is polling for the authorization code.
    Waiting,
}

pub struct LoginState {
    pub stage: LoginStage,
    pub url: String,
    pub busy: bool,
    pub error: Option<String>,
}

impl Default for LoginState {
    fn default() -> Self {
        LoginState {
            stage: LoginStage::Idle,
            url: String::new(),
            busy: false,
            error: None,
        }
    }
}

pub fn login_state() -> Screen {
    Screen::Login(LoginState::default())
}

pub fn search_state() -> Screen {
    Screen::Search(SearchState {
        input_mode: true,
        ..Default::default()
    })
}

#[derive(Default)]
pub struct ForumTreeState {
    pub nodes: Vec<Node>,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ThreadListState {
    pub node_id: u32,
    pub title: String,
    pub threads: Vec<Thread>,
    pub page: u32,
    pub last_page: u32,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ThreadViewState {
    pub thread: Thread,
    pub posts: Vec<Post>,
    pub page: u32,
    pub last_page: u32,
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub scroll: u16,
    pub links: Vec<String>,
    pub link_popup: bool,
    pub sel_local: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ComposeTarget {
    ThreadReply { thread_id: u32, thread_title: String },
    NewThread { node_id: u32 },
    ConversationReply { conversation_id: u32, conversation_title: String },
}

impl ComposeTarget {
    pub fn heading(&self) -> String {
        match self {
            ComposeTarget::ThreadReply { thread_title, .. } => {
                format!("Reply to: {thread_title}")
            }
            ComposeTarget::NewThread { node_id } => format!("New thread in node {node_id}"),
            ComposeTarget::ConversationReply {
                conversation_title, ..
            } => format!("Reply to conversation: {conversation_title}"),
        }
    }
}

#[derive(Default)]
pub struct ComposeState {
    pub target: Option<ComposeTarget>,
    pub title: String,
    pub body: String,
    pub title_field: bool,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ConversationsState {
    pub conversations: Vec<Conversation>,
    pub page: u32,
    pub last_page: u32,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ConversationViewState {
    pub conversation: Conversation,
    pub messages: Vec<ConversationMessage>,
    pub page: u32,
    pub last_page: u32,
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub scroll: u16,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct NewConversationState {
    pub recipients: String,
    pub title: String,
    pub body: String,
    pub field: usize,
    pub resolving: usize,
    pub resolved_ids: Vec<u32>,
    pub errors: Vec<String>,
    pub busy: bool,
}

#[derive(Default)]
pub struct AlertsState {
    pub alerts: Vec<Alert>,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub input_mode: bool,
    pub results: Vec<SearchHit>,
    pub page: u32,
    pub last_page: u32,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ProfileState {
    pub title: String,
    pub user: Option<User>,
    pub loading: bool,
    pub error: Option<String>,
}

// ---------- screen enum + dispatch ----------

pub enum Screen {
    Login(LoginState),
    ForumTree(ForumTreeState),
    ThreadList(ThreadListState),
    ThreadView(ThreadViewState),
    Compose(ComposeState),
    Conversations(ConversationsState),
    ConversationView(ConversationViewState),
    NewConversation(NewConversationState),
    Alerts(AlertsState),
    Search(SearchState),
    Profile(ProfileState),
}

/// Actions a screen asks the app to perform.
pub enum Action {
    None,
    PopScreen,
    OpenThreadList(u32, String),
    OpenThread(Thread),
    OpenProfile(u32, String),
    OpenConversation(Conversation),
    LoadForum(u32, u32),
    LoadThread(u32, u32),
    LoadConversations(u32),
    LoadConversation(u32, u32),
    LoadAlerts,
    LoadNodes,
    RunSearch(String, u32),
    MarkThreadRead(u32),
    MarkForumRead(u32),
    MarkAlertRead(u32),
    StartReply(Thread),
    StartReplyConversation(Conversation),
    StartNewThread(u32),
    StartNewConversation,
    SubmitReply { thread_id: u32, message: String },
    SubmitThread { node_id: u32, title: String, message: String },
    SubmitConvoReply { id: u32, message: String },
    ResolveRecipients(Vec<String>, String, String),
    LoginBegin,
    OpenUrl(String),
    OscCopy(String),
    PasteClipboard,
    Quit,
}

impl Screen {
    pub fn render(&mut self, f: &mut Frame, area: Rect, theme: &Theme) {
        match self {
            Screen::Login(s) => misc::render_login(s, f, area, theme),
            Screen::ForumTree(s) => browse::render_forum_tree(s, f, area, theme),
            Screen::ThreadList(s) => browse::render_thread_list(s, f, area, theme),
            Screen::ThreadView(s) => browse::render_thread_view(s, f, area, theme),
            Screen::Compose(s) => misc::render_compose(s, f, area, theme),
            Screen::Conversations(s) => social::render_conversations(s, f, area, theme),
            Screen::ConversationView(s) => social::render_conversation_view(s, f, area, theme),
            Screen::NewConversation(s) => social::render_new_conversation(s, f, area, theme),
            Screen::Alerts(s) => social::render_alerts(s, f, area, theme),
            Screen::Search(s) => misc::render_search(s, f, area, theme),
            Screen::Profile(s) => misc::render_profile(s, f, area, theme),
        }
    }

    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Action {
        match self {
            Screen::Login(s) => misc::login_key(s, key),
            Screen::ForumTree(s) => browse::forum_tree_key(s, key),
            Screen::ThreadList(s) => browse::thread_list_key(s, key),
            Screen::ThreadView(s) => browse::thread_view_key(s, key),
            Screen::Compose(s) => misc::compose_key(s, key),
            Screen::Conversations(s) => social::conversations_key(s, key),
            Screen::ConversationView(s) => social::conversation_view_key(s, key),
            Screen::NewConversation(s) => social::new_conversation_key(s, key),
            Screen::Alerts(s) => social::alerts_key(s, key),
            Screen::Search(s) => misc::search_key(s, key),
            Screen::Profile(s) => misc::profile_key(s, key),
        }
    }

    pub fn input_capture(&self) -> bool {
        match self {
            Screen::ThreadView(s) => s.link_popup,
            _ => false,
        }
    }
}

// ---------- shared render helpers ----------

/// Map a BBCode style to a terminal style. Quote bodies dim, code goes cyan,
/// spoilers disappear until rendered deliberately elsewhere.
pub(crate) fn style_from(
    theme: &Theme,
    s: &common::bbcode::Style,
) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    let mut st = Style::new().fg(theme.text);
    if s.quote_depth > 0 {
        st = st.fg(theme.dim).add_modifier(Modifier::ITALIC);
    }
    if s.code {
        st = st.fg(Color::Cyan).bg(Color::Black);
    }
    if s.spoiler {
        st = st.fg(Color::Black).bg(Color::Black);
    }
    if s.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.underline {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if s.strikethrough {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    st
}

pub(crate) fn link_style(theme: &Theme) -> ratatui::style::Style {
    ratatui::style::Style::new()
        .fg(theme.link)
        .add_modifier(ratatui::style::Modifier::UNDERLINED)
}

pub(crate) fn centered_box(
    area: Rect,
    width: u16,
    height: u16,
) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(
        x,
        y,
        width.min(area.width),
        height.min(area.height),
    )
}

#[allow(dead_code)]
fn _keep_imports(u: &User, n: &Node, t: &Thread, p: &Post) {
    let _ = (u.user_id, n.node_id, t.thread_id, p.post_id);
}
