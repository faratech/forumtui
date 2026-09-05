//! Social screens: the Inbox (conversations + alerts, tabbed and — at >= 110
//! columns — split with the open conversation), the standalone conversation
//! view + new-conversation composer it can still push, and the pieces shared
//! between the inline and standalone view.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::{
    browse::{push_bbcode, truncate},
    Action, AlertsState, ConversationViewState, ConversationsState, InboxPane, InboxState,
    InboxTab, NewConversationState,
};
use crate::chrome::{self, Hints};
use crate::glyph::Glyphs;
use crate::theme::{fmt_age, fmt_time, Theme};

/// Both panes fit side by side from here up (DESIGN.md) — the same threshold
/// Home uses for its own tree+list split.
const INBOX_DUAL_MIN_COLS: u16 = 110;
/// Width of the tabbed list pane in the two-pane layout (DESIGN.md: "Inbox
/// list 50 + view").
const INBOX_PANE_COLS: u16 = 50;

// ================= Inbox =================

pub fn inbox_hints() -> Hints {
    Hints::with_short(
        &[
            ("Enter", "open"),
            ("Tab", "alerts/conversations"),
            ("n", "new message"),
            ("r", "reply"),
            ("m", "mark read"),
            ("R", "refresh"),
            ("Esc", "back"),
        ],
        &[
            ("Enter", "open"),
            ("Tab", "tabs"),
            ("n", "new"),
            ("r", "reply"),
            ("m", "read"),
            ("R", ""),
            ("Esc", "back"),
        ],
        0,
    )
}

/// `Tab` means two different things depending on which half has the
/// keyboard: from the tabbed list it switches Conversations/Alerts, from the
/// view pane it returns focus to the list (mirrored by `q`, `h`/`Left`, and by
/// the global Esc branch in `app::handle_key` since Esc never reaches here for
/// an unfocused-capture screen). `q` goes back one level here, not out of the
/// client — the same key on the pushed `ConversationView` pops that screen, and
/// the inline pane is the same step at a wider terminal.
/// `n` (new message) and `r` (reply to whatever the view pane already has open)
/// work from the list regardless of which tab is active; from the view pane `r`
/// is handled identically inside `conversation_view_key_inner`.
pub fn inbox_key(s: &mut InboxState, key: KeyEvent) -> Action {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        match s.focus {
            InboxPane::List => s.tab = s.tab.other(),
            InboxPane::View => s.focus = InboxPane::List,
        }
        return Action::None;
    }
    match s.focus {
        InboxPane::View => match key.code {
            KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                s.focus = InboxPane::List;
                Action::None
            }
            _ => match &mut s.view {
                Some(view) => conversation_view_key_inner(view, key),
                None => {
                    s.focus = InboxPane::List;
                    Action::None
                }
            },
        },
        InboxPane::List => {
            match key.code {
                KeyCode::Char('n') => return Action::StartNewConversation(None),
                KeyCode::Char('r') => {
                    return match &s.view {
                        Some(view) => Action::StartReplyConversation(view.conversation.clone()),
                        None => Action::None,
                    };
                }
                _ => {}
            }
            match s.tab {
                InboxTab::Conversations => conversations_list_key(&mut s.convos, key),
                InboxTab::Alerts => alerts_list_key(&mut s.alerts, key),
            }
        }
    }
}

