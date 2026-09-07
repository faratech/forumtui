//! Application state: screen stack, message pump, background tasks, frame.

use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tokio::sync::mpsc;

use common::api::{Toggle, WfApi, WfApiClient};
use common::error::Error;
use common::models::*;

use crate::chrome::{self, GateState};
use crate::event;
use crate::glyph::{self, Glyphs};
use crate::hit::{self, Hit, HitMap};
use crate::overlay::{self, GoTarget, Palette, PaletteEvent, Prefix, PrefixEvent};
use crate::screens::{self, Action, ComposeTarget, Screen};
use crate::theme::Theme;

/// How many image loads may be in flight at once (issue #543). Small on
/// purpose: decoration is never what the user is waiting for, and `draw`
/// spawns one task per visible image slot the moment a thread opens.
const IMAGE_LOAD_CONCURRENCY: usize = 3;

/// How long a status *toast* (`App::set_status`) stays on the status row
/// before the event loop clears it (DESIGN.md: "Toasts (success/error)
/// replace the left text and clear after 4 s" — issue #571).
const STATUS_TOAST_SECS: u64 = 4;
/// How long `session_recovery_pending` may stay open before the event loop's
/// tick force-clears it as a pure backstop (issue #587/#591): the recheck
/// task itself now reports back unconditionally — a `ReportGuard` inside it
/// (see `recheck_stored_session`) sends its own `Msg::Bootstrap { Err(NoToken) }`
/// if the task ever exits (panics, is dropped) without having sent anything —
/// so this timer only needs to cover the case where that report is somehow
/// never delivered at all.
///
/// It must sit ABOVE the recheck's own legitimate network budget: a single
/// `/me` call is `CONNECT_TIMEOUT` (10 s) + `REQUEST_TIMEOUT` (30 s), the
/// recheck can spend that twice (a token refresh inside `valid_token()`
/// before the `/me` request itself), and `api_gate` may additionally be
/// carrying a rate-limit penalty (`DEFAULT_RATE_LIMIT_RETRY_SECS` = 30 s, or
/// a server-supplied `Retry-After`). 15 s was shorter than one single
/// `REQUEST_TIMEOUT`, so a merely slow — not dead — recheck used to be timed
/// out from under a still-valid session (issue #591). 120 s comfortably
/// covers 2×(10+30) + a 30-40 s retry-after penalty with headroom.
const SESSION_RECOVERY_TIMEOUT_SECS: u64 = 120;

/// What kind of failure a `TaskError` carries — just enough for a caller to
/// decide whether the stored session itself is the problem, as opposed to
/// the network or the server having a bad moment (issue #551).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskErrorKind {
    /// `Error::NoToken` — there was never a session to restore.
    NoToken,
    /// The OAuth token endpoint refused the grant/refresh outright.
    OAuth,
    /// The API answered with a structured error; carries its HTTP status.
    Api(u16),
    /// Everything else: transport failures (DNS/connect/timeout/TLS/5xx
    /// bundled as `Error::Http`), the client's own throttle gate, I/O, etc.
    /// The token may still be perfectly valid — this is "try again", not
    /// "log in again".
    Other,
}

impl TaskErrorKind {
    /// True only for the failures that mean the stored session itself is
    /// invalid — the sole cases that should ever send the user back to the
    /// login screen. A transient transport error or a server 5xx must never
    /// discard a working token (issue #551). This is a coarse, variant-only
    /// check; `TaskError::ends_session()` refines it further by also
    /// inspecting `code` (issue #555) and is what callers should actually
    /// use.
    ///
    /// `Api(403)` is deliberately NOT here (issue #564): XenForo answers
    /// ordinary permission refusals with HTTP 403 too — reacting to your own
    /// post, replying to a closed thread, a missing OAuth scope, viewing a
    /// forum/thread you can't — and none of those means the *token* is bad.
    /// The only 403s that really mean "this account/session is over" are the
    /// bootstrap `/me` path's account-gone codes, which
    /// `TaskError::is_account_gone()` recognizes by `code` for the one
    /// caller (`Msg::Bootstrap`) that needs them.
    pub fn ends_session(self) -> bool {
        matches!(self, TaskErrorKind::NoToken | TaskErrorKind::OAuth | TaskErrorKind::Api(401))
    }
}

/// Pages one navigation may pull in to fill the pane (#705). Two extra
/// pages cover any realistic terminal at XF's 20 rows a page; without a
/// bound, a pane measured from the previous screen — or a forum whose pages
/// come back short — walks page after page, which is what the reader saw as
/// "it downloaded many pages and the pagination kept changing".
const FILL_PAGE_BUDGET: u8 = 3;

/// Ceiling on a file this client will upload (#709). XF has its own limit,
/// usually lower, but reading a huge file into memory to be refused by the
/// server is worse than saying so first.
const MAX_UPLOAD_BYTES: u64 = 24 * 1024 * 1024;

/// `~` in a typed path means what the shell means by it — the prompt is
/// where a path is typed, and a terminal user types `~/shot.png`.
fn shellexpand_home(path: &str) -> String {
    let trimmed = path.trim();
    let Some(rest) = trimmed.strip_prefix('~') else {
        return trimmed.to_string();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return trimmed.to_string();
    };
    let mut out = std::path::PathBuf::from(home);
    let rest = rest.trim_start_matches('/');
    if !rest.is_empty() {
        out.push(rest);
    }
    out.to_string_lossy().to_string()
}

/// The content type for an upload, from the file's extension. XF sniffs the
/// bytes itself and this is only what the multipart part declares, so the
/// list stays short and honest: the types a support forum actually receives.
fn mime_for(filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "zip" => "application/zip",
        "pdf" => "application/pdf",
        "dmp" | "etl" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

/// Serializable error payload crossing from background tasks into the UI.
#[derive(Debug, Clone)]
pub struct TaskError {
    pub message: String,
    #[allow(dead_code)]
    pub code: Option<String>,
    pub max_page: Option<u32>,
    pub kind: TaskErrorKind,
}

/// Hard cap on how much of an *unstructured* error message ever reaches a
/// status line. `Error::Api`/`Error::OAuth`'s `"http_error"` fallback (and,
/// in principle, any opaque `Display` in the `Other` bucket) hands back
/// whatever the server sent verbatim — a raw JSON envelope, a Cloudflare
/// challenge page, a WAF HTML block — and that is exactly what pushed
/// "press r to retry" off the end of a 120-column terminal (issue #561).
/// A genuinely parsed server message (a real `code`) is left untouched:
/// this only guards the fallback case.
const RAW_MESSAGE_CAP: usize = 80;

/// Truncate `s` to at most `max` *chars* (not bytes, so a multi-byte
/// codepoint is never split), appending `…` when it was longer.
fn cap_message(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

impl TaskError {
    /// True only when the failure means the stored session itself is
    /// invalid. This refines `TaskErrorKind::ends_session()` (which only
    /// looks at status/variant) with the wire-level `code`: an OAuth or
    /// Api(401) failure whose `code` is the synthetic `"http_error"` —
    /// meaning the body wasn't JSON at all, e.g. a Cloudflare 5xx/429 page
    /// or a WAF challenge HTML response — is never a genuine "this token/
    /// grant is bad" rejection, so it must not end the session (issue #555;
    /// sibling of #551, which handled the coarse status classification).
    pub fn ends_session(&self) -> bool {
        self.kind.ends_session() && self.code.as_deref() != Some("http_error")
    }

    /// True for the small set of HTTP 403s that mean "there is no account to
    /// restore", not "you lack permission for this one action": XenForo's
    /// bootstrap `/me` call (`App::restore_session`/`recheck_stored_session`,
    /// answered as `Msg::Bootstrap`) can 403 with `you_have_been_banned`,
    /// `your_account_has_been_rejected`/`your_account_has_been_disabled`
    /// (`XF\Api\Controller\AbstractController::assertUserState`), or
    /// `api_error.api_key_inactive` (`XF\Api\App::validateUserFromApiHeader`)
    /// — every one of those really does mean the session is over. Every
    /// other 403 (permission refusals on an ordinary content call,
    /// `missing_scope`) must NOT end the session — see
    /// `TaskErrorKind::ends_session` (issue #564). Called by
    /// `session_error_of` for `Msg::Bootstrap`, and (issue #594) by
    /// `Msg::LoginComplete { Err }`'s handler — the freshly exchanged token's
    /// own `/me` verification call in `finish_login` is the other place a
    /// bootstrap-shaped 403 like this can land.
    fn is_account_gone(&self) -> bool {
        if !matches!(self.kind, TaskErrorKind::Api(403)) {
            return false;
        }
        let Some(code) = self.code.as_deref() else { return false };
        let code = code.strip_prefix("api_error.").unwrap_or(code);
        matches!(
            code,
            "you_have_been_banned"
                | "your_account_has_been_rejected"
                | "your_account_has_been_disabled"
                | "api_key_inactive"
        )
    }

    pub fn of(e: &Error) -> Self {
        match e {
            Error::Api { code, message, status, max_page } => TaskError {
                message: if code == "http_error" {
                    cap_message(message, RAW_MESSAGE_CAP)
                } else if code.strip_prefix("api_error.").unwrap_or(code) == "missing_scope" {
                    // A missing OAuth scope is not a permission the member
                    // lacks and not something a retry fixes (issue #564).
                    // It is one of two things, and the remedy is the same
                    // for both: the grant this token was issued under
                    // predates the scope (issue #695 — the catalogs shipped
                    // before `media:read`/`resource:read` were requested),
                    // or the client registration is out of date. Signing in
                    // again mints a token with the current scope set, so
                    // name that instead of leaving the raw phrase to be
                    // read as a dead end.
                    format!("{message}. Sign out with Ctrl+L and sign in again to grant it.")
                } else {
                    message.clone()
                },
                code: Some(code.clone()),
                max_page: *max_page,
                kind: TaskErrorKind::Api(*status),
            },
            Error::OAuth { code, message, .. } => TaskError {
                message: if code == "http_error" {
                    cap_message(message, RAW_MESSAGE_CAP)
                } else {
                    message.clone()
                },
                code: Some(code.clone()),
                max_page: None,
                kind: TaskErrorKind::OAuth,
            },
            Error::NoToken => TaskError {
                message: e.to_string(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::NoToken,
            },
            other => TaskError {
                message: cap_message(&other.to_string(), RAW_MESSAGE_CAP),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Other,
            },
        }
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

type TaskResult<T> = Result<T, TaskError>;

pub enum Msg {
    /// The stored-session check came back. `generation` is the
    /// `App::bootstrap_generation` the check was started under: `end_session`
    /// (and so `logout`) bumps it, so a restore that was already in flight
    /// when the user pressed Ctrl+L can never sign them back in against an
    /// erased token store (issue #557). `recheck_stored_session` also
    /// reports through this — never `SessionLost` — for both its outcomes,
    /// `Ok` and "nothing to adopt" (`Err(NoToken)`), so its own verdict is
    /// never confused with a poller's `SessionLost` landing in the same
    /// recovery window (issue #587).
    Bootstrap { generation: u64, result: Result<User, TaskError> },
    /// A background task decided the session itself is over — only the
    /// pollers send this (they otherwise drop their errors); the token-store
    /// recheck deliberately does not (issue #587), so this message shape
    /// inside a `session_recovery_pending` window can only ever be a
    /// poller's report of the grant the recheck is already replacing, and
    /// the pump's boundary swallows it as stale rather than ending the
    /// session out from under the recovery.
    SessionLost(TaskError),
    /// The three login messages carry the `generation` of the flow that sent
    /// them. `begin_login` bumps `App::login_generation` on every restart, so
    /// a message from a superseded flow (the user pressed Enter again after a
    /// denied approval) is dropped instead of overwriting the live one —
    /// issue #547.
    LoginReady { generation: u64, url: String },
    LoginFailed { generation: u64, message: String },
    LoginComplete { generation: u64, result: Result<User, TaskError> },
    NodesLoaded(TaskResult<Vec<Node>>),
    /// A forum page came back. `node_id` is the forum that was asked for, so
    /// a reply that outraced a newer request is dropped instead of landing in
    /// whichever list happens to be on top (`Gate::wait` only spaces request
    /// *starts*, so two `load_forum` calls can finish out of order).
    /// `append` marks a viewport-fill page (#699): it extends the list
    /// rather than replacing it, so a tall terminal is not left showing 20
    /// rows in a pane with room for 45.
    ForumLoaded { node_id: u32, page: u32, append: bool, seq: u64, result: TaskResult<ForumReply> },
    ThreadLoaded { id: u32, page: u32, result: TaskResult<ThreadReply> },
    ReplySent(TaskResult<Post>),
    ThreadCreated(TaskResult<Thread>),
    MarkedRead(TaskResult<()>),
    ConversationsLoaded { page: u32, result: TaskResult<ConversationsReply> },
    /// A conversation page came back. `mark_read` says whether this load was
    /// the USER opening the conversation — only then may the client tell the
    /// server it has been read. The dual-pane Inbox primes its right half with
    /// the newest conversation before anyone has selected it, and marking that
    /// read was a real data side-effect of merely pressing `c` (issue #541).
    ConversationLoaded {
        id: u32,
        page: u32,
        mark_read: bool,
        result: TaskResult<ConversationReply>,
    },
    /// The website's composer drafts, merged into the local store (#716).
    /// Carries no error arm on purpose: a draft sync that fails is a no-op
    /// the user cannot act on, so the failure is logged where it happens and
    /// never reaches the session boundary.
    DraftsLoaded(Vec<common::models::RemoteDraft>),
    ConvoReplySent(TaskResult<()>),
    ConvoCreated(TaskResult<Conversation>),
    /// The id travels with the result (issue #608) so a successful mark can
    /// flip that one row's `is_unread` locally instead of reloading page 1 of
    /// the conversations list — which used to throw the user back to page 1
    /// (and a re-clamped selection) no matter which page they marked from.
    ConversationMarked(u32, TaskResult<()>),
    /// One recipient name resolved (or failed to). The payload is a
    /// `TaskResult` so a transport/429/5xx/401 failure is not flattened into
    /// "not found" — and so a session-ending one still reaches the boundary
    /// (issue #597).
    RecipientResolved { name: String, id: TaskResult<Option<u32>> },
    AlertsLoaded(TaskResult<AlertsReply>),
    AlertMarked(TaskResult<()>),
    /// Media Gallery / Resource Manager catalog pages (issue #680). Replies
    /// adopt the topmost matching screen; the screens' loading guards keep
    /// one fetch in flight, so page stamping is enough identity.
    MediaLoaded { page: u32, result: TaskResult<MediaListReply> },
    ResourceLoaded { page: u32, result: TaskResult<ResourceListReply> },
    /// The gallery's category tree, and one resource's page (#697).
    MediaCategoriesLoaded(TaskResult<common::models::MediaCategoriesReply>),
    /// An attachment upload finished (#709): the key it went under and the
    /// file, or a message saying why not.
    AttachmentUploaded(std::result::Result<(String, common::models::Attachment), String>),
    /// Post edits, deletions and solution toggles (#708).
    PostEdited { post_id: u32, result: TaskResult<()> },
    PostDeleted { post_id: u32, thread_id: u32, result: TaskResult<()> },
    SolutionMarked { post_id: u32, thread_id: u32, result: TaskResult<()> },
    ResourceViewLoaded { id: u32, result: TaskResult<common::models::ResourceReply> },
    SearchDone { generation: u64, page: u32, result: TaskResult<SearchResultsReply> },
    /// A go-to palette member lookup came back. `query` is the palette query
    /// that asked, so a stale answer to an edited query is dropped.
    PaletteMember { query: String, user: Option<User> },
    /// A profile fetch came back. `generation` is the `open_profile` request
    /// the screen is waiting on; an older reply (profile A opened again, or
    /// B pushed over a slow A) is dropped instead of filling the topmost
    /// profile screen.
    ProfileLoaded { generation: u64, result: TaskResult<User> },
    LoggedOut(Result<(), String>),
    /// A thumbnail / avatar / the sign-in logo finished loading off-thread.
    /// Only sent by the `images` feature's loader.
    /// `key` is `images::store_key(url, cols, rows)` — decoded payloads are
    /// sized for one exact rect, so a resize re-encodes rather than stretches.
    #[cfg_attr(not(feature = "images"), allow(dead_code))]
    ImageLoaded {
        key: String,
        result: Result<crate::images::Loaded, String>,
    },
    /// A like or a vote came back. XF toggles both, so the reply's
    /// insert-vs-delete is what decides the wording, and the ♡/▲ footer is
    /// baked into the post lines, so the page is re-fetched (issue #538).
    PostToggled { verb: PostVerb, result: TaskResult<Toggle> },
    Notice(String),
}

/// Which toggle a `Msg::PostToggled` is answering, so the notice can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostVerb {
    Like,
    VoteUp,
    VoteDown,
}

impl PostVerb {
    fn of_vote(vote_type: &str) -> PostVerb {
        if vote_type.eq_ignore_ascii_case("down") {
            PostVerb::VoteDown
        } else {
            PostVerb::VoteUp
        }
    }

    /// The status line for an outcome. "liked" is never claimed for an unlike.
    pub fn notice(self, toggle: Toggle) -> &'static str {
        match (self, toggle) {
            (PostVerb::Like, Toggle::Inserted) => "Post liked.",
            (PostVerb::Like, Toggle::Removed) => "Like removed.",
            (PostVerb::VoteUp, Toggle::Inserted) => "Voted up.",
            (PostVerb::VoteDown, Toggle::Inserted) => "Voted down.",
            (PostVerb::VoteUp | PostVerb::VoteDown, Toggle::Removed) => "Vote removed.",
        }
    }

    /// The status line when the call itself failed.
    fn failure(self) -> &'static str {
        match self {
            PostVerb::Like => "Like failed",
            PostVerb::VoteUp | PostVerb::VoteDown => "Vote failed",
        }
    }
}

pub struct App {
    pub api: Arc<dyn WfApi>,
    pub client: Arc<WfApiClient>,
    pub tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    pub theme: Theme,
    pub glyphs: Glyphs,
    /// Inline graphics: detected tier, decoded-protocol LRU, disk cache.
    pub images: crate::images::Images,
    /// At most this many image loads are in flight at once. `draw` spawns one
    /// task per visible image slot in the first frame after a thread opens, so
    /// without a cap a cold cache turns one keypress into a screenful of
    /// concurrent fetch+decode work (issue #543). The per-request spacing is
    /// still `WfApiClient::image_gate`'s.
    #[cfg_attr(not(feature = "images"), allow(dead_code))]
    image_slots: Arc<tokio::sync::Semaphore>,
    pub screens: Vec<Screen>,
    pub me: Option<User>,
    pub alerts_unread: u32,
    /// Which screen each breadcrumb names, stamped every frame beside the
    /// header's hit boxes (#700).
    crumb_targets: Vec<usize>,
    /// Mints `ThreadListState::load_seq` (#705).
    next_list_seq: u64,
    pub convos_unread: u32,
    pub status: String,
    /// When the current `status` was shown as a toast (`Some`) — cleared by
    /// the event loop's tick once `STATUS_TOAST_SECS` have passed, at which
    /// point the status row goes blank rather than keep showing whatever
    /// last happened to succeed or fail (DESIGN.md: "Toasts (success/error)
    /// replace the left text and clear after 4 s" — unimplemented until
    /// issue #571). `None` means `status` is a persistent hint/progress
    /// message (`set_hint`) that stays until something else overwrites it.
    status_set_at: Option<std::time::Instant>,
    /// Set just before a reload whose only purpose is to refresh a thread page
    /// in place (`thread_id`, `page`, `sel_post`, `scroll`); consumed by the
    /// matching `Msg::ThreadLoaded` (issue #538).
    keep_thread_position: Option<(u32, u32, usize, usize)>,
    show_help: bool,
    /// The go-to palette, when it is open. It owns the keyboard while it is.
    palette: Option<Palette>,
    /// The `g` chord: armed by `g`, resolved (or cancelled) by the next key.
    prefix: Prefix,
    /// Mouse drag selection (anchor cell, end cell).
    selection: Option<Selection>,
    /// Mirror of the last drawn frame's cell symbols per row + byte offsets
    /// per column (the terminal buffer between draws is the blank next
    /// frame, so selection text must come from our own copy).
    screen_rows: Vec<String>,
    screen_cols: Vec<Vec<usize>>,
    /// In-app clipboard: last copied selection, pasteable with Ctrl+Y in the
    /// composer — works even when the terminal declines OSC 52.
    clipboard: String,
    last_click_instant: Option<std::time::Instant>,
    last_click_pos: (u16, u16),
    click_count: u8,
    /// Where the left button went down, so the release can tell a click from
    /// a drag: press and release in the same cell, with the band never having
    /// left it, is a click.
    press_pos: Option<(u16, u16)>,
    last_title: String,
    should_quit: bool,
    /// Handles for the alerts/conversations poll loops spawned by
    /// `start_pollers`, so `logout` can abort them instead of leaving them
    /// running (and doubled by the next login's `start_pollers` call) —
    /// issue #524.
    poller_handles: Vec<tokio::task::JoinHandle<()>>,
    /// Abort handles for the in-flight *writes* (post a reply, start a
    /// thread, send/reply to a DM) spawned by `spawn_write`. A write waits
    /// on `api_gate`/`write_gate` (up to 30 s after a previous post, 180 s
    /// after a new thread) before `valid_token()` is even consulted, so a
    /// write spawned by `^S` and then abandoned by `Ctrl+L` used to go out
    /// *after* sign-out — under whatever token was in memory by then,
    /// possibly the next account's on a shared machine — and announce
    /// "Reply posted." over the sign-in screen. `end_session` aborts them:
    /// a write belongs to the session that started it (issue #567).
    write_handles: Vec<tokio::task::AbortHandle>,
    /// True after a transient (transport/5xx) failure to restore a stored
    /// session — cleared on a successful restore or a session-ending error.
    /// While true, `r` re-runs the session check instead of whatever the top
    /// screen would otherwise do with it (issue #551).
    bootstrap_retry_needed: bool,
    /// Which stored-session check is the live one. Bumped by `end_session`
    /// (hence by `logout`), and stamped on `Msg::Bootstrap`, so a restore
    /// still in flight when the session ends is dropped instead of reviving
    /// a signed-out client (issue #557).
    bootstrap_generation: u64,
    /// One-shot guard for the mid-session token recheck: a `NoToken` in a
    /// live session may just mean a sibling instance rotated the shared
    /// `token.json`, so the store is re-read once before the session is
    /// ended. Cleared whenever a session begins or ends (issue #557).
    session_recovery_tried: bool,
    /// True while that recheck is actually in flight (set with
    /// `session_recovery_tried`, cleared by the `Msg::Bootstrap` it reports
    /// back — issue #587: deliberately never by a `Msg::SessionLost`, which
    /// inside this window can only be a poller's report of the very grant
    /// this recheck is already replacing). Any OAuth or NoToken rejection
    /// that lands inside that window belongs to that same grant — every
    /// caller queued behind the failed refresh reports the same one — so it
    /// is stale and must not end the session out from under the recovery
    /// (issue #568). `expire_session_recovery_timeout` bounds how long this
    /// may stay true if the recheck task never reports back at all.
    session_recovery_pending: bool,
    /// When `session_recovery_pending` went true — the event loop's tick
    /// compares this against `SESSION_RECOVERY_TIMEOUT_SECS` so a recheck
    /// task that dies without reporting back cannot leave the window (and
    /// every poller failure it swallows) open forever (issue #587). `None`
    /// whenever `session_recovery_pending` is false.
    session_recovery_started_at: Option<std::time::Instant>,
    /// Which login flow is the live one. Bumped by every `begin_login`, and
    /// stamped on the flow's messages so a superseded flow's `LoginReady` /
    /// `LoginFailed` / `LoginComplete` is ignored (issue #547).
    login_generation: u64,
    /// Which `open_profile` request is the live one, stamped on the pushed
    /// `ProfileState` and its `Msg::ProfileLoaded` so a slow reply to an
    /// older profile cannot fill a newer screen (same pattern as
    /// `login_generation`).
    profile_generation: u64,
    /// Minted by every search load and stamped on the waiting `SearchState`
    /// and its `Msg::SearchDone`. App-wide, so two Search screens (a member
    /// list pushed over a slow keyword search) can never mint the same
    /// number and adopt each other's replies.
    search_generation: u64,
    /// Abort handle for the in-flight login task, so pressing Enter while the
    /// client is still polling stops that poll loop instead of leaving two
    /// flows racing for the same screen.
    login_task: Option<tokio::task::AbortHandle>,
    /// The body zone of the last frame (between the header band and the key
    /// bar). The wheel scrolls what is inside it and nothing else (#549).
    body_rect: ratatui::layout::Rect,
    /// What the last frame drew *where* — rebuilt by `draw`, resolved by
    /// `handle_mouse`. Empty for the whole run when `WFTUI_MOUSE=0`, which
    /// is also when nothing enabled mouse capture in the first place.
    hits: HitMap,
    /// Unsent composer drafts, keyed by which composer they belong to
    /// (#715). Held in memory for the session and mirrored to disk, so Esc
    /// and a crash both survive.
    drafts: std::collections::HashMap<common::drafts::DraftKey, common::drafts::Draft>,
    draft_store: common::drafts::Store,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: (u16, u16),
    end: (u16, u16),
}

impl Selection {
    fn rect(&self) -> (u16, u16, u16, u16) {
        (
            self.anchor.0.min(self.end.0),
            self.anchor.1.min(self.end.1),
            self.anchor.0.max(self.end.0),
            self.anchor.1.max(self.end.1),
        )
    }
}

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        use ratatui::crossterm::event::{
            EnableBracketedPaste, EnableMouseCapture, KeyboardEnhancementFlags,
            PushKeyboardEnhancementFlags,
        };
        use ratatui::crossterm::terminal::*;
        enable_raw_mode()?;
        // `WFTUI_MOUSE=0` never captures: the terminal keeps its own
        // selection, scrollback and touch gestures, and the client builds no
        // hit map to resolve events it will not receive.
        let capture = common::config::mouse_enabled();
        let enter = if capture {
            ratatui::crossterm::execute!(
                std::io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste,
            )
        } else {
            ratatui::crossterm::execute!(
                std::io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
            )
        };
        if let Err(e) = enter {
            // Don't leave the terminal in raw mode if we're bailing out here —
            // otherwise the caller's `eprintln!` (and the shell prompt after
            // it) render with no line discipline.
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        Ok(TerminalGuard)
    }
}

/// The restore steps `restore_terminal` issues, in order — extracted as plain
/// data so the ordering can be pinned by a unit test without a real terminal.
/// The order matters (Pop first, matching the enable-side Push-last symmetry)
/// but each step MUST be independent: see `restore_terminal`. Test-only: this
/// exists purely as the pinned reference for `restore_terminal`'s ordering.
#[cfg(test)]
fn restore_steps() -> [&'static str; 5] {
    [
        "pop_keyboard_enhancement_flags",
        "disable_bracketed_paste",
        "leave_alternate_screen",
        "disable_mouse_capture",
        "cursor_show",
    ]
}

/// Shared by `TerminalGuard::drop` and the panic hook in `main.rs`.
///
/// Each crossterm command is issued in its OWN `execute!` call. In crossterm
/// 0.29 `PopKeyboardEnhancementFlags::is_ansi_code_supported()` is `false` on
/// Windows and its `execute_winapi()` returns `Err(Unsupported)`; `execute!`
/// expands to a `queue().and_then(queue).and_then(queue)...` chain, so a
/// single call listing all five commands would short-circuit at Pop and skip
/// `LeaveAlternateScreen`/`DisableMouseCapture`/`cursor::Show` on every exit
/// path on Windows (issue #531). Splitting them means Pop's failure can never
/// gate the commands that actually restore visible terminal state.
pub(crate) fn restore_terminal() {
    use ratatui::crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    let _ = execute!(std::io::stdout(), ratatui::crossterm::cursor::Show);
    let _ = disable_raw_mode();
    // LAST: crossterm restores the attributes it snapshotted at its first
    // `enable_raw_mode()`, which may have been taken while the graphics query
    // still had ICANON/ECHO cleared (#532). Our own pre-query snapshot wins.
    crate::tty::restore();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        emit_raw(&common::osc::set_title(""));
    }
}

#[cfg(test)]
mod terminal_guard_tests {
    use super::*;

    /// Pins the restore ordering: Pop first (mirroring Push-last on the way
    /// in), then the three commands that actually restore visible terminal
    /// state, then cursor::Show. Each is independent — see `restore_terminal`.
    #[test]
    fn restore_steps_are_ordered_and_independent() {
        let steps = restore_steps();
        assert_eq!(
            steps,
            [
                "pop_keyboard_enhancement_flags",
                "disable_bracketed_paste",
                "leave_alternate_screen",
                "disable_mouse_capture",
                "cursor_show",
            ]
        );
    }

    /// Regression for issue #531: `restore_terminal` must not early-return or
    /// panic when an individual crossterm command errors (e.g. no attached
    /// tty in a test process, or — on Windows — PopKeyboardEnhancementFlags
    /// being unsupported); every step is `let _ =`-isolated so later steps
    /// always run.
    #[test]
    fn restore_terminal_runs_to_completion_without_a_tty() {
        restore_terminal();
    }
}

pub async fn run(images: crate::images::Images) -> u8 {
    // Every fallible step that can be done on the normal screen happens
    // BEFORE `TerminalGuard::new()` enters the alternate screen. Once the
    // guard exists, an `eprintln!` here would land on the alt screen and be
    // erased the instant `_guard` drops and issues `LeaveAlternateScreen` —
    // the same tty, so the message never reaches the user (issue #546).
    let client = match WfApiClient::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("client init failed: {e}");
            return 1;
        }
    };
    let mut terminal = match ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(
        std::io::stdout(),
    )) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terminal init failed: {e}");
            return 1;
        }
    };

    let _guard = match TerminalGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("terminal setup failed: {e}");
            return 1;
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();

    // Signal handling is armed as the very first thing after raw mode is
    // entered. It used to spawn only after the whole `App` was built; a
    // SIGTERM/SIGHUP arriving before the handlers registered took the
    // default disposition and killed the process with the terminal left in
    // raw mode on the alternate screen. The loop is deliberate too: the old
    // `select!` served exactly one signal and then the task ended, so a
    // second signal delivered while teardown was still in progress reverted
    // to the default disposition and bricked the terminal anyway. Every
    // delivery just sends "quit" — acting on it is idempotent.
    #[cfg(unix)]
    {
        let tx = tx.clone();
        tokio::spawn(forward_signals(tx));
    }

    let draft_store = common::drafts::Store::new();
    let mut app = App {
        next_list_seq: 1,
        api: client.clone(),
        client,
        tx,
        rx,
        theme: Theme::detect(),
        glyphs: glyph::detect(),
        images,
        image_slots: Arc::new(tokio::sync::Semaphore::new(IMAGE_LOAD_CONCURRENCY)),
        screens: Vec::new(),
        me: None,
        alerts_unread: 0,
        crumb_targets: Vec::new(),
        convos_unread: 0,
        status: "Starting…".into(),
        status_set_at: None,
        keep_thread_position: None,
        show_help: false,
        palette: None,
        prefix: Prefix::default(),
        selection: None,
        screen_rows: Vec::new(),
        screen_cols: Vec::new(),
        clipboard: String::new(),
        last_click_instant: None,
        last_click_pos: (0, 0),
        click_count: 0,
        press_pos: None,
        last_title: String::new(),
        should_quit: false,
        poller_handles: Vec::new(),
        write_handles: Vec::new(),
        bootstrap_retry_needed: false,
        bootstrap_generation: 0,
        session_recovery_tried: false,
        session_recovery_pending: false,
        session_recovery_started_at: None,
        login_generation: 0,
        profile_generation: 0,
        search_generation: 0,
        login_task: None,
        body_rect: ratatui::layout::Rect::default(),
        hits: HitMap::new(common::config::mouse_enabled()),
        // Drafts left by the previous run (#715): an Esc or a crash mid-post
        // must not cost the words.
        drafts: draft_store.load(),
        draft_store,
    };
    app.bootstrap().await;
    let reader = crate::event::spawn_reader();
    let outcome = app.event_loop(&mut terminal, reader).await;
    drop(_guard);
    outcome
}

/// Deliver SIGTERM/SIGHUP/SIGINT as `Msg::Notice("quit")`, forever — a
/// signal can arrive while an earlier one's teardown is still running, and
/// the process must keep opting out of the default (terminal-bricking)
/// disposition until it is actually gone. Spawned by `run` the moment raw
/// mode is entered; ends only if a stream fails to register.
#[cfg(unix)]
async fn forward_signals(tx: mpsc::UnboundedSender<Msg>) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut hup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return,
    };
    loop {
        tokio::select! {
            _ = term.recv() => {
                tx.send(Msg::Notice("quit".into())).ok();
            }
            _ = hup.recv() => {
                tx.send(Msg::Notice("quit".into())).ok();
            }
            _ = int.recv() => {
                tx.send(Msg::Notice("quit".into())).ok();
            }
        }
    }
}

mod actions;
mod input;
mod msg;
mod session;

