//! Screen states and the Action outbox.
//!
//! Screens own their local state and return `Action`s from key handling; the
//! app executes them (spawning tasks, pushing screens). Screens never hold an
//! `App` reference — that keeps borrows trivial and screens testable.

mod browse;
mod misc;
mod social;

/// Quick-node ids and the resolver that turns one into an `Action` (a category
/// opens its first forum, a link-forum opens its URL). The go-to palette and
/// the `g` which-key both navigate through these.
pub(crate) use browse::{NEWS_NODE, SECURITY_NODE, TUTORIALS_NODE, open_node_action};

use ratatui::Frame;
use ratatui::layout::Rect;

use common::models::*;

use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
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
    /// Graphics policy, stamped by the app before every frame.
    pub images: crate::images::Policy,
    /// The logo rect the last frame reserved, in absolute screen coordinates.
    pub image_requests: Vec<crate::images::Request>,
}

impl Default for LoginState {
    fn default() -> Self {
        LoginState {
            stage: LoginStage::Idle,
            url: String::new(),
            busy: false,
            error: None,
            images: crate::images::Policy::default(),
            image_requests: Vec::new(),
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
    /// First rendered row of the Forums panel, kept so the selection stays
    /// visible without the panel jumping around on every keypress.
    pub scroll: usize,
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
    /// `pagination.total` when the server sends one — the `1–20 of 431` panel
    /// footer. 0 means "unknown", and the footer is then omitted.
    pub total: u64,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ThreadViewState {
    pub thread: Thread,
    /// Name of the forum the thread lives in, for the first line of the card
    /// stack. `Thread` carries only `node_id`, so the app resolves it from the
    /// loaded node tree; empty when unknown (the segment is then dropped).
    pub forum_title: String,
    pub posts: Vec<Post>,
    pub page: u32,
    pub last_page: u32,
    /// `pagination.total` — the thread's post count across all pages.
    pub total: u64,
    pub lines: Vec<ratatui::text::Line<'static>>,
    /// Inner width the current `lines` were wrapped for; a resize rebuilds.
    pub width: u16,
    pub scroll: u16,
    pub links: Vec<String>,
    pub link_popup: bool,
    pub sel_local: usize,
    pub sel_post: usize,
    pub post_line_offsets: Vec<usize>,
    pub loading: bool,
    pub error: Option<String>,
    /// Graphics policy, stamped by the app before every frame. A change to it
    /// invalidates `lines`: inline images reserve rows that the text tier
    /// does not.
    pub images: crate::images::Policy,
    /// Image boxes reserved by `rebuild_lines`, in `lines` coordinates.
    pub image_slots: Vec<crate::images::Slot>,
    /// Those of `image_slots` that were fully on screen in the last frame,
    /// translated to absolute screen coordinates for the app to paint.
    pub image_requests: Vec<crate::images::Request>,
}

/// Which half of the Home screen has the keyboard.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Tree,
    List,
}

/// Home is one screen holding both halves, not a tree screen that pushes a
/// list screen: at >= 110 columns they are side by side and Enter loads into
/// the right-hand pane, and below that the same state renders whichever half
/// has focus. Keeping them in one `Screen` is what makes the wide layout a
/// render-time decision instead of a different navigation model.
#[derive(Default)]
pub struct HomeState {
    pub tree: ForumTreeState,
    pub list: ThreadListState,
    pub focus: Pane,
    /// Set by the renderer: true when both panes are on screen. Key handling
    /// reads it (Enter fills the pane instead of pushing a screen), and the
    /// draw always precedes the keys it is asked about in the event loop.
    pub dual: bool,
}

pub fn home_state(loading: bool) -> Screen {
    Screen::Home(HomeState {
        tree: ForumTreeState {
            loading,
            ..Default::default()
        },
        ..Default::default()
    })
}

#[derive(Debug, Clone)]
pub enum ComposeTarget {
    ThreadReply { thread_id: u32, thread_title: String },
    NewThread { node_id: u32 },
    ConversationReply {
        conversation_id: u32,
        conversation_title: String,
        participants: String,
    },
}

#[derive(Default)]
pub struct ComposeState {
    pub target: Option<ComposeTarget>,
    pub title: String,
    pub body: String,
    pub title_cursor: usize,
    pub body_cursor: usize,
    pub title_field: bool,
    pub busy: bool,
    pub error: Option<String>,
    /// The signed-in member, for the editor's `as <user>` segment. Empty
    /// omits that segment — nothing upstream populates this yet.
    pub author: String,
    /// The post number this reply will become (thread's post count + 1), for
    /// the editor's `post #n` segment. `None` omits it.
    pub reply_number: Option<u32>,
    /// Narrow-layout (< 110 cols) toggle: false shows the editor, true shows
    /// the rendered preview. `^O` flips it; unused once both panels fit.
    pub preview: bool,
    /// Attachments this draft may reference with `[ATTACH]id[/ATTACH]`.
    /// Nothing populates it yet — attachment *upload* is implemented in
    /// `common` but not wired into compose (CLAUDE.md "Known gaps") — so in
    /// practice today only `[IMG]` URLs resolve to a picture. The resolution
    /// path is here (and tested) so wiring upload up is the only work left.
    pub attachments: Vec<Attachment>,
    /// Graphics policy, stamped by the app before every frame.
    pub images: crate::images::Policy,
    /// Source pixel sizes the image store has learned, stamped with it: the
    /// preview's caption reads them for its `W×H` segment, which for a bare
    /// `[IMG]` URL is only knowable once the bytes have been fetched.
    pub image_sizes: crate::images::Sizes,
    /// The preview's derived lines, image slots and request memo. Rebuilt
    /// only when the draft, the pane width, the tier or a learned size
    /// changes — that is what keeps typing from re-parsing and re-requesting.
    pub preview_cache: misc::PreviewCache,
    /// Image rects the last preview render reserved, in absolute screen
    /// coordinates, for the app to paint.
    pub image_requests: Vec<crate::images::Request>,
}

#[derive(Default)]
pub struct ConversationsState {
    pub conversations: Vec<Conversation>,
    pub page: u32,
    pub last_page: u32,
    /// `pagination.total` when the server sends one — the Inbox panel's
    /// `1–5 of 18` bottom footer. 0 means "unknown", and the footer falls
    /// back to a bare count.
    pub total: u64,
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
    pub sel_msg: usize,
    pub msg_line_offsets: Vec<u16>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct NewConversationState {
    pub recipients: String,
    pub title: String,
    pub body: String,
    pub recipients_cursor: usize,
    pub title_cursor: usize,
    pub body_cursor: usize,
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

/// Which tab of the Inbox has its data on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboxTab {
    #[default]
    Conversations,
    Alerts,
}

impl InboxTab {
    pub fn other(self) -> InboxTab {
        match self {
            InboxTab::Conversations => InboxTab::Alerts,
            InboxTab::Alerts => InboxTab::Conversations,
        }
    }
}

/// Which half of the Inbox screen has the keyboard: the tabbed list, or the
/// conversation loaded into the right-hand pane. Kept distinct from `Pane`
/// (Home's tree/list) — Inbox's halves are "the tabbed list" and "the open
/// conversation", and reusing `Pane::Tree`/`Pane::List` for that would read
/// backwards at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboxPane {
    #[default]
    List,
    View,
}

/// Inbox is one screen holding both the tabbed list (conversations or alerts)
/// and, once a conversation has been opened, its message view — the same
/// shape as `HomeState` holding a tree and a list. At >= 110 columns they sit
/// side by side and Enter fills `view`; below that Enter pushes a standalone
/// `ConversationView` screen as before (see `render_inbox`/`app::open_conversation`).
#[derive(Default)]
pub struct InboxState {
    pub tab: InboxTab,
    pub convos: ConversationsState,
    pub alerts: AlertsState,
    pub view: Option<ConversationViewState>,
    pub focus: InboxPane,
    /// Set by the renderer: true when both panes are on screen.
    pub dual: bool,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub query_cursor: usize,
    pub author: String,
    pub author_cursor: usize,
    pub active_field: usize,
    pub content_type: u8,
    pub order: u8,
    pub input_mode: bool,
    pub results: Vec<SearchHit>,
    pub page: u32,
    pub last_page: u32,
    /// `pagination.total` when the server sends one — drives the `N results`
    /// line and the `1–N of total` panel footer. 0 means "unknown", and both
    /// fall back to `results.len()`.
    pub total: u64,
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
    Home(HomeState),
    /// The standalone Forums screen. Home now owns the tree, so nothing
    /// constructs this today; it is kept whole (render, keys, hints) as the
    /// pushable "all forums" surface the go-to palette will want in phase 2d,
    /// and because narrow terminals may yet want a dedicated tree screen.
    #[allow(dead_code)]
    ForumTree(ForumTreeState),
    ThreadList(ThreadListState),
    ThreadView(ThreadViewState),
    Compose(ComposeState),
    /// DMs + alerts, tabbed and (>= 110 cols) split with the open
    /// conversation. `c`/`a` open this on the matching tab.
    Inbox(InboxState),
    ConversationView(ConversationViewState),
    NewConversation(NewConversationState),
    Search(SearchState),
    Profile(ProfileState),
}

/// Actions a screen asks the app to perform.
pub enum Action {
    None,
    PopScreen,
    OpenThreadList(u32, String),
    OpenLatestThreads,
    OpenThread(Thread),
    OpenProfile(u32, String),
    OpenMemberContent {
        user_id: u32,
        username: String,
        content: String,
    },
    OpenConversation(Conversation),
    LoadForum(u32, u32),
    LoadThread(u32, u32),
    LoadConversations(u32),
    LoadConversation(u32, u32),
    /// Re-fetch the alerts list. The background poller only refreshes the
    /// header badge, so this is the Inbox's only manual reload.
    LoadAlerts,
    LoadNodes,
    RunSearchQuery(SearchQuery),
    MarkThreadRead(u32),
    MarkForumRead(u32),
    MarkAlertRead(u32),
    MarkConversationRead(u32),
    ReactPost(u32),
    VotePost(u32, String),
    StartReply(Thread),
    StartReplyConversation(Conversation),
    StartNewThread(u32),
    StartNewConversation(Option<String>),
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
    pub fn render(&mut self, f: &mut Frame, area: Rect, theme: &Theme, g: &Glyphs) {
        match self {
            Screen::Login(s) => misc::render_login(s, f, area, theme, g),
            Screen::Home(s) => browse::render_home(s, f, area, theme, g),
            Screen::ForumTree(s) => browse::render_forum_tree(s, f, area, theme, g),
            Screen::ThreadList(s) => browse::render_thread_list(s, f, area, theme, g),
            Screen::ThreadView(s) => browse::render_thread_view(s, f, area, theme, g),
            Screen::Compose(s) => misc::render_compose(s, f, area, theme, g),
            Screen::Inbox(s) => social::render_inbox(s, f, area, theme, g),
            Screen::ConversationView(s) => social::render_conversation_view(s, f, area, theme, g),
            Screen::NewConversation(s) => social::render_new_conversation(s, f, area, theme, g),
            Screen::Search(s) => misc::render_search(s, f, area, theme, g),
            Screen::Profile(s) => misc::render_profile(s, f, area, theme, g),
        }
    }

    /// Hand this screen the graphics tier before rendering it. Only the two
    /// screens that draw images care; a change invalidates the thread view's
    /// wrapped lines, because inline images reserve rows the text tier does
    /// not (`width = 0` is the renderer's "rebuild me" signal).
    pub fn set_image_policy(&mut self, policy: crate::images::Policy, sizes: &crate::images::Sizes) {
        match self {
            Screen::ThreadView(s) => {
                if s.images != policy {
                    s.images = policy;
                    s.width = 0;
                }
            }
            Screen::Login(s) => s.images = policy,
            Screen::Compose(s) => {
                s.images = policy;
                // The store only ever gains entries, so a length change is
                // exactly "it learned a size the caption did not have".
                if s.image_sizes.len() != sizes.len() {
                    s.image_sizes = sizes.clone();
                }
            }
            _ => {}
        }
    }

    /// The image rects the last `render` reserved, in absolute screen
    /// coordinates. Empty on every screen that draws no images, and on every
    /// tier that cannot paint one.
    pub fn image_requests(&self) -> &[crate::images::Request] {
        match self {
            Screen::ThreadView(s) => &s.image_requests,
            Screen::Login(s) => &s.image_requests,
            Screen::Compose(s) => &s.image_requests,
            _ => &[],
        }
    }

    /// The key bar for this screen. Owned here rather than drawn inside each
    /// panel: one bar, one place, always in the same row (DESIGN.md zone 3).
    pub fn hints(&self) -> Hints {
        match self {
            Screen::Login(_) => misc::login_hints(),
            Screen::Home(_) => browse::home_hints(),
            Screen::ForumTree(_) => browse::forum_tree_hints(),
            Screen::ThreadList(_) => browse::thread_list_hints(),
            Screen::ThreadView(_) => browse::thread_view_hints(),
            Screen::Compose(s) => misc::compose_hints(s),
            Screen::Inbox(_) => social::inbox_hints(),
            Screen::ConversationView(_) => social::conversation_view_hints(),
            Screen::NewConversation(_) => social::new_conversation_hints(),
            Screen::Search(_) => misc::search_hints(),
            Screen::Profile(_) => misc::profile_hints(),
        }
    }

    /// This screen's segment of the header breadcrumb. Usually the title, but
    /// a few screens read better with a shorter or more concrete name.
    pub fn crumb(&self) -> String {
        match self {
            Screen::Login(_) => "Login".into(),
            Screen::Home(_) => "Forums".into(),
            Screen::ForumTree(_) => "Forums".into(),
            Screen::ThreadList(s) => browse::thread_list_crumb(s),
            Screen::ThreadView(s) => browse::thread_view_crumb(s),
            Screen::Compose(s) => misc::compose_crumb(s),
            Screen::Inbox(_) => "Inbox".into(),
            Screen::ConversationView(s) => social::conversation_view_crumb(s),
            Screen::NewConversation(_) => "New conversation".into(),
            Screen::Search(s) => misc::search_crumb(s),
            Screen::Profile(s) => s.title.clone(),
        }
    }

    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Action {
        match self {
            Screen::Login(s) => misc::login_key(s, key),
            Screen::Home(s) => browse::home_key(s, key),
            Screen::ForumTree(s) => browse::forum_tree_key(s, key),
            Screen::ThreadList(s) => browse::thread_list_key(s, key),
            Screen::ThreadView(s) => browse::thread_view_key(s, key),
            Screen::Compose(s) => misc::compose_key(s, key),
            Screen::Inbox(s) => social::inbox_key(s, key),
            Screen::ConversationView(s) => social::conversation_view_key(s, key),
            Screen::NewConversation(s) => social::new_conversation_key(s, key),
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

    /// `g g` — the top of whatever this screen is showing. Kept out of the key
    /// handlers because the chord is resolved globally (`overlay::Prefix`), so
    /// no screen ever sees a bare `g`.
    pub fn goto_top(&mut self) {
        match self {
            Screen::Home(h) => match h.focus {
                Pane::Tree => h.tree.sel = 0,
                Pane::List => h.list.sel = 0,
            },
            Screen::ForumTree(t) => t.sel = 0,
            Screen::ThreadList(l) => l.sel = 0,
            Screen::ThreadView(v) => {
                v.scroll = 0;
                v.sel_post = 0;
            }
            Screen::Inbox(ib) => match ib.tab {
                InboxTab::Conversations => ib.convos.sel = 0,
                InboxTab::Alerts => ib.alerts.sel = 0,
            },
            Screen::ConversationView(v) => {
                v.scroll = 0;
                v.sel_msg = 0;
            }
            Screen::Search(s) => s.sel = 0,
            _ => {}
        }
    }

    /// `G` — the bottom. Scroll positions are clamped by the renderers (they
    /// are the only place that knows the panel height), so overshooting here
    /// is both safe and the only way to mean "the end".
    pub fn goto_bottom(&mut self) {
        match self {
            Screen::Home(h) => match h.focus {
                Pane::Tree => h.tree.sel = h.tree.nodes.len().saturating_sub(1),
                Pane::List => h.list.sel = h.list.threads.len().saturating_sub(1),
            },
            Screen::ForumTree(t) => t.sel = t.nodes.len().saturating_sub(1),
            Screen::ThreadList(l) => l.sel = l.threads.len().saturating_sub(1),
            Screen::ThreadView(v) => {
                v.scroll = v.lines.len() as u16;
                v.sel_post = v.posts.len().saturating_sub(1);
            }
            Screen::Inbox(ib) => match ib.tab {
                InboxTab::Conversations => {
                    ib.convos.sel = ib.convos.conversations.len().saturating_sub(1)
                }
                InboxTab::Alerts => ib.alerts.sel = ib.alerts.alerts.len().saturating_sub(1),
            },
            Screen::ConversationView(v) => {
                v.scroll = v.lines.len() as u16;
                v.sel_msg = v.messages.len().saturating_sub(1);
            }
            Screen::Search(s) => s.sel = s.results.len().saturating_sub(1),
            _ => {}
        }
    }

    /// The header of this screen's own group on the `?` keys card (the middle
    /// group, between MOVE and EVERYWHERE).
    pub fn keys_group(&self) -> &'static str {
        match self {
            Screen::Login(_) => "SIGNING IN",
            Screen::Home(_) | Screen::ForumTree(_) | Screen::ThreadList(_) => "THIS LIST",
            Screen::ThreadView(_) => "THIS THREAD",
            Screen::Compose(_) | Screen::NewConversation(_) => "THIS DRAFT",
            Screen::Inbox(_) => "THIS INBOX",
            Screen::ConversationView(_) => "THIS CONVERSATION",
            Screen::Search(_) => "THIS SEARCH",
            Screen::Profile(_) => "THIS MEMBER",
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Screen::Login(_) => "Login",
            Screen::Home(_) => "Forums",
            Screen::ForumTree(_) => "Forums",
            Screen::ThreadList(s) => &s.title,
            Screen::ThreadView(s) => &s.thread.title,
            Screen::Compose(_) => "Compose",
            Screen::Inbox(_) => "Inbox",
            Screen::ConversationView(s) => &s.conversation.title,
            Screen::NewConversation(_) => "New Conversation",
            Screen::Search(_) => "Search",
            Screen::Profile(s) => &s.title,
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
        st = theme.code();
    }
    if s.spoiler {
        // Unchanged from before the redesign: black on black hides the text on
        // every terminal, where Modifier::HIDDEN is widely unimplemented.
        st = Style::new().fg(Color::Black).bg(Color::Black);
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
    theme.link()
}

/// The one panel style, pre-bound to "this is the single visible panel".
/// Phase 2 introduces real two-pane layouts and will pass `focused` through.
pub(crate) fn solo_panel(
    theme: &Theme,
    g: &Glyphs,
    title: &str,
    right: Option<&str>,
    bottom: Option<&str>,
) -> ratatui::widgets::Block<'static> {
    chrome::panel(theme, g, title, true, right, bottom)
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

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::glyph::{ASCII, UNICODE};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// One of every screen, with just enough state to render.
    fn all_screens() -> Vec<Screen> {
        vec![
            login_state(),
            Screen::Home(HomeState {
                tree: ForumTreeState {
                    nodes: vec![Node {
                        node_id: 4,
                        title: "Windows News".into(),
                        node_type: "Forum".into(),
                        depth: 1,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                list: ThreadListState {
                    node_id: 4,
                    title: "Windows News".into(),
                    threads: vec![Thread {
                        thread_id: 1,
                        title: "A thread".into(),
                        prefix: Some("Windows 11".into()),
                        is_unread: true,
                        ..Default::default()
                    }],
                    page: 1,
                    last_page: 3,
                    total: 60,
                    ..Default::default()
                },
                focus: Pane::List,
                dual: false,
            }),
            Screen::ForumTree(ForumTreeState {
                nodes: vec![Node {
                    node_id: 4,
                    title: "Windows News".into(),
                    node_type: "Forum".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            Screen::ThreadList(ThreadListState {
                node_id: 4,
                title: "Windows News".into(),
                threads: vec![Thread {
                    thread_id: 1,
                    title: "A thread".into(),
                    prefix: Some("Windows 11".into()),
                    ..Default::default()
                }],
                page: 1,
                last_page: 3,
                ..Default::default()
            }),
            Screen::ThreadView(ThreadViewState {
                thread: Thread {
                    thread_id: 1,
                    title: "A thread".into(),
                    ..Default::default()
                },
                posts: vec![Post {
                    post_id: 9,
                    username: "HItest".into(),
                    message: "hello".into(),
                    ..Default::default()
                }],
                page: 1,
                last_page: 1,
                ..Default::default()
            }),
            Screen::Compose(ComposeState {
                target: Some(ComposeTarget::ThreadReply {
                    thread_id: 1,
                    thread_title: "A thread".into(),
                }),
                body: "draft".into(),
                ..Default::default()
            }),
            Screen::Compose(ComposeState {
                target: Some(ComposeTarget::NewThread { node_id: 4 }),
                title_field: true,
                ..Default::default()
            }),
            Screen::Inbox(InboxState {
                convos: ConversationsState {
                    conversations: vec![Conversation {
                        conversation_id: 3,
                        title: "A DM".into(),
                        ..Default::default()
                    }],
                    page: 1,
                    last_page: 1,
                    total: 1,
                    ..Default::default()
                },
                alerts: AlertsState {
                    alerts: vec![Alert {
                        alert_id: 1,
                        username: "kemical".into(),
                        content_type: "post_quote".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                view: Some(ConversationViewState {
                    conversation: Conversation {
                        conversation_id: 3,
                        title: "A DM".into(),
                        ..Default::default()
                    },
                    page: 1,
                    last_page: 1,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            Screen::ConversationView(ConversationViewState {
                conversation: Conversation {
                    conversation_id: 3,
                    title: "A DM".into(),
                    ..Default::default()
                },
                page: 1,
                last_page: 1,
                ..Default::default()
            }),
            Screen::NewConversation(NewConversationState::default()),
            Screen::Search(SearchState {
                query: "edge update".into(),
                results: vec![SearchHit {
                    content_type: "thread".into(),
                    title: "A hit".into(),
                    ..Default::default()
                }],
                page: 1,
                last_page: 2,
                ..Default::default()
            }),
            Screen::Profile(ProfileState {
                title: "HItest".into(),
                user: Some(User {
                    user_id: 7,
                    username: "HItest".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ]
    }

    #[test]
    fn every_screen_supplies_hints_with_a_valid_primary() {
        for s in all_screens() {
            let h = s.hints();
            assert!(!h.keys.is_empty(), "{} has no hints", s.title());
            assert!(
                h.primary < h.keys.len(),
                "{}: primary {} is out of range",
                s.title(),
                h.primary
            );
        }
    }

    /// DESIGN.md: "below 90 columns screens supply a short hint set". The
    /// `\u{2026}` clip cuts *inside* the last word, so an 80-column key bar
    /// that still needs it is a screen missing its short set.
    #[test]
    fn every_screen_fits_its_key_bar_in_eighty_columns() {
        let theme = crate::theme::Theme::truecolor();
        for s in all_screens() {
            let h = s.hints();
            assert!(
                h.short.is_empty() || h.primary < h.short.len(),
                "{}: primary {} is out of range for the short set",
                s.title(),
                h.primary
            );
            let line = crate::chrome::key_bar(&theme, &h, 80);
            let text: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert!(
                !text.contains('\u{2026}'),
                "{} clips its key bar at 80 columns: {text}",
                s.title()
            );
            assert!(line.width() <= 80, "{}: {text}", s.title());
        }
    }

    #[test]
    fn every_screen_supplies_a_non_empty_crumb() {
        for s in all_screens() {
            assert!(!s.crumb().trim().is_empty(), "{} has an empty crumb", s.title());
        }
    }

    /// `g g` / `G` reach every screen through the global chord, so every
    /// screen must survive them — including the ones with nothing to move.
    #[test]
    fn every_screen_survives_goto_top_and_bottom_and_names_its_keys_group() {
        for mut s in all_screens() {
            let group = s.keys_group();
            assert!(!group.is_empty(), "{} has no keys group", s.title());
            assert_eq!(
                group.to_uppercase(),
                group,
                "{}: the card's group headers are caps",
                s.title()
            );
            s.goto_bottom();
            s.goto_top();
            s.goto_bottom();
        }
        // The moves are real where there is something to move.
        let mut list = Screen::ThreadList(ThreadListState {
            threads: vec![Thread::default(), Thread::default(), Thread::default()],
            ..Default::default()
        });
        list.goto_bottom();
        assert!(matches!(&list, Screen::ThreadList(l) if l.sel == 2));
        list.goto_top();
        assert!(matches!(&list, Screen::ThreadList(l) if l.sel == 0));
    }

    /// Renders every screen headless. This catches panics from arithmetic on
    /// small areas and from layout constraints that no longer fit.
    #[test]
    fn every_screen_renders_at_wide_and_tiny_sizes_in_both_glyph_sets() {
        for (w, h) in [(120u16, 36u16), (80, 24), (40, 10), (20, 5)] {
            for theme in [Theme::truecolor(), Theme::ansi16(), Theme::mono()] {
                for g in [&UNICODE, &ASCII] {
                    let mut term =
                        Terminal::new(TestBackend::new(w, h)).expect("test terminal");
                    for mut s in all_screens() {
                        term.draw(|f| {
                            let area = f.area();
                            s.render(f, area, &theme, g);
                        })
                        .expect("render");
                    }
                }
            }
        }
    }
}
