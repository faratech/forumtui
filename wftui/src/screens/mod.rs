//! Screen states and the Action outbox.
//!
//! Screens own their local state and return `Action`s from key handling; the
//! app executes them (spawning tasks, pushing screens). Screens never hold an
//! `App` reference — that keeps borrows trivial and screens testable.

mod browse;
mod library;
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
use crate::hit::HitMap;
use crate::theme::Theme;

// ---------- state structs ----------

#[derive(Debug, Clone)]
pub enum LoginStage {
    Idle,
    /// Short link issued; the TUI is polling for the authorization code.
    Waiting,
}

pub struct LoginState {
    /// Which line of the sign-in panel carries the link, stamped while it is
    /// drawn so a click can open it (#701).
    pub url_line: Option<usize>,
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
            url_line: None,
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
    /// How many of the leading rows in `threads` are pinned/sticky threads
    /// (XF's separate `sticky` array, prepended here for display but never
    /// counted towards `pagination.total` or a page's row count — see
    /// `list_range`).
    pub sticky_count: usize,
    /// The FIRST page held in `threads`. More may follow it: a page is 20
    /// rows server-side and a tall terminal shows far more, so the app keeps
    /// fetching the next page until the pane is full (#699).
    pub page: u32,
    pub last_page: u32,
    /// Consecutive pages held, starting at `page`. 1 unless the viewport
    /// asked for more.
    pub pages_loaded: u32,
    /// `pagination.per_page`, so the range footer stays right across a
    /// multi-page fill.
    pub per_page: u32,
    /// `pagination.total` when the server sends one — the `1–20 of 431` panel
    /// footer. 0 means "unknown", and the footer is then omitted.
    pub total: u64,
    pub sel: usize,
    /// Body rows the last render could show, stamped by the renderer: what
    /// "enough to fill the pane" means for the fill loop.
    pub visible: usize,
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
    /// First visual row on screen. `usize`, not `u16`: a single post can
    /// wrap past 65,536 rows on this site (`messageMaxLength` is 0), and a
    /// truncated offset makes the tail of it unreachable (issue #558).
    pub scroll: usize,
    pub links: Vec<String>,
    /// `(line, index into `links`)` for each `[n] url` row `rebuild_lines`
    /// wrote, and `(line, image ordinal within its post)` for each image
    /// caption and the rows reserved under it. Both are what makes a click
    /// on a link or a picture mean that link or that picture — `lines` is a
    /// flat span run, so nothing else in it remembers what a row was.
    pub link_lines: Vec<(usize, usize)>,
    pub image_lines: Vec<(usize, usize)>,
    pub link_popup: bool,
    pub sel_local: usize,
    pub sel_post: usize,
    pub post_line_offsets: Vec<usize>,
    pub loading: bool,
    pub error: Option<String>,
    /// The newest post date the reader has actually had on screen (#694),
    /// and the newest one already reported. A thread is marked read up to
    /// what was *seen*, not to "now" on open — XF's mark-read takes a date
    /// and refuses to move backwards, so a half-read thread stays half
    /// unread, exactly as it would on the site.
    pub seen_date: i64,
    pub reported_date: i64,
    /// `[SPOILER]` bodies render hidden until this is flipped with `x`
    /// (issue #621): hidden is black-on-black by `style_from`, revealed is
    /// the text's own styling. Flipping forces `rebuild_lines` via
    /// `width = 0`, the same trick the image-policy change uses.
    pub reveal_spoilers: bool,
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
    /// Where the last frame drew each half, in absolute screen coordinates, so
    /// a mouse wheel can act on the pane the pointer is over instead of the
    /// one that happens to have the keyboard (issue #549). Zero-sized when
    /// that half was not on screen — a `Rect` of no area contains nothing.
    pub tree_rect: Rect,
    pub list_rect: Rect,
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
    /// The body's wrap, kept incrementally (issue #678): the renderer and
    /// the caret model both read it, so a keystroke re-wraps the logical
    /// line it touched instead of the whole draft.
    pub wrap: crate::editor::WrapCache,
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
    /// The body pane's inner size in cells, stamped by the renderer every
    /// frame. The key handler needs it to wrap the draft the same way the
    /// screen does (Up/Down move by *visual* row, PageUp/PageDown by a
    /// pane), and the draw always precedes the keys it is asked about.
    pub body_width: u16,
    pub body_height: u16,
    /// Where the last frame drew the Title field and the body, in absolute
    /// screen coordinates — what a click resolves its caret against
    /// (`click_field`). Zero-sized when that field is not on screen, so a
    /// stale rect can never answer for a point.
    pub title_rect: Rect,
    pub body_rect: Rect,
    /// First visual row of the body on screen. Follows the caret, so text
    /// typed past the bottom of the pane scrolls into view instead of being
    /// written blind (issue #519). `usize`: a multi-MB paste is a draft this
    /// client accepts, and a `u16` offset wrapped past 65,536 rows (#558).
    pub body_scroll: usize,
    /// The column a run of Up/Down is aiming at, in cells. Set by the first
    /// vertical move and cleared by every other key, so crossing a short
    /// line does not clip the caret's column permanently (issue #523).
    pub body_desired_col: Option<usize>,
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
    /// First visual row on screen — `usize` for the same reason as
    /// `ThreadViewState::scroll` (issue #558).
    pub scroll: usize,
    pub sel_msg: usize,
    pub msg_line_offsets: Vec<usize>,
    /// What `lines`/`msg_line_offsets` were last built for: the pane width
    /// and the selected message (the only two things the layout depends on).
    /// `None` forces a rebuild — every writer of `messages` clears it. Both
    /// the Inbox pane and the standalone screen used to re-wrap and re-parse
    /// every message on every frame (issue #522).
    pub built: Option<(u16, usize, bool)>,
    pub loading: bool,
    pub error: Option<String>,
    /// `[SPOILER]` bodies in the messages render hidden until `x` flips
    /// this (#669 — the conversation-view sibling of the thread view's
    /// toggle). Part of the `built` memo key, so a flip forces a rebuild.
    pub reveal_spoilers: bool,
}

