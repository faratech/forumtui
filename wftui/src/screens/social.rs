//! Social screens: conversations (DMs), conversation view, new conversation,
//! alerts.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};


use super::{browse::push_bbcode, Action, ConversationViewState, NewConversationState};
use crate::app::footer_line;
use crate::theme::{fmt_time, Theme};

// ================= conversations list =================

pub fn conversations_key(s: &mut super::ConversationsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.conversations.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Char('[') | KeyCode::PageUp => {
            if s.page > 1 {
                s.loading = true;
                Action::LoadConversations(s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadConversations(s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.conversations.get(s.sel) {
            Some(c) => Action::OpenConversation(c.clone()),
            None => Action::None,
        },
        KeyCode::Char('n') => Action::StartNewConversation(None),
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadConversations(s.page)
        }
        _ => Action::None,
    }
}

pub fn render_conversations(
    s: &mut super::ConversationsState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let title = format!(
        " Conversations — page {}/{} ",
        s.page, s.last_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(title, theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.conversations.is_empty() {
        f.render_widget(Paragraph::new("Loading conversations…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .conversations
        .iter()
        .map(|c| {
            let unread = c.is_unread_conv();
            let marker = if unread { "● " } else { "  " };
            let style = if unread {
                theme.base().add_modifier(Modifier::BOLD)
            } else {
                theme.base()
            };
            let mut line1 = vec![
                Span::styled(marker, theme.accent),
                Span::styled(c.title.clone(), style),
            ];
            if c.is_starred_conv() {
                line1.push(Span::styled(" ★", theme.warn));
            }
            line1.push(Span::styled(
                format!("  ({} replies)", c.reply_count),
                theme.dim(),
            ));

            let participants = c.participants_display();
            let mut line2 = vec![
                Span::raw("    "),
                Span::styled("👥 ", theme.accent),
                Span::styled("Participants: ", theme.dim()),
                Span::styled(participants, theme.base()),
            ];
            if !c.last_message_username.is_empty() {
                line2.push(Span::styled(
                    format!(
                        " · last by {} ({})",
                        c.last_message_username,
                        fmt_time(c.last_message_date)
                    ),
                    theme.dim(),
                ));
            }
            ListItem::new(vec![Line::from(line1), Line::from(line2)])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );

    let hints = footer_line(
        theme,
        &[("Enter", "read"), ("[/]", "page"), ("n", "new DM"), ("r", "refresh")],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= conversation view =================

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

    pub fn rebuild_lines(&mut self, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut msg_offsets: Vec<u16> = Vec::new();

        // Prominent participants callout header at the top
        let starter = self.conversation.starter();
        let participants_str = self.participants_display();
        lines.push(Line::from(vec![
            Span::styled("┌── 👥 CONVERSATION PARTICIPANTS ──", theme.dim()),
            Span::styled("──────────────────────────────────────────", theme.dim()),
        ]));
        if !starter.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("│ ", theme.dim()),
                Span::styled("Started by: ", theme.dim()),
                Span::styled(starter.to_string(), theme.title().add_modifier(Modifier::BOLD)),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("│ ", theme.dim()),
            Span::styled("All Participants: ", theme.dim()),
            Span::styled(participants_str, theme.base().add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(Span::styled(
            "└──────────────────────────────────────────────────────────────────────────",
            theme.dim(),
        )));
        lines.push(Line::from(Span::raw("")));

        for (i, msg) in self.messages.iter().enumerate() {
            let offset = lines.len() as u16;
            msg_offsets.push(offset);
            let num = i + 1 + ((self.page.saturating_sub(1)) as usize * 20);
            lines.push(Line::from(vec![
                Span::styled("■ ", theme.accent),
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
}

pub fn conversation_view_key(s: &mut ConversationViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
            Action::PopScreen
        }
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

pub fn render_conversation_view(
    s: &mut ConversationViewState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let parts_summary = s.all_participants().join(", ");
    let title = format!(
        " ✉ {} [👥 {}] — {}/{} ",
        super::browse::truncate(&s.conversation.title, 32),
        super::browse::truncate(&parts_summary, 28),
        s.page,
        s.last_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(title, theme.title()));
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
        Constraint::Length(1), // Hints footer
    ])
    .split(inner);

    // Persistent participant header bar
    let header_lines = vec![
        Line::from(vec![
            Span::styled("👥 Participants: ", theme.accent),
            Span::styled(
                s.participants_display(),
                theme.base().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            theme.dim(),
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
    let hints = footer_line(
        theme,
        &[
            ("↑↓/PgUp/PgDn", "scroll"),
            ("n/N", "next/prev msg"),
            ("r", "reply"),
            ("p", "author profile"),
            ("P", "starter profile"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
    );
    f.render_widget(Paragraph::new(hints), body[2]);
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

pub fn render_new_conversation(
    s: &mut super::NewConversationState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(" New conversation ", theme.title()));
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

    let hints = footer_line(theme, &[("Enter", "next/send"), ("Tab", "next field")]);
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= alerts =================

pub fn alerts_key(s: &mut super::AlertsState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.alerts.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Enter | KeyCode::Char(' ') => match s.alerts.get(s.sel) {
            Some(a) => Action::MarkAlertRead(a.alert_id),
            None => Action::None,
        },
        KeyCode::Char('o') => match s.alerts.get(s.sel).and_then(|a| a.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::None,
        },
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadAlerts
        }
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => Action::PopScreen,
        _ => Action::None,
    }
}

pub fn render_alerts(
    s: &mut super::AlertsState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(" Alerts ", theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.alerts.is_empty() {
        f.render_widget(Paragraph::new("Loading alerts…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .alerts
        .iter()
        .map(|a| {
            let (marker, style) = if a.viewed {
                ("  ", theme.dim())
            } else {
                ("● ", theme.base().add_modifier(Modifier::BOLD))
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, theme.accent),
                Span::styled(a.username.clone(), style),
                Span::styled(format!("  {} ", a.content_type.replace('_', " ")), theme.dim()),
                Span::styled(fmt_time(a.alert_date), theme.dim()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()),
        inner,
        &mut state,
    );

    let hints = footer_line(
        theme,
        &[("Enter/Space", "mark read"), ("o", "open"), ("r", "refresh")],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::models::{Conversation, ConversationMessage, ConversationRecipient};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        state.rebuild_lines(&theme);

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
