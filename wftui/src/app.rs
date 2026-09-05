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
use crate::overlay::{self, GoTarget, Palette, PaletteEvent, Prefix, PrefixEvent};
use crate::screens::{self, Action, ComposeTarget, Screen};
use crate::theme::Theme;

/// How many image loads may be in flight at once (issue #543). Small on
/// purpose: decoration is never what the user is waiting for, and `draw`
/// spawns one task per visible image slot the moment a thread opens.
const IMAGE_LOAD_CONCURRENCY: usize = 3;

/// Serializable error payload crossing from background tasks into the UI.
#[derive(Debug, Clone)]
pub struct TaskError {
    pub message: String,
    #[allow(dead_code)]
    pub code: Option<String>,
    pub max_page: Option<u32>,
}

impl TaskError {
    pub fn of(e: &Error) -> Self {
        match e {
            Error::Api { code, message, max_page, .. } => TaskError {
                message: message.clone(),
                code: Some(code.clone()),
                max_page: *max_page,
            },
            other => TaskError {
                message: other.to_string(),
                code: None,
                max_page: None,
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
    Bootstrap(Result<User, TaskError>),
    LoginReady { url: String },
    LoginFailed(String),
    LoginComplete(Result<User, TaskError>),
    NodesLoaded(TaskResult<Vec<Node>>),
    /// A forum page came back. `node_id` is the forum that was asked for, so
    /// a reply that outraced a newer request is dropped instead of landing in
    /// whichever list happens to be on top (`Gate::wait` only spaces request
    /// *starts*, so two `load_forum` calls can finish out of order).
    ForumLoaded { node_id: u32, page: u32, result: TaskResult<ForumReply> },
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
    ConvoReplySent(TaskResult<()>),
    ConvoCreated(TaskResult<Conversation>),
    ConversationMarked(TaskResult<()>),
    RecipientResolved { name: String, id: Option<u32> },
    AlertsLoaded(TaskResult<AlertsReply>),
    AlertMarked(TaskResult<()>),
    SearchDone { page: u32, result: TaskResult<SearchResultsReply> },
    /// A go-to palette member lookup came back. `query` is the palette query
    /// that asked, so a stale answer to an edited query is dropped.
    PaletteMember { query: String, user: Option<User> },
    ProfileLoaded(TaskResult<User>),
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
    pub convos_unread: u32,
    pub status: String,
    /// Set just before a reload whose only purpose is to refresh a thread page
    /// in place (`thread_id`, `page`, `sel_post`, `scroll`); consumed by the
    /// matching `Msg::ThreadLoaded` (issue #538).
    keep_thread_position: Option<(u32, u32, usize, u16)>,
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
    last_title: String,
    should_quit: bool,
    /// Handles for the alerts/conversations poll loops spawned by
    /// `start_pollers`, so `logout` can abort them instead of leaving them
    /// running (and doubled by the next login's `start_pollers` call) —
    /// issue #524.
    poller_handles: Vec<tokio::task::JoinHandle<()>>,
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
        if let Err(e) = ratatui::crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
        ) {
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
    let _guard = match TerminalGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("terminal setup failed: {e}");
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

    let client = match WfApiClient::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("client init failed: {e}");
            return 1;
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App {
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
        convos_unread: 0,
        status: "Starting…".into(),
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
        last_title: String::new(),
        should_quit: false,
        poller_handles: Vec::new(),
    };
    #[cfg(unix)]
    {
        let tx = app.tx.clone();
        tokio::spawn(async move {
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
        });
    }
    app.bootstrap().await;
    let reader = crate::event::spawn_reader();
    let outcome = app.event_loop(&mut terminal, reader).await;
    drop(_guard);
    outcome
}

impl App {
    async fn bootstrap(&mut self) {
        self.screens.push(screens::home_state(true));
        if self.client.has_tokens().await {
            self.status = "Restoring session…".into();
            let api = self.api.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = api.me().await.map_err(|e| TaskError::of(&e));
                tx.send(Msg::Bootstrap(result)).ok();
            });
        } else {
            // No session: start the browser login immediately — zero keys.
            self.screens.push(screens::login_state());
            self.begin_login();
        }
    }

    fn start_pollers(&mut self) {
        // Defensive: a stray second call (there should never be one with the
        // callers below, but this keeps the invariant "at most one poller
        // pair running" regardless) stops the previous pair first rather
        // than doubling the poll rate.
        self.stop_pollers();
        // Alerts poller: unread count for the status bar.
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.poller_handles.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(common::config::ALERT_POLL_SECS)).await;
                if let Ok(page) = api.alerts(1).await {
                    let unread = page.alerts.iter().filter(|a| !a.viewed()).count() as u32;
                    tx.send(Msg::Notice(format!("alerts:{unread}"))).ok();
                }
            }
        }));
        // Conversations unread poller.
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.poller_handles.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(
                    common::config::CONVERSATION_POLL_SECS,
                ))
                .await;
                if let Ok(page) = api.conversations(1).await {
                    let unread = common::models::count_unread_conversations(&page.conversations);
                    tx.send(Msg::Notice(format!("convos:{unread}"))).ok();
                }
            }
        }));
    }

    /// Abort every poller spawned by `start_pollers` and forget its handles.
    /// Called on logout so a signed-out session stops hitting `/alerts` and
    /// `/conversations` — and so the next login's `start_pollers` starts a
    /// fresh pair instead of adding to whatever was already running
    /// (issue #524).
    fn stop_pollers(&mut self) {
        for h in self.poller_handles.drain(..) {
            h.abort();
        }
    }

    async fn event_loop(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
        reader: std::sync::mpsc::Receiver<crate::event::Input>,
    ) -> u8 {
        loop {
            let _ = terminal.draw(|f| self.draw(f));
            // Drain background messages.
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg);
                if self.should_quit {
                    break;
                }
            }
            if self.should_quit {
                return 0;
            }
            // Debounced palette member lookup: the loop's idle path is the
            // only place with a clock, and typing must not fan out requests.
            self.poll_palette_member();
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
                                        self.status =
                                            format!("Copied {n} chars to clipboard (selection cleared)");
                                    }
                                } else {
                                    return 0;
                                }
                            } else {
                                self.handle_key(k);
                            }
                        }
                    }
                    if self.should_quit {
                        return 0;
                    }
                }
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

        // Crumbs are the screen stack's own names. Login is excluded: it is a
        // gate, not a place, and it owns the whole screen while it is up.
        // Home is one screen but two places: `Forums › <current forum>`. Every
        // other screen contributes exactly its own crumb; Login contributes
        // none (it is a gate, not a place).
        let mut crumbs: Vec<String> = Vec::with_capacity(self.screens.len() + 1);
        for s in &self.screens {
            match s {
                Screen::Login(_) => {}
                Screen::Home(h) => {
                    crumbs.push("Forums".to_string());
                    if !h.list.title.is_empty() {
                        crumbs.push(h.list.title.clone());
                    }
                }
                other => crumbs.push(other.crumb()),
            }
        }
        let me_name = self.me.as_ref().map(|u| u.username.as_str());
        f.render_widget(
            Paragraph::new(chrome::header_line(
                &self.theme,
                &self.glyphs,
                &crumbs,
                me_name,
                self.convos_unread,
                self.alerts_unread,
                None,
                top.width,
            )),
            top,
        );

        // Render the top screen; popups handled inside renderers.
        // The graphics policy is stamped on first: the thread view reserves
        // rows for inline images while it wraps text, so a tier change has to
        // reach it before `render` rebuilds the lines.
        let policy = self.images.policy();
        let screen = self.screens.last_mut().expect("screen stack never empty");
        screen.set_image_policy(policy, self.images.sizes());
        let screen_title = screen.title().to_string();
        let screen_hints = screen.hints();
        let keys_group = screen.keys_group();
        screen.render(f, body, &self.theme, &self.glyphs);

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
        if let Some(p) = &self.palette {
            p.render(f, body, &self.theme, &self.glyphs);
            bar = Some(Palette::hints(&self.glyphs));
            status_left = Palette::status().to_string();
        }
        if self.prefix.armed() {
            overlay::render_which_key(f, body, &self.theme, &self.glyphs);
        }
        if self.show_help {
            overlay::render_keys_card(
                f,
                body,
                &self.theme,
                &self.glyphs,
                keys_group,
                &screen_hints,
            );
            bar = Some(overlay::keys_card_hints());
            status_left = overlay::keys_card_status().to_string();
        }

        f.render_widget(
            Paragraph::new(chrome::key_bar(
                &self.theme,
                bar.as_ref().unwrap_or(&screen_hints),
                keys.width,
            )),
            keys,
        );

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
        let mut rows = Vec::with_capacity(area.height as usize);
        let mut cols = Vec::with_capacity(area.height as usize);
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

    fn handle_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('l') {
            self.logout();
            return;
        }
        // The palette owns every key while it is up, including `?` and Esc.
        // The event is taken first so the arms can borrow `self` again.
        if let Some(event) = self.palette.as_mut().map(|p| p.key(k)) {
            match event {
                PaletteEvent::None => {}
                PaletteEvent::Close => {
                    self.palette = None;
                    self.status.clear();
                }
                PaletteEvent::Run(target) => {
                    self.palette = None;
                    self.status.clear();
                    self.run_palette_target(target);
                }
            }
            return;
        }
        if k.code == KeyCode::Char('?') && !self.input_active() {
            self.show_help = !self.show_help;
            return;
        }
        if self.show_help {
            // Any other key closes the overlay.
            self.show_help = false;
            if k.code == KeyCode::Esc {
                return;
            }
        }
        // The key after `g` resolves the chord or cancels it; either way it is
        // consumed, so a mistyped chord never fires a stray command.
        if self.prefix.armed() {
            if let PrefixEvent::Go(target) = self.prefix.resolve(k) {
                self.go(target);
            }
            return;
        }
        let capture = self.screens.last().map(|s| s.input_capture()).unwrap_or(false);
        // `Ctrl+K` / `:` / `g` / `G` only when no input field owns the keyboard
        // (`^K` is BBCode `[ICODE]` in the composer) and only once there is a
        // session to navigate — the login screen is a gate, not a place.
        let plain = k.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        if self.me.is_some() && !capture && !self.input_active() {
            let palette_key = (k.modifiers.contains(KeyModifiers::CONTROL)
                && k.code == KeyCode::Char('k'))
                || (plain && k.code == KeyCode::Char(':'));
            if palette_key {
                self.open_palette();
                return;
            }
            if plain && k.code == KeyCode::Char('g') {
                self.prefix.arm();
                return;
            }
            if plain && k.code == KeyCode::Char('G') {
                if let Some(screen) = self.screens.last_mut() {
                    screen.goto_bottom();
                }
                return;
            }
        }
        // Global navigation only when no input field owns the keyboard.
        if k.modifiers.is_empty() && !capture && !self.input_active() {
            match k.code {
                KeyCode::Char('c') => {
                    self.open_inbox(screens::InboxTab::Conversations);
                    return;
                }
                KeyCode::Char('a') => {
                    self.open_inbox(screens::InboxTab::Alerts);
                    return;
                }
                KeyCode::Char('s') | KeyCode::Char('/') => {
                    self.push_screen(screens::search_state());
                    return;
                }
                _ => {}
            }
        }
        if k.code == KeyCode::Esc && !capture {
            // Screens get first refusal (issue #520): a busy composer refuses
            // Esc out loud rather than being popped out from under an
            // in-flight post, and a screen with its own Esc arm (Search's
            // edit mode, the composers' discard) runs it.
            match self.screens.last().map(Screen::esc_intent) {
                Some(screens::EscIntent::Blocked(hint)) => {
                    self.status = hint.to_string();
                    return;
                }
                Some(screens::EscIntent::Screen) => {
                    let action = match self.screens.last_mut() {
                        Some(screen) => screen.on_key(k),
                        None => Action::None,
                    };
                    self.execute_action(action);
                    return;
                }
                _ => {}
            }
            // Home is the root screen, so a bare Esc there would quit. In the
            // list pane it means "back to the forums", which is what Esc means
            // everywhere else in the client.
            if let Some(Screen::Home(h)) = self.screens.last_mut()
                && h.focus == screens::Pane::List
            {
                h.focus = screens::Pane::Tree;
                return;
            }
            // Same idea for the Inbox: Esc from the inline view pane returns
            // to the tabbed list rather than leaving the screen entirely.
            if let Some(Screen::Inbox(ib)) = self.screens.last_mut()
                && ib.focus == screens::InboxPane::View
            {
                ib.focus = screens::InboxPane::List;
                return;
            }
            if self.screens.len() > 1 {
                self.screens.pop();
                self.status.clear();
            } else {
                self.should_quit = true;
            }
            return;
        }
        tracing::debug!("key: {:?} mods {:?}", k.code, k.modifiers);
        let action = match self.screens.last_mut() {
            Some(screen) => screen.on_key(k),
            None => Action::None,
        };
        self.execute_action(action);
    }

    fn execute_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::PopScreen => {
                if self.screens.len() > 1 {
                    self.screens.pop();
                }
            }
            Action::Quit => self.should_quit = true,
            Action::OpenThreadList(mut node_id, mut title) => {
                let mut target_url = None;
                if let Some(tree) = self.tree()
                    && let Some(node) = tree.nodes.iter().find(|n| n.node_id == node_id)
                {
                    if node.node_type == "Category" {
                        if let Some(child) = tree.nodes.iter().find(|c| {
                            (c.parent_node_id == node.node_id || c.depth > node.depth)
                                && c.node_type == "Forum"
                        }) {
                            node_id = child.node_id;
                            title = child.title.clone();
                        }
                    } else if matches!(node.node_type.as_str(), "LinkForum" | "Page") {
                        target_url = node.view_url.clone();
                    }
                }
                if let Some(url) = target_url {
                    self.open_url(&url);
                } else {
                    self.open_list(node_id, title);
                }
            }
            Action::OpenLatestThreads => self.open_list(0, "Latest posts".to_string()),
            Action::OpenThread(thread) => self.open_thread(&thread),
            Action::OpenProfile(user_id, name) => self.open_profile(user_id, &name),
            Action::OpenMemberContent {
                user_id,
                username,
                content,
            } => {
                let display_title = format!("{username}'s {content}");
                let s = screens::SearchState {
                    query: format!("by: {username} ({content})"),
                    input_mode: false,
                    loading: true,
                    ..Default::default()
                };
                self.push_screen(Screen::Search(s));
                let api = self.api.clone();
                let tx = self.tx.clone();
                self.status = format!("Searching {display_title}…");
                tokio::spawn(async move {
                    let result = api
                        .search_member(user_id, &content, 1)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::SearchDone { page: 1, result }).ok();
                });
            }
            Action::OpenConversation(conv) => self.open_conversation(conv),
            Action::LoadForum(node_id, page) => self.load_forum(node_id, page),
            Action::LoadThread(id, page) => self.load_thread(id, page),
            Action::LoadConversations(page) => self.load_conversations(page),
            // Paging inside a conversation the user already opened.
            Action::LoadConversation(id, page) => self.load_conversation(id, page, true),
            Action::LoadAlerts => self.load_alerts(),
            Action::LoadNodes => {
                if let Some(tree) = self.tree_mut() {
                    tree.loading = true;
                    tree.error = None;
                }
                self.load_nodes();
            }
            Action::RunSearchQuery(query) => self.run_search_query(query),
            Action::MarkThreadRead(id) => self.mark_thread_read(id),
            Action::MarkForumRead(node_id) => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .mark_forum_read(node_id)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::MarkedRead(result)).ok();
                });
            }
            Action::MarkAlertRead(id) => self.mark_alert_read(id),
            Action::MarkConversationRead(id) => self.mark_conversation_read(id),
            Action::ReactPost(post_id) => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.react_post(post_id, 1).await.map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostToggled { verb: PostVerb::Like, result }).ok();
                });
            }
            Action::VotePost(post_id, vote_type) => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                let verb = PostVerb::of_vote(&vote_type);
                tokio::spawn(async move {
                    let result = api
                        .vote_post(post_id, &vote_type)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::PostToggled { verb, result }).ok();
                });
            }
            Action::StartReply(thread) => self.reply_to_thread(&thread),
            Action::StartReplyConversation(conv) => self.reply_to_conversation(&conv),
            Action::StartNewThread(node_id) => self.new_thread(node_id),
            Action::StartNewConversation(recipient) => {
                let mut state = screens::NewConversationState::default();
                if let Some(r) = recipient {
                    state.recipients = format!("{r}, ");
                    // Char count, not byte length — editor.rs cursors are
                    // char indices, and a non-ASCII username (e.g. "Zoë")
                    // has more bytes than chars (issue #535).
                    state.recipients_cursor = state.recipients.chars().count();
                    state.field = 1;
                }
                self.push_screen(Screen::NewConversation(state));
            }
            Action::SubmitReply { thread_id, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.reply(thread_id, &message).await.map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ReplySent(result)).ok();
                });
            }
            Action::SubmitThread { node_id, title, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .create_thread(node_id, &title, &message)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ThreadCreated(result)).ok();
                });
            }
            Action::SubmitConvoReply { id, message } => {
                let api = self.api.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .reply_conversation(id, &message)
                        .await
                        .map_err(|e| TaskError::of(&e));
                    tx.send(Msg::ConvoReplySent(result)).ok();
                });
            }
            Action::ResolveRecipients(names, _title, _body) => {
                // The NewConversation screen keeps title/body; we only resolve
                // names to ids and report back per name.
                for name in names {
                    let api = self.api.clone();
                    let tx = self.tx.clone();
                    let name_clone = name.clone();
                    tokio::spawn(async move {
                        let id = api.find_user(&name_clone).await.unwrap_or(None);
                        tx.send(Msg::RecipientResolved {
                            name: name_clone,
                            id: id.map(|u| u.user_id),
                        })
                        .ok();
                    });
                }
            }
            Action::LoginBegin => self.begin_login(),
            Action::PasteClipboard => {
                let mut text = self.clipboard.clone();
                if text.is_empty()
                    && let Some(sys) = common::osc::read_from_system_clipboard()
                {
                    text = sys;
                }
                if text.is_empty() {
                    self.status = "Nothing copied yet — drag, double-click, or use system clipboard.".into();
                    return;
                }
                self.handle_paste(text);
            }
            Action::OscCopy(value) => {
                self.copy_text(&value);
                self.status = "Copied to your clipboard (OSC 52 + system clipboard).".into();
            }
            Action::OpenUrl(url) => self.open_url(&url),
        }
    }

    // ---- the go-to palette and the `g` chord ----

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

    /// Copy text via OSC 52 (remote terminal), local system clipboard tools, and in-app clipboard.
    pub fn copy_text(&mut self, text: &str) {
        emit_raw(&common::osc::set_clipboard(text));
        common::osc::copy_to_system_clipboard(text);
        self.clipboard = text.to_string();
    }

    // ---- login orchestration ----

    fn begin_login(&mut self) {
        if let Some(Screen::Login(ls)) = self.screens.last_mut() {
            ls.busy = true;
            ls.error = None;
        }
        let tx = self.tx.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = run_login_flow(&tx, client).await {
                tx.send(Msg::LoginFailed(e)).ok();
            }
        });
    }

    // ---- paste handling (bracketed paste and clipboard) ----

    pub fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.clipboard = text.clone();
        // The palette owns the keyboard while it is up, so it owns pastes too.
        if let Some(palette) = self.palette.as_mut() {
            let sanitized = text.replace(['\r', '\n'], " ");
            crate::editor::insert_str(&mut palette.query, &mut palette.cursor, &sanitized);
            palette.after_paste();
            return;
        }
        if let Some(screen) = self.screens.last_mut() {
            match screen {
                Screen::Compose(cs) => {
                    if cs.title_field {
                        let sanitized = text.replace(['\r', '\n'], " ");
                        crate::editor::insert_str(&mut cs.title, &mut cs.title_cursor, &sanitized);
                    } else {
                        let sanitized = text.replace('\r', "");
                        crate::editor::insert_str(&mut cs.body, &mut cs.body_cursor, &sanitized);
                    }
                    self.status = format!("Pasted {} characters", text.chars().count());
                }
                Screen::Search(ss) => {
                    let sanitized = text.replace(['\r', '\n'], " ");
                    if ss.active_field == 1 {
                        crate::editor::insert_str(&mut ss.author, &mut ss.author_cursor, &sanitized);
                    } else {
                        crate::editor::insert_str(&mut ss.query, &mut ss.query_cursor, &sanitized);
                    }
                    self.status = format!("Pasted {} characters into search", text.chars().count());
                }
                Screen::NewConversation(ncs) => {
                    match ncs.field {
                        0 => {
                            let sanitized = text.replace(['\r', '\n'], " ");
                            crate::editor::insert_str(
                                &mut ncs.recipients,
                                &mut ncs.recipients_cursor,
                                &sanitized,
                            );
                        }
                        1 => {
                            let sanitized = text.replace(['\r', '\n'], " ");
                            crate::editor::insert_str(
                                &mut ncs.title,
                                &mut ncs.title_cursor,
                                &sanitized,
                            );
                        }
                        _ => {
                            let sanitized = text.replace('\r', "");
                            crate::editor::insert_str(
                                &mut ncs.body,
                                &mut ncs.body_cursor,
                                &sanitized,
                            );
                        }
                    }
                    self.status = format!("Pasted {} characters", text.chars().count());
                }
                _ => {
                    self.status = format!(
                        "Pasted {} characters into clipboard (press Ctrl+Y to insert)",
                        text.chars().count()
                    );
                }
            }
        }
    }

    // ---- mouse: selection + multi-click + wheel scrolling ----

    fn handle_mouse(&mut self, me: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{KeyCode, MouseEventKind as K};

        let pos = (me.column, me.row);
        match me.kind {
            K::Down(ratatui::crossterm::event::MouseButton::Left, ..) => {
                let now = std::time::Instant::now();
                let is_rapid = self.last_click_instant.is_some_and(|t| {
                    now.duration_since(t) < Duration::from_millis(400)
                }) && self.last_click_pos.0.abs_diff(pos.0) <= 2
                    && self.last_click_pos.1 == pos.1;

                if is_rapid {
                    self.click_count = (self.click_count % 3) + 1;
                } else {
                    self.click_count = 1;
                }
                self.last_click_instant = Some(now);
                self.last_click_pos = pos;

                if self.click_count == 2 {
                    // Double-click: select word
                    if let Some((start_col, end_col)) = self.find_word_bounds(pos.0, pos.1) {
                        self.selection = Some(Selection {
                            anchor: (start_col, pos.1),
                            end: (end_col, pos.1),
                        });
                        let text = self.extract_selection_text(start_col, pos.1, end_col, pos.1);
                        if !text.is_empty() {
                            let n = text.chars().count();
                            self.copy_text(&text);
                            self.status =
                                format!("Copied word ({n} chars) to clipboard (and Ctrl+Y)");
                        }
                    }
                } else if self.click_count == 3 {
                    // Triple-click: select line
                    if let Some((start_col, end_col)) = self.find_line_bounds(pos.1) {
                        self.selection = Some(Selection {
                            anchor: (start_col, pos.1),
                            end: (end_col, pos.1),
                        });
                        let text = self.extract_selection_text(start_col, pos.1, end_col, pos.1);
                        if !text.is_empty() {
                            let n = text.chars().count();
                            self.copy_text(&text);
                            self.status =
                                format!("Copied line ({n} chars) to clipboard (and Ctrl+Y)");
                        }
                    }
                } else {
                    self.selection = Some(Selection {
                        anchor: pos,
                        end: pos,
                    });
                }
            }
            K::Drag(ratatui::crossterm::event::MouseButton::Left, ..) => {
                if let Some(sel) = &mut self.selection {
                    sel.end = pos;
                }
            }
            K::Up(ratatui::crossterm::event::MouseButton::Left, ..) => {
                if self.click_count > 1 {
                    return;
                }
                if let Some(sel) = self.selection.take() {
                    let (x0, y0, x1, y1) = sel.rect();
                    if (x0, y0) == (x1, y1) {
                        return; // plain click: clear the selection
                    }
                    let text = self.extract_selection_text(x0, y0, x1, y1);
                    if !text.is_empty() {
                        let n = text.chars().count();
                        self.copy_text(&text);
                        self.status =
                            format!("Copied {n} chars to your clipboard (and Ctrl+Y)");
                    }
                }
            }
            K::ScrollUp => {
                for _ in 0..3 {
                    self.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                }
            }
            K::ScrollDown => {
                for _ in 0..3 {
                    self.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                }
            }
            _ => {}
        }
    }

    fn find_word_bounds(&self, x: u16, y: u16) -> Option<(u16, u16)> {
        let y_idx = y as usize;
        if y_idx >= self.screen_rows.len() || y_idx >= self.screen_cols.len() {
            return None;
        }
        let row = &self.screen_rows[y_idx];
        let offsets = &self.screen_cols[y_idx];
        if offsets.len() < 2 {
            return None;
        }
        let max_col = (offsets.len().saturating_sub(2)) as u16;
        let x = x.min(max_col);

        let get_char = |col: u16| -> Option<char> {
            let idx = col as usize;
            if idx + 1 >= offsets.len() {
                return None;
            }
            let s = offsets[idx];
            let e = offsets[idx + 1];
            row.get(s..e)?.chars().next()
        };
        // A column covered by the double-width character to its left carries
        // no bytes of its own (see `capture_screen`); stepping over those is
        // what lets a CJK word select as a word (issue #525).
        let is_continuation = |col: u16| -> bool {
            let idx = col as usize;
            idx + 1 < offsets.len() && offsets[idx] == offsets[idx + 1]
        };
        // Clicking the right half of a wide character means the character.
        let mut x = x;
        while x > 0 && is_continuation(x) {
            x -= 1;
        }

        let target = get_char(x)?;
        if target.is_whitespace() {
            return None;
        }
        let is_word_char = |c: char| {
            c.is_alphanumeric()
                || c == '_'
                || c == '-'
                || c == '.'
                || c == '/'
                || c == ':'
                || c == '@'
        };
        let target_is_word = is_word_char(target);

        let mut start_col = x;
        while start_col > 0 {
            let mut prev = start_col - 1;
            while prev > 0 && is_continuation(prev) {
                prev -= 1;
            }
            if let Some(c) = get_char(prev)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                start_col = prev;
                continue;
            }
            break;
        }

        let mut end_col = x;
        while end_col < max_col {
            let mut next = end_col + 1;
            while next < max_col && is_continuation(next) {
                next += 1;
            }
            if let Some(c) = get_char(next)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                end_col = next;
                continue;
            }
            break;
        }
        // Include the trailing half of a wide last character, so the band
        // covers what the eye sees and the byte slice ends after it.
        while end_col < max_col && is_continuation(end_col + 1) {
            end_col += 1;
        }

        Some((start_col, end_col))
    }

    fn find_line_bounds(&self, y: u16) -> Option<(u16, u16)> {
        let y_idx = y as usize;
        if y_idx >= self.screen_rows.len() || y_idx >= self.screen_cols.len() {
            return None;
        }
        let offsets = &self.screen_cols[y_idx];
        if offsets.len() < 2 {
            return None;
        }
        let max_col = (offsets.len().saturating_sub(2)) as u16;
        Some((0, max_col))
    }

    fn extract_selection_text(&self, x0: u16, y0: u16, x1: u16, y1: u16) -> String {
        if self.screen_rows.is_empty() {
            return String::new();
        }
        let max_y = (y1 as usize).min(self.screen_rows.len() - 1);
        let mut lines: Vec<String> = Vec::new();
        for y in (y0 as usize)..=max_y {
            let row = &self.screen_rows[y];
            let offsets = &self.screen_cols[y];
            let start = offsets[(x0 as usize).min(offsets.len() - 1)];
            let end = offsets[(x1 as usize + 1).min(offsets.len() - 1)];
            lines.push(row[start..end].trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Paint the live selection highlight over the finished frame.
    fn paint_selection(&self, f: &mut Frame) {
        use ratatui::style::{Color, Style};
        let Some(sel) = &self.selection else {
            return;
        };
        let (x0, y0, x1, y1) = sel.rect();
        let area = f.area();
        let style = Style::new().fg(Color::Black).bg(Color::LightBlue);
        for y in y0..=y1.min(area.height.saturating_sub(1)) {
            for x in x0..=x1.min(area.width.saturating_sub(1)) {
                let cell = &mut f.buffer_mut()[(x, y)];
                // The selection band must never touch an image: kitty encodes
                // the image id in the cell's foreground colour, so re-styling
                // the anchor cell would repaint a different image (or none).
                if is_image_cell(cell) {
                    continue;
                }
                cell.set_style(style);
            }
        }
    }

    pub fn input_active(&self) -> bool {
        // Login has no free-text field anymore (short link + polling), so
        // global keys like ? work there too.
        matches!(
            self.screens.last(),
            Some(Screen::Search(_))
                | Some(Screen::Compose(_))
                | Some(Screen::NewConversation(_))
        ) || capture_active(self)
    }
    pub fn push_screen(&mut self, screen: Screen) {
        self.screens.push(screen);
    }

    // ---- screen openers / actions ----

    /// Open the Inbox on `tab`, or — if it is already the top screen — just
    /// switch to that tab in place (so `c`/`a` from anywhere never stacks a
    /// second Inbox on top of the first).
    fn open_inbox(&mut self, tab: screens::InboxTab) {
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
        if let Some(Screen::Inbox(inbox)) = self.screens.last_mut()
            && inbox.dual
        {
            inbox.view = Some(screens::ConversationViewState {
                conversation: conv,
                page: 1,
                loading: true,
                ..Default::default()
            });
            inbox.focus = screens::InboxPane::View;
            self.load_conversation(cid, 1, true);
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

    pub fn load_conversations(&mut self, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.conversations(page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationsLoaded { page, result }).ok();
        });
    }

    pub fn load_alerts(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.alerts(1).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::AlertsLoaded(result)).ok();
        });
    }

    pub fn mark_conversation_read(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .mark_conversation_read(id)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationMarked(result)).ok();
        });
    }

    pub fn load_forum(&mut self, node_id: u32, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = if node_id == 0 {
                api.threads(page)
                    .await
                    .map(|r| ForumReply {
                        forum: Forum {
                            node_id: 0,
                            title: "Latest posts".to_string(),
                            ..Default::default()
                        },
                        threads: r.threads,
                        pagination: r.pagination,
                        sticky: Vec::new(),
                    })
                    .map_err(|e| TaskError::of(&e))
            } else {
                api.forum(node_id, page).await.map_err(|e| TaskError::of(&e))
            };
            tx.send(Msg::ForumLoaded { node_id, page, result }).ok();
        });
    }

    // ---- where forum/thread-list state lives ----
    //
    // Home owns both a tree and a list; the pre-redesign ForumTree/ThreadList
    // screens still exist for pushes from search, alerts and the narrow
    // layout. These three helpers are the single place that knows both shapes,
    // so a message handler never has to.

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
            loading: true,
            ..Default::default()
        }));
        self.load_forum(node_id, 1);
    }

    pub fn open_thread(&mut self, thread: &Thread) {
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

    pub fn load_thread(&mut self, id: u32, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.thread(id, page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ThreadLoaded { id, page, result }).ok();
        });
    }

    pub fn open_profile(&mut self, user_id: u32, fallback_name: &str) {
        self.push_screen(Screen::Profile(screens::ProfileState {
            title: fallback_name.to_string(),
            loading: true,
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
            tx.send(Msg::ProfileLoaded(result)).ok();
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
        // `reply_count` counts replies, so the thread holds `reply_count + 1`
        // posts and this draft becomes the next one after that.
        let reply_number = u32::try_from(thread.reply_count.saturating_add(2)).ok();
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: thread.thread_id,
                thread_title: thread.title.clone(),
            }),
            author: self.me_name(),
            reply_number,
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

    pub fn mark_thread_read(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.mark_thread_read(id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::MarkedRead(result)).ok();
        });
    }

    pub fn mark_alert_read(&mut self, id: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.mark_alert_read(id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::AlertMarked(result)).ok();
        });
    }

    pub fn logout(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Revoke the refresh token first and the access token second,
            // each with its `token_type_hint` — the endpoint defaults to
            // `access_token`, so the old single call left the 90-day refresh
            // token valid for anyone holding a copy of `token.json`
            // (issue #527). A failure here is reported, never swallowed:
            // against stock XenForo this call cannot currently succeed for a
            // public PKCE client (its revoke endpoint requires the
            // `client_secret` this client deliberately does not have), and a
            // silent "Logged out." would hide that the tokens are still
            // live. The server-side fix is a relay in the TuiLink add-on and
            // is out of this client's scope.
            let mut failed: Vec<&str> = Vec::new();
            let tokens = client.token_set().await;
            if let Some(tokens) = tokens {
                match common::http::build() {
                    Ok(http) => {
                        for (token, hint) in [
                            (&tokens.refresh_token, "refresh_token"),
                            (&tokens.access_token, "access_token"),
                        ] {
                            if token.is_empty() {
                                continue;
                            }
                            if let Err(e) = common::oauth::revoke(&http, token, Some(hint)).await {
                                tracing::warn!("logout: {hint} was not revoked server-side: {e}");
                                failed.push(hint);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("logout: no HTTP client to revoke with: {e}");
                        failed.push("refresh_token");
                    }
                }
            }
            let result = match client.forget_tokens().await {
                Err(e) => Err(e.to_string()),
                Ok(()) if !failed.is_empty() => Err(format!(
                    "the server kept {} valid \u{2014} sign out in a browser to end the session",
                    failed.join(" and ").replace('_', " ")
                )),
                Ok(()) => Ok(()),
            };
            tx.send(Msg::LoggedOut(result)).ok();
        });
        self.stop_pollers();
        self.me = None;
        self.alerts_unread = 0;
        self.convos_unread = 0;
        self.screens
            .retain(|s| matches!(s, Screen::Home(_) | Screen::ForumTree(_)));
        self.screens.push(screens::login_state());
        self.status = "Logged out.".into();
    }

    // ---- message handling ----

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::LoginReady { url } => {
                // Hand the short link over every channel we have: OSC 8 on
                // screen (rendered by the login screen), OSC 52 clipboard,
                // and a file for plain `cat`.
                emit_raw(&common::osc::set_clipboard(&url));
                let path = common::config::token_path().with_file_name("login-url.txt");
                let _ = std::fs::write(&path, &url);
                self.status = "Login link → clipboard + login-url.txt".into();
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.url = url;
                    ls.stage = crate::screens::LoginStage::Waiting;
                }
            }
            Msg::LoginFailed(message) => {
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.error = Some(message);
                    if matches!(
                        ls.stage,
                        crate::screens::LoginStage::Waiting
                    ) {
                        ls.stage = crate::screens::LoginStage::Idle;
                    }
                }
            }
            Msg::LoginComplete(Ok(user)) => {
                self.me = Some(user);
                if self.screens.len() > 1 && matches!(self.screens.last(), Some(Screen::Login(_))) {
                    self.screens.pop();
                }
                self.status = format!(
                    "Welcome, {}.",
                    self.me.as_ref().map(|u| u.username.as_str()).unwrap_or("")
                );
                self.start_pollers();
                self.load_nodes();
                self.prime_home_list();
            }
            Msg::LoginComplete(Err(e)) => {
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.error = Some(e.message);
                    ls.stage = crate::screens::LoginStage::Idle;
                }
            }
            Msg::ImageLoaded { key, result } => {
                self.images.on_loaded(key, result);
            }
            Msg::PostToggled { verb, result } => {
                match result {
                    Ok(toggle) => {
                        self.status = verb.notice(toggle).to_string();
                        // The ♡/▲ counts live in the baked post lines, so the
                        // page has to come back from the server; keep the
                        // reader where they were while it does (issue #538).
                        if let Some((id, page, sel_post, scroll)) =
                            open_thread_position(&self.screens)
                        {
                            self.keep_thread_position = Some((id, page, sel_post, scroll));
                            self.load_thread(id, page);
                        }
                    }
                    Err(e) => {
                        self.status = format!("{}: {}", verb.failure(), e.message);
                    }
                }
            }
            Msg::Notice(n) => {
                if n == "quit" {
                    self.should_quit = true;
                } else if let Some(v) = n.strip_prefix("alerts:") {
                    self.alerts_unread = v.parse().unwrap_or(0);
                } else if let Some(v) = n.strip_prefix("convos:") {
                    self.convos_unread = v.parse().unwrap_or(0);
                } else {
                    self.status = n;
                }
            }
            Msg::Bootstrap(Ok(user)) => {
                self.me = Some(user);
                self.status = "Session restored.".into();
                self.start_pollers();
                if matches!(self.screens.last(), Some(Screen::Login(_))) {
                    self.screens.pop();
                }
                self.load_nodes();
                self.prime_home_list();
            }
            Msg::Bootstrap(Err(e)) => {
                // Dead token → login screen (only if none is up already —
                // bootstrap pushes one when there is no token at all).
                if !self.screens.iter().any(|s| matches!(s, Screen::Login(_))) {
                    self.screens.push(screens::login_state());
                }
                self.status = format!("Session expired ({e}); log in again.");
            }
            Msg::NodesLoaded(result) => {
                let tree = self.tree_mut();
                if let Some(tree) = tree {
                    match result {
                        Ok(nodes) => {
                            // See ForumLoaded (issue #537).
                            tree.error = None;
                            if tree.sel == 0
                                && !nodes.is_empty()
                                && let Some(idx) = nodes.iter().position(|n| n.node_type == "Forum")
                            {
                                tree.sel = idx;
                            }
                            tree.nodes = nodes;
                            tree.loading = false;
                            tree.sel = tree.sel.min(tree.nodes.len().saturating_sub(1));
                        }
                        Err(e) => {
                            tree.loading = false;
                            tree.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ForumLoaded { node_id, page, result } => {
                let list = self.list_mut_for(node_id);
                if let Some(list) = list {
                    match result {
                        Ok(reply) => {
                            // A previous failed load must not keep rendering
                            // "Error: ... Press r to retry" forever once a
                            // later load succeeds (issue #537).
                            list.error = None;
                            if !reply.forum.title.is_empty() {
                                list.title = reply.forum.title;
                            }
                            // Sticky threads arrive in their own array (XF
                            // excludes them from `threads`/`pagination` so
                            // they don't shift pagination); prepend them so
                            // they render first without inflating the count.
                            list.sticky_count = reply.sticky.len();
                            list.threads = reply.sticky;
                            list.threads.extend(reply.threads);
                            list.page = page;
                            list.last_page = reply.pagination.last_page.max(1);
                            list.total = reply.pagination.total;
                            list.loading = false;
                            list.sel = list.sel.min(list.threads.len().saturating_sub(1));
                        }
                        Err(e) => {
                            list.loading = false;
                            list.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ThreadLoaded { id, page, result } => {
                // One-shot: a reload that only exists to refresh the ♡/▲
                // counts must not scroll the reader back to the top or move
                // the selection out from under the next `l`/`v` (issue #538).
                let keep = self
                    .keep_thread_position
                    .take()
                    .filter(|(kid, kpage, _, _)| *kid == id && *kpage == page);
                let view = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ThreadView(view) if view.thread.thread_id == id => Some(view),
                    _ => None,
                });
                if let Some(view) = view {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded: clear a stale error on success
                            // (issue #537).
                            view.error = None;
                            if reply.thread.thread_id > 0 || !reply.thread.title.is_empty() {
                                view.thread = reply.thread;
                            }
                            view.posts = reply.posts;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.total = reply.pagination.total;
                            view.loading = false;
                            view.scroll = 0;
                            view.sel_post = 0;
                            if let Some((_, _, sel_post, scroll)) = keep {
                                view.sel_post =
                                    sel_post.min(view.posts.len().saturating_sub(1));
                                view.scroll = scroll;
                            }
                            view.rebuild_lines(&self.theme, &self.glyphs);
                        }
                        Err(e) => {
                            view.loading = false;
                            // Clamp to the server-reported max page.
                            if let Some(max) = e.max_page {
                                view.last_page = max;
                                let id = view.thread.thread_id;
                                view.loading = true;
                                self.load_thread(id, max);
                            } else {
                                view.error = Some(e.message);
                            }
                        }
                    }
                }
            }
            Msg::ReplySent(result) => {
                let (compose_idx, target) = self
                    .screens
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, s)| match s {
                        Screen::Compose(c) if matches!(c.target, Some(ComposeTarget::ThreadReply { .. })) => {
                            Some((idx, c.target.clone()))
                        }
                        _ => None,
                    })
                    .unzip();
                match result {
                    Ok(_) => {
                        let thread_id = match target.flatten() {
                            Some(ComposeTarget::ThreadReply { thread_id, .. }) => thread_id,
                            _ => 0,
                        };
                        if let Some(idx) = compose_idx {
                            self.screens.remove(idx);
                        }
                        self.status = "Reply posted.".into();
                        if thread_id > 0 {
                            // Land on the page the new reply actually lands
                            // on, not page 1 (issue #529). The API gives no
                            // `per_page`, so this client can't compute that
                            // page from `post.position` alone — instead ask
                            // one past the last page we knew about and let
                            // `Msg::ThreadLoaded`'s existing `max_page` clamp
                            // (below) settle on the true last page, whether
                            // or not the reply pushed the thread onto a page
                            // that didn't exist a moment ago. A single-page
                            // thread clamps straight back to page 1.
                            let known_last = known_thread_last_page(&self.screens, thread_id);
                            self.load_thread(thread_id, known_last.saturating_add(1));
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            // The composer is gone (popped, or the screen
                            // stack moved on): the failure has nowhere to
                            // render, so say it in the status line instead of
                            // dropping it (issue #520).
                            self.status = format!("Reply failed: {}", e.message);
                        }
                    }
                }
            }
            Msg::ThreadCreated(result) => {
                let compose_idx = self.screens.iter().rposition(|s| match s {
                    Screen::Compose(c) => matches!(c.target, Some(ComposeTarget::NewThread { .. })),
                    _ => false,
                });
                match result {
                    Ok(thread) => {
                        if let Some(idx) = compose_idx {
                            self.screens.remove(idx);
                        }
                        self.status = "Thread created.".into();
                        // The list we are about to refresh may be showing a
                        // different forum (Home's pane, say) — point it at the
                        // new thread's forum before the reply lands in it.
                        let node_id = thread.node_id;
                        if let Some(list) = self.list_mut()
                            && list.node_id != node_id
                        {
                            list.node_id = node_id;
                            list.page = 1;
                            list.total = 0;
                        }
                        self.load_forum(node_id, 1);
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            self.status = format!("Thread failed: {}", e.message);
                        }
                    }
                }
            }
            Msg::MarkedRead(Ok(())) => self.status = "Marked read.".into(),
            Msg::MarkedRead(Err(e)) => self.status = format!("Mark-read failed: {e}"),
            Msg::ConversationsLoaded { page, result } => {
                let mut new_unread: Option<u32> = None;
                let mut auto_load: Option<u32> = None;
                if let Some(inbox) = self.inbox_mut() {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded (issue #537).
                            inbox.convos.error = None;
                            inbox.convos.conversations = reply.conversations;
                            inbox.convos.page = page;
                            inbox.convos.last_page = reply.pagination.last_page.max(1);
                            inbox.convos.total = reply.pagination.total;
                            inbox.convos.loading = false;
                            inbox.convos.sel = inbox
                                .convos
                                .sel
                                .min(inbox.convos.conversations.len().saturating_sub(1));
                            new_unread = Some(common::models::count_unread_conversations(
                                &inbox.convos.conversations,
                            ));
                            // Prime the view pane with the first conversation
                            // so a dual Inbox never opens onto an empty right
                            // panel.
                            if inbox.dual
                                && inbox.view.is_none()
                                && inbox.tab == screens::InboxTab::Conversations
                                && let Some(first) = inbox.convos.conversations.first().cloned()
                            {
                                let cid = first.conversation_id;
                                inbox.view = Some(screens::ConversationViewState {
                                    conversation: first,
                                    page: 1,
                                    loading: true,
                                    ..Default::default()
                                });
                                auto_load = Some(cid);
                            }
                        }
                        Err(e) => {
                            inbox.convos.loading = false;
                            inbox.convos.error = Some(e.message);
                        }
                    }
                }
                if let Some(n) = new_unread {
                    self.convos_unread = n;
                }
                if let Some(cid) = auto_load {
                    // Primed, not opened: never mark this one read (#541).
                    self.load_conversation(cid, 1, false);
                }
            }
            Msg::ConversationLoaded { id, page, mark_read: user_opened, result } => {
                let mut mark_read: Option<u32> = None;
                if let Some(view) = self.conversation_view_mut(id) {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded/ThreadLoaded (issue #537).
                            view.error = None;
                            if reply.conversation.conversation_id > 0 {
                                view.conversation = reply.conversation;
                            }
                            view.messages = reply.messages;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.loading = false;
                            // A freshly loaded page is a different set of
                            // messages (paging, or a post-reply reload) —
                            // sel_msg/scroll from the previous page no longer
                            // refer to anything here. rebuild_message_lines
                            // also clamps defensively, but resetting here
                            // means the view opens at the top of the new page
                            // instead of some carried-over offset (issue #534).
                            view.sel_msg = 0;
                            view.scroll = 0;
                            // The lines are derived by the renderer (which is
                            // the only place that knows the pane width); this
                            // just invalidates them.
                            view.built = None;
                            if user_opened {
                                mark_read = Some(view.conversation.conversation_id);
                            }
                        }
                        Err(e) => {
                            view.loading = false;
                            view.error = Some(e.message);
                        }
                    }
                }
                if let Some(cid) = mark_read {
                    // Keep the client's own picture in step with the server:
                    // the Inbox row keeps its unread glyph (and the header
                    // badge its count) otherwise, until some later poll
                    // happens to contradict them (issue #541).
                    if let Some(inbox) = self.inbox_mut() {
                        if let Some(row) = inbox
                            .convos
                            .conversations
                            .iter_mut()
                            .find(|c| c.conversation_id == cid)
                        {
                            row.is_unread = false;
                            row.conversation_unread = false;
                        }
                        let unread =
                            common::models::count_unread_conversations(&inbox.convos.conversations);
                        self.convos_unread = unread;
                    }
                    let api = self.api.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let _ = api.mark_conversation_read(cid).await;
                        let _ = tx;
                    });
                }
            }
            Msg::ConvoReplySent(result) => {
                let (compose_idx, target) = self
                    .screens
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, s)| match s {
                        Screen::Compose(c) if matches!(c.target, Some(ComposeTarget::ConversationReply { .. })) => {
                            Some((idx, c.target.clone()))
                        }
                        _ => None,
                    })
                    .unzip();
                match result {
                    Ok(()) => {
                        let cid = match target.flatten() {
                            Some(ComposeTarget::ConversationReply {
                                conversation_id, ..
                            }) => conversation_id,
                            _ => 0,
                        };
                        if let Some(idx) = compose_idx {
                            self.screens.remove(idx);
                        }
                        self.status = "Message sent.".into();
                        if cid > 0 {
                            self.load_conversation(cid, 1, true);
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        } else {
                            self.status = format!("Message failed: {}", e.message);
                        }
                    }
                }
            }
            Msg::RecipientResolved { name, id } => {
                let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::NewConversation(nc) => Some(nc),
                    _ => None,
                });
                let done = if let Some(nc) = nc {
                    nc.resolving = nc.resolving.saturating_sub(1);
                    match id {
                        Some(id) => nc.resolved_ids.push(id),
                        None => nc.errors.push(format!("{name}: not found")),
                    }
                    nc.resolving == 0
                } else {
                    false
                };
                if done {
                    let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                        Screen::NewConversation(nc) => Some(nc),
                        _ => None,
                    });
                    let (ids, title, body) = if let Some(nc) = nc {
                        nc.busy = false;
                        if nc.resolved_ids.is_empty() || !nc.errors.is_empty() {
                            (Vec::new(), String::new(), String::new())
                        } else {
                            (
                                nc.resolved_ids.clone(),
                                nc.title.clone(),
                                nc.body.clone(),
                            )
                        }
                    } else {
                        (Vec::new(), String::new(), String::new())
                    };
                    if !ids.is_empty() {
                        let api = self.api.clone();
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let result = api
                                .create_conversation(&ids, &title, &body)
                                .await
                                .map_err(|e| TaskError::of(&e));
                            tx.send(Msg::ConvoCreated(result)).ok();
                        });
                    }
                }
            }
            Msg::ConvoCreated(result) => match result {
                Ok(conv) => {
                    // Unwind back to the Inbox (or the root, if this started
                    // from somewhere that never opened one — e.g. a Profile's
                    // "message" action), then show the new conversation.
                    while self.screens.len() > 1
                        && !matches!(self.screens.last(), Some(Screen::Inbox(_)))
                    {
                        self.screens.pop();
                    }
                    self.status = "Conversation started.".into();
                    if let Some(inbox) = self.inbox_mut() {
                        inbox.tab = screens::InboxTab::Conversations;
                    }
                    self.open_conversation(conv);
                }
                Err(e) => {
                    let nc = self.screens.iter_mut().rev().find_map(|s| match s {
                        Screen::NewConversation(nc) => Some(nc),
                        _ => None,
                    });
                    if let Some(new) = nc {
                        new.busy = false;
                        new.errors.push(e.message);
                    }
                }
            },
            Msg::AlertsLoaded(result) => {
                let mut new_unread: Option<u32> = None;
                if let Some(inbox) = self.inbox_mut() {
                    let alerts = &mut inbox.alerts;
                    match result {
                        Ok(page) => {
                            // See ForumLoaded (issue #537).
                            alerts.error = None;
                            new_unread =
                                Some(page.alerts.iter().filter(|a| !a.viewed()).count() as u32);
                            alerts.alerts = page.alerts;
                            alerts.loading = false;
                            alerts.sel = alerts.sel.min(alerts.alerts.len().saturating_sub(1));
                        }
                        Err(e) => {
                            alerts.loading = false;
                            alerts.error = Some(e.message);
                        }
                    }
                }
                if let Some(n) = new_unread {
                    self.alerts_unread = n;
                }
            }
            Msg::AlertMarked(result) => match result {
                Ok(()) => {
                    self.load_alerts();
                    self.status = "Alert marked read.".into();
                }
                Err(e) => self.status = format!("Mark failed: {e}"),
            },
            Msg::ConversationMarked(result) => match result {
                Ok(()) => {
                    self.load_conversations(1);
                    self.status = "Conversation marked read.".into();
                }
                Err(e) => self.status = format!("Mark failed: {e}"),
            },
            Msg::SearchDone { page, result } => {
                let search = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Search(search) => Some(search),
                    _ => None,
                });
                if let Some(search) = search {
                    match result {
                        Ok(reply) => {
                            // See ForumLoaded (issue #537).
                            search.error = None;
                            // `set_results` also derives the snippets, so the
                            // renderer never re-parses a post (issue #522).
                            search.set_results(reply.results);
                            search.page = page;
                            search.last_page = reply.pagination.last_page.max(1);
                            search.total = reply.pagination.total;
                            search.loading = false;
                            search.sel = 0;
                        }
                        Err(e) => {
                            search.loading = false;
                            search.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::PaletteMember { query, user } => {
                if let Some(found) = user
                    && let Some(palette) = self.palette.as_mut()
                    && palette.query.trim() == query
                {
                    palette.push_member(&found);
                }
            }
            Msg::ProfileLoaded(result) => {
                let profile = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Profile(profile) => Some(profile),
                    _ => None,
                });
                if let Some(profile) = profile {
                    match result {
                        Ok(user) => {
                            // See ForumLoaded (issue #537).
                            profile.error = None;
                            profile.user = Some(user);
                            profile.loading = false;
                        }
                        Err(e) => {
                            profile.loading = false;
                            profile.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::LoggedOut(result) => match result {
                Ok(()) => tracing::info!("logout complete"),
                Err(e) => {
                    // The local token file is gone either way; say what did
                    // not happen instead of leaving "Logged out." standing.
                    tracing::warn!("logout error: {e}");
                    self.status = format!("Logged out locally, but {e}.");
                }
            },
        }
    }

    pub fn load_nodes(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.nodes().await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::NodesLoaded(result)).ok();
        });
    }

    /// `mark_read` must be `true` only for a load the user asked for (opening
    /// a conversation, paging inside it, the post-reply reload) — never for
    /// the dual-pane Inbox priming its view pane (issue #541).
    pub fn load_conversation(&mut self, id: u32, page: u32, mark_read: bool) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .conversation(id, page)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationLoaded { id, page, mark_read, result }).ok();
        });
    }

    pub fn run_search_query(&mut self, query: SearchQuery) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        let page = query.page;
        self.status = "Searching…".into();
        tokio::spawn(async move {
            let result = api
                .search_advanced(&query)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::SearchDone { page, result }).ok();
        });
    }

    /// Load one image off the UI thread: disk cache first, then the site
    /// through `api_gate`, then decode + encode on a blocking task. Only the
    /// finished payload crosses back, as `Msg::ImageLoaded`.
    #[cfg(feature = "images")]
    fn spawn_image_load(&mut self, pending: crate::images::Pending) {
        let Some(picker) = self.images.picker() else {
            return;
        };
        let client = self.client.clone();
        let disk = self.images.disk();
        let tx = self.tx.clone();
        let slots = self.image_slots.clone();
        tokio::spawn(async move {
            // Decoration waits its turn behind at most a couple of siblings;
            // `image_gate` then spaces the ones that get through, in a lane of
            // its own so nothing here delays an interactive call (issue #543).
            let _permit = slots.acquire_owned().await;
            let key = pending.store_key();
            let result = crate::images::load(&client, &disk, picker, &pending).await;
            tx.send(Msg::ImageLoaded { key, result }).ok();
        });
    }

    #[cfg(not(feature = "images"))]
    fn spawn_image_load(&mut self, _pending: crate::images::Pending) {}

    pub fn open_url(&mut self, url: &str) {
        if url.is_empty() {
            return;
        }
        let target = if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            let base = common::config::base_url();
            if url.starts_with('/') {
                format!("{base}{url}")
            } else {
                format!("{base}/{url}")
            }
        };
        // Remote sessions: even if the opener targets the wrong machine, the
        // URL is now in the user's local clipboard.
        self.copy_text(&target);
        self.status = format!("Opening {target} (also copied to clipboard)");
        let _ = common::oauth::open_browser(&target);
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
) -> Result<(), String> {
    let client_id = common::config::oauth_client_id().map_err(|e| e.to_string())?;
    let http = common::http::build().map_err(|e| e.to_string())?;
    let pkce = common::oauth::generate_pkce();
    let state = common::oauth::generate_state();

    let link = common::oauth::register_link(&http, &state, &pkce.challenge)
        .await
        .map_err(|e| e.to_string())?;
    tx.send(Msg::LoginReady { url: link.url.clone() }).ok();
    let _ = common::oauth::open_browser(&link.url);

    for _ in 0..300 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match common::oauth::poll_link(&http, &link.id).await {
            Ok(common::oauth::PollStatus::Authorized(code)) => {
                let tokens = common::oauth::exchange_code(
                    &http,
                    &code,
                    &common::config::tui_done_url(),
                    &pkce.verifier,
                    &client_id,
                )
                .await
                .map_err(|e| e.to_string())?;
                finish_login(tx, client, tokens).await;
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
) {
    if let Err(e) = client.set_tokens(tokens).await {
        tx.send(Msg::LoginFailed(format!("token store: {e}"))).ok();
        return;
    }
    match client.me().await {
        Ok(user) => {
            tx.send(Msg::LoginComplete(Ok(user))).ok();
        }
        Err(e) => {
            tx.send(Msg::LoginComplete(Err(TaskError::of(&e)))).ok();
        }
    }
}

/// The last page this client already believed `thread_id` had, found the
/// same way `list_showing` locates a forum's list: scan the screen stack for
/// an open `ThreadView` on that thread. `1` when none is open — reply is
/// always initiated from one, but a stale/missing view must not crash the
/// reload, just fall back to asking for page 2 (which the API's max-page
/// clamp will correct if that's wrong too).
/// The topmost open thread view as (`thread_id`, `page`, `sel_post`,
/// `scroll`) — what a like/vote needs to refresh the page it acted on without
/// losing the reader's place.
fn open_thread_position(screens: &[Screen]) -> Option<(u32, u32, usize, u16)> {
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

    /// Issue #527: a revoke that did not take must not hide behind
    /// "Logged out." — the tokens are still live on the server until they
    /// expire, and the only honest place to say so is the status line.
    #[test]
    fn a_failed_revoke_is_reported_in_the_status_line() {
        let mut app = test_app();
        app.status = "Logged out.".into();
        app.handle_msg(Msg::LoggedOut(Err(
            "the server kept refresh token valid".into()
        )));
        assert!(
            app.status.contains("Logged out locally")
                && app.status.contains("refresh token"),
            "the revoke failure was swallowed: {:?}",
            app.status
        );

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
            screen.render(f, area, &app.theme, &app.glyphs);
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
                screen.render(f, area, &app.theme, &app.glyphs);
            })
            .expect("draw");
        let buf2 = term2.backend().buffer().clone();
        let screen2: String = (0..24)
            .map(|y| (0..80).map(|x| buf2[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        assert!(!screen2.contains("Error:"), "thread view still shows the stale error:\n{screen2}");
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

    /// A `WfApi` that never touches the network: every call fails with
    /// `NoToken` except `mark_conversation_read`, which just records the id.
    /// Substituted for `App::api` so a handler test can assert on the
    /// PRESENCE or ABSENCE of a server-side side-effect (issue #541) without
    /// a single request leaving the process.
    #[derive(Default)]
    struct RecordingApi {
        marked_read: std::sync::Mutex<Vec<u32>>,
    }

    impl RecordingApi {
        fn marks(&self) -> Vec<u32> {
            self.marked_read.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl WfApi for RecordingApi {
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
        async fn reply(&self, _: u32, _: &str) -> common::error::Result<common::models::Post> {
            Err(common::error::Error::NoToken)
        }
        async fn create_thread(&self, _: u32, _: &str, _: &str) -> common::error::Result<Thread> {
            Err(common::error::Error::NoToken)
        }
        async fn mark_thread_read(&self, _: u32) -> common::error::Result<()> {
            Err(common::error::Error::NoToken)
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
            _: &[u32],
            _: &str,
            _: &str,
        ) -> common::error::Result<Conversation> {
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
        async fn search_advanced(
            &self,
            _: &SearchQuery,
        ) -> common::error::Result<SearchResultsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn search_member(
            &self,
            _: u32,
            _: &str,
            _: u32,
        ) -> common::error::Result<SearchResultsReply> {
            Err(common::error::Error::NoToken)
        }
        async fn react_post(&self, _: u32, _: u32) -> common::error::Result<Toggle> {
            Err(common::error::Error::NoToken)
        }
        async fn vote_post(&self, _: u32, _: &str) -> common::error::Result<Toggle> {
            Err(common::error::Error::NoToken)
        }
        async fn me(&self) -> common::error::Result<User> {
            Err(common::error::Error::NoToken)
        }
        async fn user(&self, _: u32) -> common::error::Result<User> {
            Err(common::error::Error::NoToken)
        }
        async fn find_user(&self, _: &str) -> common::error::Result<Option<User>> {
            Err(common::error::Error::NoToken)
        }
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

    /// Builds an `App` for poller bookkeeping tests. `start_pollers` spawns
    /// loops that hold `Arc<dyn WfApi>`, but both loops `sleep` for their
    /// full interval before ever calling the API, so a real `WfApiClient`
    /// (reading whatever token store is on this machine, same as any other
    /// cold start) never actually makes a network call within the test.
    fn test_app() -> App {
        let client = Arc::new(WfApiClient::new().expect("client init"));
        let (tx, rx) = mpsc::unbounded_channel();
        App {
            api: client.clone(),
            client,
            tx,
            rx,
            theme: Theme::detect(),
            glyphs: glyph::detect(),
            images: crate::images::Images::default(),
            image_slots: Arc::new(tokio::sync::Semaphore::new(IMAGE_LOAD_CONCURRENCY)),
            screens: Vec::new(),
            me: None,
            alerts_unread: 3,
            convos_unread: 5,
            status: String::new(),
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
            last_title: String::new(),
            should_quit: false,
            poller_handles: Vec::new(),
        }
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
    #[test]
    fn esc_reaches_the_screen_first_and_never_pops_a_busy_composer() {
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
}