#[derive(Default)]
pub struct NewConversationState {
    pub recipients: String,
    pub title: String,
    pub body: String,
    /// Same contract as `ComposeState::wrap` (issue #678).
    pub wrap: crate::editor::WrapCache,
    pub recipients_cursor: usize,
    pub title_cursor: usize,
    pub body_cursor: usize,
    pub field: usize,
    pub resolving: usize,
    pub resolved_ids: Vec<u32>,
    pub errors: Vec<String>,
    /// True from `submit()` until `Msg::ConvoCreated` lands — the whole
    /// lifecycle, resolution *and* the write behind the 30 s write gate
    /// (issue #597). It is the only re-entry guard for Enter and the only
    /// thing that blocks Esc while a write is in flight.
    pub busy: bool,
    /// The second stage of that lifecycle: recipients are resolved and
    /// `create_conversation` is spawned/awaiting its gate. Only changes what
    /// the screen says ("Sending…" rather than "Resolving recipients…").
    pub sending: bool,
    /// Same contract as `ComposeState`'s: the message pane's size stamped by
    /// the renderer, the caret-following scroll offset, and the sticky
    /// desired column for Up/Down.
    pub body_width: u16,
    pub body_height: u16,
    pub body_scroll: usize,
    pub body_desired_col: Option<usize>,
    /// The three fields' last-drawn rects, for click-to-caret.
    pub to_rect: Rect,
    pub title_rect: Rect,
    pub body_rect: Rect,
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
    /// The panes' last-drawn rects — see `HomeState::tree_rect` (issue #549).
    pub list_rect: Rect,
    pub view_rect: Rect,
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
    /// Member-content mode: this screen is showing one member's threads or
    /// posts (profile `t`/`p`) as `(user_id, content)` — fetched with
    /// `search_member`, not a keyword search. `query` is only a display label
    /// there ("by: name (thread)"), so paging and the `t` toggle have to
    /// re-issue `search_member`; feeding the label back into `search_advanced`
    /// searched the site for that literal string (issue #548).
    pub member: Option<(u32, String)>,
    pub results: Vec<SearchHit>,
    /// One pre-rendered dim snippet per result, parallel to `results`.
    ///
    /// Derived from the hit's BBCode message, which is a full post: doing it
    /// in the renderer re-parsed every message on screen at the event loop's
    /// 20 fps (issue #522). Fill it with `set_results`, which is the only
    /// thing that should ever assign `results`.
    pub snippets: Vec<String>,
    pub page: u32,
    pub last_page: u32,
    /// `pagination.total` when the server sends one — drives the `N results`
    /// line and the `1–N of total` panel footer. 0 means "unknown", and both
    /// fall back to `results.len()`.
    pub total: u64,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Which in-flight load this screen is waiting on: the `App`-wide
    /// `search_generation` its fetch was stamped with. A reply to an older
    /// query — or to a member list this screen was pushed over — carries a
    /// number no live screen is waiting on and is dropped instead of
    /// overwriting the results (same pattern as `ThreadLoaded`).
    pub generation: u64,
    /// Where the query text and the author segment were drawn on the `/` row
    /// (absolute screen coordinates), so a click lands in the right field
    /// with the caret where the pointer is. `author_rect` is zero-sized on
    /// the narrow rungs of `render_query_row`'s ladder, where the author
    /// segment is not drawn at all.
    pub query_rect: Rect,
    pub author_rect: Rect,
}

impl SearchState {
    /// Adopt a page of results and derive everything the renderer would
    /// otherwise re-derive per frame.
    pub fn set_results(&mut self, results: Vec<SearchHit>) {
        self.snippets = results
            .iter()
            .map(|h| misc::search_snippet(&h.message, 100))
            .collect();
        self.results = results;
    }
}

/// Which pane of the Media Gallery has the keyboard (#697) — the same
/// two-pane shape Home uses for forums and threads.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaPane {
    Categories,
    #[default]
    Items,
}

/// The Media Gallery's state (#680, #697): the category tree beside one
/// fetched page of that category's media.
#[derive(Default)]
pub struct MediaListState {
    pub items: Vec<MediaItem>,
    pub categories: Vec<MediaCategory>,
    /// 0 is "All media"; 1.. index `categories`.
    pub cat_sel: usize,
    pub focus: MediaPane,
    /// `None` is the whole gallery, `Some(id)` one category.
    pub category: Option<u32>,
    /// Panel title for the item pane — the category's name, or "All media".
    pub category_title: String,
    pub page: u32,
    pub last_page: u32,
    pub total: u64,
    pub sel: usize,
    /// First item row on screen, and how many fit — the renderer stamps
    /// both, and the app reads `visible` to keep a tall pane fed with
    /// enough items (the API's page size is fixed server-side).
    pub scroll: usize,
    pub visible: usize,
    pub dual: bool,
    /// Where each pane was drawn, so a click can focus the pane under the
    /// pointer before its row index is applied (the Home contract).
    pub cat_rect: ratatui::layout::Rect,
    pub items_rect: ratatui::layout::Rect,
    pub loading: bool,
    pub error: Option<String>,
    pub images: crate::images::Policy,
    pub image_requests: Vec<crate::images::Request>,
}

/// The Resource Manager catalog screen's state (#680).
#[derive(Default)]
pub struct ResourceListState {
    pub items: Vec<Resource>,
    pub page: u32,
    pub last_page: u32,
    pub total: u64,
    pub sel: usize,
    pub loading: bool,
    pub error: Option<String>,
}

/// The resource page (#697): one resource rendered in the client, from the
/// same BBCode the site renders.
#[derive(Default)]
pub struct ResourceViewState {
    pub id: u32,
    pub resource: Option<Resource>,
    /// The laid-out page; rebuilt when the pane's width changes.
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub width: u16,
    pub scroll: usize,
    pub links: Vec<String>,
    /// `(line, index into `links`)` for each `[n] url` row, so a click on one
    /// opens that link (#701).
    pub link_lines: Vec<(usize, usize)>,
    pub loading: bool,
    pub error: Option<String>,
    pub images: crate::images::Policy,
    pub image_slots: Vec<crate::images::Slot>,
    pub image_requests: Vec<crate::images::Request>,
}

/// Everything the viewer needs to show one picture, built by whoever opens
/// it (a gallery row today; a post's attachment next).
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct ImageOpen {
    pub title: String,
    pub meta: String,
    pub description: String,
    /// The URL the image store fetches and paints.
    pub key: String,
    pub px: Option<(u32, u32)>,
    pub web_url: Option<String>,
}

/// The full-size image viewer (#697).
#[derive(Default)]
pub struct ImageViewState {
    pub title: String,
    pub meta: String,
    pub description: String,
    pub key: Option<String>,
    pub px: Option<(u32, u32)>,
    pub web_url: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub images: crate::images::Policy,
    pub image_requests: Vec<crate::images::Request>,
}

impl ImageViewState {
    pub fn of(open: ImageOpen) -> Self {
        ImageViewState {
            title: open.title,
            meta: open.meta,
            description: open.description,
            key: Some(open.key),
            px: open.px,
            web_url: open.web_url,
            ..Default::default()
        }
    }
}

#[derive(Default)]
pub struct ProfileState {
    pub title: String,
    pub user: Option<User>,
    pub loading: bool,
    pub error: Option<String>,
    /// Which `open_profile` request this screen is waiting on. The resolved
    /// id is computed off-thread (a name-only lookup), so the request token
    /// minted at open time is the only identity the reply can be matched
    /// against — a stale reply for an older profile must not fill this one.
    pub generation: u64,
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
    /// The Media Gallery (XFMG) catalog — `g m` / palette (#680).
    MediaGallery(MediaListState),
    /// The Resource Manager (XFRM) catalog — `g r` / palette (#680).
    Resources(ResourceListState),
    /// One resource, rendered here rather than handed to a browser (#697).
    ResourceView(ResourceViewState),
    /// One picture, as large as the pane allows (#697).
    ImageView(ImageViewState),
}