impl App {
    /// Show a status *toast* — a one-off success/failure notice ("Reply
    /// posted.", "Like removed.", "Copied 12 chars…") — that the event
    /// loop's tick clears on its own after `STATUS_TOAST_SECS` (DESIGN.md;
    /// issue #571). Use this for anything that reports what an action just
    /// did; use `set_hint` for an ongoing or persistent message instead.
    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_set_at = Some(std::time::Instant::now());
    }

    /// Show a persistent status line — a hint ("Sign in first, or press q
    /// to quit."), an in-progress notice ("Restoring session…"), or a gate
    /// explanation ("Session expired (...); log in again.") — that stays
    /// until something else overwrites it rather than auto-clearing like a
    /// toast (`set_status`). Also the right call whenever a *stale* toast
    /// timer must not linger on a status write that isn't one.
    fn set_hint(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_set_at = None;
    }

    /// Clears a status toast once `STATUS_TOAST_SECS` have passed since
    /// `set_status` showed it — the event loop calls this once per tick,
    /// before `draw`, so a stale "Reply posted." doesn't sit on the status
    /// row through everything the reader does next (issue #571). A no-op
    /// for a persistent hint (`status_set_at` is `None`) or a toast that
    /// hasn't aged out yet.
    /// Returns true when the toast expired this tick — the caller owes the
    /// screen one redraw (#674).
    fn expire_status_toast(&mut self) -> bool {
        if let Some(at) = self.status_set_at
            && at.elapsed() >= Duration::from_secs(STATUS_TOAST_SECS)
        {
            self.status.clear();
            self.status_set_at = None;
            return true;
        }
        false
    }

    /// Does the next tick owe the screen a repaint even if no event or
    /// message arrived (#674)? Only things that ANIMATE on the wall clock
    /// qualify: the write-gate countdown (its seconds and bar fill
    /// continuously) and any in-flight fetch's spinner (8 fps off the wall
    /// clock). Everything else — toasts expiring, the recovery backstop,
    /// poller replies — marks the loop dirty exactly once via its own path.
    fn needs_continuous_redraw(&self) -> bool {
        !self.client.write_gate.pending_wait().is_zero()
            || self.screens.iter().any(|s| s.is_loading())
    }

    async fn event_loop(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
        mut reader: std::sync::mpsc::Receiver<crate::event::Input>,
    ) -> u8 {
        // A dead reader used to be swallowed (`Disconnected` -> `None`), so
        // one tty error left the frame redrawing forever with no way to
        // type (#653). Readers are replaced a bounded number of times; past
        // that the client exits through the normal terminal-restoring
        // teardown with a nonzero code.
        let mut reader_deaths = 0u32;
        // The dirty flag is the idle-CPU fix (#674): the frame is rebuilt
        // only when something happened (input, message, a timer boundary)
        // or while something animates (gate countdown, loading spinners).
        // An idle session's loop costs one 50 ms `recv_timeout` and nothing
        // else. `last_drawn_gate` catches the countdown's zero crossing —
        // the tick where the gate frees must still paint "Ready".
        let mut dirty = true;
        let mut last_drawn_gate = self.client.write_gate.pending_wait();
        loop {
            if self.expire_status_toast() {
                dirty = true;
            }
            if self.expire_session_recovery_timeout() {
                dirty = true;
            }
            let gate_pending = self.client.write_gate.pending_wait();
            if dirty || gate_pending != last_drawn_gate || self.needs_continuous_redraw() {
                let _ = terminal.draw(|f| self.draw(f));
                last_drawn_gate = gate_pending;
                dirty = false;
            }
            // Drain background messages.
            let mut got_msg = false;
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg);
                got_msg = true;
                if self.should_quit {
                    break;
                }
            }
            if self.should_quit {
                return 0;
            }
            if got_msg {
                dirty = true;
            }
            // Debounced palette member lookup: the loop's idle path is the
            // only place with a clock, and typing must not fan out requests.
            self.poll_palette_member();
            // Infinite scroll (#700): one path for every way of moving down
            // a list — keys, wheel, `G`, a click.
            self.autoload_more();
            // Wait up to 50ms for input; yields periodically to allow background tasks / pollers to refresh status.
            if let Some(first) = crate::event::next(&reader, Duration::from_millis(50)) {
                let mut inputs = vec![first];
                // Non-blocking drain of any remaining pending events in the queue
                while let Some(extra) = crate::event::next(&reader, Duration::ZERO) {
                    inputs.push(extra);
                }
                for input in inputs {
                    match input {
                        crate::event::Input::Resize => {
                            let _ = terminal.clear();
                        }
                        crate::event::Input::Mouse(me) => {
                            self.handle_mouse(me);
                        }
                        crate::event::Input::Paste(text) => {
                            self.handle_paste(text);
                        }
                        crate::event::Input::Key(k) => {
                            if event::is_ctrl_c(k) {
                                if let Some(sel) = self.selection.take() {
                                    let (x0, y0, x1, y1) = sel.rect();
                                    let text = self.extract_selection_text(x0, y0, x1, y1);
                                    if !text.is_empty() {
                                        let n = text.chars().count();
                                        self.copy_text(&text);
                                        self.set_status(format!(
                                            "Copied {n} chars to clipboard (selection cleared)"
                                        ));
                                    }
                                } else {
                                    return 0;
                                }
                            } else {
                                self.handle_key(k);
                            }
                        }
                        crate::event::Input::ReaderDied => {
                            reader_deaths += 1;
                            if crate::event::reader_should_restart(reader_deaths) {
                                reader = crate::event::spawn_reader();
                                self.set_status("Terminal input restarted.");
                            } else {
                                self.set_hint("Terminal input failed — exiting.");
                                self.should_quit = true;
                            }
                        }
                    }
                    if self.should_quit {
                        // Nonzero only when input itself gave out: the
                        // terminal was restored either way, but callers and
                        // logs can tell the exits apart.
                        return if reader_deaths > crate::event::MAX_READER_RESPAWNS {
                            3
                        } else {
                            0
                        };
                    }
                }
                // Any input batch may have changed state (even a Resize's
                // `terminal.clear` needs the repaint that follows).
                dirty = true;
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        // The three zones of DESIGN.md: header band, body, key bar + status.
        let [top, body, keys, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        // Where the wheel is allowed to act (issue #549).
        self.body_rect = body;
        // One hit map per frame, in draw order: the body's own targets first,
        // then (if an overlay takes over) only the overlay's, then the header
        // and key bar, which are true whatever is on top of the body.
        self.hits.clear();

        // Crumbs are the screen stack's own names. Login is excluded: it is a
        // gate, not a place, and it owns the whole screen while it is up.
        // Home is one screen but two places: `Forums › <current forum>`. Every
        // other screen contributes exactly its own crumb; Login contributes
        // none (it is a gate, not a place).
        let mut crumbs: Vec<String> = Vec::with_capacity(self.screens.len() + 1);
        // Which screen each crumb stands for, so clicking one can pop back to
        // exactly that place (#700). Home contributes two crumbs and both
        // point at Home — "Forums" and the forum it is showing are one screen.
        let mut crumb_targets: Vec<usize> = Vec::with_capacity(self.screens.len() + 1);
        for (i, s) in self.screens.iter().enumerate() {
            match s {
                Screen::Login(_) => {}
                Screen::Home(h) => {
                    crumbs.push("Forums".to_string());
                    crumb_targets.push(i);
                    if !h.list.title.is_empty() {
                        crumbs.push(h.list.title.clone());
                        crumb_targets.push(i);
                    }
                }
                other => {
                    crumbs.push(other.crumb());
                    crumb_targets.push(i);
                }
            }
        }
        self.crumb_targets = crumb_targets;
        let me_name = self.me.as_ref().map(|u| u.username.as_str());
        let (header, badges, crumb_hits) = chrome::header_line_hits(
            &self.theme,
            &self.glyphs,
            &crumbs,
            me_name,
            self.convos_unread,
            self.alerts_unread,
            None,
            top.width,
        );
        f.render_widget(Paragraph::new(header), top);

        // Render the top screen; popups handled inside renderers.
        // The graphics policy is stamped on first: the thread view reserves
        // rows for inline images while it wraps text, so a tier change has to
        // reach it before `render` rebuilds the lines.
        let policy = self.images.policy();
        // #716: whether this thread has an unsent reply waiting. Stamped per
        // frame rather than on open, so returning from the composer with Esc
        // updates the cap immediately.
        let draft_here = match self.screens.last() {
            Some(Screen::ThreadView(v)) => self
                .drafts
                .contains_key(&common::drafts::DraftKey::ThreadReply(v.thread.thread_id)),
            _ => false,
        };
        let screen = self.screens.last_mut().expect("screen stack never empty");
        if let Screen::ThreadView(v) = screen {
            v.has_draft = draft_here;
        }
        screen.set_image_policy(policy, self.images.sizes());
        let screen_title = screen.title().to_string();
        screen.render(f, body, &self.theme, &self.glyphs, &mut self.hits);
        // Hints are read AFTER render (#656): the renderers stamp layout
        // facts (`dual`, focus rects) while they draw, and the two-pane
        // screens' hint sets branch on pane state — the bar must describe
        // the frame that was just drawn, not the one before it. Snapshotting
        // first showed a one-frame-stale bar whenever a layout change
        // crossed a hint-relevant boundary (the 110-column dual crossover).
        let screen_hints = screen.hints();
        let keys_group = screen.keys_group();

        // Inline images, painted over the rects the screen just reserved.
        // Suppressed while any overlay is up: `overlay::dim_body` re-styles
        // every body cell (kitty's placeholders encode the image id in the
        // cell colours) and the overlay's own `Clear` erases the rect for that
        // frame. Closing the overlay makes the cells differ again, so the next
        // frame re-emits the image without any extra bookkeeping.
        let overlay_open = self.palette.is_some()
            || self.show_help
            || self.prefix.armed()
            || capture_active(self);
        if !overlay_open {
            let reqs: Vec<crate::images::Request> = self
                .screens
                .last()
                .map(|s| s.image_requests().to_vec())
                .unwrap_or_default();
            for pending in self.images.paint(f, &reqs) {
                self.spawn_image_load(pending);
            }
        }

        // Overlays cover the body only: the header band, the key bar and the
        // status row always keep saying what is true. An overlay that owns the
        // keyboard also owns the key bar and the status text.
        let mut bar: Option<chrome::Hints> = None;
        let mut status_left = self.status.clone();
        if self.palette.is_some() || self.show_help {
            overlay::dim_body(f, body, &self.theme);
        }
        // An APP overlay owns the body's pointer as completely as it owns
        // the keyboard: the body's hits go with the frame it is drawn over,
        // so nothing behind the glass can be clicked through it. (The thread
        // view's link popup is the screen's own overlay — it registers its
        // rows during `render`, over the body's, and must keep them.)
        if self.palette.is_some() || self.show_help || self.prefix.armed() {
            self.hits.clear();
        }
        if let Some(p) = &self.palette {
            p.render(f, body, &self.theme, &self.glyphs, &mut self.hits);
            bar = Some(Palette::hints(&self.glyphs));
            status_left = Palette::status().to_string();
        }
        if self.prefix.armed() {
            overlay::render_which_key(f, body, &self.theme, &self.glyphs, &mut self.hits);
        }
        if self.show_help {
            overlay::render_keys_card(
                f,
                body,
                &self.theme,
                &self.glyphs,
                keys_group,
                &screen_hints,
                &mut self.hits,
            );
            bar = Some(overlay::keys_card_hints());
            status_left = overlay::keys_card_status().to_string();
        }

        let (key_bar, caps) = chrome::key_bar_hits(
            &self.theme,
            bar.as_ref().unwrap_or(&screen_hints),
            keys.width,
        );
        f.render_widget(Paragraph::new(key_bar), keys);
        // Header and key bar last: they are outside the body, so they stay
        // clickable under every overlay — and clicking a cap while one is up
        // presses that key through the same routing the keyboard uses.
        for b in badges {
            self.hits.push(
                ratatui::layout::Rect::new(b.x, top.y, b.width, 1),
                Hit::Badge(b.tab),
            );
        }
        // Crumbs after the badges: the badges sit on the right of the same
        // row and must win where they overlap nothing, but last-registered
        // wins and the two never share cells anyway.
        for c in crumb_hits {
            self.hits.push(
                ratatui::layout::Rect::new(c.x, top.y, c.width, 1),
                Hit::Crumb(c.index),
            );
        }
        for c in caps {
            self.hits.push(
                ratatui::layout::Rect::new(c.x, keys.y, c.width, 1),
                Hit::Key(c.key),
            );
        }

        let new_title = if self.alerts_unread > 0 || self.convos_unread > 0 {
            format!(
                "wftui · {screen_title} [{} DM · {} alerts]",
                self.convos_unread, self.alerts_unread
            )
        } else {
            format!("wftui · {screen_title}")
        };
        if new_title != self.last_title {
            emit_raw(&common::osc::set_title(&new_title));
            self.last_title = new_title;
        }

        // The write gate is the one thing on the status row that must always
        // be readable: it is the difference between "wait" and "we 429ed".
        let pending = self.client.write_gate.pending_wait();
        let gate = if pending.is_zero() {
            GateState::Ready
        } else {
            GateState::Waiting {
                left: pending,
                total: Duration::from_millis(common::config::WRITE_COOLDOWN_MS),
            }
        };
        f.render_widget(
            Paragraph::new(chrome::status_line(
                &self.theme,
                &self.glyphs,
                &status_left,
                Style::new().fg(self.theme.dim),
                gate,
                status.width,
            )),
            status,
        );

        self.paint_selection(f);
        self.capture_screen(f);
    }

    /// Snapshot the finished frame's symbols so mouse-selection can extract
    /// the visible text later (between draws the terminal buffer is the
    /// blank next-frame buffer, not what is on screen).
    fn capture_screen(&mut self, f: &mut Frame) {
        let area = f.area();
        // Reuse the previous frame's buffers (#675): at the old
        // unconditional 20 fps cadence this mirror was ~1 MB/s of
        // allocation churn even when the frame was identical to the last
        // one. `mem::take` keeps the Strings'/Vecs' capacities alive.
        let mut rows = std::mem::take(&mut self.screen_rows);
        let mut cols = std::mem::take(&mut self.screen_cols);
        rows.clear();
        cols.clear();
        for y in 0..area.height {
            let mut row = String::with_capacity(area.width as usize);
            let mut offsets = Vec::with_capacity(area.width as usize + 1);
            // Columns still covered by the double-width character to their
            // left. `set_stringn` *resets* those cells (their symbol reads
            // back as " "), so mirroring them verbatim put a phantom space
            // after every ideograph in the clipboard and broke word-select
            // on CJK (issue #525). They contribute no bytes; their offset
            // entry stays where the wide character ended, which keeps the
            // column-to-byte map — and every selection through it — exact.
            let mut continuation = 0usize;
            for x in 0..area.width {
                offsets.push(row.len());
                let cell = &f.buffer_mut()[(x, y)];
                if continuation > 0 {
                    continuation -= 1;
                    continue;
                }
                // Image cells are not text: `ratatui-image` puts a whole
                // escape payload in one anchor cell's symbol and marks the
                // rest of the rect `Skip`. Copying either into the selection
                // mirror would put raw escape bytes on the user's clipboard
                // and desync every byte offset on the row.
                if is_image_cell(cell) {
                    row.push(' ');
                } else {
                    let symbol = cell.symbol();
                    row.push_str(symbol);
                    continuation = crate::chrome::cell_width(symbol).saturating_sub(1);
                }
            }
            offsets.push(row.len());
            rows.push(row);
            cols.push(offsets);
        }
        self.screen_rows = rows;
        self.screen_cols = cols;
    }

    // ---- key routing ----

    /// The forum "here" means right now: the open thread's forum, else the
    /// thread list's, else the forum under the tree cursor. `None` when the
    /// answer would be "latest posts", which is not a forum you can post to.
    fn current_forum(&self) -> Option<(u32, String)> {
        for s in self.screens.iter().rev() {
            match s {
                Screen::ThreadView(v) if v.thread.node_id > 0 => {
                    let title = if v.forum_title.is_empty() {
                        "this forum".to_string()
                    } else {
                        v.forum_title.clone()
                    };
                    return Some((v.thread.node_id, title));
                }
                Screen::ThreadList(l) if l.node_id > 0 => {
                    return Some((l.node_id, l.title.clone()));
                }
                Screen::Home(h) if h.list.node_id > 0 => {
                    return Some((h.list.node_id, h.list.title.clone()));
                }
                _ => {}
            }
        }
        self.tree()
            .and_then(|t| t.nodes.get(t.sel))
            .filter(|n| n.node_type == "Forum")
            .map(|n| (n.node_id, n.title.clone()))
    }

    /// Build the palette: actions first (so the unfiltered list is a list of
    /// verbs), then every forum the cached tree knows. Members arrive later,
    /// from `poll_palette_member`.
    fn open_palette(&mut self) {
        use overlay::{Item, Target};
        let mut items = Vec::new();
        if let Some((node_id, title)) = self.current_forum() {
            items.push(Item::action(
                format!("New thread in {title}"),
                "N",
                Target::NewThread(node_id),
            ));
            items.push(Item::action(
                "Mark forum read".to_string(),
                "m",
                Target::MarkForumRead(node_id),
            ));
        }
        items.push(Item::action(
            "Latest posts".to_string(),
            "L",
            Target::Latest,
        ));
        for (id, title, key) in [
            (screens::NEWS_NODE, "Windows News", "1"),
            (screens::SECURITY_NODE, "Security Alerts", "2"),
            (screens::TUTORIALS_NODE, "Windows Tutorials", "3"),
        ] {
            items.push(Item::action(
                title.to_string(),
                key,
                Target::QuickNode(id, title.to_string()),
            ));
        }
        items.push(Item::action("Inbox".to_string(), "c", Target::Inbox));
        items.push(Item::action("Alerts".to_string(), "a", Target::Alerts));
        items.push(Item::action(
            "Media Gallery".to_string(),
            "gm",
            Target::MediaGallery,
        ));
        items.push(Item::action(
            "Resources".to_string(),
            "gr",
            Target::Resources,
        ));
        // Named with a count, because the whole point is that a draft you
        // forgot about is otherwise invisible (#716).
        if !self.drafts.is_empty() {
            items.push(Item::action(
                format!("Drafts ({})", self.drafts.len()),
                "gd",
                Target::Drafts,
            ));
        }
        items.push(Item::action("Search".to_string(), "/", Target::Search));
        items.push(Item::action("Sign out".to_string(), "^L", Target::SignOut));
        items.push(Item::action("Quit".to_string(), "q", Target::Quit));
        if let Some(tree) = self.tree() {
            for node in &tree.nodes {
                // Categories are not destinations (`/forums/{id}` 404s for
                // them); `open_node_action` resolves the rest.
                if node.node_type != "Category" {
                    items.push(Item::forum(node.node_id, node.title.clone()));
                }
            }
        }
        self.palette = Some(Palette::new(items));
    }

    /// Ask the site about a member the palette query might name. Exact-name
    /// lookup (`/users/find-name`) is the only member API `WfApi` exposes, so
    /// the palette resolves a typed handle rather than offering suggestions.
    /// `Palette::member_query_due` debounces and de-duplicates.
    fn poll_palette_member(&mut self) {
        let Some(query) = self.palette.as_mut().and_then(|p| p.member_query_due()) else {
            return;
        };
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let user = api.find_user(&query).await.unwrap_or(None);
            tx.send(Msg::PaletteMember { query, user }).ok();
        });
    }

    /// Showing the alerts list marks them *viewed* — what XF's own web UI
    /// does when you open the alerts page (#694). Viewed is not read: the
    /// counter clears, unactioned alerts stay highlighted, and Enter/`m` on
    /// a row still marks that one read.
    ///
    /// Only sent when there is a count to clear, so revisiting the tab does
    /// not spend a request each time.
    fn mark_alerts_viewed(&mut self) {
        if self.alerts_unread == 0 {
            return;
        }
        self.alerts_unread = 0;
        let api = self.api.clone();
        tokio::spawn(async move {
            // Nothing waits on this: the badge is already cleared locally,
            // and a failure means the next poll puts the count back.
            let _ = api.mark_alerts_viewed().await;
        });
    }

    /// The one way a screen leaves the stack. Whatever it owes on the way
    /// out is settled here (#694), so no exit path can forget it.
    fn pop_screen(&mut self) -> bool {
        if self.screens.len() <= 1 {
            return false;
        }
        let gone = self.screens.pop();
        if let Some(Screen::ThreadView(view)) = &gone {
            self.report_read(view);
        }
        // Esc used to destroy an unsent post outright (#715). Keep it instead
        // — Esc still closes instantly, so the ordinary empty-composer case
        // pays nothing, and the words come back next time this composer
        // opens. The successful-send paths do not come through here: they
        // `screens.remove(idx)` and clear the draft explicitly.
        if let Some(Screen::Compose(c)) = &gone {
            self.stash_draft(c);
        }
        true
    }

    /// Keep (or clear) the draft slot this composer owns.
    ///
    /// An empty composer *removes* any stored draft rather than storing a
    /// blank one, so opening a reply, thinking better of it and pressing Esc
    /// does not leave an empty draft to be resumed later.
    fn stash_draft(&mut self, c: &screens::ComposeState) {
        let Some(key) = c.target.as_ref().map(|t| t.draft_key()) else {
            return;
        };
        let draft = common::drafts::Draft {
            title: c.title.clone(),
            body: c.body.clone(),
            attachment_key: c.attachment_key.clone(),
            saved_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            label: self.draft_label(c.target.as_ref()),
            // Whatever the website's copy carried, this one is now ours and
            // its files (if any) are reachable through `attachment_key`.
            remote_attachments: false,
        };
        if draft.is_empty() {
            self.discard_draft(key);
            return;
        }
        let too_big = draft.too_big_to_persist();
        self.push_draft(key, &draft);
        self.drafts.insert(key, draft);
        self.persist_drafts();
        if too_big {
            // Honest about the limit rather than promising a recovery that a
            // restart will not deliver.
            self.set_status("Draft kept for this session — too large to save to disk.");
        } else {
            self.set_status("Draft saved. Open this composer again to resume it.");
        }
    }

    /// Close a composer whose write just succeeded and forget its draft: the
    /// post is on the site now, so there is nothing left to recover (#715).
    ///
    /// Deliberately not `pop_screen`, which *saves* the draft — and which
    /// could not be used here anyway, since a composer may be buried under a
    /// screen the user opened while the write was in flight.
    fn close_sent_composer(&mut self, idx: usize) {
        let key = match self.screens.get(idx) {
            Some(Screen::Compose(c)) => c.target.as_ref().map(|t| t.draft_key()),
            _ => None,
        };
        if let Some(key) = key {
            // `discard_draft` also clears it on the website, which is the
            // whole reason this path exists: the REST API does not delete
            // drafts the way XF's web post controller does (#716).
            self.discard_draft(key);
        }
        self.screens.remove(idx);
    }

    /// Forget a draft, in memory and on disk. Called when the composer is
    /// left empty, when the user discards it, and when the write it belongs
    /// to actually succeeds.
    fn discard_draft(&mut self, key: common::drafts::DraftKey) {
        if self.drafts.remove(&key).is_some() {
            self.persist_drafts();
        }
        // Unconditionally, not only when a local copy existed: the draft may
        // have been written in the browser and never resumed here, and the
        // point of a discard is that it is gone from both.
        self.drop_remote_draft(key);
    }

    /// Forget every draft, in memory and on disk. For the two moments where
    /// the drafts stop belonging to whoever is now at the keyboard: an
    /// explicit sign-out, and the token store turning out to hold a
    /// different account.
    ///
    /// Deliberately local-only — it does **not** clear the website's copies
    /// (#716). Those live behind the signing-out user's own account, where
    /// they are already private and where they are still wanted; the problem
    /// this solves is the next person at this keyboard, not the drafts
    /// themselves. Signing out of a terminal must not destroy the reply
    /// somebody has half-written in their browser.
    fn clear_all_drafts(&mut self) {
        if !self.drafts.is_empty() {
            self.drafts.clear();
        }
        if let Err(e) = self.draft_store.erase() {
            tracing::warn!("could not clear drafts: {e}");
        }
    }

    /// The drafts list's rows, newest first.
    fn draft_rows(&self) -> Vec<screens::DraftRow> {
        let mut rows: Vec<screens::DraftRow> = self
            .drafts
            .iter()
            .map(|(key, d)| screens::DraftRow {
                key: *key,
                label: d.label.clone(),
                // First non-empty line, so a row says what was written rather
                // than just that something was.
                preview: d
                    .body
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .to_string(),
                saved_at: d.saved_at,
                shared: key.xf_key().is_some(),
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.saved_at));
        rows
    }

    fn open_drafts(&mut self) {
        let rows = self.draft_rows();
        self.push_screen(Screen::Drafts(screens::DraftsState { rows, sel: 0 }));
    }

    /// Reopen the composer a listed draft belongs to.
    ///
    /// The composer is built from the key plus the draft's own label — we do
    /// not have the thread or conversation here, and do not need it:
    /// `push_screen` fills the body from the draft, and the write carries the
    /// id, not the object.
    fn resume_draft(&mut self, key: common::drafts::DraftKey) {
        use common::drafts::DraftKey;
        let label = self.drafts.get(&key).map(|d| d.label.clone()).unwrap_or_default();
        match key {
            DraftKey::ThreadReply(thread_id) => {
                self.push_screen(Screen::Compose(screens::ComposeState {
                    target: Some(ComposeTarget::ThreadReply {
                        thread_id,
                        thread_title: label,
                    }),
                    author: self.me_name(),
                    ..Default::default()
                }));
            }
            DraftKey::NewThread(node_id) => {
                self.push_screen(Screen::Compose(screens::ComposeState {
                    target: Some(ComposeTarget::NewThread { node_id }),
                    title_field: true,
                    author: self.me_name(),
                    ..Default::default()
                }));
            }
            DraftKey::ConversationReply(conversation_id) => {
                self.push_screen(Screen::Compose(screens::ComposeState {
                    target: Some(ComposeTarget::ConversationReply {
                        conversation_id,
                        conversation_title: label,
                        participants: String::new(),
                    }),
                    author: self.me_name(),
                    ..Default::default()
                }));
            }
            DraftKey::EditPost(post_id) => {
                // An edit draft carries no thread id of its own. The label
                // still names the thread, and the write is by post id.
                self.push_screen(Screen::Compose(screens::ComposeState {
                    target: Some(ComposeTarget::EditPost {
                        post_id,
                        thread_id: 0,
                        thread_title: label,
                    }),
                    author: self.me_name(),
                    ..Default::default()
                }));
            }
        }
    }

    /// Name a draft's target in words, for the drafts list. "thread-51465" is
    /// not an answer to "what was I writing?", and the key is all the list
    /// would otherwise have.
    fn draft_label(&self, target: Option<&ComposeTarget>) -> String {
        match target {
            Some(ComposeTarget::ThreadReply { thread_title, .. }) => thread_title.clone(),
            Some(ComposeTarget::EditPost { thread_title, .. }) => {
                format!("Edit in {thread_title}")
            }
            Some(ComposeTarget::NewThread { node_id }) => {
                // Name the forum when the tree is loaded; the id alone is no
                // better than the key.
                let forum = self
                    .screens
                    .iter()
                    .find_map(|s| match s {
                        Screen::Home(h) => Some(&h.tree),
                        Screen::ForumTree(t) => Some(t),
                        _ => None,
                    })
                    .and_then(|t| t.nodes.iter().find(|n| n.node_id == *node_id))
                    .map(|n| n.title.clone());
                match forum {
                    Some(title) => format!("New thread in {title}"),
                    None => "New thread".to_string(),
                }
            }
            Some(ComposeTarget::ConversationReply { conversation_title, .. }) => {
                conversation_title.clone()
            }
            None => String::new(),
        }
    }

    /// Mirror a draft to the website, best effort (#716).
    ///
    /// Fire-and-forget on purpose: the local save has already succeeded by
    /// the time this runs, so a relay failure costs nothing the user can act
    /// on — and blocking Esc on a network round trip to save something we
    /// already saved would be the wrong trade. `drafts.json` stays the store
    /// the composer reads; this only keeps the website in step.
    fn push_draft(&self, key: common::drafts::DraftKey, draft: &common::drafts::Draft) {
        // `None` means XF has no draft for this kind — an edit. Local only.
        let Some(xf_key) = key.xf_key() else {
            return;
        };
        let api = self.api.clone();
        let (message, title) = (draft.body.clone(), draft.title.clone());
        let attachment_key = draft.attachment_key.clone();
        tokio::spawn(async move {
            if let Err(e) = api
                .save_draft(&xf_key, &message, &title, attachment_key.as_deref())
                .await
            {
                tracing::warn!("could not sync draft {xf_key} to the site: {e}");
            }
        });
    }

    /// Forget a draft on the website too.
    ///
    /// Load-bearing on the send path: XF's *web* controller deletes the draft
    /// when a post succeeds, but the REST API never touches drafts at all, so
    /// without this every post made from the TUI would leave a stale draft
    /// waiting in the browser's editor.
    fn drop_remote_draft(&self, key: common::drafts::DraftKey) {
        let Some(xf_key) = key.xf_key() else {
            return;
        };
        let api = self.api.clone();
        tokio::spawn(async move {
            if let Err(e) = api.delete_draft(&xf_key).await {
                tracing::warn!("could not clear draft {xf_key} on the site: {e}");
            }
        });
    }

    /// Merge the website's drafts into the local store, newest wins (#716).
    ///
    /// Runs once when a session goes live rather than when a composer opens,
    /// so opening a composer stays instant — it reads a map that is already
    /// in memory.
    fn merge_remote_drafts(&mut self, remote: Vec<common::models::RemoteDraft>) {
        let mut changed = false;
        for r in remote {
            // A kind this build does not know is skipped, not guessed at.
            let Some(key) = common::drafts::DraftKey::from_xf_key(&r.key) else {
                continue;
            };
            if let Some(local) = self.drafts.get(&key)
                && local.saved_at >= r.last_update
            {
                // Ours is the same age or newer: keep it. Equal timestamps
                // mean this is the copy we pushed.
                continue;
            }
            self.drafts.insert(
                key,
                common::drafts::Draft {
                    title: r.title,
                    body: r.message,
                    // The website's copy cannot hand us a usable attachment
                    // key, but a key minted in *this* session still works, so
                    // keep one if we have it rather than dropping it.
                    attachment_key: self.drafts.get(&key).and_then(|d| d.attachment_key.clone()),
                    saved_at: r.last_update,
                    remote_attachments: r.has_attachments,
                    // The relay sends no name for the target, so keep any
                    // label we already had rather than blanking the list row.
                    label: self.drafts.get(&key).map(|d| d.label.clone()).unwrap_or_default(),
                },
            );
            changed = true;
        }
        if changed {
            self.persist_drafts();
        }
    }

    /// Ask the website for this account's drafts. Errors are logged and
    /// dropped: nothing about the composer depends on this call succeeding.
    fn sync_drafts(&self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match api.list_drafts().await {
                Ok(drafts) => {
                    tx.send(Msg::DraftsLoaded(drafts)).ok();
                }
                Err(e) => tracing::warn!("could not read drafts from the site: {e}"),
            }
        });
    }

    /// Write the store, best effort. A draft is a convenience: failing to
    /// save one is worth a line in the log, never an error that interrupts
    /// the user mid-post.
    fn persist_drafts(&self) {
        if let Err(e) = self.draft_store.save(&self.drafts) {
            tracing::warn!("could not save drafts: {e}");
        }
    }

    /// Navigate to a clicked breadcrumb (#700): pop everything the crumb is
    /// an ancestor of. The brand (`usize::MAX`) is the root — Home.
    ///
    /// Popping rather than pushing is what makes this navigation and not a
    /// new place: the crumb names a screen that is already on the stack, and
    /// clicking it returns to that screen exactly as Esc would, so its
    /// scroll position, selection and loaded pages survive.
    fn go_to_crumb(&mut self, index: usize) {
        // Never through the sign-in gate: it owns the screen while it is up.
        if matches!(self.screens.last(), Some(Screen::Login(_))) {
            return;
        }
        let target = if index == usize::MAX {
            self.screens
                .iter()
                .position(|s| matches!(s, Screen::Home(_)))
                .unwrap_or(0)
        } else {
            match self.crumb_targets.get(index) {
                Some(&i) => i,
                None => return,
            }
        };
        if target + 1 >= self.screens.len() {
            // Already there — a click on the last crumb is a no-op, not a
            // pop of the screen the reader is looking at.
            return;
        }
        while self.screens.len() > target + 1 {
            // Through `pop_screen`, so a thread left behind by a crumb click
            // still reports what was read in it (#694).
            self.pop_screen();
        }
        // Any overlay was raised over the screen we just left.
        self.palette = None;
        self.show_help = false;
        self.prefix = Prefix::default();
    }

    fn run_palette_target(&mut self, target: overlay::Target) {
        use overlay::Target as T;
        match target {
            T::Forum(node_id, title) => self.execute_action(Action::OpenThreadList(node_id, title)),
            T::QuickNode(node_id, title) => self.open_quick_node(node_id, &title),
            T::Latest => self.execute_action(Action::OpenLatestThreads),
            T::NewThread(node_id) => self.execute_action(Action::StartNewThread(node_id)),
            T::MarkForumRead(node_id) => self.execute_action(Action::MarkForumRead(node_id)),
            T::Inbox => self.open_inbox(screens::InboxTab::Conversations),
            T::Alerts => self.open_inbox(screens::InboxTab::Alerts),
            T::MediaGallery => self.execute_action(Action::OpenMediaGallery),
            T::Resources => self.execute_action(Action::OpenResources),
            T::Drafts => self.execute_action(Action::OpenDrafts),
            T::Search => self.push_screen(screens::search_state()),
            T::SignOut => self.logout(),
            T::Quit => self.should_quit = true,
            T::Member(user_id, name) => self.open_profile(user_id, &name),
        }
    }

    /// One of the `1`/`2`/`3` destinations, resolved through the loaded tree so
    /// a category opens its first forum and a link-forum opens its URL.
    fn open_quick_node(&mut self, node_id: u32, title: &str) {
        let nodes = self
            .tree()
            .map(|t| t.nodes.clone())
            .unwrap_or_default();
        let action = screens::open_node_action(&nodes, node_id, title);
        self.execute_action(action);
    }

    /// Run a `g <key>` chord.
    fn go(&mut self, target: GoTarget) {
        match target {
            GoTarget::News => self.open_quick_node(screens::NEWS_NODE, "Windows News"),
            GoTarget::Security => self.open_quick_node(screens::SECURITY_NODE, "Security Alerts"),
            GoTarget::Tutorials => {
                self.open_quick_node(screens::TUTORIALS_NODE, "Windows Tutorials")
            }
            GoTarget::Latest => self.execute_action(Action::OpenLatestThreads),
            GoTarget::Media => self.execute_action(Action::OpenMediaGallery),
            GoTarget::Resources => self.execute_action(Action::OpenResources),
            GoTarget::Drafts => self.execute_action(Action::OpenDrafts),
            GoTarget::Inbox => self.open_inbox(screens::InboxTab::Conversations),
            GoTarget::Alerts => self.open_inbox(screens::InboxTab::Alerts),
            GoTarget::Home => {
                self.screens.truncate(1);
                self.status.clear();
            }
            GoTarget::Profile => {
                if let Some(me) = &self.me {
                    let (id, name) = (me.user_id, me.username.clone());
                    self.open_profile(id, &name);
                }
            }
            GoTarget::Top => {
                if let Some(screen) = self.screens.last_mut() {
                    screen.goto_top();
                }
            }
        }
    }

    pub fn push_screen(&mut self, mut screen: Screen) {
        // Restore here rather than at each opener (#715): all four composers
        // — reply, quote-reply, edit, new thread, conversation reply — reach
        // the stack through this one call, so a fifth cannot forget to.
        if let Screen::Compose(c) = &mut screen
            && let Some(key) = c.target.as_ref().map(|t| t.draft_key())
            && let Some(draft) = self.drafts.get(&key)
        {
            // What the composer would have shown without a draft, so `^X`
            // can put it back — for an edit that is the post's current text.
            c.seed_title = c.title.clone();
            c.seed_body = c.body.clone();
            c.title = draft.title.clone();
            // A seeded body is material the reader just asked for — `Q`'s
            // quote of a specific post — so the draft must not swallow it.
            // The draft goes UNDER the quote, where a reply goes. An edit is
            // the exception: its seed is the post's current text and the
            // draft is a newer version of exactly that, so appending would
            // duplicate the post instead of editing it.
            let is_edit = matches!(c.target, Some(ComposeTarget::EditPost { .. }));
            c.body = match (c.body.trim().is_empty(), is_edit) {
                (false, false) => format!("{}\n\n{}", c.body.trim_end(), draft.body),
                _ => draft.body.clone(),
            };
            // The files attached before Esc belong to this draft's key, and
            // without it they are attached to nothing (#709).
            c.attachment_key = draft.attachment_key.clone();
            c.title_cursor = c.title.chars().count();
            c.body_cursor = c.body.chars().count();
            c.resumed = true;
            if draft.remote_attachments {
                // Say it rather than let the files quietly not be there: the
                // website's copy carries attachments, and XF's temp hash
                // cannot be turned back into a key this client could spend
                // (#716).
                self.set_hint(
                    "Resumed a draft from the website. Its attachments stay there —                      ^X discards the draft.",
                );
            } else {
                self.set_hint("Resumed your saved draft — ^X discards it.");
            }
        }
        self.screens.push(screen);
    }

    // ---- screen openers / actions ----

    /// Open the Inbox on `tab`, or — if it is already the top screen — just
    /// switch to that tab in place (so `c`/`a` from anywhere never stacks a
    /// second Inbox on top of the first).
    fn open_inbox(&mut self, tab: screens::InboxTab) {
        if tab == screens::InboxTab::Alerts {
            self.mark_alerts_viewed();
        }
        if let Some(Screen::Inbox(inbox)) = self.screens.last_mut() {
            inbox.tab = tab;
            return;
        }
        self.push_screen(Screen::Inbox(screens::InboxState {
            tab,
            convos: screens::ConversationsState {
                loading: true,
                ..Default::default()
            },
            alerts: screens::AlertsState {
                loading: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        self.load_conversations(1);
        self.load_alerts();
    }

    /// The topmost Inbox screen, if any — the router other Inbox-related
    /// message handlers use to find where conversations/alerts data lives.
    fn inbox_mut(&mut self) -> Option<&mut screens::InboxState> {
        self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::Inbox(inbox) => Some(inbox),
            _ => None,
        })
    }

    /// The `ConversationViewState` a `ConversationLoaded`/reply-sent message
    /// belongs to: the topmost standalone `ConversationView`, or the Inbox's
    /// inline view pane — whichever currently holds this conversation.
    fn conversation_view_mut(&mut self, id: u32) -> Option<&mut screens::ConversationViewState> {
        self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::ConversationView(view) if view.conversation.conversation_id == id => {
                Some(view)
            }
            Screen::Inbox(inbox) => inbox
                .view
                .as_mut()
                .filter(|v| v.conversation.conversation_id == id),
            _ => None,
        })
    }

    /// Open a conversation: into the Inbox's view pane when it is the top
    /// screen and dual, otherwise as a pushed `ConversationView` exactly as
    /// before.
    pub fn open_conversation(&mut self, conv: Conversation) {
        let cid = conv.conversation_id;
        // Re-opening the conversation already loading in the view pane must
        // not fire a second fetch over the first — ConversationLoaded
        // matches by id only, so the last reply to land would win (#667).
        if let Some(Screen::Inbox(inbox)) = self.screens.last_mut()
            && inbox.dual
        {
            let already_loading = inbox
                .view
                .as_ref()
                .is_some_and(|v| v.conversation.conversation_id == cid && v.loading);
            if !already_loading {
                inbox.view = Some(screens::ConversationViewState {
                    conversation: conv,
                    page: 1,
                    loading: true,
                    ..Default::default()
                });
                inbox.focus = screens::InboxPane::View;
                self.load_conversation(cid, 1, true);
            }
            return;
        }
        if let Some(Screen::ConversationView(v)) = self.screens.last()
            && v.conversation.conversation_id == cid
            && v.loading
        {
            return;
        }
        self.push_screen(Screen::ConversationView(screens::ConversationViewState {
            conversation: conv,
            page: 1,
            loading: true,
            ..Default::default()
        }));
        self.load_conversation(cid, 1, true);
    }

    /// The node tree, wherever it is: the topmost `ForumTree`, else Home's.
    fn tree(&self) -> Option<&screens::ForumTreeState> {
        self.screens.iter().rev().find_map(|s| match s {
            Screen::ForumTree(tree) => Some(tree),
            Screen::Home(h) => Some(&h.tree),
            _ => None,
        })
    }

    fn tree_mut(&mut self) -> Option<&mut screens::ForumTreeState> {
        self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::ForumTree(tree) => Some(tree),
            Screen::Home(h) => Some(&mut h.tree),
            _ => None,
        })
    }

    /// The live thread list, whatever it is showing: the topmost `ThreadList`,
    /// or Home's list pane — whichever is higher on the stack, so a pushed
    /// list is never overwritten by the Home pane underneath it. For anything
    /// answering a specific request, use `list_mut_for`.
    fn list_mut(&mut self) -> Option<&mut screens::ThreadListState> {
        self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::ThreadList(list) => Some(list),
            Screen::Home(h) => Some(&mut h.list),
            _ => None,
        })
    }

    /// The same list, but only if it is showing `node_id`.
    fn list_mut_for(&mut self, node_id: u32) -> Option<&mut screens::ThreadListState> {
        list_showing(&mut self.screens, node_id)
    }

    /// Fill Home's list pane with the latest posts as soon as we are signed
    /// in, so the two-pane Home has something to read on arrival instead of an
    /// empty right half. Only ever runs against a pane nobody has loaded yet.
    fn prime_home_list(&mut self) {
        let should = matches!(
            self.screens.first(),
            Some(Screen::Home(h))
                if h.list.threads.is_empty() && !h.list.loading && h.list.node_id == 0
        );
        if !should {
            return;
        }
        if let Some(Screen::Home(h)) = self.screens.first_mut() {
            h.list.title = "Latest posts".to_string();
            h.list.page = 1;
            h.list.loading = true;
        }
        self.load_forum(0, 1);
    }

    /// Open a forum: into Home's right-hand pane when the two-pane layout is
    /// up, otherwise as a pushed screen exactly as before.
    fn open_list(&mut self, node_id: u32, title: String) {
        if let Some(Screen::Home(h)) = self.screens.last_mut()
            && h.dual
        {
            h.list.node_id = node_id;
            h.list.title = title;
            h.list.page = 1;
            h.list.last_page = 1;
            h.list.total = 0;
            h.list.threads.clear();
            h.list.sticky_count = 0;
            h.list.sel = 0;
            h.list.scroll_reset();
            h.list.error = None;
            h.list.loading = true;
            h.focus = screens::Pane::List;
            self.load_forum(node_id, 1);
            return;
        }
        self.push_screen(Screen::ThreadList(screens::ThreadListState {
            node_id,
            title,
            page: 1,
            pages_loaded: 1,
            fill_budget: FILL_PAGE_BUDGET,
            loading: true,
            ..Default::default()
        }));
        self.load_forum(node_id, 1);
    }

    pub fn open_thread(&mut self, thread: &Thread) {
        // A bounced Enter used to stack a second identical view on top —
        // only the top one ever loads, so popping back revealed a twin
        // stuck on "Loading…" forever (#667).
        if let Some(Screen::ThreadView(v)) = self.screens.last()
            && v.thread.thread_id == thread.thread_id
            && v.loading
        {
            return;
        }
        // `Thread` carries only `node_id`; the card stack wants the forum's
        // name, and the loaded tree is the only place that has it.
        let forum_title = self
            .tree()
            .and_then(|t| {
                t.nodes
                    .iter()
                    .find(|n| n.node_id == thread.node_id)
                    .map(|n| n.title.clone())
            })
            .unwrap_or_default();
        self.push_screen(Screen::ThreadView(screens::ThreadViewState {
            thread: thread.clone(),
            forum_title,
            page: 1,
            loading: true,
            ..Default::default()
        }));
        self.load_thread(thread.thread_id, 1);
    }

    pub fn open_profile(&mut self, user_id: u32, fallback_name: &str) {
        self.profile_generation += 1;
        let generation = self.profile_generation;
        self.push_screen(Screen::Profile(screens::ProfileState {
            title: fallback_name.to_string(),
            loading: true,
            generation,
            ..Default::default()
        }));
        let api = self.api.clone();
        let tx = self.tx.clone();
        let fallback_name = fallback_name.to_string();
        tokio::spawn(async move {
            // Search hits (and any other caller that only has a username)
            // pass user_id 0; resolve it to a real id via find-name first,
            // since `GET /users/0` always 404s (issue #521).
            let found = if user_id == 0 {
                api.find_user(&fallback_name).await.ok().flatten().map(|u| u.user_id)
            } else {
                None
            };
            let id = resolve_profile_id(user_id, found);
            let result = api.user(id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ProfileLoaded { generation, result }).ok();
        });
    }

    /// The signed-in member's name, for the composer's `as <user>` segment.
    fn me_name(&self) -> String {
        self.me
            .as_ref()
            .map(|u| u.username.clone())
            .unwrap_or_default()
    }

    pub fn reply_to_thread(&mut self, thread: &Thread) {
        self.reply_to_thread_with(thread, String::new());
    }

    /// Reply with the draft already seeded — `Q` seeds it with the selected
    /// post's quote block (#707).
    pub fn reply_to_thread_with(&mut self, thread: &Thread, body: String) {
        // `reply_count` counts replies, so the thread holds `reply_count + 1`
        // posts and this draft becomes the next one after that.
        let reply_number = u32::try_from(thread.reply_count.saturating_add(2)).ok();
        let cursor = body.chars().count();
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: thread.thread_id,
                thread_title: thread.title.clone(),
            }),
            author: self.me_name(),
            reply_number,
            body,
            // Below the quote, where the reply goes.
            body_cursor: cursor,
            ..Default::default()
        }));
    }

    pub fn new_thread(&mut self, node_id: u32) {
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::NewThread { node_id }),
            title_field: true,
            author: self.me_name(),
            ..Default::default()
        }));
    }

    pub fn reply_to_conversation(&mut self, conv: &Conversation) {
        let participants = conv.participants_display();
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ConversationReply {
                conversation_id: conv.conversation_id,
                conversation_title: conv.title.clone(),
                participants,
            }),
            author: self.me_name(),
            ..Default::default()
        }));
    }

}