fn conversations_list_key(c: &mut ConversationsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        // `r` is taken by "reply to the open conversation" one level up, so
        // manual refresh is `R`/F5. Without it the only way to re-fetch is to
        // leave the screen and come back — `open_inbox` short-circuits when
        // the Inbox is already on top.
        KeyCode::Char('R') | KeyCode::F(5) => {
            c.loading = true;
            Action::LoadConversations(c.page)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if c.sel > 0 {
                c.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if c.sel + 1 < c.conversations.len() {
                c.sel += 1;
            }
            Action::None
        }
        KeyCode::Char('[') | KeyCode::PageUp => {
            if c.page > 1 {
                c.loading = true;
                Action::LoadConversations(c.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            if c.page < c.last_page {
                c.loading = true;
                Action::LoadConversations(c.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => match c.conversations.get(c.sel) {
            Some(conv) => Action::OpenConversation(conv.clone()),
            None => Action::None,
        },
        KeyCode::Char('m') => match c.conversations.get(c.sel) {
            Some(conv) => Action::MarkConversationRead(conv.conversation_id),
            None => Action::None,
        },
        _ => Action::None,
    }
}

fn alerts_list_key(a: &mut AlertsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        // See `conversations_list_key`: `R`/F5, because `r` replies.
        KeyCode::Char('R') | KeyCode::F(5) => {
            a.loading = true;
            Action::LoadAlerts
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if a.sel > 0 {
                a.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if a.sel + 1 < a.alerts.len() {
                a.sel += 1;
            }
            Action::None
        }
        // "Enter opens as today": today's Enter on an alert marks it read
        // (there is no separate "open" step short of `o`), so that is exactly
        // what the Inbox's primary `Enter` hint does here too.
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('m') => match a.alerts.get(a.sel) {
            Some(al) => Action::MarkAlertRead(al.alert_id),
            None => Action::None,
        },
        KeyCode::Char('o') => match a.alerts.get(a.sel).and_then(|al| al.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::None,
        },
        _ => Action::None,
    }
}

pub fn render_inbox(
    s: &mut InboxState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let dual = area.width >= INBOX_DUAL_MIN_COLS;
    s.dual = dual;
    if !dual {
        match s.focus {
            InboxPane::List => render_inbox_list(s, f, area, theme, g, true),
            InboxPane::View => match &mut s.view {
                Some(view) => render_inbox_view_panel(view, f, area, theme, g, true),
                None => render_inbox_list(s, f, area, theme, g, true),
            },
        }
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Length(INBOX_PANE_COLS), Constraint::Min(0)]).areas(area);
    render_inbox_list(s, f, left, theme, g, s.focus == InboxPane::List);
    match &mut s.view {
        Some(view) => render_inbox_view_panel(view, f, right, theme, g, s.focus == InboxPane::View),
        None => {
            f.render_widget(chrome::panel(theme, g, "Inbox", false, None, None), right);
        }
    }
}

fn render_inbox_list(
    s: &mut InboxState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    let bottom = match s.tab {
        InboxTab::Conversations => conversations_range(&s.convos),
        InboxTab::Alerts => alerts_range(&s.alerts),
    };
    let block = chrome::panel(theme, g, "Inbox", focused, None, bottom.as_deref());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [tabs_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    f.render_widget(Paragraph::new(tab_row(theme, s)), tabs_area);

    match s.tab {
        InboxTab::Conversations => {
            render_conversations_rows(&mut s.convos, f, body_area, theme, g, focused)
        }
        InboxTab::Alerts => render_alerts_rows(&mut s.alerts, f, body_area, theme, g, focused),
    }
}

/// The `Conversations N | Alerts N` tab row: the active tab's chip in
/// `accent_bg`, the other neutral. The counts are unread counts (matching the
/// header's own `Inbox`/`Alerts` badges), not the number of rows loaded.
fn tab_row(theme: &Theme, s: &InboxState) -> Vec<Line<'static>> {
    let unread_convos = s
        .convos
        .conversations
        .iter()
        .filter(|c| c.is_unread_conv())
        .count();
    let unread_alerts = s.alerts.alerts.iter().filter(|a| !a.viewed).count();
    let conv_label = format!("Conversations {unread_convos}");
    let alert_label = format!("Alerts {unread_alerts}");
    let (conv_chip, alert_chip) = match s.tab {
        InboxTab::Conversations => (
            chrome::chip_active(theme, &conv_label),
            chrome::chip(theme, &alert_label),
        ),
        InboxTab::Alerts => (
            chrome::chip(theme, &conv_label),
            chrome::chip_active(theme, &alert_label),
        ),
    };
    vec![
        Line::from(vec![Span::raw(" "), conv_chip, Span::raw(" "), alert_chip]),
        Line::from(Span::raw("")),
    ]
}

/// `1–5 of 18`, the same math as the thread list's bottom footer (browse.rs's
/// `list_range` — private there, so reimplemented here).
fn conversations_range(s: &ConversationsState) -> Option<String> {
    if s.conversations.is_empty() {
        return None;
    }
    if s.total == 0 {
        return Some(format!("{} conversations", s.conversations.len()));
    }
    let len = s.conversations.len() as u64;
    let (start, end) = if s.page >= s.last_page.max(1) && s.total >= len {
        (s.total - len + 1, s.total)
    } else {
        let start = (s.page.max(1) as u64 - 1) * len + 1;
        (start, start + len - 1)
    };
    Some(format!("{start}\u{2013}{end} of {}", s.total))
}

fn alerts_range(s: &AlertsState) -> Option<String> {
    if s.alerts.is_empty() {
        return None;
    }
    let unread = s.alerts.iter().filter(|a| !a.viewed).count();
    Some(format!("{unread} of {} unread", s.alerts.len()))
}

/// Two-line conversation rows: unread glyph + bold title, then dim
/// `participants · N replies · age`.
fn render_conversations_rows(
    s: &mut ConversationsState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if s.loading && s.conversations.is_empty() {
        f.render_widget(
            Paragraph::new("Loading conversations…").style(theme.dim()),
            area,
        );
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            area,
        );
        return;
    }
    if s.conversations.is_empty() {
        f.render_widget(Paragraph::new("No conversations yet.").style(theme.dim()), area);
        return;
    }

    let tw = (area.width as usize).saturating_sub(3);
    let items: Vec<ListItem> = s
        .conversations
        .iter()
        .map(|c| {
            let unread = c.is_unread_conv();
            let title_style = if unread {
                theme.base().add_modifier(Modifier::BOLD)
            } else {
                theme.base()
            };
            let marker = if unread {
                Span::styled(g.unread.to_string(), Style::new().fg(theme.accent))
            } else {
                Span::raw(" ")
            };
            let line1 = Line::from(vec![
                Span::raw(" "),
                marker,
                Span::raw(" "),
                Span::styled(truncate(&c.title, tw), title_style),
            ]);
            let meta = format!(
                "{} \u{00B7} {} replies \u{00B7} {}",
                c.participants_display(),
                c.reply_count,
                fmt_age(c.last_message_date)
            );
            let line2 = Line::from(vec![
                Span::raw("   "),
                Span::styled(truncate(&meta, tw), theme.dim()),
            ]);
            ListItem::new(vec![line1, line2])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.conversations.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    f.render_stateful_widget(list, area, &mut state);
}

/// Alert rows: the kind glyph, username bold when unread, dim age.
fn render_alerts_rows(
    s: &mut AlertsState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if s.loading && s.alerts.is_empty() {
        f.render_widget(Paragraph::new("Loading alerts…").style(theme.dim()), area);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            area,
        );
        return;
    }
    if s.alerts.is_empty() {
        f.render_widget(Paragraph::new("No alerts yet.").style(theme.dim()), area);
        return;
    }

    let items: Vec<ListItem> = s
        .alerts
        .iter()
        .map(|a| {
            let (marker, style) = if a.viewed {
                (Span::raw(" "), theme.dim())
            } else {
                (
                    Span::styled(g.unread.to_string(), Style::new().fg(theme.accent)),
                    theme.base().add_modifier(Modifier::BOLD),
                )
            };
            let kind = a.content_type.to_ascii_lowercase();
            let kind_glyph = if kind.contains("quote") {
                g.quote_alert
            } else if kind.contains("mention") {
                g.mention
            } else {
                g.reply_alert
            };
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                marker,
                Span::raw(" "),
                Span::styled(format!("{kind_glyph} "), theme.dim()),
                Span::styled(a.username.clone(), style),
                Span::styled(format!("  {} ", a.content_type.replace('_', " ")), theme.dim()),
                Span::styled(fmt_age(a.alert_date), theme.dim()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel.min(s.alerts.len() - 1)));
    let list = List::new(items);
    let list = if focused {
        list.highlight_style(theme.selected())
    } else {
        list
    };
    f.render_stateful_widget(list, area, &mut state);
}

/// The inline view pane: title = the conversation, right segment `r reply`
/// (DESIGN.md's Inbox artboard — the page-position segment other panels show
/// there is replaced by the one key this pane actually wants advertised).
/// Then `with <participants> · started <age> · N messages`, a rule, and
/// message cards.
fn render_inbox_view_panel(
    view: &mut ConversationViewState,
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &Theme,
    g: &Glyphs,
    focused: bool,
) {
    let title = truncate(&view.conversation.title, 40);
    let block = chrome::panel(theme, g, &title, focused, Some("r reply"), None);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if view.loading && view.messages.is_empty() {
        f.render_widget(Paragraph::new("Loading messages…").style(theme.dim()), inner);
        return;
    }
    if let Some(err) = &view.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    let started = view.messages.first().map(|m| m.message_date).unwrap_or(0);
    let total_messages = view.conversation.reply_count + 1;
    let mut meta = vec![
        Span::styled("with ", theme.dim()),
        Span::styled(view.participants_display(), theme.base()),
    ];
    let mut tail = String::new();
    if started > 0 {
        tail.push_str(&format!(" \u{00B7} started {} ago", fmt_age(started)));
    }
    tail.push_str(&format!(" \u{00B7} {total_messages} messages"));
    meta.push(Span::styled(tail, theme.dim()));
    let rule = if g.ascii { "-" } else { "\u{2500}" };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(meta),
            Line::from(Span::styled(rule.repeat(inner.width as usize), theme.faint())),
        ]),
        header_area,
    );

    view.rebuild_message_lines(theme, g, body_area.width);
    let total = view.lines.len() as u16;
    let max_scroll = total.saturating_sub(body_area.height);
    if view.scroll > max_scroll {
        view.scroll = max_scroll;
    }
    f.render_widget(
        Paragraph::new(view.lines.clone()).scroll((view.scroll, 0)),
        body_area,
    );
}

/// Left content, then padding, then right-aligned content — a small local
/// cousin of ThreadView's `justify` (private to `browse`), used for the
/// message-card header's right-aligned age.
fn justify_line(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    if lw + rw < width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
        Line::from(spans)
    } else {
        Line::from(left)
    }
}

/// Greedy word-wrap over styled spans for the message-card body — a smaller
/// cousin of ThreadView's wrapper (also private to `browse`). Good enough for
/// a compact preview card; it does not try to preserve exact inter-word
/// spacing beyond a single space.
fn wrap_line(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for sp in spans {
        for word in sp.content.split_whitespace() {
            let wlen = word.chars().count();
            if wlen > width {
                if used > 0 {
                    out.push(std::mem::take(&mut line));
                }
                let mut rest = word;
                while rest.chars().count() > width {
                    let head: String = rest.chars().take(width).collect();
                    let bytes = head.len();
                    out.push(vec![Span::styled(head, sp.style)]);
                    rest = &rest[bytes..];
                }
                line.push(Span::styled(rest.to_string(), sp.style));
                used = rest.chars().count();
                continue;
            }
            if used > 0 && used + 1 + wlen > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            } else if used > 0 {
                line.push(Span::raw(" "));
                used += 1;
            }
            line.push(Span::styled(word.to_string(), sp.style));
            used += wlen;
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

// ================= conversation view (standalone + shared) =================

impl ConversationViewState {
    pub fn all_participants(&self) -> Vec<String> {
        let mut names = self.conversation.participant_names();
        for msg in &self.messages {
            if !msg.username.is_empty()
                && !names.iter().any(|n| n.eq_ignore_ascii_case(&msg.username))
            {
                names.push(msg.username.clone());
            }
        }
        names
    }

    pub fn participants_display(&self) -> String {
        let starter = self.conversation.starter();
        let all = self.all_participants();
        if all.is_empty() {
            return "No participants listed".to_string();
        }
        let mut parts = Vec::new();
        if !starter.is_empty() {
            parts.push(format!("{starter} (starter)"));
        }
        for name in all {
            if !starter.eq_ignore_ascii_case(&name) {
                parts.push(name);
            }
        }
        parts.join(", ")
    }

    pub fn rebuild_lines(&mut self, theme: &Theme, g: &Glyphs) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut msg_offsets: Vec<u16> = Vec::new();
        let rule = if g.ascii { "-" } else { "\u{2500}" };

        // Prominent participants callout header at the top
        let starter = self.conversation.starter();
        let participants_str = self.participants_display();
        lines.push(Line::from(Span::styled(
            format!("CONVERSATION PARTICIPANTS {}", rule.repeat(48)),
            theme.faint(),
        )));
        if !starter.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", g.gutter), theme.faint()),
                Span::styled("Started by: ", theme.dim()),
                Span::styled(starter.to_string(), theme.title().add_modifier(Modifier::BOLD)),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", g.gutter), theme.faint()),
            Span::styled("All participants: ", theme.dim()),
            Span::styled(participants_str, theme.base().add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(Span::styled(rule.repeat(74), theme.faint())));
        lines.push(Line::from(Span::raw("")));

        for (i, msg) in self.messages.iter().enumerate() {
            let offset = lines.len() as u16;
            msg_offsets.push(offset);
            let num = i + 1 + ((self.page.saturating_sub(1)) as usize * 20);
            lines.push(Line::from(vec![
                chrome::initials_chip(theme, &msg.username),
                Span::raw(" "),
                Span::styled(
                    msg.username.clone(),
                    theme.base().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" · #{num} · {}", fmt_time(msg.message_date)), theme.dim()),
            ]));
            let mut sink: Vec<Line<'static>> = Vec::new();
            let mut links = Vec::new();
            push_bbcode(&mut sink, &mut links, &msg.message, theme);
            lines.extend(sink);
            lines.push(Line::from(Span::raw("")));
        }
        self.lines = lines;
        self.msg_line_offsets = msg_offsets;
    }

    /// Message-card lines for the Inbox's inline view pane: initials chip,
    /// bold username, right-aligned age, then the body behind a gutter glyph
    /// (accent when this is the selected message, per DESIGN.md). Rebuilt on
    /// every render — a conversation is a handful of messages, so this is
    /// cheap — which is simpler than width-caching and keeps the `n`/`N`
    /// navigation in `conversation_view_key_inner` working off always-fresh
    /// offsets. Unlike `rebuild_lines` (the standalone screen's banner-style
    /// dump), this has no "CONVERSATION PARTICIPANTS" header — the Inbox pane
    /// draws its own compact `with … · started … · N messages` line above it.
    pub fn rebuild_message_lines(&mut self, theme: &Theme, g: &Glyphs, width: u16) {
        let width = width.max(1) as usize;
        let body_w = width.saturating_sub(3).max(4);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut offsets: Vec<u16> = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            offsets.push(lines.len() as u16);
            let header_left = vec![
                chrome::initials_chip(theme, &msg.username),
                Span::raw("  "),
                Span::styled(
                    msg.username.clone(),
                    theme.base().add_modifier(Modifier::BOLD),
                ),
            ];
            let header_right = vec![Span::styled(fmt_age(msg.message_date), theme.dim())];
            lines.push(justify_line(header_left, header_right, width));

            let gutter_style = if i == self.sel_msg {
                Style::new().fg(theme.accent)
            } else {
                theme.faint()
            };
            let mut sink: Vec<Line<'static>> = Vec::new();
            let mut links = Vec::new();
            push_bbcode(&mut sink, &mut links, &msg.message, theme);
            for logical in sink {
                for wrapped in wrap_line(&logical.spans, body_w) {
                    let mut spans = vec![
                        Span::raw(" "),
                        Span::styled(g.gutter.to_string(), gutter_style),
                        Span::raw(" "),
                    ];
                    spans.extend(wrapped);
                    lines.push(Line::from(spans));
                }
            }
            lines.push(Line::from(Span::raw("")));
        }
        self.lines = lines;
        self.msg_line_offsets = offsets;
    }
}

