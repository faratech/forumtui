//! Application state: screen stack, message pump, background tasks, frame.

use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tokio::sync::mpsc;

use common::api::{WfApi, WfApiClient};
use common::error::Error;
use common::models::*;

use crate::event;
use crate::screens::{self, Action, ComposeTarget, Screen};
use crate::theme::Theme;

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
    ForumLoaded { page: u32, result: TaskResult<ForumReply> },
    ThreadLoaded { id: u32, page: u32, result: TaskResult<ThreadReply> },
    ReplySent(TaskResult<Post>),
    ThreadCreated(TaskResult<Thread>),
    MarkedRead(TaskResult<()>),
    ConversationsLoaded { page: u32, result: TaskResult<ConversationsReply> },
    ConversationLoaded { id: u32, page: u32, result: TaskResult<ConversationReply> },
    ConvoReplySent(TaskResult<()>),
    ConvoCreated(TaskResult<Conversation>),
    RecipientResolved { name: String, id: Option<u32> },
    AlertsLoaded(TaskResult<AlertsReply>),
    AlertMarked(TaskResult<()>),
    SearchDone { page: u32, result: TaskResult<SearchResultsReply> },
    ProfileLoaded(TaskResult<User>),
    LoggedOut(Result<(), String>),
    Notice(String),
}