/// What Esc means on the screen that is on top (see `Screen::esc_intent`).
pub enum EscIntent {
    /// The app's own Esc handling applies: pane focus, then pop/quit.
    App,
    /// The screen handles it in `on_key`.
    Screen,
    /// Esc is refused while a write is in flight; show this in the status
    /// line so the refusal is not silent.
    Blocked(&'static str),
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
    /// Browse the Media Gallery catalog, fetching page 1 (#680).
    OpenMediaGallery,
    /// Browse the Resource Manager catalog, fetching page 1 (#680).
    OpenResources,
    /// Fetch one page of the media / resource catalogs (#680), optionally
    /// scoped to a gallery category (#697).
    LoadMedia { category: Option<u32>, page: u32 },
    LoadResources(u32),
    /// The gallery's category tree (#697).
    LoadMediaCategories,
    /// Open one resource's page in the client, and (re)fetch it (#697).
    OpenResource(u32),
    LoadResource(u32),
    /// Show one picture full size in the client (#697).
    OpenImage(Box<ImageOpen>),
    /// One page of a member's threads/posts (issue #548). `content` is
    /// XenForo's `content` parameter for `/search/member`: "thread" or "post".
    LoadMemberContent {
        user_id: u32,
        content: String,
        page: u32,
    },
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
    /// A screen-level refusal that still needs to say something — unlike
    /// `Action::None`, which is silent. E.g. `N`/`m` on the synthetic
    /// "Latest posts" list (node 0), which has no real forum to post into or
    /// mark read (issue #552).
    Notice(String),
    PasteClipboard,
    Quit,
}

impl Screen {
    /// Draw this screen into `area`, registering what the pointer can hit as
    /// it goes. `hits` is the frame's map (`App::draw` clears it); a screen
    /// that registers nothing simply cannot be clicked, which is exactly what
    /// `WFTUI_MOUSE=0` turns every screen into.
    pub fn render(
        &mut self,
        f: &mut Frame,
        area: Rect,
        theme: &Theme,
        g: &Glyphs,
        hits: &mut HitMap,
    ) {
        match self {
            Screen::Login(s) => misc::render_login(s, f, area, theme, g, hits),
            Screen::Home(s) => browse::render_home(s, f, area, theme, g, hits),
            Screen::ForumTree(s) => browse::render_forum_tree(s, f, area, theme, g, hits),
            Screen::ThreadList(s) => browse::render_thread_list(s, f, area, theme, g, hits),
            Screen::ThreadView(s) => browse::render_thread_view(s, f, area, theme, g, hits),
            Screen::Compose(s) => misc::render_compose(s, f, area, theme, g, hits),
            Screen::Inbox(s) => social::render_inbox(s, f, area, theme, g, hits),
            Screen::ConversationView(s) => {
                social::render_conversation_view(s, f, area, theme, g, hits)
            }
            Screen::NewConversation(s) => {
                social::render_new_conversation(s, f, area, theme, g, hits)
            }
            Screen::Search(s) => misc::render_search(s, f, area, theme, g, hits),
            Screen::Profile(s) => misc::render_profile(s, f, area, theme, g, hits),
            Screen::MediaGallery(s) => library::render_media_gallery(s, f, area, theme, g, hits),
            Screen::Resources(s) => library::render_resources(s, f, area, theme, g, hits),
            Screen::ResourceView(s) => library::render_resource_view(s, f, area, theme, g, hits),
            Screen::ImageView(s) => library::render_image_view(s, f, area, theme, g, hits),
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
            Screen::MediaGallery(s) => s.images = policy,
            Screen::ImageView(s) => s.images = policy,
            Screen::ResourceView(s) => {
                if s.images != policy {
                    s.images = policy;
                    // Same trick the thread view uses: a policy change
                    // invalidates the laid-out lines, because the icon
                    // reserves rows the text tier does not.
                    s.width = 0;
                }
            }
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
            Screen::MediaGallery(s) => &s.image_requests,
            Screen::ResourceView(s) => &s.image_requests,
            Screen::ImageView(s) => &s.image_requests,
            Screen::Login(s) => &s.image_requests,
            Screen::Compose(s) => &s.image_requests,
            _ => &[],
        }
    }

    /// The key bar for this screen. Owned here rather than drawn inside each
    /// panel: one bar, one place, always in the same row (DESIGN.md zone 3).
    pub fn hints(&self) -> Hints {
        match self {
            Screen::Login(s) => misc::login_hints(s),
            Screen::Home(s) => browse::home_hints(s),
            Screen::ForumTree(_) => browse::forum_tree_hints(),
            Screen::ThreadList(s) => browse::thread_list_hints(s.node_id),
            Screen::ThreadView(s) => browse::thread_view_hints(s),
            Screen::Compose(s) => misc::compose_hints(s),
            Screen::Inbox(s) => social::inbox_hints(s),
            Screen::ConversationView(s) => social::conversation_view_hints(s),
            Screen::NewConversation(_) => social::new_conversation_hints(),
            Screen::Search(s) => misc::search_hints(s),
            Screen::Profile(_) => misc::profile_hints(),
            Screen::MediaGallery(s) => library::media_hints(s),
            Screen::Resources(s) => library::resources_hints(s),
            Screen::ResourceView(s) => library::resource_view_hints(s),
            Screen::ImageView(s) => library::image_view_hints(s),
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
            Screen::MediaGallery(_) => "Media Gallery".into(),
            Screen::Resources(_) => "Resources".into(),
            Screen::ResourceView(s) => s
                .resource
                .as_ref()
                .map(|r| r.title.clone())
                .unwrap_or_else(|| "Resource".into()),
            Screen::ImageView(s) => s.title.clone(),
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
            Screen::MediaGallery(s) => library::media_list_key(s, key),
            Screen::Resources(s) => library::resources_list_key(s, key),
            Screen::ResourceView(s) => library::resource_view_key(s, key),
            Screen::ImageView(s) => library::image_view_key(s, key),
        }
    }

    /// Give the keyboard to the pane under `(col, row)`, if this screen has
    /// two of them and the point is inside one.
    ///
    /// The mouse wheel calls this before it scrolls: with the dual-pane Home
    /// and Inbox layouts the pointer is usually over the pane that does NOT
    /// have focus, and the wheel used to move the focused one instead
    /// (issue #549). The rects are whatever the last frame drew.
    pub fn focus_pane_at(&mut self, col: u16, row: u16) {
        let at = ratatui::layout::Position::new(col, row);
        match self {
            Screen::Home(h) => {
                if h.tree_rect.contains(at) {
                    h.focus = Pane::Tree;
                } else if h.list_rect.contains(at) {
                    h.focus = Pane::List;
                }
            }
            Screen::Inbox(ib) => {
                if ib.list_rect.contains(at) {
                    ib.focus = InboxPane::List;
                } else if ib.view_rect.contains(at) && ib.view.is_some() {
                    ib.focus = InboxPane::View;
                }
            }
            Screen::MediaGallery(m) => {
                if m.dual && m.cat_rect.contains(at) {
                    m.focus = MediaPane::Categories;
                } else if m.items_rect.contains(at) {
                    m.focus = MediaPane::Items;
                }
            }
            _ => {}
        }
    }

