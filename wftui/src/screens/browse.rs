//! Browsing screens: forum tree, thread list, thread view.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use common::bbcode::{self, Chunk};
use common::models::Node;

use super::{link_style, style_from, Action, ThreadListState, ThreadViewState};
use crate::app::footer_line;
use crate::theme::{fmt_time, Theme};

const NEWS_NODE: u32 = 4;
const SECURITY_NODE: u32 = 84;
const TUTORIALS_NODE: u32 = 305;

// ================= forum tree =================

pub fn forum_tree_key(s: &mut super::ForumTreeState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.nodes.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.nodes.get(s.sel) {
            Some(node) => Action::OpenThreadList(node.node_id, node.title.clone()),
            None => Action::None,
        },
        KeyCode::Char('1') => open_node_action(&s.nodes, NEWS_NODE, "Windows News"),
        KeyCode::Char('2') => open_node_action(&s.nodes, SECURITY_NODE, "Security Alerts"),
        KeyCode::Char('3') => open_node_action(&s.nodes, TUTORIALS_NODE, "Windows Tutorials"),
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadNodes
        }
        KeyCode::Char('N') => match s.nodes.get(s.sel) {
            Some(node) => Action::StartNewThread(node.node_id),
            None => Action::None,
        },
        KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

fn open_node_action(nodes: &[Node], id: u32, fallback_title: &str) -> Action {
    if let Some(node) = nodes.iter().find(|n| n.node_id == id) {
        Action::OpenThreadList(node.node_id, node.title.clone())
    } else {
        Action::OpenThreadList(id, fallback_title.to_string())
    }
}