pub struct App {
    pub api: Arc<dyn WfApi>,
    pub client: Arc<WfApiClient>,
    pub tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    pub theme: Theme,
    pub screens: Vec<Screen>,
    pub me: Option<User>,
    pub alerts_unread: u32,
    pub convos_unread: u32,
    pub status: String,
    show_help: bool,
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
    should_quit: bool,
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
        ratatui::crossterm::execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
        )?;
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use ratatui::crossterm::event::{
            DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
        };
        use ratatui::crossterm::execute;
        use ratatui::crossterm::terminal::*;
        let _ = execute!(
            std::io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            DisableMouseCapture,
            ratatui::crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

pub async fn run() -> u8 {
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
        screens: Vec::new(),
        me: None,
        alerts_unread: 0,
        convos_unread: 0,
        status: "Starting…".into(),
        show_help: false,
        selection: None,
        screen_rows: Vec::new(),
        screen_cols: Vec::new(),
        clipboard: String::new(),
        last_click_instant: None,
        last_click_pos: (0, 0),
        click_count: 0,
        should_quit: false,
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
            tokio::select! {
                _ = term.recv() => {
                    tx.send(Msg::Notice("quit".into())).ok();
                }
                _ = hup.recv() => {
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
        self.screens.push(Screen::ForumTree(screens::ForumTreeState {
            loading: true,
            ..Default::default()
        }));
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

    fn start_pollers(&self) {
        // Alerts poller: unread count for the status bar.
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(common::config::ALERT_POLL_SECS)).await;
                if let Ok(page) = api.alerts(1).await {
                    let unread = page.alerts.iter().filter(|a| !a.viewed).count() as u32;
                    tx.send(Msg::Notice(format!("alerts:{unread}"))).ok();
                }
            }
        });
        // Conversations unread poller.
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(
                    common::config::CONVERSATION_POLL_SECS,
                ))
                .await;
                if let Ok(page) = api.conversations(1).await {
                    let unread = page
                        .conversations
                        .iter()
                        .filter(|c| c.conversation_unread)
                        .count() as u32;
                    tx.send(Msg::Notice(format!("convos:{unread}"))).ok();
                }
            }
        });
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
            // Block on the reader thread briefly so draws stay responsive.
            // Drain everything pending before the next draw.
            while let Some(input) = crate::event::next(&reader, Duration::from_millis(80)) {
                match input {
                    crate::event::Input::Resize => {}
                    crate::event::Input::Mouse(me) => {
                        self.handle_mouse(me);
                    }
                    crate::event::Input::Paste(text) => {
                        self.handle_paste(text);
                    }
                    crate::event::Input::Key(k) => {
                        if event::is_ctrl_c(k) {
                            return 0;
                        }
                        self.handle_key(k);
                    }
                }
                if self.should_quit {
                    return 0;
                }
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [top, body, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let me_name = self
            .me
            .as_ref()
            .map(|u| u.username.as_str())
            .unwrap_or("anonymous");
        let top_line = Line::from(vec![
            Span::styled(" wftui ", self.theme.title()),
            Span::styled(me_name.to_string(), self.theme.dim()),
            Span::raw("  "),
            Span::styled(
                format!("✉ {} unread", self.convos_unread),
                self.theme.dim(),
            ),
            Span::raw("  "),
            Span::styled(format!("🔔 {} new", self.alerts_unread), self.theme.dim()),
        ]);
        f.render_widget(Paragraph::new(top_line), top);

        // Render the top screen; popups handled inside renderers.
        let screen = self.screens.last_mut().expect("screen stack never empty");
        screen.render(f, body, &self.theme);

        let status_line = Line::from(Span::styled(
            format!(" {} ", self.status),
            Style::new().fg(self.theme.dim),
        ));
        f.render_widget(Paragraph::new(status_line), status);

        if self.show_help {
            let area = crate::screens::centered_box(f.area(), 58, 17);
            f.render_widget(ratatui::widgets::Clear, area);
            let block = ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_style(self.theme.accent)
                .title(Span::styled(" Keys (? closes) ", self.theme.title()));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let keys = [
                "Enter/1/2/3  open forum · news · alerts · tutorials",
                "j/k ↑↓       move            Enter open",
                "[ ] PgUp/Dn  page            r refresh",
                "r reply      N new thread    m mark read",
                "o links      u open in web   p OP profile",
                "c DMs        a alerts        s search",
                "i edit query (search)        n new DM",
                "Ctrl+S send (compose)        Tab next field",
                "Esc back     q quit          Ctrl+L logout",
                "drag mouse   select + copy   wheel scroll  Ctrl+Y paste",
            ];
            let lines: Vec<Line> = keys
                .iter()
                .map(|k| Line::from(Span::styled((*k).to_string(), self.theme.base())))
                .collect();
            f.render_widget(Paragraph::new(lines), inner);
        }

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
            for x in 0..area.width {
                offsets.push(row.len());
                row.push_str(f.buffer_mut()[(x, y)].symbol());
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
        let capture = self.screens.last().map(|s| s.input_capture()).unwrap_or(false);
        // Global navigation only when no input field owns the keyboard.
        if k.modifiers.is_empty() && !capture && !self.input_active() {
            match k.code {
                KeyCode::Char('c') => {
                    self.open_conversations();
                    return;
                }
                KeyCode::Char('a') => {
                    self.open_alerts();
                    return;
                }
                KeyCode::Char('s') => {
                    self.push_screen(screens::search_state());
                    return;
                }
                _ => {}
            }
        }
        if k.code == KeyCode::Esc && !capture {
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
            Action::OpenThreadList(node_id, title) => {
                self.push_screen(Screen::ThreadList(screens::ThreadListState {
                    node_id,
                    title,
                    page: 1,
                    loading: true,
                    ..Default::default()
                }));
                self.load_forum(node_id, 1);
            }
            Action::OpenThread(thread) => self.open_thread(&thread),
            Action::OpenProfile(user_id, name) => self.open_profile(user_id, &name),
            Action::OpenConversation(conv) => {
                let cid = conv.conversation_id;
                self.push_screen(Screen::ConversationView(screens::ConversationViewState {
                    conversation: conv,
                    page: 1,
                    loading: true,
                    ..Default::default()
                }));
                self.load_conversation(cid, 1);
            }
            Action::LoadForum(node_id, page) => self.load_forum(node_id, page),
            Action::LoadThread(id, page) => self.load_thread(id, page),
            Action::LoadConversations(page) => self.load_conversations(page),
            Action::LoadConversation(id, page) => self.load_conversation(id, page),
            Action::LoadAlerts => self.load_alerts(),
            Action::LoadNodes => {
                if let Some(Screen::ForumTree(tree)) = self.screens.last_mut() {
                    tree.loading = true;
                    tree.error = None;
                }
                self.load_nodes();
            }
            Action::RunSearch(query, page) => self.run_search(query, page),
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
            Action::StartReply(thread) => self.reply_to_thread(&thread),
            Action::StartReplyConversation(conv) => self.reply_to_conversation(&conv),
            Action::StartNewThread(node_id) => self.new_thread(node_id),
            Action::StartNewConversation => {
                self.push_screen(Screen::NewConversation(screens::NewConversationState::default()));
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
                let text = self.clipboard.clone();
                if text.is_empty() {
                    self.status = "Nothing copied yet — drag or double-click to select text.".into();
                    return;
                }
                self.handle_paste(text);
            }
            Action::OscCopy(value) => {
                emit_raw(&common::osc::set_clipboard(&value));
                self.status =
                    "Copied to your clipboard (OSC 52 — enable clipboard access in your \
                     terminal if it did not arrive)."
                        .into();
            }
            Action::OpenUrl(url) => self.open_url(&url),
        }
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
                    crate::editor::insert_str(&mut ss.query, &mut ss.cursor, &sanitized);
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
                            emit_raw(&common::osc::set_clipboard(&text));
                            self.clipboard = text;
                            let n = self.clipboard.chars().count();
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
                            emit_raw(&common::osc::set_clipboard(&text));
                            self.clipboard = text;
                            let n = self.clipboard.chars().count();
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
                        emit_raw(&common::osc::set_clipboard(&text));
                        self.clipboard = text;
                        let n = self.clipboard.chars().count();
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
            if let Some(c) = get_char(start_col - 1)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                start_col -= 1;
                continue;
            }
            break;
        }

        let mut end_col = x;
        while end_col < max_col {
            if let Some(c) = get_char(end_col + 1)
                && !c.is_whitespace()
                && is_word_char(c) == target_is_word
            {
                end_col += 1;
                continue;
            }
            break;
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

    fn open_conversations(&mut self) {
        self.push_screen(Screen::Conversations(screens::ConversationsState {
            loading: true,
            ..Default::default()
        }));
        self.load_conversations(1);
    }

    pub fn load_conversations(&mut self, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.conversations(page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationsLoaded { page, result }).ok();
        });
    }

    fn open_alerts(&mut self) {
        self.push_screen(Screen::Alerts(screens::AlertsState {
            loading: true,
            ..Default::default()
        }));
        self.load_alerts();
    }

    pub fn load_alerts(&mut self) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.alerts(1).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::AlertsLoaded(result)).ok();
        });
    }

    pub fn load_forum(&mut self, node_id: u32, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api.forum(node_id, page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ForumLoaded { page, result }).ok();
        });
    }

    pub fn open_thread(&mut self, thread: &Thread) {
        self.push_screen(Screen::ThreadView(screens::ThreadViewState {
            thread: thread.clone(),
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
        tokio::spawn(async move {
            let result = api.user(user_id).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::ProfileLoaded(result)).ok();
        });
    }

    pub fn reply_to_thread(&mut self, thread: &Thread) {
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: thread.thread_id,
                thread_title: thread.title.clone(),
            }),
            ..Default::default()
        }));
    }

    pub fn new_thread(&mut self, node_id: u32) {
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::NewThread { node_id }),
            title_field: true,
            ..Default::default()
        }));
    }

    pub fn reply_to_conversation(&mut self, conv: &Conversation) {
        self.push_screen(Screen::Compose(screens::ComposeState {
            target: Some(ComposeTarget::ConversationReply {
                conversation_id: conv.conversation_id,
                conversation_title: conv.title.clone(),
            }),
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
            let result = async {
                if let Ok(token) = client.valid_token().await {
                    let http = common::http::build()?;
                    let _ = common::oauth::revoke(&http, &token).await;
                }
                client.forget_tokens().await
            }
            .await;
            tx.send(Msg::LoggedOut(result.map_err(|e| e.to_string()))).ok();
        });
        self.me = None;
        self.screens.retain(|s| matches!(s, Screen::ForumTree(_)));
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
            }
            Msg::LoginComplete(Err(e)) => {
                if let Some(Screen::Login(ls)) = self.screens.last_mut() {
                    ls.busy = false;
                    ls.error = Some(e.message);
                    ls.stage = crate::screens::LoginStage::Idle;
                }
            }
            Msg::Notice(n) => {
                if n == "quit" {
                    self.should_quit = true;
                } else if let Some(v) = n.strip_prefix("alerts:") {
                    self.alerts_unread = v.parse().unwrap_or(0);
                } else if let Some(v) = n.strip_prefix("convos:") {
                    self.convos_unread = v.parse().unwrap_or(0);
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
                let tree = self.screens.iter_mut().find_map(|s| match s {
                    Screen::ForumTree(tree) => Some(tree),
                    _ => None,
                });
                if let Some(tree) = tree {
                    match result {
                        Ok(nodes) => {
                            tree.nodes = nodes;
                            tree.loading = false;
                        }
                        Err(e) => {
                            tree.loading = false;
                            tree.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ForumLoaded { page, result } => {
                let list = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ThreadList(list) => Some(list),
                    _ => None,
                });
                if let Some(list) = list {
                    match result {
                        Ok(reply) => {
                            list.threads = reply.threads;
                            list.page = page;
                            list.last_page = reply.pagination.last_page.max(1);
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
                let view = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ThreadView(view) if view.thread.thread_id == id => Some(view),
                    _ => None,
                });
                if let Some(view) = view {
                    match result {
                        Ok(reply) => {
                            if !reply.thread.title.is_empty() {
                                view.thread.title = reply.thread.title.clone();
                            }
                            view.posts = reply.posts;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.loading = false;
                            view.scroll = 0;
                            view.rebuild_lines(&self.theme);
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
                            self.load_thread(thread_id, 1);
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
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
                        self.load_forum(thread.node_id, 1);
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::MarkedRead(Ok(())) => self.status = "Marked read.".into(),
            Msg::MarkedRead(Err(e)) => self.status = format!("Mark-read failed: {e}"),
            Msg::ConversationsLoaded { page, result } => {
                let convs = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Conversations(convs) => Some(convs),
                    _ => None,
                });
                if let Some(convs) = convs {
                    match result {
                        Ok(reply) => {
                            convs.conversations = reply.conversations;
                            convs.page = page;
                            convs.last_page = reply.pagination.last_page.max(1);
                            convs.loading = false;
                            convs.sel = convs.sel.min(convs.conversations.len().saturating_sub(1));
                        }
                        Err(e) => {
                            convs.loading = false;
                            convs.error = Some(e.message);
                        }
                    }
                }
            }
            Msg::ConversationLoaded { id, page, result } => {
                let view = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::ConversationView(view) if view.conversation.conversation_id == id => {
                        Some(view)
                    }
                    _ => None,
                });
                if let Some(view) = view {
                    match result {
                        Ok(reply) => {
                            view.messages = reply.messages;
                            view.page = page;
                            view.last_page = reply.pagination.last_page.max(1);
                            view.loading = false;
                            view.rebuild_lines(&self.theme);
                            let cid = view.conversation.conversation_id;
                            let api = self.api.clone();
                            let tx = self.tx.clone();
                            tokio::spawn(async move {
                                let _ = api.mark_conversation_read(cid).await;
                                let _ = tx;
                            });
                        }
                        Err(e) => {
                            view.loading = false;
                            view.error = Some(e.message);
                        }
                    }
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
                            self.load_conversation(cid, 1);
                        }
                    }
                    Err(e) => {
                        if let Some(idx) = compose_idx
                            && let Screen::Compose(compose) = &mut self.screens[idx]
                        {
                            compose.busy = false;
                            compose.error = Some(e.message);
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
                    // Unwind back to the conversations list, then show the new one.
                    while self.screens.len() > 1
                        && !matches!(self.screens.last(), Some(Screen::Conversations(_)))
                    {
                        self.screens.pop();
                    }
                    self.status = "Conversation started.".into();
                    let cid = conv.conversation_id;
                    self.push_screen(Screen::ConversationView(
                        screens::ConversationViewState {
                            conversation: conv,
                            page: 1,
                            loading: true,
                            ..Default::default()
                        },
                    ));
                    self.load_conversation(cid, 1);
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
                let alerts = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Alerts(alerts) => Some(alerts),
                    _ => None,
                });
                if let Some(alerts) = alerts {
                    match result {
                        Ok(page) => {
                            self.alerts_unread =
                                page.alerts.iter().filter(|a| !a.viewed).count() as u32;
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
            }
            Msg::AlertMarked(result) => match result {
                Ok(()) => {
                    self.load_alerts();
                    self.status = "Alert marked read.".into();
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
                            search.results = reply.results;
                            search.page = page;
                            search.last_page = reply.pagination.last_page.max(1);
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
            Msg::ProfileLoaded(result) => {
                let profile = self.screens.iter_mut().rev().find_map(|s| match s {
                    Screen::Profile(profile) => Some(profile),
                    _ => None,
                });
                if let Some(profile) = profile {
                    match result {
                        Ok(user) => {
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
                Err(e) => tracing::warn!("logout error: {e}"),
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

    pub fn load_conversation(&mut self, id: u32, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .conversation(id, page)
                .await
                .map_err(|e| TaskError::of(&e));
            tx.send(Msg::ConversationLoaded { id, page, result }).ok();
        });
    }

    pub fn run_search(&mut self, query: String, page: u32) {
        let api = self.api.clone();
        let tx = self.tx.clone();
        self.status = "Searching…".into();
        tokio::spawn(async move {
            let result = api.search(&query, page).await.map_err(|e| TaskError::of(&e));
            tx.send(Msg::SearchDone { page, result }).ok();
        });
    }

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
        emit_raw(&common::osc::set_clipboard(&target));
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

/// Shared footer hint lines used by list screens.
pub fn footer_line(theme: &Theme, hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, desc) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled((*desc).to_string(), theme.dim()));
    }
    Line::from(spans)
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