pub fn conversation_view_key(s: &mut ConversationViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
        _ => conversation_view_key_inner(s, key),
    }
}

/// Scrolling, paging, and the content actions (reply, next/prev message,
/// author/starter profile) — shared by the standalone `ConversationView`
/// screen and the Inbox's inline view pane. Navigation-away keys (Esc/q/h/
/// Left) differ by context (pop the screen vs. return focus to the Inbox
/// list), so callers handle those themselves before delegating here.
fn conversation_view_key_inner(s: &mut ConversationViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            s.scroll = s.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            s.scroll = s.scroll.saturating_add(1);
            Action::None
        }
        KeyCode::PageUp => {
            s.scroll = s.scroll.saturating_sub(20);
            Action::None
        }
        KeyCode::PageDown => {
            s.scroll = s.scroll.saturating_add(20);
            Action::None
        }
        KeyCode::Char('[') => {
            if s.page > 1 {
                s.loading = true;
                Action::LoadConversation(s.conversation.conversation_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadConversation(s.conversation.conversation_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char('n') => {
            if !s.msg_line_offsets.is_empty() && s.sel_msg + 1 < s.msg_line_offsets.len() {
                s.sel_msg += 1;
                s.scroll = s.msg_line_offsets[s.sel_msg];
            }
            Action::None
        }
        KeyCode::Char('N') => {
            if s.sel_msg > 0 && !s.msg_line_offsets.is_empty() {
                s.sel_msg -= 1;
                s.scroll = s.msg_line_offsets[s.sel_msg];
            }
            Action::None
        }
        KeyCode::Char('p') => {
            if let Some(msg) = s.messages.get(s.sel_msg).or_else(|| s.messages.first()) {
                Action::OpenProfile(msg.user_id, msg.username.clone())
            } else {
                Action::None
            }
        }
        KeyCode::Char('P') => {
            let uid = s.conversation.starter_id();
            let name = s.conversation.starter().to_string();
            if !name.is_empty() {
                Action::OpenProfile(uid, name)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') => Action::StartReplyConversation(s.conversation.clone()),
        _ => Action::None,
    }
}

pub fn conversation_view_hints() -> Hints {
    Hints::with_short(
        &[
            ("r", "reply"),
            ("j/k", "scroll"),
            ("n/N", "next/prev msg"),
            ("p", "author profile"),
            ("P", "starter profile"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
        &[
            ("r", "reply"),
            ("j/k", ""),
            ("n/N", "msg"),
            ("p", "profile"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
        0,
    )
}

pub fn conversation_view_crumb(s: &ConversationViewState) -> String {
    s.conversation.title.clone()
}

pub fn render_conversation_view(
    s: &mut ConversationViewState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let right = format!("page {} of {}", s.page.max(1), s.last_page.max(1));
    let bottom = if s.messages.is_empty() {
        None
    } else {
        Some(format!("{} messages", s.messages.len()))
    };
    let block = super::solo_panel(
        theme,
        g,
        &truncate(&s.conversation.title, 40),
        Some(&right),
        bottom.as_deref(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.lines.is_empty() {
        f.render_widget(Paragraph::new("Loading messages…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let body = Layout::vertical([
        Constraint::Length(2), // Persistent participant header
        Constraint::Min(1),    // Scrollable message body
    ])
    .split(inner);

    // Persistent participant header bar
    let rule = if g.ascii { "-" } else { "\u{2500}" };
    let header_lines = vec![
        Line::from(vec![
            Span::styled("With: ", theme.dim()),
            Span::styled(
                s.participants_display(),
                theme.base().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            rule.repeat(inner.width as usize),
            theme.faint(),
        )),
    ];
    f.render_widget(Paragraph::new(header_lines), body[0]);

    let view = body[1];
    let total = s.lines.len() as u16;
    let max_scroll = total.saturating_sub(view.height);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }

    f.render_widget(
        Paragraph::new(s.lines.clone())
            .scroll((s.scroll, 0))
            .wrap(Wrap { trim: false }),
        view,
    );
}

// ================= new conversation =================

pub fn new_conversation_key(s: &mut super::NewConversationState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
    if key.code == KeyCode::Esc {
        return Action::PopScreen;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('v') => return Action::PasteClipboard,
            KeyCode::Char('a') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::move_home(text, cursor);
                return Action::None;
            }
            KeyCode::Char('e') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::move_end(text, cursor);
                return Action::None;
            }
            KeyCode::Char('w') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::delete_word_back(text, cursor);
                return Action::None;
            }
            KeyCode::Char('u') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::kill_to_start(text, cursor);
                return Action::None;
            }
            KeyCode::Char('k') => {
                let (text, cursor) = match s.field {
                    0 => (&mut s.recipients, &mut s.recipients_cursor),
                    1 => (&mut s.title, &mut s.title_cursor),
                    _ => (&mut s.body, &mut s.body_cursor),
                };
                crate::editor::kill_to_end(text, cursor);
                return Action::None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => {
            s.field = (s.field + 1) % 3;
            Action::None
        }
        KeyCode::Backspace => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::delete_back(text, cursor);
            Action::None
        }
        KeyCode::Delete => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::delete_forward(text, cursor);
            Action::None
        }
        KeyCode::Left => {
            let cursor = match s.field {
                0 => &mut s.recipients_cursor,
                1 => &mut s.title_cursor,
                _ => &mut s.body_cursor,
            };
            crate::editor::move_left(cursor);
            Action::None
        }
        KeyCode::Right => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_right(text, cursor);
            Action::None
        }
        KeyCode::Home => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_home(text, cursor);
            Action::None
        }
        KeyCode::End => {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::move_end(text, cursor);
            Action::None
        }
        KeyCode::Enter => {
            if s.field == 2 {
                s.submit()
            } else {
                s.field += 1;
                Action::None
            }
        }
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let (text, cursor) = match s.field {
                0 => (&mut s.recipients, &mut s.recipients_cursor),
                1 => (&mut s.title, &mut s.title_cursor),
                _ => (&mut s.body, &mut s.body_cursor),
            };
            crate::editor::insert_char(text, cursor, c);
            Action::None
        }
        _ => Action::None,
    }
}

impl NewConversationState {
    fn submit(&mut self) -> Action {
        if self.busy {
            return Action::None;
        }
        let names: Vec<String> = self
            .recipients
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if names.is_empty() || self.title.trim().is_empty() || self.body.trim().is_empty() {
            self.errors.push("Fill in recipients, title and message.".into());
            return Action::None;
        }
        self.busy = true;
        self.resolved_ids.clear();
        self.errors.clear();
        self.resolving = names.len();
        Action::ResolveRecipients(names, self.title.clone(), self.body.clone())
    }
}

pub fn new_conversation_hints() -> Hints {
    Hints::new(
        &[
            ("Enter", "next/send"),
            ("Tab", "next field"),
            ("Esc", "cancel"),
        ],
        0,
    )
}

pub fn render_new_conversation(
    s: &mut super::NewConversationState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
    g: &Glyphs,
) {
    let block = super::solo_panel(theme, g, "New conversation", None, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let text = vec![
        Line::from(vec![
            Span::styled("To (usernames, comma-separated): ", theme.dim()),
            Span::styled(s.recipients.clone(), theme.base()),
        ]),
        Line::from(vec![
            Span::styled("Title: ", theme.dim()),
            Span::styled(s.title.clone(), theme.base()),
        ]),
        Line::from(Span::styled("Message (Tab to next field):", theme.dim())),
        Line::from(Span::styled(s.body.clone(), theme.base())),
    ];
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }),
        inner,
    );

    let cur_pos = match s.field {
        0 => {
            let col = s.recipients.chars().take(s.recipients_cursor).count() as u16;
            let prefix_len = 32u16;
            Some((
                (inner.x + prefix_len + col).min(inner.x + inner.width.saturating_sub(1)),
                inner.y,
            ))
        }
        1 => {
            let col = s.title.chars().take(s.title_cursor).count() as u16;
            let prefix_len = 7u16;
            Some((
                (inner.x + prefix_len + col).min(inner.x + inner.width.saturating_sub(1)),
                inner.y + 1,
            ))
        }
        2 => {
            let (col, row) = crate::editor::cursor_coords(&s.body, s.body_cursor);
            Some((
                (inner.x + col).min(inner.x + inner.width.saturating_sub(1)),
                (inner.y + 3 + row).min(inner.y + inner.height.saturating_sub(2)),
            ))
        }
        _ => None,
    };
    if let Some((x, y)) = cur_pos {
        f.set_cursor_position((x, y));
    }

    let mut status_lines: Vec<Line> = Vec::new();
    for e in &s.errors {
        status_lines.push(Line::from(Span::styled(e.clone(), Style::new().fg(theme.error))));
    }
    if s.busy {
        status_lines.push(Line::from(Span::styled("Resolving recipients…", theme.dim())));
    }
    if !status_lines.is_empty() {
        let status_area = ratatui::layout::Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(status_lines.len() as u16),
            inner.width,
            status_lines.len() as u16,
        );
        f.render_widget(Paragraph::new(status_lines), status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::models::{Alert, Conversation, ConversationMessage, ConversationRecipient};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn sample_conversation() -> Conversation {
        Conversation {
            conversation_id: 3,
            title: "Terminal client beta — first impressions".into(),
            start_user_id: 1,
            start_username: "Mike".into(),
            reply_count: 6,
            last_message_date: 1_700_000_000,
            conversation_unread: true,
            recipients: vec![ConversationRecipient {
                user_id: 2,
                username: "kemical".into(),
            }],
            ..Default::default()
        }
    }

    fn sample_inbox_state() -> InboxState {
        let conv = sample_conversation();
        let messages = vec![
            ConversationMessage {
                message_id: 1,
                conversation_id: 3,
                user_id: 2,
                username: "kemical".into(),
                message: "Ran it over SSH from the Windows Terminal preview.".into(),
                message_date: 1_700_000_000,
            },
            ConversationMessage {
                message_id: 2,
                conversation_id: 3,
                user_id: 1,
                username: "Mike".into(),
                message: "Kitty too?".into(),
                message_date: 1_700_003_600,
            },
        ];
        InboxState {
            convos: ConversationsState {
                conversations: vec![conv.clone(); 5],
                page: 1,
                last_page: 4,
                total: 18,
                ..Default::default()
            },
            view: Some(ConversationViewState {
                conversation: conv,
                messages,
                page: 1,
                last_page: 1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn render_rows_inbox(s: &mut InboxState, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::truecolor();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_inbox(s, f, area, &theme, &crate::glyph::UNICODE);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// `q` in the inline view pane is one level out, not quit — the same step
    /// the pushed `ConversationView` takes below the dual-layout threshold.
    #[test]
    fn q_leaves_the_inline_conversation_pane_like_h_and_left() {
        for k in [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        ] {
            let mut s = sample_inbox_state();
            s.focus = InboxPane::View;
            let action = inbox_key(&mut s, k);
            assert!(matches!(action, Action::None), "{:?} should not act", k.code);
            assert_eq!(s.focus, InboxPane::List, "{:?} should return focus", k.code);
        }
        // ...and from the list `q` still leaves the Inbox entirely.
        let mut s = sample_inbox_state();
        s.focus = InboxPane::List;
        assert!(matches!(inbox_key(&mut s, key('q')), Action::PopScreen));
    }

    /// `r` is reply, so refresh is `R` — and it must reach both lists rather
    /// than being swallowed by the Inbox's own `r` interception.
    #[test]
    fn shift_r_refreshes_whichever_inbox_list_is_showing() {
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);

        let mut s = sample_inbox_state();
        s.convos.page = 3;
        assert!(matches!(
            inbox_key(&mut s, shift_r),
            Action::LoadConversations(3)
        ));
        assert!(s.convos.loading);

        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        assert!(matches!(inbox_key(&mut s, shift_r), Action::LoadAlerts));
        assert!(s.alerts.loading);

        // F5 is the same key for terminals that send it.
        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        assert!(matches!(inbox_key(&mut s, f5), Action::LoadAlerts));

        // Lowercase `r` still means reply, not refresh.
        let mut s = sample_inbox_state();
        assert!(matches!(
            inbox_key(&mut s, key('r')),
            Action::StartReplyConversation(_)
        ));

        // The key bar advertises it.
        assert!(inbox_hints().keys.iter().any(|(k, _)| *k == "R"));
    }

    #[test]
    fn inbox_dual_layout_shows_tabs_and_both_tabs_bottom_titles() {
        let mut s = sample_inbox_state();
        let rows = render_rows_inbox(&mut s, 120, 36);
        assert!(s.dual, "120 cols should be dual");
        let text = rows.join("\n");
        assert!(text.contains("Conversations"), "{text}");
        assert!(text.contains("Alerts"), "{text}");
        assert!(text.contains("of 18"), "conversations footer missing: {text}");
        assert!(text.contains("r reply"), "view panel right segment: {text}");
        assert!(text.contains("kemical"), "message card author: {text}");

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 1,
            username: "kemical".into(),
            content_type: "post_quote".into(),
            viewed: false,
            ..Default::default()
        }];
        let rows = render_rows_inbox(&mut s, 120, 36);
        let text = rows.join("\n");
        assert!(text.contains("unread"), "alerts footer missing: {text}");
    }

    #[test]
    fn inbox_renders_one_panel_when_narrow() {
        let mut s = sample_inbox_state();
        let rows = render_rows_inbox(&mut s, 80, 24);
        assert!(!s.dual, "80 cols should not be dual");
        let text = rows.join("\n");
        assert!(text.contains("Conversations"), "{text}");
        // No inline view pane at this width, so its right segment is absent.
        assert!(!text.contains("r reply"), "{text}");
    }

    #[test]
    fn tab_key_switches_tabs_from_the_list_and_returns_from_the_view() {
        let mut s = InboxState::default();
        assert_eq!(s.tab, InboxTab::Conversations);
        inbox_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.tab, InboxTab::Alerts);
        inbox_key(&mut s, KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(s.tab, InboxTab::Conversations);

        s.view = Some(ConversationViewState::default());
        s.focus = InboxPane::View;
        inbox_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(s.focus, InboxPane::List, "Tab from the view returns to the list");
        assert_eq!(s.tab, InboxTab::Conversations, "Tab from the view must not also flip tabs");

        s.focus = InboxPane::View;
        inbox_key(&mut s, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(s.focus, InboxPane::List, "h also returns focus from the view");
    }

    #[test]
    fn enter_opens_a_conversation_and_marks_an_alert_read_by_tab() {
        let mut s = sample_inbox_state();
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::OpenConversation(c) if c.conversation_id == 3));

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 9,
            username: "kemical".into(),
            content_type: "post_quote".into(),
            ..Default::default()
        }];
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkAlertRead(id) if id == 9));
    }

    #[test]
    fn n_and_r_work_from_the_list_regardless_of_active_tab() {
        let mut s = sample_inbox_state();
        s.tab = InboxTab::Alerts;
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartNewConversation(None)));

        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(act, Action::StartReplyConversation(c) if c.conversation_id == 3));
    }

    #[test]
    fn m_marks_read_on_whichever_tab_is_active() {
        let mut s = sample_inbox_state();
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkConversationRead(id) if id == 3));

        s.tab = InboxTab::Alerts;
        s.alerts.alerts = vec![Alert {
            alert_id: 9,
            username: "kemical".into(),
            content_type: "post_quote".into(),
            ..Default::default()
        }];
        let act = inbox_key(&mut s, KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(matches!(act, Action::MarkAlertRead(id) if id == 9));
    }

    #[test]
    fn conversation_participants_and_view_actions() {
        let conv = Conversation {
            conversation_id: 55,
            title: "Security Review".into(),
            start_user_id: 1,
            start_username: "Alice".into(),
            recipients: vec![
                ConversationRecipient {
                    user_id: 2,
                    username: "Bob".into(),
                },
                ConversationRecipient {
                    user_id: 3,
                    username: "Charlie".into(),
                },
            ],
            ..Default::default()
        };

        let messages = vec![
            ConversationMessage {
                message_id: 501,
                conversation_id: 55,
                user_id: 1,
                username: "Alice".into(),
                message: "Shall we audit the firewall?".into(),
                ..Default::default()
            },
            ConversationMessage {
                message_id: 502,
                conversation_id: 55,
                user_id: 4,
                username: "Dave_Auditor".into(),
                message: "I can help review the logs.".into(),
                ..Default::default()
            },
        ];

        let mut state = ConversationViewState {
            conversation: conv,
            messages,
            page: 1,
            last_page: 1,
            ..Default::default()
        };

        // Check that all participants include starter, recipients, and new message senders
        let all = state.all_participants();
        assert_eq!(all, vec!["Alice", "Bob", "Charlie", "Dave_Auditor"]);
        assert_eq!(
            state.participants_display(),
            "Alice (starter), Bob, Charlie, Dave_Auditor"
        );

        let theme = Theme::dark();
        state.rebuild_lines(&theme, &crate::glyph::UNICODE);

        // Verify the rendered lines contain CONVERSATION PARTICIPANTS
        let text: String = state
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("CONVERSATION PARTICIPANTS"));
        assert!(text.contains("Alice (starter), Bob, Charlie, Dave_Auditor"));

        // 'p': open profile of active message author (msg 0: Alice)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 1 && name == "Alice"));

        // 'n': advance to next message (msg 1: Dave_Auditor)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::None));
        assert_eq!(state.sel_msg, 1);

        // 'p': open profile of Dave_Auditor
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 4 && name == "Dave_Auditor"));

        // 'P': open starter profile (Alice)
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        );
        assert!(matches!(act, Action::OpenProfile(uid, name) if uid == 1 && name == "Alice"));

        // 'r': start reply to conversation
        let act = conversation_view_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );
        assert!(matches!(act, Action::StartReplyConversation(c) if c.conversation_id == 55));
    }
}
