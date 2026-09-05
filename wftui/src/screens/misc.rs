//! Login, compose, search, profile screens.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use common::models::SearchHit;

use super::{browse::truncate, Action, ComposeTarget, LoginStage};
use crate::app::footer_line;
use crate::theme::Theme;

// ================= login =================

pub fn login_key(s: &mut super::LoginState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
    match (&s.stage, key.code) {
        (LoginStage::Idle, KeyCode::Enter) => Action::LoginBegin,
        (LoginStage::Waiting, KeyCode::Char('c')) => Action::OscCopy(s.url.clone()),
        (LoginStage::Waiting, KeyCode::Char('o')) => Action::OpenUrl(s.url.clone()),
        _ => Action::None,
    }
}

pub fn render_login(
    s: &mut super::LoginState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent)
        .title(Span::styled(" Log in to WindowsForum ", theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    match &s.stage {
        LoginStage::Idle => {
            lines.push(Line::from(vec![
                Span::styled(
                    " Enter ",
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" log in with your browser"),
            ]));
        }
        LoginStage::Waiting => {
            lines.push(Line::from(Span::raw(
                "1. Open this short link (browser, phone — anything):",
            )));
            // Plain text, never OSC-embedded: escape sequences inside ratatui
            // spans get re-emitted per-cell on diff and corrupt the screen.
            // Drag-select copies it; c copies it without selecting.
            lines.push(Line::from(Span::styled(
                s.url.clone(),
                Style::new()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "   also in your clipboard and at {}/login-url.txt",
                    std::path::Path::new(&common::config::token_path())
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                theme.dim(),
            )));
            lines.push(Line::from(Span::raw("")));
            lines.push(Line::from(Span::raw(
                "2. Approve access — this window finishes on its own.",
            )));
            lines.push(Line::from(Span::raw("")));
        }
    }
    if s.busy {
        lines.push(Line::from(Span::styled("Working…", theme.dim)));
    }
    if let Some(err) = &s.error {
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::new().fg(theme.error),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled(" c ", Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw("copy link   "),
        Span::styled(" o ", Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw("open here   "),
        Span::styled(" Enter ", Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw("restart login"),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ================= compose =================

pub fn compose_key(s: &mut super::ComposeState, key: KeyEvent) -> Action {
    if s.busy {
        return Action::None;
    }
    let target = match &s.target {
        Some(t) => t.clone(),
        None => return Action::None,
    };
    let is_new_thread = matches!(target, ComposeTarget::NewThread { .. });
    if !is_new_thread {
        s.title_field = false;
    }

    if key.code == KeyCode::Esc {
        return Action::PopScreen;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                match target {
                    ComposeTarget::ThreadReply { thread_id, .. } => {
                        if s.body.trim().is_empty() {
                            s.error = Some("Reply is empty.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitReply {
                            thread_id,
                            message: s.body.clone(),
                        };
                    }
                    ComposeTarget::NewThread { node_id } => {
                        if s.title.trim().is_empty() || s.body.trim().is_empty() {
                            s.error = Some("Title and message are required.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitThread {
                            node_id,
                            title: s.title.clone(),
                            message: s.body.clone(),
                        };
                    }
                    ComposeTarget::ConversationReply {
                        conversation_id, ..
                    } => {
                        if s.body.trim().is_empty() {
                            s.error = Some("Message is empty.".into());
                            return Action::None;
                        }
                        s.busy = true;
                        return Action::SubmitConvoReply {
                            id: conversation_id,
                            message: s.body.clone(),
                        };
                    }
                }
            }
            KeyCode::Char('y') | KeyCode::Char('v') => {
                return Action::PasteClipboard;
            }
            _ => {}
        }
    }

    if s.title_field && is_new_thread {
        match key.code {
            KeyCode::Tab | KeyCode::Enter => {
                s.title_field = false;
                s.body_cursor = s.body.chars().count();
                Action::None
            }
            KeyCode::Backspace => {
                crate::editor::delete_back(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Left => {
                crate::editor::move_left(&mut s.title_cursor);
                Action::None
            }
            KeyCode::Right => {
                crate::editor::move_right(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Home => {
                crate::editor::move_home(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::End => {
                crate::editor::move_end(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_home(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_end(&s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::delete_word_back(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_start(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_end(&mut s.title, &mut s.title_cursor);
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                crate::editor::insert_char(&mut s.title, &mut s.title_cursor, c);
                Action::None
            }
            _ => Action::None,
        }
    } else {
        match key.code {
            KeyCode::Tab => {
                if is_new_thread {
                    s.title_field = true;
                    s.title_cursor = s.title.chars().count();
                } else {
                    crate::editor::insert_str(&mut s.body, &mut s.body_cursor, "    ");
                }
                Action::None
            }
            KeyCode::Backspace => {
                crate::editor::delete_back(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Left => {
                crate::editor::move_left(&mut s.body_cursor);
                Action::None
            }
            KeyCode::Right => {
                crate::editor::move_right(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Home => {
                crate::editor::move_home(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::End => {
                crate::editor::move_end(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_home(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::move_end(&s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::delete_word_back(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_start(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                crate::editor::kill_to_end(&mut s.body, &mut s.body_cursor);
                Action::None
            }
            KeyCode::Enter => {
                crate::editor::insert_char(&mut s.body, &mut s.body_cursor, '\n');
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                crate::editor::insert_char(&mut s.body, &mut s.body_cursor, c);
                Action::None
            }
            _ => Action::None,
        }
    }
}

pub fn render_compose(
    s: &mut super::ComposeState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let heading = s
        .target
        .as_ref()
        .map(|t| t.heading())
        .unwrap_or_else(|| "Compose".into());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent)
        .title(Span::styled(format!(" {heading} "), theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let is_new_thread = matches!(s.target, Some(ComposeTarget::NewThread { .. }));
    let mut lines: Vec<Line<'static>> = Vec::new();
    if is_new_thread {
        lines.push(Line::from(vec![
            Span::styled("Title: ", theme.dim()),
            Span::styled(s.title.clone(), theme.base()),
        ]));
        lines.push(Line::from(Span::styled(
            "Message (Tab = switch field):",
            theme.dim(),
        )));
    }
    for line in s.body.split('\n') {
        lines.push(Line::from(Span::styled(line.to_string(), theme.base())));
    }
    if let Some(err) = &s.error {
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::new().fg(theme.error),
        )));
    }
    if s.busy {
        lines.push(Line::from(Span::styled("Sending…", theme.dim)));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    if is_new_thread && s.title_field {
        let cur_col = s.title.chars().take(s.title_cursor).count() as u16;
        let cur_x = (inner.x + 7 + cur_col).min(inner.x + inner.width.saturating_sub(1));
        let cur_y = inner.y;
        f.set_cursor_position((cur_x, cur_y));
    } else {
        let (b_col, b_row) = crate::editor::cursor_coords(&s.body, s.body_cursor);
        let base_y = if is_new_thread { inner.y + 2 } else { inner.y };
        let cur_x = (inner.x + b_col).min(inner.x + inner.width.saturating_sub(1));
        let cur_y = (base_y + b_row).min(inner.y + inner.height.saturating_sub(2));
        f.set_cursor_position((cur_x, cur_y));
    }

    let mut hints = footer_line(
        theme,
        &[
            ("Enter", "newline"),
            ("Tab", "field"),
            ("Ctrl+S", "send"),
            ("Esc", "cancel"),
        ],
    )
    .spans;
    hints.push(Span::styled(
        "  (XF allows one post/30s, one thread/180s — mirrored client-side)",
        theme.dim(),
    ));
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(Line::from(hints)), hint_area);
}

// ================= search =================

pub fn search_key(s: &mut super::SearchState, key: KeyEvent) -> Action {
    if s.input_mode {
        if key.code == KeyCode::Esc {
            return Action::PopScreen;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('v') => return Action::PasteClipboard,
                KeyCode::Char('a') => {
                    crate::editor::move_home(&s.query, &mut s.cursor);
                    return Action::None;
                }
                KeyCode::Char('e') => {
                    crate::editor::move_end(&s.query, &mut s.cursor);
                    return Action::None;
                }
                KeyCode::Char('w') => {
                    crate::editor::delete_word_back(&mut s.query, &mut s.cursor);
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    crate::editor::kill_to_start(&mut s.query, &mut s.cursor);
                    return Action::None;
                }
                KeyCode::Char('k') => {
                    crate::editor::kill_to_end(&mut s.query, &mut s.cursor);
                    return Action::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => {
                let q = s.query.trim().to_string();
                if q.is_empty() {
                    return Action::None;
                }
                s.input_mode = false;
                s.loading = true;
                Action::RunSearch(q, 1)
            }
            KeyCode::Backspace => {
                crate::editor::delete_back(&mut s.query, &mut s.cursor);
                Action::None
            }
            KeyCode::Delete => {
                crate::editor::delete_forward(&mut s.query, &mut s.cursor);
                Action::None
            }
            KeyCode::Left => {
                crate::editor::move_left(&mut s.cursor);
                Action::None
            }
            KeyCode::Right => {
                crate::editor::move_right(&s.query, &mut s.cursor);
                Action::None
            }
            KeyCode::Home => {
                crate::editor::move_home(&s.query, &mut s.cursor);
                Action::None
            }
            KeyCode::End => {
                crate::editor::move_end(&s.query, &mut s.cursor);
                Action::None
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                crate::editor::insert_char(&mut s.query, &mut s.cursor, c);
                Action::None
            }
            _ => Action::None,
        }
    } else {
        match key.code {
            KeyCode::Esc => Action::PopScreen,
            KeyCode::Char('i') => {
                s.input_mode = true;
                s.cursor = s.query.chars().count();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if s.sel > 0 {
                    s.sel -= 1;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if s.sel + 1 < s.results.len() {
                    s.sel += 1;
                }
                Action::None
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                if s.page > 1 {
                    s.loading = true;
                    Action::RunSearch(s.query.trim().to_string(), s.page - 1)
                } else {
                    Action::None
                }
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                if s.page < s.last_page {
                    s.loading = true;
                    Action::RunSearch(s.query.trim().to_string(), s.page + 1)
                } else {
                    Action::None
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => match s.results.get(s.sel) {
                Some(hit) if hit.content_type == "thread" => {
                    Action::OpenThread(common::models::Thread {
                        thread_id: hit.content_id as u32,
                        title: hit.title.clone(),
                        view_url: hit.view_url.clone(),
                        ..Default::default()
                    })
                }
                Some(hit) => match &hit.view_url {
                    Some(url) => Action::OpenUrl(url.clone()),
                    None => Action::None,
                },
                None => Action::None,
            },
            _ => Action::None,
        }
    }
}

pub fn render_search(
    s: &mut super::SearchState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(" Search ", theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let body = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Query: ", theme.dim()),
            Span::styled(s.query.clone(), theme.base()),
        ])),
        body[0],
    );

    if s.input_mode {
        let cur_col = s.query.chars().take(s.cursor).count() as u16;
        let cur_x = (body[0].x + 7 + cur_col).min(body[0].x + body[0].width.saturating_sub(1));
        let cur_y = body[0].y;
        f.set_cursor_position((cur_x, cur_y));
    }

    if s.loading {
        f.render_widget(Paragraph::new("Searching…"), body[1]);
    } else if s.results.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "No results (press i to type a query).".to_string(),
                theme.dim(),
            )),
            body[1],
        );
    } else {
        let items: Vec<ListItem> = s
            .results
            .iter()
            .map(|hit: &SearchHit| {
                ListItem::new(Line::from(vec![
                    Span::styled(truncate(&hit.title, 70), theme.base()),
                    Span::styled(
                        format!("  {} · {}", hit.content_type, hit.username),
                        theme.dim(),
                    ),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(s.sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(theme.selected()),
            body[1],
            &mut state,
        );
    }
    let hints = footer_line(
        theme,
        &[
            ("i", "edit query"),
            ("Enter", "open"),
            ("[/]", "page"),
            ("Esc", "back"),
        ],
    );
    f.render_widget(Paragraph::new(hints), body[2]);
}

// ================= profile =================

pub fn profile_key(s: &mut super::ProfileState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::PopScreen,
        KeyCode::Char('o') => match s.user.as_ref().and_then(|u| u.view_url.clone()) {
            Some(url) => Action::OpenUrl(url),
            None => Action::None,
        },
        _ => Action::None,
    }
}

pub fn render_profile(
    s: &mut super::ProfileState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(format!(" Member: {} ", s.title), theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading {
        f.render_widget(Paragraph::new("Loading profile…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}")).style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let Some(user) = &s.user else {
        return;
    };
    let lines = vec![
        Line::from(Span::styled(
            user.username.clone(),
            theme.base().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(user.custom_title.clone(), theme.dim())),
        Line::from(Span::raw("")),
        Line::from(Span::raw(format!("Messages: {}", user.message_count))),
        Line::from(Span::styled(
            format!("Registered {}", crate::theme::fmt_time(user.register_date)),
            theme.dim(),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);

    let hints = footer_line(theme, &[("o", "open on the web"), ("Esc", "back")]);
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
    use crate::screens::ComposeState;

    #[test]
    fn new_thread_enter_in_title_advances_to_body() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::NewThread { node_id: 4 }),
            title_field: true,
            title: "Test Title".into(),
            body: String::new(),
            ..Default::default()
        };
        compose_key(&mut s, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!s.title_field, "Enter in title field should advance to body");
        assert!(s.body.is_empty(), "Enter in title should not push newline to body");
    }

    #[test]
    fn reply_tab_does_not_divert_to_title() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            title_field: false,
            title: String::new(),
            body: String::new(),
            ..Default::default()
        };
        compose_key(&mut s, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!s.title_field, "Reply mode should never focus title");
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(s.body, "    a");
        assert!(s.title.is_empty());
    }

    #[test]
    fn compose_cursor_navigation_and_word_deletion() {
        let mut s = ComposeState {
            target: Some(ComposeTarget::ThreadReply {
                thread_id: 1,
                thread_title: "Thread".into(),
            }),
            title_field: false,
            title: String::new(),
            body: "hello world".into(),
            body_cursor: 11,
            ..Default::default()
        };

        // Delete word back: removes "world"
        compose_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.body, "hello ");
        assert_eq!(s.body_cursor, 6);

        // Move left 2 chars
        compose_key(&mut s, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        compose_key(&mut s, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 4);

        // Insert character inside
        compose_key(&mut s, KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert_eq!(s.body, "hellXo ");
        assert_eq!(s.body_cursor, 5);

        // Home key
        compose_key(&mut s, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 0);

        // End key
        compose_key(&mut s, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(s.body_cursor, 7);
    }

    #[test]
    fn search_input_editing_and_kill_lines() {
        let mut s = crate::screens::SearchState {
            query: "rust ratatui crossterm".into(),
            cursor: 22,
            input_mode: true,
            ..Default::default()
        };

        // Ctrl+W: delete word back
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.query, "rust ratatui ");
        assert_eq!(s.cursor, 13);

        // Move home
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.cursor, 0);

        // Move right 4
        for _ in 0..4 {
            search_key(&mut s, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(s.cursor, 4);

        // Ctrl+K: kill to end
        search_key(
            &mut s,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(s.query, "rust");
        assert_eq!(s.cursor, 4);
    }
}