/// The scheme prefix of `url` (the part before its first `:`), lower-cased,
/// when it is a syntactically valid RFC 3986 scheme
/// (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`) — never true of a
/// scheme-less relative path, which never contains a colon before its first
/// `/`. A colon that appears only after a `/` (e.g. a query string) does not
/// count as a scheme.
fn url_scheme(url: &str) -> Option<String> {
    let colon = url.find(':')?;
    if let Some(slash) = url.find('/')
        && slash < colon
    {
        return None;
    }
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// Decide what `Action::OpenUrl`/`o` should actually hand the system opener.
/// `Ok` is the absolute URL to open; `Err` carries the refused scheme for the
/// status message. Only `http`/`https`/`mailto`/`ftp` (matched
/// ASCII-case-insensitively, so `HTTPS://…` and `MAILTO:…` both work) pass
/// through unchanged; every other scheme (`javascript`, `data`, `file`,
/// `vbscript`, or anything unrecognized) is refused outright rather than
/// silently mis-handled. A URL with no scheme at all is a site-relative path
/// and gets `base` prefixed, exactly as before (issue #553).
fn resolve_open_url(url: &str, base: &str) -> Result<String, String> {
    match url_scheme(url) {
        Some(scheme) => match scheme.as_str() {
            "http" | "https" | "mailto" | "ftp" => Ok(url.to_string()),
            other => Err(other.to_string()),
        },
        None => Ok(if let Some(path) = url.strip_prefix('/') {
            format!("{base}/{path}")
        } else {
            format!("{base}/{url}")
        }),
    }
}

/// Write bytes straight to the terminal (escape sequences must bypass the
/// ratatui buffer; the next full-frame draw repairs anything transient).
fn emit_raw(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// True for a cell owned by an inline image: either the anchor cell holding
/// the protocol's escape payload, or one of the covered cells filling out the
/// rest of the image rect. Text-extraction and restyling must both leave these
/// alone (CLAUDE.md hard rules 1 and 5).
///
/// ratatui 0.30 replaced the `Cell::skip` bool with `Cell::diff_option`, and
/// ratatui-image 11 uses two of its variants: `ForcedWidth(1)` on the anchor
/// cell that carries the escape payload, `Skip` on every cell the image
/// covers. Nothing in ratatui-widgets sets a diff option on ordinary text, so
/// "diff option is not `None`" is exactly "this cell is not text".
fn is_image_cell(cell: &ratatui::buffer::Cell) -> bool {
    !matches!(cell.diff_option, ratatui::buffer::CellDiffOption::None)
        || cell.symbol().contains('\u{1b}')
}

/// The screens' input-capture probe, free of `self` borrow entanglement.
fn capture_active(app: &App) -> bool {
    app.screens.last().map(|s| s.input_capture()).unwrap_or(false)
}

/// The whole login flow: register a short link, show it, poll for the
/// authorization code relayed by /tui-done, exchange it. Poll errors are
/// transient and retried; only expiry/timeout fail the flow.
async fn run_login_flow(
    tx: &mpsc::UnboundedSender<Msg>,
    client: Arc<WfApiClient>,
    generation: u64,
) -> Result<(), String> {
    let client_id = common::config::oauth_client_id().map_err(|e| e.to_string())?;
    let http = common::http::build().map_err(|e| e.to_string())?;
    let pkce = common::oauth::generate_pkce();
    let state = common::oauth::generate_state();

    // The origin comes from the client, not `config::base_url()`: the flow
    // must talk to the same site the session will be stored for, and a test
    // that ever polls this task can only reach the client's harmless base
    // (issue #565).
    let base = client.base_url().to_string();
    let link = common::oauth::register_link(&http, &base, &state, &pkce.challenge)
        .await
        .map_err(|e| e.to_string())?;
    tx.send(Msg::LoginReady { generation, url: link.url.clone() }).ok();
    let _ = common::oauth::open_browser(&link.url);

    for _ in 0..300 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match common::oauth::poll_link(&http, &base, &link.id).await {
            Ok(common::oauth::PollStatus::Authorized(code)) => {
                let tokens = common::oauth::exchange_code(
                    &http,
                    &base,
                    &code,
                    &common::config::tui_done_url(),
                    &pkce.verifier,
                    &client_id,
                )
                .await
                .map_err(|e| e.to_string())?;
                finish_login(tx, client, tokens, generation).await;
                return Ok(());
            }
            Ok(common::oauth::PollStatus::Expired) => {
                return Err("login link expired — press Enter to start again".into());
            }
            Ok(common::oauth::PollStatus::Waiting) => {}
            Err(e) => tracing::warn!("login poll error (retrying): {e}"),
        }
    }
    Err("timed out waiting for approval — press Enter to start again".into())
}

/// Exchange completion shared by browser and paste paths: persist tokens,
/// fetch the profile, report success.
async fn finish_login(
    tx: &mpsc::UnboundedSender<Msg>,
    client: Arc<WfApiClient>,
    tokens: common::token::TokenSet,
    generation: u64,
) {
    if let Err(e) = client.set_tokens(tokens).await {
        tx.send(Msg::LoginFailed { generation, message: format!("token store: {e}") })
            .ok();
        return;
    }
    match client.me().await {
        Ok(user) => {
            tx.send(Msg::LoginComplete { generation, result: Ok(user) }).ok();
        }
        Err(e) => {
            tx.send(Msg::LoginComplete { generation, result: Err(TaskError::of(&e)) })
                .ok();
        }
    }
}

/// The session-ending failure a message carries, if it carries one — the
/// whole reason `App::handle_msg` can have a single session boundary
/// (issue #557). Every message whose `Err` arm can only come from an API
/// call is listed; `Msg::LoginComplete` deliberately is not, because a
/// failed sign-in belongs to the login screen that is already up (and its
/// own generation check owns it), and neither are the local-only outcomes
/// (`LoggedOut`, `ImageLoaded`, `Notice`, `PaletteMember`).
/// `RecipientResolved` joined the list in issue #597: its lookup is an API
/// call like any other, so a `NoToken`/401 from it must end (or re-check)
/// the session rather than reading as "no such member".
fn session_error_of(msg: &Msg) -> Option<&TaskError> {
    // Only `Msg::Bootstrap` carries the answer to the bootstrap `/me` call
    // (`restore_session`/`recheck_stored_session`) — the one place a 403
    // can mean "there is no account", not merely "no permission for this
    // action" (issue #564).
    let is_bootstrap = matches!(msg, Msg::Bootstrap { .. });
    let err = match msg {
        Msg::SessionLost(e) => e,
        Msg::Bootstrap { result: Err(e), .. }
        | Msg::NodesLoaded(Err(e))
        | Msg::ForumLoaded { result: Err(e), .. }
        | Msg::ThreadLoaded { result: Err(e), .. }
        | Msg::ReplySent(Err(e))
        | Msg::ThreadCreated(Err(e))
        | Msg::MarkedRead(Err(e))
        | Msg::ConversationsLoaded { result: Err(e), .. }
        | Msg::ConversationLoaded { result: Err(e), .. }
        | Msg::ConvoReplySent(Err(e))
        | Msg::ConvoCreated(Err(e))
        | Msg::ConversationMarked(_, Err(e))
        | Msg::AlertsLoaded(Err(e))
        | Msg::MediaLoaded { result: Err(e), .. }
        | Msg::ResourceLoaded { result: Err(e), .. }
        | Msg::MediaCategoriesLoaded(Err(e))
        | Msg::ResourceViewLoaded { result: Err(e), .. }
        | Msg::AlertMarked(Err(e))
        | Msg::SearchDone { result: Err(e), .. }
        | Msg::ProfileLoaded { result: Err(e), .. }
        | Msg::RecipientResolved { id: Err(e), .. }
        | Msg::PostToggled { result: Err(e), .. } => e,
        _ => return None,
    };
    (err.ends_session() || (is_bootstrap && err.is_account_gone())).then_some(err)
}

/// Rewrite the session-shaped error the boundary just consumed into a plain
/// retryable one, so the message can still be dispatched to the screen that
/// is waiting on it (issue #588).
///
/// The boundary used to `return` outright whenever it started a recheck, or
/// swallowed a stale rejection inside the recovery window — which meant the
/// owning screen never saw its `Err` arm at all. For a write that arm is the
/// only place `ComposeState::busy` is cleared, so the composer sat on
/// "Sending…" forever: every key was ignored and Esc answered "wait for the
/// result" for a result that was never coming. A load's arm is likewise the
/// only place `loading` is cleared, and `prime_home_list` refuses to reload a
/// list that still claims to be loading, so Home's pane stayed on "Loading…"
/// after "Session restored.". Neither the session nor the draft is lost here:
/// the recheck is already running and the user can simply press ^S / `r`
/// again, which is exactly what the rewritten message says.
fn mark_retryable(msg: &mut Msg) {
    let err = match msg {
        Msg::SessionLost(e)
        | Msg::Bootstrap { result: Err(e), .. }
        | Msg::NodesLoaded(Err(e))
        | Msg::ForumLoaded { result: Err(e), .. }
        | Msg::ThreadLoaded { result: Err(e), .. }
        | Msg::ReplySent(Err(e))
        | Msg::ThreadCreated(Err(e))
        | Msg::MarkedRead(Err(e))
        | Msg::ConversationsLoaded { result: Err(e), .. }
        | Msg::ConversationLoaded { result: Err(e), .. }
        | Msg::ConvoReplySent(Err(e))
        | Msg::ConvoCreated(Err(e))
        | Msg::ConversationMarked(_, Err(e))
        | Msg::AlertsLoaded(Err(e))
        | Msg::MediaLoaded { result: Err(e), .. }
        | Msg::ResourceLoaded { result: Err(e), .. }
        | Msg::MediaCategoriesLoaded(Err(e))
        | Msg::ResourceViewLoaded { result: Err(e), .. }
        | Msg::AlertMarked(Err(e))
        | Msg::SearchDone { result: Err(e), .. }
        | Msg::ProfileLoaded { result: Err(e), .. }
        | Msg::RecipientResolved { id: Err(e), .. }
        | Msg::PostToggled { result: Err(e), .. } => e,
        _ => return,
    };
    err.message = SESSION_RECHECK_RETRY_MSG.to_string();
    // `Other` is the "try again" bucket: nothing downstream may read this
    // rewritten error back as a reason to end the session, and `code`/
    // `max_page` described the rejection that is gone now.
    err.kind = TaskErrorKind::Other;
    err.code = None;
    err.max_page = None;
}

/// What a message consumed by the session boundary tells its screen instead
/// (issue #588).
const SESSION_RECHECK_RETRY_MSG: &str = "Session token changed elsewhere — re-checking; try again";

/// The last page this client already believed `thread_id` had, found the
/// same way `list_showing` locates a forum's list: scan the screen stack for
/// an open `ThreadView` on that thread. `1` when none is open — reply is
/// always initiated from one, but a stale/missing view must not crash the
/// reload, just fall back to asking for page 2 (which the API's max-page
/// clamp will correct if that's wrong too).
/// The topmost open thread view as (`thread_id`, `page`, `sel_post`,
/// `scroll`) — what a like/vote needs to refresh the page it acted on without
/// losing the reader's place.
fn open_thread_position(screens: &[Screen]) -> Option<(u32, u32, usize, usize)> {
    screens.iter().rev().find_map(|s| match s {
        Screen::ThreadView(v) if v.thread.thread_id > 0 => {
            Some((v.thread.thread_id, v.page.max(1), v.sel_post, v.scroll))
        }
        _ => None,
    })
}

fn known_thread_last_page(screens: &[Screen], thread_id: u32) -> u32 {
    screens
        .iter()
        .rev()
        .find_map(|s| match s {
            Screen::ThreadView(v) if v.thread.thread_id == thread_id => Some(v.last_page.max(1)),
            _ => None,
        })
        .unwrap_or(1)
}

/// The conversation twin of `known_thread_last_page` — a standalone
/// `ConversationView` or the Inbox's inline view pane, whichever is showing
/// this conversation (mirrors `App::conversation_view_mut`'s match arms).
fn known_conversation_last_page(screens: &[Screen], conversation_id: u32) -> u32 {
    screens
        .iter()
        .rev()
        .find_map(|s| match s {
            Screen::ConversationView(v) if v.conversation.conversation_id == conversation_id => {
                Some(v.last_page.max(1))
            }
            Screen::Inbox(inbox) => inbox
                .view
                .as_ref()
                .filter(|v| v.conversation.conversation_id == conversation_id)
                .map(|v| v.last_page.max(1)),
            _ => None,
        })
        .unwrap_or(1)
}

/// The id `open_profile` should fetch: search hits only carry a username, so
/// they call in with `user_id == 0` and a resolved id from `find_user` (or
/// `None` if the lookup came up empty). Any caller that already had a real
/// id keeps it — a stale/empty `found` never overrides a nonzero `user_id`
/// (issue #521: `p` on a search result used to always open `/users/0`).
fn resolve_profile_id(user_id: u32, found: Option<u32>) -> u32 {
    if user_id == 0 {
        found.unwrap_or(0)
    } else {
        user_id
    }
}

/// The thread list a `ForumLoaded` for `node_id` is addressed to: the topmost
/// `ThreadList` — or Home's list pane — that is actually showing that forum.
///
/// Every caller points a list at a node *before* calling `load_forum`, and
/// `Gate::wait` only spaces request starts, so two in-flight loads can finish
/// out of order. Matching on the node is what stops the slower reply from
/// overwriting the list that the faster one already filled — the same
/// discipline `Msg::ThreadLoaded` applies with `thread_id`. `None` means the
/// reply is stale: drop it.
fn list_showing(screens: &mut [Screen], node_id: u32) -> Option<&mut screens::ThreadListState> {
    screens.iter_mut().rev().find_map(|s| match s {
        Screen::ThreadList(list) if list.node_id == node_id => Some(list),
        Screen::Home(h) if h.list.node_id == node_id => Some(&mut h.list),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::{Cell, CellDiffOption};

    /// Image cells are not text. `ratatui-image` puts a whole escape payload
    /// in one anchor cell's symbol (marked `ForcedWidth(1)`, because the
    /// payload's display width is not its byte width) and marks every cell the
    /// image covers `Skip`; all of them must be invisible to the selection
    /// mirror and to the selection band's restyling (CLAUDE.md hard rules 1
    /// and 5).
    #[test]
    fn image_cells_are_recognised_by_both_the_anchor_and_the_covered_cells() {
        let mut plain = Cell::new("a");
        assert!(!is_image_cell(&plain));

        // The rest of an image rect: no symbol of its own, just `Skip`.
        let mut skipped = Cell::new(" ");
        skipped.set_diff_option(CellDiffOption::Skip);
        assert!(is_image_cell(&skipped));

        // The anchor cell: the protocol's payload lives in its symbol, and the
        // widget forces its width to one column.
        let mut anchor = Cell::new("\x1b_Gf=100,i=7;AAAA\x1b\\");
        anchor.set_diff_option(CellDiffOption::ForcedWidth(
            std::num::NonZeroU16::new(1).unwrap(),
        ));
        assert!(is_image_cell(&anchor));
        // ...and it is caught by the escape probe alone, too.
        assert!(is_image_cell(&Cell::new("\x1b_Gf=100,i=7;AAAA\x1b\\")));

        // A cell that merely *looks* busy is still text.
        plain.set_symbol("\u{2503}");
        assert!(!is_image_cell(&plain));
    }

    /// Issue #527/#575: a revoke that did not take must not hide behind
    /// "Logged out." — the tokens are still live on the server until they
    /// expire, and the only honest place to say so is the status line. Round
    /// 6 routed that message through `set_status`, so it was a 4-second
    /// toast `expire_status_toast` blanked while the user was still reading
    /// the sign-in link box — the #527 gap going silent again. It must be a
    /// persistent hint (`status_set_at` stays `None`, so nothing ever clears
    /// it on its own), and it must also land on the Login screen's own error
    /// line so it stays beside the sign-in box rather than only on the
    /// status row.
    #[test]
    fn a_failed_revoke_is_reported_in_the_status_line() {
        let mut app = test_app();
        app.screens.push(screens::login_state());
        app.status = "Logged out.".into();
        // A stale toast timer must not survive: if this write went through
        // `set_status`, `status_set_at` would stay `Some` and the message
        // would vanish `STATUS_TOAST_SECS` later.
        app.status_set_at = Some(std::time::Instant::now());
        app.handle_msg(Msg::LoggedOut(Err(
            "the server kept refresh token valid".into()
        )));
        assert!(
            app.status.contains("Logged out locally")
                && app.status.contains("refresh token"),
            "the revoke failure was swallowed: {:?}",
            app.status
        );
        assert!(
            app.status_set_at.is_none(),
            "a mandated security signal must be a persistent hint, not a toast that expires"
        );
        match app.screens.last() {
            Some(Screen::Login(ls)) => {
                assert!(
                    ls.error.as_deref().is_some_and(|e| e.contains("refresh token")),
                    "the warning must also surface beside the sign-in box: {:?}",
                    ls.error
                );
            }
            _ => panic!("expected the Login screen on top"),
        }

        // A clean logout says nothing extra.
        app.status = "Logged out.".into();
        app.handle_msg(Msg::LoggedOut(Ok(())));
        assert_eq!(app.status, "Logged out.");
    }

    /// Issue #537: a transient load failure must not be permanent. Once a
    /// later `ForumLoaded`/`ThreadLoaded` succeeds, `error` must clear so the
    /// panel goes back to drawing real content instead of the frozen
    /// "Error: ... Press r to retry." — verified both on the state and by
    /// actually rendering the panel headlessly.
    #[test]
    fn a_successful_load_clears_a_previous_error_from_the_panel() {
        let mut app = test_app();
        app.push_screen(Screen::ThreadList(screens::ThreadListState {
            node_id: 1,
            error: Some("connection reset".into()),
            loading: true,
            ..Default::default()
        }));
        app.handle_msg(Msg::ForumLoaded {
            node_id: 1,
            page: 1,
            append: false,
            seq: 0,
            result: Ok(ForumReply::default()),
        });
        {
            let Some(Screen::ThreadList(list)) = app.screens.last() else {
                panic!("expected the ThreadList screen");
            };
            assert!(list.error.is_none(), "a successful ForumLoaded must clear the old error");
        }

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            let screen = app.screens.last_mut().expect("screen");
            screen.render(f, area, &app.theme, &app.glyphs, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        let screen_text: String = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        assert!(
            !screen_text.contains("Error:"),
            "the panel is still showing the stale error after a successful reload:\n{screen_text}"
        );

        // Same contract for the thread view.
        app.screens.clear();
        app.push_screen(Screen::ThreadView(screens::ThreadViewState {
            error: Some("timed out".into()),
            loading: true,
            thread: Thread { thread_id: 9, ..Default::default() },
            ..Default::default()
        }));
        app.handle_msg(Msg::ThreadLoaded {
            id: 9,
            page: 1,
            result: Ok(ThreadReply {
                thread: Thread { thread_id: 9, ..Default::default() },
                ..Default::default()
            }),
        });
        {
            let Some(Screen::ThreadView(view)) = app.screens.last() else {
                panic!("expected the ThreadView screen");
            };
            assert!(view.error.is_none(), "a successful ThreadLoaded must clear the old error");
        }
        let mut term2 =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term2
            .draw(|f| {
                let area = f.area();
                let screen = app.screens.last_mut().expect("screen");
                screen.render(f, area, &app.theme, &app.glyphs, &mut crate::hit::HitMap::default());
            })
            .expect("draw");
        let buf2 = term2.backend().buffer().clone();
        let screen2: String = (0..24)
            .map(|y| (0..80).map(|x| buf2[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        assert!(!screen2.contains("Error:"), "thread view still shows the stale error:\n{screen2}");
    }

    /// The stale-reply identity rule for profiles: a `ProfileLoaded` reply
    /// fills only the screen whose `open_profile` request it answers. Open
    /// A, push B over it while A is still in flight, let A's reply land —
    /// B must still be loading and must never adopt A's user (same pattern
    /// as ForumLoaded, issue #537).
    #[tokio::test]
    async fn a_profile_reply_fills_only_the_screen_that_requested_it() {
        let mut app = test_app();
        app.open_profile(11, "alice");
        let alice_generation = app.profile_generation;
        app.open_profile(22, "bob");
        let bob_generation = app.profile_generation;
        assert_ne!(alice_generation, bob_generation);

        app.handle_msg(Msg::ProfileLoaded {
            generation: alice_generation,
            result: Ok(User { user_id: 11, username: "alice".into(), ..Default::default() }),
        });
        {
            let Some(Screen::Profile(top)) = app.screens.last() else {
                panic!("expected bob's profile screen on top");
            };
            assert_eq!(top.title, "bob");
            assert!(top.user.is_none(), "alice's reply must not fill bob's screen");
            assert!(top.loading, "bob's screen is still waiting on its own reply");
        }

        app.handle_msg(Msg::ProfileLoaded {
            generation: bob_generation,
            result: Ok(User { user_id: 22, username: "bob".into(), ..Default::default() }),
        });
        {
            let Some(Screen::Profile(top)) = app.screens.last() else {
                panic!("expected bob's profile screen on top");
            };
            assert_eq!(top.user.as_ref().map(|u| u.user_id), Some(22));
            assert!(!top.loading);
        }

        // Alice's screen is still live underneath; her (now-arrived) reply
        // fills it, and only it.
        app.screens.pop();
        app.handle_msg(Msg::ProfileLoaded {
            generation: alice_generation,
            result: Ok(User { user_id: 11, username: "alice".into(), ..Default::default() }),
        });
        let Some(Screen::Profile(alice)) = app.screens.last() else {
            panic!("expected alice's profile screen");
        };
        assert_eq!(alice.user.as_ref().map(|u| u.user_id), Some(11));
    }

    /// The same identity rule for searches: a member-content list pushed
    /// over a slow one must not adopt the earlier screen's reply, and the
    /// generations must be minted App-wide (two screens each bumping their
    /// own counter would both say "1" and adopt each other's replies).
    #[tokio::test]
    async fn a_search_reply_fills_only_the_screen_that_requested_it() {
        let mut app = test_app();
        app.execute_action(Action::OpenMemberContent {
            user_id: 11,
            username: "alice".into(),
            content: "thread".into(),
        });
        let alice_generation = app.search_generation;
        app.execute_action(Action::OpenMemberContent {
            user_id: 22,
            username: "bob".into(),
            content: "post".into(),
        });
        let bob_generation = app.search_generation;
        assert_ne!(alice_generation, bob_generation, "generations are App-wide");

        app.handle_msg(Msg::SearchDone {
            generation: alice_generation,
            page: 1,
            result: Ok(SearchResultsReply::default()),
        });
        {
            let Some(Screen::Search(top)) = app.screens.last() else {
                panic!("expected bob's search screen on top");
            };
            assert_eq!(top.member.as_ref().map(|m| m.0), Some(22));
            assert!(top.loading, "bob's screen is still waiting on its own reply");
            assert!(top.results.is_empty(), "alice's reply must not fill bob's screen");
        }

        app.handle_msg(Msg::SearchDone {
            generation: bob_generation,
            page: 1,
            result: Ok(SearchResultsReply::default()),
        });
        let Some(Screen::Search(top)) = app.screens.last() else {
            panic!("expected bob's search screen on top");
        };
        assert!(!top.loading, "bob's own reply clears the wait");
    }

    /// Issue #535: the recipients field prefilled from a profile's `c` key
    /// must seed a CHAR cursor, not a byte length — every editor.rs helper
    /// indexes by chars, and a non-ASCII username has more bytes than chars.
    #[test]
    fn recipient_prefill_seeds_a_char_cursor_not_a_byte_length() {
        let mut app = test_app();
        app.execute_action(Action::StartNewConversation(Some("Zoë".into())));
        let Some(Screen::NewConversation(state)) = app.screens.last() else {
            panic!("expected a NewConversation screen to be pushed");
        };
        assert_eq!(state.recipients, "Zoë, ");
        assert_eq!(state.recipients.len(), 6, "sanity: the byte length differs from the char count");
        assert_eq!(
            state.recipients_cursor,
            state.recipients.chars().count(),
            "cursor must be a char index"
        );
        assert_eq!(state.recipients_cursor, 5);

        // With the old byte-length seed (6), Ctrl+W would drain
        // `start..*cursor` = `0..6` against a 5-char Vec and panic. It must
        // now just delete the whole prefilled string.
        let mut cursor = state.recipients_cursor;
        let mut text = state.recipients.clone();
        crate::editor::delete_word_back(&mut text, &mut cursor);
        assert_eq!(text, "");
        assert_eq!(cursor, 0);
    }

    /// Issue #525: `set_stringn` resets the cell(s) a double-width character
    /// covers, so mirroring every cell's symbol put a phantom space after
    /// each ideograph on the clipboard and made word-select stop at it.
    #[test]
    fn the_selection_mirror_skips_wide_char_continuation_cells() {
        use ratatui::backend::TestBackend;
        use ratatui::widgets::Paragraph;

        let mut app = test_app();
        let mut term = ratatui::Terminal::new(TestBackend::new(12, 1)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            f.render_widget(Paragraph::new("\u{6f22}\u{5b57} ok"), area);
            app.capture_screen(f);
        })
        .expect("draw");

        assert_eq!(
            app.screen_rows[0].trim_end(),
            "\u{6f22}\u{5b57} ok",
            "the mirror inserted a phantom space after a wide character"
        );

        // Columns: 0-1 = 漢, 2-3 = 字, 4 = space, 5-6 = "ok".
        for col in [0u16, 1, 2, 3] {
            assert_eq!(
                app.find_word_bounds(col, 0),
                Some((0, 3)),
                "clicking column {col} must select the whole ideographic word"
            );
        }
        assert_eq!(
            app.extract_selection_text(0, 0, 3, 0),
            "\u{6f22}\u{5b57}",
            "the copied text must not carry the continuation cells"
        );
        assert_eq!(app.find_word_bounds(5, 0), Some((5, 6)));
        assert_eq!(app.extract_selection_text(0, 0, 6, 0), "\u{6f22}\u{5b57} ok");
        assert_eq!(app.find_word_bounds(4, 0), None, "a space is not a word");
    }

    /// Issue #562: `paint_selection` used to hard-code Black-on-LightBlue,
    /// the one place in the client that ignored the theme — every other
    /// selected/focused band goes through `Theme::selected()`, which reverses
    /// instead of tinting on `NO_COLOR`/16-colour terminals. With
    /// `Theme::mono()` (what `NO_COLOR` selects) the selection band must
    /// paint no colour at all, same as `theme::tests::mono_paints_nothing`
    /// asserts for every other role.
    #[test]
    fn mono_selection_band_paints_no_colour() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;

        let mut app = test_app();
        app.theme = crate::theme::Theme::mono();
        app.selection = Some(Selection {
            anchor: (0, 0),
            end: (3, 0),
        });
        let mut term = ratatui::Terminal::new(TestBackend::new(12, 1)).expect("terminal");
        term.draw(|f| app.paint_selection(f)).expect("draw");
        let buf = term.backend().buffer().clone();
        for x in 0..=3u16 {
            let cell = &buf[(x, 0)];
            assert_eq!(cell.fg, Color::Reset, "mono selection painted a foreground at col {x}");
            assert_eq!(cell.bg, Color::Reset, "mono selection painted a background at col {x}");
        }
    }

    /// Issue #529: after posting a reply the client must ask for one page
    /// past whatever it already knew as the thread's last page (so the
    /// existing `max_page` clamp in `Msg::ThreadLoaded` lands on the true
    /// last page, new or not) — never hard-code page 1 and strand the
    /// reader's own reply off-screen on a multi-page thread.
    #[test]
    fn known_thread_last_page_finds_the_open_view_or_defaults_to_one() {
        let view = |thread_id: u32, last_page: u32| {
            Screen::ThreadView(screens::ThreadViewState {
                thread: Thread { thread_id, ..Default::default() },
                last_page,
                ..Default::default()
            })
        };
        let screens = vec![view(1, 1), view(42, 3)];
        assert_eq!(known_thread_last_page(&screens, 42), 3);
        assert_eq!(known_thread_last_page(&screens, 1), 1);
        // No open ThreadView for this thread -> defensive default, never 0.
        assert_eq!(known_thread_last_page(&screens, 999), 1);

        // A thread that briefly reports last_page 0 (unloaded) never yields
        // a "request page 1" that a downstream +1 would leave at 1 forever.
        let screens = vec![view(7, 0)];
        assert_eq!(known_thread_last_page(&screens, 7), 1);
    }

    /// The conversation twin of the test above (issue #550): must find a
    /// standalone `ConversationView`, must also find one sitting in the
    /// Inbox's dual-pane view slot, and must default defensively to 1 rather
    /// than 0 for both "no matching view" and "not yet loaded".
    #[test]
    fn known_conversation_last_page_finds_the_open_view_or_defaults_to_one() {
        let convo = |id: u32| Conversation { conversation_id: id, ..Default::default() };
        let standalone = |id: u32, last_page: u32| {
            Screen::ConversationView(screens::ConversationViewState {
                conversation: convo(id),
                last_page,
                ..Default::default()
            })
        };
        let inbox_view = |id: u32, last_page: u32| {
            Screen::Inbox(screens::InboxState {
                dual: true,
                view: Some(screens::ConversationViewState {
                    conversation: convo(id),
                    last_page,
                    ..Default::default()
                }),
                ..Default::default()
            })
        };

        let screens = vec![standalone(1, 1), standalone(42, 3)];
        assert_eq!(known_conversation_last_page(&screens, 42), 3);
        assert_eq!(known_conversation_last_page(&screens, 1), 1);
        assert_eq!(known_conversation_last_page(&screens, 999), 1, "no matching view");

        let screens = vec![inbox_view(9, 4)];
        assert_eq!(known_conversation_last_page(&screens, 9), 4, "Inbox's dual-pane view slot");

        // A conversation that briefly reports last_page 0 (unloaded) never
        // yields a "request page 1" that a downstream +1 would leave at 1
        // forever.
        let screens = vec![standalone(7, 0)];
        assert_eq!(known_conversation_last_page(&screens, 7), 1);
    }

    /// Issue #608: `Msg::ConversationMarked` used to reload page 1 of the
    /// conversations list unconditionally, throwing the user back to page 1
    /// (with a re-clamped selection) no matter which page they marked read
    /// from. It must instead flip that one row in place and recount the
    /// unread badge — the list (and its page) must not move at all.
    #[tokio::test]
    async fn conversation_marked_flips_the_row_in_place_and_keeps_the_current_page() {
        let mut app = test_app();
        let convo = |id: u32, unread: bool| Conversation {
            conversation_id: id,
            is_unread: unread,
            ..Default::default()
        };
        app.push_screen(Screen::Inbox(screens::InboxState {
            convos: screens::ConversationsState {
                conversations: vec![convo(21, true), convo(22, true)],
                page: 2,
                last_page: 2,
                ..Default::default()
            },
            ..Default::default()
        }));
        app.convos_unread = 5;

        app.handle_msg(Msg::ConversationMarked(22, Ok(())));

        let Some(Screen::Inbox(ib)) = app.screens.last() else {
            panic!("expected the Inbox screen to survive");
        };
        assert_eq!(ib.convos.page, 2, "the list must not jump back to page 1");
        let row = ib.convos.conversations.iter().find(|c| c.conversation_id == 22).unwrap();
        assert!(!row.is_unread, "the marked row must flip locally");
        let other = ib.convos.conversations.iter().find(|c| c.conversation_id == 21).unwrap();
        assert!(other.is_unread, "an unrelated row must be untouched");
        assert_eq!(
            app.convos_unread, 1,
            "the badge must be recounted from the (now one-less-unread) list, not left stale"
        );

        // No API call escaped besides the one this test already accounted
        // for — no reload was spawned.
        tokio::task::yield_now().await;
    }

    /// Issue #553: `open_url` used to test only for a lower-case
    /// `http://`/`https://` prefix, so every other scheme (a `mailto:` link
    /// from `[EMAIL]`, an upper-case `HTTPS://` auto-link, `ftp://`) fell
    /// through to the "site-relative path" branch and got `base_url()`
    /// prefixed onto it, mangling it into a 404 on the forum. `resolve_open_url`
    /// must pass the four allowed schemes through untouched (matched
    /// ASCII-case-insensitively) and refuse everything else with a scheme it
    /// names, never silently rewriting it.
    #[test]
    fn resolve_open_url_allows_four_schemes_and_refuses_the_rest() {
        let base = "https://windowsforum.com";

        // The four allowed schemes pass through byte-for-byte.
        assert_eq!(
            resolve_open_url("mailto:someone@example.com", base),
            Ok("mailto:someone@example.com".into())
        );
        assert_eq!(
            resolve_open_url("HTTPS://Example.com/x", base),
            Ok("HTTPS://Example.com/x".into()),
            "case must not affect whether the scheme is recognized, or the URL itself"
        );
        assert_eq!(
            resolve_open_url("ftp://host/file", base),
            Ok("ftp://host/file".into())
        );
        assert_eq!(
            resolve_open_url("http://windowsforum.com/threads/1", base),
            Ok("http://windowsforum.com/threads/1".into())
        );

        // Everything else with a scheme is refused, never rewritten.
        assert_eq!(resolve_open_url("javascript:alert(1)", base), Err("javascript".into()));
        assert_eq!(resolve_open_url("data:text/html,x", base), Err("data".into()));
        assert_eq!(resolve_open_url("file:///etc/passwd", base), Err("file".into()));
        assert_eq!(resolve_open_url("VBScript:msgbox(1)", base), Err("vbscript".into()));

        // No scheme at all: a site-relative path, prefixed with base exactly
        // as before.
        assert_eq!(
            resolve_open_url("/threads/123", base),
            Ok("https://windowsforum.com/threads/123".into())
        );
        assert_eq!(
            resolve_open_url("threads/123", base),
            Ok("https://windowsforum.com/threads/123".into())
        );
        // A colon appearing only in a query string (after the first `/`) is
        // not a scheme.
        assert_eq!(
            resolve_open_url("/search?q=foo:bar", base),
            Ok("https://windowsforum.com/search?q=foo:bar".into())
        );
    }

    /// A `WfApi` that never touches the network: every call fails with
    /// `NoToken` except `mark_conversation_read`, which just records the id.
    /// Substituted for `App::api` so a handler test can assert on the
    /// PRESENCE or ABSENCE of a server-side side-effect (issue #541) without
    /// a single request leaving the process.
    #[derive(Default)]
    struct RecordingApi {
        /// #694: what the client actually reported as read, and how often it
        /// cleared the alert counter.
        threads_read: std::sync::Mutex<Vec<(u32, Option<i64>)>>,
        /// #708: the post writes that actually went out.
        edits: std::sync::Mutex<Vec<(u32, String)>>,
        deletes: std::sync::Mutex<Vec<(u32, bool)>>,
        solutions: std::sync::Mutex<Vec<u32>>,
        alerts_viewed: std::sync::atomic::AtomicUsize,
        marked_read: std::sync::Mutex<Vec<u32>>,
        /// `(user_id, content, page)` per `search_member` call, and the
        /// keywords of every `search_advanced` call — issue #548 is exactly
        /// "the member list asked the keyword search instead".
        member_searches: std::sync::Mutex<Vec<(u32, String, u32)>>,
        keyword_searches: std::sync::Mutex<Vec<String>>,
        /// `(thread_id, message)` per `reply` call that actually reached the
        /// stub — issue #567 is "the write went out after sign-out", so the
        /// absence of an entry here is the assertion.
        /// #709: replies now record the attachment key they carried.
        replies: std::sync::Mutex<Vec<(u32, String, Option<String>)>>,
        /// `post_id` per `react_post` call that reached the stub, under the
        /// same `write_delay` contract as `replies` — a like is a write
        /// (issue #567's rule) and "the reaction went out after sign-out" is
        /// what must never be observable here.
        reactions: std::sync::Mutex<Vec<u32>>,
        /// Stands in for the politeness gates: `reply` waits this long
        /// *before* recording anything, the way `WfApiClient::post_form`
        /// waits on `api_gate`/`write_gate` before it even looks at the
        /// token. Zero by default, so every other test is unaffected.
        write_delay: Duration,
        /// `(ids, title, body)` per `create_conversation` call that reached
        /// the stub. Issue #597 is "Enter twice started two conversations",
        /// so the LENGTH of this is the assertion.
        conversations_created: std::sync::Mutex<Vec<(Vec<u32>, String, String)>>,
        /// Overrides `me()`'s default `NoToken` so a test can make the
        /// bootstrap/recheck `/me` call succeed — issue #580's pinning test
        /// needs the recheck to actually answer `Bootstrap { Ok }` so it can
        /// assert that answer is honoured rather than swallowed.
        me_ok: std::sync::Mutex<Option<User>>,
        /// #716: the draft relay calls that reached the stub, and what
        /// `list_drafts` should answer. `drafts_fail` makes the relay error,
        /// which is how "a relay failure must not cost the local draft" is
        /// tested.
        drafts_saved: std::sync::Mutex<Vec<(String, String, String)>>,
        drafts_deleted: std::sync::Mutex<Vec<String>>,
        drafts_remote: std::sync::Mutex<Vec<common::models::RemoteDraft>>,
        drafts_fail: std::sync::atomic::AtomicBool,
    }

    impl RecordingApi {
        fn marks(&self) -> Vec<u32> {
            self.marked_read.lock().expect("lock").clone()
        }
        fn member_searches(&self) -> Vec<(u32, String, u32)> {
            self.member_searches.lock().expect("lock").clone()
        }
        fn keyword_searches(&self) -> Vec<String> {
            self.keyword_searches.lock().expect("lock").clone()
        }
        fn conversations_created(&self) -> Vec<(Vec<u32>, String, String)> {
            self.conversations_created.lock().expect("lock").clone()
        }
        fn replies(&self) -> Vec<(u32, String, Option<String>)> {
            self.replies.lock().expect("lock").clone()
        }
        fn reactions(&self) -> Vec<u32> {
            self.reactions.lock().expect("lock").clone()
        }
        fn drafts_saved(&self) -> Vec<(String, String, String)> {
            self.drafts_saved.lock().expect("lock").clone()
        }
        fn drafts_deleted(&self) -> Vec<String> {
            self.drafts_deleted.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl WfApi for RecordingApi {
        async fn list_drafts(&self) -> common::error::Result<Vec<common::models::RemoteDraft>> {
            if self.drafts_fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(common::error::Error::NoToken);
            }
            Ok(self.drafts_remote.lock().expect("lock").clone())
        }
        async fn save_draft(
            &self,
            xf_key: &str,
            message: &str,
            title: &str,
            _attachment_key: Option<&str>,
        ) -> common::error::Result<()> {
            if self.drafts_fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(common::error::Error::NoToken);
            }
            self.drafts_saved.lock().expect("lock").push((
                xf_key.to_string(),
                message.to_string(),
                title.to_string(),
            ));
            Ok(())
        }
        async fn delete_draft(&self, xf_key: &str) -> common::error::Result<()> {
            if self.drafts_fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(common::error::Error::NoToken);
            }
            self.drafts_deleted.lock().expect("lock").push(xf_key.to_string());
            Ok(())
        }
        async fn mark_conversation_read(&self, id: u32) -> common::error::Result<()> {
            self.marked_read.lock().expect("lock").push(id);
            Ok(())
        }
        async fn nodes(&self) -> common::error::Result<Vec<Node>> {
            Err(common::error::Error::NoToken)
        }
        async fn forum(&self, _: u32, _: u32) -> common::error::Result<ForumReply> {
            Err(common::error::Error::NoToken)
        }
        async fn threads(&self, _: u32) -> common::error::Result<common::models::ThreadsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn thread(&self, _: u32, _: u32) -> common::error::Result<ThreadReply> {
            Err(common::error::Error::NoToken)
        }
        async fn thread_posts(&self, _: u32, _: u32) -> common::error::Result<common::models::PostsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn reply(
            &self,
            thread_id: u32,
            message: &str,
            attachment_key: Option<&str>,
        ) -> common::error::Result<common::models::Post> {
            // Recorded AFTER the simulated gate, not before: several tests
            // assert that nothing went out while the write was still
            // waiting, and an early record would make them pass on a lie.
            tokio::time::sleep(self.write_delay).await;
            self.replies.lock().expect("lock").push((
                thread_id,
                message.to_string(),
                attachment_key.map(str::to_string),
            ));
            Err(common::error::Error::NoToken)
        }
        async fn create_thread(
            &self,
            _: u32,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> common::error::Result<Thread> {
            Err(common::error::Error::NoToken)
        }
        async fn edit_post(
            &self,
            id: u32,
            message: &str,
            _: Option<&str>,
        ) -> common::error::Result<()> {
            self.edits.lock().expect("lock").push((id, message.to_string()));
            Ok(())
        }
        async fn delete_post(&self, id: u32, hard: bool) -> common::error::Result<()> {
            self.deletes.lock().expect("lock").push((id, hard));
            Ok(())
        }
        async fn mark_solution(&self, id: u32) -> common::error::Result<()> {
            self.solutions.lock().expect("lock").push(id);
            Ok(())
        }
        async fn mark_alerts_viewed(&self) -> common::error::Result<()> {
            self.alerts_viewed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn mark_thread_read(&self, id: u32, date: Option<i64>) -> common::error::Result<()> {
            self.threads_read.lock().expect("lock").push((id, date));
            Ok(())
        }
        async fn mark_forum_read(&self, _: u32) -> common::error::Result<()> {
            Err(common::error::Error::NoToken)
        }
        async fn conversations(&self, _: u32) -> common::error::Result<ConversationsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn conversation(&self, _: u32, _: u32) -> common::error::Result<ConversationReply> {
            Err(common::error::Error::NoToken)
        }
        async fn reply_conversation(&self, _: u32, _: &str) -> common::error::Result<()> {
            Err(common::error::Error::NoToken)
        }
        async fn create_conversation(
            &self,
            ids: &[u32],
            title: &str,
            body: &str,
        ) -> common::error::Result<Conversation> {
            self.conversations_created
                .lock()
                .expect("lock")
                .push((ids.to_vec(), title.to_string(), body.to_string()));
            Err(common::error::Error::NoToken)
        }
        async fn delete_conversation(&self, _: u32) -> common::error::Result<()> {
            Err(common::error::Error::NoToken)
        }
        async fn alerts(&self, _: u32) -> common::error::Result<AlertsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn mark_alert_read(&self, _: u32) -> common::error::Result<()> {
            Err(common::error::Error::NoToken)
        }
        async fn search(&self, _: &str, _: u32) -> common::error::Result<SearchResultsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn media_list(
            &self,
            _: Option<u32>,
            _: u32,
        ) -> common::error::Result<common::models::MediaListReply> {
            Ok(common::models::MediaListReply::default())
        }
        async fn media_categories(
            &self,
        ) -> common::error::Result<common::models::MediaCategoriesReply> {
            Ok(common::models::MediaCategoriesReply::default())
        }
        async fn media_item(
            &self,
            id: u32,
        ) -> common::error::Result<common::models::MediaItemReply> {
            Ok(common::models::MediaItemReply {
                media: common::models::MediaItem { media_id: id, ..Default::default() },
            })
        }
        async fn resource(
            &self,
            id: u32,
        ) -> common::error::Result<common::models::ResourceReply> {
            Ok(common::models::ResourceReply {
                resource: common::models::Resource { resource_id: id, ..Default::default() },
            })
        }
        async fn resources_list(
            &self,
            _: u32,
        ) -> common::error::Result<common::models::ResourceListReply> {
            Ok(common::models::ResourceListReply::default())
        }
        async fn search_advanced(
            &self,
            query: &SearchQuery,
        ) -> common::error::Result<SearchResultsReply> {
            self.keyword_searches
                .lock()
                .expect("lock")
                .push(query.keywords.clone());
            Err(common::error::Error::NoToken)
        }
        async fn search_member(
            &self,
            user_id: u32,
            content: &str,
            page: u32,
        ) -> common::error::Result<SearchResultsReply> {
            self.member_searches
                .lock()
                .expect("lock")
                .push((user_id, content.to_string(), page));
            Err(common::error::Error::NoToken)
        }
        async fn react_post(&self, post_id: u32, _: u32) -> common::error::Result<Toggle> {
            tokio::time::sleep(self.write_delay).await;
            self.reactions.lock().expect("lock").push(post_id);
            Err(common::error::Error::NoToken)
        }
        async fn vote_post(&self, _: u32, _: &str) -> common::error::Result<Toggle> {
            Err(common::error::Error::NoToken)
        }
        async fn me(&self) -> common::error::Result<User> {
            match self.me_ok.lock().expect("lock").clone() {
                Some(user) => Ok(user),
                None => Err(common::error::Error::NoToken),
            }
        }
        async fn user(&self, _: u32) -> common::error::Result<User> {
            Err(common::error::Error::NoToken)
        }
        async fn find_user(&self, _: &str) -> common::error::Result<Option<User>> {
            Err(common::error::Error::NoToken)
        }
    }

    /// Issue #549: the wheel used to be three synthetic Up/Down keys pushed
    /// through `handle_key` with the pointer position thrown away, so on the
    /// dual-pane Home scrolling over the thread list walked the Forums cursor,
    /// a wheel over the header or key bar still scrolled the body, and the
    /// three keys closed the `?` card on the way past.
    #[tokio::test]
    async fn the_wheel_scrolls_the_pane_under_the_pointer_and_leaves_overlays_alone() {
        use ratatui::crossterm::event::{MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let wheel = |column: u16, row: u16| MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        let mut app = test_app();
        // The frame the panes were drawn in: header row 0, body rows 1..21,
        // key bar and status below it; Forums 37 wide, list to its right.
        app.body_rect = Rect::new(0, 1, 120, 20);
        app.screens.push(Screen::Home(screens::HomeState {
            tree: screens::ForumTreeState {
                nodes: (1..=5)
                    .map(|node_id| Node {
                        node_id,
                        title: format!("Forum {node_id}"),
                        node_type: "Forum".into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            list: screens::ThreadListState {
                threads: (1..=5)
                    .map(|thread_id| Thread {
                        thread_id,
                        title: format!("Thread {thread_id}"),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            focus: screens::Pane::Tree,
            dual: true,
            tree_rect: Rect::new(0, 1, 37, 20),
            list_rect: Rect::new(37, 1, 83, 20),
        }));

        // Wheel over the RIGHT pane while the tree has the keyboard.
        app.handle_mouse(wheel(100, 5));
        {
            let Some(Screen::Home(h)) = app.screens.last() else {
                panic!("expected Home");
            };
            assert_eq!(h.list.sel, 3, "the list under the pointer must scroll");
            assert_eq!(h.tree.sel, 0, "the focused tree must not move");
            assert_eq!(h.focus, screens::Pane::List, "the pointer takes the keyboard");
        }

        // ...and back over the left pane.
        app.handle_mouse(wheel(10, 5));
        {
            let Some(Screen::Home(h)) = app.screens.last() else {
                panic!("expected Home");
            };
            assert_eq!(h.tree.sel, 3, "the tree under the pointer must scroll");
            assert_eq!(h.list.sel, 3, "the list must stay where it was");
            assert_eq!(h.focus, screens::Pane::Tree);
        }

        // Outside the body — the key bar — nothing scrolls at all.
        app.handle_mouse(wheel(10, 21));
        app.handle_mouse(wheel(10, 0));
        {
            let Some(Screen::Home(h)) = app.screens.last() else {
                panic!("expected Home");
            };
            assert_eq!((h.tree.sel, h.list.sel), (3, 3), "only the body scrolls");
        }

        // The `?` card and an armed `g` chord survive a wheel.
        app.show_help = true;
        app.handle_mouse(wheel(100, 5));
        assert!(app.show_help, "the wheel must not close the keys card");
        app.show_help = false;
        app.prefix.arm();
        app.handle_mouse(wheel(100, 5));
        assert!(app.prefix.armed(), "the wheel must not cancel an armed g chord");
    }

    /// Issue #548: the Search screen a profile's `t`/`p` opens is a member's
    /// content list. Paging it and flipping threads/posts must reach
    /// `search_member` with the right page and content — before this, they
    /// posted the human label ("by: kemical (thread)") to the keyword search,
    /// which returned unrelated hits or nothing at all.
    #[tokio::test]
    async fn member_content_keys_reach_search_member_and_never_the_keyword_search() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();

        app.execute_action(Action::OpenMemberContent {
            user_id: 42,
            username: "kemical".into(),
            content: "thread".into(),
        });
        tokio::task::yield_now().await;
        assert_eq!(api.member_searches(), vec![(42, "thread".into(), 1)]);

        // The screen remembers the mode, so the keys route through it.
        let Some(Screen::Search(search)) = app.screens.last_mut() else {
            panic!("expected the member's content on top");
        };
        assert_eq!(search.member, Some((42, "thread".to_string())));
        search.page = 2;
        search.last_page = 5;
        // The initial fetch's reply stays in the channel in this test, so
        // clear the wait it would have cleared.
        search.loading = false;

        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        tokio::task::yield_now().await;
        // The page fetch is now in flight (`loading = true`); its reply
        // clears the flag before `t` may fire the next request.
        let Some(Screen::Search(search)) = app.screens.last_mut() else {
            panic!("expected the member's content on top");
        };
        search.loading = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        tokio::task::yield_now().await;

        assert_eq!(
            api.member_searches(),
            vec![
                (42, "thread".into(), 1),
                (42, "thread".into(), 3),
                (42, "post".into(), 1),
            ],
            "] pages the member's own list; t flips the content type"
        );
        assert!(
            api.keyword_searches().is_empty(),
            "member content must never be requested as a keyword search: {:?}",
            api.keyword_searches()
        );
    }

    /// Issue #597: `NewConversationState::busy` used to be dropped the moment
    /// the last recipient resolved — *before* `create_conversation` was
    /// spawned behind the api + write gates (up to 30 s). In that window a
    /// second Enter started a second resolve + create (two identical
    /// conversations) and Esc popped the screen out from under the in-flight
    /// write. `busy` now spans the whole lifecycle.
    #[tokio::test]
    async fn a_second_enter_after_recipients_resolve_cannot_start_a_second_conversation() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.push_screen(Screen::NewConversation(screens::NewConversationState {
            recipients: "kemical".into(),
            title: "hi".into(),
            body: "hello".into(),
            field: 2,
            ..Default::default()
        }));

        // Enter in the message field submits: resolution starts.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        tokio::task::yield_now().await;
        // The one recipient resolves.
        app.handle_msg(Msg::RecipientResolved { name: "kemical".into(), id: Ok(Some(9)) });
        tokio::task::yield_now().await;
        assert_eq!(api.conversations_created().len(), 1, "one create was spawned");

        let Some(Screen::NewConversation(nc)) = app.screens.last() else {
            panic!("the screen must still be up while the write is in flight");
        };
        assert!(nc.busy, "busy must span the write, not just the resolution");
        assert!(nc.sending, "the screen says Sending… once the write is spawned");

        // Enter again, then let a resolution answer land: no second create.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        tokio::task::yield_now().await;
        app.handle_msg(Msg::RecipientResolved { name: "kemical".into(), id: Ok(Some(9)) });
        tokio::task::yield_now().await;
        assert_eq!(
            api.conversations_created().len(),
            1,
            "a second Enter must not queue a duplicate conversation"
        );

        // Esc cannot pop the screen the write is going to report back to.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(app.screens.last(), Some(Screen::NewConversation(_))),
            "Esc must be blocked while the create is in flight"
        );
    }

    /// Issue #597, second defect on the same path: `find_user(..).unwrap_or(None)`
    /// turned every transport/429/5xx error into "<name>: not found", sending
    /// the user off to rename a perfectly good recipient.
    #[tokio::test]
    async fn a_failed_recipient_lookup_does_not_read_as_not_found() {
        let mut app = test_app();
        app.push_screen(Screen::NewConversation(screens::NewConversationState {
            recipients: "kemical".into(),
            title: "hi".into(),
            body: "hello".into(),
            busy: true,
            resolving: 1,
            ..Default::default()
        }));
        app.handle_msg(Msg::RecipientResolved {
            name: "kemical".into(),
            id: Err(TaskError {
                message: "server error".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Api(500),
            }),
        });
        let Some(Screen::NewConversation(nc)) = app.screens.last() else {
            panic!("expected the NewConversation screen");
        };
        assert_eq!(nc.errors.len(), 1, "{:?}", nc.errors);
        assert!(nc.errors[0].contains("lookup failed"), "{:?}", nc.errors);
        assert!(!nc.errors[0].contains("not found"), "{:?}", nc.errors);
        assert!(!nc.busy, "a failed resolution hands the form back");
        assert!(!nc.sending);
    }

    /// Issue #597: `ConvoCreated(Err)` with the screen already gone used to
    /// drop the failure on the floor — every other write arm says it out loud.
    #[test]
    fn a_create_failure_with_the_screen_gone_still_reaches_the_status_line() {
        let mut app = test_app();
        app.handle_msg(Msg::ConvoCreated(Err(TaskError {
            message: "flooding".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::Other,
        })));
        assert!(app.status.contains("flooding"), "status was {:?}", app.status);
    }

    /// Issue #541: a dual-pane Inbox primes its view pane with the newest
    /// conversation before the user has selected anything. That load must not
    /// mark it read on the server; the load the user actually asked for must,
    /// and must also stop the Inbox row and the header badge from claiming it
    /// is still unread.
    #[tokio::test]
    async fn only_a_user_opened_conversation_is_marked_read() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();

        let conv = Conversation {
            conversation_id: 7,
            title: "Hello".into(),
            is_unread: true,
            ..Default::default()
        };
        let reply = || ConversationReply {
            conversation: Conversation { conversation_id: 7, ..Default::default() },
            ..Default::default()
        };
        app.screens.push(Screen::Inbox(screens::InboxState {
            dual: true,
            convos: screens::ConversationsState {
                conversations: vec![conv.clone()],
                page: 1,
                last_page: 1,
                ..Default::default()
            },
            view: Some(screens::ConversationViewState {
                conversation: conv.clone(),
                page: 1,
                loading: true,
                ..Default::default()
            }),
            ..Default::default()
        }));
        app.convos_unread = 1;

        // The priming load: no mark-read, and the row stays unread.
        app.handle_msg(Msg::ConversationLoaded {
            id: 7,
            page: 1,
            mark_read: false,
            result: Ok(reply()),
        });
        tokio::task::yield_now().await;
        assert!(api.marks().is_empty(), "priming the pane marked a DM read on the server");
        {
            let Some(Screen::Inbox(inbox)) = app.screens.last() else {
                panic!("expected the Inbox screen");
            };
            assert!(inbox.convos.conversations[0].is_unread_conv());
        }
        assert_eq!(app.convos_unread, 1);

        // The user opening it: marked read once, and the client's own picture
        // (row + badge) follows.
        app.handle_msg(Msg::ConversationLoaded {
            id: 7,
            page: 1,
            mark_read: true,
            result: Ok(reply()),
        });
        tokio::task::yield_now().await;
        assert_eq!(api.marks(), vec![7]);
        let Some(Screen::Inbox(inbox)) = app.screens.last() else {
            panic!("expected the Inbox screen");
        };
        assert!(
            !inbox.convos.conversations[0].is_unread_conv(),
            "the Inbox row still shows unread after the server was told otherwise"
        );
        assert_eq!(app.convos_unread, 0);
    }

    /// Issue #550: after a DM reply the client used to hard-code a reload of
    /// page 1, stranding the reply on a multi-page conversation exactly like
    /// #529 did for threads before that fix. `ConvoReplySent(Ok)` must ask
    /// for one page past whatever it already knew as the conversation's last
    /// page, mirroring `ReplySent`.
    #[tokio::test]
    async fn convo_reply_sent_asks_for_one_page_past_the_known_last_page() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();

        app.screens.push(Screen::ConversationView(screens::ConversationViewState {
            conversation: Conversation { conversation_id: 7, ..Default::default() },
            page: 2,
            last_page: 2,
            ..Default::default()
        }));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ConversationReply {
                conversation_id: 7,
                conversation_title: "Hello".into(),
                participants: "kemical".into(),
            }),
            body: "reply text".into(),
            ..Default::default()
        }));

        app.handle_msg(Msg::ConvoReplySent(Ok(())));
        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("load_conversation must send a message")
            .expect("channel open");
        match sent {
            Msg::ConversationLoaded { id, page, mark_read, .. } => {
                assert_eq!(id, 7);
                assert_eq!(page, 3, "must ask for known last_page (2) + 1, not hard-coded 1");
                assert!(mark_read, "the reader's own reload must still mark the DM read");
            }
            _ => panic!("expected ConversationLoaded"),
        }
        // The composer must be gone and the status must say so.
        assert!(!app.screens.iter().any(|s| matches!(s, Screen::Compose(_))));
        assert_eq!(app.status, "Message sent.");
    }

    /// Issue #550: a `ConversationLoaded` error that carries `max_page` (the
    /// reply-reload above guessed one page too far) must clamp and re-ask for
    /// the true last page, exactly as `Msg::ThreadLoaded` already does —
    /// never leave the view showing a bare "Error: ..." for a page that
    /// simply does not exist yet.
    #[tokio::test]
    async fn conversation_loaded_error_with_max_page_reloads_the_clamped_page() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();

        app.screens.push(Screen::ConversationView(screens::ConversationViewState {
            conversation: Conversation { conversation_id: 7, ..Default::default() },
            page: 3,
            last_page: 3,
            loading: true,
            ..Default::default()
        }));

        app.handle_msg(Msg::ConversationLoaded {
            id: 7,
            page: 3,
            mark_read: true,
            result: Err(TaskError {
                message: "invalid page".into(),
                code: Some("invalid_page".into()),
                max_page: Some(2),
                kind: TaskErrorKind::Api(400),
            }),
        });

        {
            let Some(Screen::ConversationView(view)) = app.screens.last() else {
                panic!("expected the ConversationView screen");
            };
            assert_eq!(view.last_page, 2, "the view must clamp to the server's max_page");
            assert!(view.loading, "the clamped reload must be in flight");
            assert!(view.error.is_none(), "a clamp-and-retry must not also show an error");
        }

        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the clamp must re-request the page")
            .expect("channel open");
        match sent {
            Msg::ConversationLoaded { id, page, mark_read, .. } => {
                assert_eq!(id, 7);
                assert_eq!(page, 2, "must re-request the clamped max_page, not the stale page");
                assert!(mark_read, "the original mark_read intent must survive the retry");
            }
            _ => panic!("expected ConversationLoaded"),
        }
    }

    /// Issue #551: `TaskError::of` must classify every `Error` variant into
    /// exactly the kind `ends_session()` needs to answer correctly — a 401/403
    /// `Api`, an `OAuth` refusal, and `NoToken` are session-ending; a 5xx/other
    /// `Api` status and everything else are not.
    #[test]
    fn task_error_of_classifies_every_error_kind() {
        let api = |status: u16| Error::Api {
            code: "x".into(),
            message: "m".into(),
            status,
            max_page: None,
        };
        assert_eq!(TaskError::of(&api(401)).kind, TaskErrorKind::Api(401));
        assert_eq!(TaskError::of(&api(403)).kind, TaskErrorKind::Api(403));
        assert_eq!(TaskError::of(&api(500)).kind, TaskErrorKind::Api(500));
        assert_eq!(TaskError::of(&api(200)).kind, TaskErrorKind::Api(200));

        assert_eq!(
            TaskError::of(&Error::OAuth {
                code: "invalid_grant".into(),
                message: "expired".into(),
                status: 400,
            })
            .kind,
            TaskErrorKind::OAuth
        );
        assert_eq!(TaskError::of(&Error::NoToken).kind, TaskErrorKind::NoToken);

        // Transport and every other non-session variant fall into `Other`.
        assert_eq!(TaskError::of(&Error::Config("bad".into())).kind, TaskErrorKind::Other);
        assert_eq!(TaskError::of(&Error::Throttled).kind, TaskErrorKind::Other);

        assert!(TaskErrorKind::Api(401).ends_session());
        // Issue #564: XenForo answers ordinary permission refusals with
        // HTTP 403 too (reacting to your own post, replying to a closed
        // thread, a missing OAuth scope, viewing content you can't) — none
        // of those means the token is bad, so `Api(403)` must NOT be
        // coarse-classified as session-ending; only the bootstrap `/me`
        // path's account-gone codes are (`TaskError::is_account_gone`, used
        // by `session_error_of` for `Msg::Bootstrap` alone).
        assert!(!TaskErrorKind::Api(403).ends_session());
        assert!(TaskErrorKind::OAuth.ends_session());
        assert!(TaskErrorKind::NoToken.ends_session());
        assert!(!TaskErrorKind::Api(500).ends_session(), "a server 5xx must not end the session");
        assert!(!TaskErrorKind::Api(200).ends_session());
        assert!(!TaskErrorKind::Other.ends_session(), "a transport failure must not end the session");

        // Issue #555: a synthetic `"http_error"` code means the body wasn't
        // JSON at all — a Cloudflare 5xx/429/HTML page, not a real OAuth/API
        // rejection — so `TaskError::ends_session()` must override the
        // coarse kind-only classification and keep the session alive.
        let oauth_http_error = TaskError::of(&Error::OAuth {
            code: "http_error".into(),
            message: "<html>bad gateway</html>".into(),
            status: 502,
        });
        assert_eq!(oauth_http_error.kind, TaskErrorKind::OAuth);
        assert!(
            !oauth_http_error.ends_session(),
            "an opaque 502 wrapped as OAuth must not end the session"
        );

        let api_http_error = TaskError::of(&Error::Api {
            code: "http_error".into(),
            message: "<html>forbidden</html>".into(),
            status: 403,
            max_page: None,
        });
        assert_eq!(api_http_error.kind, TaskErrorKind::Api(403));
        assert!(
            !api_http_error.ends_session(),
            "a non-JSON 403 (e.g. a WAF challenge page) must not end the session"
        );

        // A genuine structured rejection still ends the session.
        let real_invalid_grant = TaskError::of(&Error::OAuth {
            code: "invalid_grant".into(),
            message: "expired".into(),
            status: 400,
        });
        assert!(real_invalid_grant.ends_session());
    }

    /// Issue #551: a transient (transport/5xx) bootstrap failure must keep
    /// the Home screen up (no Login pushed), arm the retry, and word the
    /// status as "press r to retry" rather than "log in again" — and that
    /// `r` must then actually re-run the session check.
    #[tokio::test]
    async fn bootstrap_transient_error_keeps_the_session_and_r_retries() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));

        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Err(TaskError {
                message: "http error: connection reset".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Other,
            }),
        });

        assert!(
            !app.screens.iter().any(|s| matches!(s, Screen::Login(_))),
            "a transient failure must not push the login screen"
        );
        assert!(app.bootstrap_retry_needed);
        assert!(
            app.status.contains("press r to retry"),
            "status must offer a retry, not claim the session expired: {:?}",
            app.status
        );
        assert!(!app.status.contains("Session expired"));

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!app.bootstrap_retry_needed, "r must consume the retry flag");
        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("r must re-run the session check")
            .expect("channel open");
        assert!(matches!(sent, Msg::Bootstrap { .. }), "expected a fresh Bootstrap check");
    }

    /// Issue #589: the arm above was written for the startup restore, where
    /// `self.me` is never set — but `recheck_stored_session` (issues
    /// #557/#587) reports through this exact same `Msg::Bootstrap { Err }`
    /// arm from a LIVE session too, whenever its own `/me` call hits a
    /// transient failure. That must not arm `bootstrap_retry_needed`: doing
    /// so would hijack the very next plain `r` — the reply key — into
    /// `restore_session()` instead of opening the composer, over a session
    /// that is actually still fine.
    ///
    /// Issue #592: this arm is also where the poller that first noticed the
    /// `NoToken` (and so triggered the recheck) needs restarting — it
    /// already sent its `SessionLost` and returned, and nothing else would
    /// ever run it again, freezing the Alerts/Inbox badges — and where
    /// `session_recovery_tried` must go back to `false`, or the next sibling
    /// rotation's `NoToken` skips the recheck gate entirely and ends the
    /// session outright with a perfectly good token set on disk.
    #[tokio::test]
    async fn bootstrap_transient_error_in_a_live_session_never_hijacks_r_from_the_composer() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, reply_count: 3, ..Default::default() },
            ..Default::default()
        }));
        app.session_recovery_tried = true;
        assert!(app.poller_handles.is_empty(), "test setup: no pollers running yet");

        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Err(TaskError {
                message: "http error: connection reset".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Other,
            }),
        });

        assert!(
            !app.bootstrap_retry_needed,
            "a transient recheck failure in a live session must not arm the retry hijack"
        );
        assert!(app.me.is_some(), "the session must remain live");
        assert!(
            app.status.contains("still signed in as kemical"),
            "status must hint at the live session, not offer a retry: {:?}",
            app.status
        );
        assert!(!app.status.contains("press r to retry"));
        assert!(
            !app.poller_handles.is_empty(),
            "the live session's pollers must be running again (issue #592)"
        );
        assert!(
            !app.session_recovery_tried,
            "the recheck gate must be re-armed for the next sibling rotation (issue #592)"
        );

        // The key that would have run `restore_session()` must instead reach
        // the ThreadView and open the composer.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(
            matches!(app.screens.last(), Some(Screen::Compose(_))),
            "r in a ThreadView after a transient recheck failure must still open the composer"
        );

        // A later `NodesLoaded(Err(NoToken))` — the next sibling rotation —
        // must start a SECOND recheck rather than fall straight to
        // `end_session` with a perfectly good token set on disk.
        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(
            app.session_recovery_pending,
            "a second recheck must have started instead of ending the session"
        );
        assert!(app.me.is_some(), "the session must still be alive while the second recheck runs");
    }

    /// Issue #551: the converse — a session-ending error (NoToken, OAuth,
    /// 401/403) must still push the Login screen and must never leave the
    /// retry armed (there is no session left to retry restoring).
    #[test]
    fn bootstrap_session_ending_error_still_goes_to_login() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));

        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Err(TaskError {
                message: "not logged in".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::NoToken,
            }),
        });

        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(!app.bootstrap_retry_needed);
        assert!(app.status.contains("Session expired"));
    }

    /// Issue #561: a stale/rotated refresh token now surfaces as
    /// `Error::OAuth{code: "invalid_grant"}` (once `token_request` parses
    /// XenForo's own `{"errors":[...]}` envelope instead of falling back to
    /// the synthetic `"http_error"`) — this is the app-side half: whatever
    /// arrives on `Msg::Bootstrap` with that real code must take the
    /// session-ending path (Login screen up, `me` cleared, no retry armed),
    /// exactly like the pre-existing `NoToken`/401/403 cases already do.
    #[test]
    fn bootstrap_invalid_grant_ends_the_session_and_shows_login() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));
        app.me = Some(User { user_id: 3, username: "stale".into(), ..Default::default() });

        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Err(TaskError::of(&Error::OAuth {
                code: "invalid_grant".into(),
                message: "The provided authorization code or refresh token is invalid.".into(),
                status: 400,
            })),
        });

        assert!(
            matches!(app.screens.last(), Some(Screen::Login(_))),
            "a genuine invalid_grant must end the session"
        );
        assert!(app.me.is_none(), "me must be cleared once the session ends");
        assert!(!app.bootstrap_retry_needed, "there is no session left to retry restoring");
    }

    /// Issue #594: `finish_login` persists the exchanged tokens BEFORE
    /// calling `client.me()` — so a transient failure on that single
    /// verification call (a timeout, a Cloudflare 5xx, an XF 500) must not
    /// force the user through an entirely new browser authorization when the
    /// tokens it just stored already work (quitting and restarting proves it
    /// by going straight through `bootstrap`/`restore_session` silently).
    /// `Msg::LoginComplete { Err }` must pop the Login screen and reuse the
    /// startup transient-retry path instead of leaving "Enter" only able to
    /// start a brand new authorize flow.
    #[tokio::test]
    async fn login_complete_transient_error_reuses_the_bootstrap_retry_path() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));
        app.screens.push(screens::login_state());

        app.handle_msg(Msg::LoginComplete {
            generation: app.login_generation,
            result: Err(TaskError {
                message: "http error: connection reset".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Other,
            }),
        });

        assert!(
            !app.screens.iter().any(|s| matches!(s, Screen::Login(_))),
            "a transient failure right after a successful token exchange must not leave the Login screen up"
        );
        assert!(app.me.is_none());
        assert!(app.bootstrap_retry_needed);

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!app.bootstrap_retry_needed, "r must consume the retry flag");
        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("r must re-run the session check")
            .expect("channel open");
        assert!(matches!(sent, Msg::Bootstrap { .. }), "expected a fresh Bootstrap check");
    }

    /// Issue #594 (converse): a session-ending or account-gone failure right
    /// after the token exchange must still surface on the Login screen — the
    /// tokens `finish_login` stored are not usable, so there is nothing to
    /// silently retry.
    #[tokio::test]
    async fn login_complete_session_ending_error_still_shows_on_the_login_screen() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));
        app.screens.push(screens::login_state());

        app.handle_msg(Msg::LoginComplete {
            generation: app.login_generation,
            result: Err(TaskError::of(&Error::OAuth {
                code: "invalid_grant".into(),
                message: "The provided authorization code or refresh token is invalid.".into(),
                status: 400,
            })),
        });

        assert!(!app.bootstrap_retry_needed);
        let Some(Screen::Login(ls)) = app.screens.last() else {
            panic!("a session-ending failure must still leave the Login screen up");
        };
        assert!(!ls.busy);
        assert!(ls.error.is_some());
    }

    /// Issue #561: the converse of the test above — a genuinely transient
    /// bootstrap failure (a Cloudflare 5xx/gateway page, wrapped by
    /// `token_request`'s http-error fallback as `Error::OAuth{code:
    /// "http_error"}` with the whole raw body as the message) must neither
    /// end the session nor leave the Forums panel spinning "Loading
    /// forums…" forever, and the status line must keep "press r to retry"
    /// on screen rather than pushed off by an uncapped raw body.
    #[test]
    fn bootstrap_transient_http_error_shows_retry_in_forums_panel_and_status() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));

        // A realistic Cloudflare gateway page — much longer than any status
        // row, which is exactly what the operator hit (issue #561).
        let raw_body = "<html><head><title>502 Bad Gateway</title></head><body><center>\
            <h1>502 Bad Gateway</h1></center><hr><center>cloudflare</center></body></html>"
            .repeat(2);
        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Err(TaskError::of(&Error::OAuth {
                code: "http_error".into(),
                message: raw_body,
                status: 502,
            })),
        });

        assert!(
            !app.screens.iter().any(|s| matches!(s, Screen::Login(_))),
            "a transient failure must not push the login screen"
        );
        assert!(app.bootstrap_retry_needed);
        assert!(!app.status.contains('\n'), "status must be one line: {:?}", app.status);
        assert!(app.status.contains("press r to retry"), "{:?}", app.status);
        assert!(
            app.status.chars().count() < 100,
            "status must comfortably fit a 120-column terminal alongside the \
             write-gate widget: {} chars: {:?}",
            app.status.chars().count(),
            app.status
        );

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            let screen = app.screens.last_mut().expect("screen");
            screen.render(f, area, &app.theme, &app.glyphs, &mut crate::hit::HitMap::default());
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        let screen_text: String = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        assert!(
            !screen_text.contains("Loading forums"),
            "the spinner must not still be showing:\n{screen_text}"
        );
        assert!(
            screen_text.contains("Press r to retry."),
            "the Forums panel must show the retry hint instead of spinning forever:\n{screen_text}"
        );
    }

    /// Issue #557: a refresh rejected with `invalid_grant` mid-session (XF
    /// rotated the refresh token under a second instance, or the 90-day
    /// grant simply ran out) used to leave a zombie client — `me` still set,
    /// pollers silent, every panel saying "not logged in". Whichever message
    /// carries the failure, the pump's session boundary must end the session
    /// once: pollers stopped, `me` cleared, the stack back to Home + Login.
    #[tokio::test]
    async fn in_session_invalid_grant_ends_the_session_from_any_message() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.keep_thread_position = Some((42, 1, 0, 0));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            ..Default::default()
        }));
        app.start_pollers();
        assert_eq!(app.poller_handles.len(), 2);
        let generation_before = app.bootstrap_generation;

        // The failure arrives on an ordinary content load, not on Bootstrap.
        app.handle_msg(Msg::ThreadLoaded {
            id: 42,
            page: 1,
            result: Err(TaskError {
                message: "oauth error [invalid_grant]".into(),
                code: Some("invalid_grant".into()),
                max_page: None,
                kind: TaskErrorKind::OAuth,
            }),
        });

        assert!(app.me.is_none(), "the header must stop showing a signed-out user");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(
            !app.screens.iter().any(|s| matches!(s, Screen::ThreadView(_))),
            "the stack must be truncated back to Home"
        );
        assert!(app.keep_thread_position.is_none());
        assert!(app.poller_handles.is_empty(), "the pollers must be stopped");
        assert!(app.status.contains("Session expired"), "status: {:?}", app.status);
        assert_ne!(
            app.bootstrap_generation, generation_before,
            "ending a session must invalidate any restore already in flight"
        );

        // A poller reporting the same failure (issue #557: it used to drop
        // the error) routes to exactly the same place — once the recheck
        // gate is exhausted. Issue #593: a bare `Api(401)` (XenForo revokes
        // the OLD access token in the same request that serves a sibling's
        // refresh, so a rotation can surface as 401 just as easily as
        // `NoToken`) now gets exactly one recheck first, same as `NoToken` —
        // see `a_live_session_401_starts_a_recheck_instead_of_ending_the_session`
        // for that case. This exercises what happens after that recheck has
        // already run once (`session_recovery_tried` already true): a second
        // 401 still ends the session rather than looping forever.
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.session_recovery_tried = true;
        app.handle_msg(Msg::SessionLost(TaskError {
            message: "unauthorized".into(),
            code: Some("api_error".into()),
            max_page: None,
            kind: TaskErrorKind::Api(401),
        }));
        assert!(app.me.is_none());
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
    }

    /// Issue #557: `Ctrl+L` while "Restoring session…" is still in flight
    /// erased the token store and then let the late `Msg::Bootstrap(Ok)`
    /// sign the user back in — pollers and all — against no tokens at all.
    /// The generation stamp must drop that answer.
    #[tokio::test]
    async fn logout_during_restore_drops_the_late_bootstrap() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));
        app.restore_session();
        let in_flight = app.bootstrap_generation;

        app.logout();
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert_eq!(app.status, "Logged out.");

        // The restore that was already running finally answers.
        app.handle_msg(Msg::Bootstrap {
            generation: in_flight,
            result: Ok(User { user_id: 7, username: "kemical".into(), ..Default::default() }),
        });

        assert!(app.me.is_none(), "a superseded restore must not sign anyone back in");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(app.poller_handles.is_empty(), "no pollers may be started for a dead session");
        assert_eq!(app.status, "Logged out.");

        // The restore started *after* the logout is still the live one.
        app.restore_session();
        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Ok(User { user_id: 7, username: "kemical".into(), ..Default::default() }),
        });
        assert!(app.me.is_some(), "the current generation's answer must still land");
    }

    /// Issue #574: each `/api/oauth2/revoke` round-trip inside `logout()` can
    /// take up to CONNECT 10s + REQUEST 30s. The old order only took the
    /// session out of memory and erased the store *after* both had returned,
    /// so a sign-in completed inside that window (`finish_login`'s
    /// `set_tokens()`) landed its fresh tokens first, and the late,
    /// unconditional `forget_tokens()` then deleted the NEW session it had
    /// nothing to do with. `logout()` must snapshot-and-forget before either
    /// revoke call, not after, so a `set_tokens()` that lands while the
    /// (mocked, deliberately slow) revoke calls are still in flight survives
    /// untouched — in memory and on disk.
    #[tokio::test]
    async fn set_tokens_after_logouts_snapshot_survives_a_slow_revoke() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/oauth2/revoke"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let store_path = scratch_config_dir().join("token-race-574.json");
        let _ = std::fs::remove_file(&store_path);
        let store = common::token::Store::with_path(store_path.clone());
        let client = Arc::new(WfApiClient::with_store(store, server.uri()).expect("client init"));
        client
            .set_tokens(common::token::TokenSet {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .await
            .unwrap();

        let mut app = test_app();
        app.client = client.clone();
        app.screens.push(screens::home_state(true));

        app.logout();

        // Long enough for the spawned task to pass its snapshot-and-forget
        // step (near-instant: no network call happens before it) but well
        // short of either mocked revoke's 300ms delay.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A sign-in lands here — exactly what `finish_login` does.
        client
            .set_tokens(common::token::TokenSet {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .await
            .unwrap();

        // Let both revoke calls finish and `Msg::LoggedOut` land.
        let sent = tokio::time::timeout(Duration::from_secs(2), app.rx.recv())
            .await
            .expect("logout must report back")
            .expect("channel open");
        assert!(matches!(sent, Msg::LoggedOut(_)), "expected LoggedOut");

        let live = client.token_set().await.expect("the new session must survive in memory");
        assert_eq!(live.access_token, "new-access");

        let on_disk = common::token::Store::with_path(store_path)
            .load()
            .unwrap()
            .expect("the new session must survive on disk");
        assert_eq!(on_disk.refresh_token, "new-refresh");
    }

    /// Issue #568: while the store recheck is in flight, every other caller
    /// that was queued behind the refresh which just failed reports the same
    /// `invalid_grant`. Those describe the grant the recheck is already
    /// replacing, so the boundary must treat them as stale: one arriving
    /// mid-recovery used to end the session anyway, undoing the recovery a
    /// sibling-rotated `token.json` was about to provide. The recheck's own
    /// outcome still decides — including when it reports the session lost.
    #[tokio::test]
    async fn an_oauth_error_arriving_mid_recheck_is_stale_and_the_recheck_still_decides() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });

        // A NoToken opens the recovery window.
        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");

        // A queued caller now reports the refresh rejection that started all
        // of this. It must not end the session.
        app.handle_msg(Msg::ThreadLoaded {
            id: 42,
            page: 1,
            result: Err(TaskError {
                message: "oauth error [invalid_grant]".into(),
                code: Some("invalid_grant".into()),
                max_page: None,
                kind: TaskErrorKind::OAuth,
            }),
        });
        assert!(
            app.me.is_some(),
            "a rejection of the grant being replaced must not end the session"
        );
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));

        // The recheck answers: nothing on disk to adopt, so the session ends
        // here — the window must not have swallowed its verdict too. Issue
        // #587: the recheck reports this through `Bootstrap { Err(NoToken) }`,
        // never `SessionLost` — that message shape is reserved for pollers.
        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the recheck must report back")
            .expect("channel open");
        assert!(
            matches!(sent, Msg::Bootstrap { result: Err(_), .. }),
            "expected Bootstrap {{ Err }}"
        );
        app.handle_msg(sent);
        assert!(app.me.is_none(), "the recheck's own verdict still ends the session");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(!app.session_recovery_pending);

        // Outside that window an OAuth rejection ends the session as before.
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.handle_msg(Msg::ThreadLoaded {
            id: 42,
            page: 1,
            result: Err(TaskError {
                message: "oauth error [invalid_grant]".into(),
                code: Some("invalid_grant".into()),
                max_page: None,
                kind: TaskErrorKind::OAuth,
            }),
        });
        assert!(app.me.is_none());
        assert!(app.status.contains("Session expired"), "status: {:?}", app.status);
    }

    /// Issue #593: XenForo revokes the OLD access token in the SAME request
    /// that serves a sibling instance's refresh, so a request already in
    /// flight (or sent during the clock-skew window before this instance's
    /// own margin trips) can see that rotation as `Api(401)
    /// api_error.unauthorized` instead of the `NoToken`/`OAuth` shapes the
    /// recovery machinery already knew about. A live session must treat it
    /// exactly the same way: start the recheck rather than sign out
    /// immediately.
    #[tokio::test]
    async fn a_live_session_401_starts_a_recheck_instead_of_ending_the_session() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });

        app.handle_msg(Msg::SessionLost(TaskError {
            message: "Unauthorized".into(),
            code: Some("api_error.unauthorized".into()),
            max_page: None,
            kind: TaskErrorKind::Api(401),
        }));

        assert!(
            app.session_recovery_tried,
            "an Api(401) in a live session must start the store recheck"
        );
        assert!(app.session_recovery_pending, "the recheck must be in flight");
        assert!(app.me.is_some(), "the session must not be ended before the recheck reports back");
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
    }

    /// Round 7 regression, issue #580: `valid_token()` clearing the
    /// in-memory guard on a rejected refresh (common/src/api.rs) means every
    /// OTHER caller queued behind that same refresh reports `NoToken`, not
    /// just `OAuth` — the 45 s alerts poller and 90 s conversations poller
    /// fire together, so two `NoToken`s land back-to-back while the first
    /// one's recheck is still in flight. The old code only swallowed
    /// `OAuth` in that window, so the second `NoToken` ran `end_session()`
    /// anyway: it bumped `bootstrap_generation`, so the recheck's own
    /// `Bootstrap { Ok }` (a sibling's rotated-but-valid token) then arrived
    /// under the OLD generation and was silently dropped, leaving the
    /// client holding a live adopted token in memory under a Login screen.
    #[tokio::test]
    async fn a_second_no_token_mid_recheck_does_not_end_the_session_and_the_recheck_still_wins() {
        // A client of our own (not `test_app()`'s shared scratch store) so
        // we can plant a sibling-rotated token set on disk without racing
        // any other test that also touches `offline_client()`'s file.
        let store_path = scratch_config_dir().join("token-580.json");
        let _ = std::fs::remove_file(&store_path);
        let store = common::token::Store::with_path(store_path.clone());
        let client = Arc::new(WfApiClient::with_store(store, OFFLINE_BASE).expect("client init"));
        client
            .set_tokens(common::token::TokenSet {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .await
            .unwrap();
        // A sibling instance rotated the refresh token on disk without this
        // process's knowledge — written straight to the store, bypassing
        // `client`, exactly like a second `wftui` sharing the config dir.
        common::token::Store::with_path(store_path)
            .save(&common::token::TokenSet {
                access_token: "sibling-access".into(),
                refresh_token: "sibling-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();

        let expected_user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.client = client;
        app.api = Arc::new(RecordingApi {
            me_ok: std::sync::Mutex::new(Some(expected_user.clone())),
            ..Default::default()
        });
        app.me = Some(expected_user.clone());
        app.screens.push(screens::home_state(false));
        let generation_before = app.bootstrap_generation;

        // Two `NoToken`s, back-to-back, with nothing drained from `rx` in
        // between — exactly the alerts-poller-then-convos-poller race.
        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");
        app.handle_msg(Msg::AlertsLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));

        assert!(app.me.is_some(), "a second NoToken mid-recheck must not end the session");
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert_eq!(
            app.bootstrap_generation, generation_before,
            "the second NoToken must not bump the generation the recheck is answering under"
        );

        // Drain rx: the recheck adopted the sibling's token and its `/me`
        // (stubbed to succeed) reports the same identity back.
        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the recheck must report back")
            .expect("channel open");
        assert!(matches!(sent, Msg::Bootstrap { result: Ok(_), .. }), "expected Bootstrap {{ Ok }}");
        app.handle_msg(sent);

        assert_eq!(app.me.as_ref().map(|u| u.user_id), Some(7), "the recheck's Ok must be honoured");
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(
            !(app.client.has_tokens().await && matches!(app.screens.last(), Some(Screen::Login(_)))),
            "must never hold a live adopted token under a Login screen"
        );
    }

    /// Poller-shaped twin of the test above, issue #587: round 8 only
    /// exercised the swallow with `Msg::NodesLoaded`/`Msg::AlertsLoaded`
    /// errors, but the alerts and conversations pollers never send those —
    /// they send `Msg::SessionLost` directly (`app.rs`'s poller loops). That
    /// message shape used to be exactly what closed the recovery window
    /// (the old boundary cleared `session_recovery_pending` on `Bootstrap`
    /// *or* `SessionLost`), so a second poller's `SessionLost(NoToken)`
    /// arriving mid-recheck fell straight through to `end_session()` and the
    /// recheck's own `Bootstrap { Ok }` then landed under the stale
    /// generation and was dropped — the client held a live adopted token
    /// under a Login screen. Fixed by only ever clearing the window on the
    /// recheck's own `Msg::Bootstrap`.
    #[tokio::test]
    async fn two_poller_session_lost_no_token_messages_do_not_end_the_session_and_the_recheck_still_wins()
     {
        let store_path = scratch_config_dir().join("token-587-pollers.json");
        let _ = std::fs::remove_file(&store_path);
        let store = common::token::Store::with_path(store_path.clone());
        let client = Arc::new(WfApiClient::with_store(store, OFFLINE_BASE).expect("client init"));
        client
            .set_tokens(common::token::TokenSet {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .await
            .unwrap();
        // A sibling instance rotated the refresh token on disk without this
        // process's knowledge.
        common::token::Store::with_path(store_path)
            .save(&common::token::TokenSet {
                access_token: "sibling-access".into(),
                refresh_token: "sibling-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();

        let expected_user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.client = client;
        app.api = Arc::new(RecordingApi {
            me_ok: std::sync::Mutex::new(Some(expected_user.clone())),
            ..Default::default()
        });
        app.me = Some(expected_user.clone());
        app.screens.push(screens::home_state(false));
        let generation_before = app.bootstrap_generation;

        // The alerts poller (45 s) notices first: `Msg::SessionLost`, exactly
        // as `app.rs`'s poller loops send it — not `NodesLoaded`.
        app.handle_msg(Msg::SessionLost(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        }));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");

        // The conversations poller (90 s) coincides and reports the same
        // rejection the same way.
        app.handle_msg(Msg::SessionLost(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        }));

        assert!(
            app.me.is_some(),
            "a second poller's SessionLost(NoToken) mid-recheck must not end the session"
        );
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert_eq!(
            app.bootstrap_generation, generation_before,
            "the second poller's report must not bump the generation the recheck is answering under"
        );

        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the recheck must report back")
            .expect("channel open");
        assert!(matches!(sent, Msg::Bootstrap { result: Ok(_), .. }), "expected Bootstrap {{ Ok }}");
        app.handle_msg(sent);

        assert_eq!(app.me.as_ref().map(|u| u.user_id), Some(7), "the recheck's Ok must be honoured");
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(
            !(app.client.has_tokens().await && matches!(app.screens.last(), Some(Screen::Login(_)))),
            "must never hold a live adopted token under a Login screen"
        );
    }

    /// Same race, but the second poller's report is an `OAuth` rejection
    /// rather than `NoToken` — the shape a poller reports when its own
    /// refresh attempt (rather than a queued caller behind someone else's)
    /// is the one XF rejects. Must be swallowed exactly like the NoToken
    /// case above (issue #587).
    #[tokio::test]
    async fn a_poller_session_lost_oauth_message_mid_recheck_is_swallowed_and_the_recheck_still_wins()
     {
        let store_path = scratch_config_dir().join("token-587-oauth-poller.json");
        let _ = std::fs::remove_file(&store_path);
        let store = common::token::Store::with_path(store_path.clone());
        let client = Arc::new(WfApiClient::with_store(store, OFFLINE_BASE).expect("client init"));
        client
            .set_tokens(common::token::TokenSet {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .await
            .unwrap();
        common::token::Store::with_path(store_path)
            .save(&common::token::TokenSet {
                access_token: "sibling-access".into(),
                refresh_token: "sibling-refresh".into(),
                expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
                scope: "test".into(),
            })
            .unwrap();

        let expected_user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.client = client;
        app.api = Arc::new(RecordingApi {
            me_ok: std::sync::Mutex::new(Some(expected_user.clone())),
            ..Default::default()
        });
        app.me = Some(expected_user.clone());
        app.screens.push(screens::home_state(false));
        let generation_before = app.bootstrap_generation;

        app.handle_msg(Msg::SessionLost(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        }));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");

        app.handle_msg(Msg::SessionLost(TaskError {
            message: "oauth error [invalid_grant]".into(),
            code: Some("invalid_grant".into()),
            max_page: None,
            kind: TaskErrorKind::OAuth,
        }));

        assert!(
            app.me.is_some(),
            "a poller's SessionLost(OAuth) mid-recheck must not end the session"
        );
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert_eq!(app.bootstrap_generation, generation_before);

        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the recheck must report back")
            .expect("channel open");
        assert!(matches!(sent, Msg::Bootstrap { result: Ok(_), .. }), "expected Bootstrap {{ Ok }}");
        app.handle_msg(sent);

        assert_eq!(app.me.as_ref().map(|u| u.user_id), Some(7), "the recheck's Ok must be honoured");
        assert!(
            !(app.client.has_tokens().await && matches!(app.screens.last(), Some(Screen::Login(_)))),
            "must never hold a live adopted token under a Login screen"
        );
    }

    /// Issue #588: the session boundary used to `return` without dispatching
    /// the message it consumed — both when it starts the recheck and when it
    /// swallows a stale rejection inside the recovery window. For a write,
    /// the `Err` arm it skipped is the ONLY place `ComposeState::busy` is
    /// cleared, so the composer sat on "Sending…" forever after "Session
    /// restored.": every key was ignored and Esc answered "wait for the
    /// result" for a result that could never arrive. The message must reach
    /// the composer as a retryable failure instead — draft intact, Esc
    /// working, ^S available again.
    #[tokio::test]
    async fn a_write_consumed_by_the_session_boundary_still_frees_the_composer() {
        let user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.me = Some(user.clone());
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 42,
                thread_title: "a thread".into(),
            }),
            body: "a draft nobody may lose".into(),
            busy: true,
            ..Default::default()
        }));

        // The write's own `valid_token()` was the first caller to hit the
        // sibling-rotated refresh token: `NoToken`, which opens the recheck.
        app.handle_msg(Msg::ReplySent(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");

        let Some(Screen::Compose(compose)) = app.screens.last() else {
            panic!("the composer must still be open");
        };
        assert!(!compose.busy, "the composer must not be left on \"Sending…\"");
        assert_eq!(compose.body, "a draft nobody may lose", "the draft must survive");
        assert_eq!(compose.error.as_deref(), Some(SESSION_RECHECK_RETRY_MSG));
        assert!(
            matches!(app.screens.last().expect("a screen").esc_intent(), screens::EscIntent::Screen),
            "Esc must be able to leave the composer again"
        );

        // The recheck adopts the sibling's token and reports the SAME
        // identity back: the session is restored and the composer stays
        // usable — no stuck busy flag to clear later.
        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Ok(user),
        });
        assert_eq!(app.status, "Session restored.");
        let Some(Screen::Compose(compose)) = app.screens.last() else {
            panic!("the composer must survive a same-identity restore");
        };
        assert!(!compose.busy);
        assert_eq!(compose.body, "a draft nobody may lose");
        assert!(matches!(
            app.screens.last().expect("a screen").esc_intent(),
            screens::EscIntent::Screen
        ));
    }

    /// The load-shaped twin of the test above (issue #588): a swallowed
    /// `ForumLoaded` left Home's list `loading`, and `prime_home_list`
    /// deliberately skips a list that is already loading — so after "Session
    /// restored." the "Latest posts" pane sat on the spinner with no reload
    /// coming until the user navigated away and back.
    #[tokio::test]
    async fn a_list_load_consumed_by_the_session_boundary_reloads_after_the_recheck() {
        let user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.me = Some(user.clone());
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.first_mut() {
            h.list.title = "Latest posts".into();
            h.list.loading = true;
        }

        app.handle_msg(Msg::ForumLoaded {
            node_id: 0,
            page: 1,
            append: false,
            seq: 0,
            result: Err(TaskError {
                message: "not logged in".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::NoToken,
            }),
        });
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");
        let Some(Screen::Home(h)) = app.screens.first() else { panic!("expected Home") };
        assert!(!h.list.loading, "a consumed load must still clear the list's spinner");
        assert_eq!(h.list.error.as_deref(), Some(SESSION_RECHECK_RETRY_MSG));

        app.handle_msg(Msg::Bootstrap {
            generation: app.bootstrap_generation,
            result: Ok(user),
        });
        let Some(Screen::Home(h)) = app.screens.first() else { panic!("expected Home") };
        assert!(
            h.list.loading,
            "the restored session must re-prime the Home list, not leave it empty"
        );
    }

    /// Issue #587/#591: if the recheck task's own `Msg::Bootstrap` report is
    /// somehow never delivered at all (the `ReportGuard` in
    /// `recheck_stored_session` covers a plain panic/drop — this exercises
    /// the pure backstop for when even that never lands), nothing would
    /// otherwise clear `session_recovery_pending` — leaving every poller's
    /// `SessionLost` swallowed as "stale" forever. The event loop's tick
    /// bounds the window instead, but it must NOT end the session on its own
    /// (issue #591): `recheck_stored_session` is only ever started with a
    /// live session, so the timer firing means a token is still adopted and
    /// in use, not that the session is over — it falls back to the same
    /// "still signed in" hint the transient-failure arm uses. Simulated
    /// clock: the window's start is backdated past
    /// `SESSION_RECOVERY_TIMEOUT_SECS` rather than actually sleeping.
    #[test]
    fn a_session_recovery_window_left_open_past_the_timeout_backstop_keeps_the_live_session() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        app.session_recovery_tried = true;
        app.session_recovery_pending = true;
        app.session_recovery_started_at = Some(std::time::Instant::now());

        // Not old enough yet: a fresh window must survive a tick.
        app.expire_session_recovery_timeout();
        assert!(app.me.is_some(), "must not touch the session before the timeout elapses");
        assert!(app.session_recovery_pending);

        // Backdate past the timeout — nothing ever reported back.
        app.session_recovery_started_at =
            Some(std::time::Instant::now() - Duration::from_secs(SESSION_RECOVERY_TIMEOUT_SECS + 1));
        app.expire_session_recovery_timeout();

        assert!(app.me.is_some(), "a live session must survive the timeout backstop");
        assert!(!matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(!app.session_recovery_pending);
        assert!(app.session_recovery_started_at.is_none());
        assert!(app.status.contains("still signed in"), "status: {:?}", app.status);
    }

    /// Issue #591: `SESSION_RECOVERY_TIMEOUT_SECS` used to be 15 s — shorter
    /// than the recheck's own legitimate network budget (`CONNECT_TIMEOUT`
    /// 10 s + `REQUEST_TIMEOUT` 30 s, possibly doubled by a token refresh
    /// inside `valid_token()` before the `/me` call itself, plus any queued
    /// `api_gate` rate-limit penalty) — so a recheck that was merely slow,
    /// not dead, still got timed out from under a perfectly good session:
    /// the Compose draft the boundary had just preserved was dropped, and
    /// the recheck's later `Msg::Bootstrap { Ok }` arrived under a
    /// now-stale generation and was discarded. A 16 s-old window — longer
    /// than the old 15 s timeout — must survive a tick untouched under the
    /// new backstop, and the recheck's own verdict must still land and
    /// restore the session with the Compose screen intact.
    #[tokio::test]
    async fn a_recheck_slower_than_the_old_15s_timeout_still_restores_the_session() {
        let user = User { user_id: 7, username: "kemical".into(), ..Default::default() };
        let mut app = test_app();
        app.me = Some(user.clone());
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 42,
                thread_title: "a thread".into(),
            }),
            body: "a draft nobody may lose".into(),
            busy: true,
            ..Default::default()
        }));

        // The write's own `valid_token()` was the first caller to hit the
        // sibling-rotated refresh token: `NoToken`, which opens the recheck.
        app.handle_msg(Msg::ReplySent(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(app.session_recovery_pending, "test setup: the recheck must be in flight");

        // Backdate the window by 16 s — longer than the OLD 15 s timeout,
        // well inside the new backstop — and let a tick run.
        app.session_recovery_started_at = Some(std::time::Instant::now() - Duration::from_secs(16));
        app.expire_session_recovery_timeout();
        assert!(
            app.session_recovery_pending,
            "16 s must not trip the new timeout backstop"
        );
        assert!(app.me.is_some(), "a merely slow recheck must not end the session early");

        // The recheck's own (slow but successful) verdict still arrives
        // under the same generation.
        app.handle_msg(Msg::Bootstrap { generation: app.bootstrap_generation, result: Ok(user) });

        assert!(app.me.is_some(), "the session must survive a slow-but-successful recheck");
        let Some(Screen::Compose(compose)) = app.screens.last() else {
            panic!("the Compose screen must survive a slow-but-successful recheck");
        };
        assert_eq!(compose.body, "a draft nobody may lose");
    }

    /// Issue #581: the token recheck's `Bootstrap { Ok(user) }` used to be
    /// adopted unconditionally — "Session restored." — even when `user_id`
    /// differs from the identity the session was running as, meaning the
    /// stored session on disk now belongs to a DIFFERENT account. Anything
    /// screen- or write-shaped that belonged to the old identity (an open
    /// screen, a write still waiting on the politeness gates) must not
    /// carry over, and the user must be told they are now signed in as
    /// someone else.
    #[tokio::test]
    async fn bootstrap_ok_with_a_different_user_tears_down_the_old_identity() {
        let api = Arc::new(RecordingApi {
            write_delay: Duration::from_millis(200),
            ..Default::default()
        });
        let mut app = test_app();
        app.api = api.clone();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));
        app.screens.push(screens::search_state());
        app.keep_thread_position = Some((42, 1, 0, 0));
        app.open_palette();
        assert!(app.palette.is_some(), "test setup: the palette must actually be open");
        // Issue #590: the old identity's unread badges and its
        // already-loaded Home thread list must not survive under the new
        // username either.
        app.alerts_unread = 5;
        app.convos_unread = 3;
        if let Some(Screen::Home(h)) = app.screens.first_mut() {
            h.list.node_id = 4; // a forum the old identity had open
            h.list.title = "News".into();
            h.list.threads = vec![Thread { thread_id: 1, title: "old identity's thread".into(), ..Default::default() }];
        }

        app.execute_action(Action::SubmitReply {
            thread_id: 42,
            message: "a draft written as the old identity".into(),
        });
        // Let the write start and park in the gate wait, exactly like
        // `a_reply_abandoned_by_ctrl_l_is_never_sent`.
        tokio::task::yield_now().await;
        assert!(api.replies().is_empty(), "test setup: the write must still be gated");

        let generation = app.bootstrap_generation;
        app.handle_msg(Msg::Bootstrap {
            generation,
            result: Ok(User { user_id: 99, username: "someone-else".into(), ..Default::default() }),
        });

        assert_eq!(app.me.as_ref().map(|u| u.user_id), Some(99));
        assert_eq!(
            app.screens.len(),
            1,
            "the old screen stack must be truncated to Home/ForumTree, not carried over"
        );
        assert!(matches!(app.screens.last(), Some(Screen::Home(_))));
        assert!(app.keep_thread_position.is_none(), "a stale thread position must not survive");
        assert!(app.palette.is_none(), "an open palette must not survive an identity change");
        assert!(
            app.status.contains("someone-else"),
            "the hint must name the new identity: {:?}",
            app.status
        );
        assert_ne!(app.status, "Session restored.");
        assert_eq!(app.alerts_unread, 0, "the old identity's alert badge must not survive");
        assert_eq!(app.convos_unread, 0, "the old identity's inbox badge must not survive");
        let Some(Screen::Home(h)) = app.screens.first() else {
            panic!("expected a Home screen at the base of the stack");
        };
        assert!(
            h.list.threads.is_empty(),
            "the old identity's Home list must not survive: {:?}",
            h.list.threads.iter().map(|t| &t.title).collect::<Vec<_>>()
        );
        assert_eq!(
            h.list.node_id, 0,
            "the Home list must be back to the priming state so prime_home_list reloads it"
        );

        // Well past the gate the old identity's write was waiting on.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            api.replies().is_empty(),
            "a write started under the old identity must never reach the API: {:?}",
            api.replies()
        );
    }

    /// Issue #600: unlike the #590 identity-change teardown above,
    /// `end_session` used to leave Home's list/focus untouched — the next
    /// sign-in (same account or a different one) inherited the previous
    /// session's forum list, unread marks and pane focus. `end_session` must
    /// reset it exactly like the identity-change path does, and a sign-in
    /// right after must re-prime it.
    #[tokio::test]
    async fn end_session_resets_homes_list_like_the_identity_change_teardown() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.first_mut() {
            h.list.node_id = 4; // a forum the old session had open
            h.list.title = "News".into();
            h.list.threads =
                vec![Thread { thread_id: 1, title: "old session's thread".into(), ..Default::default() }];
            h.focus = screens::Pane::List;
        }

        app.end_session("Session expired; log in again.");

        {
            let Some(Screen::Home(h)) = app.screens.first() else {
                panic!("expected the retained Home screen");
            };
            assert!(
                h.list.threads.is_empty(),
                "the old session's Home list must not survive end_session: {:?}",
                h.list.threads.iter().map(|t| &t.title).collect::<Vec<_>>()
            );
            assert_eq!(
                h.list.node_id, 0,
                "must be back to the priming state so prime_home_list reloads it"
            );
            assert!(
                matches!(h.focus, screens::Pane::Tree),
                "focus must not stay pinned on the old list pane"
            );
        }

        // The next sign-in must actually reload Latest into the reset pane,
        // not sit on the (now-empty) old state forever.
        app.handle_msg(Msg::LoginComplete {
            generation: app.login_generation,
            result: Ok(User { user_id: 99, username: "someone-else".into(), ..Default::default() }),
        });
        let Some(Screen::Home(h)) = app.screens.first() else {
            panic!("expected the retained Home screen");
        };
        assert!(h.list.loading, "the next sign-in must have re-primed Home's list");
    }

    /// Issue #600 (boundary half): a load already in flight when the session
    /// ended must still reach its own `Err` arm so `loading` clears, rather
    /// than being swallowed whole by the boundary's `end_session; return`
    /// branch — `end_session` cannot reset every retained screen's every
    /// loading flag (here, `ForumTreeState::loading`, which the #600 Home
    /// list reset above deliberately leaves alone).
    #[test]
    fn boundary_end_session_branch_lets_a_retained_screens_spinner_clear() {
        let mut app = test_app();
        app.me = None; // already signed out by the time this stale reply lands
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.first_mut() {
            h.tree.loading = true;
        }

        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));

        let Some(Screen::Home(h)) = app.screens.first() else {
            panic!("expected the retained Home screen");
        };
        assert!(
            !h.tree.loading,
            "the boundary's end_session branch must mark the message retryable and let it \
             fall through, not strand a retained screen's spinner"
        );
    }

    /// Same-user path: a token recheck that simply confirms the account
    /// already signed in must behave exactly as before — no teardown, the
    /// familiar "Session restored." status.
    #[tokio::test]
    async fn bootstrap_ok_with_the_same_user_is_unchanged() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));
        app.screens.push(screens::search_state());
        app.keep_thread_position = Some((42, 1, 0, 0));

        let generation = app.bootstrap_generation;
        app.handle_msg(Msg::Bootstrap {
            generation,
            result: Ok(User { user_id: 7, username: "kemical".into(), ..Default::default() }),
        });

        assert_eq!(app.me.as_ref().map(|u| u.user_id), Some(7));
        assert_eq!(app.screens.len(), 2, "the same-user path must not touch the screen stack");
        assert!(app.keep_thread_position.is_some(), "same-user path must not clear this either");
        assert_eq!(app.status, "Session restored.");
    }

    /// Issue #557: `Error::NoToken` in a live session may only mean a second
    /// instance sharing `token.json` rotated the refresh token, so the store
    /// is re-read once before the session ends. With nothing new on disk the
    /// recheck reports back and the session ends exactly as before.
    #[tokio::test]
    async fn no_token_mid_session_rechecks_the_store_once_before_ending() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });

        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));

        assert!(app.me.is_some(), "the session survives until the store has been re-read");
        assert!(app.session_recovery_tried);
        assert!(app.status.contains("re-checking"), "status: {:?}", app.status);

        let sent = tokio::time::timeout(std::time::Duration::from_secs(1), app.rx.recv())
            .await
            .expect("the recheck must report back")
            .expect("channel open");
        // Nothing new on disk (this process already holds whatever is there),
        // so the recheck reports `Bootstrap { Err(NoToken) }` rather than a
        // fresh `me()` result — never `SessionLost` (issue #587).
        assert!(
            matches!(sent, Msg::Bootstrap { result: Err(_), .. }),
            "expected Bootstrap {{ Err }}"
        );
        app.handle_msg(sent);
        assert!(app.me.is_none());
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));

        // And the recheck is one-shot: the next NoToken ends the session
        // straight away instead of looping on the store.
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.session_recovery_tried = true;
        app.handle_msg(Msg::NodesLoaded(Err(TaskError {
            message: "not logged in".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::NoToken,
        })));
        assert!(app.me.is_none());
        assert!(app.status.contains("Session expired"));
    }

    /// Issue #564: XenForo answers ordinary permission refusals with HTTP
    /// 403 too (liking your own post, replying to a closed thread, viewing a
    /// forum you can't) — none of those means the token is bad, so unlike
    /// `Api(401)` an `Api(403)` on an ordinary content call must NOT end the
    /// session. `PostToggled`/`ReplySent`/`ThreadLoaded` must each route the
    /// failure to their own handler (status line / view/compose error)
    /// instead, leaving `me`, the pollers and a busy `Compose` screen alone.
    #[tokio::test]
    async fn permission_refusal_403s_on_ordinary_calls_do_not_end_the_session() {
        let phrase_error = |code: &str| TaskError {
            message: "some phrase text".into(),
            code: Some(code.into()),
            max_page: None,
            kind: TaskErrorKind::Api(403),
        };

        // PostToggled: reacting to your own content.
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.start_pollers();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            ..Default::default()
        }));
        app.handle_msg(Msg::PostToggled {
            verb: PostVerb::Like,
            result: Err(phrase_error("reacting_to_your_own_content_is_considered_cheating")),
        });
        assert!(app.me.is_some(), "a 403 permission refusal must not sign the user out");
        assert!(!app.poller_handles.is_empty(), "the pollers must keep running");
        assert!(
            matches!(app.screens.last(), Some(Screen::ThreadView(_))),
            "the thread view must not be replaced by Login"
        );
        assert!(app.status.contains("Like failed"), "status: {:?}", app.status);

        // ReplySent: replying to a closed thread, with a busy Compose whose
        // draft must survive.
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.start_pollers();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply { thread_id: 1, thread_title: "A thread".into() }),
            body: "my draft".into(),
            busy: true,
            ..Default::default()
        }));
        app.handle_msg(Msg::ReplySent(Err(phrase_error(
            "you_may_not_perform_this_action_because_discussion_is_closed",
        ))));
        assert!(app.me.is_some(), "a 403 permission refusal must not sign the user out");
        assert!(!app.poller_handles.is_empty(), "the pollers must keep running");
        match app.screens.last() {
            Some(Screen::Compose(cs)) => {
                assert_eq!(cs.body, "my draft", "the draft must survive");
                assert!(!cs.busy, "the composer must stop showing a spinner");
                assert!(cs.error.is_some(), "the refusal must show on the composer");
            }
            _ => panic!("expected the Compose screen to survive"),
        }

        // ThreadLoaded: viewing a thread/forum the user lacks permission for.
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.start_pollers();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            ..Default::default()
        }));
        app.handle_msg(Msg::ThreadLoaded { id: 42, page: 1, result: Err(phrase_error("no_permission")) });
        assert!(app.me.is_some(), "a 403 permission refusal must not sign the user out");
        assert!(!app.poller_handles.is_empty(), "the pollers must keep running");
        match app.screens.last() {
            Some(Screen::ThreadView(v)) => {
                assert!(v.error.is_some(), "the refusal must show in the thread view, not sign out");
            }
            _ => panic!("expected the ThreadView screen to survive"),
        }
    }

    /// Issue #564: the bootstrap `/me` check is the one caller allowed to
    /// treat certain 403s as "the account is gone" — but only those exact
    /// codes, and `missing_scope` is reworded as a configuration error
    /// rather than either signing out or showing a raw phrase.
    #[tokio::test]
    async fn bootstrap_403s_end_the_session_only_for_account_gone_codes() {
        for code in [
            "you_have_been_banned",
            "your_account_has_been_rejected",
            "your_account_has_been_disabled",
            "api_error.api_key_inactive",
        ] {
            let mut app = test_app();
            app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
            app.start_pollers();
            app.screens.push(screens::home_state(false));
            app.handle_msg(Msg::Bootstrap {
                generation: app.bootstrap_generation,
                result: Err(TaskError {
                    message: "account gone".into(),
                    code: Some(code.into()),
                    max_page: None,
                    kind: TaskErrorKind::Api(403),
                }),
            });
            assert!(app.me.is_none(), "code {code:?} must end the session");
            assert!(matches!(app.screens.last(), Some(Screen::Login(_))), "code {code:?}");
            assert!(app.poller_handles.is_empty(), "code {code:?} must stop the pollers");
        }

        // missing_scope on bootstrap must not end the session, and its
        // message must name the remedy (#695): the grant is re-minted by
        // signing in again, so a reader is not left staring at XF's phrase
        // wondering what to do about it.
        let err = TaskError::of(&Error::Api {
            code: "api_error.missing_scope".into(),
            message: "This request requires access to the following scope: media:read".into(),
            status: 403,
            max_page: None,
        });
        assert!(
            err.message.contains("media:read") && err.message.contains("sign in again"),
            "message: {:?}",
            err.message
        );
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.start_pollers();
        app.screens.push(screens::home_state(false));
        app.handle_msg(Msg::Bootstrap { generation: app.bootstrap_generation, result: Err(err) });
        assert!(app.me.is_some(), "missing_scope must not sign the user out");
        assert!(!app.poller_handles.is_empty());
    }

    /// Issue #538: XF's react/vote endpoints are toggles, so the notice has to
    /// come from the reply's `action`, and the refresh they trigger must not
    /// move the reader (or the selection the next `l` acts on).
    #[test]
    fn like_and_vote_notices_follow_the_servers_insert_or_delete() {
        use common::api::Toggle::{Inserted, Removed};
        assert_eq!(PostVerb::Like.notice(Inserted), "Post liked.");
        assert_eq!(PostVerb::Like.notice(Removed), "Like removed.");
        assert_eq!(PostVerb::VoteUp.notice(Inserted), "Voted up.");
        assert_eq!(PostVerb::VoteDown.notice(Inserted), "Voted down.");
        assert_eq!(PostVerb::VoteUp.notice(Removed), "Vote removed.");
        assert_eq!(PostVerb::VoteDown.notice(Removed), "Vote removed.");
        assert_eq!(PostVerb::of_vote("up"), PostVerb::VoteUp);
        assert_eq!(PostVerb::of_vote("DOWN"), PostVerb::VoteDown);
    }

    #[test]
    fn the_refresh_after_a_like_keeps_the_reader_and_the_selection_in_place() {
        let mut app = test_app();
        let posts = |n: u32| {
            (1..=n)
                .map(|i| common::models::Post { post_id: i, ..Default::default() })
                .collect::<Vec<_>>()
        };
        app.push_screen(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            page: 2,
            sel_post: 3,
            scroll: 12,
            posts: posts(5),
            ..Default::default()
        }));

        // What the toggle handler hands to `load_thread`.
        assert_eq!(open_thread_position(&app.screens), Some((42, 2, 3, 12)));

        app.keep_thread_position = Some((42, 2, 3, 12));
        app.handle_msg(Msg::ThreadLoaded {
            id: 42,
            page: 2,
            result: Ok(ThreadReply {
                thread: Thread { thread_id: 42, ..Default::default() },
                posts: posts(5),
                ..Default::default()
            }),
        });
        {
            let Some(Screen::ThreadView(view)) = app.screens.last() else {
                panic!("expected the ThreadView screen");
            };
            assert_eq!(view.sel_post, 3, "a counts refresh must not move the selection");
            assert_eq!(view.scroll, 12, "…nor scroll the reader back to the top");
        }
        assert!(app.keep_thread_position.is_none(), "the hint is one-shot");

        // Every other load still lands at the top of the page.
        app.handle_msg(Msg::ThreadLoaded {
            id: 42,
            page: 2,
            result: Ok(ThreadReply {
                thread: Thread { thread_id: 42, ..Default::default() },
                posts: posts(5),
                ..Default::default()
            }),
        });
        let Some(Screen::ThreadView(view)) = app.screens.last() else {
            panic!("expected the ThreadView screen");
        };
        assert_eq!((view.sel_post, view.scroll), (0, 0));
    }

    /// The scratch config dir every test's client, disk cache and
    /// `login-url.txt` are pinned to. Nothing under `dirs::config_dir()`
    /// (the operator's real `~/.config/wftui`) may ever be opened by a test.
    fn scratch_config_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("wftui-test-app-{}", std::process::id()))
    }

    /// An origin no test can actually reach: port 1 on loopback refuses
    /// instantly, so a request that escapes a stub fails fast instead of
    /// travelling to windowsforum.com.
    const OFFLINE_BASE: &str = "http://127.0.0.1:1";

    /// The `WfApiClient` every test's `App` owns: a token store in the
    /// scratch dir (so it starts with NO session and can never read, rewrite
    /// or erase the machine owner's `token.json`) and an unreachable origin.
    fn offline_client() -> Arc<WfApiClient> {
        let store = common::token::Store::with_path(scratch_config_dir().join("token.json"));
        let client = WfApiClient::with_store(store, OFFLINE_BASE).expect("client init");
        // Checked on every single `test_app()`, not just in the guard test:
        // if someone ever swaps this back to `WfApiClient::new()`, every
        // test in the crate fails loudly instead of quietly reading (and
        // revoking) the operator's session.
        assert!(
            !client.store_path().starts_with(common::config::default_config_root()),
            "a test client resolved the real config dir: {:?}",
            client.store_path()
        );
        Arc::new(client)
    }

    /// #715: Esc used to destroy an unsent post outright. It must keep it,
    /// and the same composer must offer it back.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn esc_keeps_the_draft_and_reopening_the_same_composer_restores_it() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "hours of typing".into();
        }
        assert!(app.pop_screen(), "Esc pops the composer");
        assert!(
            matches!(app.screens.last(), Some(Screen::Home(_))),
            "the composer is gone from the stack"
        );

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "hours of typing", "the words must come back verbatim");
                assert_eq!(c.body_cursor, "hours of typing".chars().count(), "caret at the end");
                assert!(c.resumed, "and the composer knows it resumed, so ^X is offered");
            }
            other => panic!("expected a composer, got {:?}", other.map(|s| s.title())),
        }
    }

    /// A draft belongs to one composer. An edit of a post and a fresh reply
    /// to the thread holding it must not share one, or a resumed edit would
    /// silently overwrite a post with someone's half-written reply.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn drafts_do_not_leak_between_composers() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "reply draft".into();
        }
        app.pop_screen();

        // A different thread: same kind of composer, different slot.
        app.reply_to_thread(&Thread { thread_id: 8, title: "Another thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert!(c.body.is_empty(), "thread 8 must not see thread 7's draft");
                assert!(!c.resumed);
            }
            _ => panic!("expected a composer"),
        }
        app.pop_screen();

        // And thread 7 still has its own.
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => assert_eq!(c.body, "reply draft"),
            _ => panic!("expected a composer"),
        }
    }

    /// Opening a composer, thinking better of it and pressing Esc must not
    /// leave an empty draft to be "resumed" later.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn an_empty_composer_stores_nothing() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "   \n ".into();
        }
        app.pop_screen();
        assert!(app.drafts.is_empty(), "whitespace is not a draft");
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => assert!(!c.resumed, "nothing to resume"),
            _ => panic!("expected a composer"),
        }
    }

    /// Once the post is on the site there is nothing left to recover, so the
    /// draft must go — otherwise the next reply to that thread comes up
    /// pre-filled with the post that was already made.
    // A tokio test: the success arm reloads the thread, which spawns.
    #[tokio::test]
    async fn a_successful_reply_clears_the_draft() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "posted words".into();
        }
        // Esc first, so there is a stored draft for the send to clear.
        app.pop_screen();
        assert_eq!(app.drafts.len(), 1);

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        app.handle_msg(Msg::ReplySent(Ok(Post { post_id: 1, thread_id: 7, ..Default::default() })));
        assert!(app.drafts.is_empty(), "a sent reply leaves no draft behind");
        assert!(
            !app.screens.iter().any(|s| matches!(s, Screen::Compose(_))),
            "and the composer is closed"
        );
    }

    /// `^X` on a resumed composer forgets the draft and puts back what the
    /// composer would have shown without one. For an edit that seed is the
    /// post's current text, so discarding a draft must not empty the post.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn discarding_a_resumed_draft_restores_the_seed_not_an_empty_editor() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        let thread = Thread { thread_id: 7, title: "A thread".into(), ..Default::default() };
        let mut post = Post { post_id: 500, thread_id: 7, ..Default::default() };
        post.message = "the post as it stands".into();

        app.execute_action(Action::StartEditPost(thread.clone(), Box::new(post.clone())));
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "an edit I abandoned".into();
        }
        app.pop_screen();

        app.execute_action(Action::StartEditPost(thread, Box::new(post)));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "an edit I abandoned", "the draft wins on reopen");
                assert_eq!(c.seed_body, "the post as it stands", "the seed is remembered");
            }
            _ => panic!("expected a composer"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(app.drafts.is_empty(), "^X forgets the draft");
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(
                    c.body, "the post as it stands",
                    "discarding must restore the post, not empty the editor"
                );
                assert!(!c.resumed, "and the cap goes away");
            }
            _ => panic!("expected a composer"),
        }
    }

    /// #715 + #581: a draft is unsent writing by one account. Signing out
    /// must not leave it on disk for whoever signs in next — on the same
    /// machine, one restart later.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn signing_out_forgets_the_drafts() {
        let mut app = test_app();
        let path = app.draft_store.path().to_path_buf();
        let _ = std::fs::remove_file(&path);
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "private words".into();
        }
        app.pop_screen();
        assert!(path.exists(), "test setup: the draft reached the disk");

        app.clear_all_drafts();
        assert!(app.drafts.is_empty(), "nothing left in memory");
        assert!(!path.exists(), "and nothing left on disk for the next account");
    }

    /// A session that merely *expires* is the same person coming back, so
    /// their draft must still be there. Only an explicit sign-out and a
    /// changed identity clear it.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn an_expired_session_keeps_the_draft() {
        let mut app = test_app();
        let _ = std::fs::remove_file(app.draft_store.path());
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "still mine".into();
        }
        app.pop_screen();

        app.end_session("Session expired; log in again.");
        assert_eq!(
            app.drafts.len(),
            1,
            "an expired session is the same user — their unsent post must survive"
        );
        let _ = std::fs::remove_file(app.draft_store.path());
    }

    /// The whole point of writing the file: a draft must survive the process,
    /// not just the screen stack.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn a_draft_survives_a_restart() {
        let mut app = test_app();
        let path = app.draft_store.path().to_path_buf();
        let _ = std::fs::remove_file(&path);
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "survives a crash".into();
        }
        app.pop_screen();

        // A second app reading the same store is what a restart looks like.
        let mut next = test_app();
        next.draft_store = common::drafts::Store::with_path(path.clone());
        next.drafts = next.draft_store.load();
        next.screens.push(screens::home_state(false));
        next.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match next.screens.last() {
            Some(Screen::Compose(c)) => assert_eq!(c.body, "survives a crash"),
            _ => panic!("expected a composer"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// #716: the point of the relay. A reply started in the browser must be
    /// what the composer offers, and the merge must not need the composer to
    /// be open — it runs once when the session goes live.
    #[tokio::test]
    async fn a_draft_from_the_website_is_offered_in_the_composer() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.merge_remote_drafts(vec![common::models::RemoteDraft {
            key: "thread-7".into(),
            message: "typed in the browser".into(),
            title: String::new(),
            last_update: 2_000,
            has_attachments: false,
        }]);
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "typed in the browser");
                assert!(c.resumed);
            }
            _ => panic!("expected a composer"),
        }
    }

    /// Newest wins, in both directions — otherwise a stale server copy would
    /// silently overwrite words typed here a moment ago.
    #[tokio::test]
    async fn the_newer_side_wins_the_merge() {
        let mut app = test_app();
        let key = common::drafts::DraftKey::ThreadReply(7);
        let local = |t: i64| common::drafts::Draft {
            body: "local".into(),
            saved_at: t,
            ..Default::default()
        };
        let remote = |t: i64| common::models::RemoteDraft {
            key: "thread-7".into(),
            message: "remote".into(),
            last_update: t,
            ..Default::default()
        };

        app.drafts.insert(key, local(5_000));
        app.merge_remote_drafts(vec![remote(1_000)]);
        assert_eq!(app.drafts[&key].body, "local", "an older server copy must not win");

        app.merge_remote_drafts(vec![remote(9_000)]);
        assert_eq!(app.drafts[&key].body, "remote", "a newer server copy must win");

        // Equal timestamps are the copy we pushed ourselves; keep ours.
        app.drafts.insert(key, local(9_000));
        app.merge_remote_drafts(vec![remote(9_000)]);
        assert_eq!(app.drafts[&key].body, "local", "a tie keeps the local copy");
    }

    /// A kind this build cannot map — a report draft, or one an add-on adds
    /// later — is skipped, never guessed at.
    #[tokio::test]
    async fn unknown_remote_draft_kinds_are_ignored() {
        let mut app = test_app();
        app.merge_remote_drafts(vec![
            common::models::RemoteDraft { key: "report-1".into(), message: "x".into(), ..Default::default() },
            common::models::RemoteDraft { key: "thread-7".into(), message: "ok".into(), ..Default::default() },
        ]);
        assert_eq!(app.drafts.len(), 1);
        assert!(app.drafts.contains_key(&common::drafts::DraftKey::ThreadReply(7)));
    }

    /// Esc mirrors the draft to the website under XenForo's own key, and a
    /// successful send clears it there too — the REST API does not delete
    /// drafts the way XF's web post controller does, so without that every
    /// post from here would leave one behind in the browser.
    #[tokio::test]
    async fn drafts_are_pushed_and_cleared_on_the_site() {
        let api = Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "shared words".into();
        }
        app.pop_screen();
        tokio::task::yield_now().await;
        assert_eq!(
            api.drafts_saved(),
            vec![("thread-7".to_string(), "shared words".to_string(), String::new())],
            "Esc must mirror the draft under XF's own key"
        );

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        app.handle_msg(Msg::ReplySent(Ok(Post { post_id: 1, thread_id: 7, ..Default::default() })));
        tokio::task::yield_now().await;
        assert_eq!(api.drafts_deleted(), vec!["thread-7".to_string()]);
    }

    /// XF has no draft for an edit, so there is nothing to sync to. The
    /// local draft still works; it just never reaches the wire.
    #[tokio::test]
    async fn an_edit_draft_is_never_sent_to_the_site() {
        let api = Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));

        let thread = Thread { thread_id: 7, title: "A thread".into(), ..Default::default() };
        let post = Post { post_id: 500, thread_id: 7, ..Default::default() };
        app.execute_action(Action::StartEditPost(thread, Box::new(post)));
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "an edit in progress".into();
        }
        app.pop_screen();
        tokio::task::yield_now().await;

        assert!(api.drafts_saved().is_empty(), "an edit has no XF draft key");
        assert_eq!(
            app.drafts.len(),
            1,
            "but it is still kept locally — that is the whole point of #715"
        );
    }

    /// The #715 regression guard, under #716: the relay is a mirror, not the
    /// store. If the site is unreachable the words must still be here.
    #[tokio::test]
    async fn a_relay_failure_never_costs_the_local_draft() {
        let api = Arc::new(RecordingApi::default());
        api.drafts_fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "survives a broken relay".into();
        }
        app.pop_screen();
        tokio::task::yield_now().await;

        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        match app.screens.last() {
            Some(Screen::Compose(c)) => assert_eq!(c.body, "survives a broken relay"),
            _ => panic!("expected a composer"),
        }
    }

    /// Signing out clears this machine's drafts, but must NOT reach across
    /// and delete the website's copies: those live behind the signing-out
    /// user's own account, where they are still wanted. Signing out of a
    /// terminal must not destroy a reply half-written in a browser.
    #[tokio::test]
    async fn signing_out_does_not_delete_the_websites_drafts() {
        let api = Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        let _ = std::fs::remove_file(app.draft_store.path());
        app.screens.push(screens::home_state(false));
        app.reply_to_thread(&Thread { thread_id: 7, title: "A thread".into(), ..Default::default() });
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "still mine on the website".into();
        }
        app.pop_screen();
        tokio::task::yield_now().await;

        app.clear_all_drafts();
        tokio::task::yield_now().await;
        assert!(app.drafts.is_empty(), "gone from this machine");
        assert!(
            api.drafts_deleted().is_empty(),
            "but never deleted from the account they belong to"
        );
        let _ = std::fs::remove_file(app.draft_store.path());
    }

    /// #716: the gap that made the store nearly useless — a draft could only
    /// be seen by reopening the exact composer that made it, so a new-thread
    /// draft in a forum you were not looking at was invisible, and there was
    /// no answer at all to "what am I part-way through?".
    #[tokio::test]
    async fn the_drafts_list_shows_every_draft_and_reopens_the_right_composer() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));

        // A new-thread draft: the exact shape that was invisible, because no
        // thread view could ever hint at it.
        app.new_thread(304);
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.title = "Tet".into();
            c.body = "Test 1 2 3".into();
        }
        app.pop_screen();

        app.execute_action(Action::OpenDrafts);
        match app.screens.last() {
            Some(Screen::Drafts(d)) => {
                assert_eq!(d.rows.len(), 1);
                assert_eq!(d.rows[0].preview, "Test 1 2 3", "the row says what was written");
                assert!(d.rows[0].shared, "a new-thread draft syncs to the site");
            }
            other => panic!("expected the drafts list, got {:?}", other.map(|s| s.title())),
        }

        // Enter resumes it, and the composer comes back with the words.
        app.execute_action(Action::ResumeDraft(common::drafts::DraftKey::NewThread(304)));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "Test 1 2 3");
                assert_eq!(c.title, "Tet");
                assert!(matches!(
                    c.target,
                    Some(ComposeTarget::NewThread { node_id: 304 })
                ));
            }
            _ => panic!("expected the composer"),
        }
    }

    /// An edit draft has no XF key, so the list must say it lives here only
    /// rather than let it look like the sync failed.
    #[tokio::test]
    async fn the_drafts_list_marks_what_does_not_sync() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        let thread = Thread { thread_id: 7, title: "A thread".into(), ..Default::default() };
        let post = Post { post_id: 500, thread_id: 7, ..Default::default() };
        app.execute_action(Action::StartEditPost(thread, Box::new(post)));
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "an edit".into();
        }
        app.pop_screen();

        app.execute_action(Action::OpenDrafts);
        match app.screens.last() {
            Some(Screen::Drafts(d)) => {
                assert_eq!(d.rows.len(), 1);
                assert!(!d.rows[0].shared, "an edit has no XF draft to sync to");
                assert_eq!(d.rows[0].label, "Edit in A thread");
            }
            _ => panic!("expected the drafts list"),
        }
    }

    /// `D` on a row deletes that draft and leaves you looking at the rest.
    #[tokio::test]
    async fn deleting_from_the_list_keeps_the_list_open() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        for id in [7u32, 8] {
            app.reply_to_thread(&Thread {
                thread_id: id,
                title: format!("Thread {id}"),
                ..Default::default()
            });
            if let Some(Screen::Compose(c)) = app.screens.last_mut() {
                c.body = format!("draft {id}");
            }
            app.pop_screen();
        }
        app.execute_action(Action::OpenDrafts);
        app.execute_action(Action::DropDraft(common::drafts::DraftKey::ThreadReply(7)));
        match app.screens.last() {
            Some(Screen::Drafts(d)) => {
                assert_eq!(d.rows.len(), 1, "the list stays open, one row lighter");
                assert_eq!(d.rows[0].key, common::drafts::DraftKey::ThreadReply(8));
            }
            _ => panic!("the list must stay open"),
        }
    }

    /// The other half of the answer: the thread view itself says a reply is
    /// waiting, since that is where a reader would look for it.
    #[tokio::test]
    async fn the_thread_view_says_when_a_reply_is_waiting() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 7, title: "A thread".into(), ..Default::default() },
            ..Default::default()
        }));

        let bar = |app: &mut App| {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24))
                .expect("terminal");
            term.draw(|f| app.draw(f)).expect("draw");
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        assert!(bar(&mut app).contains("reply"), "test setup: the reply cap is shown");
        assert!(
            !bar(&mut app).contains("resume draft"),
            "with no draft it must just say reply"
        );

        app.drafts.insert(
            common::drafts::DraftKey::ThreadReply(7),
            common::drafts::Draft { body: "waiting".into(), ..Default::default() },
        );
        assert!(
            bar(&mut app).contains("resume draft"),
            "an unsent reply to this thread must be visible from the thread"
        );
    }

    /// The question a reader actually asks: I wrote something, pressed Esc,
    /// came back to the thread later and pressed `r` — is it there? Driven
    /// through the real key handler, not by calling `reply_to_thread`, so it
    /// covers the whole path from the keystroke.
    #[tokio::test]
    async fn pressing_r_in_a_thread_reopens_the_draft() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 7, title: "A thread".into(), ..Default::default() },
            ..Default::default()
        }));

        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "half a reply".into();
        } else {
            panic!("r must open the composer");
        }
        app.pop_screen();
        assert!(
            matches!(app.screens.last(), Some(Screen::ThreadView(_))),
            "Esc returns to the thread"
        );

        // ... later, from the same thread.
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "half a reply", "the draft must be waiting");
                assert!(c.resumed);
            }
            other => panic!("expected the composer, got {:?}", other.map(|s| s.title())),
        }
    }

    /// `Q` is different, and the difference matters: it seeds the composer
    /// with a quote of the selected post. A draft must not silently swallow
    /// that — the reader just asked for this specific quote, so the quote
    /// wins and the draft is appended under it rather than thrown away.
    #[tokio::test]
    async fn quoting_keeps_both_the_quote_and_the_draft() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.drafts.insert(
            common::drafts::DraftKey::ThreadReply(7),
            common::drafts::Draft { body: "half a reply".into(), ..Default::default() },
        );

        let thread = Thread { thread_id: 7, title: "A thread".into(), ..Default::default() };
        let post = Post {
            post_id: 99,
            thread_id: 7,
            user_id: 3,
            username: "someone".into(),
            message: "the quoted words".into(),
            ..Default::default()
        };
        app.execute_action(Action::StartReplyQuoting(thread, Box::new(post)));

        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert!(c.body.contains("the quoted words"), "the quote must survive: {:?}", c.body);
                assert!(c.body.contains("half a reply"), "and so must the draft: {:?}", c.body);
                assert!(
                    c.body.find("the quoted words") < c.body.find("half a reply"),
                    "the draft belongs under the quote, where a reply goes"
                );
            }
            _ => panic!("expected the composer"),
        }
    }

    /// GUARD (issue #565). No test may read or write the machine owner's
    /// real config dir, and none may reach the live site. Both happened:
    /// `test_app()` built a real `WfApiClient::new()`, so
    /// `bootstrap_transient_error_keeps_the_session_and_r_retries` sent a
    /// live `GET /api/me` with the operator's own bearer, and a test that
    /// awaited past `logout()` sent two `POST /api/oauth2/revoke` and erased
    /// the real `token.json`. This pins every path and origin the test `App`
    /// can reach.
    #[tokio::test]
    async fn guard_no_test_touches_the_real_config_dir_or_the_live_site() {
        let real = common::config::default_config_root();
        let scratch = scratch_config_dir();
        let app = test_app();

        // 1. The session store is scratch, and it holds nothing — so no
        //    handler that spawns a call can ever authenticate one.
        let store = app.client.store_path();
        assert!(store.starts_with(&scratch), "store escaped the scratch dir: {store:?}");
        assert!(!store.starts_with(&real), "store resolved the real config dir: {store:?}");
        assert!(
            !app.client.has_tokens().await,
            "a test client must never see a stored session"
        );

        // 2. Every other file the app writes hangs off that same store path.
        let login_url = app.client.store_path().with_file_name("login-url.txt");
        assert!(login_url.starts_with(&scratch), "{login_url:?}");
        assert!(app.images.disk_dir().starts_with(&scratch), "{:?}", app.images.disk_dir());
        assert!(!app.images.disk_dir().starts_with(&real), "{:?}", app.images.disk_dir());
        // #715: drafts are the third file the app writes, and a test that
        // pops a composer writes one — it must never land on the operator's.
        let drafts = app.draft_store.path();
        assert!(drafts.starts_with(&scratch), "draft store escaped the scratch dir: {drafts:?}");
        assert!(!drafts.starts_with(&real), "draft store resolved the real config dir: {drafts:?}");

        // 3. The origin is unreachable, and the API seam is a stub, not the
        //    network client (`me()` answers without a request).
        assert_eq!(app.client.base_url(), OFFLINE_BASE);
        assert!(!app.client.base_url().contains("windowsforum.com"));
        assert!(matches!(app.api.me().await, Err(common::error::Error::NoToken)));

        // 4. Poisoned: a config dir that is a plain FILE cannot hold a
        //    store, and nothing may quietly fall back to one that can.
        let poison = scratch.join("poison");
        let _ = std::fs::remove_dir_all(&poison);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(&poison, b"poison").unwrap();
        let poisoned = common::token::Store::with_path(poison.join("token.json"));
        assert!(
            !matches!(poisoned.load(), Ok(Some(_))),
            "a poisoned config dir must never yield a session"
        );
        assert!(
            poisoned
                .save(&common::token::TokenSet {
                    access_token: "guard".into(),
                    refresh_token: "guard".into(),
                    expires_at: 0,
                    scope: "guard".into(),
                })
                .is_err(),
            "a stray save must fail loudly, not land in the real store"
        );
        let _ = std::fs::remove_file(&poison);
    }

    /// Builds an `App` for handler and poller bookkeeping tests.
    ///
    /// Issue #565: this used to build a real `WfApiClient::new()`, which
    /// reads the machine owner's `~/.config/wftui/token.json` — a test that
    /// pressed `r` then really sent `GET /api/me` to windowsforum.com with
    /// the operator's bearer, and one that awaited past `logout()` revoked
    /// that session and deleted the store. Now `App::api` is a stub that
    /// answers `NoToken` for everything (no request can leave the process
    /// even if a handler spawns one) and `App::client` is pinned to a
    /// scratch store and an unreachable origin.
    /// Gives every `test_app()` its own draft store; see the comment there.
    static TEST_DRAFT_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn test_app() -> App {
        let client = offline_client();
        let (tx, rx) = mpsc::unbounded_channel();
        App {
            crumb_targets: Vec::new(),
            next_list_seq: 1,
            api: Arc::new(RecordingApi::default()),
            client,
            tx,
            rx,
            theme: Theme::detect(),
            glyphs: glyph::detect(),
            images: crate::images::Images::text_only_at(
                scratch_config_dir().join("cache").join("img"),
            ),
            drafts: std::collections::HashMap::new(),
            // Scratch dir, never the operator's own (#565) — asserted by
            // `guard_no_test_touches_the_real_config_dir_or_the_live_site`.
            //
            // One file PER APP, not one shared by every test: the suite runs
            // in parallel, and a shared store meant a test that removed its
            // draft file deleted another test's draft mid-run. A test that
            // deliberately wants two apps to share one (the restart case)
            // points the second at the first's `draft_store.path()`.
            draft_store: common::drafts::Store::with_path(
                scratch_config_dir().join(format!(
                    "drafts-{}.json",
                    TEST_DRAFT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                )),
            ),
            image_slots: Arc::new(tokio::sync::Semaphore::new(IMAGE_LOAD_CONCURRENCY)),
            screens: Vec::new(),
            me: None,
            alerts_unread: 3,
            convos_unread: 5,
            status: String::new(),
            status_set_at: None,
            keep_thread_position: None,
            show_help: false,
            palette: None,
            prefix: Prefix::default(),
            selection: None,
            screen_rows: Vec::new(),
            screen_cols: Vec::new(),
            clipboard: String::new(),
            last_click_instant: None,
            last_click_pos: (0, 0),
            click_count: 0,
            press_pos: None,
            last_title: String::new(),
            should_quit: false,
            poller_handles: Vec::new(),
            write_handles: Vec::new(),
            bootstrap_retry_needed: false,
            bootstrap_generation: 0,
            session_recovery_tried: false,
            session_recovery_pending: false,
            session_recovery_started_at: None,
            login_generation: 0,
            profile_generation: 0,
            search_generation: 0,
            login_task: None,
            body_rect: ratatui::layout::Rect::default(),
            // Tests build the map enabled: every hit-map test drives it
            // directly, and `WFTUI_MOUSE` is process-global (the env lock
            // lives in `common::config`), so it is never read here.
            hits: HitMap::new(true),
        }
    }

    /// Issue #571: DESIGN.md says a status toast ("Reply posted.", "Like
    /// removed.", "Copied N chars…") replaces the left status text and
    /// clears after 4 s — `App::status` had no timer at all, so a toast sat
    /// there indefinitely. `set_status` stamps it; `expire_status_toast`
    /// (the event loop's per-tick check) clears it once the stamp is old
    /// enough. A persistent hint (`set_hint`) — an ongoing state or a
    /// "why didn't that work" explanation — must never auto-clear. The
    /// clock is simulated by backdating the stamp rather than sleeping.
    #[test]
    fn status_toasts_clear_after_four_seconds_but_hints_never_do() {
        let mut app = test_app();

        app.set_status("Reply posted.");
        assert_eq!(app.status, "Reply posted.");
        assert!(app.status_set_at.is_some());

        // Not old enough yet: a fresh toast must survive a tick.
        app.expire_status_toast();
        assert_eq!(app.status, "Reply posted.", "must not clear before 4 s");

        // Simulated clock: backdate the toast past the 4 s mark instead of
        // actually sleeping.
        app.status_set_at = Some(std::time::Instant::now() - Duration::from_secs(5));
        app.expire_status_toast();
        assert_eq!(app.status, "", "a stale toast must clear");
        assert!(app.status_set_at.is_none());

        // A persistent hint carries no stamp at all, so it survives any
        // number of ticks.
        app.set_hint("Sign in first, or press q to quit.");
        assert!(app.status_set_at.is_none(), "a hint must not be a toast");
        app.expire_status_toast();
        app.expire_status_toast();
        assert_eq!(
            app.status, "Sign in first, or press q to quit.",
            "a persistent hint must never auto-clear"
        );
    }

    /// Issue #576: four "Copied …" writes (Ctrl+C-with-selection, and the
    /// double-/triple-click/drag-release copy paths in `handle_mouse`) used
    /// to assign `self.status` directly, skipping `status_set_at` — so the
    /// message either inherited whatever toast timer was already running or
    /// (behind a persistent hint) never expired at all. `set_status` and
    /// `set_hint` are the only two places `App::status` may be written
    /// directly; this pins that every other write routes through one of
    /// them, so a new direct assignment fails this test immediately instead
    /// of silently reintroducing the bug.
    ///
    /// Every module of `app` is scanned, not just this one (#714): the split
    /// moved most of the writers out of `mod.rs`, and a scan of one file
    /// would have kept passing while covering almost nothing. A new sibling
    /// module must be added here — `unscanned_app_modules` is the guard that
    /// says so.
    #[test]
    fn no_direct_status_writes_bypass_set_status_or_set_hint() {
        let src = format!(
            "{}{}{}{}{}",
            include_str!("mod.rs"),
            include_str!("actions.rs"),
            include_str!("input.rs"),
            include_str!("msg.rs"),
            include_str!("session.rs"),
        );
        let direct_writes: Vec<&str> = src
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("self.status ="))
            .collect();
        assert_eq!(
            direct_writes,
            vec!["self.status = s.into();", "self.status = s.into();"],
            "only set_status/set_hint may assign `self.status` directly — \
             found an unexpected direct write, which will inherit or never \
             clear a stale toast timer (issue #576)"
        );
    }

    /// #714: `Screen` carries `#[allow(clippy::large_enum_variant)]`. Boxing
    /// the outliers is only worth it if the gap is large, and the numbers
    /// live here so the next reader does not have to re-litigate it from
    /// scratch. This is a measurement, not a limit — if a variant grows past
    /// the ceiling, take the number seriously before raising it.
    #[test]
    fn screen_enum_size_is_measured_not_assumed() {
        let size = std::mem::size_of::<Screen>();
        eprintln!("size_of::<Screen>() = {size} bytes");
        // 984 bytes as measured on 2026-09-07. A screen is moved only when
        // one is pushed or popped, and the stack holds a handful, so boxing
        // the largest variant would buy nothing measurable — hence the
        // `allow(clippy::large_enum_variant)` on the enum.
        assert!(
            size <= 1536,
            "Screen grew to {size} bytes. Every screen in the stack, and every \
             move of one, pays this. Box the largest variant rather than \
             raising the ceiling."
        );
    }

    /// #714: the scan above names its files by hand, because `include_str!`
    /// takes a literal. A new `app/` module that nobody adds to that list
    /// would be silently exempt from it — and from any future source scan
    /// written the same way. This fails when the directory and the list
    /// disagree.
    #[test]
    fn every_app_module_is_covered_by_the_source_scans() {
        const SCANNED: [&str; 5] = ["mod.rs", "actions.rs", "input.rs", "msg.rs", "session.rs"];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("app");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("app module dir")
            .filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().into_owned();
                name.ends_with(".rs").then_some(name)
            })
            .collect();
        on_disk.sort();
        let mut expected: Vec<String> = SCANNED.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            on_disk, expected,
            "an app/ module is missing from the source scans in this file — \
             add it to SCANNED and to no_direct_status_writes_bypass_set_status_or_set_hint"
        );
    }

    /// Issue #556: the sign-in screen is a gate, not a place. With no
    /// session, the global `c`/`a`/`s`/`/` navigation block must not push an
    /// authenticated screen over Login (it must instead let `on_key` handle
    /// `c` as "copy link"), and Esc must not pop the gate onto a
    /// session-less Home.
    #[test]
    fn sign_in_screen_blocks_global_nav_and_esc_while_logged_out() {
        let mut app = test_app();
        app.screens.push(screens::login_state());
        if let Some(Screen::Login(ls)) = app.screens.last_mut() {
            ls.stage = screens::LoginStage::Waiting;
            ls.url = "https://windowsforum.com/tui-start/abc123".into();
        }
        assert!(app.me.is_none());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.screens.len(), 1, "c must not push the Inbox over a session-less Login screen");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert_eq!(
            app.clipboard, "https://windowsforum.com/tui-start/abc123",
            "the login screen's own 'c' handling must still fire"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.screens.len(), 1, "a/s// must not push screens while logged out");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.screens.len(),
            1,
            "Esc must not pop the sign-in gate onto a session-less Home"
        );
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
        assert!(app.status.contains("Sign in first"));
    }

    /// Issue #560: Ctrl+L (sign out) is handled before the palette/help/`g`
    /// prefix layers ever see it, so `logout()` itself must tear them down —
    /// otherwise the palette stays armed over the freshly-pushed Login
    /// screen and its `Enter` can still push an authenticated screen with no
    /// session.
    #[tokio::test]
    async fn ctrl_l_with_the_palette_open_clears_it_and_leaves_login_on_top() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.open_palette();
        assert!(app.palette.is_some(), "test setup: the palette must actually be open");

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));

        assert!(app.palette.is_none(), "logout must close the palette");
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
    }

    /// Issue #567: a write spawned by `^S` waits on the politeness gates
    /// (`api_gate`, then `write_gate` — 30 s after a previous post, 180 s
    /// after a new thread) *before* `valid_token()` is ever consulted, and
    /// `handle_key` routes `Ctrl+L` to `logout()` ahead of any busy check
    /// (unlike `Esc`, blocked on a busy composer since #520). So the
    /// composer was torn down while the request still went out afterwards —
    /// with whatever token was in memory once the gate opened, which on a
    /// shared machine is the *next* account's — and the client announced
    /// "Reply posted." over the sign-in screen, publishing the draft the
    /// user believed discarded. A write now belongs to the session that
    /// started it: `end_session` aborts every in-flight one.
    #[tokio::test]
    async fn a_reply_abandoned_by_ctrl_l_is_never_sent() {
        // The stub stands in for the gate: it waits before recording, so a
        // recorded call means the request really would have left.
        let api = Arc::new(RecordingApi {
            write_delay: Duration::from_millis(200),
            ..Default::default()
        });
        let mut app = test_app();
        app.api = api.clone();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        app.execute_action(Action::SubmitReply {
            thread_id: 42,
            message: "a draft the user then abandoned".into(),
        });
        // Let the write task start and park in the gate wait.
        tokio::task::yield_now().await;
        assert!(
            api.replies().is_empty(),
            "test setup: the write must still be waiting on the gate"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(app.me.is_none(), "test setup: Ctrl+L must have ended the session");

        // Well past the gate the abandoned write was waiting on.
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            api.replies().is_empty(),
            "a write abandoned by Ctrl+L must never reach the API: {:?}",
            api.replies()
        );
        let mut msgs = Vec::new();
        while let Ok(m) = app.rx.try_recv() {
            msgs.push(m);
        }
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::ReplySent(_))),
            "no ReplySent may land on the sign-in screen"
        );
    }

    /// The same rule for a like: a ♡ press is an attributed server-side
    /// mutation, but it rode a plain `tokio::spawn`, so Ctrl+L tore the
    /// session down while the reaction still went out afterwards — under
    /// whatever token was in memory once the gate opened. Toggles go through
    /// `spawn_write` now, so `end_session` aborts them like any other write.
    #[tokio::test]
    async fn a_like_abandoned_by_ctrl_l_is_never_sent() {
        let api = Arc::new(RecordingApi {
            write_delay: Duration::from_millis(200),
            ..Default::default()
        });
        let mut app = test_app();
        app.api = api.clone();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        app.execute_action(Action::ReactPost(99));
        tokio::task::yield_now().await;
        assert!(
            api.reactions().is_empty(),
            "test setup: the toggle must still be in flight"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(app.me.is_none(), "test setup: Ctrl+L must have ended the session");

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            api.reactions().is_empty(),
            "a like abandoned by Ctrl+L must never reach the API: {:?}",
            api.reactions()
        );
        let mut msgs = Vec::new();
        while let Ok(m) = app.rx.try_recv() {
            msgs.push(m);
        }
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::PostToggled { .. })),
            "no PostToggled may land on the sign-in screen"
        );
    }

    /// #667: a bounced Enter used to stack a second identical ThreadView on
    /// top of the first — only the top one ever loads, so popping back
    /// revealed a twin stuck on "Loading…" forever.
    #[tokio::test]
    async fn a_double_enter_on_one_thread_stacks_no_duplicate_view() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        let thread = Thread { thread_id: 42, ..Default::default() };
        app.execute_action(Action::OpenThread(thread.clone()));
        let depth = app.screens.len();
        assert!(matches!(app.screens.last(), Some(Screen::ThreadView(_))));

        app.execute_action(Action::OpenThread(thread));
        assert_eq!(app.screens.len(), depth, "no duplicate view while loading");
    }

    /// #667: re-opening the conversation already loading in the view pane
    /// must not fire a second fetch over the first — ConversationLoaded
    /// matches by id only, so the last reply to land would win.
    #[tokio::test]
    async fn reopening_a_loading_conversation_does_not_replace_the_view() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        // The dual-pane Inbox is where the view pane lives.
        app.screens.push(Screen::Inbox(screens::InboxState {
            dual: true,
            ..Default::default()
        }));
        let conv = common::models::Conversation {
            conversation_id: 5,
            title: "A DM".into(),
            ..Default::default()
        };
        app.open_conversation(conv.clone());
        {
            let Some(Screen::Inbox(inbox)) = app.screens.last_mut() else {
                panic!("expected the Inbox");
            };
            let view = inbox.view.as_mut().expect("the view pane primed");
            view.page = 7; // a marker: a replacement would reset it to 1
        }

        app.open_conversation(conv);
        let Some(Screen::Inbox(inbox)) = app.screens.last() else {
            panic!("expected the Inbox");
        };
        let view = inbox.view.as_ref().expect("the view survives");
        assert_eq!(view.page, 7, "the in-flight load was not replaced");
        assert!(view.loading);
    }

    /// #680: both navigation surfaces reach the new catalogs — `g m` / `g r`
    /// and the go-to palette's rows — each pushing its screen and firing the
    /// page-1 fetch through the stub.
    #[tokio::test]
    async fn the_gallery_and_resources_are_reachable_by_chord_and_palette() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        let Some(Screen::MediaGallery(m)) = app.screens.last() else {
            panic!("g m must open the Media Gallery, got {:?}", app.screens.last().map(|s| s.title()));
        };
        assert!(m.loading, "the page-1 fetch is in flight");

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let Some(Screen::Resources(r)) = app.screens.last() else {
            panic!("g r must open Resources, got {:?}", app.screens.last().map(|s| s.title()));
        };
        assert!(r.loading, "the page-1 fetch is in flight");

        // The palette teaches the same two destinations: typing the row's
        // name and pressing Enter runs the same opener the chord does.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        for c in "media gallery".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.screens.last(), Some(Screen::MediaGallery(_))),
            "the palette row must open the gallery too"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        for c in "resources".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.screens.last(), Some(Screen::Resources(_))),
            "and the resources row opens the resource catalog"
        );
    }

    /// #710: clicking a play row plays THAT video. Measured on a drawn
    /// frame, so the hit box and the row can never disagree.
    #[test]
    fn clicking_a_play_row_plays_that_video() {
        let mut app = home_app();
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 9, title: "A thread".into(), ..Default::default() },
            posts: vec![Post {
                post_id: 1,
                username: "kemical".into(),
                message: "one [MEDIA=youtube]aaa[/MEDIA]\ntwo [MEDIA=youtube]bbb[/MEDIA]"
                    .into(),
                ..Default::default()
            }],
            page: 1,
            last_page: 1,
            ..Default::default()
        }));
        frame(&mut app, 120, 24);

        // Both rows registered, and each answers as its own index.
        let mut found: Vec<(usize, u16)> = Vec::new();
        for y in 0..24u16 {
            if let Some(Hit::Video(n)) = app.hits.at(3, y) {
                found.push((*n, y));
            }
        }
        assert_eq!(found.len(), 2, "one hit row per video: {found:?}");
        assert_eq!(found[0].0, 0);
        assert_eq!(found[1].0, 1);

        // Clicking the SECOND row opens THAT video, not the first. It goes
        // to the browser (#711's in-app player was removed — it cost 7-26
        // Mbit/s of terminal traffic through the web server for a video the
        // reader could watch at about 1), so what is pinned here is which
        // url the click resolves to.
        app.click_hit((3, found[1].1), false);
        // `open_url` says what it opened, and puts it on the clipboard for a
        // remote session where the opener targets the wrong machine.
        assert!(
            app.status.contains("bbb"),
            "the row that was clicked decides the video: {:?}",
            app.status
        );
    }


    /// #709: an uploaded file lands at the caret as `[ATTACH]id[/ATTACH]`,
    /// its key is remembered, and the write that follows carries that key —
    /// without it the upload is attached to nothing.
    #[tokio::test]
    async fn an_upload_lands_at_the_caret_and_its_key_reaches_the_write() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 9,
                thread_title: "A thread".into(),
            }),
            body: "See: after".into(),
            // Right after "See: ".
            body_cursor: 5,
            uploading: true,
            ..Default::default()
        }));

        app.handle_msg(Msg::AttachmentUploaded(Ok((
            "key-1".to_string(),
            common::models::Attachment {
                attachment_id: 55,
                filename: "shot.png".into(),
                ..Default::default()
            },
        ))));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "See: [ATTACH]55[/ATTACH]after", "inserted at the caret");
                assert_eq!(c.body_cursor, 5 + "[ATTACH]55[/ATTACH]".chars().count());
                assert_eq!(c.attachment_key.as_deref(), Some("key-1"));
                assert!(!c.uploading);
                assert_eq!(c.attachments.len(), 1);
            }
            _ => panic!("the composer should still be open"),
        }

        // And the reply carries the key.
        app.execute_action(Action::SubmitReply {
            thread_id: 9,
            message: "See: [ATTACH]55[/ATTACH]after".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let sent = api.replies();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].2.as_deref(), Some("key-1"), "the write carries the key");
    }

    /// A failed upload says why and leaves the draft alone — the file is the
    /// point of the post often enough that silently dropping it would be
    /// worse than the failure.
    #[tokio::test]
    async fn a_failed_upload_reports_and_keeps_the_draft() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 9,
                thread_title: "A thread".into(),
            }),
            body: "my draft".into(),
            uploading: true,
            ..Default::default()
        }));
        app.handle_msg(Msg::AttachmentUploaded(Err(
            "/tmp/nope.png: No such file or directory".into(),
        )));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "my draft");
                assert!(!c.uploading);
                assert!(c.attachment_key.is_none());
                assert!(c.error.as_deref().is_some_and(|e| e.contains("No such file")));
            }
            _ => panic!("the composer should still be open"),
        }
    }

    /// #708: editing a post sends the edit, closes the editor and reloads
    /// the thread the EDITOR names — not whatever thread view happens to be
    /// on the stack — so the reader sees what was saved.
    #[tokio::test]
    async fn editing_a_post_sends_it_and_reopens_the_thread() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 9, title: "A thread".into(), ..Default::default() },
            posts: vec![Post {
                post_id: 500,
                message: "before".into(),
                can_edit: true,
                ..Default::default()
            }],
            page: 2,
            last_page: 2,
            ..Default::default()
        }));

        app.execute_action(Action::StartEditPost(
            Thread { thread_id: 9, title: "A thread".into(), ..Default::default() },
            Box::new(Post { post_id: 500, message: "before".into(), can_edit: true, ..Default::default() }),
        ));
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "before", "the editor opens on what is there now");
                assert!(matches!(
                    c.target,
                    Some(ComposeTarget::EditPost { post_id: 500, thread_id: 9, .. })
                ));
            }
            _ => panic!("an editor should be open"),
        }

        app.execute_action(Action::SubmitEdit {
            post_id: 500,
            message: "after".into(),
        });
        tokio::task::yield_now().await;
        assert_eq!(
            api.edits.lock().expect("lock").clone(),
            vec![(500, "after".to_string())]
        );

        app.handle_msg(Msg::PostEdited { post_id: 500, result: Ok(()) });
        assert!(
            matches!(app.screens.last(), Some(Screen::ThreadView(_))),
            "the editor closes on success"
        );
        assert_eq!(
            app.keep_thread_position.map(|(id, page, _, _)| (id, page)),
            Some((9, 2)),
            "and the thread reloads on the page the post is on"
        );
    }

    /// A failed edit keeps the editor open with the draft intact — losing a
    /// rewritten post to a 403 would be worse than the 403.
    #[tokio::test]
    async fn a_failed_edit_keeps_the_draft() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::EditPost {
                post_id: 500,
                thread_id: 9,
                thread_title: "A thread".into(),
            }),
            body: "my rewrite".into(),
            busy: true,
            ..Default::default()
        }));
        app.handle_msg(Msg::PostEdited {
            post_id: 500,
            result: Err(TaskError {
                message: "no permission".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Api(403),
            }),
        });
        match app.screens.last() {
            Some(Screen::Compose(c)) => {
                assert_eq!(c.body, "my rewrite", "the draft survives");
                assert!(!c.busy);
                assert_eq!(c.error.as_deref(), Some("no permission"));
            }
            _ => panic!("the editor must stay open"),
        }
    }

    /// Deleting is soft, always: a hard delete from a keystroke is not
    /// something this client offers.
    #[tokio::test]
    async fn deleting_a_post_is_a_soft_delete() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        app.execute_action(Action::DeletePost { post_id: 500, thread_id: 9 });
        tokio::task::yield_now().await;
        assert_eq!(api.deletes.lock().expect("lock").clone(), vec![(500, false)]);
    }

    /// #694: reading a thread marks it read — up to the newest post that
    /// was actually on screen, not to "now" on open, and only when leaving.
    #[tokio::test]
    async fn leaving_a_thread_marks_it_read_up_to_what_was_seen() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.threads = vec![Thread { thread_id: 9, is_unread: true, ..Default::default() }];
        }
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 9, ..Default::default() },
            posts: vec![
                Post { post_id: 1, post_date: 100, ..Default::default() },
                Post { post_id: 2, post_date: 200, ..Default::default() },
            ],
            page: 1,
            last_page: 1,
            seen_date: 100,
            ..Default::default()
        }));

        app.pop_screen();
        // The write is spawned, like every other one in this app.
        tokio::task::yield_now().await;
        let reported = || api.threads_read.lock().expect("lock").clone();
        assert_eq!(
            reported(),
            vec![(9, Some(100))],
            "marked up to the post that was seen, not the newest one"
        );
        // Only page 1's first post was seen, so the row stays unread.
        match app.screens.last() {
            Some(Screen::Home(h)) => assert!(h.list.threads[0].is_unread),
            _ => panic!("home"),
        }

        // Read to the end this time: the row flips without a refetch.
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 9, ..Default::default() },
            posts: vec![
                Post { post_id: 1, post_date: 100, ..Default::default() },
                Post { post_id: 2, post_date: 200, ..Default::default() },
            ],
            page: 1,
            last_page: 1,
            seen_date: 200,
            ..Default::default()
        }));
        app.pop_screen();
        tokio::task::yield_now().await;
        assert_eq!(reported().len(), 2);
        assert_eq!(reported()[1], (9, Some(200)));
        match app.screens.last() {
            Some(Screen::Home(h)) => assert!(!h.list.threads[0].is_unread, "row flips locally"),
            _ => panic!("home"),
        }
    }

    /// Nothing new seen, nothing sent: backing in and out of a thread must
    /// not spend a request each time.
    #[tokio::test]
    async fn leaving_a_thread_with_nothing_new_reports_nothing() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 9, ..Default::default() },
            posts: vec![Post { post_id: 1, post_date: 100, ..Default::default() }],
            seen_date: 100,
            reported_date: 100,
            ..Default::default()
        }));
        app.pop_screen();
        tokio::task::yield_now().await;
        assert!(
            api.threads_read.lock().expect("lock").is_empty(),
            "already reported: no request"
        );
    }

    /// #694: opening the Alerts tab marks them viewed — the badge clears and
    /// XF stops counting them, which is what the web UI does. It is sent
    /// only when there is a count to clear.
    #[tokio::test]
    async fn showing_the_alerts_tab_marks_them_viewed_once() {
        let api = std::sync::Arc::new(RecordingApi::default());
        let mut app = test_app();
        app.api = api.clone();
        app.screens.push(screens::home_state(false));
        app.alerts_unread = 3;
        app.open_inbox(screens::InboxTab::Alerts);
        assert_eq!(app.alerts_unread, 0, "the badge clears immediately");
        tokio::task::yield_now().await;
        let viewed = || api.alerts_viewed.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(viewed(), 1);

        // Revisiting with nothing to clear sends nothing.
        app.open_inbox(screens::InboxTab::Alerts);
        tokio::task::yield_now().await;
        assert_eq!(viewed(), 1, "no count, no request");
    }

    /// #700: clicking a breadcrumb goes back to the place it names — a pop,
    /// so the screen it returns to keeps its scroll, selection and pages.
    /// The last crumb is where you already are, and clicking it must not pop
    /// the screen out from under the reader.
    #[tokio::test]
    async fn clicking_a_breadcrumb_returns_to_that_screen() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(Screen::ThreadList(screens::ThreadListState {
            node_id: 4,
            title: "Windows News".into(),
            ..Default::default()
        }));
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 1, title: "A thread".into(), ..Default::default() },
            ..Default::default()
        }));
        // Home(0), ThreadList(1), ThreadView(2) — the crumb row the frame
        // built for exactly that stack.
        app.crumb_targets = vec![0, 0, 1, 2];

        // The ThreadList crumb: back to the list, thread view gone.
        app.go_to_crumb(2);
        assert_eq!(app.screens.len(), 2);
        assert!(matches!(app.screens.last(), Some(Screen::ThreadList(_))));

        // The crumb for where we already are is inert.
        app.go_to_crumb(2);
        assert_eq!(app.screens.len(), 2, "the last crumb must not pop anything");

        // The brand is the root.
        app.go_to_crumb(usize::MAX);
        assert_eq!(app.screens.len(), 1);
        assert!(matches!(app.screens.last(), Some(Screen::Home(_))));
    }

    /// The sign-in gate owns the screen while it is up: no crumb click may
    /// pop it (the same rule Esc follows, issue #556).
    #[tokio::test]
    async fn a_breadcrumb_click_cannot_escape_the_sign_in_gate() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(screens::login_state());
        app.crumb_targets = vec![0, 0];
        app.go_to_crumb(0);
        assert!(matches!(app.screens.last(), Some(Screen::Login(_))));
    }

    /// #700: infinite scroll — arriving within a screenful of the last
    /// loaded row pulls the next page in, and it stays quiet until then.
    #[tokio::test]
    async fn scrolling_towards_the_end_pulls_the_next_page_in() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.node_id = 4;
            h.list.threads = (0..40)
                .map(|i| Thread { thread_id: i, ..Default::default() })
                .collect();
            h.list.page = 1;
            h.list.pages_loaded = 2;
            h.list.per_page = 20;
            h.list.last_page = 22;
            h.list.visible = 10;
            h.list.sel = 5;
            // What a real load grants (#705): a budget of pages this
            // navigation may pull in.
            h.list.fill_budget = FILL_PAGE_BUDGET;
        }
        app.autoload_more();
        let loading = |app: &App| match app.screens.last() {
            Some(Screen::Home(h)) => h.list.loading,
            _ => panic!("home"),
        };
        assert!(!loading(&app), "near the top: nothing to fetch yet");

        // Within a screenful of the end.
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.sel = 32;
        }
        app.autoload_more();
        assert!(loading(&app), "approaching the end must fetch the next page");

        // And it never fires twice at once.
        app.autoload_more();
        assert!(loading(&app));

        // On the last page there is nothing left to pull.
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.loading = false;
            h.list.page = 21;
            h.list.pages_loaded = 2;
            h.list.sel = 39;
        }
        app.autoload_more();
        assert!(!loading(&app), "no page after the last one");
    }

    /// #705: clicking through forums fast used to leave pages from the
    /// forum just left behind landing in the list that replaced it — the
    /// reply was routed by `node_id` alone, and a fresh load did not reset
    /// the loaded window, so the first fill of a new forum asked for
    /// `page + pages_loaded` carried over from the old one.
    #[tokio::test]
    async fn a_reply_for_a_load_the_list_moved_on_from_is_dropped() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.node_id = 4;
            h.list.threads = vec![Thread { thread_id: 1, ..Default::default() }];
            h.list.page = 1;
            h.list.pages_loaded = 3;
            h.list.load_seq = 7;
        }
        let reply = |page: u32| ForumReply {
            forum: common::models::Forum { node_id: 4, title: "News".into(), ..Default::default() },
            threads: vec![Thread { thread_id: 900 + page, ..Default::default() }],
            sticky: Vec::new(),
            pagination: common::models::Pagination {
                current_page: page,
                last_page: 22,
                total: 431,
                per_page: 20,
            },
        };

        // A page from the generation before this one: dropped whole, whether
        // it would have appended or replaced.
        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 4,
            append: true,
            seq: 6,
            result: Ok(reply(4)),
        });
        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 2,
            append: false,
            seq: 6,
            result: Ok(reply(2)),
        });
        match app.screens.last() {
            Some(Screen::Home(h)) => {
                assert_eq!(h.list.threads.len(), 1, "a stale reply must not land");
                assert_eq!(h.list.page, 1);
                assert_eq!(h.list.pages_loaded, 3);
            }
            _ => panic!("home"),
        }

        // The generation this list IS waiting on lands normally.
        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 1,
            append: false,
            seq: 7,
            result: Ok(reply(1)),
        });
        match app.screens.last() {
            Some(Screen::Home(h)) => assert_eq!(h.list.threads[0].thread_id, 901),
            _ => panic!("home"),
        }
    }

    /// Opening a forum starts its window over. Carrying `pages_loaded` from
    /// the previous forum made the first fill request page 4 of a forum the
    /// reader had just opened, and the cap then claimed pages the list did
    /// not hold (#705).
    #[tokio::test]
    async fn opening_a_forum_starts_a_fresh_window() {
        let mut app = test_app();
        app.screens.push(screens::home_state(true));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            // `dual` is stamped by the renderer; open_list keeps the Home
            // list only when the layout has one.
            h.dual = true;
            h.list.node_id = 4;
            h.list.pages_loaded = 3;
            h.list.per_page = 20;
            h.list.visible = 40;
            h.list.load_seq = 999;
        }
        app.open_list(307, "BSOD".into());
        match app.screens.last() {
            Some(Screen::Home(h)) => {
                assert_eq!(h.list.node_id, 307);
                assert_eq!(h.list.page, 1);
                assert_eq!(h.list.pages_loaded, 1, "the window starts over");
                assert_eq!(h.list.per_page, 0, "and so does the page size");
                assert_ne!(
                    h.list.load_seq, 999,
                    "a fresh load mints a new generation, so replies still in \
                     flight for the old one are already stale"
                );
                assert_eq!(h.list.fill_budget, FILL_PAGE_BUDGET);
            }
            _ => panic!("home"),
        }
    }

    /// #705, the shape the reader actually hit: a pane figure measured on
    /// the screen before must never drive a fill. `visible` is zeroed by a
    /// fresh load and only a render of THIS list stamps it again, so the
    /// automatic fill cannot start against a number that was never about
    /// this forum.
    #[tokio::test]
    async fn the_automatic_fill_waits_for_a_real_measurement() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.node_id = 307;
            h.list.load_seq = 5;
            h.list.fill_budget = FILL_PAGE_BUDGET;
            h.list.visible = 0; // what a fresh load leaves behind
        }
        app.handle_msg(Msg::ForumLoaded {
            node_id: 307,
            page: 1,
            append: false,
            seq: 5,
            result: Ok(ForumReply {
                forum: common::models::Forum {
                    node_id: 307,
                    title: "BSOD".into(),
                    ..Default::default()
                },
                threads: (0..20)
                    .map(|i| Thread { thread_id: i, ..Default::default() })
                    .collect(),
                sticky: Vec::new(),
                pagination: common::models::Pagination {
                    current_page: 1,
                    last_page: 233,
                    total: 4645,
                    per_page: 20,
                },
            }),
        });
        match app.screens.last() {
            Some(Screen::Home(h)) => {
                assert!(!h.list.loading, "no fill against an unmeasured pane");
                assert_eq!(h.list.threads.len(), 20);
            }
            _ => panic!("home"),
        }
    }

    /// The fill is bounded (#705). A pane measured from the previous screen,
    /// or a forum whose pages come back short, must not walk page after page
    /// — which is what "it downloaded many pages and the pagination kept
    /// changing" was.
    #[tokio::test]
    async fn the_viewport_fill_is_bounded() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.node_id = 307;
            h.list.load_seq = 5;
            h.list.fill_budget = FILL_PAGE_BUDGET;
            // A pane claiming room for far more than the forum will ever
            // give it in one page.
            h.list.visible = 500;
        }
        let short_page = |page: u32| ForumReply {
            forum: common::models::Forum { node_id: 307, title: "BSOD".into(), ..Default::default() },
            threads: vec![Thread { thread_id: page, ..Default::default() }],
            sticky: Vec::new(),
            pagination: common::models::Pagination {
                current_page: page,
                last_page: 233,
                total: 4645,
                per_page: 20,
            },
        };
        let mut page = 1u32;
        let mut fetches = 0;
        // Drive the chain: every reply that asks for another page gets one.
        loop {
            app.handle_msg(Msg::ForumLoaded {
                node_id: 307,
                page,
                append: page > 1,
                seq: 5,
                result: Ok(short_page(page)),
            });
            let still_loading = match app.screens.last() {
                Some(Screen::Home(h)) => h.list.loading,
                _ => panic!("home"),
            };
            if !still_loading {
                break;
            }
            page += 1;
            fetches += 1;
            assert!(fetches <= 10, "the fill never stopped: {fetches} pages");
        }
        assert!(
            fetches <= FILL_PAGE_BUDGET as u32,
            "a 233-page forum must not be walked: {fetches} extra pages"
        );
    }

    /// #699: XF fixes the page size server-side (20 rows, and it ignores
    /// `per_page`/`limit`), so a tall terminal was left showing 20 rows in a
    /// pane with room for 45. A reply that does not fill the pane pulls the
    /// next page in and appends it — one page in flight at a time — and it
    /// stops as soon as the pane is full or the pages run out.
    #[tokio::test]
    async fn a_short_page_pulls_in_the_next_one_until_the_pane_is_full() {
        let page_of = |n: u32, count: usize| ForumReply {
            forum: common::models::Forum { node_id: 4, title: "Windows News".into(), ..Default::default() },
            threads: (0..count)
                .map(|i| Thread {
                    thread_id: n * 100 + i as u32,
                    title: format!("thread {n}-{i}"),
                    ..Default::default()
                })
                .collect(),
            sticky: Vec::new(),
            pagination: common::models::Pagination {
                current_page: n,
                last_page: 3,
                total: 50,
                per_page: 20,
            },
        };

        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        // A pane with room for 45 rows.
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.visible = 45;
            h.list.node_id = 4;
            h.list.fill_budget = FILL_PAGE_BUDGET;
        }

        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 1,
            append: false,
            seq: 0,
            result: Ok(page_of(1, 20)),
        });
        let list = |app: &App| match app.screens.last() {
            Some(Screen::Home(h)) => (
                h.list.threads.len(),
                h.list.page,
                h.list.pages_loaded,
                h.list.loading,
            ),
            _ => panic!("home"),
        };
        assert_eq!(list(&app), (20, 1, 1, true), "20 rows in a 45-row pane must ask for more");

        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 2,
            append: true,
            seq: 0,
            result: Ok(page_of(2, 20)),
        });
        assert_eq!(list(&app), (40, 1, 2, true), "still short of 45: keep going");

        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 3,
            append: true,
            seq: 0,
            result: Ok(page_of(3, 10)),
        });
        let (rows, page, loaded, loading) = list(&app);
        assert_eq!((rows, page, loaded), (50, 1, 3));
        assert!(!loading, "the last page ends the fill even though the pane has room");

        // A fresh (non-append) load replaces rather than piling up.
        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 2,
            append: false,
            seq: 0,
            result: Ok(page_of(2, 20)),
        });
        let (rows, page, loaded, _) = list(&app);
        assert_eq!((rows, page, loaded), (20, 2, 1), "a real page turn starts over");
    }

    /// A fill page that fails leaves the rows already on screen alone: the
    /// reader is looking at good data, and an error banner over it would be
    /// a lie about what they can see.
    #[tokio::test]
    async fn a_failed_fill_page_does_not_error_the_rows_already_shown() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            h.list.node_id = 4;
            h.list.threads = vec![Thread { thread_id: 1, ..Default::default() }];
            h.list.page = 1;
            h.list.pages_loaded = 1;
            h.list.loading = true;
        }
        app.handle_msg(Msg::ForumLoaded {
            node_id: 4,
            page: 2,
            append: true,
            seq: 0,
            result: Err(TaskError {
                message: "gateway timeout".into(),
                code: None,
                max_page: None,
                kind: TaskErrorKind::Other,
            }),
        });
        match app.screens.last() {
            Some(Screen::Home(h)) => {
                assert!(h.list.error.is_none(), "no banner over good rows");
                assert_eq!(h.list.threads.len(), 1);
                assert!(!h.list.loading);
            }
            _ => panic!("home"),
        }
    }

    /// #680: a catalog page fills the topmost screen it belongs to, and a
    /// reply that arrives for a screen which is not waiting on one is
    /// dropped — the same stale-reply discipline every other load follows.
    #[tokio::test]
    async fn a_catalog_page_fills_its_screen_and_a_stale_reply_is_dropped() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.execute_action(Action::OpenMediaGallery);

        app.handle_msg(Msg::MediaLoaded {
            page: 1,
            result: Ok(common::models::MediaListReply {
                media: vec![common::models::MediaItem {
                    media_id: 33005,
                    title: "Registry Explained".into(),
                    ..Default::default()
                }],
                pagination: common::models::Pagination {
                    current_page: 1,
                    last_page: 4,
                    total: 80,
                    per_page: 12,
                },
            }),
        });
        let Some(Screen::MediaGallery(m)) = app.screens.last() else {
            panic!("the gallery must still be on top");
        };
        assert!(!m.loading, "the fetch is finished");
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.last_page, 4);
        assert_eq!(m.total, 80);

        // Nothing is in flight now, so a late second reply must not land.
        app.handle_msg(Msg::MediaLoaded {
            page: 2,
            result: Ok(common::models::MediaListReply::default()),
        });
        let Some(Screen::MediaGallery(m)) = app.screens.last() else {
            panic!("the gallery must still be on top");
        };
        assert_eq!(m.items.len(), 1, "a reply nobody is waiting on must be dropped");
        assert_eq!(m.page, 1, "and it must not renumber the page either");
    }

    /// #674: the idle-skip predicate — an idle session needs no redraw,
    /// a pending write gate (its countdown animates) or any in-flight
    /// fetch's spinner does.
    #[test]
    fn needs_continuous_redraw_tracks_animation_sources_only() {
        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        assert!(
            !app.needs_continuous_redraw(),
            "an idle session must not force redraws"
        );

        // A static busy text (compose "Sending…") does NOT animate.
        app.screens.clear();
        app.screens.push(Screen::Compose(screens::ComposeState {
            busy: true,
            ..Default::default()
        }));
        assert!(
            !app.needs_continuous_redraw(),
            "static busy text must not force redraws"
        );

        // A pending write gate animates its countdown.
        app.client.write_gate.penalize(Duration::from_secs(30));
        assert!(app.needs_continuous_redraw());

        // A loading screen animates its spinner.
        app.screens.clear();
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            loading: true,
            ..Default::default()
        }));
        assert!(app.needs_continuous_redraw());
    }

    /// #674: the toast-expiry boundary marks dirty exactly once — while a
    /// toast is live nothing redraws, the tick that clears it does.
    #[test]
    fn toast_expiry_marks_dirty_exactly_once() {
        let mut app = test_app();
        app.set_status("Reply posted.");
        assert!(!app.expire_status_toast(), "a fresh toast is not expired");
        app.status_set_at = Some(
            std::time::Instant::now() - Duration::from_secs(STATUS_TOAST_SECS + 1),
        );
        assert!(app.expire_status_toast(), "the boundary tick clears it");
        assert!(!app.expire_status_toast(), "and only once");
    }

    /// The signal forwarder keeps opting the process out of the default
    /// disposition for as long as it runs: a second SIGTERM delivered while
    /// the first one's teardown is still in progress must arrive as another
    /// "quit", not revert to default disposition and kill the process with
    /// the terminal still raw. (Registered streams are process-global, so
    /// this test leaves a handler behind — nothing else in the suite relies
    /// on the default one.)
    #[cfg(unix)]
    #[tokio::test]
    async fn signals_keep_arriving_as_quit_after_the_first_one() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward_signals(tx));
        // Let the spawned task run its first poll: the three `signal()`
        // registrations happen there, before the first await. The settle
        // time is the guard against killing our own process while the
        // default disposition is still in force.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // SAFETY: kills this test process with SIGTERM. The handler was
        // registered by `forward_signals` above, so the default disposition
        // (die, terminal raw) is no longer in force.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            matches!(first, Ok(Some(Msg::Notice(ref n))) if n == "quit"),
            "the first SIGTERM must arrive as quit"
        );

        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        let second = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            matches!(second, Ok(Some(Msg::Notice(ref n))) if n == "quit"),
            "the second SIGTERM must arrive as quit too — the loop must not end"
        );

        task.abort();
    }

    /// An abort cannot stop a `PostToggled` that is already sitting in the
    /// channel when the session ends. It may still flip the notice, but it
    /// must not reload the thread as a signed-out client.
    #[tokio::test]
    async fn a_post_toggled_landing_after_teardown_reloads_nothing() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: Thread { thread_id: 42, ..Default::default() },
            page: 2,
            ..Default::default()
        }));
        let toggle = |result: Result<Toggle, TaskError>| Msg::PostToggled {
            verb: PostVerb::Like,
            result,
        };

        // Live session: the ♡ counts live in the baked post lines, so the
        // page re-fetch keeps the reader's place (issue #538).
        app.handle_msg(toggle(Ok(Toggle::Inserted)));
        assert!(
            matches!(&app.keep_thread_position, Some((42, 2, ..))),
            "a live session reloads the page: {:?}",
            app.keep_thread_position
        );
        app.keep_thread_position = None;

        // Same message after the session ended: no reload, nothing marked.
        app.me = None;
        app.handle_msg(toggle(Ok(Toggle::Inserted)));
        assert!(
            app.keep_thread_position.is_none(),
            "a toggle landing after teardown must not reload the thread"
        );
    }

    /// Issue #570: Ctrl+L on the sign-in screen with no session is not a
    /// sign-out — there is nothing to sign out of. It used to tear the
    /// screen down and push a fresh Idle one anyway, discarding the short
    /// link the user was about to open on their phone.
    #[test]
    fn ctrl_l_on_a_waiting_login_screen_leaves_it_untouched() {
        let mut app = test_app();
        app.screens.push(screens::login_state());
        if let Some(Screen::Login(ls)) = app.screens.last_mut() {
            ls.stage = screens::LoginStage::Waiting;
            ls.url = "https://windowsforum.com/tui-start/abc123".into();
        }
        app.status = "Login link \u{2192} clipboard + login-url.txt".into();
        assert!(app.me.is_none());

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));

        assert_eq!(app.screens.len(), 1, "Ctrl+L must not push a second screen");
        match app.screens.last() {
            Some(Screen::Login(ls)) => {
                assert!(
                    matches!(ls.stage, screens::LoginStage::Waiting),
                    "the stage must survive"
                );
                assert_eq!(
                    ls.url, "https://windowsforum.com/tui-start/abc123",
                    "the link must survive"
                );
            }
            _ => panic!("expected the Login screen to survive untouched"),
        }
        assert_ne!(app.status, "Logged out.", "no sign-out happened, so no such status");
    }

    /// #612: the keys card is modal — "any key closes" means the key is
    /// consumed by closing, never also dispatched to the screen underneath.
    /// `q` on Home used to quit the client through the card, and `j`/`k`
    /// moved the hidden list behind it.
    #[tokio::test]
    async fn the_keys_card_consumes_its_dismissing_key() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.screens.push(screens::home_state(false));

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.show_help, "test setup: the card is open");

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.show_help, "the card closed");
        assert!(!app.should_quit, "q closed the card — it must not quit the client");
        assert!(
            matches!(app.screens.last(), Some(Screen::Home(_))),
            "the screen underneath is untouched"
        );
    }

    /// #613: the profile's DM key used to be `c`, which the global-nav
    /// block intercepted to open the Inbox first — the advertised "send DM"
    /// key could never fire. Rebound to `d`: through the real handle_key
    /// path it now pushes the DM composer with the member pre-filled, while
    /// the global `c` still opens the Inbox.
    #[tokio::test]
    async fn the_profile_dm_key_opens_the_dm_composer() {
        let mut app = test_app();
        app.me = Some(User { user_id: 7, username: "kemical".into(), ..Default::default() });
        app.open_profile(42, "SysAdmin");
        if let Some(Screen::Profile(p)) = app.screens.last_mut() {
            p.user = Some(User { user_id: 42, username: "SysAdmin".into(), ..Default::default() });
        }

        // The global `c` still opens the Inbox from the profile...
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(
            matches!(app.screens.last(), Some(Screen::Inbox(_))),
            "the global `c` is untouched"
        );
        app.screens.pop();

        // ...and `d` reaches the profile's own DM action.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let Some(Screen::NewConversation(state)) = app.screens.last() else {
            panic!("expected the DM composer, got {:?}", app.screens.last().map(|s| s.title()));
        };
        assert_eq!(state.recipients, "SysAdmin, ", "the member is pre-filled");
    }

    /// Issue #559: a pasted tab must not desync the caret from the drawn
    /// text. `cell_width("\t") == 1` but ratatui's grapheme filter draws a
    /// raw tab as zero cells, so leaving it in the buffer put the caret one
    /// column right of where the text actually ended.
    #[test]
    fn pasting_a_tab_into_the_composer_keeps_the_caret_at_the_drawn_text_end() {
        let mut app = test_app();
        app.screens.push(Screen::Compose(screens::ComposeState::default()));

        app.handle_paste("a\tb".into());

        let Some(Screen::Compose(cs)) = app.screens.last() else {
            panic!("expected the Compose screen");
        };
        assert_eq!(cs.body, "a   b", "the tab must expand to spaces, not stay raw");
        let (_, col) = crate::editor::caret_position(&cs.body, 80, cs.body_cursor);
        assert_eq!(
            col,
            cs.body.chars().count(),
            "the caret column must equal where the drawn text actually ends"
        );
    }

    /// Issue #547: `Enter` is advertised as "restart login" for the whole
    /// Login screen, so it must restart while the client is polling — and the
    /// restart must actually supersede the running flow: abort its poll loop
    /// and make every message it already queued stale, or a denied approval's
    /// late `LoginFailed`/`LoginReady` lands on the fresh flow's screen.
    #[tokio::test]
    async fn enter_while_waiting_restarts_login_and_supersedes_the_old_flow() {
        let mut app = test_app();
        app.screens.push(screens::login_state());
        if let Some(Screen::Login(ls)) = app.screens.last_mut() {
            ls.stage = screens::LoginStage::Waiting;
            ls.url = "https://windowsforum.com/tui-start/old".into();
        }

        // 1. The key the bar advertises has to reach `begin_login` from
        //    `Waiting`, not only from `Idle`.
        let action = app
            .screens
            .last_mut()
            .expect("login screen is on top")
            .on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(action, Action::LoginBegin),
            "Enter must restart the login flow while it is polling"
        );

        // 2. A restart aborts the flow that was running. (The stand-in task is
        //    a plain sleep: the real login task must never be polled in a test,
        //    it would talk to the live site.)
        let running = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(3600)).await });
        app.login_task = Some(running.abort_handle());
        let before = app.login_generation;

        app.begin_login();
        let fresh = app.login_task.take().expect("the new flow is tracked");
        fresh.abort();
        assert!(
            running.await.unwrap_err().is_cancelled(),
            "restarting must abort the superseded poll loop"
        );

        let generation = app.login_generation;
        assert_ne!(before, generation, "each restart gets its own generation");
        match app.screens.last() {
            Some(Screen::Login(ls)) => {
                assert!(ls.busy, "the restarted flow is working again");
                assert!(matches!(ls.stage, screens::LoginStage::Idle));
                assert!(ls.url.is_empty(), "the superseded link must be cleared");
            }
            _ => unreachable!(),
        }

        // 3. The superseded flow's messages are ignored...
        app.handle_msg(Msg::LoginReady {
            generation: before,
            url: "https://windowsforum.com/tui-start/stale".into(),
        });
        app.handle_msg(Msg::LoginFailed {
            generation: before,
            message: "login link expired".into(),
        });
        match app.screens.last() {
            Some(Screen::Login(ls)) => {
                assert!(ls.url.is_empty(), "a stale link must not be shown");
                assert!(ls.error.is_none(), "a stale failure must not be reported");
                assert!(ls.busy, "a stale message must not end the live flow");
                assert!(matches!(ls.stage, screens::LoginStage::Idle));
            }
            _ => unreachable!(),
        }

        // ...while the live flow's still land.
        app.handle_msg(Msg::LoginFailed {
            generation,
            message: "timed out waiting for approval".into(),
        });
        match app.screens.last() {
            Some(Screen::Login(ls)) => {
                assert!(!ls.busy);
                assert_eq!(ls.error.as_deref(), Some("timed out waiting for approval"));
            }
            _ => unreachable!(),
        }
    }

    /// Issue #655: `login-url.txt` is written 0600, like every other file
    /// in the config dir — the link is not a credential, but a shared
    /// machine has no business reading it either.
    #[cfg(unix)]
    #[tokio::test]
    async fn login_url_file_is_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let mut app = test_app();
        let scratch = scratch_config_dir();
        std::fs::create_dir_all(&scratch).unwrap();
        app.handle_msg(Msg::LoginReady {
            generation: app.login_generation,
            url: "https://windowsforum.com/tui-start/abc123".into(),
        });
        let path = app.client.store_path().with_file_name("login-url.txt");
        let mode = std::fs::metadata(&path).expect("the login link is persisted").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "login-url.txt must be owner-only: {path:?}");
    }

    /// Issue #524: a second `start_pollers` (the re-login path) must not
    /// stack a second alerts/conversations pair on top of the first, and
    /// `logout` must abort the pair entirely and clear the unread counters
    /// they fed — not leave orphaned loops still polling after sign-out.
    #[tokio::test]
    async fn start_pollers_replaces_the_previous_pair_and_logout_stops_them() {
        let mut app = test_app();

        app.start_pollers();
        assert_eq!(app.poller_handles.len(), 2, "one alerts + one conversations loop");
        let first_pair: Vec<_> = app.poller_handles.iter().map(|h| h.id()).collect();

        // Simulate logout -> login again without this fix: doubles the rate.
        app.start_pollers();
        assert_eq!(app.poller_handles.len(), 2, "re-login must not stack a second pair");
        let second_pair: Vec<_> = app.poller_handles.iter().map(|h| h.id()).collect();
        assert_ne!(
            first_pair, second_pair,
            "the first pair must actually be replaced (aborted), not merely uncounted"
        );

        app.logout();
        assert!(app.poller_handles.is_empty(), "logout must abort every poller");
        assert_eq!(app.alerts_unread, 0, "logout must clear the stale alerts count");
        assert_eq!(app.convos_unread, 0, "logout must clear the stale conversations count");
    }

    /// Issue #520: the app used to swallow Esc for every screen. A composer
    /// whose post is already in flight must survive it (with an audible
    /// refusal), an idle one must run its own discard arm, Search must be
    /// able to leave edit mode without closing, and a failure that arrives
    /// after the composer is gone must still be visible.
    // Async: the draft lifecycle now spawns relay calls (#716).
    #[tokio::test]
    async fn esc_reaches_the_screen_first_and_never_pops_a_busy_composer() {
        let esc = || KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let composer = |busy: bool| {
            Screen::Compose(screens::ComposeState {
                target: Some(ComposeTarget::ThreadReply {
                    thread_id: 1,
                    thread_title: "A thread".into(),
                }),
                body: "draft".into(),
                busy,
                ..Default::default()
            })
        };

        let mut app = test_app();
        app.screens.push(screens::home_state(false));
        app.screens.push(composer(true));
        app.handle_key(esc());
        assert_eq!(app.screens.len(), 2, "a busy composer must not be popped mid-send");
        assert!(
            app.status.contains("Esc"),
            "the refusal must not be silent: {:?}",
            app.status
        );

        // Idle: the composer's own Esc arm discards it.
        app.screens.pop();
        app.screens.push(composer(false));
        app.handle_key(esc());
        assert_eq!(app.screens.len(), 1, "Esc discards an idle composer");

        // Search: Esc leaves edit mode, and only then closes the screen.
        app.screens.push(Screen::Search(screens::SearchState {
            query: "edge".into(),
            input_mode: true,
            ..Default::default()
        }));
        app.handle_key(esc());
        assert_eq!(app.screens.len(), 2, "Esc in the query field must not close Search");
        assert!(matches!(app.screens.last(), Some(Screen::Search(s)) if !s.input_mode));
        app.handle_key(esc());
        assert_eq!(app.screens.len(), 1, "a second Esc leaves Search");

        // A late failure with no composer left on the stack still surfaces.
        app.status.clear();
        app.handle_msg(Msg::ReplySent(Err(TaskError {
            message: "Flood control".into(),
            code: None,
            max_page: None,
            kind: TaskErrorKind::Other,
        })));
        assert!(
            app.status.contains("Flood control"),
            "a late Err was dropped: {:?}",
            app.status
        );
    }

    /// A search hit's `p` (open profile) must resolve id 0 through
    /// `find_user` rather than ever fetching `/users/0` (issue #521).
    #[test]
    fn resolve_profile_id_prefers_a_found_id_only_when_the_caller_had_none() {
        // Search result: no id, but find-name resolved one.
        assert_eq!(resolve_profile_id(0, Some(42)), 42);
        // Search result: no id, and find-name came up empty (deleted user,
        // typo'd/ambiguous name, network error) — do not synthesize one.
        assert_eq!(resolve_profile_id(0, None), 0);
        // Caller already had a real id (thread/post author): never let a
        // find-name result override it.
        assert_eq!(resolve_profile_id(7, Some(999)), 7);
        assert_eq!(resolve_profile_id(7, None), 7);
    }

    /// Two `load_forum` calls can finish out of order (the rate-limit gate
    /// spaces starts, not completions). The slower reply must not land in the
    /// list the faster one already filled.
    #[test]
    fn a_forum_reply_only_reaches_the_list_that_asked_for_it() {
        let home = |node_id: u32| {
            Screen::Home(screens::HomeState {
                list: screens::ThreadListState {
                    node_id,
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let pushed = |node_id: u32| {
            Screen::ThreadList(screens::ThreadListState {
                node_id,
                title: format!("node {node_id}"),
                ..Default::default()
            })
        };

        // Home on Latest (0), Security (46) pushed on top; News (4) is in
        // flight from the screen the user already popped.
        let mut stack = vec![home(0), pushed(46)];
        assert!(list_showing(&mut stack, 4).is_none(), "stale News reply must be dropped");
        assert_eq!(
            list_showing(&mut stack, 46).map(|l| l.node_id),
            Some(46),
            "the Security reply still lands"
        );
        assert_eq!(list_showing(&mut stack, 0).map(|l| l.node_id), Some(0));

        // Topmost wins when two lists show the same forum.
        let mut same = vec![home(4), pushed(4)];
        list_showing(&mut same, 4).unwrap().title = "hit".into();
        assert!(matches!(&same[1], Screen::ThreadList(l) if l.title == "hit"));
        assert!(matches!(&same[0], Screen::Home(h) if h.list.title.is_empty()));
    }

    // ================= mouse and touch =================
    //
    // Terminals deliver a tap as a left click, a two-finger scroll as a
    // wheel and a long press as a right click, so these tests are the touch
    // tests too. Every one of them drives the REAL `App::draw`, so the hit
    // map under test is the one the client registers, at the geometry the
    // renderers actually laid out — not a hand-built map that can drift.

    use crate::hit::HitPane;
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    /// Draw one frame into a `w x h` test terminal. The frame is thrown away;
    /// what is kept is `App::hits` (and `App::screen_rows`).
    fn frame(app: &mut App, w: u16, h: u16) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
            .expect("terminal");
        term.draw(|f| app.draw(f)).expect("draw");
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Press and release in the same cell: a click.
    fn click(app: &mut App, x: u16, y: u16) {
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    }

    /// A click a moment later — the double-click window is 400 ms, and two
    /// clicks in a test land in the same microsecond, so a *separate* click
    /// says so explicitly instead of depending on the wall clock.
    fn click_later(app: &mut App, x: u16, y: u16) {
        app.last_click_instant = None;
        click(app, x, y);
    }

    /// The first cell whose topmost hit satisfies `pred`, scanning the frame
    /// left to right, top to bottom.
    fn find_hit(app: &App, w: u16, h: u16, pred: impl Fn(&Hit) -> bool) -> Option<(u16, u16)> {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .find(|(x, y)| app.hits.at(*x, *y).is_some_and(&pred))
    }

    fn thread(id: u32) -> Thread {
        Thread {
            thread_id: id,
            title: format!("Thread {id}"),
            view_url: Some(format!("https://windowsforum.com/threads/{id}/")),
            ..Default::default()
        }
    }

    /// A signed-in client on the dual-pane Home: three forums, four threads.
    fn home_app() -> App {
        let mut app = test_app();
        app.me = Some(User {
            user_id: 1,
            username: "Mike".into(),
            ..Default::default()
        });
        app.screens.push(Screen::Home(screens::HomeState {
            tree: screens::ForumTreeState {
                nodes: (1..=3)
                    .map(|node_id| Node {
                        node_id,
                        title: format!("Forum {node_id}"),
                        node_type: "Forum".into(),
                        view_url: Some(format!("https://windowsforum.com/forums/{node_id}/")),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            list: screens::ThreadListState {
                node_id: 1,
                title: "Forum 1".into(),
                threads: (1..=4).map(thread).collect(),
                page: 1,
                last_page: 1,
                ..Default::default()
            },
            ..Default::default()
        }));
        app
    }

    /// The Home frame's geometry, spelled out once because the rest of these
    /// tests read it: header row 0, body rows 1..=21, key bar row 22, status
    /// #700: the crumb row really is clickable — the hit boxes land on the
    /// crumb text the frame drew, and each one names the screen it stands
    /// for. Measured off a drawn frame rather than recomputed, because a
    /// second guess at the header's overflow ladder is exactly the bug.
    #[test]
    fn the_breadcrumb_row_maps_each_crumb_to_its_screen() {
        let mut app = home_app();
        app.screens.push(Screen::ThreadList(screens::ThreadListState {
            node_id: 4,
            title: "Windows News".into(),
            ..Default::default()
        }));
        frame(&mut app, 120, 24);

        // Row 0 is the header. Walk it and collect what each cell answers.
        let mut seen: Vec<(usize, u16)> = Vec::new();
        for x in 0..120u16 {
            if let Some(Hit::Crumb(i)) = app.hits.at(x, 0) {
                seen.push((*i, x));
            }
        }
        assert!(!seen.is_empty(), "the crumb row registered nothing");
        // The brand at the far left is the root.
        assert!(
            seen.iter().any(|(i, x)| *i == usize::MAX && *x < 6),
            "the brand must be the root crumb: {seen:?}"
        );
        // Every other index is a real screen this stack has.
        for (i, _) in &seen {
            assert!(
                *i == usize::MAX || app.crumb_targets.get(*i).is_some(),
                "crumb {i} names no screen: targets {:?}",
                app.crumb_targets
            );
        }
        // The last crumb is the screen on top, and clicking it changes
        // nothing; the one before it pops back one place.
        let deepest = seen.iter().map(|(i, _)| *i).filter(|i| *i != usize::MAX).max().unwrap();
        let depth_before = app.screens.len();
        app.go_to_crumb(deepest);
        assert_eq!(app.screens.len(), depth_before, "the current crumb is inert");
        if deepest > 0 {
            app.go_to_crumb(deepest - 1);
            assert!(app.screens.len() < depth_before, "an ancestor crumb pops back");
        }
    }

    /// row 23. The Forums panel is 37 wide (inner x 1..=35, y from 2): the
    /// QUICK block is rows 2..=6 (` QUICK`, then L/1/2/3), row 7 is blank and
    /// the nodes start at row 8. The thread list's inner starts at x 38, with
    /// the column header on row 2 and the first thread on row 3.
    #[test]
    fn the_home_frame_maps_quick_keys_forum_rows_and_thread_rows_to_their_own_panes() {
        let mut app = home_app();
        frame(&mut app, 120, 24);

        // QUICK rows are keys, not list rows: clicking one presses it.
        assert_eq!(app.hits.at(3, 3), Some(&Hit::Key("L")));
        assert_eq!(app.hits.at(3, 4), Some(&Hit::Key("1")));
        // The ` QUICK` header and the blank row below the block are inert —
        // the pane underneath is all they answer with.
        assert_eq!(app.hits.at(3, 2), Some(&Hit::Pane(HitPane::Tree)));
        assert_eq!(app.hits.at(3, 7), Some(&Hit::Pane(HitPane::Tree)));

        // Forum rows, and the pane under them.
        assert_eq!(app.hits.at(3, 8), Some(&Hit::Row(0)));
        assert_eq!(app.hits.at(3, 10), Some(&Hit::Row(2)));
        assert_eq!(app.hits.pane_at(3, 8), Some(HitPane::Tree));

        // The thread list is the other pane, with its own row indices.
        assert_eq!(app.hits.at(60, 3), Some(&Hit::Row(0)));
        assert_eq!(app.hits.at(60, 5), Some(&Hit::Row(2)));
        assert_eq!(app.hits.pane_at(60, 5), Some(HitPane::List));

        // A click in the right-hand pane takes the keyboard with it (#549's
        // rects, now for clicks as well as the wheel).
        assert!(matches!(app.screens.last(), Some(Screen::Home(h)) if h.focus == screens::Pane::Tree));
        click(&mut app, 60, 5);
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!(h.focus, screens::Pane::List, "the pointer takes the keyboard");
        assert_eq!(h.list.sel, 2, "and the row it landed on is selected");
    }

    /// The list rule: the first click on a row selects it, and clicking the
    /// row that is *already* selected opens it — the same thing Enter does,
    /// through the same dispatch.
    #[tokio::test]
    async fn a_click_selects_the_row_and_clicking_it_again_opens_the_thread() {
        let mut app = home_app();
        frame(&mut app, 120, 24);

        click(&mut app, 60, 5);
        assert_eq!(app.screens.len(), 1, "the first click only moves the selection");
        assert!(matches!(app.screens.last(), Some(Screen::Home(h)) if h.list.sel == 2));

        frame(&mut app, 120, 24);
        click_later(&mut app, 60, 5);
        assert_eq!(app.screens.len(), 2, "the second click on the same row opens it");
        let Some(Screen::ThreadView(v)) = app.screens.last() else {
            panic!("expected the thread view");
        };
        assert_eq!(v.thread.thread_id, 3, "and it opens the row that was clicked");
    }

    /// The other way to open: a double click, which never waits for a second
    /// frame — the second press is the gesture.
    #[tokio::test]
    async fn a_double_click_on_an_unselected_row_opens_it_straight_away() {
        let mut app = home_app();
        frame(&mut app, 120, 24);
        // Row 3 of the list, which nothing has selected yet.
        click(&mut app, 60, 6);
        click(&mut app, 60, 6);
        assert_eq!(app.screens.len(), 2, "a double click opens without a second frame");
        let Some(Screen::ThreadView(v)) = app.screens.last() else {
            panic!("expected the thread view");
        };
        assert_eq!(v.thread.thread_id, 4);
    }

    /// Right click (a long press on a phone terminal) is "open this on the
    /// site". The URL resolution is asserted on its own rather than through
    /// `open_url`, which spawns the user's real browser.
    #[test]
    fn a_right_click_on_a_thread_row_selects_it_and_resolves_its_web_url() {
        let mut app = home_app();
        frame(&mut app, 120, 24);

        assert_eq!(
            app.right_click_url((60, 5)),
            Some("https://windowsforum.com/threads/3/".to_string())
        );
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!(h.list.sel, 2, "the row it opens is the row it selected");

        // A row whose payload carried no `view_url` says so instead of
        // silently doing nothing (the same rule `u` follows).
        if let Some(Screen::Home(h)) = app.screens.last_mut() {
            for t in &mut h.list.threads {
                t.view_url = None;
            }
        }
        frame(&mut app, 120, 24);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), 60, 5));
        assert_eq!(app.status, "Nothing here to open on the site.");
    }

    /// The key bar is a row of buttons: clicking a cap presses that key —
    /// through `handle_key`, so nothing about a click bypasses the gating a
    /// keypress goes through. The header's unread badges open the Inbox on
    /// their own tab.
    #[tokio::test]
    async fn key_bar_caps_press_their_key_and_header_badges_open_their_inbox_tab() {
        let mut app = home_app();
        frame(&mut app, 120, 24);

        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::Key("j/k")).expect("a j/k cap");
        assert_eq!(y, 22, "the key bar is the second row from the bottom");
        click(&mut app, x, y);
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!(h.tree.sel, 1, "a compound cap presses its first key: j moves down");

        // `test_app` starts with 5 unread DMs and 3 unread alerts, so both
        // badges are drawn.
        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::Badge(screens::InboxTab::Alerts))
            .expect("an Alerts badge");
        assert_eq!(y, 0, "the badges are on the header band");
        click(&mut app, x, y);
        let Some(Screen::Inbox(ib)) = app.screens.last() else {
            panic!("expected the Inbox");
        };
        assert_eq!(ib.tab, screens::InboxTab::Alerts);
    }

    /// The thread view's three targets: a link row, an image (its caption and
    /// the rows reserved for it), and the post card everything else in the
    /// post belongs to.
    #[test]
    fn the_thread_view_maps_links_images_and_posts_and_a_click_selects_the_post() {
        let mut app = test_app();
        app.me = Some(User {
            user_id: 1,
            username: "Mike".into(),
            ..Default::default()
        });
        let post = |post_id: u32, message: &str| Post {
            post_id,
            user_id: 7,
            username: "kemical".into(),
            message: message.to_string(),
            ..Default::default()
        };
        let mut second = post(2, "second post");
        second.attachments = vec![Attachment {
            attachment_id: 5,
            filename: "shot.png".into(),
            content_type: "image/png".into(),
            ..Default::default()
        }];
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: thread(9),
            posts: vec![post(1, "see https://example.com/a for details"), second],
            page: 1,
            last_page: 1,
            ..Default::default()
        }));
        frame(&mut app, 80, 24);

        let link = find_hit(&app, 80, 24, |h| {
            h == &Hit::Link("https://example.com/a".to_string())
        })
        .expect("a link");
        assert_eq!(
            app.hits.post_at(link.0, link.1),
            Some(0),
            "the post under a link is still the post it belongs to"
        );
        // Both halves of a link are clickable: the ` [1]` marker beside the
        // label in the body (four cells — the span, not the row), and the
        // whole `[1] url` row listed under the post.
        let link_rows: Vec<u16> = (0..24)
            .filter(|y| (0..80).any(|x| matches!(app.hits.at(x, *y), Some(Hit::Link(_)))))
            .collect();
        assert!(
            link_rows.len() >= 2,
            "the inline marker and the link row: {link_rows:?}"
        );
        let marker: Vec<u16> = (0..80)
            .filter(|x| matches!(app.hits.at(*x, link_rows[0]), Some(Hit::Link(_))))
            .collect();
        assert_eq!(
            marker.len(),
            3,
            "the inline marker is the `[1]` span, not the whole row"
        );
        let image = find_hit(&app, 80, 24, |h| h == &Hit::Image(1)).expect("an image caption");
        assert_eq!(app.hits.post_at(image.0, image.1), Some(1));

        // A click on a post body selects that post and invalidates the
        // cached width, which is what `n`/`N` do (issue #542).
        let (x, y) = find_hit(&app, 80, 24, |h| h == &Hit::Post(1)).expect("the second post");
        click(&mut app, x, y);
        let Some(Screen::ThreadView(v)) = app.screens.last() else {
            panic!("expected the thread view");
        };
        assert_eq!(v.sel_post, 1);
        assert_eq!(v.width, 0, "the gutter colour is baked in, so the lines must rebuild");
    }

    /// The Inbox: its tab chips switch tabs in place, and its rows are two
    /// lines each on the conversations tab.
    ///
    /// `#[tokio::test]`, not `#[test]`: switching to Alerts marks them
    /// viewed (#694), and that spawns.
    #[tokio::test]
    async fn inbox_tab_chips_switch_tabs_and_two_line_rows_map_both_of_their_lines() {
        let mut app = test_app();
        app.me = Some(User {
            user_id: 1,
            username: "Mike".into(),
            ..Default::default()
        });
        app.screens.push(Screen::Inbox(screens::InboxState {
            convos: screens::ConversationsState {
                conversations: (1..=3)
                    .map(|conversation_id| Conversation {
                        conversation_id,
                        title: format!("DM {conversation_id}"),
                        ..Default::default()
                    })
                    .collect(),
                page: 1,
                last_page: 1,
                ..Default::default()
            },
            alerts: screens::AlertsState {
                alerts: (1..=3)
                    .map(|alert_id| Alert {
                        alert_id,
                        username: "kemical".into(),
                        content_type: "post".into(),
                        action: "quote".into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        }));
        frame(&mut app, 120, 24);

        // Inner x 1, tabs on row 2, rows from row 4 — two lines each.
        assert_eq!(app.hits.at(10, 4), Some(&Hit::Row(0)));
        assert_eq!(app.hits.at(10, 5), Some(&Hit::Row(0)), "the meta line is the row");
        assert_eq!(app.hits.at(10, 6), Some(&Hit::Row(1)));

        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::Tab(screens::InboxTab::Alerts))
            .expect("an Alerts chip");
        assert_eq!(y, 2, "the tab row is the panel's first inner row");
        click(&mut app, x, y);
        assert!(matches!(app.screens.last(), Some(Screen::Inbox(ib)) if ib.tab == screens::InboxTab::Alerts));

        // Alerts are one line each; clicking the second selects it.
        frame(&mut app, 120, 24);
        assert_eq!(app.hits.at(10, 5), Some(&Hit::Row(1)));
        click(&mut app, 10, 5);
        let Some(Screen::Inbox(ib)) = app.screens.last() else {
            panic!("expected the Inbox");
        };
        assert_eq!(ib.alerts.sel, 1);
    }

    /// Search hits are a title line plus a dim snippet line; both belong to
    /// the hit, and a click selects it.
    #[test]
    fn search_hits_map_their_title_and_snippet_lines_to_one_row() {
        let mut app = test_app();
        app.me = Some(User {
            user_id: 1,
            username: "Mike".into(),
            ..Default::default()
        });
        let mut search = screens::SearchState {
            query: "edge".into(),
            page: 1,
            last_page: 1,
            ..Default::default()
        };
        search.set_results(
            (1..=3)
                .map(|content_id| SearchHit {
                    content_type: "thread".into(),
                    content_id,
                    title: format!("Hit {content_id}"),
                    message: "a body long enough to make a snippet".into(),
                    ..Default::default()
                })
                .collect(),
        );
        app.screens.push(Screen::Search(search));
        frame(&mut app, 80, 24);

        // Panel inner y 2: query, chips, rule, count, blank, then results.
        assert_eq!(app.hits.at(10, 7), Some(&Hit::Row(0)));
        assert_eq!(app.hits.at(10, 8), Some(&Hit::Row(0)), "the snippet is the row");
        assert_eq!(app.hits.at(10, 9), Some(&Hit::Row(1)));
        // The `/ query` row is a field, not a result.
        assert_eq!(app.hits.at(10, 2), Some(&Hit::Field(0)));

        click(&mut app, 10, 9);
        let Some(Screen::Search(s)) = app.screens.last() else {
            panic!("expected Search");
        };
        assert_eq!(s.sel, 1);
    }

    /// Every overlay: its rows work, a click off it closes it, and nothing
    /// behind the glass can be reached through it.
    #[tokio::test]
    async fn overlay_rows_run_and_a_click_outside_closes_each_overlay() {
        // 1. The go-to palette: its rows run, and the body under it is gone.
        let mut app = home_app();
        app.open_palette();
        frame(&mut app, 120, 24);
        assert!(
            !matches!(app.hits.at(60, 5), Some(Hit::Row(_))),
            "the body must not be clickable through an overlay"
        );
        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::PaletteRow(0)).expect("a palette row");
        click(&mut app, x, y);
        assert!(app.palette.is_none(), "running a row closes the palette");
        // Row 0 with a forum in view is "New thread in <forum>" — the click
        // ran it, exactly as Enter on that row would have.
        assert!(
            matches!(
                app.screens.last(),
                Some(Screen::Compose(c))
                    if matches!(c.target, Some(ComposeTarget::NewThread { node_id: 1 }))
            ),
            "the palette row must run its own target"
        );

        // 2. ...and a click off it just closes it.
        let mut app = home_app();
        app.open_palette();
        frame(&mut app, 120, 24);
        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::CloseOverlay).expect("an outside cell");
        click(&mut app, x, y);
        assert!(app.palette.is_none(), "a click outside closes the palette");

        // 3. The keys card.
        app.show_help = true;
        frame(&mut app, 120, 24);
        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::CloseOverlay).expect("an outside cell");
        click(&mut app, x, y);
        assert!(!app.show_help, "a click outside closes the keys card");

        // 4. The `g` which-key.
        app.prefix.arm();
        frame(&mut app, 120, 24);
        let (x, y) = find_hit(&app, 120, 24, |h| h == &Hit::CloseOverlay).expect("an outside cell");
        click(&mut app, x, y);
        assert!(!app.prefix.armed(), "a click outside cancels the chord");

        // 5. The thread view's link popup, which is the screen's own overlay.
        let mut app = test_app();
        app.screens.push(Screen::ThreadView(screens::ThreadViewState {
            thread: thread(9),
            posts: vec![Post {
                post_id: 1,
                username: "kemical".into(),
                message: "see https://example.com/a".into(),
                ..Default::default()
            }],
            page: 1,
            last_page: 1,
            link_popup: true,
            ..Default::default()
        }));
        frame(&mut app, 80, 24);
        let (x, y) = find_hit(&app, 80, 24, |h| h == &Hit::CloseOverlay).expect("an outside cell");
        click(&mut app, x, y);
        assert!(
            matches!(app.screens.last(), Some(Screen::ThreadView(v)) if !v.link_popup),
            "a click off the link popup closes it"
        );
    }

    /// The whole click-vs-drag rule: press and release in one cell is a
    /// click; anything that moves stays the drag-selection it always was.
    #[test]
    fn a_press_and_release_in_one_cell_is_a_click_and_any_movement_is_a_drag() {
        let mut app = home_app();
        frame(&mut app, 120, 24);

        // A drag across the list must not select a row — it is a selection.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 60, 5));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 63, 5));
        assert!(app.selection.is_some(), "a drag paints a selection band");
        // Released over a blank run of the title column, so nothing reaches
        // the clipboard: this test must never write the developer's own.
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 63, 5));
        assert!(app.selection.is_none(), "the band is consumed on release");
        assert!(
            !app.status.starts_with("Copied"),
            "the drag covered blank cells, so nothing should have been copied: {}",
            app.status
        );
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!(h.list.sel, 0, "a drag is not a click: the row must not move");
        assert_eq!(h.focus, screens::Pane::Tree, "and it must not steal the keyboard");

        // The same cell, without the movement between press and release, is
        // a click. (A moment later: two presses in one cell inside the
        // 400 ms window would be the double-click "open" instead.)
        click_later(&mut app, 60, 5);
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!(h.list.sel, 2);
    }

    /// A click in a composer puts the caret where the pointer is, measured
    /// in cells through the same visual-row model that drew it: on a wrapped
    /// row, and after a double-width character.
    #[test]
    fn a_click_in_the_composer_places_the_caret_on_a_wrapped_row_and_after_a_cjk_character() {
        let mut app = test_app();
        let body = "one two three four five six seven eight nine ten eleven twelve thirteen"
            .to_string();
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 9,
                thread_title: "A thread".into(),
            }),
            body: body.clone(),
            ..Default::default()
        }));
        frame(&mut app, 40, 24);

        let (rect, width) = match app.screens.last() {
            Some(Screen::Compose(c)) => (c.body_rect, c.body_width as usize),
            _ => panic!("expected the composer"),
        };
        assert!(rect.height > 1 && width > 0, "the body pane must be on screen");
        assert_eq!(app.hits.at(rect.x + 4, rect.y + 1), Some(&Hit::Field(1)));

        // Second visual row, five cells in: the caret must land on exactly
        // that cell of that row when measured back the way it is drawn.
        click(&mut app, rect.x + 5, rect.y + 1);
        let Some(Screen::Compose(c)) = app.screens.last() else {
            panic!("expected the composer");
        };
        assert_eq!(
            crate::editor::caret_position(&body, width, c.body_cursor),
            (1, 5),
            "the caret must land on the clicked cell of the clicked row"
        );

        // CJK: two cells per character, so column 4 is after the second one.
        if let Some(Screen::Compose(c)) = app.screens.last_mut() {
            c.body = "\u{6f22}\u{5b57}\u{30c6}\u{30b9}\u{30c8}".into();
            c.body_cursor = 0;
        }
        frame(&mut app, 40, 24);
        click(&mut app, rect.x + 4, rect.y);
        let Some(Screen::Compose(c)) = app.screens.last() else {
            panic!("expected the composer");
        };
        assert_eq!(
            c.body_cursor, 2,
            "column 4 is two ideographs in, not four characters in"
        );
    }

    /// The composer's BBCode caps are buttons too: clicking one runs the
    /// same chord the cap names, through `handle_key`.
    #[test]
    fn clicking_a_bbcode_cap_wraps_the_draft_the_way_the_chord_does() {
        let mut app = test_app();
        app.screens.push(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 9,
                thread_title: "A thread".into(),
            }),
            ..Default::default()
        }));
        frame(&mut app, 80, 24);

        let (x, y) = find_hit(&app, 80, 24, |h| h == &Hit::Cap("^B")).expect("a bold cap");
        click(&mut app, x, y);
        let Some(Screen::Compose(c)) = app.screens.last() else {
            panic!("expected the composer");
        };
        assert_eq!(c.body, "[B][/B]");
        assert_eq!(c.body_cursor, 3, "the caret lands between the tags");
    }

    /// `WFTUI_MOUSE=0`: nothing captures the mouse, so the client registers
    /// no hits and ignores anything that arrives anyway. (The env var itself
    /// is pinned by `common::config::tests::mouse_is_on_unless_the_env_turns_it_off`;
    /// the map is process-local, so this test sets it directly rather than
    /// racing every other test for the env lock.)
    #[test]
    fn with_the_mouse_disabled_nothing_is_registered_and_no_event_acts() {
        let mut app = home_app();
        app.hits = HitMap::new(false);
        frame(&mut app, 120, 24);
        assert_eq!(app.hits.count(), 0, "a disabled map registers nothing");

        click(&mut app, 60, 5);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), 60, 5));
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 60, 5));
        let Some(Screen::Home(h)) = app.screens.last() else {
            panic!("expected Home");
        };
        assert_eq!((h.list.sel, h.tree.sel), (0, 0), "no event may act");
        assert_eq!(h.focus, screens::Pane::Tree);
        assert!(app.selection.is_none(), "and no selection band is started");
    }
}