pub fn render_forum_tree(
    s: &mut super::ForumTreeState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(" Forums ", theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading {
        f.render_widget(Paragraph::new("Loading forums…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress r to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .nodes
        .iter()
        .map(|n| {
            let indent = "  ".repeat(n.depth.min(6) as usize);
            ListItem::new(Line::from(vec![
                Span::styled(indent, theme.dim()),
                Span::raw(n.title.clone()),
                Span::styled(
                    format!("  ({} discussions)", n.node_id),
                    theme.dim(),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.sel));
    f.render_stateful_widget(
        List::new(items).highlight_style(theme.selected()).block(Block::default()),
        inner,
        &mut state,
    );

    let hints = footer_line(
        theme,
        &[
            ("↑↓", "move"),
            ("Enter", "open"),
            ("1/2/3", "news/security/tutorials"),
            ("N", "new thread"),
            ("r", "refresh"),
            ("c", "DMs"),
            ("a", "alerts"),
            ("s", "search"),
            ("q", "quit"),
        ],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= thread list =================

pub fn thread_list_key(s: &mut ThreadListState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sel > 0 {
                s.sel -= 1;
            }
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel + 1 < s.threads.len() {
                s.sel += 1;
            }
            Action::None
        }
        KeyCode::Char('[') | KeyCode::PageUp => {
            if s.page > 1 {
                s.loading = true;
                Action::LoadForum(s.node_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadForum(s.node_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Enter | KeyCode::Char('l') => match s.threads.get(s.sel) {
            Some(t) => Action::OpenThread(t.clone()),
            None => Action::None,
        },
        KeyCode::Char('m') => Action::MarkForumRead(s.node_id),
        KeyCode::Char('N') => Action::StartNewThread(s.node_id),
        KeyCode::Char('r') => {
            s.loading = true;
            Action::LoadForum(s.node_id, s.page)
        }
        _ => Action::None,
    }
}

pub fn render_thread_list(
    s: &mut ThreadListState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let title = format!(
        " {} — page {}/{} ",
        s.title,
        s.page,
        s.last_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim())
        .title(Span::styled(title, theme.title()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if s.loading && s.threads.is_empty() {
        f.render_widget(Paragraph::new("Loading threads…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\nPress r to retry."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = s
        .threads
        .iter()
        .map(|t| {
            let mut spans = vec![];
            if t.sticky {
                spans.push(Span::styled("📌 ", theme.warn));
            }
            spans.push(Span::styled(t.title.clone(), theme.base()));
            spans.push(Span::styled(
                format!(
                    "  {} by {} · {} replies · last {}",
                    fmt_time(t.post_date),
                    t.username,
                    t.reply_count,
                    t.last_post_username
                ),
                theme.dim(),
            ));
            ListItem::new(Line::from(spans))
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
        &[
            ("Enter", "read"),
            ("[/]", "page"),
            ("m", "mark read"),
            ("N", "new thread"),
            ("r", "refresh"),
        ],
    );
    let hint_area = ratatui::layout::Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(hints), hint_area);
}

// ================= thread view =================

impl ThreadViewState {
    pub fn rebuild_lines(&mut self, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut links: Vec<String> = Vec::new();
        if let Some(url) = &self.thread.view_url {
            links.push(url.clone());
            lines.push(Line::from(Span::styled(
                format!("[{}] thread on the web", links.len()),
                link_style(theme),
            )));
        }
        for post in &self.posts {
            lines.push(Line::from(vec![
                Span::styled("■ ", theme.accent),
                Span::styled(
                    post.username.clone(),
                    theme.base().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  · {}", fmt_time(post.post_date), post.post_id),
                    theme.dim(),
                ),
            ]));
            push_bbcode(&mut lines, &mut links, &post.message, theme);
            lines.push(Line::from(Span::raw("")));
        }
        self.lines = lines;
        self.links = links;
    }
}

/// Render BBCode source into static lines, collecting link targets.
pub(crate) fn push_bbcode(
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<String>,
    src: &str,
    theme: &Theme,
) {
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    for chunk in bbcode::render(src) {
        match chunk {
            Chunk::Text(t, s) => {
                if t.is_empty() {
                    continue;
                }
                // Preserve line breaks inside the chunk.
                for (i, seg) in t.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    if !seg.is_empty() {
                        current_spans
                            .push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
            }
            Chunk::Link(label, url, s) => {
                links.push(url.clone());
                // Plain styled text only — never embed OSC sequences in
                // spans (ratatui re-emits cells and the terminal eats the
                // surrounding text). Select-with-mouse or open via [o].
                for (i, seg) in label.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    if !seg.is_empty() {
                        current_spans
                            .push(Span::styled(seg.to_string(), style_from(theme, &s)));
                    }
                }
                current_spans.push(Span::styled(
                    format!(" [{}]", links.len()),
                    link_style(theme),
                ));
            }
        }
    }
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }
}

pub fn thread_view_key(s: &mut ThreadViewState, key: KeyEvent) -> Action {
    if s.link_popup {
        return link_popup_key(s, key);
    }
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
                Action::LoadThread(s.thread.thread_id, s.page - 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char(']') => {
            if s.page < s.last_page {
                s.loading = true;
                Action::LoadThread(s.thread.thread_id, s.page + 1)
            } else {
                Action::None
            }
        }
        KeyCode::Char('r') => Action::StartReply(s.thread.clone()),
        KeyCode::Char('p') => {
            Action::OpenProfile(s.thread.user_id, s.thread.username.clone())
        }
        KeyCode::Char('m') => Action::MarkThreadRead(s.thread.thread_id),
        KeyCode::Char('o') => {
            if !s.links.is_empty() {
                s.link_popup = true;
            }
            Action::None
        }
        KeyCode::Char('u') => match &s.thread.view_url {
            Some(url) => Action::OpenUrl(url.clone()),
            None => Action::None,
        },
        _ => Action::None,
    }
}

fn link_popup_key(s: &mut ThreadViewState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            s.link_popup = false;
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            s.sel_local = s.sel_local.saturating_sub(1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if s.sel_local + 1 < s.links.len() {
                s.sel_local += 1;
            }
            Action::None
        }
        KeyCode::Enter => match s.links.get(s.sel_local) {
            Some(url) => {
                s.link_popup = false;
                Action::OpenUrl(url.clone())
            }
            None => Action::None,
        },
        _ => Action::None,
    }
}

pub fn render_thread_view(
    s: &mut ThreadViewState,
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let title = format!(
        " {} — post {}/{} ",
        truncate(&s.thread.title, 60),
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
        f.render_widget(Paragraph::new("Loading thread…"), inner);
        return;
    }
    if let Some(err) = &s.error {
        f.render_widget(
            Paragraph::new(format!("Error: {err}\n\n[/] to change page."))
                .style(Style::new().fg(theme.error)),
            inner,
        );
        return;
    }

    let content_height = inner.height as usize;
    let total = s.lines.len() as u16;
    let max_scroll = total.saturating_sub(content_height as u16);
    if s.scroll > max_scroll {
        s.scroll = max_scroll;
    }

    let body = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let view = body[0];
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
            ("[/]", "post page"),
            ("r", "reply"),
            ("m", "mark read"),
            ("o", "links"),
            ("u", "open in browser"),
        ],
    );
    f.render_widget(Paragraph::new(hints), body[1]);

    if s.link_popup {
        let popup_area = super::centered_box(area, area.width.min(70), (s.links.len() as u16 + 4).min(20));
        let popup = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.accent)
            .title(Span::styled(" Links — Enter to open, Esc to close ", theme.title()));
        let p_inner = popup.inner(popup_area);
        f.render_widget(ratatui::widgets::Clear, popup_area);
        f.render_widget(popup, popup_area);
        let items: Vec<ListItem> = s
            .links
            .iter()
            .enumerate()
            .map(|(i, url)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" [{}] ", i + 1), theme.accent),
                    Span::styled(url.clone(), link_style(theme)),
                ]))
            })
            .collect();
        let sel = s.sel_local.min(s.links.len().saturating_sub(1));
        let mut state = ListState::default().with_selected(Some(sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(theme.selected()),
            p_inner,
            &mut state,
        );
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_bbcode_keeps_inline_formatting_on_same_line() {
        let theme = Theme::dark();
        let mut lines = Vec::new();
        let mut links = Vec::new();
        push_bbcode(
            &mut lines,
            &mut links,
            "Hello [B]world[/B] from [URL=https://example.com]Windows[/URL]!",
            &theme,
        );
        assert_eq!(lines.len(), 1, "inline styling should remain on a single line");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "https://example.com");
    }

    #[test]
    fn push_bbcode_respects_newlines() {
        let theme = Theme::dark();
        let mut lines = Vec::new();
        let mut links = Vec::new();
        push_bbcode(
            &mut lines,
            &mut links,
            "Line 1 with [B]bold[/B]\nLine 2 with [I]italic[/I]\n\nLine 4",
            &theme,
        );
        assert_eq!(lines.len(), 4);
    }
}