    /// The row the focused pane's list has selected — read before a click
    /// moves it, so "clicked the row that was already selected" can mean
    /// "open it" (`App::click_hit`).
    pub fn selected_index(&self) -> Option<usize> {
        match self {
            Screen::Home(h) => Some(match h.focus {
                Pane::Tree => h.tree.sel,
                Pane::List => h.list.sel,
            }),
            Screen::ForumTree(t) => Some(t.sel),
            Screen::ThreadList(l) => Some(l.sel),
            Screen::Inbox(ib) => Some(match ib.tab {
                InboxTab::Conversations => ib.convos.sel,
                InboxTab::Alerts => ib.alerts.sel,
            }),
            Screen::Search(s) => Some(s.sel),
            Screen::MediaGallery(m) => Some(match m.focus {
                MediaPane::Categories => m.cat_sel,
                MediaPane::Items => m.sel,
            }),
            Screen::Resources(r) => Some(r.sel),
            _ => None,
        }
    }

    /// Put the selection on row `i` of the focused pane's list. Out-of-range
    /// indices are ignored rather than clamped: a click can only ever name a
    /// row the last frame actually drew, so an index past the end means the
    /// list changed underneath and the old selection is the better answer.
    pub fn select_index(&mut self, i: usize) {
        fn set(sel: &mut usize, len: usize, i: usize) {
            if i < len {
                *sel = i;
            }
        }
        match self {
            Screen::Home(h) => match h.focus {
                Pane::Tree => set(&mut h.tree.sel, h.tree.nodes.len(), i),
                Pane::List => set(&mut h.list.sel, h.list.threads.len(), i),
            },
            Screen::ForumTree(t) => set(&mut t.sel, t.nodes.len(), i),
            Screen::ThreadList(l) => set(&mut l.sel, l.threads.len(), i),
            Screen::Inbox(ib) => match ib.tab {
                InboxTab::Conversations => {
                    set(&mut ib.convos.sel, ib.convos.conversations.len(), i)
                }
                InboxTab::Alerts => set(&mut ib.alerts.sel, ib.alerts.alerts.len(), i),
            },
            Screen::Search(s) => set(&mut s.sel, s.results.len(), i),
            Screen::MediaGallery(m) => match m.focus {
                // The category list carries an extra leading "All media" row.
                MediaPane::Categories => set(&mut m.cat_sel, m.categories.len() + 1, i),
                MediaPane::Items => set(&mut m.sel, m.items.len(), i),
            },
            Screen::Resources(r) => set(&mut r.sel, r.items.len(), i),
            _ => {}
        }
    }

    /// Select the post/message card `i` — what a click on a post body means.
    ///
    /// The thread view's gutter colour is baked into `lines` by
    /// `rebuild_lines`, so this invalidates the cached width exactly the way
    /// `n`/`N` do (issue #542); the DM view's `built` memo keys on
    /// `sel_msg`, so assigning it is enough there. Neither scrolls: the
    /// reader is already looking at the card they clicked.
    pub fn select_post(&mut self, i: usize) {
        match self {
            Screen::ThreadView(v) => {
                if i < v.posts.len() {
                    v.sel_post = i;
                    v.width = 0;
                }
            }
            Screen::ConversationView(v) => {
                if i < v.messages.len() {
                    v.sel_msg = i;
                }
            }
            Screen::Inbox(ib) => {
                if let Some(v) = &mut ib.view
                    && i < v.messages.len()
                {
                    v.sel_msg = i;
                }
            }
            _ => {}
        }
    }

    /// Does this screen have a fetch in flight — i.e. does it animate a
    /// spinner that needs the event loop redrawing while it runs (#674)?
    /// Static busy text ("Sending…") does not count: nothing on screen
    /// changes until a message arrives.
    pub fn is_loading(&self) -> bool {
        match self {
            Screen::Home(h) => h.tree.loading || h.list.loading,
            Screen::ForumTree(t) => t.loading,
            Screen::ThreadList(l) => l.loading,
            Screen::ThreadView(v) => v.loading,
            Screen::Inbox(i) => {
                i.convos.loading
                    || i.alerts.loading
                    || i.view.as_ref().is_some_and(|v| v.loading)
            }
            Screen::ConversationView(v) => v.loading,
            Screen::Search(s) => s.loading,
            Screen::Profile(p) => p.loading,
            Screen::MediaGallery(m) => m.loading,
            Screen::Resources(r) => r.loading,
            Screen::ResourceView(r) => r.loading,
            Screen::ImageView(v) => v.loading,
            // The Login Waiting stage animates its "waiting for approval"
            // spinner too — but only while a flow is live.
            Screen::Login(l) => l.busy || matches!(l.stage, LoginStage::Waiting),
            Screen::Compose(_) | Screen::NewConversation(_) => false,
        }
    }

    /// The site URL for whatever this screen currently has selected — what a
    /// right click (a long press on a phone terminal) opens, and the same
    /// thing `u` opens in the thread view. `None` when the payload carried
    /// no `view_url`, which is also when `u` would do nothing.
    pub fn web_url(&self) -> Option<String> {
        match self {
            Screen::Home(h) => match h.focus {
                Pane::Tree => h.tree.nodes.get(h.tree.sel).and_then(|n| n.view_url.clone()),
                Pane::List => h.list.threads.get(h.list.sel).and_then(|t| t.view_url.clone()),
            },
            Screen::ForumTree(t) => t.nodes.get(t.sel).and_then(|n| n.view_url.clone()),
            Screen::ThreadList(l) => l.threads.get(l.sel).and_then(|t| t.view_url.clone()),
            Screen::ThreadView(v) => v
                .posts
                .get(v.sel_post)
                .and_then(|p| p.view_url.clone())
                .or_else(|| v.thread.view_url.clone()),
            Screen::Inbox(ib) => match ib.focus {
                // With the view pane focused, "open on the site" means the
                // conversation on screen — the list may not even be drawn.
                InboxPane::View => {
                    ib.view.as_ref().and_then(|v| v.conversation.view_url.clone())
                }
                InboxPane::List => match ib.tab {
                    InboxTab::Conversations => ib
                        .convos
                        .conversations
                        .get(ib.convos.sel)
                        .and_then(|c| c.view_url.clone()),
                    InboxTab::Alerts => ib
                        .alerts
                        .alerts
                        .get(ib.alerts.sel)
                        .and_then(|a| a.alert_url.clone()),
                },
            },
            Screen::ConversationView(v) => v.conversation.view_url.clone(),
            Screen::Search(s) => s.results.get(s.sel).and_then(|h| h.view_url.clone()),
            Screen::MediaGallery(m) => m.items.get(m.sel).and_then(|m| m.view_url.clone()),
            Screen::Resources(r) => r.items.get(r.sel).and_then(|r| r.view_url.clone()),
            Screen::ResourceView(r) => r.resource.as_ref().and_then(|r| r.view_url.clone()),
            Screen::ImageView(v) => v.web_url.clone(),
            Screen::Profile(p) => p.user.as_ref().and_then(|u| u.view_url.clone()),
            _ => None,
        }
    }

    /// Give this screen's field `i` the keyboard and put the caret where the
    /// pointer is (`Hit::Field`). Screen-local field numbering, and the caret
    /// arithmetic lives beside the renderer that stamped the field's rect.
    pub fn click_field(&mut self, field: usize, col: u16, row: u16) {
        match self {
            Screen::Compose(s) => misc::compose_click_field(s, field, col, row),
            Screen::NewConversation(s) => social::new_conversation_click_field(s, field, col, row),
            Screen::Search(s) => misc::search_click_field(s, field, col),
            _ => {}
        }
    }

    /// Who owns an Esc keypress.
    ///
    /// The app used to handle Esc for every screen before `on_key` ever saw
    /// it, which made three things impossible (issue #520): leaving Search's
    /// edit mode without closing the screen, a composer's own Esc arm, and —
    /// worst — it popped a *busy* composer whose post was already spawned
    /// and waiting on the write gate, so the post still landed, the screen
    /// that would have shown a failure was gone, and the error was dropped.
    pub fn esc_intent(&self) -> EscIntent {
        match self {
            // The sign-in screen is a gate, not a place: Esc must not pop it
            // onto a session-less Home stuck loading forever (issue #556).
            Screen::Login(_) => EscIntent::Blocked("Sign in first, or press q to quit."),
            Screen::Compose(c) if c.busy => EscIntent::Blocked(
                "Sending\u{2026} Esc cannot cancel it \u{2014} wait for the result.",
            ),
            // Issue #597: `busy` now spans the create write too, so Esc is
            // blocked for the whole lifecycle — it used to pop the screen out
            // from under an in-flight `create_conversation`.
            Screen::NewConversation(n) if n.sending => EscIntent::Blocked(
                "Sending\u{2026} Esc cannot cancel it \u{2014} wait for the result.",
            ),
            Screen::NewConversation(n) if n.busy => EscIntent::Blocked(
                "Resolving recipients\u{2026} Esc cannot cancel it \u{2014} wait for the result.",
            ),
            Screen::Compose(_) | Screen::NewConversation(_) => EscIntent::Screen,
            Screen::Search(s) if s.input_mode => EscIntent::Screen,
            _ => EscIntent::App,
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
                // Force `rebuild_lines` (issue #542 — see thread_view_key's
                // n/N for the same fix and why).
                v.width = 0;
            }
            Screen::Inbox(ib) => match ib.focus {
                // The view pane is a real pane with its own scroll: `g` must
                // move what is on screen, not the hidden list (issue class:
                // Home's arms route by `h.focus` for the same reason).
                InboxPane::View => {
                    if let Some(v) = &mut ib.view {
                        v.scroll = 0;
                        v.sel_msg = 0;
                    }
                }
                InboxPane::List => match ib.tab {
                    InboxTab::Conversations => ib.convos.sel = 0,
                    InboxTab::Alerts => ib.alerts.sel = 0,
                },
            },
            Screen::ConversationView(v) => {
                v.scroll = 0;
                v.sel_msg = 0;
            }
            Screen::Search(s) => s.sel = 0,
            Screen::MediaGallery(m) => {
                m.sel = 0;
                m.scroll = 0;
            }
            Screen::Resources(r) => r.sel = 0,
            Screen::ResourceView(r) => r.scroll = 0,
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
                v.scroll = v.lines.len();
                v.sel_post = v.posts.len().saturating_sub(1);
                v.width = 0;
            }
            Screen::Inbox(ib) => match ib.focus {
                InboxPane::View => {
                    if let Some(v) = &mut ib.view {
                        v.scroll = v.lines.len();
                        v.sel_msg = v.messages.len().saturating_sub(1);
                    }
                }
                InboxPane::List => match ib.tab {
                    InboxTab::Conversations => {
                        ib.convos.sel = ib.convos.conversations.len().saturating_sub(1)
                    }
                    InboxTab::Alerts => ib.alerts.sel = ib.alerts.alerts.len().saturating_sub(1),
                },
            },
            Screen::ConversationView(v) => {
                v.scroll = v.lines.len();
                v.sel_msg = v.messages.len().saturating_sub(1);
            }
            Screen::Search(s) => s.sel = s.results.len().saturating_sub(1),
            Screen::MediaGallery(m) => m.sel = m.items.len().saturating_sub(1),
            Screen::Resources(r) => r.sel = r.items.len().saturating_sub(1),
            Screen::ResourceView(r) => r.scroll = r.lines.len().saturating_sub(1),
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
            Screen::MediaGallery(_) => "THIS GALLERY",
            Screen::Resources(_) => "THESE RESOURCES",
            Screen::ResourceView(_) => "THIS RESOURCE",
            Screen::ImageView(_) => "THIS IMAGE",
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
            Screen::MediaGallery(_) => "Media Gallery",
            Screen::Resources(_) => "Resources",
            Screen::ResourceView(_) => "Resource",
            Screen::ImageView(_) => "Image",
        }
    }
}

// ---------- shared render helpers ----------

/// Map a BBCode style to a terminal style. Quote bodies dim, code goes cyan,
/// spoilers disappear until rendered deliberately elsewhere.
pub(crate) fn style_from(
    theme: &Theme,
    s: &common::bbcode::Style,
    reveal_spoilers: bool,
) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    let mut st = Style::new().fg(theme.text);
    if s.quote_depth > 0 {
        st = st.fg(theme.dim).add_modifier(Modifier::ITALIC);
    }
    if s.code {
        st = theme.code();
    }
    if s.spoiler && !reveal_spoilers {
        // Hidden until `x` reveals (#621): black on black hides the text on
        // every terminal, where Modifier::HIDDEN is widely unimplemented.
        st = Style::new().fg(Color::Black).bg(Color::Black);
    }
    // #702: the post's own colour. After `code`/`quote` (which set a colour
    // of their own) and before the spoiler check below, because a hidden
    // spoiler must stay black-on-black whatever colour it carries.
    if let Some(rgb) = s.color
        && !s.code
        && let Some(c) = crate::theme::quantize(theme.tier, rgb)
    {
        st = st.fg(c);
    }
    // `[FONT=monospace]` and friends: a terminal is already monospace, so
    // the only honest reading is "the author meant this as code". The code
    // foreground says that without the block background a real [CODE] gets.
    if s.mono && !s.code {
        st = st.fg(theme.code_fg);
    }
    if s.highlight {
        // XF's marker pen. Reversed rather than a fixed background: it has
        // to stay legible against whatever colour the text already carries,
        // and on every tier including mono.
        st = st.add_modifier(Modifier::REVERSED);
    }
    // A terminal cannot scale glyphs, so XF's 1-7 size scale reads as
    // emphasis: above normal is bold, below it is dim. Headings are sizes
    // the author meant structurally, so they carry both bold and the
    // accent colour the rest of the chrome uses for headings.
    if let Some(level) = s.heading {
        // XF's three levels are h2/h3/h4 — a real hierarchy, so they get
        // three weights here rather than one: the top one is underlined as
        // well as bold, the middle one keeps the accent colour, and the
        // third is bold alone.
        st = st.add_modifier(Modifier::BOLD);
        if level <= 2 && theme.tier != crate::theme::Tier::Mono && s.color.is_none() {
            st = st.fg(theme.accent);
        }
        if level == 1 {
            st = st.add_modifier(Modifier::UNDERLINED);
        }
    } else if let Some(size) = s.size {
        match size {
            1..=3 => st = st.add_modifier(Modifier::DIM),
            5..=7 => st = st.add_modifier(Modifier::BOLD),
            _ => {}
        }
    }
    if s.spoiler && !reveal_spoilers {
        // Re-applied after colour and size: a hidden spoiler is hidden
        // whatever it carries (#621).
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
                ..Default::default()
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
            Screen::MediaGallery(MediaListState {
                items: vec![MediaItem {
                    media_id: 33005,
                    title: "Registry Explained".into(),
                    username: "op".into(),
                    media_date: 1_700_000_000,
                    ..Default::default()
                }],
                page: 1,
                last_page: 4,
                total: 80,
                ..Default::default()
            }),
            Screen::Resources(ResourceListState {
                items: vec![Resource {
                    resource_id: 150883,
                    title: "Handy tool".into(),
                    tag_line: "does things".into(),
                    username: "author".into(),
                    resource_date: 1_700_000_000,
                    download_count: 42,
                    rating_avg: Some(4.5),
                    ..Default::default()
                }],
                page: 1,
                last_page: 1,
                total: 1,
                ..Default::default()
            }),
            Screen::ResourceView(ResourceViewState {
                id: 150883,
                resource: Some(Resource {
                    resource_id: 150883,
                    title: "Handy tool".into(),
                    tag_line: "does things".into(),
                    username: "author".into(),
                    version: "2.5.8".into(),
                    description: "A [B]useful[/B] tool.".into(),
                    download_count: 42,
                    rating_avg: Some(4.5),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            Screen::ImageView(ImageViewState {
                title: "Registry Explained".into(),
                meta: "op \u{b7} 3d \u{b7} 1536\u{d7}1024".into(),
                description: "A screenshot of the registry editor.".into(),
                key: Some("https://data.windowsforum.com/x.jpg".into()),
                px: Some((1536, 1024)),
                web_url: Some("https://windowsforum.com/media/x.1/".into()),
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

    /// The Inbox's `g g`/`G` and web_url must follow the focused pane, the
    /// way Home's do. With the view pane focused they used to move the
    /// hidden list selection instead — in the narrow layout the list is not
    /// even on screen, so `G` mutated invisible state while the visible
    /// conversation did not move.
    #[test]
    fn inbox_goto_and_web_url_follow_the_focused_pane() {
        let convo = |id: u32, url: &str| Conversation {
            conversation_id: id,
            view_url: Some(url.into()),
            ..Default::default()
        };
        let mut inbox = Screen::Inbox(InboxState {
            convos: ConversationsState {
                conversations: vec![convo(1, "https://wf/c.1/"), convo(2, "https://wf/c.2/")],
                ..Default::default()
            },
            view: Some(ConversationViewState {
                conversation: convo(3, "https://wf/c.3/"),
                messages: vec![ConversationMessage::default(), ConversationMessage::default()],
                lines: vec![ratatui::text::Line::raw("one"), ratatui::text::Line::raw("two")],
                ..Default::default()
            }),
            ..Default::default()
        });

        // List focus keeps the classic behaviour.
        inbox.goto_bottom();
        assert!(
            matches!(&inbox, Screen::Inbox(ib) if ib.focus == InboxPane::List && ib.convos.sel == 1),
            "list focus: G moves the list selection"
        );
        assert_eq!(
            inbox.web_url().as_deref(),
            Some("https://wf/c.2/"),
            "list focus: the URL is the selected conversation's"
        );

        // View focus: G/g g scroll the open conversation and leave the list
        // alone; the URL names the conversation on screen.
        {
            let Screen::Inbox(ib) = &mut inbox else { panic!("inbox") };
            ib.focus = InboxPane::View;
            ib.convos.sel = 0;
        }
        inbox.goto_bottom();
        {
            let Screen::Inbox(ib) = &inbox else { panic!("inbox") };
            assert_eq!(ib.convos.sel, 0, "the hidden list must not move");
            let Some(v) = &ib.view else { panic!("view") };
            assert_eq!(v.sel_msg, 1, "view focus: G selects the last message");
            assert_eq!(v.scroll, v.lines.len(), "view focus: G scrolls to the end");
        }
        assert_eq!(
            inbox.web_url().as_deref(),
            Some("https://wf/c.3/"),
            "view focus: the URL is the viewed conversation's"
        );
        inbox.goto_top();
        {
            let Screen::Inbox(ib) = &inbox else { panic!("inbox") };
            let Some(v) = &ib.view else { panic!("view") };
            assert_eq!((v.sel_msg, v.scroll), (0, 0), "view focus: g g returns to the top");
        }
    }

    /// A key-bar label's literal key(s), or `None` for a label with no
    /// single key to press: a compound/movement pair (`j/k`, `n/N`, `[/]`,
    /// `1/2/3`, `1-9`, ...) or `Tab`, which is a within-screen focus/field
    /// switch that deliberately returns `Action::None` everywhere (its own
    /// screens pin that directly, e.g.
    /// `social::tab_key_switches_tabs_from_the_list_and_returns_from_the_view`).
    fn key_for_label(label: &str) -> Option<KeyEvent> {
        match label {
            "j/k" | "h/l" | "n/N" | "[/]" | "[ ]" | "1/2/3" | "1-9" | "Tab" => None,
            "Enter" => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            "Esc" => Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            _ if label.len() == 2 && label.starts_with('^') => Some(KeyEvent::new(
                KeyCode::Char(label.chars().nth(1).unwrap().to_ascii_lowercase()),
                KeyModifiers::CONTROL,
            )),
            _ if label.chars().count() == 1 => Some(KeyEvent::new(
                label.chars().next().map(KeyCode::Char).unwrap(),
                KeyModifiers::NONE,
            )),
            _ => None,
        }
    }

    /// Issue #561: `q` was advertised on the Login key bar but `login_key`
    /// never handled it, so it silently did nothing. Rather than pin that
    /// one key, press every key each screen's own key bar advertises and
    /// check the dispatch actually does something — the same class of bug
    /// (a hint promising a key its screen's `on_key` never handles) can
    /// recur for any other cap.
    ///
    /// Each screen below is built with just enough state for its non-skipped
    /// keys to produce a real `Action`. A key is skipped, with a reason,
    /// only when it is: compound/movement (`key_for_label` returns `None`),
    /// routed by `App::handle_key`'s global nav before `Screen::on_key` ever
    /// sees it (`c`/`a`/`s`/`/`/`g`/`?`), or a deliberate screen-internal
    /// toggle with no `Action` of its own (documented inline at the arm that
    /// handles it, e.g. `w`/`o` in `thread_view_key`, `^O` in `compose_key`).
    #[test]
    fn every_advertised_key_dispatches_to_something() {
        // Global keys `App::handle_key` intercepts before a screen's
        // `on_key` ever runs (app.rs's `c`/`a`/`s`//`` block and the `g`/`?`
        // arms) — a screen's own dispatch has nothing to check for these.
        const GLOBAL_NAV: &[&str] = &["c", "a", "s", "/", "g", "?"];

        struct Case {
            name: &'static str,
            // A factory rather than one instance: pressing an earlier key
            // (e.g. Compose's `^S`) mutates state (`busy = true`) that would
            // make a later key's check spurious if the screen were shared,
            // so every key gets its own fresh instance.
            factory: fn() -> Screen,
            /// Extra labels to skip on top of `GLOBAL_NAV`, each with the
            /// reason inlined at its call site below.
            skip: &'static [&'static str],
        }

        fn login_waiting() -> Screen {
            let login = LoginState {
                stage: LoginStage::Waiting,
                url: "https://windowsforum.com/tui-start/abc123".into(),
                ..Default::default()
            };
            Screen::Login(login)
        }

        let cases = vec![
            Case {
                name: "Login",
                factory: login_waiting,
                skip: &[],
            },
            Case {
                name: "Home",
                factory: || Screen::Home(HomeState {
                    tree: ForumTreeState {
                        nodes: vec![Node {
                            node_id: 4,
                            title: "Windows News".into(),
                            node_type: "Forum".into(),
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
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    focus: Pane::List,
                    ..Default::default()
                }),
                // `j/k` is a compound label; `Tab` switches panes in place.
                skip: &["j/k", "Tab"],
            },
            Case {
                name: "ForumTree",
                factory: || Screen::ForumTree(ForumTreeState {
                    nodes: vec![Node {
                        node_id: 4,
                        title: "Windows News".into(),
                        node_type: "Forum".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                skip: &["j/k", "h/l", "1/2/3"],
            },
            Case {
                name: "ThreadList",
                factory: || Screen::ThreadList(ThreadListState {
                    node_id: 4,
                    title: "Windows News".into(),
                    threads: vec![Thread {
                        thread_id: 1,
                        title: "A thread".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                skip: &["j/k", "[/]"],
            },
            Case {
                name: "ThreadView",
                factory: || Screen::ThreadView(ThreadViewState {
                    thread: Thread {
                        thread_id: 1,
                        title: "A thread".into(),
                        view_url: Some("https://windowsforum.com/threads/1/".into()),
                        ..Default::default()
                    },
                    posts: vec![Post {
                        post_id: 9,
                        username: "HItest".into(),
                        message: "hello".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                skip: &[
                    "j/k", "n/N", "1-9",
                    // `x` only flips `reveal_spoilers` — screen state, no
                    // `Action` (pinned directly by the spoiler-reveal test).
                    "x",
                    // `w` (watch) is deliberately always `Action::None`:
                    // XenForo's REST API has no thread-watch endpoint (see
                    // the comment on its arm in `thread_view_key`).
                    "w",
                    // `o` (links) opens a local popup overlay — a screen
                    // state change, not an `Action` — so it too always
                    // returns `Action::None` by design.
                    "o",
                ],
            },
            Case {
                name: "Compose",
                factory: || Screen::Compose(ComposeState {
                    target: Some(ComposeTarget::ThreadReply {
                        thread_id: 1,
                        thread_title: "A thread".into(),
                    }),
                    body: "draft".into(),
                    ..Default::default()
                }),
                skip: &[
                    "Tab",
                    // `^O` only flips the narrow-layout preview toggle —
                    // screen state, no `Action` (see its arm in
                    // `compose_key`).
                    "^O",
                    // No `^A` entry to skip here any more (issue #577):
                    // `compose_hints` used to advertise `^A attach` while
                    // `Ctrl+A` actually falls through to the readline
                    // `move_home` binding in both fields (attachment upload
                    // is implemented in `common` but not wired into compose
                    // — CLAUDE.md "Known gaps") — an advertised key doing
                    // something else with no notice. The cap was dropped
                    // from both hint sets instead of adding it to this skip
                    // list, so this loop no longer sees it at all; pinned
                    // directly by
                    // `misc::tests::compose_hints_never_advertise_attach_and_ctrl_a_still_moves_home`.
                ],
            },
            Case {
                name: "Inbox",
                factory: || Screen::Inbox(InboxState {
                    convos: ConversationsState {
                        conversations: vec![Conversation {
                            conversation_id: 3,
                            title: "A DM".into(),
                            ..Default::default()
                        }],
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
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                skip: &[
                    "Tab",
                    // The List pane's own `on_key` has no `Esc` arm at all
                    // (only `q`/`h`/`Left` pop); "back" for `Esc` is
                    // delivered entirely by `App::handle_key`'s
                    // `EscIntent::App` handling (it pops the screen before
                    // `Screen::on_key` is ever called for it), so there is
                    // nothing for this screen-level dispatch check to see.
                    "Esc",
                ],
            },
            Case {
                // The state `open_inbox` lands in on a narrow terminal or
                // the Alerts tab: no view pane, so `r reply` must not be
                // advertised at all (it used to be a silent no-op there).
                name: "Inbox (no view pane)",
                factory: || Screen::Inbox(InboxState {
                    convos: ConversationsState {
                        conversations: vec![Conversation {
                            conversation_id: 3,
                            title: "A DM".into(),
                            ..Default::default()
                        }],
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
                    view: None,
                    ..Default::default()
                }),
                skip: &["Tab", "Esc"],
            },
            Case {
                // The Media Gallery catalog: with items present Enter/o open
                // the selected media page; R refreshes.
                name: "MediaGallery",
                factory: || Screen::MediaGallery(MediaListState {
                    items: vec![MediaItem {
                        media_id: 33005,
                        title: "Registry Explained".into(),
                        username: "op".into(),
                        view_url: Some("https://windowsforum.com/media/registry.33005/".into()),
                        ..Default::default()
                    }],
                    page: 1,
                    last_page: 2,
                    ..Default::default()
                }),
                skip: &["Esc"],
            },
            Case {
                // The Resource Manager catalog: same shape, resources carry
                // tag lines and download counts.
                name: "Resources",
                factory: || Screen::Resources(ResourceListState {
                    items: vec![Resource {
                        resource_id: 150883,
                        title: "Handy tool".into(),
                        tag_line: "does things".into(),
                        username: "author".into(),
                        view_url: Some("https://windowsforum.com/resources/handy.150883/".into()),
                        ..Default::default()
                    }],
                    page: 1,
                    last_page: 1,
                    ..Default::default()
                }),
                skip: &["Esc"],
            },
            Case {
                // `end_session` and every first run land here: Enter begins
                // sign-in, q quits — both worked, and this pins them so a
                // hint change cannot silently advertise a dead key (#658).
                name: "Login (Idle)",
                factory: || Screen::Login(LoginState::default()),
                skip: &[],
            },
            Case {
                // The new-thread flow: `Tab` switches fields and `^O`/`^Y`
                // are screen-state toggles (no `Action`), like the
                // ThreadReply compose; `^S` with a non-empty draft submits.
                name: "Compose (new thread)",
                factory: || Screen::Compose(ComposeState {
                    target: Some(ComposeTarget::NewThread { node_id: 4 }),
                    title: "A title".into(),
                    body: "A body".into(),
                    title_field: true,
                    ..Default::default()
                }),
                skip: &["Tab", "^O", "^Y"],
            },
            Case {
                // The view pane focused (#605's state): `r` replies and `p`
                // opens the open message's author — the bar's keys must work
                // from this pane, not just the list's.
                name: "Inbox (view focused)",
                factory: || Screen::Inbox(InboxState {
                    focus: InboxPane::View,
                    view: Some(ConversationViewState {
                        conversation: Conversation {
                            conversation_id: 3,
                            title: "A DM".into(),
                            username: "kemical".into(),
                            ..Default::default()
                        },
                        messages: vec![ConversationMessage {
                            message_id: 1,
                            user_id: 7,
                            username: "kemical".into(),
                            ..Default::default()
                        }],
                        lines: vec![ratatui::text::Line::raw("hello")],
                        msg_line_offsets: vec![0],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                skip: &["Tab", "Esc"],
            },
            Case {
                // The Alerts tab: Enter/m mark read, `n` starts a message,
                // `R` refreshes — and `r` is hidden (no view pane; see
                // "Inbox (no view pane)").
                name: "Inbox (Alerts tab)",
                factory: || Screen::Inbox(InboxState {
                    tab: InboxTab::Alerts,
                    alerts: AlertsState {
                        alerts: vec![Alert {
                            alert_id: 1,
                            username: "kemical".into(),
                            content_type: "post_quote".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    view: None,
                    ..Default::default()
                }),
                skip: &["Tab", "Esc"],
            },
            Case {
                // Typing in the query field: only the editor's own keys are
                // advertised (the browse-mode caps type text there).
                name: "Search (input mode)",
                factory: || Screen::Search(SearchState {
                    query: "edge".into(),
                    input_mode: true,
                    ..Default::default()
                }),
                // `Esc` leaves edit mode — screen state, no `Action`.
                skip: &["Esc"],
            },
            Case {
                // A member's content list (profile `t`/`p`): `t` flips
                // threads/posts through search_member; with no results,
                // "Enter open" is not advertised at all.
                name: "Search (member mode)",
                factory: || Screen::Search(SearchState {
                    query: "by: kemical (thread)".into(),
                    member: Some((42, "thread".into())),
                    content_type: 1,
                    ..Default::default()
                }),
                // `Esc` pops; `i` clears the label and flips into query edit
                // — screen state, no `Action`.
                skip: &["Esc", "i"],
            },
            Case {
                name: "ConversationView",
                factory: || Screen::ConversationView(ConversationViewState {
                    conversation: Conversation {
                        conversation_id: 3,
                        title: "A DM".into(),
                        username: "HItest".into(),
                        ..Default::default()
                    },
                    messages: vec![ConversationMessage {
                        message_id: 1,
                        user_id: 7,
                        username: "kemical".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                skip: &[
                    "j/k", "n/N", "[/]",
                    // `x` only flips `reveal_spoilers` — screen state, no
                    // `Action` (pinned by the social spoiler-reveal test).
                    "x",
                ],
            },
            Case {
                name: "NewConversation",
                factory: || Screen::NewConversation(NewConversationState {
                    field: 2,
                    recipients: "kemical".into(),
                    title: "hi".into(),
                    body: "hello".into(),
                    ..Default::default()
                }),
                skip: &["Tab"],
            },
            Case {
                name: "Search",
                factory: || Screen::Search(SearchState {
                    query: "edge update".into(),
                    results: vec![SearchHit {
                        content_type: "thread".into(),
                        content_id: 1,
                        title: "A hit".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                skip: &[
                    "[ ]",
                    // `i` only flips into query-edit mode — screen state,
                    // no `Action` (see its arm in `search_key`).
                    "i",
                ],
            },
            Case {
                // The resource page: j/k scroll (no Action), d downloads,
                // o opens the site, R refetches.
                name: "ResourceView",
                factory: || Screen::ResourceView(ResourceViewState {
                    id: 1,
                    resource: Some(Resource {
                        resource_id: 1,
                        title: "Handy tool".into(),
                        view_url: Some("https://windowsforum.com/resources/x.1/".into()),
                        current_download_url: Some(
                            "https://windowsforum.com/resources/x.1/download".into(),
                        ),
                        ..Default::default()
                    }),
                    lines: vec![ratatui::text::Line::raw("body")],
                    ..Default::default()
                }),
                // `j/k` is a compound label, and scrolling is screen state
                // with no `Action` of its own.
                skip: &["j/k", "Esc"],
            },
            Case {
                name: "ImageView",
                factory: || Screen::ImageView(ImageViewState {
                    title: "A picture".into(),
                    key: Some("https://data.windowsforum.com/x.jpg".into()),
                    web_url: Some("https://windowsforum.com/media/x.1/".into()),
                    ..Default::default()
                }),
                skip: &["Esc"],
            },
            Case {
                name: "Profile",
                factory: || Screen::Profile(ProfileState {
                    title: "HItest".into(),
                    user: Some(User {
                        user_id: 7,
                        username: "HItest".into(),
                        view_url: Some("https://windowsforum.com/members/hitest.7/".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                skip: &[],
            },
        ];

        for Case { name, factory, skip } in cases {
            let hints = factory().hints();
            for (label, _desc) in &hints.keys {
                if GLOBAL_NAV.contains(label) || skip.contains(label) {
                    continue;
                }
                let Some(key) = key_for_label(label) else {
                    continue;
                };
                // A fresh instance per key: pressing an earlier one may have
                // mutated state (e.g. Compose's `^S` sets `busy = true`,
                // which makes every later key return `Action::None`) that
                // would make this check spurious if screens were shared.
                let mut screen = factory();
                let action = screen.on_key(key);
                assert!(
                    !matches!(action, Action::None),
                    "{name}: advertised key {label:?} dispatched to nothing"
                );
            }
        }
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
                            s.render(f, area, &theme, g, &mut crate::hit::HitMap::default());
                        })
                        .expect("render");
                    }
                }
            }
        }
    }
}
